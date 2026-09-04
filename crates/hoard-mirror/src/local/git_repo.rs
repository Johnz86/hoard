//! A read-only view of a git clone on disk: refs, trees, blobs, commits and LFS objects.

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind as GitEntryKind;
use hoard_core::{CommitSha, Sha256};
use jiff::Timestamp;

use crate::error::{MirrorError, Result};

const LFS_POINTER_VERSION: &str = "version https://git-lfs.github.com/spec/v1";
const LFS_OID_PREFIX: &str = "oid sha256:";
const LFS_SIZE_PREFIX: &str = "size ";
const MAX_LFS_POINTER_BYTES: u64 = 1024;
const BRANCH_PREFIX: &str = "refs/heads/";
const TAG_PREFIX: &str = "refs/tags/";
const PREFERRED_DEFAULT_BRANCHES: [&str; 2] = ["main", "master"];

/// Whether a ref names a branch or a tag; hoard serves both as Hub revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefKind {
    Branch,
    Tag,
}

/// A branch or tag with the commit it resolves to, peeled through annotated tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    pub name: String,
    pub kind: RefKind,
    pub target: CommitSha,
    pub when: Option<Timestamp>,
}

/// Whether a tree entry is a file (blob, executable blob, symlink or submodule) or a
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    File,
    Directory,
}

/// The sha256 and declared size a git LFS pointer stands in for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LfsObject {
    pub oid: Sha256,
    pub size: u64,
    pub pointer_size: u64,
}

/// One entry of a tree listing. `oid` is always the git object name, so an LFS file
/// reports the sha1 of its pointer blob here and the sha256 under `lfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub kind: EntryKind,
    pub oid: CommitSha,
    pub size: u64,
    pub lfs: Option<LfsObject>,
}

/// Where the bytes of a blob live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSource {
    /// The blob is a plain git object and its bytes are inline.
    Loose { data: Vec<u8> },
    /// The blob is an LFS pointer and the media file is present in the LFS store.
    LfsFile { path: PathBuf },
    /// The blob is an LFS pointer whose media file this node does not hold.
    MissingLfs,
}

/// A single file of a revision, resolved through LFS when the blob is a pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub oid: CommitSha,
    pub size: u64,
    pub lfs: Option<LfsObject>,
    pub source: BlobSource,
}

/// One commit of a revision's history, newest first as the REFS tab shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: CommitSha,
    pub title: String,
    pub message: String,
    pub author: String,
    pub date: Timestamp,
}

/// A git clone this node serves, opened read-only.
///
/// `gix` is built here without its `parallel` feature, so the open repository handle it
/// hands out is reference counted rather than atomically shared and the view is neither
/// `Send` nor `Sync`. Open it, use it and drop it inside one `spawn_blocking` call.
#[derive(Debug)]
pub struct GitRepo {
    repo: gix::ThreadSafeRepository,
    git_dir: PathBuf,
}

impl GitRepo {
    /// Opens either a worktree root or the `.git` directory of a clone, bare or not.
    pub fn open(path: &Path) -> Result<GitRepo> {
        let repo = gix::open(path)
            .map_err(|error| MirrorError::RepoNotFound(format!("{}: {error}", path.display())))?
            .into_sync();
        let git_dir = repo.git_dir().to_path_buf();
        Ok(GitRepo { repo, git_dir })
    }

    /// Cheap probe used by discovery, true for worktrees, bare repos and linked worktrees.
    pub fn is_git_repo(path: &Path) -> bool {
        let dot_git = path.join(".git");
        if dot_git.is_file() {
            return true;
        }
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            path.to_path_buf()
        };
        git_dir.join("HEAD").is_file() && git_dir.join("objects").is_dir()
    }

    /// The directory holding `objects`, `refs` and the LFS store.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Branches and tags with the commit each resolves to.
    pub fn refs(&self) -> Result<Vec<GitRef>> {
        let repo = self.repo.to_thread_local();
        let platform = repo
            .references()
            .map_err(|error| MirrorError::Git(format!("refs unavailable: {error}")))?;
        let iter = platform
            .all()
            .map_err(|error| MirrorError::Git(format!("refs unavailable: {error}")))?;
        let mut refs = Vec::new();
        for entry in iter {
            let Ok(mut reference) = entry else { continue };
            let full_name = reference.name().as_bstr().to_string();
            let Some((kind, name)) = classify_ref(&full_name) else {
                continue;
            };
            let target = reference
                .peel_to_id()
                .map_err(|error| MirrorError::Git(format!("{full_name} does not peel: {error}")))?;
            let target_sha = commit_sha(target.as_ref())?;
            refs.push(GitRef {
                name,
                kind,
                target: target_sha,
                when: commit_timestamp(&repo, target.detach()),
            });
        }
        refs.sort_by(|left, right| (left.kind, &left.name).cmp(&(right.kind, &right.name)));
        Ok(refs)
    }

    /// The branch `HEAD` points at, falling back to `main`, `master` then any branch.
    pub fn default_branch(&self) -> Result<String> {
        let repo = self.repo.to_thread_local();
        if let Ok(Some(head)) = repo.head_name() {
            let full_name = head.as_bstr().to_string();
            if let Some(name) = full_name.strip_prefix(BRANCH_PREFIX) {
                return Ok(name.to_owned());
            }
        }
        let refs = self.refs()?;
        let branches = || refs.iter().filter(|item| item.kind == RefKind::Branch);
        for candidate in PREFERRED_DEFAULT_BRANCHES {
            if branches().any(|item| item.name == candidate) {
                return Ok(candidate.to_owned());
            }
        }
        branches()
            .map(|item| item.name.clone())
            .next()
            .ok_or_else(|| MirrorError::RevisionNotFound("HEAD".to_owned()))
    }

    /// Resolves a branch, tag, full sha or unambiguous short sha to its commit.
    pub fn resolve(&self, revision: &str) -> Result<CommitSha> {
        let repo = self.repo.to_thread_local();
        let not_found = || MirrorError::RevisionNotFound(revision.to_owned());
        let id = repo.rev_parse_single(revision).map_err(|_| not_found())?;
        let commit = repo
            .find_object(id)
            .map_err(|_| not_found())?
            .peel_to_kind(gix::object::Kind::Commit)
            .map_err(|_| not_found())?;
        commit_sha(commit.id.as_ref())
    }

    /// Lists a directory of a revision. `path` is empty for the root; `recursive` walks
    /// the whole subtree and reports directories alongside files.
    pub fn tree(&self, sha: CommitSha, path: &str, recursive: bool) -> Result<Vec<TreeEntry>> {
        let repo = self.repo.to_thread_local();
        let tree = subtree(&repo, sha, path)?;
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let mut entries = Vec::new();
        if recursive {
            let records = tree
                .traverse()
                .breadthfirst
                .files()
                .map_err(|error| MirrorError::Git(format!("tree walk failed: {error}")))?;
            entries.reserve(records.len());
            for record in records {
                entries.push(tree_entry(
                    &repo,
                    format!("{prefix}{}", record.filepath.to_str_lossy()),
                    record.oid,
                    record.mode.kind(),
                )?);
            }
        } else {
            for record in tree.iter() {
                let record = record
                    .map_err(|error| MirrorError::Git(format!("tree is corrupt: {error}")))?;
                entries.push(tree_entry(
                    &repo,
                    format!("{prefix}{}", record.filename().to_str_lossy()),
                    record.object_id(),
                    record.mode().kind(),
                )?);
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    /// Resolves one file of a revision, following an LFS pointer into the LFS store.
    pub fn blob(&self, sha: CommitSha, path: &str) -> Result<BlobRef> {
        let repo = self.repo.to_thread_local();
        let entry = file_entry(&repo, sha, path)?;
        let oid = entry.object_id();
        let size = object_size(&repo, oid)?;
        let small = (size <= MAX_LFS_POINTER_BYTES)
            .then(|| blob_data(&repo, oid))
            .transpose()?;
        let pointer = small
            .as_deref()
            .and_then(|bytes| parse_lfs_pointer(bytes, size));
        let oid = commit_sha(oid.as_ref())?;
        match pointer {
            Some(lfs) => {
                let media = self.lfs_object_path(&lfs.oid);
                let source = if media.is_file() {
                    BlobSource::LfsFile { path: media }
                } else {
                    BlobSource::MissingLfs
                };
                Ok(BlobRef {
                    oid,
                    size: lfs.size,
                    lfs: Some(lfs),
                    source,
                })
            }
            None => {
                let data = match small {
                    Some(data) => data,
                    None => blob_data(&repo, entry.object_id())?,
                };
                Ok(BlobRef {
                    oid,
                    size,
                    lfs: None,
                    source: BlobSource::Loose { data },
                })
            }
        }
    }

    /// History of a revision, newest commit first.
    pub fn commits(&self, sha: CommitSha, limit: usize) -> Result<Vec<CommitInfo>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let repo = self.repo.to_thread_local();
        let walk = repo
            .rev_walk(Some(object_id(sha)?))
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .all()
            .map_err(|error| MirrorError::Git(format!("history unavailable: {error}")))?;
        let mut commits = Vec::with_capacity(limit);
        for info in walk.take(limit) {
            let info =
                info.map_err(|error| MirrorError::Git(format!("history is broken: {error}")))?;
            let commit = repo
                .find_commit(info.id)
                .map_err(|error| MirrorError::Git(format!("commit unreadable: {error}")))?;
            commits.push(commit_info(&commit)?);
        }
        Ok(commits)
    }

    /// Reads a text file of a revision, used for README rendering. `None` when the path is
    /// absent, not valid UTF-8, or an LFS pointer whose media file is missing.
    pub fn read_text(&self, sha: CommitSha, path: &str) -> Result<Option<String>> {
        let blob = match self.blob(sha, path) {
            Ok(blob) => blob,
            Err(MirrorError::EntryNotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let bytes = match blob.source {
            BlobSource::Loose { data } => data,
            BlobSource::LfsFile { path } => std::fs::read(path)?,
            BlobSource::MissingLfs => return Ok(None),
        };
        Ok(String::from_utf8(bytes).ok())
    }

    /// `<git_dir>/lfs/objects/<a>/<b>/<oid>`, the layout `git lfs` writes.
    pub fn lfs_object_path(&self, oid: &Sha256) -> PathBuf {
        let hex = oid.to_hex();
        self.git_dir
            .join("lfs")
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }
}

fn classify_ref(full_name: &str) -> Option<(RefKind, String)> {
    if let Some(name) = full_name.strip_prefix(BRANCH_PREFIX) {
        return Some((RefKind::Branch, name.to_owned()));
    }
    full_name
        .strip_prefix(TAG_PREFIX)
        .map(|name| (RefKind::Tag, name.to_owned()))
}

fn commit_sha(id: &gix::hash::oid) -> Result<CommitSha> {
    let bytes: [u8; 20] = id
        .as_bytes()
        .try_into()
        .map_err(|_| MirrorError::Git(format!("object name {id} is not a sha1")))?;
    Ok(CommitSha::from_bytes(bytes))
}

fn object_id(sha: CommitSha) -> Result<gix::ObjectId> {
    gix::ObjectId::try_from(sha.as_bytes().as_slice())
        .map_err(|error| MirrorError::Git(format!("bad object name: {error}")))
}

fn commit_timestamp(repo: &gix::Repository, id: gix::ObjectId) -> Option<Timestamp> {
    let commit = repo.find_commit(id).ok()?;
    let time = commit.time().ok()?;
    Timestamp::from_second(time.seconds).ok()
}

fn subtree<'repo>(
    repo: &'repo gix::Repository,
    sha: CommitSha,
    path: &str,
) -> Result<gix::Tree<'repo>> {
    let commit = repo
        .find_commit(object_id(sha)?)
        .map_err(|_| MirrorError::RevisionNotFound(sha.to_hex()))?;
    let root = commit
        .tree()
        .map_err(|error| MirrorError::Git(format!("root tree unreadable: {error}")))?;
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(root);
    }
    let entry = root
        .lookup_entry_by_path(path)
        .map_err(|error| MirrorError::Git(format!("tree lookup failed: {error}")))?
        .ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    entry
        .object()
        .map_err(|error| MirrorError::Git(format!("tree unreadable: {error}")))?
        .try_into_tree()
        .map_err(|_| MirrorError::EntryNotFound(path.to_owned()))
}

fn file_entry<'repo>(
    repo: &'repo gix::Repository,
    sha: CommitSha,
    path: &str,
) -> Result<gix::object::tree::Entry<'repo>> {
    let path = path.trim_matches('/');
    let root = subtree(repo, sha, "")?;
    let entry = root
        .lookup_entry_by_path(path)
        .map_err(|error| MirrorError::Git(format!("tree lookup failed: {error}")))?
        .ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    if entry.mode().kind() == GitEntryKind::Tree {
        return Err(MirrorError::EntryNotFound(path.to_owned()));
    }
    Ok(entry)
}

fn object_size(repo: &gix::Repository, oid: gix::ObjectId) -> Result<u64> {
    repo.find_header(oid)
        .map(|header| header.size())
        .map_err(|error| MirrorError::Git(format!("object header unreadable: {error}")))
}

fn blob_data(repo: &gix::Repository, oid: gix::ObjectId) -> Result<Vec<u8>> {
    repo.find_object(oid)
        .map(|object| object.detach().data)
        .map_err(|error| MirrorError::Git(format!("object unreadable: {error}")))
}

fn tree_entry(
    repo: &gix::Repository,
    path: String,
    oid: gix::ObjectId,
    kind: GitEntryKind,
) -> Result<TreeEntry> {
    let entry_kind = match kind {
        GitEntryKind::Tree => EntryKind::Directory,
        GitEntryKind::Blob
        | GitEntryKind::BlobExecutable
        | GitEntryKind::Link
        | GitEntryKind::Commit => EntryKind::File,
    };
    if entry_kind == EntryKind::Directory {
        return Ok(TreeEntry {
            path,
            kind: entry_kind,
            oid: commit_sha(oid.as_ref())?,
            size: 0,
            lfs: None,
        });
    }
    let size = object_size(repo, oid)?;
    let lfs = if size <= MAX_LFS_POINTER_BYTES {
        parse_lfs_pointer(&blob_data(repo, oid)?, size)
    } else {
        None
    };
    Ok(TreeEntry {
        path,
        kind: entry_kind,
        oid: commit_sha(oid.as_ref())?,
        size: lfs.map_or(size, |pointer| pointer.size),
        lfs,
    })
}

fn commit_info(commit: &gix::Commit<'_>) -> Result<CommitInfo> {
    let decoded = commit
        .decode()
        .map_err(|error| MirrorError::Git(format!("commit unreadable: {error}")))?;
    let message = decoded.message.to_str_lossy().into_owned();
    let title = decoded
        .message()
        .title
        .trim()
        .to_str_lossy()
        .replace(['\n', '\r'], " ");
    let author = decoded
        .author()
        .map_err(|error| MirrorError::Git(format!("commit author unreadable: {error}")))?;
    let date = Timestamp::from_second(author.seconds())
        .map_err(|error| MirrorError::Git(format!("commit time out of range: {error}")))?;
    Ok(CommitInfo {
        id: commit_sha(commit.id().as_ref())?,
        title,
        message,
        author: format!("{} <{}>", author.name, author.email),
        date,
    })
}

fn parse_lfs_pointer(data: &[u8], pointer_size: u64) -> Option<LfsObject> {
    let text = std::str::from_utf8(data).ok()?;
    if !text.starts_with(LFS_POINTER_VERSION) {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(LFS_OID_PREFIX) {
            oid = value.trim().parse::<Sha256>().ok();
        } else if let Some(value) = line.strip_prefix(LFS_SIZE_PREFIX) {
            size = value.trim().parse::<u64>().ok();
        }
    }
    Some(LfsObject {
        oid: oid?,
        size: size?,
        pointer_size,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is utf-8")
    }

    fn write(dir: &Path, path: &str, contents: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("parent exists");
        }
        fs::write(full, contents).expect("file written");
    }

    fn sample_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();
        git(path, &["init", "--initial-branch=main", "--quiet"]);
        git(path, &["config", "user.email", "hoard@example.invalid"]);
        git(path, &["config", "user.name", "Hoard Test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        write(path, "README.md", "# sample\n");
        write(path, "config/params.json", "{\"n\": 1}\n");
        git(path, &["add", "."]);
        git(
            path,
            &["commit", "--quiet", "-m", "first commit\n\nbody line"],
        );
        write(path, "notes.txt", "second\n");
        git(path, &["add", "."]);
        git(path, &["commit", "--quiet", "-m", "second commit"]);
        git(path, &["tag", "v1.0"]);
        git(path, &["branch", "side"]);
        dir
    }

    fn open(dir: &TempDir) -> GitRepo {
        GitRepo::open(dir.path()).expect("repo opens")
    }

    #[test]
    fn probe_recognises_worktrees_and_git_dirs() {
        let dir = sample_repo();
        assert!(GitRepo::is_git_repo(dir.path()));
        assert!(GitRepo::is_git_repo(&dir.path().join(".git")));
        let empty = tempfile::tempdir().expect("temp dir");
        assert!(!GitRepo::is_git_repo(empty.path()));
    }

    #[test]
    fn opens_worktree_root_and_git_directory() {
        let dir = sample_repo();
        let from_root = open(&dir);
        let from_git_dir = GitRepo::open(&dir.path().join(".git")).expect("git dir opens");
        assert_eq!(from_root.git_dir(), from_git_dir.git_dir());
        assert_eq!(
            from_root.refs().expect("refs"),
            from_git_dir.refs().expect("refs")
        );
    }

    #[test]
    fn lists_branches_and_tags_with_targets() {
        let dir = sample_repo();
        let repo = open(&dir);
        let refs = repo.refs().expect("refs");
        let names: Vec<_> = refs
            .iter()
            .map(|item| (item.kind, item.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (RefKind::Branch, "main"),
                (RefKind::Branch, "side"),
                (RefKind::Tag, "v1.0"),
            ]
        );
        let head = git(dir.path(), &["rev-parse", "main"]).trim().to_owned();
        for item in &refs {
            assert_eq!(item.target.to_hex(), head);
            assert!(item.when.is_some());
        }
        assert_eq!(repo.default_branch().expect("default branch"), "main");
    }

    #[test]
    fn resolves_every_revision_form() {
        let dir = sample_repo();
        let repo = open(&dir);
        let head = git(dir.path(), &["rev-parse", "main"]).trim().to_owned();
        let expected: CommitSha = head.parse().expect("sha parses");
        assert_eq!(repo.resolve("main").expect("branch"), expected);
        assert_eq!(repo.resolve("v1.0").expect("tag"), expected);
        assert_eq!(repo.resolve(&head).expect("full sha"), expected);
        assert_eq!(repo.resolve(&head[..8]).expect("short sha"), expected);
        assert!(matches!(
            repo.resolve("no-such-ref"),
            Err(MirrorError::RevisionNotFound(_))
        ));
    }

    #[test]
    fn tree_listing_matches_git_object_names() {
        let dir = sample_repo();
        let repo = open(&dir);
        let sha = repo.resolve("main").expect("main resolves");

        let root = repo.tree(sha, "", false).expect("root tree");
        let shallow: Vec<_> = root
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind))
            .collect();
        assert_eq!(
            shallow,
            vec![
                ("README.md", EntryKind::File),
                ("config", EntryKind::Directory),
                ("notes.txt", EntryKind::File),
            ]
        );

        let deep = repo.tree(sha, "", true).expect("recursive tree");
        let paths: Vec<_> = deep.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["README.md", "config", "config/params.json", "notes.txt"]
        );

        let nested = repo.tree(sha, "config", false).expect("subtree");
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].path, "config/params.json");

        let readme = deep
            .iter()
            .find(|entry| entry.path == "README.md")
            .expect("readme listed");
        let hashed = git(dir.path(), &["hash-object", "README.md"])
            .trim()
            .to_owned();
        assert_eq!(readme.oid.to_hex(), hashed);
        assert_eq!(readme.size, 9);
        assert!(readme.lfs.is_none());
    }

    #[test]
    fn blob_returns_bytes_and_text() {
        let dir = sample_repo();
        let repo = open(&dir);
        let sha = repo.resolve("main").expect("main resolves");
        let blob = repo.blob(sha, "README.md").expect("blob reads");
        assert_eq!(blob.size, 9);
        assert!(blob.lfs.is_none());
        assert_eq!(
            blob.source,
            BlobSource::Loose {
                data: b"# sample\n".to_vec()
            }
        );
        assert_eq!(
            repo.read_text(sha, "README.md").expect("text reads"),
            Some("# sample\n".to_owned())
        );
        assert_eq!(
            repo.read_text(sha, "absent.md").expect("absent is none"),
            None
        );
        assert!(matches!(
            repo.blob(sha, "config"),
            Err(MirrorError::EntryNotFound(_))
        ));
    }

    #[test]
    fn commits_are_newest_first() {
        let dir = sample_repo();
        let repo = open(&dir);
        let sha = repo.resolve("main").expect("main resolves");
        let commits = repo.commits(sha, 10).expect("history");
        let titles: Vec<_> = commits.iter().map(|commit| commit.title.as_str()).collect();
        assert_eq!(titles, vec!["second commit", "first commit"]);
        assert_eq!(commits[0].id, sha);
        assert_eq!(commits[0].author, "Hoard Test <hoard@example.invalid>");
        assert!(commits[1].message.contains("body line"));
        assert!(commits[0].date >= commits[1].date);
        assert_eq!(repo.commits(sha, 1).expect("limit honoured").len(), 1);
        assert!(repo.commits(sha, 0).expect("zero limit").is_empty());
    }

    #[test]
    fn lfs_pointer_resolves_to_media_file_and_reports_missing() {
        let dir = sample_repo();
        let path = dir.path();
        let payload = b"synthetic weights";
        let digest = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(payload);
            hex::encode(hasher.finalize())
        };
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize {}\n",
            payload.len()
        );
        write(path, "model.bin", &pointer);
        git(path, &["add", "."]);
        git(path, &["commit", "--quiet", "-m", "add lfs pointer"]);

        let media = path
            .join(".git/lfs/objects")
            .join(&digest[0..2])
            .join(&digest[2..4]);
        fs::create_dir_all(&media).expect("lfs dirs");
        let media = media.join(&digest);
        fs::write(&media, payload).expect("media written");

        let repo = open(&dir);
        let sha = repo.resolve("main").expect("main resolves");
        let entry = repo
            .tree(sha, "", false)
            .expect("tree")
            .into_iter()
            .find(|entry| entry.path == "model.bin")
            .expect("pointer listed");
        let lfs = entry.lfs.expect("entry is lfs");
        assert_eq!(lfs.oid.to_hex(), digest);
        assert_eq!(lfs.size, payload.len() as u64);
        assert_eq!(entry.size, payload.len() as u64);
        assert_eq!(lfs.pointer_size, pointer.len() as u64);
        assert_eq!(
            entry.oid.to_hex(),
            git(path, &["hash-object", "model.bin"]).trim()
        );

        let blob = repo.blob(sha, "model.bin").expect("blob resolves");
        assert_eq!(blob.size, payload.len() as u64);
        assert_eq!(
            blob.source,
            BlobSource::LfsFile {
                path: media.clone()
            }
        );

        fs::remove_file(&media).expect("media removed");
        let blob = repo.blob(sha, "model.bin").expect("blob resolves");
        assert_eq!(blob.source, BlobSource::MissingLfs);
        assert_eq!(
            repo.read_text(sha, "model.bin").expect("missing media"),
            None
        );
    }

    #[test]
    fn pointer_parsing_rejects_other_blobs() {
        assert!(parse_lfs_pointer(b"# sample\n", 9).is_none());
        assert!(parse_lfs_pointer(LFS_POINTER_VERSION.as_bytes(), 41).is_none());
        let pointer = format!(
            "{LFS_POINTER_VERSION}\noid sha256:{}\nsize 12\n",
            "aa".repeat(32)
        );
        let parsed =
            parse_lfs_pointer(pointer.as_bytes(), pointer.len() as u64).expect("pointer parses");
        assert_eq!(parsed.size, 12);
        assert_eq!(parsed.pointer_size, pointer.len() as u64);
    }

    #[test]
    fn bare_repositories_are_readable() {
        let source = sample_repo();
        let dir = tempfile::tempdir().expect("temp dir");
        let bare = dir.path().join("mirror.git");
        let source_path = source.path().to_string_lossy().into_owned();
        git(
            dir.path(),
            &["clone", "--bare", "--quiet", &source_path, "mirror.git"],
        );
        let repo = GitRepo::open(&bare).expect("bare repo opens");
        assert_eq!(repo.default_branch().expect("default branch"), "main");
        let sha = repo.resolve("main").expect("main resolves");
        assert_eq!(repo.tree(sha, "", true).expect("tree").len(), 4);
    }
}
