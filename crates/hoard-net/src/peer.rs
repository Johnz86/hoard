//! Talking to another node: reading its digest, deriving states, and pulling its bytes.
//!
//! A peer speaks the same Hub face this node serves, so a pull is not a second transfer
//! protocol: it is an ordinary block-cached resolve against the peer's base URL, which
//! brings resumption and single-flight block downloads with it. What this module adds is
//! the verification at the end, the job row the PULLING state reads, and the index write
//! that turns the fetched bytes into a local, searchable artifact.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use jiff::Timestamp;
use serde::Deserialize;
use sha2::{Digest, Sha256 as Sha256Hasher};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use hoard_core::{ArtifactId, BlobId, CommitSha, EntryId, JobId, NodeId, RepoId, Sha256, State};
use hoard_index::config::SourceKind;
use hoard_index::hash::git_blob_sha1;
use hoard_index::infer::{InferInput, infer};
use hoard_index::sniff::sniff_path;
use hoard_index::store::Store;
use hoard_index::store::artifact::{
    NewArtifact, replace_evidence, set_card, set_tensor_summary, set_triage, upsert_artifact,
};
use hoard_index::store::blob::{blob_by_sha256, set_sha256, upsert_blob};
use hoard_index::store::entry::{NewEntry, entries_for_revision, upsert_entry};
use hoard_index::store::job::{JobProgress, create_job, fail_job, finish_job, update_job};
use hoard_index::store::node::local_node;
use hoard_index::store::repo::{NewRepo, repo_by_qualified, upsert_repo};
use hoard_index::store::revision::{NewRevision, revision_by_sha, upsert_revision};
use hoard_index::triage::needs_triage;
use hoard_mirror::resolve::{FileRequest, Mirror, read_range};
use hoard_mirror::upstream::UpstreamClient;
use hoard_mirror::upstream::peer::peer_upstream;

use crate::digest::{self, DEFAULT_LIMIT, DigestCursor, DigestPage};
use crate::discovery::{Discovery, DiscoveryMode, PeerEvent, forget_peer, record_peer, touch_peer};
use crate::error::{NetError, Result};
use crate::identity::NodeIdentity;

/// The job kind a peer pull runs under, which is what the JOBS tab groups it by.
pub const PULL_KIND: &str = "pull";

/// How often the gossip loop polls every known peer when nothing else wakes it.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// How many digest pages one poll of one peer consumes, so a large peer cannot starve the
/// others of a turn.
const PAGES_PER_POLL: u32 = 20;

/// How often a running pull writes its progress. Every chunk would be a write per megabyte.
const PROGRESS_EVERY: Duration = Duration::from_millis(400);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// How many times a pull re-records its file path before giving up on outlasting the
/// background ingest of the same bytes.
const PATH_ATTEMPTS: u32 = 5;
const PATH_RETRY: Duration = Duration::from_millis(60);

/// The directory a pull materializes bytes into, under the node's data directory.
pub const PULL_DIR: &str = "pulled";

/// What `GET /healthz` on a peer answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PeerHealth {
    pub status: String,
    #[serde(default)]
    pub peers: u64,
}

/// One row of a peer's `/api/v1/nodes`, read only to learn what the peer calls itself.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PeerNodeRow {
    name: String,
    #[serde(rename = "self", default)]
    is_self: bool,
}

/// The subset of a peer's `/api/v1/stats` this node records against its row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct PeerStats {
    #[serde(default)]
    pub artifacts: u64,
    #[serde(default)]
    pub indexed_bytes: u64,
}

/// The read-only client one node points at another.
///
/// Read routes are open on the LAN; the node token is carried only so a peer that requires
/// one for reads still answers. Nothing here ever writes to a peer.
#[derive(Debug, Clone)]
pub struct PeerClient {
    http: reqwest::Client,
    identity: NodeIdentity,
    token: Option<String>,
}

impl PeerClient {
    pub fn new(identity: NodeIdentity, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            identity,
            token,
        })
    }

    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    pub fn name(&self) -> &str {
        self.identity.name()
    }

    pub async fn health(&self) -> Result<PeerHealth> {
        self.read_json("healthz").await
    }

    pub async fn stats(&self) -> Result<PeerStats> {
        self.read_json("api/v1/stats").await
    }

    /// What the peer calls itself, so the NODES tab shows a name rather than an address.
    pub async fn advertised_name(&self) -> Result<Option<String>> {
        let rows: Vec<PeerNodeRow> = self.read_json("api/v1/nodes").await?;
        Ok(rows
            .into_iter()
            .find(|row| row.is_self)
            .map(|row| row.name)
            .filter(|name| !name.trim().is_empty()))
    }

    /// One page of the peer's digest, starting after `since`.
    pub async fn digest(&self, since: Option<DigestCursor>, limit: u32) -> Result<DigestPage> {
        let mut url = self
            .identity
            .url()
            .join("api/v1/peers/digest")
            .map_err(|error| NetError::Config(error.to_string()))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
            if let Some(cursor) = since {
                query.append_pair("since", &cursor.encode());
            }
        }
        let body = self.get(url).await?.text().await?;
        serde_json::from_str(&body).map_err(|error| NetError::BadDigest {
            peer: self.name().to_owned(),
            reason: error.to_string(),
        })
    }

    async fn read_json<T: serde::de::DeserializeOwned>(&self, suffix: &str) -> Result<T> {
        let url = self
            .identity
            .url()
            .join(suffix)
            .map_err(|error| NetError::Config(error.to_string()))?;
        Ok(self.get(url).await?.json().await?)
    }

    async fn get(&self, url: url::Url) -> Result<reqwest::Response> {
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        Ok(request.send().await?.error_for_status()?)
    }
}

/// Everything the STATE column depends on for one entry on one node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateFacts {
    /// This node holds the bytes.
    pub held: bool,
    /// This node offers them on the LAN.
    pub served: bool,
    /// A transfer is running for them right now.
    pub pulling: bool,
    /// The bytes are here but hoard could not say what they are.
    pub triage: bool,
    /// A pinned commit no longer matches the ref on the repository's origin.
    pub stale: bool,
}

/// The one place a state is decided, so the rule cannot drift between the table, the
/// inspector, the gossip writer and a pull.
///
/// The order is by urgency rather than by the order the design lists them: a running
/// transfer is what a reader wants to see first, bytes that are not here at all cannot be
/// anything but REMOTE, a pin that has moved is actionable, and TRIAGE outranks SERVED
/// because unclassified bytes should not read as ready to use.
pub fn derive_state(facts: StateFacts) -> State {
    if facts.pulling {
        return State::Pulling;
    }
    if !facts.held {
        return State::Remote;
    }
    if facts.stale {
        return State::Stale;
    }
    if facts.triage {
        return State::Triage;
    }
    match facts.served {
        true => State::Served,
        false => State::Local,
    }
}

/// Whether a pinned commit has moved on the repository it came from.
pub fn pin_is_stale(pinned: Option<CommitSha>, origin: Option<CommitSha>) -> bool {
    matches!((pinned, origin), (Some(pinned), Some(origin)) if pinned != origin)
}

/// One file to fetch from one peer.
#[derive(Debug, Clone, PartialEq)]
pub struct PullRequest {
    pub peer: NodeIdentity,
    pub repo: RepoId,
    pub revision: CommitSha,
    pub path: String,
    /// What the bytes must hash to. A pull that ends on anything else is refused.
    pub sha256: Sha256,
    pub size: u64,
}

impl PullRequest {
    pub fn new(
        peer: NodeIdentity,
        repo: RepoId,
        revision: CommitSha,
        path: impl Into<String>,
        sha256: Sha256,
        size: u64,
    ) -> Self {
        Self {
            peer,
            repo,
            revision,
            path: path.into(),
            sha256,
            size,
        }
    }

    /// The file name the pulled bytes land under.
    pub fn file_name(&self) -> &str {
        let trimmed = self.path.trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    }
}

/// What one completed pull added to this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub sha256: Sha256,
    pub size: u64,
    pub abs_path: PathBuf,
    pub blob: BlobId,
    pub artifact: ArtifactId,
    pub entry: EntryId,
}

/// Fetches files from peers through the block-cached proxy path and files them locally.
#[derive(Debug, Clone)]
pub struct Puller {
    store: Store,
    mirror: Arc<Mirror>,
    client: UpstreamClient,
    root: PathBuf,
    serve: bool,
    triage_confidence: f32,
}

impl Puller {
    /// `root` is where pulled bytes are materialized; `serve` is the `[node] serve` flag,
    /// which is the difference between a pulled artifact reading SERVED and reading LOCAL.
    ///
    /// The client is built with the network enabled even when the mirror is offline: a peer
    /// is the LAN, and refusing to cross the LAN is not what offline mode means.
    pub fn new(
        store: Store,
        mirror: Arc<Mirror>,
        root: impl Into<PathBuf>,
        serve: bool,
        triage_confidence: f32,
    ) -> Result<Self> {
        Ok(Self {
            store,
            mirror,
            client: UpstreamClient::new(false).map_err(NetError::Mirror)?,
            root: root.into(),
            serve,
            triage_confidence,
        })
    }

    /// Where a pulled file lands: readable, stable, and unique per revision.
    pub fn destination(&self, request: &PullRequest) -> PathBuf {
        let mut path = self.root.join(request.repo.qualified());
        path.push(request.revision.short());
        for segment in request.path.split('/').filter(|part| !part.is_empty()) {
            path.push(segment);
        }
        path
    }

    /// Pulls one file, verifies it and files it in the index.
    ///
    /// A job row is created if one is not supplied, so a pull always shows up in the JOBS
    /// tab. The entry shows PULLING with its percentage while bytes are moving.
    pub async fn pull(&self, request: &PullRequest, job: Option<JobId>) -> Result<PullOutcome> {
        let subject = format!("{} \u{2190} {}", request.path, request.peer.name());
        let job = match job {
            Some(id) => id,
            None => {
                self.store
                    .call_mut(move |conn| create_job(conn, PULL_KIND, Some(&subject)))
                    .await?
            }
        };
        match self.run(request, job).await {
            Ok(outcome) => {
                self.store
                    .call_mut(move |conn| finish_job(conn, job))
                    .await?;
                Ok(outcome)
            }
            Err(error) => {
                let message = error.to_string();
                let settled = self
                    .store
                    .call_mut(move |conn| fail_job(conn, job, &message))
                    .await;
                if let Err(settling) = settled {
                    tracing::error!(error = %settling, "the failed pull job could not be settled");
                }
                Err(error)
            }
        }
    }

    async fn run(&self, request: &PullRequest, job: JobId) -> Result<PullOutcome> {
        let destination = self.destination(request);
        let revision = self.local_revision(request).await?;
        self.mark(request, revision, &destination, State::Pulling, Some(0.0))
            .await?;

        let hub = peer_upstream(self.client.clone(), request.peer.url().as_str())
            .map_err(NetError::Mirror)?;
        let mirror = self.mirror.with_upstream(hub);
        let answer = mirror
            .open_file(FileRequest::new(
                &request.repo,
                &request.revision.to_hex(),
                &request.path,
            ))
            .await
            .map_err(NetError::Mirror)?;
        let total = match answer.size {
            0 => request.size,
            size => size,
        };
        if let Some(advertised) = advertised_sha256(&answer)
            && advertised != request.sha256
        {
            self.mark(request, revision, &destination, State::Remote, None)
                .await?;
            return Err(NetError::HashMismatch {
                peer: request.peer.name().to_owned(),
                path: request.path.clone(),
                expected: request.sha256.to_hex(),
                actual: advertised.to_hex(),
            });
        }

        let stream = read_range(&answer, None).await.map_err(NetError::Mirror)?;
        let sha256 = self
            .stream_to_disk(request, revision, &destination, stream, total, job)
            .await?;

        if sha256 != request.sha256 {
            if let Err(error) = tokio::fs::remove_file(&destination).await {
                tracing::warn!(path = %destination.display(), %error, "unverified bytes remain");
            }
            self.mark(request, revision, &destination, State::Remote, None)
                .await?;
            return Err(NetError::HashMismatch {
                peer: request.peer.name().to_owned(),
                path: request.path.clone(),
                expected: request.sha256.to_hex(),
                actual: sha256.to_hex(),
            });
        }
        if answer.ingest().is_some() {
            self.settle_ingest(revision, &request.path).await?;
        }
        self.file(request, revision, destination, sha256).await
    }

    /// Streams the answer into place, hashing as it goes so the verification costs one pass.
    async fn stream_to_disk(
        &self,
        request: &PullRequest,
        revision: hoard_core::RevisionId,
        destination: &Path,
        mut stream: hoard_mirror::resolve::ByteStream,
        total: u64,
        job: JobId,
    ) -> Result<Sha256> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = destination.with_extension("hoard-partial");
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut hasher = Sha256Hasher::new();
        let mut written = 0u64;
        let started = Instant::now();
        let mut reported = Instant::now();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(NetError::Mirror)?;
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
            written += chunk.len() as u64;
            if reported.elapsed() >= PROGRESS_EVERY {
                reported = Instant::now();
                let fraction = fraction(written, total);
                self.report(
                    request,
                    revision,
                    destination,
                    job,
                    written,
                    total,
                    started,
                    fraction,
                )
                .await?;
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, destination).await?;
        Ok(Sha256::from_bytes(hasher.finalize().into()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn report(
        &self,
        request: &PullRequest,
        revision: hoard_core::RevisionId,
        destination: &Path,
        job: JobId,
        written: u64,
        total: u64,
        started: Instant,
        fraction: f32,
    ) -> Result<()> {
        let elapsed = started.elapsed().as_secs_f64();
        let progress = JobProgress {
            phase: Some(format!(
                "pulling {} from {}",
                request.path,
                request.peer.name()
            )),
            progress: fraction,
            bytes: written,
            total: Some(total),
            rate_bps: (elapsed > 0.0).then(|| (written as f64 / elapsed) as u64),
        };
        self.store
            .call_mut(move |conn| update_job(conn, job, &progress))
            .await?;
        self.mark(
            request,
            revision,
            destination,
            State::Pulling,
            Some(fraction),
        )
        .await
    }

    /// The local repository and revision the pulled path belongs to, created on first pull.
    async fn local_revision(&self, request: &PullRequest) -> Result<hoard_core::RevisionId> {
        let repo = request.repo.clone();
        let revision = request.revision;
        let rows = self
            .store
            .call_mut(move |conn| {
                let node = local_node(conn)?;
                let qualified = repo.qualified();
                let repo_row = match repo_by_qualified(conn, &qualified, node.id)? {
                    Some(existing) => existing.id,
                    None => upsert_repo(conn, &NewRepo::new(repo, node.id, SourceKind::Loose))?,
                };
                Ok(match revision_by_sha(conn, repo_row, &revision)? {
                    Some(existing) => existing.id,
                    None => upsert_revision(conn, &NewRevision::new(repo_row, revision))?,
                })
            })
            .await?;
        Ok(rows)
    }

    /// Writes the entry this node shows for the file, with the state a reader should see.
    async fn mark(
        &self,
        request: &PullRequest,
        revision: hoard_core::RevisionId,
        destination: &Path,
        state: State,
        progress: Option<f32>,
    ) -> Result<()> {
        let path = request.path.clone();
        let size = request.size;
        let abs_path = destination.to_string_lossy().into_owned();
        self.store
            .call_mut(move |conn| {
                let node = local_node(conn)?;
                let mut entry = NewEntry::new(revision, path, node.id);
                entry.size = size;
                entry.state = state;
                entry.pull_progress = progress;
                entry.abs_path = Some(abs_path);
                entry.present = state != State::Remote;
                upsert_entry(conn, &entry, Timestamp::now())?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Interprets the verified bytes and records them as an artifact of this node.
    async fn file(
        &self,
        request: &PullRequest,
        revision: hoard_core::RevisionId,
        destination: PathBuf,
        sha256: Sha256,
    ) -> Result<PullOutcome> {
        let path = destination.clone();
        let facts = tokio::task::spawn_blocking(move || read_facts(&path))
            .await
            .map_err(|_| NetError::Config("reading the pulled file was cancelled".to_owned()))??;
        let subject = format!("{} \u{2190} {}", request.path, request.peer.name());
        let request = request.clone();
        let served = self.serve;
        let threshold = self.triage_confidence;
        let mut outcome = None;
        for attempt in 0..PATH_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(PATH_RETRY).await;
            }
            outcome = Some(
                self.write_entry(
                    &request,
                    revision,
                    &destination,
                    sha256,
                    &facts,
                    served,
                    threshold,
                )
                .await?,
            );
            if self.path_is_recorded(&destination).await? {
                break;
            }
        }
        let outcome =
            outcome.ok_or_else(|| NetError::Config("a pull recorded no entry".to_owned()))?;
        tracing::info!(subject, size = outcome.size, "pulled and verified");
        Ok(outcome)
    }

    /// Waits for the proxy path's background ingest of the same bytes.
    ///
    /// A completed cache entry is filed in the background, which is how any proxied
    /// download becomes searchable, and that write knows only the blocks. A pull is about
    /// the file it materialized, so it lets that write land before recording its own; the
    /// ingest is the only writer that sets an artifact on this entry.
    async fn settle_ingest(&self, revision: hoard_core::RevisionId, path: &str) -> Result<()> {
        for attempt in 0..PATH_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(PATH_RETRY).await;
            }
            let wanted = path.to_owned();
            let filed = self
                .store
                .call(move |conn| {
                    Ok(entries_for_revision(conn, revision)?
                        .into_iter()
                        .any(|row| row.path == wanted && row.artifact.is_some()))
                })
                .await?;
            if filed {
                return Ok(());
            }
        }
        tracing::debug!(path, "no background ingest landed for a pulled file");
        Ok(())
    }

    /// Whether the entry for these bytes still names the file this pull materialized.
    ///
    /// The proxy path ingests a completed cache entry in the background, which is how any
    /// proxied download becomes searchable, and that write knows only the blocks, not the
    /// file. A pull is about the file, so it confirms its own path is the one that stood.
    async fn path_is_recorded(&self, destination: &Path) -> Result<bool> {
        let wanted = destination.to_string_lossy().into_owned();
        let found = self
            .store
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT COUNT(*) FROM entry WHERE abs_path = ?1",
                        [wanted],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0))
            })
            .await?;
        Ok(found > 0)
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_entry(
        &self,
        request: &PullRequest,
        revision: hoard_core::RevisionId,
        destination: &Path,
        sha256: Sha256,
        facts: &PulledFacts,
        served: bool,
        threshold: f32,
    ) -> Result<PullOutcome> {
        let request = request.clone();
        let facts = facts.clone();
        let destination = destination.to_path_buf();
        let outcome = self
            .store
            .call_mut(move |conn| {
                let node = local_node(conn)?;
                let now = Timestamp::now();
                let blob = match blob_by_sha256(conn, &sha256)? {
                    Some(existing) => existing.id,
                    None => upsert_blob(conn, &sha256.to_hex(), facts.size, now)?,
                };
                set_sha256(conn, blob, sha256, facts.git_sha1, now)?;

                let triage = needs_triage(&facts.inference, threshold);
                let artifact = write_artifact(conn, blob, &request, &facts, triage, now)?;

                let mut entry = NewEntry::new(revision, request.path.clone(), node.id);
                entry.blob = Some(blob);
                entry.artifact = Some(artifact);
                entry.abs_path = Some(destination.to_string_lossy().into_owned());
                entry.size = facts.size;
                entry.state = derive_state(StateFacts {
                    held: true,
                    served,
                    triage,
                    ..StateFacts::default()
                });
                let entry = upsert_entry(conn, &entry, now)?;
                Ok(PullOutcome {
                    sha256,
                    size: facts.size,
                    abs_path: destination,
                    blob,
                    artifact,
                    entry,
                })
            })
            .await?;
        Ok(outcome)
    }
}

/// What one pulled file proves about itself, read once on a blocking thread.
#[derive(Debug, Clone)]
struct PulledFacts {
    size: u64,
    git_sha1: Option<CommitSha>,
    sniff: hoard_index::sniff::SniffResult,
    inference: hoard_index::infer::Inference,
}

fn read_facts(path: &Path) -> Result<PulledFacts> {
    let sniff = sniff_path(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let mut input = InferInput::new(sniff.format, path, &name, sniff.size);
    if let Some(tensors) = &sniff.tensors {
        input = input.with_tensors(tensors);
    }
    if let Some(config) = &sniff.config {
        input = input.with_config(config);
    }
    let frontmatter = sniff
        .readme
        .as_ref()
        .map(|readme| readme.frontmatter.clone());
    if let Some(frontmatter) = frontmatter.as_deref() {
        input = input.with_frontmatter(frontmatter);
    }
    let inference = infer(&input);
    let git_sha1 = git_blob_sha1(path).ok();
    Ok(PulledFacts {
        size: sniff.size,
        git_sha1,
        sniff,
        inference,
    })
}

fn write_artifact(
    conn: &<hoard_index::store::Reader as std::ops::Deref>::Target,
    blob: BlobId,
    request: &PullRequest,
    facts: &PulledFacts,
    triage: bool,
    now: Timestamp,
) -> hoard_index::Result<ArtifactId> {
    let inference = &facts.inference;
    let mut input = NewArtifact::new(blob, request.file_name());
    input.sniff_version = hoard_index::scan::SNIFF_VERSION;
    input.org = request.repo.org().map(str::to_owned);
    input.format = facts.sniff.format;
    input.role = inference.role;
    input.sub_role = inference.sub_role;
    input.role_confidence = inference.role_confidence;
    input.base = inference.base.clone();
    input.base_label = inference.base_label.clone();
    input.base_confidence = inference.base_confidence;
    input.precision = inference.precision.clone();
    input.quant = inference.quant.clone();
    input.rank = inference.rank;
    input.alpha = inference.alpha;
    input.params = inference.params;
    input.context = inference.context;
    input.trigger = inference.trigger.clone();

    let id = upsert_artifact(conn, &input, now)?;
    if let Some(tensors) = &facts.sniff.tensors {
        set_tensor_summary(conn, id, tensors)?;
    }
    replace_evidence(conn, id, &inference.evidence)?;
    if let Some(readme) = &facts.sniff.readme {
        set_card(conn, id, &readme.body)?;
    }
    set_triage(conn, id, triage)?;
    Ok(id)
}

/// The content hash a peer names in its headers, when it names one.
///
/// A disagreement here is caught before a byte moves; a peer whose headers and bytes
/// disagree is caught by the hash taken over the stream itself.
fn advertised_sha256(answer: &hoard_mirror::resolve::FileAnswer) -> Option<Sha256> {
    answer
        .linked_etag
        .as_deref()
        .or(Some(answer.etag.as_str()))
        .map(|etag| etag.trim_matches('"'))
        .and_then(|etag| etag.parse::<Sha256>().ok())
}

fn fraction(written: u64, total: u64) -> f32 {
    match total {
        0 => 0.0,
        total => (written as f64 / total as f64).clamp(0.0, 1.0) as f32,
    }
}

/// How the background gossip task is configured.
#[derive(Debug, Clone)]
pub struct GossipConfig {
    pub identity: NodeIdentity,
    pub discovery: DiscoveryMode,
    pub statics: Vec<String>,
    pub interval: Duration,
    pub digest_limit: u32,
    /// Sent as a bearer token to peers that require one even for reads.
    pub token: Option<String>,
}

impl GossipConfig {
    pub fn new(identity: NodeIdentity) -> Self {
        Self {
            identity,
            discovery: DiscoveryMode::default(),
            statics: Vec::new(),
            interval: DEFAULT_INTERVAL,
            digest_limit: DEFAULT_LIMIT,
            token: None,
        }
    }

    pub fn with_discovery(mut self, discovery: DiscoveryMode) -> Self {
        self.discovery = discovery;
        self
    }

    pub fn with_statics(mut self, statics: Vec<String>) -> Self {
        self.statics = statics;
        self
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }
}

/// The running advertisement, browse and gossip loop.
#[derive(Debug)]
pub struct GossipHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl GossipHandle {
    /// Asks the loop to finish. It withdraws the mDNS advertisement on its way out.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Stops the loop and waits for it, which is what a graceful shutdown wants.
    pub async fn shutdown(self) {
        self.stop();
        if let Err(error) = self.task.await {
            tracing::debug!(%error, "the gossip task ended abruptly");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// Starts discovery and gossip in the background.
///
/// Nothing in here can fail the server: a machine with no multicast route, no peers, or no
/// network at all logs and carries on serving.
pub fn spawn(store: Store, config: GossipConfig) -> GossipHandle {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let (discovery, mut arrivals) =
            Discovery::start(&config.identity, config.discovery, &config.statics);
        tracing::info!(
            node = %config.identity.name(),
            url = %config.identity.url(),
            mdns = discovery.is_advertising(),
            statics = config.statics.len(),
            "node discovery started"
        );
        let mut peers: HashMap<NodeId, Peer> = HashMap::new();
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() {
                        break;
                    }
                }
                event = arrivals.recv() => match event {
                    Some(event) => on_event(&store, &config, &mut peers, event).await,
                    None => break,
                },
                _ = ticker.tick() => poll_all(&store, &config, &mut peers).await,
            }
        }
        drop(discovery);
        tracing::info!("node discovery stopped");
    });
    GossipHandle { stop, task }
}

/// One peer as the loop tracks it.
#[derive(Debug)]
struct Peer {
    client: PeerClient,
    row: i64,
}

async fn on_event(
    store: &Store,
    config: &GossipConfig,
    peers: &mut HashMap<NodeId, Peer>,
    event: PeerEvent,
) {
    match event {
        PeerEvent::Arrived(identity) => {
            let id = identity.id().clone();
            match record(store, config, identity).await {
                Ok(peer) => {
                    let name = peer.client.name().to_owned();
                    peers.insert(id, peer);
                    tracing::info!(peer = %name, "peer found");
                }
                Err(error) => tracing::warn!(%error, "a discovered peer could not be recorded"),
            }
        }
        PeerEvent::Departed(id) => {
            peers.remove(&id);
            if let Err(error) = forget_peer(store, &id).await {
                tracing::warn!(%error, "a departed peer could not be marked offline");
            }
        }
    }
}

/// Records a discovered peer, under the name it answers to when it will say.
///
/// The identity a static address gives is the address itself; the peer's own `/api/v1/nodes`
/// is the only place its name lives, so it is asked once, on arrival. The id stays the one
/// discovery derived: it is the stable key, and two nodes started from one machine share the
/// machine id they would otherwise advertise.
async fn record(store: &Store, config: &GossipConfig, identity: NodeIdentity) -> Result<Peer> {
    let identity = identity.disambiguated_from(config.identity.id())?;
    let client = PeerClient::new(identity.clone(), config.token.clone())?;
    let identity = match client.advertised_name().await {
        Ok(Some(name)) => NodeIdentity::new(identity.id().clone(), name, identity.url().as_str())?,
        Ok(None) => identity,
        Err(error) => {
            tracing::debug!(%error, "a peer did not say what it is called");
            identity
        }
    };
    let row = record_peer(store, &identity).await?;
    Ok(Peer {
        client: PeerClient::new(identity, config.token.clone())?,
        row,
    })
}

async fn poll_all(store: &Store, config: &GossipConfig, peers: &mut HashMap<NodeId, Peer>) {
    for (id, peer) in peers.iter() {
        if let Err(error) = poll(store, config, peer).await {
            tracing::warn!(peer = %peer.client.name(), %error, "peer poll failed");
            if let Err(error) = forget_peer(store, id).await {
                tracing::warn!(%error, "an unreachable peer could not be marked offline");
            }
        }
    }
}

/// Reads everything new from one peer and files it, then remembers how far it got.
async fn poll(store: &Store, config: &GossipConfig, peer: &Peer) -> Result<()> {
    peer.client.health().await?;
    let indexed = peer.client.stats().await.map(|stats| stats.indexed_bytes)?;
    touch_peer(store, peer.row, indexed).await?;

    let mut cursor = digest::read_cursor(store, peer.row).await?;
    for _ in 0..PAGES_PER_POLL {
        let page = peer.client.digest(cursor, config.digest_limit).await?;
        if page.rows.is_empty() {
            break;
        }
        let applied = digest::apply(store, peer.client.name(), peer.row, &page).await?;
        tracing::debug!(
            peer = %peer.client.name(),
            entries = applied.entries,
            artifacts = applied.artifacts,
            "digest applied"
        );
        cursor = page
            .next_cursor
            .as_deref()
            .map(DigestCursor::parse)
            .transpose()?;
        digest::write_cursor(store, peer.row, cursor).await?;
        if cursor.is_none() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hoard_core::{NodeId, RepoType};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_mirror::access::{AccessPolicy, AccessRule};
    use hoard_mirror::cache::BlockCache;
    use hoard_mirror::config::{Geometry, MirrorConfig};
    use hoard_mirror::upstream::Hub;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const SAFETENSORS: &[u8] = b"";

    fn identity(url: &str) -> NodeIdentity {
        NodeIdentity::new(NodeId::new("laptop-1").expect("id"), "laptop", url)
            .expect("identity builds")
    }

    fn commit() -> CommitSha {
        "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3"
            .parse()
            .expect("sha parses")
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("kohya".to_owned()), "film-grain")
    }

    /// A minimal safetensors file: an eight byte header length, then the JSON header.
    fn safetensors(tensor: &str) -> Vec<u8> {
        let header = format!(
            "{{\"{tensor}\":{{\"dtype\":\"F16\",\"shape\":[4,4],\"data_offsets\":[0,32]}}}}"
        );
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    }

    fn sha256_of(bytes: &[u8]) -> Sha256 {
        Sha256::from_bytes(Sha256Hasher::digest(bytes).into())
    }

    struct World {
        _dir: TempDir,
        store: Store,
        puller: Puller,
    }

    async fn world(root: &TempDir) -> World {
        let store = Store::open_in_memory().expect("store opens");
        store
            .call_mut(|conn| {
                upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                Ok(())
            })
            .await
            .expect("local node records");
        let config = Arc::new(MirrorConfig {
            geometry: Geometry::new(1 << 20, 1 << 16).expect("valid geometry"),
            proxy: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
            cache: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
            ..MirrorConfig::default()
        });
        let cache = Arc::new(BlockCache::at_root(
            Arc::clone(&config),
            root.path().join("repos"),
        ));
        let access = Arc::new(AccessPolicy::compile(&config).expect("rules compile"));
        let upstream = Hub::new(
            UpstreamClient::new(true).expect("client builds"),
            "https://huggingface.co",
        )
        .expect("hub builds");
        let mirror = Arc::new(Mirror::new(store.clone(), cache, access, upstream));
        let puller = Puller::new(
            store.clone(),
            mirror,
            root.path().join(PULL_DIR),
            true,
            0.85,
        )
        .expect("puller builds");
        World {
            _dir: TempDir::new().expect("temp dir"),
            store,
            puller,
        }
    }

    /// The bytes a mismatching pull is refused over never reach the index; what the proxy
    /// path files from the cache is what the peer really served, under its own hash.
    ///
    /// A peer serving one file over the Hub routes a pull uses.
    async fn peer_serving(bytes: Vec<u8>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/kohya/film-grain"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "kohya/film-grain",
                "sha": commit().to_hex(),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/models/kohya/film-grain/revision/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "kohya/film-grain",
                "sha": commit().to_hex(),
            })))
            .mount(&server)
            .await;
        let length = bytes.len();
        Mock::given(method("HEAD"))
            .and(path_regex(r"^/kohya/film-grain/resolve/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", length.to_string().as_str())
                    .insert_header("x-repo-commit", commit().to_hex().as_str())
                    .insert_header(
                        "etag",
                        format!("\"{}\"", sha256_of(&bytes).to_hex()).as_str(),
                    )
                    .insert_header("accept-ranges", "bytes"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/kohya/film-grain/resolve/.*"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header(
                        "content-range",
                        format!("bytes 0-{}/{length}", length - 1).as_str(),
                    )
                    .insert_header("x-repo-commit", commit().to_hex().as_str())
                    .set_body_bytes(bytes),
            )
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn every_state_derives_from_the_same_facts() {
        assert_eq!(
            derive_state(StateFacts {
                held: true,
                served: true,
                ..StateFacts::default()
            }),
            State::Served
        );
        assert_eq!(
            derive_state(StateFacts {
                held: true,
                ..StateFacts::default()
            }),
            State::Local
        );
        assert_eq!(derive_state(StateFacts::default()), State::Remote);
        assert_eq!(
            derive_state(StateFacts {
                pulling: true,
                ..StateFacts::default()
            }),
            State::Pulling
        );
        assert_eq!(
            derive_state(StateFacts {
                held: true,
                served: true,
                pulling: true,
                triage: true,
                stale: true
            }),
            State::Pulling
        );
        assert_eq!(
            derive_state(StateFacts {
                held: true,
                served: true,
                stale: true,
                triage: true,
                ..StateFacts::default()
            }),
            State::Stale
        );
        assert_eq!(
            derive_state(StateFacts {
                held: true,
                served: true,
                triage: true,
                ..StateFacts::default()
            }),
            State::Triage
        );
        assert_eq!(
            derive_state(StateFacts {
                served: true,
                triage: true,
                stale: true,
                ..StateFacts::default()
            }),
            State::Remote
        );
    }

    #[test]
    fn a_pin_is_stale_only_when_both_shas_are_known_and_differ() {
        let other = CommitSha::from_bytes([7u8; 20]);
        assert!(pin_is_stale(Some(commit()), Some(other)));
        assert!(!pin_is_stale(Some(commit()), Some(commit())));
        assert!(!pin_is_stale(Some(commit()), None));
        assert!(!pin_is_stale(None, Some(commit())));
    }

    #[test]
    fn a_pull_lands_under_the_repository_and_revision_it_came_from() {
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open_in_memory().expect("store opens");
        let config = Arc::new(MirrorConfig::default());
        let cache = Arc::new(BlockCache::at_root(Arc::clone(&config), dir.path()));
        let access = Arc::new(AccessPolicy::default());
        let upstream = Hub::new(
            UpstreamClient::new(true).expect("client builds"),
            "https://huggingface.co",
        )
        .expect("hub builds");
        let mirror = Arc::new(Mirror::new(store, cache, access, upstream));
        let puller = Puller::new(mirror.store().clone(), mirror, "/srv/pulled", true, 0.85)
            .expect("puller builds");
        let request = PullRequest::new(
            identity("http://laptop:7099"),
            repo(),
            commit(),
            "loras/film-grain.safetensors",
            sha256_of(SAFETENSORS),
            0,
        );
        assert_eq!(
            puller.destination(&request),
            PathBuf::from(format!(
                "/srv/pulled/models/kohya/film-grain/{}/loras/film-grain.safetensors",
                commit().short()
            ))
        );
        assert_eq!(request.file_name(), "film-grain.safetensors");
    }

    #[tokio::test]
    async fn a_verified_pull_becomes_a_local_searchable_artifact() {
        let root = TempDir::new().expect("temp dir");
        let world = world(&root).await;
        let bytes = safetensors("lora_unet_down_blocks.lora_down.weight");
        let server = peer_serving(bytes.clone()).await;
        let request = PullRequest::new(
            identity(&server.uri()),
            repo(),
            commit(),
            "loras/film-grain.safetensors",
            sha256_of(&bytes),
            bytes.len() as u64,
        );

        let outcome = world
            .puller
            .pull(&request, None)
            .await
            .expect("the pull verifies");
        assert_eq!(outcome.sha256, sha256_of(&bytes));
        assert_eq!(outcome.size, bytes.len() as u64);
        assert!(outcome.abs_path.exists());

        let rows = world
            .store
            .call(|conn| {
                hoard_index::store::search_exec::query_rows(
                    conn,
                    "state = ?",
                    &["served".into()],
                    "",
                    10,
                    0,
                )
            })
            .await
            .expect("rows read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].blob_sha256, Some(sha256_of(&bytes)));
        assert_eq!(rows[0].state, State::Served);
        assert_eq!(rows[0].abs_path.as_deref(), outcome.abs_path.to_str());
    }

    #[tokio::test]
    async fn a_pull_whose_bytes_do_not_hash_is_refused_and_indexes_nothing() {
        let root = TempDir::new().expect("temp dir");
        let world = world(&root).await;
        let bytes = safetensors("lora_unet_down_blocks.lora_down.weight");
        let server = peer_serving(bytes.clone()).await;
        let request = PullRequest::new(
            identity(&server.uri()),
            repo(),
            commit(),
            "loras/film-grain.safetensors",
            sha256_of(b"different bytes entirely"),
            bytes.len() as u64,
        );

        let error = world
            .puller
            .pull(&request, None)
            .await
            .expect_err("a mismatch is refused");
        assert!(matches!(error, NetError::HashMismatch { .. }));
        assert!(!world.puller.destination(&request).exists());

        let requested = sha256_of(b"different bytes entirely");
        let claimed = world
            .store
            .call(move |conn| Ok(blob_by_sha256(conn, &requested)?.is_some()))
            .await
            .expect("blob lookup");
        assert!(
            !claimed,
            "the bytes hoard asked for never entered the index"
        );

        let jobs = world
            .store
            .call(|conn| hoard_index::store::job::list_jobs(conn, None))
            .await
            .expect("jobs read");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, PULL_KIND);
        assert!(jobs[0].error.is_some());
    }

    #[tokio::test]
    async fn a_peer_client_reads_health_stats_and_the_digest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "status": "ok", "index": "ok", "upstream": "ok", "peers": 2 }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "artifacts": 3, "indexed_bytes": 4096 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .and(query_param("since", "d1-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [{
                    "sha256": sha256_of(SAFETENSORS).to_hex(),
                    "size": 12,
                    "role": "lora",
                    "base": "sdxl",
                    "repo": "models/kohya/film-grain",
                    "revision_sha": commit().to_hex(),
                    "path": "loras/a.safetensors",
                    "state": "served"
                }],
                "next_cursor": null,
                "total": 1
            })))
            .mount(&server)
            .await;

        let client = PeerClient::new(identity(&server.uri()), None).expect("client builds");
        assert_eq!(client.health().await.expect("health reads").peers, 2);
        assert_eq!(
            client.stats().await.expect("stats read").indexed_bytes,
            4096
        );
        let page = client
            .digest(Some(DigestCursor::new(5)), 10)
            .await
            .expect("digest reads");
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].path, "loras/a.safetensors");
    }

    #[tokio::test]
    async fn a_peer_that_answers_with_nonsense_is_a_named_bad_digest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"rows\":[{\"sha256\":1}]}"))
            .mount(&server)
            .await;
        let client = PeerClient::new(identity(&server.uri()), None).expect("client builds");
        let error = client.digest(None, 10).await.expect_err("nonsense refused");
        assert!(matches!(error, NetError::BadDigest { peer, .. } if peer == "laptop"));
    }

    #[tokio::test]
    async fn gossip_starts_and_stops_without_a_peer_in_sight() {
        let store = Store::open_in_memory().expect("store opens");
        store
            .call_mut(|conn| {
                upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                Ok(())
            })
            .await
            .expect("local node records");
        let config = GossipConfig::new(identity("http://127.0.0.1:7099"))
            .with_discovery(DiscoveryMode::Static)
            .with_interval(Duration::from_millis(20));
        let handle = spawn(store, config);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!handle.is_finished());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn gossip_files_a_static_peers_digest_as_remote_entries() {
        let bytes = safetensors("lora_unet_down_blocks.lora_down.weight");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "status": "ok", "peers": 0 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "artifacts": 1, "indexed_bytes": 512 })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/nodes"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{ "id": "alpha-1", "name": "alpha", "self": true }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [{
                    "sha256": sha256_of(&bytes).to_hex(),
                    "size": bytes.len(),
                    "role": "lora",
                    "base": "sdxl",
                    "repo": "models/kohya/film-grain",
                    "revision_sha": commit().to_hex(),
                    "path": "loras/film-grain.safetensors",
                    "state": "served"
                }],
                "next_cursor": null,
                "total": 1
            })))
            .mount(&server)
            .await;

        let store = Store::open_in_memory().expect("store opens");
        store
            .call_mut(|conn| {
                upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                Ok(())
            })
            .await
            .expect("local node records");
        let config = GossipConfig::new(identity("http://127.0.0.1:7099"))
            .with_discovery(DiscoveryMode::Static)
            .with_statics(vec![server.uri()])
            .with_interval(Duration::from_millis(20));
        let handle = spawn(store.clone(), config);

        let mut remote = 0;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            remote = store
                .call(|conn| {
                    hoard_index::store::search_exec::count_rows(
                        conn,
                        "state = ?",
                        &["remote".into()],
                    )
                })
                .await
                .expect("count reads");
            if remote > 0 {
                break;
            }
        }
        let peers = store
            .call(hoard_index::store::node::list_nodes)
            .await
            .expect("nodes read");
        handle.shutdown().await;
        assert_eq!(remote, 1);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[1].name, "alpha", "a peer is named as it names itself");
        assert_eq!(peers[1].indexed_bytes, 512);
        assert!(peers[1].online);
    }
}
