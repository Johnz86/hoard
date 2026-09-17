//! The `~/.cache/huggingface/hub` layout.
//!
//! `models--org--name/` holds `refs/<branch>` files naming a commit, `snapshots/<sha>/`
//! trees of symlinks and `blobs/<hash>` where the file name already **is** the content
//! hash: 64 hex characters for the sha256 of an LFS file, 40 for the git blob sha1 of a
//! small one. Those names are used as given, so a 314 GB cache is indexed without reading
//! a single byte of weights.

use std::path::{Path, PathBuf};

use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use hoard_core::{CommitSha, GitSha1, RepoId, RepoType, Sha256};

use super::RefKind;
use crate::discover::walker::{self, NoiseFilter, WalkOptions};
use crate::discover::{
    DEFAULT_REF, DiscoveredEntry, DiscoveredRef, DiscoveredRepo, DiscoveredRevision, RevisionName,
};
use crate::error::{IndexError, Result};

const REFS_DIR: &str = "refs";
/// The tree of symlinks one revision of a cached repository is walked through.
pub(crate) const SNAPSHOTS_DIR: &str = "snapshots";
/// Where the bytes those symlinks resolve to are filed, under the repository root.
pub(crate) const BLOBS_DIR: &str = "blobs";

/// How deep `refs/` is read. Names nest at most a level or two (`refs/pr/1`), and a
/// directory symlink pointing at an ancestor would otherwise loop until the stack is gone.
const REFS_DEPTH: usize = 4;

pub fn repo_dir_name(path: &Path) -> Option<RepoId> {
    let name = path.file_name()?.to_str()?;
    parse_dir_name(name)
}

/// Decodes `models--org--name`, where `--` stands for the `/` of the repository id.
pub fn parse_dir_name(name: &str) -> Option<RepoId> {
    let (prefix, rest) = name.split_once("--")?;
    let repo_type: RepoType = prefix.parse().ok()?;
    if rest.is_empty() {
        return None;
    }
    let reference = rest.replace("--", "/");
    RepoId::parse_ref(repo_type, &reference)
        .ok()
        .or_else(|| Some(RepoId::new(repo_type, None, rest)))
}

/// Reads every repository the cache holds. Repositories are read in parallel; the walk
/// inside each snapshot stays single threaded so the pool is not oversubscribed.
pub fn discover(
    root: &Path,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<Vec<DiscoveredRepo>> {
    super::read_all(
        root,
        options.with_default_boundary(root),
        noise,
        super::Units::HfCache(plan(root, noise)?),
    )
}

#[derive(Debug)]
pub(super) struct CacheUnit {
    path: PathBuf,
    repo: RepoId,
}

pub(super) fn plan(root: &Path, noise: &NoiseFilter) -> Result<Vec<CacheUnit>> {
    let mut units: Vec<CacheUnit> = std::fs::read_dir(root)
        .map_err(|error| IndexError::io(root, error))?
        .flatten()
        .filter(|child| !noise.rejects(&child.path()))
        .filter_map(|child| {
            let path = child.path();
            let repo = repo_dir_name(&path).filter(|_| path.is_dir())?;
            Some(CacheUnit { path, repo })
        })
        .collect();
    units.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    units.sort_by_cached_key(|unit| unit.repo.qualified());
    Ok(units)
}

pub(super) fn read(
    units: &[CacheUnit],
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Vec<DiscoveredRepo> {
    units
        .par_iter()
        .filter_map(
            |unit| match read_repo(&unit.path, &unit.repo, options, noise) {
                Ok(repo) => Some(repo),
                Err(error) => {
                    tracing::warn!(path = %unit.path.display(), %error, "cache repository skipped");
                    None
                }
            },
        )
        .collect()
}

fn read_repo(
    path: &Path,
    repo: &RepoId,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<DiscoveredRepo> {
    let named = read_refs(&path.join(REFS_DIR))?;
    let mut discovered = DiscoveredRepo::new(repo.clone(), path.to_path_buf());
    let mut shas: Vec<CommitSha> = Vec::new();

    for sha in snapshot_shas(&path.join(SNAPSHOTS_DIR))? {
        let entries = read_snapshot(&path.join(SNAPSHOTS_DIR).join(sha.to_hex()), options, noise)?;
        shas.push(sha);
        discovered
            .revisions
            .push(DiscoveredRevision::new(RevisionName::Commit(sha), entries));
    }
    for (_, sha) in &named {
        if !shas.contains(sha) {
            shas.push(*sha);
            discovered.revisions.push(DiscoveredRevision::new(
                RevisionName::Commit(*sha),
                Vec::new(),
            ));
        }
    }
    for (name, sha) in named {
        let Some(index) = shas.iter().position(|known| known == &sha) else {
            continue;
        };
        discovered.refs.push(DiscoveredRef {
            name,
            kind: RefKind::Branch,
            revision: index,
        });
    }
    discovered.default_ref = default_ref(&discovered);
    Ok(discovered)
}

fn default_ref(repo: &DiscoveredRepo) -> String {
    if repo.refs.iter().any(|entry| entry.name == DEFAULT_REF) {
        return DEFAULT_REF.to_owned();
    }
    repo.refs
        .first()
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| DEFAULT_REF.to_owned())
}

/// Every `refs/<name>` file with the commit it holds, nested names joined with `/`.
fn read_refs(root: &Path) -> Result<Vec<(String, CommitSha)>> {
    let mut refs = Vec::new();
    collect_refs(root, root, REFS_DEPTH, &mut refs)?;
    refs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(refs)
}

fn collect_refs(
    root: &Path,
    dir: &Path,
    depth: usize,
    refs: &mut Vec<(String, CommitSha)>,
) -> Result<()> {
    let Ok(children) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for child in children.flatten() {
        let path = child.path();
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if depth > 0 {
                collect_refs(root, &path, depth - 1, refs)?;
            }
            continue;
        }
        if file_type.is_symlink() {
            tracing::debug!(path = %path.display(), "ref is a symlink and is not followed");
            continue;
        }
        let Some(name) = walker::relative(root, &path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match text.trim().parse::<CommitSha>() {
            Ok(sha) => refs.push((name, sha)),
            Err(_) => tracing::debug!(path = %path.display(), "ref does not name a commit"),
        }
    }
    Ok(())
}

fn snapshot_shas(dir: &Path) -> Result<Vec<CommitSha>> {
    let Ok(children) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut shas: Vec<CommitSha> = children
        .flatten()
        .filter(|child| child.path().is_dir())
        .filter_map(|child| child.file_name().to_str()?.parse::<CommitSha>().ok())
        .collect();
    shas.sort();
    Ok(shas)
}

fn read_snapshot(
    dir: &Path,
    options: &WalkOptions,
    noise: &NoiseFilter,
) -> Result<Vec<DiscoveredEntry>> {
    let files = walker::walk(dir, options, noise)?;
    Ok(files.into_iter().map(cache_entry).collect())
}

fn cache_entry(file: walker::DiscoveredFile) -> DiscoveredEntry {
    let mut entry = DiscoveredEntry::from_file(file);
    if !entry.is_symlink {
        return entry;
    }
    let Some(link) = entry.abs_path.as_ref() else {
        return entry;
    };
    let Ok(blob) = std::fs::canonicalize(link) else {
        return entry;
    };
    let (sha256, git_sha1) = blob_hashes(&blob);
    entry.sha256 = sha256;
    entry.git_sha1 = git_sha1;
    entry.abs_path = Some(blob);
    entry
}

/// The content hash a cache blob file name already carries.
pub fn blob_hashes(blob: &Path) -> (Option<Sha256>, Option<GitSha1>) {
    let Some(name) = blob.file_name().and_then(|name| name.to_str()) else {
        return (None, None);
    };
    match name.len() {
        64 => (name.parse::<Sha256>().ok(), None),
        40 => (None, name.parse::<GitSha1>().ok()),
        _ => (None, None),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use std::path::Path;

    use crate::hash::{git_blob_sha1_bytes, sha256_bytes};

    /// Writes one cache repository: a blob named by its own hash, a snapshot symlinking to
    /// it and a ref naming the snapshot.
    pub(crate) fn write_repo(root: &Path, dir_name: &str, sha: &str, files: &[(&str, &[u8])]) {
        let repo = root.join(dir_name);
        let snapshot = repo.join("snapshots").join(sha);
        fs::create_dir_all(repo.join("blobs")).expect("blobs dir is creatable");
        fs::create_dir_all(repo.join("refs")).expect("refs dir is creatable");
        fs::create_dir_all(&snapshot).expect("snapshot dir is creatable");
        fs::write(repo.join("refs/main"), sha).expect("ref is writable");

        for (name, bytes) in files {
            let hash = if bytes.len() as u64 >= 1024 {
                sha256_bytes(bytes).to_hex()
            } else {
                git_blob_sha1_bytes(bytes).to_hex()
            };
            let blob = repo.join("blobs").join(&hash);
            fs::write(&blob, bytes).expect("blob is writable");
            let link = snapshot.join(name);
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent).expect("snapshot subdir is creatable");
            }
            unix_fs::symlink(&blob, &link).expect("snapshot symlink is creatable");
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::hash::{git_blob_sha1_bytes, sha256_bytes};

    const SNAPSHOT: &str = "f3ff3571791e39611d31c381e3a41a3af07b4987";

    #[test]
    fn directory_names_decode_to_repository_ids() {
        let models = parse_dir_name("models--hexgrad--Kokoro-82M").expect("name decodes");
        assert_eq!(models.qualified(), "models/hexgrad/Kokoro-82M");
        let datasets = parse_dir_name("datasets--mlabonne--FineTome-100k").expect("name decodes");
        assert_eq!(datasets.repo_type(), RepoType::Datasets);
        let bare = parse_dir_name("models--gpt2").expect("name decodes");
        assert_eq!(bare.qualified(), "models/gpt2");
        assert_eq!(bare.org(), None);
        assert!(parse_dir_name("loras").is_none());
        assert!(parse_dir_name("models--").is_none());
    }

    #[test]
    fn blob_names_are_read_as_hashes_and_never_recomputed() {
        let sha = sha256_bytes(b"weights");
        let (sha256, git) = blob_hashes(Path::new(&format!("/blobs/{}", sha.to_hex())));
        assert_eq!(sha256, Some(sha));
        assert_eq!(git, None);

        let oid = git_blob_sha1_bytes(b"config");
        let (sha256, git) = blob_hashes(Path::new(&format!("/blobs/{}", oid.to_hex())));
        assert_eq!(sha256, None);
        assert_eq!(git, Some(oid));

        assert_eq!(blob_hashes(Path::new("/blobs/short")), (None, None));
    }

    #[test]
    fn a_cache_repository_yields_its_refs_revisions_and_blob_backed_entries() {
        let dir = TempDir::new().expect("temp dir");
        let weights = vec![7u8; 2048];
        fixture::write_repo(
            dir.path(),
            "models--hexgrad--Kokoro-82M",
            SNAPSHOT,
            &[
                ("model.safetensors", weights.as_slice()),
                ("voices/af.pt", b"voice"),
            ],
        );

        let repos = discover(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("cache is readable");
        assert_eq!(repos.len(), 1);
        let repo = &repos[0];
        assert_eq!(repo.repo.qualified(), "models/hexgrad/Kokoro-82M");
        assert_eq!(repo.default_ref, "main");
        assert_eq!(repo.refs.len(), 1);
        assert_eq!(repo.refs[0].name, "main");
        assert_eq!(repo.revisions.len(), 1);
        assert_eq!(
            repo.revisions[0].name,
            RevisionName::Commit(SNAPSHOT.parse().expect("sha parses"))
        );

        let entries = &repo.revisions[0].entries;
        assert_eq!(entries.len(), 2);
        let model = &entries[0];
        assert_eq!(model.rel_path, "model.safetensors");
        assert!(model.is_symlink);
        assert_eq!(model.size, 2048);
        assert_eq!(model.sha256, Some(sha256_bytes(&weights)));
        assert_eq!(model.git_sha1, None);
        let path = model.abs_path.as_ref().expect("the blob is present");
        assert!(path.parent().is_some_and(|dir| dir.ends_with("blobs")));

        let voice = &entries[1];
        assert_eq!(voice.rel_path, "voices/af.pt");
        assert_eq!(voice.git_sha1, Some(git_blob_sha1_bytes(b"voice")));
        assert_eq!(voice.sha256, None);
    }

    #[test]
    fn every_window_of_a_plan_reads_what_the_whole_plan_reads() {
        let dir = TempDir::new().expect("temp dir");
        let weights = vec![7u8; 2048];
        fixture::write_repo(
            dir.path(),
            "models--hexgrad--Kokoro-82M",
            SNAPSHOT,
            &[
                ("model.safetensors", weights.as_slice()),
                ("voices/af.pt", b"voice"),
            ],
        );
        fixture::write_repo(
            dir.path(),
            "datasets--mlabonne--FineTome-100k",
            SNAPSHOT,
            &[("data/train.parquet", b"rows")],
        );
        fixture::write_repo(
            dir.path(),
            "models--gpt2",
            SNAPSHOT,
            &[("config.json", b"{}")],
        );

        crate::discover::assert_windows_read_the_whole_plan(
            dir.path(),
            crate::config::SourceKind::HfCache,
        );
    }

    #[test]
    fn a_ref_without_a_snapshot_still_gets_a_revision_to_point_at() {
        let dir = TempDir::new().expect("temp dir");
        let repo = dir.path().join("models--org--name");
        std::fs::create_dir_all(repo.join("refs")).expect("refs dir is creatable");
        std::fs::write(repo.join("refs/main"), SNAPSHOT).expect("ref is writable");

        let repos = discover(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("cache is readable");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].revisions.len(), 1);
        assert!(repos[0].revisions[0].entries.is_empty());
        assert_eq!(repos[0].refs[0].revision, 0);
    }

    #[test]
    fn a_directory_loop_under_refs_terminates_instead_of_overflowing_the_stack() {
        let dir = TempDir::new().expect("temp dir");
        let repo = dir.path().join("models--org--name");
        let refs = repo.join("refs");
        std::fs::create_dir_all(refs.join("pr")).expect("refs dir is creatable");
        std::fs::write(refs.join("main"), SNAPSHOT).expect("ref is writable");
        std::fs::write(refs.join("pr/1"), SNAPSHOT).expect("ref is writable");
        std::os::unix::fs::symlink(&refs, refs.join("pr/loop")).expect("symlink is creatable");

        let repos = discover(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("cache is readable");
        let names: Vec<&str> = repos[0]
            .refs
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        assert_eq!(names, ["main", "pr/1"]);
    }

    #[test]
    fn non_repository_directories_are_ignored() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".locks")).expect("dir is creatable");
        std::fs::write(dir.path().join("CACHEDIR.TAG"), b"tag").expect("file is writable");

        let repos = discover(dir.path(), &WalkOptions::default(), &NoiseFilter::default())
            .expect("cache is readable");
        assert!(repos.is_empty());
    }
}
