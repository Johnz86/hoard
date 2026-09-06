//! What a nominated root holds, in the one shape the scan pipeline consumes.
//!
//! Three layouts reach the same result: the HuggingFace cache, a directory of git clones
//! and bare folders. Each yields repositories with revisions, refs and entries; nothing
//! here reads a header, hashes bytes or reads the catalog beyond the noise list.

pub mod git_clone;
pub mod hf_cache;
pub mod loose;
pub mod walker;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{CommitSha, GitSha1, RepoId, Sha256};

use crate::config::{SourceConfig, SourceKind};
use crate::error::{IndexError, Result};
use crate::hash::FileStamp;

pub use walker::{DiscoveredFile, NoiseFilter, WalkOptions, walk};

/// Whether a name points at a moving branch or a fixed tag. Ordered so branches sort ahead
/// of tags, which is the order the REFS tab presents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Branch,
    Tag,
}

impl RefKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RefKind::Branch => "branch",
            RefKind::Tag => "tag",
        }
    }
}

impl fmt::Display for RefKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RefKind {
    type Err = IndexError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "branch" => Ok(RefKind::Branch),
            "tag" => Ok(RefKind::Tag),
            other => Err(IndexError::Config(format!("unknown ref kind {other:?}"))),
        }
    }
}

/// One path inside a revision, with anything the layout already proves about its bytes.
///
/// `abs_path` is the file to read: the blob a cache snapshot symlinks to, the LFS object a
/// pointer names, the file itself for a loose folder. It is `None` when the layout knows
/// the path but this disk does not hold the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEntry {
    pub rel_path: String,
    pub abs_path: Option<PathBuf>,
    pub size: u64,
    pub stamp: Option<FileStamp>,
    pub is_symlink: bool,
    pub sha256: Option<Sha256>,
    pub git_sha1: Option<GitSha1>,
    pub lfs_pointer: bool,
}

impl DiscoveredEntry {
    pub fn from_file(file: DiscoveredFile) -> Self {
        Self {
            rel_path: file.rel_path,
            abs_path: Some(file.abs_path),
            size: file.size,
            stamp: Some(file.stamp),
            is_symlink: file.is_symlink,
            sha256: None,
            git_sha1: None,
            lfs_pointer: false,
        }
    }

    /// True when this node holds the bytes and can sniff them.
    pub fn present(&self) -> bool {
        self.abs_path.is_some()
    }
}

/// How a revision is named. Git and cache revisions carry the commit they came from; a
/// loose folder has none until its contents have been fingerprinted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionName {
    Commit(CommitSha),
    Synthetic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRevision {
    pub name: RevisionName,
    pub title: Option<String>,
    pub author: Option<String>,
    pub committed: Option<Timestamp>,
    pub entries: Vec<DiscoveredEntry>,
}

impl DiscoveredRevision {
    pub fn new(name: RevisionName, entries: Vec<DiscoveredEntry>) -> Self {
        Self {
            name,
            title: None,
            author: None,
            committed: None,
            entries,
        }
    }
}

/// A named pointer at one of the repository's revisions, by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRef {
    pub name: String,
    pub kind: RefKind,
    pub revision: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    pub repo: RepoId,
    pub root_path: PathBuf,
    pub git_dir: Option<PathBuf>,
    pub default_ref: String,
    pub revisions: Vec<DiscoveredRevision>,
    pub refs: Vec<DiscoveredRef>,
}

impl DiscoveredRepo {
    pub fn new(repo: RepoId, root_path: PathBuf) -> Self {
        Self {
            repo,
            root_path,
            git_dir: None,
            default_ref: DEFAULT_REF.to_owned(),
            revisions: Vec::new(),
            refs: Vec::new(),
        }
    }

    pub fn file_count(&self) -> usize {
        self.revisions
            .iter()
            .map(|revision| revision.entries.len())
            .sum()
    }
}

/// The branch name a repository presents when nothing else names one.
pub const DEFAULT_REF: &str = "main";

/// Everything one source yielded, tagged with the layout that produced it. An `auto`
/// source resolves to one of these, and the resolved kind is what the source row records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovered {
    HfCache(Vec<DiscoveredRepo>),
    Git(Vec<DiscoveredRepo>),
    Loose(Vec<DiscoveredRepo>),
}

impl Discovered {
    pub fn kind(&self) -> SourceKind {
        match self {
            Discovered::HfCache(_) => SourceKind::HfCache,
            Discovered::Git(_) => SourceKind::Git,
            Discovered::Loose(_) => SourceKind::Loose,
        }
    }

    pub fn repos(&self) -> &[DiscoveredRepo] {
        match self {
            Discovered::HfCache(repos) | Discovered::Git(repos) | Discovered::Loose(repos) => repos,
        }
    }

    pub fn into_repos(self) -> Vec<DiscoveredRepo> {
        match self {
            Discovered::HfCache(repos) | Discovered::Git(repos) | Discovered::Loose(repos) => repos,
        }
    }

    pub fn file_count(&self) -> usize {
        self.repos().iter().map(DiscoveredRepo::file_count).sum()
    }
}

/// Reads one source, resolving `auto` against what the directory actually looks like.
pub fn discover(source: &SourceConfig, noise: &NoiseFilter) -> Result<Discovered> {
    let options = WalkOptions::from_source(source)?;
    let root = source.path.as_path();
    match resolve_kind(source.kind, root) {
        SourceKind::HfCache => Ok(Discovered::HfCache(hf_cache::discover(
            root, &options, noise,
        )?)),
        SourceKind::Git => Ok(Discovered::Git(git_clone::discover(root, &options, noise)?)),
        SourceKind::Loose | SourceKind::Auto => Ok(Discovered::Loose(loose::discover(
            root,
            &source.name,
            source.repo_per_folder,
            &options,
            noise,
        )?)),
    }
}

/// Decides what a root is by looking at it: cache repo directories make it a cache, a git
/// directory or clones beneath it make it git, anything else is loose.
pub fn resolve_kind(kind: SourceKind, root: &Path) -> SourceKind {
    if kind != SourceKind::Auto {
        return kind;
    }
    if git_clone::is_git_repo(root) {
        return SourceKind::Git;
    }
    let Ok(children) = std::fs::read_dir(root) else {
        return SourceKind::Loose;
    };
    let mut clones = false;
    for child in children.flatten() {
        let path = child.path();
        if !path.is_dir() {
            continue;
        }
        if hf_cache::repo_dir_name(&path).is_some() {
            return SourceKind::HfCache;
        }
        clones = clones || git_clone::is_git_repo(&path);
    }
    if clones {
        SourceKind::Git
    } else {
        SourceKind::Loose
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn ref_kinds_round_trip_their_spelling() {
        for kind in [RefKind::Branch, RefKind::Tag] {
            assert_eq!(kind.as_str().parse::<RefKind>().expect("kind parses"), kind);
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert!("note".parse::<RefKind>().is_err());
    }

    #[test]
    fn branches_sort_ahead_of_tags() {
        let mut kinds = vec![RefKind::Tag, RefKind::Branch, RefKind::Tag];
        kinds.sort();
        assert_eq!(kinds, [RefKind::Branch, RefKind::Tag, RefKind::Tag]);
    }

    #[test]
    fn auto_recognizes_a_cache_a_clone_directory_and_a_bare_folder() {
        let dir = TempDir::new().expect("temp dir");
        let cache = dir.path().join("cache");
        fs::create_dir_all(cache.join("models--org--name/snapshots")).expect("tree is creatable");
        assert_eq!(resolve_kind(SourceKind::Auto, &cache), SourceKind::HfCache);

        let clones = dir.path().join("clones");
        fs::create_dir_all(clones.join("repo/.git/objects")).expect("tree is creatable");
        fs::write(clones.join("repo/.git/HEAD"), b"ref: refs/heads/main\n")
            .expect("head is writable");
        assert_eq!(resolve_kind(SourceKind::Auto, &clones), SourceKind::Git);
        assert_eq!(
            resolve_kind(SourceKind::Auto, &clones.join("repo")),
            SourceKind::Git
        );

        let plain = dir.path().join("models");
        fs::create_dir_all(plain.join("loras")).expect("tree is creatable");
        assert_eq!(resolve_kind(SourceKind::Auto, &plain), SourceKind::Loose);

        assert_eq!(
            resolve_kind(SourceKind::HfCache, &plain),
            SourceKind::HfCache
        );
    }

    #[test]
    fn a_discovered_source_reports_its_resolved_kind_and_file_count() {
        let dir = TempDir::new().expect("temp dir");
        fs::create_dir_all(dir.path().join("loras")).expect("tree is creatable");
        fs::write(dir.path().join("loras/a.safetensors"), b"a").expect("file is writable");

        let source = SourceConfig {
            name: "models".to_owned(),
            path: dir.path().to_path_buf(),
            kind: SourceKind::Auto,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: true,
        };
        let discovered = discover(&source, &NoiseFilter::default()).expect("discovery succeeds");
        assert_eq!(discovered.kind(), SourceKind::Loose);
        assert_eq!(discovered.file_count(), 1);
        assert_eq!(discovered.repos().len(), 1);
        assert_eq!(
            discovered.into_repos()[0].repo.qualified(),
            "models/models/loras"
        );
    }
}
