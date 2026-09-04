//! A directory of git clones, read with `gix`.
//!
//! Refs name the revisions; the tree at each ref names the entries. A file whose blob is a
//! git LFS pointer resolves to `<git dir>/lfs/objects/<a>/<b>/<oid>`; when that object is
//! not on this disk the path is still indexed, as an entry with no bytes behind it.

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use hoard_core::{CommitSha, RepoId, RepoType};

use crate::discover::walker::{self, NoiseFilter};
use crate::discover::{
    DEFAULT_REF, DiscoveredEntry, DiscoveredRef, DiscoveredRepo, DiscoveredRevision, RevisionName,
};
use crate::error::{IndexError, Result};
use crate::sniff::{MAX_LFS_POINTER_BYTES, parse_lfs_pointer};
use crate::store::revision::RefKind;

const BRANCH_PREFIX: &str = "refs/heads/";
const TAG_PREFIX: &str = "refs/tags/";

/// How many directory levels below the source root a clone may sit, so `root/org/repo` is
/// found without walking a whole disk.
const CLONE_DEPTH: usize = 2;

/// True for a worktree, a bare repository or a linked worktree.
pub fn is_git_repo(path: &Path) -> bool {
    let dot_git = path.join(".git");
    if dot_git.is_file() || dot_git.is_dir() {
        return true;
    }
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

/// Reads every clone at or below `root`.
pub fn discover(root: &Path, noise: &NoiseFilter) -> Result<Vec<DiscoveredRepo>> {
    let mut clones = Vec::new();
    collect_clones(root, root, CLONE_DEPTH, noise, &mut clones)?;
    clones.sort();

    let mut repos: Vec<DiscoveredRepo> = clones
        .par_iter()
        .filter_map(|path| match read_clone(root, path) {
            Ok(repo) => Some(repo),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "clone skipped");
                None
            }
        })
        .collect();
    repos.sort_by(|left, right| left.repo.qualified().cmp(&right.repo.qualified()));
    Ok(repos)
}

fn collect_clones(
    root: &Path,
    dir: &Path,
    depth: usize,
    noise: &NoiseFilter,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    if noise.rejects(dir) {
        return Ok(());
    }
    if is_git_repo(dir) {
        found.push(dir.to_path_buf());
        return Ok(());
    }
    if depth == 0 {
        return Ok(());
    }
    let children = std::fs::read_dir(dir).map_err(|error| IndexError::io(dir, error))?;
    for child in children.flatten() {
        let path = child.path();
        if path.is_dir() {
            collect_clones(root, &path, depth - 1, noise, found)?;
        }
    }
    Ok(())
}

fn read_clone(root: &Path, path: &Path) -> Result<DiscoveredRepo> {
    let repository = gix::open(path)
        .map_err(|error| IndexError::Git(format!("{} does not open: {error}", path.display())))?;
    let git_dir = repository.git_dir().to_path_buf();
    let work_dir = repository.workdir().map(Path::to_path_buf);
    let id = repo_id(&repository, root, path);

    let mut discovered = DiscoveredRepo::new(id, path.to_path_buf());
    discovered.git_dir = Some(git_dir.clone());

    let mut shas: Vec<CommitSha> = Vec::new();
    for (name, kind, sha) in read_refs(&repository)? {
        let index = match shas.iter().position(|known| known == &sha) {
            Some(index) => index,
            None => {
                let entries = read_tree(&repository, sha, &git_dir, work_dir.as_deref())?;
                let mut revision = DiscoveredRevision::new(RevisionName::Commit(sha), entries);
                revision.title = commit_title(&repository, sha);
                shas.push(sha);
                discovered.revisions.push(revision);
                shas.len() - 1
            }
        };
        discovered.refs.push(DiscoveredRef {
            name,
            kind,
            revision: index,
        });
    }
    discovered.default_ref = default_ref(&repository, &discovered);
    Ok(discovered)
}

/// The upstream identity when a remote names one, otherwise the directory layout below the
/// source root.
fn repo_id(repository: &gix::Repository, root: &Path, path: &Path) -> RepoId {
    repository
        .config_snapshot()
        .string("remote.origin.url")
        .and_then(|url| repo_from_url(&url.to_str_lossy()))
        .unwrap_or_else(|| repo_from_path(root, path))
}

/// Recovers `type/org/name` from a clone URL, http or scp shaped.
pub fn repo_from_url(url: &str) -> Option<RepoId> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let after_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let path = match after_scheme.split_once('/') {
        Some((_, rest)) => rest,
        None => after_scheme.split_once(':').map(|(_, rest)| rest)?,
    };
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let (repo_type, segments) = match segments.split_first() {
        Some((head, rest)) if !rest.is_empty() => match head.parse::<RepoType>() {
            Ok(repo_type) => (repo_type, rest.to_vec()),
            Err(_) => (RepoType::Models, segments),
        },
        _ => (RepoType::Models, segments),
    };
    match segments.as_slice() {
        [] => None,
        [name] => Some(RepoId::new(repo_type, None, *name)),
        rest => {
            let name = rest.last()?;
            let org = rest.get(rest.len() - 2)?;
            Some(RepoId::new(repo_type, Some((*org).to_owned()), *name))
        }
    }
}

fn repo_from_path(root: &Path, path: &Path) -> RepoId {
    let relative = walker::relative(root, path).unwrap_or_default();
    let segments: Vec<&str> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let fallback = || {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
            .to_owned()
    };
    match segments.as_slice() {
        [] => RepoId::new(RepoType::Models, None, fallback()),
        [name] => RepoId::new(RepoType::Models, None, *name),
        rest => {
            let name = rest.last().copied().unwrap_or_default();
            let org = rest.get(rest.len() - 2).copied().unwrap_or_default();
            RepoId::new(RepoType::Models, Some(org.to_owned()), name)
        }
    }
}

fn read_refs(repository: &gix::Repository) -> Result<Vec<(String, RefKind, CommitSha)>> {
    let platform = repository
        .references()
        .map_err(|error| IndexError::Git(format!("refs unavailable: {error}")))?;
    let iter = platform
        .all()
        .map_err(|error| IndexError::Git(format!("refs unavailable: {error}")))?;
    let mut refs = Vec::new();
    for entry in iter {
        let Ok(mut reference) = entry else { continue };
        let full_name = reference.name().as_bstr().to_string();
        let Some((kind, name)) = classify_ref(&full_name) else {
            continue;
        };
        let Ok(target) = reference.peel_to_id() else {
            tracing::debug!(reference = full_name, "ref does not peel to an object");
            continue;
        };
        let Some(sha) = commit_sha(target.as_ref()) else {
            continue;
        };
        refs.push((name, kind, sha));
    }
    refs.sort_by(|left, right| (left.1, &left.0).cmp(&(right.1, &right.0)));
    Ok(refs)
}

fn classify_ref(full_name: &str) -> Option<(RefKind, String)> {
    if let Some(name) = full_name.strip_prefix(BRANCH_PREFIX) {
        return Some((RefKind::Branch, name.to_owned()));
    }
    full_name
        .strip_prefix(TAG_PREFIX)
        .map(|name| (RefKind::Tag, name.to_owned()))
}

fn default_ref(repository: &gix::Repository, discovered: &DiscoveredRepo) -> String {
    if let Ok(Some(head)) = repository.head_name()
        && let Some(name) = head.as_bstr().to_string().strip_prefix(BRANCH_PREFIX)
    {
        return name.to_owned();
    }
    discovered
        .refs
        .iter()
        .find(|entry| entry.kind == RefKind::Branch)
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| DEFAULT_REF.to_owned())
}

fn read_tree(
    repository: &gix::Repository,
    sha: CommitSha,
    git_dir: &Path,
    work_dir: Option<&Path>,
) -> Result<Vec<DiscoveredEntry>> {
    let object_id = gix::ObjectId::try_from(sha.as_bytes().as_slice())
        .map_err(|error| IndexError::Git(format!("bad object name: {error}")))?;
    let commit = repository
        .find_commit(object_id)
        .map_err(|error| IndexError::Git(format!("commit {sha} unreadable: {error}")))?;
    let tree = commit
        .tree()
        .map_err(|error| IndexError::Git(format!("tree of {sha} unreadable: {error}")))?;
    let records = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|error| IndexError::Git(format!("tree walk of {sha} failed: {error}")))?;

    let mut entries = Vec::with_capacity(records.len());
    for record in records {
        if matches!(record.mode.kind(), EntryKind::Tree | EntryKind::Commit) {
            continue;
        }
        let rel_path = record.filepath.to_str_lossy().into_owned();
        entries.push(tree_entry(
            repository, rel_path, record.oid, git_dir, work_dir,
        ));
    }
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(entries)
}

fn tree_entry(
    repository: &gix::Repository,
    rel_path: String,
    oid: gix::ObjectId,
    git_dir: &Path,
    work_dir: Option<&Path>,
) -> DiscoveredEntry {
    let size = repository
        .find_header(oid)
        .map(|header| header.size())
        .unwrap_or_default();
    let pointer = (size <= MAX_LFS_POINTER_BYTES)
        .then(|| repository.find_object(oid).ok())
        .flatten()
        .and_then(|object| parse_lfs_pointer(&object.detach().data));

    let mut entry = DiscoveredEntry {
        rel_path,
        abs_path: None,
        size,
        stamp: None,
        is_symlink: false,
        sha256: None,
        git_sha1: commit_sha(oid.as_ref()),
        lfs_pointer: pointer.is_some(),
    };
    match pointer {
        Some(pointer) => {
            entry.size = pointer.size;
            entry.sha256 = Some(pointer.sha256);
            entry.git_sha1 = None;
            entry.abs_path = present(lfs_object_path(git_dir, &pointer.sha256.to_hex()));
        }
        None => {
            entry.abs_path = work_dir.and_then(|dir| present(dir.join(&entry.rel_path)));
        }
    }
    if let Some(path) = entry.abs_path.as_ref()
        && let Ok(metadata) = std::fs::metadata(path)
    {
        entry.size = metadata.len();
        entry.stamp = Some(walker::stamp_of(&metadata));
    }
    entry
}

fn present(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// `<git dir>/lfs/objects/<a>/<b>/<oid>`, the layout `git lfs` writes.
pub fn lfs_object_path(git_dir: &Path, oid_hex: &str) -> PathBuf {
    git_dir
        .join("lfs")
        .join("objects")
        .join(&oid_hex[0..2])
        .join(&oid_hex[2..4])
        .join(oid_hex)
}

fn commit_title(repository: &gix::Repository, sha: CommitSha) -> Option<String> {
    let object_id = gix::ObjectId::try_from(sha.as_bytes().as_slice()).ok()?;
    let commit = repository.find_commit(object_id).ok()?;
    let message = commit.message().ok()?;
    Some(message.summary().to_string())
}

fn commit_sha(id: &gix::hash::oid) -> Option<CommitSha> {
    id.as_bytes().try_into().ok().map(CommitSha::from_bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::hash::sha256_bytes;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "hoard")
            .env("GIT_AUTHOR_EMAIL", "hoard@example.invalid")
            .env("GIT_COMMITTER_NAME", "hoard")
            .env("GIT_COMMITTER_EMAIL", "hoard@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A clone holding one plain file, one LFS pointer whose object is present and one
    /// whose object is not.
    fn build_clone(root: &Path) -> (PathBuf, Vec<u8>) {
        let clone = root.join("org").join("repo");
        fs::create_dir_all(&clone).expect("clone dir is creatable");
        git(&clone, &["init", "--initial-branch=main", "--quiet"]);

        fs::write(clone.join("config.json"), b"{\"n\": 1}").expect("file is writable");
        let media = vec![9u8; 4096];
        let oid = sha256_bytes(&media).to_hex();
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            media.len()
        );
        fs::write(clone.join("model.safetensors"), &pointer).expect("pointer is writable");
        fs::write(
            clone.join("absent.safetensors"),
            pointer.replace(&oid, &"a".repeat(64)),
        )
        .expect("pointer is writable");

        let object = lfs_object_path(&clone.join(".git"), &oid);
        fs::create_dir_all(object.parent().expect("object has a parent"))
            .expect("lfs dir is creatable");
        fs::write(&object, &media).expect("lfs object is writable");

        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "first import", "--quiet"]);
        git(&clone, &["tag", "v1"]);
        (clone, media)
    }

    #[test]
    fn clone_urls_decode_to_repository_ids() {
        assert_eq!(
            repo_from_url("https://huggingface.co/hexgrad/Kokoro-82M.git")
                .expect("url decodes")
                .qualified(),
            "models/hexgrad/Kokoro-82M"
        );
        assert_eq!(
            repo_from_url("https://huggingface.co/datasets/mlabonne/FineTome-100k")
                .expect("url decodes")
                .qualified(),
            "datasets/mlabonne/FineTome-100k"
        );
        assert_eq!(
            repo_from_url("git@github.com:johnz86/hoard.git")
                .expect("url decodes")
                .qualified(),
            "models/johnz86/hoard"
        );
        assert!(repo_from_url("nonsense").is_none());
    }

    #[test]
    fn a_real_clone_indexes_refs_tree_entries_and_lfs_pointers() {
        let dir = TempDir::new().expect("temp dir");
        let (clone, media) = build_clone(dir.path());

        let repos = discover(dir.path(), &NoiseFilter::default()).expect("clones are readable");
        assert_eq!(repos.len(), 1);
        let repo = &repos[0];
        assert_eq!(repo.repo.qualified(), "models/org/repo");
        assert_eq!(repo.git_dir.as_deref(), Some(clone.join(".git").as_path()));
        assert_eq!(repo.default_ref, "main");

        let names: Vec<&str> = repo.refs.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["main", "v1"]);
        assert_eq!(repo.refs[0].kind, RefKind::Branch);
        assert_eq!(repo.refs[1].kind, RefKind::Tag);
        assert_eq!(repo.revisions.len(), 1);
        assert_eq!(repo.revisions[0].title.as_deref(), Some("first import"));

        let entries = &repo.revisions[0].entries;
        assert_eq!(entries.len(), 3);

        let absent = &entries[0];
        assert_eq!(absent.rel_path, "absent.safetensors");
        assert!(absent.lfs_pointer);
        assert!(!absent.present());
        assert_eq!(absent.size, media.len() as u64);

        let config = &entries[1];
        assert_eq!(config.rel_path, "config.json");
        assert!(!config.lfs_pointer);
        assert!(config.present());
        assert!(config.git_sha1.is_some());

        let model = &entries[2];
        assert_eq!(model.rel_path, "model.safetensors");
        assert!(model.lfs_pointer);
        assert!(model.present());
        assert_eq!(model.sha256, Some(sha256_bytes(&media)));
        assert_eq!(model.size, media.len() as u64);
        assert!(
            model
                .abs_path
                .as_ref()
                .is_some_and(|path| path.starts_with(clone.join(".git/lfs")))
        );
    }

    #[test]
    fn a_directory_with_no_clones_yields_nothing() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir_all(dir.path().join("plain")).expect("dir is creatable");
        assert!(
            discover(dir.path(), &NoiseFilter::default())
                .expect("root is readable")
                .is_empty()
        );
    }
}
