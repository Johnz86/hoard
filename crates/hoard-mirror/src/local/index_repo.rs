//! Hub metadata and byte locations answered from the SQLite index.
//!
//! Every route backed here reads the files of one revision with a single entry query and
//! derives the Hub shapes from them, so a repository this node already holds is served
//! without opening git and without touching the network.

use std::collections::BTreeMap;
use std::path::PathBuf;

use hoard_core::{ArtifactId, CommitSha, RepoId, RepoRowId, RepoType, Sha256, clean_repo_path};
use hoard_index::hash::git_blob_sha1_bytes;
use hoard_index::sniff::parse_readme;
use hoard_index::store::blob::blob;
use hoard_index::store::entry::entries_for_revision;
use hoard_index::store::node::local_node;
use hoard_index::store::repo::{
    RepoFilter, RepoRow, list_repos as list_repo_rows, repo_by_qualified,
};
use hoard_index::store::revision::{RevisionRow, resolve_revision, revision_by_sha};
use hoard_index::store::{Reader, Store, Writer};
use hoard_index::IndexError;
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

/// One file of a revision with everything the Hub shapes need, read in one entry query.
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

/// Runs an index read on a blocking thread with one pooled connection.
pub(crate) async fn read_index<T, F>(store: &Store, f: F) -> Result<T>
where
    F: FnOnce(&Reader) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(MirrorError::from)?;
        f(&connection)
    })
    .await
    .map_err(|_| MirrorError::Index(IndexError::TaskJoin))?
}

/// Runs an index write on a blocking thread, serialized against every other writer.
pub(crate) async fn write_index<T, F>(store: &Store, f: F) -> Result<T>
where
    F: FnOnce(&mut Writer<'_>) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut connection = store.write().map_err(MirrorError::from)?;
        f(&mut connection)
    })
    .await
    .map_err(|_| MirrorError::Index(IndexError::TaskJoin))?
}

/// The repository as this node holds it. A repo only a peer holds is not servable here.
pub(crate) fn repo_row(conn: &Reader, repo: &RepoId) -> Result<RepoRow> {
    let node = local_node(conn)?;
    repo_by_qualified(conn, &repo.qualified(), node.id)?
        .ok_or_else(|| MirrorError::RepoNotFound(repo.qualified()))
}

pub(crate) fn revision_row(conn: &Reader, repo: &RepoRow, rev: &str) -> Result<RevisionRow> {
    resolve_revision(conn, repo.id, rev).map_err(|_| MirrorError::RevisionNotFound(rev.to_owned()))
}

pub(crate) fn revision_row_by_sha(
    conn: &Reader,
    repo: &RepoRow,
    sha: &CommitSha,
) -> Result<RevisionRow> {
    revision_by_sha(conn, repo.id, sha)?.ok_or_else(|| MirrorError::RevisionNotFound(sha.to_hex()))
}

/// Every present file of a revision, sorted by path.
///
/// The entry rows come back in one query; the content hashes are then read once per
/// distinct blob, so the cost is bound by the revision rather than by how many paths a
/// request asked about.
pub(crate) fn revision_files(conn: &Reader, revision: &RevisionRow) -> Result<Vec<RevisionFile>> {
    let rows = entries_for_revision(conn, revision.id)?;
    let mut hashes: BTreeMap<i64, (Option<Sha256>, Option<CommitSha>)> = BTreeMap::new();
    let mut files = Vec::with_capacity(rows.len());
    for row in rows {
        if !row.present {
            continue;
        }
        let (sha256, git_sha1) = match row.blob {
            Some(id) => match hashes.get(&id.get()) {
                Some(found) => *found,
                None => {
                    let record = blob(conn, id)?;
                    let pair = (record.sha256, record.git_sha1);
                    hashes.insert(id.get(), pair);
                    pair
                }
            },
            None => (None, None),
        };
        files.push(RevisionFile {
            path: row.path,
            size: row.size,
            abs_path: row.abs_path.map(PathBuf::from),
            sha256,
            git_sha1,
            artifact: row.artifact,
            lfs_pointer: row.lfs_pointer,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// `/api/{repo_type}/{ref}` and `/api/{repo_type}/{ref}/revision/{revision}`.
pub async fn repo_info(store: &Store, repo: &RepoId, revision: &str) -> Result<RepoInfo> {
    let wanted = repo.clone();
    let rev = revision.to_owned();
    read_index(store, move |conn| {
        let row = repo_row(conn, &wanted)?;
        let revision = revision_row(conn, &row, &rev)?;
        let files = revision_files(conn, &revision)?;
        let card = read_card(&row, &files);
        Ok(build_repo_info(
            &wanted,
            &row,
            &revision,
            files.iter().map(|file| Sibling::new(&file.path)).collect(),
            card,
        ))
    })
    .await
}

/// `/api/{repo_type}/{ref}/tree/{revision}/{path}`.
pub async fn tree(
    store: &Store,
    repo: &RepoId,
    sha: &CommitSha,
    path: &str,
    recursive: bool,
    expand: bool,
) -> Result<Vec<TreeEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    let prefix =
        clean_repo_path(path).ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    read_index(store, move |conn| {
        let row = repo_row(conn, &wanted)?;
        let revision = revision_row_by_sha(conn, &row, &sha)?;
        let files = revision_files(conn, &revision)?;
        Ok(build_tree(&files, &prefix, recursive, expand, &revision))
    })
    .await
}

/// `/api/{repo_type}/{ref}/paths-info/{revision}`, answered for every requested path from
/// one pass over the revision rather than one upstream call per path.
pub async fn paths_info(
    store: &Store,
    repo: &RepoId,
    sha: &CommitSha,
    paths: &[String],
) -> Result<Vec<PathsInfoEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    let paths = paths.to_vec();
    read_index(store, move |conn| {
        let row = repo_row(conn, &wanted)?;
        let revision = revision_row_by_sha(conn, &row, &sha)?;
        let files = revision_files(conn, &revision)?;
        Ok(build_paths_info(&files, &paths))
    })
    .await
}

/// `/api/{repo_type}/{ref}/commits/{revision}`.
///
/// A repository the index synthesized from an HF cache snapshot or a loose folder has one
/// commit per revision and no ancestry, so the listing is that single commit.
pub async fn commits(
    store: &Store,
    repo: &RepoId,
    sha: &CommitSha,
    limit: u32,
) -> Result<Vec<CommitEntry>> {
    let wanted = repo.clone();
    let sha = *sha;
    read_index(store, move |conn| {
        let row = repo_row(conn, &wanted)?;
        let revision = revision_row_by_sha(conn, &row, &sha)?;
        if limit.min(MAX_COMMITS) == 0 {
            return Ok(Vec::new());
        }
        Ok(vec![build_commit(&row, &revision)])
    })
    .await
}

/// The bytes behind one path of one revision.
///
/// A file whose content hash this node has not computed yet is reported as missing: a
/// client verifies the `etag` it is given, so an unverifiable answer is worse than a 404.
pub async fn locate(
    store: &Store,
    repo: &RepoId,
    sha: &CommitSha,
    path: &str,
) -> Result<FileLocation> {
    let wanted = repo.clone();
    let sha = *sha;
    let cleaned =
        clean_repo_path(path).ok_or_else(|| MirrorError::EntryNotFound(path.to_owned()))?;
    read_index(store, move |conn| {
        let row = repo_row(conn, &wanted)?;
        let revision = revision_row_by_sha(conn, &row, &sha)?;
        let files = revision_files(conn, &revision)?;
        let file = files
            .into_iter()
            .find(|file| file.path == cleaned)
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
            repo_id: row.id,
            commit: revision.sha,
        })
    })
    .await
}

/// `GET /api/models`, `/api/datasets` and `/api/spaces`.
///
/// The Hub omits `siblings` from a listing and so does hoard; a client that wants the file
/// list asks for the repo itself.
pub async fn list_repos(store: &Store, query: &RepoListQuery) -> Result<Vec<RepoInfo>> {
    let query = query.clone();
    read_index(store, move |conn| {
        let node = local_node(conn)?;
        let filter = RepoFilter {
            text: query.search.clone(),
            repo_type: Some(query.repo_type),
            node_row: Some(node.id),
        };
        let page = query.limit.max(1).saturating_mul(2).min(512);
        let mut cursor = None;
        let mut out = Vec::new();
        while out.len() < query.limit as usize {
            let rows = list_repo_rows(conn, &filter, cursor, page)?;
            let exhausted = rows.len() < page as usize;
            for row in rows {
                cursor = Some(row.id);
                if query
                    .author
                    .as_deref()
                    .is_some_and(|author| row.repo.org() != Some(author))
                {
                    continue;
                }
                let Ok(revision) = revision_row(conn, &row, "main") else {
                    continue;
                };
                let card = stored_card(&row);
                let repo = row.repo.clone();
                out.push(build_repo_info(&repo, &row, &revision, Vec::new(), card));
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

/// The README body and the parsed frontmatter behind `description` and `cardData`.
#[derive(Debug, Clone, Default, PartialEq)]
struct RepoCard {
    description: String,
    data: Option<Value>,
}

fn stored_card(row: &RepoRow) -> RepoCard {
    RepoCard {
        description: row.description.clone().unwrap_or_default(),
        data: row
            .card_data
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
    }
}

/// The card as the Hub reports it: the stored columns when discovery filled them, and the
/// README of the revision otherwise.
fn read_card(row: &RepoRow, files: &[RevisionFile]) -> RepoCard {
    let stored = stored_card(row);
    if !stored.description.is_empty() && stored.data.is_some() {
        return stored;
    }
    let Some(readme) = files
        .iter()
        .find(|file| file.path == README_PATH)
        .and_then(|file| file.abs_path.as_deref())
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= README_LIMIT)
        })
        .and_then(|path| std::fs::read_to_string(path).ok())
    else {
        return stored;
    };
    let document = parse_readme(&readme);
    let data = if document.frontmatter.is_empty() {
        stored.data
    } else {
        Some(Value::Object(
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
        ))
    };
    RepoCard {
        description: if stored.description.is_empty() {
            document.body.trim().to_owned()
        } else {
            stored.description
        },
        data,
    }
}

fn build_repo_info(
    repo: &RepoId,
    row: &RepoRow,
    revision: &RevisionRow,
    siblings: Vec<Sibling>,
    card: RepoCard,
) -> RepoInfo {
    let sha = revision.sha.to_hex();
    let last_modified = revision
        .committed
        .or(row.last_modified)
        .or(row.created_at)
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let created_at = row.created_at.unwrap_or(last_modified);
    RepoInfo {
        internal_id: internal_id(repo, &sha),
        id: repo.reference(),
        author: repo.org().map(str::to_owned),
        sha,
        last_modified: HubTimestamp::new(last_modified),
        private: row.private,
        gated: row.gated,
        disabled: false,
        tags: tags(row, card.data.as_ref()),
        description: card.description,
        paperswithcode_id: None,
        downloads: row.downloads,
        likes: row.likes,
        card_data: card.data,
        siblings,
        created_at: HubTimestamp::new(created_at),
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

fn tags(row: &RepoRow, card: Option<&Value>) -> Vec<String> {
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
    if let Some(license) = row.license.as_deref() {
        let tag = format!("license:{license}");
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

fn build_commit(row: &RepoRow, revision: &RevisionRow) -> CommitEntry {
    let author = revision
        .author
        .clone()
        .or_else(|| row.repo.org().map(str::to_owned));
    CommitEntry {
        id: revision.sha.to_hex(),
        title: revision
            .title
            .clone()
            .unwrap_or_else(|| DEFAULT_COMMIT_TITLE.to_owned()),
        message: String::new(),
        authors: author.into_iter().map(CommitAuthor::new).collect(),
        date: HubTimestamp::new(
            revision
                .committed
                .or(row.last_modified)
                .unwrap_or(Timestamp::UNIX_EPOCH),
        ),
    }
}

fn last_commit(revision: &RevisionRow) -> LastCommit {
    LastCommit {
        id: revision.sha.to_hex(),
        title: revision
            .title
            .clone()
            .unwrap_or_else(|| DEFAULT_COMMIT_TITLE.to_owned()),
        date: HubTimestamp::new(revision.committed.unwrap_or(Timestamp::UNIX_EPOCH)),
    }
}

/// Splits a path into the part under `prefix`, or `None` when it is not under it.
fn under<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

fn build_tree(
    files: &[RevisionFile],
    prefix: &str,
    recursive: bool,
    expand: bool,
    revision: &RevisionRow,
) -> Vec<TreeEntry> {
    let mut entries: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for file in files {
        let Some(rest) = under(&file.path, prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match (rest.split_once('/'), recursive) {
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
    if expand {
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
        if path.is_empty() || seen.iter().any(|other| *other == path) {
            continue;
        }
        if let Some(file) = files.iter().find(|file| file.path == path) {
            out.push(PathsInfoEntry::new(file.tree_entry()));
        } else if files
            .iter()
            .any(|file| under(&file.path, &path).is_some_and(|rest| !rest.is_empty()))
        {
            out.push(PathsInfoEntry::new(TreeEntry::directory(UNKNOWN_OID, path.clone())));
        } else {
            continue;
        }
        seen.push(path);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hoard_core::NodeId;
    use hoard_core::{RepoType, State};
    use hoard_index::config::SourceKind;
    use hoard_index::store::artifact::{NewArtifact, upsert_artifact};
    use hoard_index::store::blob::{set_sha256, upsert_blob};
    use hoard_index::store::entry::{NewEntry, upsert_entry};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};
    use hoard_index::store::revision::{NewRevision, RefKind, upsert_ref, upsert_revision};

    use super::*;

    const WEIGHTS: &str = "unet/model.safetensors";
    const WEIGHTS_SHA256: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const CONFIG_SHA1: &str = "2222222222222222222222222222222222222222";
    const COMMIT: [u8; 20] = [0x46; 20];

    pub(crate) fn commit_sha() -> CommitSha {
        CommitSha::from_bytes(COMMIT)
    }

    fn at() -> Timestamp {
        "2026-01-17T10:22:31Z".parse().expect("timestamp parses")
    }

    /// A repository with one revision, one LFS-sized weight file and two small files.
    pub(crate) fn seed(store: &Store) -> RepoId {
        let conn = store.write().expect("writer");
        let node = upsert_node(
            &conn,
            &NewNode::local(NodeId::new("node-1").expect("node id"), "workstation"),
        )
        .expect("node inserts");
        let repo = RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper");
        let repo_row = upsert_repo(
            &conn,
            &NewRepo {
                license: Some("apache-2.0".to_owned()),
                last_modified: Some(at()),
                created_at: Some(at()),
                ..NewRepo::new(repo.clone(), node, SourceKind::HfCache)
            },
        )
        .expect("repo inserts");
        let revision = upsert_revision(
            &conn,
            &NewRevision {
                title: Some("Upload weights".to_owned()),
                author: Some("openai".to_owned()),
                committed: Some(at()),
                ..NewRevision::new(repo_row, commit_sha())
            },
        )
        .expect("revision inserts");
        upsert_ref(&conn, repo_row, "main", RefKind::Branch, revision).expect("ref inserts");

        let big = upsert_blob(&conn, "xxh3:weights", 4_265_146_304, at()).expect("blob inserts");
        set_sha256(
            &conn,
            big,
            WEIGHTS_SHA256.parse().expect("sha256 parses"),
            None,
            at(),
        )
        .expect("hash records");
        let small = upsert_blob(&conn, "xxh3:config", 12, at()).expect("blob inserts");
        set_sha256(
            &conn,
            small,
            "3333333333333333333333333333333333333333333333333333333333333333"
                .parse()
                .expect("sha256 parses"),
            Some(CONFIG_SHA1.parse().expect("sha1 parses")),
            at(),
        )
        .expect("hash records");
        let readme = upsert_blob(&conn, "xxh3:readme", 64, at()).expect("blob inserts");

        let artifact = upsert_artifact(&conn, &NewArtifact::new(big, "whisper"), at())
            .expect("artifact inserts");
        upsert_entry(
            &conn,
            &NewEntry {
                blob: Some(big),
                artifact: Some(artifact),
                abs_path: Some("/data/hub/blobs/weights".to_owned()),
                size: 4_265_146_304,
                state: State::Served,
                ..NewEntry::new(revision, WEIGHTS, node)
            },
            at(),
        )
        .expect("entry inserts");
        upsert_entry(
            &conn,
            &NewEntry {
                blob: Some(small),
                abs_path: Some("/data/hub/blobs/config".to_owned()),
                size: 12,
                ..NewEntry::new(revision, "config.json", node)
            },
            at(),
        )
        .expect("entry inserts");
        upsert_entry(
            &conn,
            &NewEntry {
                blob: Some(readme),
                abs_path: Some("/data/hub/blobs/readme".to_owned()),
                size: 64,
                ..NewEntry::new(revision, README_PATH, node)
            },
            at(),
        )
        .expect("entry inserts");
        drop(conn);
        repo
    }

    fn store() -> Store {
        Store::open_in_memory().expect("store opens")
    }

    #[tokio::test]
    async fn repo_metadata_matches_the_documented_body() {
        let store = store();
        let repo = seed(&store);
        let info = repo_info(&store, &repo, "main").await.expect("info reads");

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
        let store = store();
        let repo = seed(&store);
        let entries = tree(&store, &repo, &commit_sha(), "", false, false)
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

        let deep = tree(&store, &repo, &commit_sha(), "unet", false, false)
            .await
            .expect("tree reads");
        assert_eq!(deep.len(), 1);
        let lfs = deep[0].lfs.as_ref().expect("the weight file is lfs");
        assert_eq!(lfs.oid, WEIGHTS_SHA256);
        assert_eq!(lfs.size, 4_265_146_304);
        assert_eq!(lfs.pointer_size, 128);
        assert_ne!(deep[0].oid, UNKNOWN_OID);
    }

    #[tokio::test]
    async fn an_expanded_tree_carries_the_last_commit_and_the_scan_report() {
        let store = store();
        let repo = seed(&store);
        let entries = tree(&store, &repo, &commit_sha(), "unet", true, true)
            .await
            .expect("tree reads");
        let commit = entries[0].last_commit.as_ref().expect("last commit");
        assert_eq!(commit.id, commit_sha().to_hex());
        assert_eq!(commit.title, "Upload weights");
        assert!(entries[0].security.as_ref().is_some_and(|scan| scan.safe));
    }

    #[tokio::test]
    async fn paths_info_answers_every_path_from_one_revision_read() {
        let store = store();
        let repo = seed(&store);
        let paths = vec![
            WEIGHTS.to_owned(),
            "config.json".to_owned(),
            "unet".to_owned(),
            "missing.bin".to_owned(),
            "config.json".to_owned(),
        ];
        let entries = paths_info(&store, &repo, &commit_sha(), &paths)
            .await
            .expect("paths info reads");
        let json = serde_json::to_value(&entries).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!([
                {
                    "type": "file",
                    "oid": "e0b8bb1cd53f6b8b78c6ffbd7f1c1b1b1b0f5b0f",
                    "size": 4265146304u64,
                    "path": WEIGHTS,
                    "lfs": {
                        "oid": WEIGHTS_SHA256,
                        "size": 4265146304u64,
                        "pointerSize": 128,
                    },
                },
                {"type": "file", "oid": CONFIG_SHA1, "size": 12, "path": "config.json"},
                {"type": "directory", "oid": UNKNOWN_OID, "size": 0, "path": "unet"},
            ])
        );
    }

    static STATEMENTS: AtomicUsize = AtomicUsize::new(0);
    static ENTRY_STATEMENTS: AtomicUsize = AtomicUsize::new(0);

    fn count(sql: &str) {
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        if sql.contains("FROM entry") {
            ENTRY_STATEMENTS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn paths_info_reads_the_entry_table_once_however_many_paths_are_asked_about() {
        let store = store();
        let repo = seed(&store);
        let mut conn = store.read().expect("reader");
        conn.trace(Some(count));

        let one = vec![WEIGHTS.to_owned()];
        let five: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|name| format!("{name}.bin"))
            .collect();

        let row = repo_row(&conn, &repo).expect("repo reads");
        let revision = revision_row_by_sha(&conn, &row, &commit_sha()).expect("revision reads");

        STATEMENTS.store(0, Ordering::Relaxed);
        ENTRY_STATEMENTS.store(0, Ordering::Relaxed);
        let files = revision_files(&conn, &revision).expect("files read");
        build_paths_info(&files, &one);
        let single = STATEMENTS.load(Ordering::Relaxed);
        assert_eq!(ENTRY_STATEMENTS.load(Ordering::Relaxed), 1);

        STATEMENTS.store(0, Ordering::Relaxed);
        ENTRY_STATEMENTS.store(0, Ordering::Relaxed);
        let files = revision_files(&conn, &revision).expect("files read");
        build_paths_info(&files, &five);
        assert_eq!(STATEMENTS.load(Ordering::Relaxed), single);
        assert_eq!(ENTRY_STATEMENTS.load(Ordering::Relaxed), 1);
        conn.trace(None);
    }

    #[tokio::test]
    async fn commits_lists_the_one_commit_of_a_synthesized_revision() {
        let store = store();
        let repo = seed(&store);
        let listed = commits(&store, &repo, &commit_sha(), 50)
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
            commits(&store, &repo, &commit_sha(), 0)
                .await
                .expect("commits read")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn locate_reports_the_bytes_and_refuses_a_path_outside_the_revision() {
        let store = store();
        let repo = seed(&store);
        let found = locate(&store, &repo, &commit_sha(), WEIGHTS)
            .await
            .expect("weights locate");
        assert_eq!(found.abs_path, PathBuf::from("/data/hub/blobs/weights"));
        assert_eq!(found.size, 4_265_146_304);
        assert!(found.is_lfs);
        assert!(!found.lfs_pointer_only);
        assert_eq!(found.commit, commit_sha());
        assert!(found.artifact_id.is_some());

        let small = locate(&store, &repo, &commit_sha(), "config.json")
            .await
            .expect("config locates");
        assert!(!small.is_lfs);
        assert_eq!(
            small.git_sha1.map(CommitSha::to_hex).as_deref(),
            Some(CONFIG_SHA1)
        );

        assert!(matches!(
            locate(&store, &repo, &commit_sha(), "nope.bin").await,
            Err(MirrorError::EntryNotFound(_))
        ));
        assert!(matches!(
            locate(&store, &repo, &commit_sha(), "../etc/passwd").await,
            Err(MirrorError::EntryNotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_file_with_no_content_hash_is_not_served() {
        let store = store();
        let repo = seed(&store);
        assert!(matches!(
            locate(&store, &repo, &commit_sha(), README_PATH).await,
            Err(MirrorError::EntryNotFound(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_repo_or_revision_is_reported_as_such() {
        let store = store();
        seed(&store);
        let missing = RepoId::new(RepoType::Models, Some("nobody".to_owned()), "nothing");
        assert!(matches!(
            repo_info(&store, &missing, "main").await,
            Err(MirrorError::RepoNotFound(_))
        ));

        let repo = RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper");
        assert!(matches!(
            repo_info(&store, &repo, "v9").await,
            Err(MirrorError::RevisionNotFound(_))
        ));
        assert!(matches!(
            tree(
                &store,
                &repo,
                &CommitSha::from_bytes([0x99; 20]),
                "",
                false,
                false
            )
            .await,
            Err(MirrorError::RevisionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn the_listing_endpoints_filter_by_search_and_author() {
        let store = store();
        seed(&store);
        let all = list_repos(&store, &RepoListQuery::new(RepoType::Models, 10))
            .await
            .expect("list reads");
        assert_eq!(all.len(), 1);
        assert!(all[0].siblings.is_empty());

        let by_author = list_repos(
            &store,
            &RepoListQuery {
                author: Some("openai".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert_eq!(by_author.len(), 1);

        let other_author = list_repos(
            &store,
            &RepoListQuery {
                author: Some("meta".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert!(other_author.is_empty());

        let by_search = list_repos(
            &store,
            &RepoListQuery {
                search: Some("whisper".to_owned()),
                ..RepoListQuery::new(RepoType::Models, 10)
            },
        )
        .await
        .expect("list reads");
        assert_eq!(by_search.len(), 1);

        let datasets = list_repos(&store, &RepoListQuery::new(RepoType::Datasets, 10))
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
