//! Bare folders and the tool layouts that look like them.
//!
//! A loose folder has no commits, so it gets one synthetic revision whose sha is derived
//! from the sorted `(relative path, fingerprint, size)` triples of its contents: the same
//! folder always names the same revision, and any change names a new one. The folder path
//! itself is handed to inference unchanged, which already reads known layout names as low
//! confidence evidence.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use ignore::DirEntry;
use parking_lot::Mutex;
use rayon::current_num_threads;

use hoard_core::{CommitSha, RepoId, RepoType};

use super::RefKind;
use crate::discover::walker::{self, DiscoveredFile, NoiseFilter, WalkOptions};
use crate::discover::{
    DEFAULT_REF, DiscoveredEntry, DiscoveredRef, DiscoveredRepo, DiscoveredRevision, RevisionName,
};
use crate::error::Result;
use crate::hash::sha256_bytes;
use crate::layout::{Tool, marks_tool};

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

/// Kohya's training folders, which name no model category and so are not layout folders.
const KOHYA_DIRS: [&str; 4] = ["img", "model", "log", "reg"];

const LAYOUT_QUORUM: usize = 2;

const MAX_ROOT_WALKS: usize = 64;

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
    let marked = |tool: Tool| dirs.iter().filter(|dir| marks_tool(dir, tool)).count();
    if marked(Tool::ComfyUi) >= LAYOUT_QUORUM {
        return Some(ToolLayout::ComfyUi);
    }
    if marked(Tool::A1111) >= LAYOUT_QUORUM {
        return Some(ToolLayout::A1111);
    }
    let kohya = dirs
        .iter()
        .filter(|dir| KOHYA_DIRS.contains(&dir.as_str()))
        .count();
    if kohya >= LAYOUT_QUORUM {
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
    let units = super::Units::Loose(plan(root, org, repo_per_folder, options, noise)?);
    super::read_all(root, options.clone(), noise, units)
}

#[derive(Debug)]
pub(super) struct LoosePlan {
    source_org: String,
    root_name: String,
    organizations: Option<BTreeSet<String>>,
    units: Vec<LooseUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LooseUnit {
    org: String,
    name: String,
    subtrees: Vec<Subtree>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Subtree {
    RootFiles,
    Folder(String),
}

pub(super) fn plan(
    root: &Path,
    org: &str,
    repo_per_folder: bool,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<LoosePlan> {
    let layout = layout(root);
    if let Some(layout) = layout {
        tracing::info!(root = %root.display(), layout = layout.as_str(), "known tool layout");
    }
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(org)
        .to_owned();
    if !repo_per_folder && layout.is_none() {
        return Ok(LoosePlan {
            units: vec![LooseUnit {
                org: org.to_owned(),
                name: root_name.clone(),
                subtrees: Vec::new(),
            }],
            source_org: org.to_owned(),
            root_name,
            organizations: None,
        });
    }
    let shape = Mutex::new(RootShape::default());
    walker::walk_with(
        root,
        options,
        noise,
        |_: &DirEntry| true,
        |file: walker::Accepted<'_>| shape.lock().record(&file.rel_path),
    )?;
    let shape = shape.into_inner();
    let organizations = shape.organizations();
    let units = shape.units(org, &root_name, &organizations);
    Ok(LoosePlan {
        source_org: org.to_owned(),
        root_name,
        organizations: Some(organizations),
        units,
    })
}

impl LoosePlan {
    pub(super) fn len(&self) -> usize {
        self.units.len()
    }

    pub(super) fn window_size(&self) -> usize {
        current_num_threads().max(self.units.len().div_ceil(MAX_ROOT_WALKS))
    }

    pub(super) fn read(
        &self,
        root: &Path,
        units: Range<usize>,
        options: &WalkOptions,
        noise: &NoiseFilter,
    ) -> Result<Vec<DiscoveredRepo>> {
        let window = &self.units[units];
        if window.is_empty() {
            return Ok(Vec::new());
        }
        let pruning = self
            .organizations
            .as_ref()
            .map(|_| Pruning::of(root, window));
        let found: Mutex<Vec<Vec<DiscoveredFile>>> = Mutex::new(vec![Vec::new(); window.len()]);
        walker::walk_with(
            root,
            options,
            noise,
            move |entry: &DirEntry| pruning.as_ref().is_none_or(|pruning| pruning.admits(entry)),
            |file: walker::Accepted<'_>| {
                if let Some(index) = self.unit_of(window, &file.rel_path) {
                    let file = file.into_file();
                    found.lock()[index].push(file);
                }
            },
        )?;
        Ok(window
            .iter()
            .zip(found.into_inner())
            .filter(|(_, files)| !files.is_empty())
            .map(|(unit, mut files)| {
                files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
                let entries = files
                    .into_iter()
                    .map(|mut file| {
                        let placed = file.rel_path.len() - self.place(&file.rel_path).2.len();
                        file.rel_path.drain(..placed);
                        DiscoveredEntry::from_file(file)
                    })
                    .collect();
                repository(root, unit, entries)
            })
            .collect())
    }

    fn unit_of(&self, window: &[LooseUnit], rel_path: &str) -> Option<usize> {
        let (org, name, _) = self.place(rel_path);
        window
            .binary_search_by(|unit| (unit.org.as_str(), unit.name.as_str()).cmp(&(org, name)))
            .ok()
    }

    fn place<'a>(&'a self, rel_path: &'a str) -> (&'a str, &'a str, &'a str) {
        match &self.organizations {
            None => (&self.source_org, &self.root_name, rel_path),
            Some(organizations) => {
                locate(rel_path, &self.root_name, &self.source_org, organizations)
            }
        }
    }
}

fn repository(root: &Path, unit: &LooseUnit, entries: Vec<DiscoveredEntry>) -> DiscoveredRepo {
    let repo = RepoId::new(RepoType::Models, Some(unit.org.clone()), unit.name.clone());
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
}

#[derive(Debug)]
struct Pruning {
    root: PathBuf,
    root_files: bool,
    folders: HashSet<String>,
    parents: HashSet<String>,
}

impl Pruning {
    fn of(root: &Path, window: &[LooseUnit]) -> Self {
        let mut pruning = Self {
            root: root.to_path_buf(),
            root_files: false,
            folders: HashSet::new(),
            parents: HashSet::new(),
        };
        for subtree in window.iter().flat_map(|unit| &unit.subtrees) {
            match subtree {
                Subtree::RootFiles => pruning.root_files = true,
                Subtree::Folder(folder) => {
                    if let Some((parent, _)) = folder.split_once('/') {
                        pruning.parents.insert(parent.to_owned());
                    }
                    pruning.folders.insert(folder.clone());
                }
            }
        }
        pruning
    }

    fn admits(&self, entry: &DirEntry) -> bool {
        let Some(rel_path) = walker::relative(&self.root, entry.path()) else {
            return entry.depth() == 0;
        };
        let is_dir = entry.file_type().is_none_or(|file_type| file_type.is_dir());
        (self.root_files && !is_dir && !rel_path.contains('/'))
            || self.folders.contains(&rel_path)
            || (is_dir && self.parents.contains(&rel_path))
            || rel_path
                .match_indices('/')
                .take(2)
                .any(|(at, _)| self.folders.contains(&rel_path[..at]))
    }
}

#[derive(Debug, Default)]
struct RootShape {
    root_files: bool,
    folders: BTreeMap<String, FolderShape>,
}

#[derive(Debug, Default)]
struct FolderShape {
    holds_files: bool,
    subfolders: BTreeSet<String>,
}

impl FolderShape {
    fn record(&mut self, rest: &str) {
        match rest.split_once('/') {
            None => self.holds_files = true,
            Some((subfolder, _)) => {
                if !self.subfolders.contains(subfolder) {
                    self.subfolders.insert(subfolder.to_owned());
                }
            }
        }
    }
}

impl RootShape {
    fn record(&mut self, rel_path: &str) {
        let Some((first, rest)) = rel_path.split_once('/') else {
            self.root_files = true;
            return;
        };
        match self.folders.get_mut(first) {
            Some(folder) => folder.record(rest),
            None => {
                let mut folder = FolderShape::default();
                folder.record(rest);
                self.folders.insert(first.to_owned(), folder);
            }
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
    fn organizations(&self) -> BTreeSet<String> {
        self.folders
            .iter()
            .filter(|(_, folder)| !folder.holds_files && !folder.subfolders.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn units(
        self,
        source_org: &str,
        root_name: &str,
        organizations: &BTreeSet<String>,
    ) -> Vec<LooseUnit> {
        let mut units: BTreeMap<(String, String), Vec<Subtree>> = BTreeMap::new();
        if self.root_files {
            units
                .entry((source_org.to_owned(), root_name.to_owned()))
                .or_default()
                .push(Subtree::RootFiles);
        }
        for (name, folder) in self.folders {
            if organizations.contains(&name) {
                for subfolder in folder.subfolders {
                    let path = format!("{name}/{subfolder}");
                    units
                        .entry((name.clone(), subfolder))
                        .or_default()
                        .push(Subtree::Folder(path));
                }
            } else {
                units
                    .entry((source_org.to_owned(), name.clone()))
                    .or_default()
                    .push(Subtree::Folder(name));
            }
        }
        units
            .into_iter()
            .map(|((org, name), subtrees)| LooseUnit {
                org,
                name,
                subtrees,
            })
            .collect()
    }
}

/// Splits `loras/style/a.safetensors` into the top folder and the path beneath it. Files
/// sitting directly in the root belong to a repository named after the root itself.
fn split_folder<'a>(rel_path: &'a str, root_name: &'a str) -> (&'a str, &'a str) {
    rel_path.split_once('/').unwrap_or((root_name, rel_path))
}

/// The organization, repository and path within it that one discovered file belongs to.
fn locate<'a>(
    rel_path: &'a str,
    root_name: &'a str,
    source_org: &'a str,
    organizations: &BTreeSet<String>,
) -> (&'a str, &'a str, &'a str) {
    let mut parts = rel_path.splitn(3, '/');
    if let (Some(org), Some(name), Some(rest)) = (parts.next(), parts.next(), parts.next())
        && organizations.contains(org)
    {
        return (org, name, rest);
    }
    let (folder, rest) = split_folder(rel_path, root_name);
    (source_org, folder, rest)
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

    fn entry_paths(repo: &DiscoveredRepo) -> Vec<&str> {
        repo.revisions[0]
            .entries
            .iter()
            .map(|entry| entry.rel_path.as_str())
            .collect()
    }

    fn per_folder(root: &Path, org: &str) -> Vec<DiscoveredRepo> {
        discover(
            root,
            org,
            true,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root is readable")
    }

    #[test]
    fn a_hoardignore_in_the_root_hides_a_file_inside_a_repository_folder() {
        let dir = TempDir::new().expect("temp dir");
        write(
            dir.path(),
            walker::HOARD_IGNORE,
            b"models/secret.safetensors\n",
        );
        write(dir.path(), "models/secret.safetensors", b"secret");
        write(dir.path(), "models/kept.safetensors", b"kept");

        let repos = per_folder(dir.path(), "local");
        let models = repos
            .iter()
            .find(|repo| repo.repo.name() == "models")
            .expect("the models repo exists");
        assert_eq!(entry_paths(models), ["kept.safetensors"]);
    }

    #[test]
    fn a_root_file_and_a_folder_named_like_the_root_share_one_repository() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("bulk");
        write(&root, "a.bin", b"a");
        write(&root, "bulk/b.bin", b"b");

        let repos = per_folder(&root, "local");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].repo.qualified(), "models/local/bulk");
        assert_eq!(entry_paths(&repos[0]), ["a.bin", "b.bin"]);
    }

    #[test]
    fn a_publisher_folder_with_a_file_of_its_own_is_one_repository() {
        let dir = TempDir::new().expect("temp dir");
        write(dir.path(), "pub/readme.md", b"readme");
        write(dir.path(), "pub/model/weights.gguf", b"GGUF");

        let repos = per_folder(dir.path(), "local");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].repo.qualified(), "models/local/pub");
        assert_eq!(entry_paths(&repos[0]), ["model/weights.gguf", "readme.md"]);
    }

    #[test]
    fn every_window_of_a_plan_reads_what_the_whole_plan_reads() {
        let dir = TempDir::new().expect("temp dir");
        comfy_tree(dir.path());
        write(dir.path(), "publisher/model/weights.gguf", b"GGUF");
        write(dir.path(), "publisher/other/weights.gguf", b"GGUF");

        crate::discover::assert_windows_read_the_whole_plan(
            dir.path(),
            crate::config::SourceKind::Loose,
        );
    }

    #[test]
    fn a_planned_loose_source_walks_only_the_folders_it_holds() {
        let comfy = TempDir::new().expect("temp dir");
        comfy_tree(comfy.path());
        let root_name = comfy
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temp dir is named");
        let unit = |org: &str, name: &str, subtree: Subtree| LooseUnit {
            org: org.to_owned(),
            name: name.to_owned(),
            subtrees: vec![subtree],
        };
        let folder = |name: &str| unit("comfy", name, Subtree::Folder(name.to_owned()));

        let planned = plan(
            comfy.path(),
            "comfy",
            false,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root plans");
        let mut expected = vec![
            folder("checkpoints"),
            folder("loras"),
            folder("vae"),
            unit("comfy", root_name, Subtree::RootFiles),
        ];
        expected.sort_by(|left, right| (&left.org, &left.name).cmp(&(&right.org, &right.name)));
        assert_eq!(planned.units, expected);

        let publisher = TempDir::new().expect("temp dir");
        let models = ["Qwen3.5-35B-A3B-GGUF", "Qwen3.6-35B-A3B-GGUF"];
        for model in models {
            write(
                publisher.path(),
                &format!("lmstudio-community/{model}/{model}.gguf"),
                b"GGUF",
            );
        }
        let planned = plan(
            publisher.path(),
            "lmstudio",
            true,
            &WalkOptions::default(),
            &NoiseFilter::default(),
        )
        .expect("loose root plans");
        let expected: Vec<LooseUnit> = models
            .iter()
            .map(|model| {
                unit(
                    "lmstudio-community",
                    model,
                    Subtree::Folder(format!("lmstudio-community/{model}")),
                )
            })
            .collect();
        assert_eq!(
            planned.units, expected,
            "a publisher plans one unit per model, not one for everything it ships"
        );
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
