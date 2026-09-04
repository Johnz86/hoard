//! Resolving a revision to a commit, and one path of that commit to bytes.
//!
//! This is the whole serving decision in one place: which sha a client meant, whether the
//! bytes are in the index, in the block cache or still upstream, and what the resolve
//! response says about them. `hoard-server` turns a [`FileAnswer`] into an HTTP response and
//! adds nothing of its own.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use hoard_core::{CommitSha, RepoId, Sha256, clean_repo_path};
use hoard_index::store::Store;
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use url::Url;

use crate::access::AccessPolicy;
use crate::cache::{BlockCache, CacheEntry, body_path};
use crate::error::{MirrorError, Result};
use crate::ingest::{CachedFile, IngestOutcome, ingest_cached_file};
use crate::local::index_repo::{self, FileLocation, read_index, repo_row, revision_row};
use crate::model::HubTimestamp;
use crate::upstream::client::{Authorization, ProxyResponse};
use crate::upstream::hub::{BlockFetch, Hub, anonymous_whoami, quoted};
use crate::upstream::range::{
    ByteRange, RangeOutcome, multipart_boundary, multipart_content_length, multipart_content_type,
    multipart_part_header, multipart_terminator, parse_ranges, unsatisfied_content_range,
};

/// How long an upstream revision lookup is trusted before it is asked again.
pub const REVISION_TTL: Duration = Duration::from_secs(300);

/// How many resolved revisions are remembered in memory.
pub const REVISION_CACHE_CAPACITY: u64 = 4_096;

/// The read size of a file served straight off the filesystem.
const LOCAL_READ: u64 = 1024 * 1024;

/// The read size of a proxied file that may not be cached, so nothing has to buffer a whole
/// block that will be thrown away.
const UNCACHED_READ: u64 = 8 * 1024 * 1024;

const DEFAULT_REVISION: &str = "main";
const OCTET_STREAM: &str = "application/octet-stream";
const REVISION_META_FILE: &str = "meta_get.json";
const SHA256_HEX_LEN: usize = 64;

/// A cache entry shared between the answer and the body stream reading it.
type SharedEntry = Arc<Mutex<CacheEntry>>;

/// A body as the server sends it, one chunk at a time and never buffered whole.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// Which of the three documented steps answered a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionSource {
    Index,
    Upstream,
    Cache,
}

/// A revision request answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub sha: CommitSha,
    pub source: RevisionSource,
}

/// What a client asked for on the resolve route.
#[derive(Debug, Clone, Copy)]
pub struct FileRequest<'a> {
    pub repo: &'a RepoId,
    pub revision: &'a str,
    pub path: &'a str,
    pub auth: Option<&'a Authorization>,
    /// A HEAD answers with identical headers and status and an empty body, and never
    /// creates or extends a cache entry.
    pub head: bool,
}

impl<'a> FileRequest<'a> {
    pub fn new(repo: &'a RepoId, revision: &'a str, path: &'a str) -> Self {
        Self {
            repo,
            revision,
            path,
            auth: None,
            head: false,
        }
    }

    pub fn with_auth(mut self, auth: Option<&'a Authorization>) -> Self {
        self.auth = auth;
        self
    }

    pub fn head(mut self, head: bool) -> Self {
        self.head = head;
        self
    }
}

/// Where the bytes of an answer come from.
#[derive(Debug)]
pub enum ByteSource {
    /// A file this node holds, named by the index.
    Local(PathBuf),
    /// A block-cache entry with every block already on disk.
    Cache(Box<CachedSource>),
    /// An upstream, with the cache entry the blocks land in when caching is allowed.
    Upstream(Box<UpstreamSource>),
}

/// A file the block cache already holds in full.
#[derive(Debug)]
pub struct CachedSource {
    entry: SharedEntry,
    /// How a block that fails its CRC is fetched again. Absent offline and when the rules
    /// deny the proxy, where a corrupt block is a failed read rather than a refetch.
    remote: Option<RemoteBytes>,
}

/// A file that has to be fetched, and where the fetched blocks are kept.
#[derive(Debug)]
pub struct UpstreamSource {
    remote: RemoteBytes,
    entry: Option<SharedEntry>,
    ingest: Option<IngestTrigger>,
}

/// The upstream bytes of one file: which upstream, which URL, whose credentials.
#[derive(Debug, Clone)]
struct RemoteBytes {
    hub: Hub,
    url: Url,
    auth: Option<Authorization>,
}

impl RemoteBytes {
    async fn fill(&self, entry: &mut CacheEntry, index: u64) -> Result<()> {
        self.hub
            .fill_block(
                entry,
                BlockFetch {
                    url: &self.url,
                    auth: self.auth.as_ref(),
                    index,
                },
            )
            .await
    }
}

/// The ingest a fully fetched file runs, so the cache entry becomes a local artifact.
#[derive(Debug, Clone)]
pub struct IngestTrigger {
    store: Store,
    cache: Arc<BlockCache>,
    entry: SharedEntry,
    repo: RepoId,
    commit: CommitSha,
    path: String,
    expected: Option<Sha256>,
}

impl IngestTrigger {
    /// Ingests the entry if it is complete, and reports what it added.
    pub async fn run(&self) -> Result<Option<IngestOutcome>> {
        let mut entry = self.entry.lock().await;
        ingest_cached_file(
            &self.store,
            &self.cache,
            &mut entry,
            CachedFile {
                repo: &self.repo,
                commit: self.commit,
                path: &self.path,
                expected: self.expected,
            },
        )
        .await
    }
}

/// One file of one revision, ready to be answered with.
#[derive(Debug)]
pub struct FileAnswer {
    pub commit: CommitSha,
    pub size: u64,
    /// Already quoted, as every resolve response requires.
    pub etag: String,
    pub linked_etag: Option<String>,
    pub linked_size: Option<u64>,
    pub content_type: String,
    pub source: ByteSource,
}

impl FileAnswer {
    /// The ingest this answer runs once its body has been served, if any.
    pub fn ingest(&self) -> Option<&IngestTrigger> {
        match &self.source {
            ByteSource::Upstream(source) => source.ingest.as_ref(),
            ByteSource::Local(_) | ByteSource::Cache(_) => None,
        }
    }
}

/// The status, headers and framing of a resolve response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedResponse {
    pub status: u16,
    /// Lower-case names, in the order `docs/api/hub-compat.md` tabulates them.
    pub headers: Vec<(String, String)>,
    pub content_length: u64,
    pub outcome: RangeOutcome,
}

impl ServedResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The response a resolve answers with, before its body.
///
/// The same value serves a GET and a HEAD: only the body differs. A `Range` that named no
/// satisfiable byte is 416 with `content-range: bytes */<size>`, several satisfiable ranges
/// are framed as `multipart/byteranges`, and no usable range at all is the whole file.
pub fn served_response(answer: &FileAnswer, range: Option<&str>) -> ServedResponse {
    let outcome = range.map_or(RangeOutcome::Whole, |header| {
        parse_ranges(header, answer.size)
    });
    let mut headers = vec![
        ("accept-ranges".to_owned(), "bytes".to_owned()),
        ("x-repo-commit".to_owned(), answer.commit.to_hex()),
        ("etag".to_owned(), answer.etag.clone()),
    ];
    if let Some(linked) = &answer.linked_etag {
        headers.push(("x-linked-etag".to_owned(), linked.clone()));
    }
    if let Some(size) = answer.linked_size {
        headers.push(("x-linked-size".to_owned(), size.to_string()));
    }

    let (status, content_length, content_type) = match &outcome {
        RangeOutcome::Whole => (200, answer.size, answer.content_type.clone()),
        RangeOutcome::Single(range) => {
            headers.push(("content-range".to_owned(), range.content_range(answer.size)));
            (206, range.len(), answer.content_type.clone())
        }
        RangeOutcome::Multi(ranges) => {
            let boundary = multipart_boundary(&answer.etag, answer.size, ranges);
            let length = multipart_content_length(&boundary, ranges, answer.size);
            (206, length, multipart_content_type(&boundary))
        }
        RangeOutcome::Unsatisfiable => {
            headers.push((
                "content-range".to_owned(),
                unsatisfied_content_range(answer.size),
            ));
            (416, 0, answer.content_type.clone())
        }
    };
    headers.push(("content-type".to_owned(), content_type));
    headers.push(("content-length".to_owned(), content_length.to_string()));
    ServedResponse {
        status,
        headers,
        content_length,
        outcome,
    }
}

/// The body of a resolve response, chunk by chunk.
///
/// Missing blocks are fetched as the stream reaches them, so a client that only wants the
/// header of a large file never pays for the rest of it.
pub async fn read_range(answer: &FileAnswer, range: Option<&str>) -> Result<ByteStream> {
    let served = served_response(answer, range);
    let steps = body_steps(answer, &served);
    let reader = Reader::open(answer).await?;
    let state = BodyState {
        steps: steps.into_iter(),
        cursor: None,
        reader,
    };
    Ok(Box::pin(futures::stream::try_unfold(
        state,
        |mut state| async move {
            loop {
                if let Some((position, end)) = state.cursor {
                    if position < end {
                        let bytes = state.reader.read(position, end - position).await?;
                        if bytes.is_empty() {
                            return Err(MirrorError::CacheCorrupt(format!(
                                "no bytes at offset {position}"
                            )));
                        }
                        state.cursor = Some((position + bytes.len() as u64, end));
                        return Ok(Some((bytes, state)));
                    }
                    state.cursor = None;
                }
                match state.steps.next() {
                    Some(Step::Literal(bytes)) => return Ok(Some((bytes, state))),
                    Some(Step::Range(range)) => {
                        state.cursor = Some((range.start, range.end + 1));
                    }
                    None => {
                        state.reader.finish();
                        return Ok(None);
                    }
                }
            }
        },
    )))
}

/// The Hub face of this node: what it holds, what it has cached and what it can fetch.
#[derive(Debug, Clone)]
pub struct Mirror {
    store: Store,
    cache: Arc<BlockCache>,
    access: Arc<AccessPolicy>,
    upstream: Hub,
    revisions: moka::future::Cache<String, CommitSha>,
}

impl Mirror {
    pub fn new(
        store: Store,
        cache: Arc<BlockCache>,
        access: Arc<AccessPolicy>,
        upstream: Hub,
    ) -> Self {
        Self {
            store,
            cache,
            access,
            upstream,
            revisions: moka::future::Cache::builder()
                .max_capacity(REVISION_CACHE_CAPACITY)
                .time_to_live(REVISION_TTL)
                .build(),
        }
    }

    /// The same node against a different upstream, which is how a peer pull reuses the
    /// proxy: a peer speaks the Hub API, so pulling from one is this code path with the
    /// peer's base URL.
    pub fn with_upstream(&self, upstream: Hub) -> Self {
        Self::new(
            self.store.clone(),
            Arc::clone(&self.cache),
            Arc::clone(&self.access),
            upstream,
        )
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn cache(&self) -> &Arc<BlockCache> {
        &self.cache
    }

    pub fn upstream(&self) -> &Hub {
        &self.upstream
    }

    pub fn is_offline(&self) -> bool {
        self.upstream.is_offline()
    }

    /// `GET /api/whoami-v2`, answered upstream, or by a synthetic anonymous identity while
    /// this node is offline so `hf auth whoami` answers instead of hanging.
    pub async fn whoami(&self, auth: Option<&Authorization>) -> Result<ProxyResponse> {
        if self.is_offline() {
            return Ok(anonymous_whoami());
        }
        self.upstream.whoami(auth).await
    }

    /// The Xet token route, behind the same rules the resolve routes apply so it cannot be
    /// used to reach a repository the rules deny.
    pub async fn xet_read_token(
        &self,
        repo: &RepoId,
        revision: &str,
        auth: Option<&Authorization>,
    ) -> Result<ProxyResponse> {
        let subject = repo.reference();
        if !self.access.proxy_allowed(&subject) {
            return Err(MirrorError::RepoNotFound(subject));
        }
        self.require_online(&subject)?;
        self.upstream.xet_read_token(repo, revision, auth).await
    }

    /// The Xet content route, forwarded so `hf_xet` reaches its bytes through the mirror.
    pub async fn xet_bridge(
        &self,
        repo_hash: &str,
        hash: &str,
        auth: Option<&Authorization>,
    ) -> Result<ProxyResponse> {
        self.require_online(hash)?;
        self.upstream.xet_bridge(repo_hash, hash, auth).await
    }

    fn require_online(&self, subject: &str) -> Result<()> {
        if self.is_offline() {
            return Err(MirrorError::Offline(subject.to_owned()));
        }
        Ok(())
    }

    /// Resolves a branch, tag or sha in the documented order: the index, then the upstream
    /// when it is reachable and the rules allow it, then whatever revision the metadata
    /// cache last saw.
    pub async fn resolve_revision(
        &self,
        repo: &RepoId,
        revision: &str,
        auth: Option<&Authorization>,
    ) -> Result<Resolution> {
        let wanted = match revision.trim() {
            "" => DEFAULT_REVISION,
            named => named,
        };
        if let Some(sha) = self.index_revision(repo, wanted).await? {
            return Ok(Resolution {
                sha,
                source: RevisionSource::Index,
            });
        }
        let key = format!("{}@{wanted}", repo.qualified());
        if let Some(sha) = self.revisions.get(&key).await {
            return Ok(Resolution {
                sha,
                source: RevisionSource::Upstream,
            });
        }
        if !self.access.proxy_allowed(&repo.reference()) {
            return self.cached_revision(repo, wanted).await.map_or_else(
                || Err(MirrorError::RepoNotFound(repo.reference())),
                |sha| {
                    Ok(Resolution {
                        sha,
                        source: RevisionSource::Cache,
                    })
                },
            );
        }
        if self.is_offline() {
            return self.cached_revision(repo, wanted).await.map_or_else(
                || {
                    Err(MirrorError::Offline(format!(
                        "{}@{wanted}",
                        repo.qualified()
                    )))
                },
                |sha| {
                    Ok(Resolution {
                        sha,
                        source: RevisionSource::Cache,
                    })
                },
            );
        }
        match self.upstream_revision(repo, wanted, auth).await {
            Ok(sha) => {
                self.revisions.insert(key, sha).await;
                Ok(Resolution {
                    sha,
                    source: RevisionSource::Upstream,
                })
            }
            Err(error) => match self.cached_revision(repo, wanted).await {
                Some(sha) => Ok(Resolution {
                    sha,
                    source: RevisionSource::Cache,
                }),
                None => Err(error),
            },
        }
    }

    /// Opens one path of one revision, choosing the cheapest source that can serve it.
    pub async fn open_file(&self, request: FileRequest<'_>) -> Result<FileAnswer> {
        let path = clean_repo_path(request.path)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| MirrorError::EntryNotFound(request.path.to_owned()))?;
        let resolution = self
            .resolve_revision(request.repo, request.revision, request.auth)
            .await?;
        if let Some(answer) = self
            .local_answer(request.repo, &resolution.sha, &path)
            .await
        {
            return Ok(answer);
        }
        if let Some(answer) = self.cached_answer(&request, &resolution.sha, &path).await? {
            return Ok(answer);
        }
        self.upstream_answer(&request, &resolution.sha, path).await
    }

    async fn index_revision(&self, repo: &RepoId, revision: &str) -> Result<Option<CommitSha>> {
        let wanted = repo.clone();
        let named = revision.to_owned();
        read_index(&self.store, move |conn| {
            let Ok(row) = repo_row(conn, &wanted) else {
                return Ok(None);
            };
            Ok(revision_row(conn, &row, &named)
                .ok()
                .map(|revision| revision.sha))
        })
        .await
    }

    async fn upstream_revision(
        &self,
        repo: &RepoId,
        revision: &str,
        auth: Option<&Authorization>,
    ) -> Result<CommitSha> {
        if !self.upstream.probe_repo(repo, auth).await? {
            return Err(MirrorError::RepoNotFound(repo.reference()));
        }
        let found = self
            .upstream
            .revision(repo, revision, auth)
            .await?
            .ok_or_else(|| MirrorError::RevisionNotFound(revision.to_owned()))?;
        if self.access.cache_allowed(&repo.reference())
            && let Err(error) = self
                .write_revision_meta(repo, &found.sha, &found.body)
                .await
        {
            tracing::warn!(repo = %repo.qualified(), %error, "revision metadata was not cached");
        }
        Ok(found.sha)
    }

    /// The newest revision the metadata cache holds, or the requested sha when the cache
    /// already has something under it.
    async fn cached_revision(&self, repo: &RepoId, revision: &str) -> Option<CommitSha> {
        if let Ok(sha) = revision.parse::<CommitSha>()
            && self.has_cached_revision(repo, &sha).await
        {
            return Some(sha);
        }
        let root = self.revision_meta_root(repo).ok()?;
        let mut entries = fs::read_dir(&root).await.ok()?;
        let mut newest: Option<(HubTimestamp, CommitSha)> = None;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Some(found) = read_revision_meta(&entry.path().join(REVISION_META_FILE)).await
            else {
                continue;
            };
            if newest.as_ref().is_none_or(|(seen, _)| found.0 > *seen) {
                newest = Some(found);
            }
        }
        newest.map(|(_, sha)| sha)
    }

    /// Whether the cache holds anything under a revision: its document, or the files that
    /// were fetched at it.
    async fn has_cached_revision(&self, repo: &RepoId, sha: &CommitSha) -> bool {
        let candidates = [
            self.cache.layout().revision_meta(repo, sha).ok(),
            self.revision_files_root(repo, sha).ok(),
        ];
        for path in candidates.into_iter().flatten() {
            if fs::metadata(&path).await.is_ok() {
                return true;
            }
        }
        false
    }

    fn revision_files_root(&self, repo: &RepoId, sha: &CommitSha) -> Result<PathBuf> {
        Ok(self
            .cache
            .layout()
            .file_entry(repo, sha, "any")?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default())
    }

    fn revision_meta_root(&self, repo: &RepoId) -> Result<PathBuf> {
        let placeholder = CommitSha::from_bytes([0u8; 20]);
        Ok(self
            .cache
            .layout()
            .revision_meta(repo, &placeholder)?
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default())
    }

    async fn write_revision_meta(
        &self,
        repo: &RepoId,
        sha: &CommitSha,
        body: &Bytes,
    ) -> Result<()> {
        let path = self.cache.layout().revision_meta(repo, sha)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let envelope = serde_json::json!({
            "status_code": 200,
            "headers": { "content-type": "application/json" },
        });
        write_atomic(&path, &serde_json::to_vec(&envelope)?).await?;
        write_atomic(&body_path(&path), body).await
    }

    async fn local_answer(&self, repo: &RepoId, sha: &CommitSha, path: &str) -> Option<FileAnswer> {
        let location = index_repo::locate(&self.store, repo, sha, path)
            .await
            .ok()?;
        if location.lfs_pointer_only {
            return None;
        }
        Some(local_answer(location, sha, path))
    }

    async fn cached_answer(
        &self,
        request: &FileRequest<'_>,
        sha: &CommitSha,
        path: &str,
    ) -> Result<Option<FileAnswer>> {
        let repo = request.repo;
        let dir = self.cache.layout().file_entry(repo, sha, path)?;
        if fs::metadata(&dir).await.is_err() {
            return Ok(None);
        }
        let Ok(entry) = self.cache.existing_entry(&dir).await else {
            return Ok(None);
        };
        if !entry.is_complete() {
            return Ok(None);
        }
        let size = entry.meta().file_size();
        let etag = String::from_utf8(entry.meta().etag().to_vec())
            .ok()
            .filter(|etag| !etag.is_empty())
            .map(quoted)
            .unwrap_or_else(|| quoted(sha.to_hex()));
        let (linked_etag, linked_size) = linked_from_etag(&etag, size);
        let remote = self.remote_bytes(request, sha, path)?;
        Ok(Some(FileAnswer {
            commit: *sha,
            size,
            etag,
            linked_etag,
            linked_size,
            content_type: guess_content_type(path),
            source: ByteSource::Cache(Box::new(CachedSource {
                entry: Arc::new(Mutex::new(entry)),
                remote,
            })),
        }))
    }

    /// How a block of this file would be fetched, when the rules and the network allow it
    /// at all.
    fn remote_bytes(
        &self,
        request: &FileRequest<'_>,
        sha: &CommitSha,
        path: &str,
    ) -> Result<Option<RemoteBytes>> {
        if self.is_offline() || !self.access.proxy_allowed(&request.repo.reference()) {
            return Ok(None);
        }
        Ok(Some(RemoteBytes {
            hub: self.upstream.clone(),
            url: self
                .upstream
                .resolve_url(request.repo, &sha.to_hex(), path)?,
            auth: request.auth.cloned(),
        }))
    }

    async fn upstream_answer(
        &self,
        request: &FileRequest<'_>,
        sha: &CommitSha,
        path: String,
    ) -> Result<FileAnswer> {
        let repo = request.repo;
        let subject = repo.reference();
        if !self.access.proxy_allowed(&subject) {
            return Err(MirrorError::RepoNotFound(subject));
        }
        if self.is_offline() {
            return Err(MirrorError::Offline(format!(
                "{}/{path}@{}",
                repo.qualified(),
                sha.to_hex()
            )));
        }
        let remote = self
            .upstream
            .file_metadata(repo, &sha.to_hex(), &path, request.auth)
            .await?;
        let etag = remote
            .etag
            .clone()
            .or_else(|| remote.linked_etag.clone())
            .ok_or_else(|| {
                MirrorError::ProxyInvalidData(format!("{path} was served without an etag"))
            })?;
        let entry = if request.head || !self.access.cache_allowed(&subject) {
            None
        } else {
            let cache_etag = remote.cache_etag().unwrap_or(etag.as_str());
            Some(Arc::new(Mutex::new(
                self.cache
                    .file_entry(repo, sha, &path, remote.size, Some(cache_etag.as_bytes()))
                    .await?,
            )))
        };
        let ingest = entry.as_ref().map(|entry| IngestTrigger {
            store: self.store.clone(),
            cache: Arc::clone(&self.cache),
            entry: Arc::clone(entry),
            repo: repo.clone(),
            commit: *sha,
            path: path.clone(),
            expected: remote.linked_etag.as_deref().and_then(parse_sha256),
        });
        Ok(FileAnswer {
            commit: remote.commit.unwrap_or(*sha),
            size: remote.size,
            etag,
            linked_etag: remote.linked_etag.clone(),
            linked_size: remote.linked_size,
            content_type: remote
                .content_type
                .clone()
                .unwrap_or_else(|| guess_content_type(&path)),
            source: ByteSource::Upstream(Box::new(UpstreamSource {
                remote: RemoteBytes {
                    hub: self.upstream.clone(),
                    url: self.upstream.resolve_url(repo, &sha.to_hex(), &path)?,
                    auth: request.auth.cloned(),
                },
                entry,
                ingest,
            })),
        })
    }
}

fn local_answer(location: FileLocation, sha: &CommitSha, path: &str) -> FileAnswer {
    let etag = if location.is_lfs {
        location.sha256.map(|hash| quoted(hash.to_hex()))
    } else {
        location
            .git_sha1
            .map(|hash| quoted(hash.to_hex()))
            .or_else(|| location.sha256.map(|hash| quoted(hash.to_hex())))
    }
    .unwrap_or_else(|| quoted(sha.to_hex()));
    let linked = location
        .is_lfs
        .then(|| location.sha256.map(|hash| quoted(hash.to_hex())))
        .flatten();
    FileAnswer {
        commit: location.commit,
        size: location.size,
        etag,
        linked_size: linked.as_ref().map(|_| location.size),
        linked_etag: linked,
        content_type: guess_content_type(path),
        source: ByteSource::Local(location.abs_path),
    }
}

/// The linked headers a cached entry implies: an etag that is a content hash is the hash of
/// the media the entry holds.
fn linked_from_etag(etag: &str, size: u64) -> (Option<String>, Option<u64>) {
    match parse_sha256(etag) {
        Some(_) => (Some(etag.to_owned()), Some(size)),
        None => (None, None),
    }
}

fn parse_sha256(etag: &str) -> Option<Sha256> {
    let inner = etag.trim().trim_matches('"');
    if inner.len() != SHA256_HEX_LEN {
        return None;
    }
    inner.parse::<Sha256>().ok()
}

fn guess_content_type(path: &str) -> String {
    mime_guess::from_path(path)
        .first()
        .map_or_else(|| OCTET_STREAM.to_owned(), |mime| mime.to_string())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut file = fs::File::create(&temp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&temp, path).await?;
    Ok(())
}

/// Reads a cached revision document, whether it is a plain mirror document or the response
/// envelope the proxy writes with its body in a sidecar.
async fn read_revision_meta(path: &Path) -> Option<(HubTimestamp, CommitSha)> {
    let raw = match fs::read(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(_) => return None,
    };
    let document: Value = match serde_json::from_slice(&raw) {
        Ok(document) => document,
        Err(_) => return None,
    };
    let document = if document.get("sha").is_some() {
        document
    } else {
        let body = fs::read(body_path(path)).await.ok()?;
        serde_json::from_slice(&body).ok()?
    };
    let sha = document.get("sha")?.as_str()?.parse::<CommitSha>().ok()?;
    let modified = document
        .get("lastModified")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<HubTimestamp>().ok())
        .unwrap_or_else(|| HubTimestamp::new(jiff::Timestamp::UNIX_EPOCH));
    Some((modified, sha))
}

/// One piece of a response body: framing the mirror wrote, or bytes of the file.
#[derive(Debug)]
enum Step {
    Literal(Bytes),
    Range(ByteRange),
}

fn body_steps(answer: &FileAnswer, served: &ServedResponse) -> Vec<Step> {
    match &served.outcome {
        RangeOutcome::Whole if answer.size > 0 => {
            vec![Step::Range(ByteRange::new(0, answer.size - 1))]
        }
        RangeOutcome::Whole | RangeOutcome::Unsatisfiable => Vec::new(),
        RangeOutcome::Single(range) => vec![Step::Range(*range)],
        RangeOutcome::Multi(ranges) => {
            let boundary = multipart_boundary(&answer.etag, answer.size, ranges);
            let mut steps = Vec::with_capacity(ranges.len() * 3 + 1);
            for range in ranges {
                steps.push(Step::Literal(Bytes::from(multipart_part_header(
                    &boundary,
                    *range,
                    answer.size,
                ))));
                steps.push(Step::Range(*range));
                steps.push(Step::Literal(Bytes::from_static(b"\r\n")));
            }
            steps.push(Step::Literal(Bytes::from(multipart_terminator(&boundary))));
            steps
        }
    }
}

#[derive(Debug)]
struct BodyState {
    steps: std::vec::IntoIter<Step>,
    cursor: Option<(u64, u64)>,
    reader: Reader,
}

#[derive(Debug)]
enum Reader {
    Local(Box<fs::File>),
    Blocks(Box<BlockReader>),
    Remote(Box<RemoteBytes>),
}

/// Reads a file out of its cache entry, fetching any block that is missing or that fails
/// its CRC when an upstream is available.
#[derive(Debug)]
struct BlockReader {
    entry: SharedEntry,
    remote: Option<RemoteBytes>,
    ingest: Option<IngestTrigger>,
}

impl Reader {
    async fn open(answer: &FileAnswer) -> Result<Self> {
        match &answer.source {
            ByteSource::Local(path) => Ok(Reader::Local(Box::new(fs::File::open(path).await?))),
            ByteSource::Cache(source) => Ok(Reader::Blocks(Box::new(BlockReader {
                entry: Arc::clone(&source.entry),
                remote: source.remote.clone(),
                ingest: None,
            }))),
            ByteSource::Upstream(source) => match &source.entry {
                Some(entry) => Ok(Reader::Blocks(Box::new(BlockReader {
                    entry: Arc::clone(entry),
                    remote: Some(source.remote.clone()),
                    ingest: source.ingest.clone(),
                }))),
                None => Ok(Reader::Remote(Box::new(source.remote.clone()))),
            },
        }
    }

    async fn read(&mut self, position: u64, limit: u64) -> Result<Bytes> {
        match self {
            Reader::Local(file) => {
                let length = usize::try_from(limit.min(LOCAL_READ)).unwrap_or(usize::MAX);
                let mut buffer = vec![0u8; length];
                file.seek(std::io::SeekFrom::Start(position)).await?;
                let read = file.read(&mut buffer).await?;
                buffer.truncate(read);
                Ok(Bytes::from(buffer))
            }
            Reader::Blocks(reader) => reader.read(position, limit).await,
            Reader::Remote(remote) => {
                let end = position + limit.min(UNCACHED_READ);
                remote
                    .hub
                    .download_range(&remote.url, remote.auth.as_ref(), position..end)
                    .await
            }
        }
    }

    /// Runs the ingest of a file whose last block just landed. Fire and forget: a client
    /// waits for its bytes, not for the index.
    fn finish(&mut self) {
        let Reader::Blocks(reader) = self else {
            return;
        };
        let Some(trigger) = reader.ingest.take() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = trigger.run().await {
                tracing::warn!(%error, "a completed cache entry was not ingested");
            }
        });
    }
}

impl BlockReader {
    async fn read(&mut self, position: u64, limit: u64) -> Result<Bytes> {
        let mut entry = self.entry.lock().await;
        let index = entry.meta().geometry().block_of(position);
        let end = block_end(&entry, position, limit);
        if !entry.has_block(index) {
            self.fill(&mut entry, index).await?;
        }
        match entry.read_range(position..end).await {
            Ok(bytes) => Ok(Bytes::from(bytes)),
            Err(error) if lost_block(&error) => {
                self.fill(&mut entry, index).await?;
                Ok(Bytes::from(entry.read_range(position..end).await?))
            }
            Err(error) => Err(error),
        }
    }

    async fn fill(&self, entry: &mut CacheEntry, index: u64) -> Result<()> {
        let remote = self.remote.as_ref().ok_or_else(|| {
            MirrorError::CacheCorrupt(format!(
                "block {index} of {} cannot be fetched again",
                entry.dir().display()
            ))
        })?;
        remote.fill(entry, index).await
    }
}

/// Whether a failed block read means the block is no longer there: a CRC mismatch, which
/// drops the block, or a block another task dropped between the presence check and the read.
fn lost_block(error: &MirrorError) -> bool {
    match error {
        MirrorError::CacheCorrupt(_) => true,
        MirrorError::Io(error) => error.kind() == ErrorKind::NotFound,
        _ => false,
    }
}

/// The end of the read that keeps one read inside one block.
fn block_end(entry: &CacheEntry, position: u64, limit: u64) -> u64 {
    let block = entry
        .meta()
        .block_range(entry.meta().geometry().block_of(position));
    (position + limit).min(block.end.max(position))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use hoard_core::{NodeId, RepoType};
    use hoard_index::config::SourceKind;
    use hoard_index::store::blob::{set_sha256, upsert_blob};
    use hoard_index::store::entry::{NewEntry, upsert_entry};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};
    use hoard_index::store::revision::{NewRevision, RefKind, upsert_ref, upsert_revision};
    use jiff::Timestamp;
    use sha2::{Digest, Sha256 as Sha256Hasher};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::access::AccessRule;
    use crate::config::{Geometry, MirrorConfig};
    use crate::upstream::client::{RetryPolicy, UpstreamClient};

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";
    const OTHER_SHA: &str = "1111111111111111111111111111111111111111";

    struct Harness {
        mirror: Mirror,
        _root: TempDir,
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn commit() -> CommitSha {
        SHA.parse().expect("sha parses")
    }

    fn config(offline: bool, allow_cache: bool) -> MirrorConfig {
        MirrorConfig {
            offline,
            geometry: Geometry::new(64, 16).expect("valid geometry"),
            proxy: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
            cache: if allow_cache {
                vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)]
            } else {
                Vec::new()
            },
            ..MirrorConfig::default()
        }
    }

    fn harness(upstream: &str, config: MirrorConfig) -> Harness {
        let root = TempDir::new().expect("temp dir");
        let offline = config.offline;
        let access = AccessPolicy::compile(&config).expect("rules compile");
        let cache = BlockCache::at_root(Arc::new(config), root.path());
        let client = UpstreamClient::new(offline)
            .expect("client builds")
            .with_retry(RetryPolicy::immediate(2));
        let hub = Hub::new(client, upstream).expect("hub builds");
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        upsert_node(
            &conn,
            &NewNode {
                node_id: NodeId::new("node-a").expect("node id"),
                name: "hoard".to_owned(),
                url: None,
                is_self: true,
            },
        )
        .expect("node inserts");
        drop(conn);
        Harness {
            mirror: Mirror::new(store, Arc::new(cache), Arc::new(access), hub),
            _root: root,
        }
    }

    fn index_file(store: &Store, dir: &Path, bytes: &[u8]) -> PathBuf {
        let abs = dir.join("model.safetensors");
        std::fs::write(&abs, bytes).expect("file writes");
        let conn = store.write().expect("writer");
        let node = hoard_index::store::node::local_node(&conn).expect("node");
        let repo_row = upsert_repo(&conn, &NewRepo::new(repo(), node.id, SourceKind::Loose))
            .expect("repo inserts");
        let revision = upsert_revision(&conn, &NewRevision::new(repo_row, commit()))
            .expect("revision inserts");
        upsert_ref(&conn, repo_row, "main", RefKind::Branch, revision).expect("ref inserts");
        let blob = upsert_blob(&conn, "fingerprint", bytes.len() as u64, Timestamp::now())
            .expect("blob inserts");
        set_sha256(
            &conn,
            blob,
            Sha256::from_bytes(Sha256Hasher::digest(bytes).into()),
            Some(hoard_index::hash::git_blob_sha1_bytes(bytes)),
            Timestamp::now(),
        )
        .expect("hashes store");
        let mut entry = NewEntry::new(revision, "model.safetensors", node.id);
        entry.blob = Some(blob);
        entry.abs_path = Some(abs.to_string_lossy().into_owned());
        entry.size = bytes.len() as u64;
        upsert_entry(&conn, &entry, Timestamp::now()).expect("entry inserts");
        abs
    }

    async fn collect(answer: &FileAnswer, range: Option<&str>) -> Vec<u8> {
        let mut stream = read_range(answer, range).await.expect("body opens");
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk reads"));
        }
        out
    }

    fn upstream_file(server: &MockServer, bytes: Vec<u8>) -> impl Future<Output = ()> {
        let size = bytes.len();
        let head = Mock::given(method("HEAD"))
            .and(path_matcher(format!(
                "/openai/whisper/resolve/{SHA}/model.safetensors"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", size.to_string().as_str())
                    .insert_header("etag", "\"upstream-etag\"")
                    .insert_header("x-xet-hash", "deadbeef")
                    .insert_header("x-repo-commit", SHA),
            );
        let get = Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/openai/whisper/resolve/{SHA}/model.safetensors"
            )))
            .respond_with(move |request: &wiremock::Request| {
                let range = request
                    .headers
                    .get("range")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                let (start, end) = parse_test_range(range, bytes.len() as u64);
                ResponseTemplate::new(206)
                    .set_body_bytes(bytes[start as usize..end as usize].to_vec())
            });
        async move {
            head.mount(server).await;
            get.mount(server).await;
        }
    }

    fn parse_test_range(header: &str, size: u64) -> (u64, u64) {
        let Some(spec) = header.strip_prefix("bytes=") else {
            return (0, size);
        };
        let mut parts = spec.split('-');
        let start = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let end = parts
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(size, |value| (value + 1).min(size));
        (start, end)
    }

    fn mount_revision(server: &MockServer, sha: &str) -> impl Future<Output = ()> {
        let probe = Mock::given(method("HEAD"))
            .and(path_matcher("/api/models/openai/whisper"))
            .respond_with(ResponseTemplate::new(200));
        let revision = Mock::given(method("GET"))
            .and(path_matcher("/api/models/openai/whisper/revision/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": sha,
                "lastModified": "2026-01-17T10:22:31.000Z",
            })));
        async move {
            probe.mount(server).await;
            revision.mount(server).await;
        }
    }

    #[tokio::test]
    async fn the_index_answers_a_revision_before_anything_else() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), config(false, true));
        let dir = TempDir::new().expect("temp dir");
        index_file(harness.mirror.store(), dir.path(), b"weights");

        let resolution = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect("resolves");
        assert_eq!(resolution.sha, commit());
        assert_eq!(resolution.source, RevisionSource::Index);
    }

    #[tokio::test]
    async fn an_upstream_revision_needs_a_visibility_probe_first() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let harness = harness(&server.uri(), config(false, true));

        let resolution = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect("resolves");
        assert_eq!(resolution.sha, commit());
        assert_eq!(resolution.source, RevisionSource::Upstream);

        let cached = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect("resolves from memory");
        assert_eq!(cached.sha, commit());

        let meta = harness
            .mirror
            .cache()
            .layout()
            .revision_meta(&repo(), &commit())
            .expect("path builds");
        assert!(meta.exists(), "the revision document is cached on disk");
        assert!(body_path(&meta).exists());
    }

    #[tokio::test]
    async fn an_invisible_repository_is_reported_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), config(false, true));
        let error = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect_err("upstream denies");
        assert!(matches!(error, MirrorError::RepoNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn a_denied_repository_never_reaches_the_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let mut config = config(false, true);
        config.proxy = vec![AccessRule::glob("other/*", true)];
        let harness = harness(&server.uri(), config);

        let error = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect_err("the rules deny it");
        assert!(matches!(error, MirrorError::RepoNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn offline_falls_back_to_the_newest_cached_revision() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), config(true, true));

        for (sha, modified) in [
            (SHA, "2026-01-17T10:22:31.000Z"),
            (OTHER_SHA, "2025-01-17T10:22:31.000Z"),
        ] {
            let parsed = sha.parse::<CommitSha>().expect("sha parses");
            let path = harness
                .mirror
                .cache()
                .layout()
                .revision_meta(&repo(), &parsed)
                .expect("path builds");
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir creates");
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "sha": sha, "lastModified": modified,
                }))
                .expect("json"),
            )
            .expect("meta writes");
        }

        let resolution = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect("resolves offline");
        assert_eq!(resolution.sha, commit());
        assert_eq!(resolution.source, RevisionSource::Cache);
    }

    #[tokio::test]
    async fn offline_without_a_cached_revision_says_so() {
        let harness = harness("http://127.0.0.1:1", config(true, true));
        let error = harness
            .mirror
            .resolve_revision(&repo(), "main", None)
            .await
            .expect_err("nothing is cached");
        assert!(matches!(error, MirrorError::Offline(_)), "{error}");
    }

    #[tokio::test]
    async fn a_local_file_is_served_from_the_index_with_its_git_etag() {
        let harness = harness("http://127.0.0.1:1", config(true, true));
        let dir = TempDir::new().expect("temp dir");
        let bytes = b"local weights".to_vec();
        index_file(harness.mirror.store(), dir.path(), &bytes);

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(answer.size, bytes.len() as u64);
        assert_eq!(
            answer.etag,
            quoted(hoard_index::hash::git_blob_sha1_bytes(&bytes).to_hex())
        );
        assert!(matches!(answer.source, ByteSource::Local(_)));
        assert_eq!(collect(&answer, None).await, bytes);
        assert_eq!(
            collect(&answer, Some("bytes=6-10")).await,
            b"weigh".to_vec()
        );
    }

    #[tokio::test]
    async fn a_traversing_path_is_refused() {
        let harness = harness("http://127.0.0.1:1", config(true, true));
        let error = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "../../etc/passwd"))
            .await
            .expect_err("traversal is refused");
        assert!(matches!(error, MirrorError::EntryNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn an_upstream_file_is_block_cached_and_served() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes: Vec<u8> = (0..=200u8).collect();
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, true));

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(answer.size, bytes.len() as u64);
        assert_eq!(answer.etag, "\"upstream-etag\"");
        assert_eq!(collect(&answer, None).await, bytes);

        let served = served_response(&answer, None);
        assert_eq!(served.status, 200);
        assert_eq!(served.header("x-repo-commit"), Some(SHA));
        assert_eq!(served.header("accept-ranges"), Some("bytes"));
        assert!(
            served
                .headers
                .iter()
                .all(|(name, _)| !name.starts_with("x-xet-"))
        );

        let again = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens from the cache");
        assert!(matches!(again.source, ByteSource::Cache(_)));
        assert_eq!(collect(&again, Some("bytes=-10")).await, bytes[191..]);
    }

    #[tokio::test]
    async fn a_head_never_creates_a_cache_entry() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        upstream_file(&server, vec![4u8; 100]).await;
        let harness = harness(&server.uri(), config(false, true));

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors").head(true))
            .await
            .expect("opens");
        assert_eq!(answer.size, 100);
        assert!(answer.ingest().is_none());
        let dir = harness
            .mirror
            .cache()
            .layout()
            .file_entry(&repo(), &commit(), "model.safetensors")
            .expect("path builds");
        assert!(!dir.exists(), "HEAD wrote {}", dir.display());

        let served = served_response(&answer, None);
        assert_eq!(served.status, 200);
        assert_eq!(served.content_length, 100);
    }

    #[tokio::test]
    async fn a_cache_denial_serves_the_bytes_without_persisting_them() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes = vec![5u8; 130];
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, false));

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(collect(&answer, None).await, bytes);
        assert!(answer.ingest().is_none());
        let dir = harness
            .mirror
            .cache()
            .layout()
            .file_entry(&repo(), &commit(), "model.safetensors")
            .expect("path builds");
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn a_completed_download_ingests_into_the_index() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes: Vec<u8> = (0..129u8).collect();
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, true));

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(collect(&answer, None).await, bytes);

        let outcome = answer
            .ingest()
            .expect("a cached download ingests")
            .run()
            .await
            .expect("ingest runs")
            .expect("the entry is complete");
        assert_eq!(
            outcome.sha256,
            Sha256::from_bytes(Sha256Hasher::digest(&bytes).into())
        );
        assert_eq!(outcome.size, bytes.len() as u64);
    }

    #[tokio::test]
    async fn a_range_answer_carries_the_documented_headers() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes: Vec<u8> = (0..200u8).collect();
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, true));
        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");

        let single = served_response(&answer, Some("bytes=10-19"));
        assert_eq!(single.status, 206);
        assert_eq!(single.header("content-range"), Some("bytes 10-19/200"));
        assert_eq!(single.header("content-length"), Some("10"));
        assert_eq!(collect(&answer, Some("bytes=10-19")).await, bytes[10..20]);

        let unsatisfiable = served_response(&answer, Some("bytes=500-600"));
        assert_eq!(unsatisfiable.status, 416);
        assert_eq!(unsatisfiable.header("content-range"), Some("bytes */200"));
        assert_eq!(unsatisfiable.content_length, 0);
        assert!(collect(&answer, Some("bytes=500-600")).await.is_empty());

        let multi = served_response(&answer, Some("bytes=0-9,100-109"));
        assert_eq!(multi.status, 206);
        let content_type = multi.header("content-type").expect("content type");
        assert!(
            content_type.starts_with("multipart/byteranges; boundary="),
            "{content_type}"
        );
        let body = collect(&answer, Some("bytes=0-9,100-109")).await;
        assert_eq!(body.len() as u64, multi.content_length);
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(text.contains("content-range: bytes 0-9/200"), "{text}");
        assert!(text.contains("content-range: bytes 100-109/200"), "{text}");
    }

    #[tokio::test]
    async fn a_corrupt_block_is_refetched_rather_than_served() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes: Vec<u8> = (0..129u8).collect();
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, true));
        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(collect(&answer, None).await, bytes);

        let dir = harness
            .mirror
            .cache()
            .layout()
            .file_entry(&repo(), &commit(), "model.safetensors")
            .expect("path builds");
        let block = dir.join("blocks/block_00000001.bin");
        std::fs::write(&block, vec![0xffu8; 64]).expect("block is corrupted");

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        assert_eq!(collect(&answer, None).await, bytes);
    }

    #[tokio::test]
    async fn an_olah_cache_directory_serves_its_files_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), config(true, true));
        let bytes: Vec<u8> = (0..129u8).collect();

        let mut entry = harness
            .mirror
            .cache()
            .file_entry(
                &repo(),
                &commit(),
                "model.safetensors",
                bytes.len() as u64,
                Some(b"\"olah-etag\""),
            )
            .await
            .expect("entry opens");
        for index in 0..entry.meta().block_count() {
            let range = entry.meta().block_range(index);
            entry
                .write_block(index, &bytes[range.start as usize..range.end as usize])
                .await
                .expect("block writes");
        }
        let meta = harness
            .mirror
            .cache()
            .layout()
            .revision_meta(&repo(), &commit())
            .expect("path builds");
        std::fs::create_dir_all(meta.parent().expect("parent")).expect("dir creates");
        std::fs::write(
            &meta,
            serde_json::to_vec(&serde_json::json!({
                "sha": SHA, "lastModified": "2026-01-17T10:22:31.000Z",
            }))
            .expect("json"),
        )
        .expect("meta writes");
        drop(entry);

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens from the olah cache");
        assert!(matches!(answer.source, ByteSource::Cache(_)));
        assert_eq!(answer.etag, "\"olah-etag\"");
        assert_eq!(collect(&answer, None).await, bytes);
    }

    #[tokio::test]
    async fn a_head_never_extends_a_partly_cached_file() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        let bytes: Vec<u8> = (0..129u8).collect();
        upstream_file(&server, bytes.clone()).await;
        let harness = harness(&server.uri(), config(false, true));

        let mut entry = harness
            .mirror
            .cache()
            .file_entry(
                &repo(),
                &commit(),
                "model.safetensors",
                bytes.len() as u64,
                Some(b"\"upstream-etag\""),
            )
            .await
            .expect("entry opens");
        entry
            .write_block(0, &bytes[0..64])
            .await
            .expect("first block writes");
        drop(entry);

        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors").head(true))
            .await
            .expect("opens");
        assert_eq!(answer.size, bytes.len() as u64);
        assert!(answer.ingest().is_none());

        let blocks = harness
            .mirror
            .cache()
            .layout()
            .file_entry(&repo(), &commit(), "model.safetensors")
            .expect("path builds")
            .join("blocks");
        let present = std::fs::read_dir(&blocks)
            .expect("blocks read")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bin"))
            .count();
        assert_eq!(present, 1, "a HEAD fetched a block");
    }

    #[tokio::test]
    async fn a_peer_pull_is_the_same_resolve_against_the_peer() {
        let peer = MockServer::start().await;
        mount_revision(&peer, SHA).await;
        let bytes: Vec<u8> = (0..70u8).collect();
        upstream_file(&peer, bytes.clone()).await;

        let harness = harness("http://127.0.0.1:1", config(false, true));
        let client = UpstreamClient::new(false)
            .expect("client builds")
            .with_retry(RetryPolicy::immediate(2));
        let mirror = harness.mirror.with_upstream(
            crate::upstream::peer::peer_upstream(client, &peer.uri()).expect("peer"),
        );

        let answer = mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens from the peer");
        assert_eq!(collect(&answer, None).await, bytes);
        assert!(
            harness
                .mirror
                .cache()
                .layout()
                .file_entry(&repo(), &commit(), "model.safetensors")
                .expect("path builds")
                .exists(),
            "a peer pull lands in the same block cache"
        );
    }

    #[tokio::test]
    async fn whoami_answers_offline_without_asking_anyone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let harness = harness(&server.uri(), config(true, true));
        let identity = harness.mirror.whoami(None).await.expect("answers");
        assert_eq!(identity.status, 200);
        let body: serde_json::Value =
            serde_json::from_slice(&identity.body).expect("json identity");
        assert_eq!(body["name"], "anonymous");
    }

    #[tokio::test]
    async fn the_xet_routes_are_forwarded_behind_the_access_rules() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/api/models/openai/whisper/xet-read-token/{SHA}"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-xet-refresh-route", "/api/models/openai/whisper")
                    .set_body_string("{\"accessToken\":\"xet\"}"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/xet-bridge-us/repohash/filehash"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 8]))
            .mount(&server)
            .await;

        let open = harness(&server.uri(), config(false, true));
        let token = open
            .mirror
            .xet_read_token(&repo(), SHA, None)
            .await
            .expect("token forwards");
        assert_eq!(token.status, 200);
        assert_eq!(
            token.header("x-xet-refresh-route"),
            Some("/api/models/openai/whisper"),
            "the token route keeps the headers hf_xet needs"
        );
        let bridge = open
            .mirror
            .xet_bridge("repohash", "filehash", None)
            .await
            .expect("bridge forwards");
        assert_eq!(bridge.body.len(), 8);

        let mut denied = config(false, true);
        denied.proxy = vec![AccessRule::glob("other/*", true)];
        let strict = harness(&server.uri(), denied);
        let error = strict
            .mirror
            .xet_read_token(&repo(), SHA, None)
            .await
            .expect_err("the rules deny it");
        assert!(matches!(error, MirrorError::RepoNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn a_served_response_never_carries_the_xet_headers() {
        let server = MockServer::start().await;
        mount_revision(&server, SHA).await;
        upstream_file(&server, vec![2u8; 32]).await;
        let harness = harness(&server.uri(), config(false, true));
        let answer = harness
            .mirror
            .open_file(FileRequest::new(&repo(), "main", "model.safetensors"))
            .await
            .expect("opens");
        let served = served_response(&answer, None);
        assert_eq!(served.header("x-xet-hash"), None);
        assert!(
            served
                .headers
                .iter()
                .all(|(name, _)| !name.starts_with("x-xet-"))
        );
    }

    #[tokio::test]
    async fn an_lfs_etag_becomes_the_linked_headers() {
        let hash = "0".repeat(64);
        assert_eq!(
            linked_from_etag(&format!("\"{hash}\""), 42),
            (Some(format!("\"{hash}\"")), Some(42))
        );
        assert_eq!(linked_from_etag("\"abc\"", 42), (None, None));
    }
}
