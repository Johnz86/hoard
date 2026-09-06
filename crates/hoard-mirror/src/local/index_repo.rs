//! Hub metadata and byte locations answered from the native catalog.
//!
//! Every route backed here reads the files of one revision once and derives the Hub shapes
//! from them, so a repository this node already holds is served without opening git and
//! without touching the network.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use hoard_core::{ArtifactId, CommitSha, RepoId, RepoRowId, RepoType, Sha256, clean_repo_path};
use hoard_index::IndexError;
use hoard_index::catalog::{
    Catalog, EntryDetail, RepositoryDetail, RepositoryFilter, RevisionDetail,
};
use hoard_index::hash::git_blob_sha1_bytes;
use hoard_index::sniff::parse_readme;
use jiff::Timestamp;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::error::{MirrorError, Result};
use crate::model::{
    CommitAuthor, CommitEntry, HubTimestamp, LastCommit, LfsInfo, PathsInfoEntry, RepoInfo,
    SecurityInfo, Sibling, TreeEntry,
};

/// Files at least this large live in git LFS under the Hub's default `.gitattributes`, so
/// their `etag` is the sha256 rather than the git blob sha1.
pub const LFS_THRESHOLD: u64 = 10 * 1024 * 1024;

/// The `oid` reported for an object whose git name this node has never computed. Forty
/// zeros is git's own spelling for "no such object", so a client reads it as unknown
/// rather than as a hash that failed to verify.
pub const UNKNOWN_OID: &str = "0000000000000000000000000000000000000000";

/// The commit listing cap the Hub applies.
pub const MAX_COMMITS: u32 = 50;

const README_PATH: &str = "README.md";
const README_LIMIT: u64 = 512 * 1024;
const DEFAULT_COMMIT_TITLE: &str = "Initial commit";

/// Where the bytes of one path in one revision live on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLocation {
    pub abs_path: PathBuf,
    pub size: u64,
    pub sha256: Option<Sha256>,
    pub git_sha1: Option<CommitSha>,
    pub is_lfs: bool,
    /// The bytes on disk are the LFS pointer rather than the media it stands for, so this
    /// path cannot be served from the index alone.
    pub lfs_pointer_only: bool,
    pub artifact_id: Option<ArtifactId>,
    pub repo_id: RepoRowId,
    pub commit: CommitSha,
}

/// Which repositories `GET /api/{repo_type}` should list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListQuery {
    pub repo_type: RepoType,
    pub search: Option<String>,
    pub author: Option<String>,
    pub limit: u32,
}

impl RepoListQuery {
    pub fn new(repo_type: RepoType, limit: u32) -> Self {
        Self {
            repo_type,
            search: None,
            author: None,
            limit,
        }
    }
}

/// One file of a revision with everything the Hub shapes need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevisionFile {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) abs_path: Option<PathBuf>,
    pub(crate) sha256: Option<Sha256>,
    pub(crate) git_sha1: Option<CommitSha>,
    pub(crate) artifact: Option<ArtifactId>,
    pub(crate) lfs_pointer: bool,
}

impl RevisionFile {
    fn of(entry: EntryDetail) -> Self {
        Self {
            path: entry.rel_path,
            size: entry.size,
            abs_path: entry.abs_path.map(PathBuf::from),
            sha256: entry.sha256,
            git_sha1: entry.git_sha1,
            artifact: entry.artifact_id,
            lfs_pointer: entry.lfs_pointer,
        }
    }

    fn is_lfs(&self) -> bool {
        self.lfs_pointer || self.size >= LFS_THRESHOLD
    }

    fn lfs_info(&self) -> Option<LfsInfo> {
        let oid = self.sha256?;
        Some(LfsInfo {
            oid: oid.to_hex(),
            size: self.size,
            pointer_size: lfs_pointer_text(&oid, self.size).len() as u64,
        })
    }

    /// The git object name of this path: the sha1 of the LFS pointer git holds in its
    /// place for an LFS file, and the sha1 of the bytes themselves otherwise.
    fn oid(&self) -> String {
        if self.is_lfs()
            && let Some(sha256) = self.sha256
        {
            let pointer = lfs_pointer_text(&sha256, self.size);
            return git_blob_sha1_bytes(pointer.as_bytes()).to_hex();
        }
        self.git_sha1
            .map_or_else(|| UNKNOWN_OID.to_owned(), CommitSha::to_hex)
    }

    fn tree_entry(&self) -> TreeEntry {
        let mut entry = TreeEntry::file(self.oid(), self.size, &self.path);
        if self.is_lfs() {
            entry.lfs = self.lfs_info();
        }
        entry
    }
}

/// The canonical git LFS pointer for a content hash, which is what git stores in place of
/// the media file and what the Hub reports as `pointerSize`.
pub fn lfs_pointer_text(oid: &Sha256, size: u64) -> String {
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {size}\n",
        oid.to_hex()
    )
}

/// Runs catalog work off the reactor: the catalog is a file, so every read and write of it
/// belongs on a blocking thread.
pub(crate) async fn on_catalog<T, F>(catalog: &Arc<Catalog>, f: F) -> Result<T>
where
    F: FnOnce(&Catalog) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let catalog = Arc::clone(catalog);
    tokio::task::spawn_blocking(move || f(&catalog))
        .await
        .map_err(|_| MirrorError::Index(IndexError::TaskJoin))?
}

/// The repository as this node holds it. A repo only a peer holds is not servable here.
pub(crate) fn repository(catalog: &Catalog, repo: &RepoId) -> Result<RepositoryDetail> {
    catalog
        .repository(repo)?
        .ok_or_else(|| MirrorError::RepoNotFound(repo.qualified()))
}

pub(crate) fn revision(
    catalog: &Catalog,
    repository: &RepositoryDetail,
    rev: &str,
) -> Result<RevisionDetail> {
    catalog
        .resolve_revision(repository, rev)
        .ok_or_else(|| MirrorError::RevisionNotFound(rev.to_owned()))
}

pub(crate) fn revision_by_sha(
    repository: &RepositoryDetail,
    sha: &CommitSha,
) -> Result<RevisionDetail> {
    repository
        .revisions
        .iter()
        .find(|revision| revision.sha.as_ref() == Some(sha))
        .cloned()
        .ok_or_else(|| MirrorError::RevisionNotFound(sha.to_hex()))
}

/// Every present file of a revision, sorted by path, read in one pass so the cost is bound
/// by the revision rather than by how many paths a request asked about.
pub(crate) fn revision_files(
    catalog: &Catalog,
    repository: &RepositoryDetail,
    revision: &RevisionDetail,
) -> Result<Vec<RevisionFile>> {
    let mut files: Vec<RevisionFile> = catalog
        .revision_entries(repository.id, revision.ordinal)?
        .into_iter()
        .filter(|entry| entry.present)
        .map(RevisionFile::of)
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// One file of a revision, named by path, without reading the rest of the revision.
pub(crate) fn revision_file(
    catalog: &Catalog,
    repository: &RepositoryDetail,
    revision: &RevisionDetail,
    path: &str,
) -> Result<Option<RevisionFile>> {
    Ok(catalog
        .revision_entry(repository.id, revision.ordinal, path)?
        .map(RevisionFile::of))
}

/// `/api/{repo_type}/{ref}` and `/api/{repo_type}/{ref}/revision/{revision}`.
pub async fn repo_info(catalog: &Arc<Catalog>, repo: &RepoId, revision: &str) -> Result<RepoInfo> {
    let wanted = repo.clone();
    let rev = revision.to_owned();
    on_catalog(catalog, move |catalog| {
        let record = repository(catalog, &wanted)?;
        let found = self::revision(catalog, &record, &rev)?;
        let files = revision_files(catalog, &record, &found)?;
        let card = read_card(&files);
        Ok(build_repo_info(
            &wanted,
            &found,
            files.iter().map(|file| Sibling::new(&file.path)).collect(),
            card,
        ))
    })
    .await
}

/// `/api/{repo_type}/{ref}/tree/{revision}/{path}`.
pub async fn tree(
    catalog: &Arc<Catalog>,
    repo: &RepoId,
    sha: &CommitSha,
    path: &str,
    listing: TreeListing,
) -> Result<Vec<TreeEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    let prefix =
        clean_repo_path(path).ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    on_catalog(catalog, move |catalog| {
        let record = repository(catalog, &wanted)?;
        let found = revision_by_sha(&record, &sha)?;
        let files = revision_files(catalog, &record, &found)?;
        Ok(build_tree(&files, &prefix, listing, &found))
    })
    .await
}

/// `/api/{repo_type}/{ref}/paths-info/{revision}`, answered for every requested path from
/// one pass over the revision rather than one upstream call per path.
pub async fn paths_info(
    catalog: &Arc<Catalog>,
    repo: &RepoId,
    sha: &CommitSha,
    paths: &[String],
) -> Result<Vec<PathsInfoEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    let paths = paths.to_vec();
    on_catalog(catalog, move |catalog| {
        let record = repository(catalog, &wanted)?;
        let found = revision_by_sha(&record, &sha)?;
        let files = revision_files(catalog, &record, &found)?;
        Ok(build_paths_info(&files, &paths))
    })
    .await
}

/// `/api/{repo_type}/{ref}/commits/{revision}`.
///
/// A repository the index synthesized from an HF cache snapshot or a loose folder has one
/// commit per revision and no ancestry, so the listing is that single commit.
pub async fn commits(
    catalog: &Arc<Catalog>,
    repo: &RepoId,
    sha: &CommitSha,
    limit: u32,
) -> Result<Vec<CommitEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    on_catalog(catalog, move |catalog| {
        let record = repository(catalog, &wanted)?;
        let found = revision_by_sha(&record, &sha)?;
        if limit.min(MAX_COMMITS) == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![build_commit(&record.repo, &found)])
    })
    .await
}

/// The bytes behind one path of one revision.
///
/// A file whose content hash this node has not computed yet is reported as missing: a
/// client verifies the `etag` it is given, so an unverifiable answer is worse than a 404.
pub async fn locate(
    catalog: &Arc<Catalog>,
    repo: &RepoId,
    sha: &CommitSha,
    path: &str,
) -> Result<FileLocation> {
    let wanted = repo.clone();
    let sha = *sha;
    let cleaned =
        clean_repo_path(path).ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    on_catalog(catalog, move |catalog| {
        let record = repository(catalog, &wanted)?;
        let found = revision_by_sha(&record, &sha)?;
        let file = revision_file(catalog, &record, &found, &cleaned)?
            .ok_or_else(|| MirrorError::EntryNotFound(cleaned.clone()))?;
        let abs_path = file
            .abs_path
            .clone()
            .ok_or_else(|| MirrorError::EntryNotFound(cleaned.clone()))?;
        let is_lfs = file.is_lfs();
        let has_etag = if is_lfs {
            file.sha256.is_some()
        } else {
            file.git_sha1.is_some() || file.sha256.is_some()
        };
        if !has_etag {
            return Err(MirrorError::EntryNotFound(cleaned));
        }
        Ok(FileLocation {
            abs_path,
            size: file.size,
            sha256: file.sha256,
            git_sha1: file.git_sha1,
            is_lfs,
            lfs_pointer_only: file.lfs_pointer,
            artifact_id: file.artifact,
            repo_id: record.id,
            commit: sha,
        })
    })
    .await
}

/// `GET /api/models`, `/api/datasets` and `/api/spaces`.
///
/// The Hub omits `siblings` from a listing and so does hoard; a client that wants the file
/// list asks for the repo itself, which is also where the card is read from.
pub async fn list_repos(catalog: &Arc<Catalog>, query: &RepoListQuery) -> Result<Vec<RepoInfo>> {
    let query = query.clone();
    on_catalog(catalog, move |catalog| {
        let filter = RepositoryFilter {
            text: query.search.clone(),
            repo_type: Some(query.repo_type),
            present_only: true,
        };
        let page = query.limit.max(1).saturating_mul(2).min(512);
        let mut cursor = None;
        let mut out = Vec::new();
        while out.len() < query.limit as usize {
            let listing = catalog.repositories(&filter, cursor, page)?;
            let exhausted = listing.rows.len() < page as usize;
            for row in &listing.rows {
                cursor = Some(row.id);
                if query
                    .author
                    .as_deref()
                    .is_some_and(|author| row.repo.org() != Some(author))
                {
                    continue;
                }
                let Some(found) = catalog.resolve_revision(row, &row.default_ref) else {
                    continue;
                };
                out.push(build_repo_info(
                    &row.repo,
                    &found,
                    Vec::new(),
                    RepoCard::default(),
                ));
                if out.len() == query.limit as usize {
                    return Ok(out);
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(out)
    })
    .await
}

/// How much of a tree one request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeListing {
    pub recursive: bool,
    pub expand: bool,
}

/// The README body and the parsed frontmatter behind `description` and `cardData`.
#[derive(Debug, Clone, Default, PartialEq)]
struct RepoCard {
    description: String,
    data: Option<Value>,
}

/// The card as the Hub reports it: the README of the revision, when this node holds one
/// small enough to read.
fn read_card(files: &[RevisionFile]) -> RepoCard {
    let Some(readme) = files
        .iter()
        .find(|file| file.path == README_PATH)
        .and_then(|file| file.abs_path.as_deref())
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= README_LIMIT)
        })
        .and_then(|path| std::fs::read_to_string(path).ok())
    else {
        return RepoCard::default();
    };
    let document = parse_readme(&readme);
    let data = (!document.frontmatter.is_empty()).then(|| {
        Value::Object(
            document
                .frontmatter
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        serde_json::from_str(value)
                            .unwrap_or_else(|_| Value::String(value.clone())),
                    )
                })
                .collect::<Map<String, Value>>(),
        )
    });
    RepoCard {
        description: document.body.trim().to_owned(),
        data,
    }
}

/// The commit a revision names, or git's own spelling for no object when the layout the
/// revision came from never carried one.
fn revision_sha(revision: &RevisionDetail) -> String {
    revision
        .sha
        .map_or_else(|| UNKNOWN_OID.to_owned(), |sha| sha.to_hex())
}

fn build_repo_info(
    repo: &RepoId,
    revision: &RevisionDetail,
    siblings: Vec<Sibling>,
    card: RepoCard,
) -> RepoInfo {
    let sha = revision_sha(revision);
    let last_modified = revision.committed.unwrap_or(Timestamp::UNIX_EPOCH);
    RepoInfo {
        internal_id: internal_id(repo, &sha),
        id: repo.reference(),
        author: repo.org().map(str::to_owned),
        sha,
        last_modified: HubTimestamp::new(last_modified),
        private: false,
        gated: false,
        disabled: false,
        tags: tags(card.data.as_ref()),
        description: card.description,
        paperswithcode_id: None,
        downloads: 0,
        likes: 0,
        card_data: card.data,
        siblings,
        created_at: HubTimestamp::new(last_modified),
    }
}

/// `_id` as the contract spells it: the sha256 of `org/repo/sha`.
fn internal_id(repo: &RepoId, sha: &str) -> String {
    let mut hasher = Sha256Hasher::new();
    hasher.update(repo.reference().as_bytes());
    hasher.update(b"/");
    hasher.update(sha.as_bytes());
    hex::encode(hasher.finalize())
}

fn tags(card: Option<&Value>) -> Vec<String> {
    let mut tags: Vec<String> = card
        .and_then(|data| data.get("tags"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(license) = card
        .and_then(|data| data.get("license"))
        .and_then(Value::as_str)
    {
        let tag = format!("license:{license}");
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

fn build_commit(repo: &RepoId, revision: &RevisionDetail) -> CommitEntry {
    let author = revision
        .author
        .clone()
        .or_else(|| repo.org().map(str::to_owned));
    CommitEntry {
        id: revision_sha(revision),
        title: revision
            .title
            .clone()
            .unwrap_or_else(|| DEFAULT_COMMIT_TITLE.to_owned()),
        message: String::new(),
        authors: author.into_iter().map(CommitAuthor::new).collect(),
        date: HubTimestamp::new(revision.committed.unwrap_or(Timestamp::UNIX_EPOCH)),
    }
}

fn last_commit(revision: &RevisionDetail) -> LastCommit {
    LastCommit {
        id: revision_sha(revision),
        title: revision
            .title
            .clone()
            .unwrap_or_else(|| DEFAULT_COMMIT_TITLE.to_owned()),
        date: HubTimestamp::new(revision.committed.unwrap_or(Timestamp::UNIX_EPOCH)),
    }
}

fn under<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

fn build_tree(
    files: &[RevisionFile],
    prefix: &str,
    listing: TreeListing,
    revision: &RevisionDetail,
) -> Vec<TreeEntry> {
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for file in files {
        let Some(rest) = under(&file.path, prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match (rest.split_once('/'), listing.recursive) {
            (Some((head, _)), false) => {
                let path = join_path(prefix, head);
                entries
                    .entry(path.clone())
                    .or_insert_with(|| TreeEntry::directory(UNKNOWN_OID, path));
            }
            (Some(_), true) => {
                for boundary in rest.match_indices('/').map(|(index, _)| index) {
                    let path = join_path(prefix, &rest[..boundary]);
                    entries
                        .entry(path.clone())
                        .or_insert_with(|| TreeEntry::directory(UNKNOWN_OID, path));
                }
                entries.insert(file.path.clone(), file.tree_entry());
            }
            (None, _) => {
                entries.insert(file.path.clone(), file.tree_entry());
            }
        }
    }
    let mut out: Vec<TreeEntry> = entries.into_values().collect();
    if listing.expand {
        let commit = last_commit(revision);
        for entry in &mut out {
            entry.last_commit = Some(commit.clone());
            if entry.lfs.is_some() || !entry.oid.is_empty() {
                entry.security = Some(SecurityInfo::clean(&entry.oid));
            }
        }
    }
    out
}

fn join_path(prefix: &str, rest: &str) -> String {
    if prefix.is_empty() {
        rest.to_owned()
    } else {
        format!("{prefix}/{rest}")
    }
}

fn build_paths_info(files: &[RevisionFile], paths: &[String]) -> Vec<PathsInfoEntry> {
    let mut out = Vec::with_capacity(paths.len());
    let mut seen: Vec<String> = Vec::with_capacity(paths.len());
    for raw in paths {
        let Some(path) = clean_repo_path(raw) else {
            continue;
        };
        if path.is_empty() || seen.contains(&path) {
            continue;
        }
        if let Some(file) = files.iter().find(|file| file.path == path) {
            out.push(PathsInfoEntry::new(file.tree_entry()));
        } else if files
            .iter()
            .any(|file| under(&file.path, &path).is_some_and(|rest| !rest.is_empty()))
        {
            out.push(PathsInfoEntry::new(TreeEntry::directory(
                UNKNOWN_OID,
                path.clone(),
            )));
        } else {
            continue;
        }
        seen.push(path);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::{Artifact, ArtifactFormat, Base, BlobId, RepoType, Role, State};
    use hoard_index::catalog::{LOCAL_NODE, SeedArtifact, SeedEntry, SeedRepository, SeedRevision};
    use hoard_index::config::{IndexConfig, SourceKind};
    use hoard_index::discover::RefKind;
    use tempfile::TempDir;

    use super::*;

    const WEIGHTS: &str = "unet/model.safetensors";
    const WEIGHTS_SHA256: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const CONFIG_SHA256: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const CONFIG_SHA1: &str = "2222222222222222222222222222222222222222";
    const COMMIT: [u8; 20] = [0x46; 20];
    const CARD: &str = "---\nlicense: apache-2.0\n---\n\nWhisper.\n";

    pub(crate) fn commit_sha() -> CommitSha {
        CommitSha::from_bytes(COMMIT)
    }

    fn at() -> Timestamp {
        "2026-01-17T10:22:31Z".parse().expect("timestamp parses")
    }

    fn listing(recursive: bool, expand: bool) -> TreeListing {
        TreeListing { recursive, expand }
    }

    fn weights_artifact() -> SeedArtifact {
        SeedArtifact {
            identity: 0x77,
            artifact: Artifact {
                id: ArtifactId::new(0),
                blob: BlobId::new(0),
                sha256: Some(WEIGHTS_SHA256.parse().expect("sha256 parses")),
                size: 4_265_146_304,
                format: ArtifactFormat::Safetensors,
                role: Role::Ckpt,
                sub_role: None,
                role_confidence: 0.9,
                base: Base::any(),
                base_label: None,
                base_confidence: 0.5,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
                sniff_version: 1,
            },
            name: "model.safetensors".to_owned(),
            org: Some("openai".to_owned()),
            tensors: None,
            evidence: Vec::new(),
            card: None,
            group_bytes: None,
            shard_count: None,
        }
    }

    fn present(rel_path: &str, abs_path: &str, size: u64) -> SeedEntry {
        SeedEntry {
            node: LOCAL_NODE,
            rel_path: rel_path.to_owned(),
            abs_path: Some(abs_path.to_owned()),
            size,
            state: State::Served,
            is_symlink: false,
            sha256: None,
            git_sha1: None,
            artifact: None,
        }
    }

    /// One repository as the catalog holds it: a README this node can read, a small file
    /// with both hashes, and an LFS-sized weight file that is an artifact.
    pub(crate) fn seeded(dir: &Path) -> (Arc<Catalog>, RepoId) {
        std::fs::write(dir.join(README_PATH), CARD).expect("card writes");
        let catalog = Catalog::open_or_scan(
            &IndexConfig {
                data_dir: dir.join("native"),
                sources: Vec::new(),
                ..IndexConfig::default()
            },
            "node-1",
            |_, _| {},
        )
        .expect("catalog opens");
        let repo = RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper");
        catalog
            .seed(&[SeedRepository {
                repo: repo.clone(),
                source: "cache".to_owned(),
                kind: SourceKind::HfCache,
                root_path: dir.to_path_buf(),
                git_dir: None,
                revisions: vec![SeedRevision {
                    name: "main".to_owned(),
                    sha: Some(commit_sha()),
                    refs: vec![("main".to_owned(), RefKind::Branch)],
                    title: Some("Upload weights".to_owned()),
                    author: Some("openai".to_owned()),
                    committed: Some(at()),
                    entries: vec![
                        present(README_PATH, &dir.join(README_PATH).to_string_lossy(), 64),
                        SeedEntry {
                            node: LOCAL_NODE,
                            sha256: Some(CONFIG_SHA256.parse().expect("sha256 parses")),
                            git_sha1: Some(CONFIG_SHA1.parse().expect("sha1 parses")),
                            ..present("config.json", "/data/hub/blobs/config", 12)
                        },
                        SeedEntry {
                            node: LOCAL_NODE,
                            sha256: Some(WEIGHTS_SHA256.parse().expect("sha256 parses")),
                            artifact: Some(weights_artifact()),
                            ..present(WEIGHTS, "/data/hub/blobs/weights", 4_265_146_304)
                        },
                    ],
                }],
            }])
            .expect("the catalog seeds");
        (Arc::new(catalog), repo)
    }

    #[tokio::test]
    async fn repo_metadata_matches_the_documented_body() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let info = repo_info(&catalog, &repo, "main")
            .await
            .expect("info reads");

        assert_eq!(info.id, "openai/whisper");
        assert_eq!(info.author.as_deref(), Some("openai"));
        assert_eq!(info.sha, commit_sha().to_hex());
        assert_eq!(
            info.last_modified.to_string(),
            "2026-01-17T10:22:31.000000Z"
        );
        assert_eq!(info.created_at.to_string(), "2026-01-17T10:22:31.000000Z");
        assert_eq!(info.tags, vec!["license:apache-2.0".to_owned()]);
        assert_eq!(
            info.siblings,
            vec![
                Sibling::new("README.md"),
                Sibling::new("config.json"),
                Sibling::new(WEIGHTS),
            ]
        );
        assert_eq!(info.internal_id.len(), 64);
        assert!(!info.private && !info.gated && !info.disabled);
    }

    #[tokio::test]
    async fn a_tree_listing_names_lfs_files_by_their_content_hash() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let entries = tree(&catalog, &repo, &commit_sha(), "", listing(false, false))
            .await
            .expect("tree reads");

        let json = serde_json::to_value(&entries).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!([
                {"type": "file", "oid": CONFIG_SHA1, "size": 12, "path": "config.json"},
                {"type": "file", "oid": UNKNOWN_OID, "size": 64, "path": "README.md"},
                {"type": "directory", "oid": UNKNOWN_OID, "size": 0, "path": "unet"},
            ])
            .as_array()
            .map(|values| {
                let mut sorted = values.clone();
                sorted.sort_by_key(|value| value["path"].as_str().unwrap_or_default().to_owned());
                serde_json::Value::Array(sorted)
            })
            .expect("array")
        );

        let deep = tree(
            &catalog,
            &repo,
            &commit_sha(),
            "unet",
            listing(false, false),
        )
        .await
        .expect("tree reads");
        assert_eq!(deep.len(), 1);
        let lfs = deep[0].lfs.as_ref().expect("the weight file is lfs");
        assert_eq!(lfs.oid, WEIGHTS_SHA256);
        assert_eq!(lfs.size, 4_265_146_304);
        assert_eq!(lfs.pointer_size, LFS_POINTER_SIZE);
        assert_eq!(deep[0].oid, WEIGHTS_POINTER_OID);
        assert_ne!(deep[0].oid, UNKNOWN_OID);
    }

    #[tokio::test]
    async fn an_expanded_tree_carries_the_last_commit_and_the_scan_report() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let entries = tree(&catalog, &repo, &commit_sha(), "unet", listing(true, true))
            .await
            .expect("tree reads");
        let commit = entries[0].last_commit.as_ref().expect("last commit");
        assert_eq!(commit.id, commit_sha().to_hex());
        assert_eq!(commit.title, "Upload weights");
        assert!(entries[0].security.as_ref().is_some_and(|scan| scan.safe));
    }

    #[tokio::test]
    async fn paths_info_answers_every_path_from_one_revision_read() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let paths = vec![
            WEIGHTS.to_owned(),
            "config.json".to_owned(),
            "unet".to_owned(),
            "missing.bin".to_owned(),
            "config.json".to_owned(),
        ];
        let entries = paths_info(&catalog, &repo, &commit_sha(), &paths)
            .await
            .expect("paths info reads");
        let json = serde_json::to_value(&entries).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!([
                {
                    "type": "file",
                    "oid": WEIGHTS_POINTER_OID,
                    "size": 4265146304u64,
                    "path": WEIGHTS,
                    "lfs": {
                        "oid": WEIGHTS_SHA256,
                        "size": 4265146304u64,
                        "pointerSize": LFS_POINTER_SIZE,
                    },
                },
                {"type": "file", "oid": CONFIG_SHA1, "size": 12, "path": "config.json"},
                {"type": "directory", "oid": UNKNOWN_OID, "size": 0, "path": "unet"},
            ])
        );
    }

    /// The git blob name of the canonical LFS pointer for [`WEIGHTS_SHA256`], confirmed
    /// against `git hash-object`. The Hub reports this as a tree entry's `oid`, because the
    /// blob git tracks for an LFS file is the pointer, never the media.
    const WEIGHTS_POINTER_OID: &str = "e5ac5c26df05b49b81220fd5128d05097e959822";
    const LFS_POINTER_SIZE: u64 = 135;

    #[test]
    fn paths_info_reads_the_revision_once() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let record = repository(&catalog, &repo).expect("repo reads");
        let found = revision_by_sha(&record, &commit_sha()).expect("revision reads");
        let files = revision_files(&catalog, &record, &found).expect("files read");

        let one = build_paths_info(&files, &[WEIGHTS.to_owned()]);
        let five = build_paths_info(
            &files,
            &[
                WEIGHTS.to_owned(),
                "config.json".to_owned(),
                "unet".to_owned(),
                "missing.bin".to_owned(),
                "config.json".to_owned(),
            ],
        );
        assert_eq!(one.len(), 1);
        assert_eq!(
            five.len(),
            3,
            "five paths are answered from the same revision read as one"
        );
        assert_eq!(one[0], five[0], "and answered identically");
    }

    #[tokio::test]
    async fn commits_lists_the_one_commit_of_a_synthesized_revision() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let listed = commits(&catalog, &repo, &commit_sha(), 50)
            .await
            .expect("commits read");
        assert_eq!(
            serde_json::to_value(&listed).expect("serializes"),
            serde_json::json!([{
                "id": commit_sha().to_hex(),
                "title": "Upload weights",
                "message": "",
                "authors": [{"name": "openai", "avatar": null}],
                "date": "2026-01-17T10:22:31.000000Z",
            }])
        );
        assert!(
            commits(&catalog, &repo, &commit_sha(), 0)
                .await
                .expect("commits read")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn locate_reports_the_bytes_and_refuses_a_path_outside_the_revision() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let found = locate(&catalog, &repo, &commit_sha(), WEIGHTS)
            .await
            .expect("weights locate");
        assert_eq!(found.abs_path, PathBuf::from("/data/hub/blobs/weights"));
        assert_eq!(found.size, 4_265_146_304);
        assert!(found.is_lfs);
        assert!(!found.lfs_pointer_only);
        assert_eq!(found.commit, commit_sha());
        assert!(found.artifact_id.is_some());

        let small = locate(&catalog, &repo, &commit_sha(), "config.json")
            .await
            .expect("config locates");
        assert!(!small.is_lfs);
        assert_eq!(
            small.git_sha1.map(CommitSha::to_hex).as_deref(),
            Some(CONFIG_SHA1)
        );

        assert!(matches!(
            locate(&catalog, &repo, &commit_sha(), "nope.bin").await,
            Err(MirrorError::EntryNotFound(_))
        ));
        assert!(matches!(
            locate(&catalog, &repo, &commit_sha(), "../etc/passwd").await,
            Err(MirrorError::EntryNotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_file_with_no_content_hash_is_not_served() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        assert!(matches!(
            locate(&catalog, &repo, &commit_sha(), README_PATH).await,
            Err(MirrorError::EntryNotFound(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_repo_or_revision_is_reported_as_such() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, repo) = seeded(dir.path());
        let missing = RepoId::new(RepoType::Models, Some("nobody".to_owned()), "nothing");
        assert!(matches!(
            repo_info(&catalog, &missing, "main").await,
            Err(MirrorError::RepoNotFound(_))
        ));

        assert!(matches!(
            repo_info(&catalog, &repo, "v9").await,
            Err(MirrorError::RevisionNotFound(_))
        ));
        assert!(matches!(
            tree(
                &catalog,
                &repo,
                &CommitSha::from_bytes([0x99; 20]),
                "",
                listing(false, false)
            )
            .await,
            Err(MirrorError::RevisionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_listing_endpoints_filter_by_search_and_author() {
        let dir = TempDir::new().expect("temp dir");
        let (catalog, _repo) = seeded(dir.path());
        let all = list_repos(&catalog, &RepoListQuery::new(RepoType::Models, 10))
            .await
            .expect("list reads");
        assert_eq!(all.len(), 1);
        assert!(all[0].siblings.is_empty());

        let by_author = list_repos(
            &catalog,
            &RepoListQuery {
                author: Some("openai".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert_eq!(by_author.len(), 1);

        let other_author = list_repos(
            &catalog,
            &RepoListQuery {
                author: Some("meta".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert!(other_author.is_empty());

        let by_search = list_repos(
            &catalog,
            &RepoListQuery {
                search: Some("whisper".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert_eq!(by_search.len(), 1);

        let datasets = list_repos(&catalog, &RepoListQuery::new(RepoType::Datasets, 10))
            .await
            .expect("list reads");
        assert!(datasets.is_empty());
    }

    #[test]
    fn an_lfs_pointer_renders_the_way_git_stores_it() {
        let oid: Sha256 = WEIGHTS_SHA256.parse().expect("sha256 parses");
        assert_eq!(
            lfs_pointer_text(&oid, 12),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{WEIGHTS_SHA256}\nsize 12\n"
            )
        );
    }
}
