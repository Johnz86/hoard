//! Bare folders and the tool layouts that look like them.
//!
//! A loose folder has no commits, so it gets one synthetic revision whose sha is derived
//! from the sorted `(relative path, fingerprint, size)` triples of its contents: the same
//! folder always names the same revision, and any change names a new one. The folder path
//! itself is handed to inference unchanged, which already reads known layout names as low
//! confidence evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hoard_core::{CommitSha, RepoId, RepoType};

use crate::discover::walker::{self, NoiseFilter, WalkOptions};
use crate::discover::{
    DEFAULT_REF, DiscoveredEntry, DiscoveredRef, DiscoveredRepo, DiscoveredRevision, RevisionName,
};
use crate::error::Result;
use crate::hash::sha256_bytes;
use crate::store::revision::RefKind;

/// A tool's models directory, recognized by the folders it keeps beside each other. A
/// recognized layout always yields one repository per category folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLayout {
    ComfyUi,
    A1111,
    Kohya,
    LlamaCpp,
}

impl ToolLayout {
    pub const fn as_str(self) -> &'static str {
        match self {
            ToolLayout::ComfyUi => "comfyui",
            ToolLayout::A1111 => "a1111",
            ToolLayout::Kohya => "kohya",
            ToolLayout::LlamaCpp => "llama.cpp",
        }
    }
}

/// Category folders of a ComfyUI `models/` directory.
const COMFYUI_DIRS: [&str; 12] = [
    "loras",
    "checkpoints",
    "controlnet",
    "vae",
    "upscale_models",
    "embeddings",
    "unet",
    "clip",
    "text_encoders",
    "diffusion_models",
    "clip_vision",
    "style_models",
];

/// Category folders of an Automatic1111 `models/` directory.
const A1111_DIRS: [&str; 5] = ["stable-diffusion", "lora", "esrgan", "vae", "embeddings"];

/// Working folders of a kohya training directory.
const KOHYA_DIRS: [&str; 4] = ["img", "model", "log", "reg"];

/// How many category folders have to agree before a layout counts as recognized.
const LAYOUT_QUORUM: usize = 2;

/// Names the tool layout a directory looks like, or `None` for a plain folder.
pub fn layout(root: &Path) -> Option<ToolLayout> {
    let Ok(children) = std::fs::read_dir(root) else {
        return None;
    };
    let mut dirs: Vec<String> = Vec::new();
    let mut gguf = false;
    for child in children.flatten() {
        let Some(name) = child.file_name().to_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        if child.path().is_dir() {
            dirs.push(name);
        } else {
            gguf = gguf || name.ends_with(".gguf");
        }
    }
    let hits = |markers: &[&str]| {
        dirs.iter()
            .filter(|dir| markers.contains(&dir.as_str()))
            .count()
    };
    if hits(&COMFYUI_DIRS) >= LAYOUT_QUORUM {
        return Some(ToolLayout::ComfyUi);
    }
    if hits(&A1111_DIRS) >= LAYOUT_QUORUM {
        return Some(ToolLayout::A1111);
    }
    if hits(&KOHYA_DIRS) >= LAYOUT_QUORUM {
        return Some(ToolLayout::Kohya);
    }
    gguf.then_some(ToolLayout::LlamaCpp)
}

/// Reads a loose root. `org` names the repositories so two sources holding a `loras`
/// folder never collide; `repo_per_folder` is forced on for a recognized tool layout.
pub fn discover(
    root: &Path,
    org: &str,
    repo_per_folder: bool,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<Vec<DiscoveredRepo>> {
    let layout = layout(root);
    if let Some(layout) = layout {
        tracing::info!(root = %root.display(), layout = layout.as_str(), "known tool layout");
    }
    let files = walker::walk(root, options, noise)?;
    let per_folder = repo_per_folder || layout.is_some();
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(org);
    let organizations = organizations(&files);

    let mut grouped: BTreeMap<(String, String), Vec<DiscoveredEntry>> = BTreeMap::new();
    for file in files {
        let (repo_org, folder, rel_path) = match per_folder {
            true => locate(&file.rel_path, root_name, org, &organizations),
            false => (org.to_owned(), root_name.to_owned(), file.rel_path.clone()),
        };
        let mut entry = DiscoveredEntry::from_file(file);
        entry.rel_path = rel_path;
        grouped.entry((repo_org, folder)).or_default().push(entry);
    }

    Ok(grouped
        .into_iter()
        .map(|((repo_org, folder), entries)| {
            let repo = RepoId::new(RepoType::Models, Some(repo_org), folder);
            let mut discovered = DiscoveredRepo::new(repo, root.to_path_buf());
            discovered
                .revisions
                .push(DiscoveredRevision::new(RevisionName::Synthetic, entries));
            discovered.refs.push(DiscoveredRef {
                name: DEFAULT_REF.to_owned(),
                kind: RefKind::Branch,
                revision: 0,
            });
            discovered
        })
        .collect())
}

/// Splits `loras/style/a.safetensors` into the top folder and the path beneath it. Files
/// sitting directly in the root belong to a repository named after the root itself.
fn split_folder(rel_path: &str, root_name: &str) -> (String, String) {
    match rel_path.split_once('/') {
        Some((folder, rest)) => (folder.to_owned(), rest.to_owned()),
        None => (root_name.to_owned(), rel_path.to_owned()),
    }
}

/// The top level folders that name an organization rather than a repository.
///
/// A folder holding no files of its own, only directories that do, is the HuggingFace
/// convention of publisher then repository, which LM Studio and any on-disk mirror of the
/// Hub follow. Reading such a folder as one repository would gather every model a
/// publisher ships into a single entry, and offer all of them as one pull.
///
/// A tool layout like ComfyUI keeps its weights directly in `loras` or `checkpoints`, so
/// those folders hold files and stay repositories of their own.
fn organizations(files: &[walker::DiscoveredFile]) -> BTreeSet<String> {
    let mut holds_files = BTreeSet::new();
    let mut holds_directories = BTreeSet::new();
    for file in files {
        let Some((first, rest)) = file.rel_path.split_once('/') else {
            continue;
        };
        match rest.contains('/') {
            false => holds_files.insert(first.to_owned()),
            true => holds_directories.insert(first.to_owned()),
        };
    }
    holds_directories
        .difference(&holds_files)
        .cloned()
        .collect()
}

/// The organization, repository and path within it that one discovered file belongs to.
fn locate(
    rel_path: &str,
    root_name: &str,
    source_org: &str,
    organizations: &BTreeSet<String>,
) -> (String, String, String) {
    let mut parts = rel_path.splitn(3, '/');
    if let (Some(org), Some(name), Some(rest)) = (parts.next(), parts.next(), parts.next())
        && organizations.contains(org)
    {
        return (org.to_owned(), name.to_owned(), rest.to_owned());
    }
    let (folder, rest) = split_folder(rel_path, root_name);
    (source_org.to_owned(), folder, rest)
}

/// One file's contribution to a folder's synthetic revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentPart<'a> {
    pub rel_path: &'a str,
    pub fingerprint: &'a str,
    pub size: u64,
}

/// The revision sha of a loose folder: sha256 over its sorted content triples, truncated
/// to the 20 bytes every revision is named by.
pub fn synthetic_sha(parts: &mut [ContentPart<'_>]) -> CommitSha {
    parts.sort_unstable_by(|left, right| {
        (left.rel_path, left.fingerprint, left.size).cmp(&(
            right.rel_path,
            right.fingerprint,
            right.size,
        ))
    });
    let mut buffer = String::with_capacity(parts.len() * 64);
    for part in parts.iter() {
        buffer.push_str(part.rel_path);
        buffer.push('\u{1f}');
        buffer.push_str(part.fingerprint);
        buffer.push('\u{1f}');
        buffer.push_str(&part.size.to_string());
        buffer.push('\n');
    }
    let digest = sha256_bytes(buffer.as_bytes());
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&digest.as_bytes()[..20]);
    CommitSha::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write(dir: &Path, relative: &str, bytes: &[u8]) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent is creatable");
        }
        fs::write(&path, bytes).expect("fixture is writable");
    }

    fn comfy_tree(dir: &Path) {
        write(dir, "loras/style.safetensors", b"lora bytes");
        write(dir, "loras/nested/other.safetensors", b"other bytes");
        write(dir, "checkpoints/base.safetensors", b"ckpt bytes");
        write(dir, "vae/vae.safetensors", b"vae bytes");
        write(dir, "notes.txt", b"loose");
    }

    #[test]
    fn known_layouts_are_recognized_by_their_category_folders() {
        let dir = TempDir::new().expect("temp dir");
        comfy_tree(dir.path());
        assert_eq!(layout(dir.path()), Some(ToolLayout::ComfyUi));

        let a1111 = dir.path().join("webui");
        for name in ["Stable-diffusion", "Lora", "ESRGAN"] {
            fs::create_dir_all(a1111.join(name)).expect("dir is creatable");
        }
        assert_eq!(layout(&a1111), Some(ToolLayout::A1111));

        let kohya = dir.path().join("train");
        for name in ["img", "model", "log"] {
            fs::create_dir_all(kohya.join(name)).expect("dir is creatable");
        }
        assert_eq!(layout(&kohya), Some(ToolLayout::Kohya));

        let llama = dir.path().join("gguf");
        fs::create_dir_all(&llama).expect("dir is creatable");
        write(&llama, "ggml-model-q4_0.gguf", b"GGUF");
        assert_eq!(layout(&llama), Some(ToolLayout::LlamaCpp));

        let plain = dir.path().join("plain");
        fs::create_dir_all(plain.join("stuff")).expect("dir is creatable");
        assert_eq!(layout(&plain), None);
    }

    #[test]
    fn a_publisher_folder_yields_one_repository_per_model() {
        let dir = TempDir::new().expect("temp dir");
        let publisher = dir.path().join("lmstudio-community");
        for model in ["Qwen3.5-35B-A3B-GGUF", "Qwen3.6-35B-A3B-GGUF"] {
            let repo = publisher.join(model);
            fs::create_dir_all(&repo).expect("dir is creatable");
            write(&repo, &format!("{model}.gguf"), b"GGUF");
            write(&repo, &format!("mmproj-{model}.gguf"), b"GGUF");
        }

        let repos = discover(
            dir.path(),
            "lmstudio",
            true,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root is readable");

        let qualified: Vec<String> = repos.iter().map(|found| found.repo.qualified()).collect();
        assert_eq!(
            qualified,
            vec![
                "models/lmstudio-community/Qwen3.5-35B-A3B-GGUF".to_owned(),
                "models/lmstudio-community/Qwen3.6-35B-A3B-GGUF".to_owned(),
            ],
            "a publisher is an organization, not a repository holding every model it ships"
        );
        for found in &repos {
            let entries = &found.revisions[0].entries;
            assert_eq!(
                entries.len(),
                2,
                "the model and its projector travel together"
            );
            assert!(
                entries.iter().all(|entry| !entry.rel_path.contains('/')),
                "paths are relative to the model, not to the publisher"
            );
        }
    }

    #[test]
    fn a_comfyui_tree_yields_one_repository_per_category_folder() {
        let dir = TempDir::new().expect("temp dir");
        comfy_tree(dir.path());

        let repos = discover(
            dir.path(),
            "comfy",
            false,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root is readable");
        let names: Vec<&str> = repos.iter().map(|repo| repo.repo.name()).collect();
        let root_name = dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temp dir is named");
        let mut expected = vec!["checkpoints", "loras", root_name, "vae"];
        expected.sort_unstable();
        assert_eq!(names, expected);

        let loras = repos
            .iter()
            .find(|repo| repo.repo.name() == "loras")
            .expect("the loras repo exists");
        assert_eq!(loras.repo.qualified(), "models/comfy/loras");
        assert_eq!(loras.repo.org(), Some("comfy"));
        assert_eq!(loras.revisions.len(), 1);
        assert_eq!(loras.revisions[0].name, RevisionName::Synthetic);
        assert_eq!(loras.refs[0].name, DEFAULT_REF);
        let paths: Vec<&str> = loras.revisions[0]
            .entries
            .iter()
            .map(|entry| entry.rel_path.as_str())
            .collect();
        assert_eq!(paths, ["nested/other.safetensors", "style.safetensors"]);
        let entry = &loras.revisions[0].entries[1];
        assert!(entry.present());
        assert!(entry.abs_path.as_ref().is_some_and(|path| path.is_file()));
    }

    #[test]
    fn a_plain_folder_can_be_indexed_as_one_repository() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), "one/a.bin", b"a");
        write(dir.path(), "two/b.bin", b"b");

        let repos = discover(
            dir.path(),
            "bulk",
            false,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root is readable");
        assert_eq!(repos.len(), 1);
        let paths: Vec<&str> = repos[0].revisions[0]
            .entries
            .iter()
            .map(|entry| entry.rel_path.as_str())
            .collect();
        assert_eq!(paths, ["one/a.bin", "two/b.bin"]);
    }

    #[test]
    fn the_synthetic_sha_is_stable_and_changes_with_the_contents() {
        let mut parts = vec![
            ContentPart {
                rel_path: "b.safetensors",
                fingerprint: "0000000000000002",
                size: 20,
            },
            ContentPart {
                rel_path: "a.safetensors",
                fingerprint: "0000000000000001",
                size: 10,
            },
        ];
        let first = synthetic_sha(&mut parts);
        let mut reordered = vec![parts[1], parts[0]];
        assert_eq!(synthetic_sha(&mut reordered), first);
        assert_eq!(first.to_hex().len(), 40);

        let mut grown = parts.to_vec();
        grown.push(ContentPart {
            rel_path: "c.safetensors",
            fingerprint: "0000000000000003",
            size: 30,
        });
        assert_ne!(synthetic_sha(&mut grown), first);

        let mut changed = vec![
            ContentPart {
                fingerprint: "000000000000ffff",
                ..parts[0]
            },
            parts[1],
        ];
        assert_ne!(synthetic_sha(&mut changed), first);
    }
}
