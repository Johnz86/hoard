//! Talking to another node: reading its digest, deriving states, and pulling its bytes.
//!
//! A peer speaks the same Hub face this node serves, so a pull is not a second transfer
//! protocol: it is an ordinary block-cached resolve against the peer's base URL, which
//! brings resumption and single-flight block downloads with it. What this module adds is
//! the verification at the end, the job row the PULLING state reads, and the index write
//! that turns the fetched bytes into a local, searchable artifact.

use std::collections::HashMap;
use std::future::Future;
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

use hoard_core::{
    ArtifactId, BlobId, CommitSha, EntryId, JobId, NodeId, RepoId, RevisionId, Sha256, State,
};
use hoard_index::config::SourceKind;
use hoard_index::hash::git_blob_sha1;
use hoard_index::infer::{InferInput, infer};
use hoard_index::sniff::sniff_path;
use hoard_index::store::Store;
use hoard_index::store::artifact::{
    NewArtifact, replace_evidence, set_card, set_tensor_summary, set_triage, upsert_artifact,
};
use hoard_index::store::blob::{blob_by_sha256, set_sha256, upsert_blob};
use hoard_index::store::entry::{NewEntry, upsert_entry};
use hoard_index::store::job::{JobProgress, create_job, fail_job, finish_job, update_job};
use hoard_index::store::node::{local_node, node};
use hoard_index::store::repo::{NewRepo, repo_by_qualified, upsert_repo};
use hoard_index::store::revision::{NewRevision, revision_by_sha, upsert_revision};
use hoard_index::store::search_exec::count_rows;
use hoard_index::triage::needs_triage;
use hoard_mirror::resolve::{FileRequest, Mirror, read_range};
use hoard_mirror::upstream::UpstreamClient;
use hoard_mirror::upstream::peer::peer_upstream;

use crate::digest::{self, DEFAULT_LIMIT, DigestCursor, DigestPage, MAX_LIMIT};
use crate::discovery::{Discovery, DiscoveryMode, PeerEvent, forget_peer, record_peer, touch_peer};
use crate::error::{NetError, Result};
use crate::identity::{NodeIdentity, clean_name};

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

/// The most bytes one answer from a peer may carry. A digest page is bounded by
/// [`MAX_LIMIT`] rows of bounded strings; anything larger is a peer answering with a body
/// that never ends.
pub const MAX_ANSWER_BYTES: u64 = 4 * 1024 * 1024;

/// How many peers one node tracks. Anything on the LAN can advertise `_hoard._tcp`, and
/// every peer costs a node row and a namespace of its own.
pub const MAX_PEERS: usize = 64;

/// How many peers are polled at once, so one unreachable peer does not hold up the rest.
const POLL_CONCURRENCY: usize = 8;

/// How many times the interval doubles for a peer that keeps failing.
const MAX_BACKOFF_STEPS: u32 = 5;

/// The suffix a partly transferred file carries until it verifies.
const PARTIAL_SUFFIX: &str = "hoard-partial";

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
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(rename = "self", default)]
    is_self: bool,
}

/// What a peer answers about itself, which is the only authority on its identity.
///
/// Discovery keys a peer by where it answers or by what it advertised; the node it turns
/// out to be is what its own index calls it, so one machine found twice is one peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerSelf {
    pub id: Option<NodeId>,
    pub name: Option<String>,
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

    /// The id and name the peer's own index carries for it, absent when it will not say.
    pub async fn reported_self(&self) -> Result<PeerSelf> {
        let rows: Vec<PeerNodeRow> = self.read_json("api/v1/nodes").await?;
        let Some(row) = rows.into_iter().find(|row| row.is_self) else {
            return Ok(PeerSelf::default());
        };
        Ok(PeerSelf {
            id: row.id.as_deref().and_then(|id| NodeId::new(id).ok()),
            name: clean_name(&row.name),
        })
    }

    /// One page of the peer's digest, starting after `since`.
    ///
    /// A page carrying more rows than were asked for is refused: the limit is what bounds
    /// one `digest::apply` transaction.
    pub async fn digest(&self, since: Option<DigestCursor>, limit: u32) -> Result<DigestPage> {
        let limit = limit.clamp(1, MAX_LIMIT);
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
        let body = self.read_bounded(self.get(url).await?).await?;
        let page: DigestPage =
            serde_json::from_slice(&body).map_err(|error| self.bad_digest(error.to_string()))?;
        if page.rows.len() > limit as usize {
            return Err(self.bad_digest(format!(
                "a page carried {} rows where {limit} were asked for",
                page.rows.len()
            )));
        }
        Ok(page)
    }

    async fn read_json<T: serde::de::DeserializeOwned>(&self, suffix: &str) -> Result<T> {
        let url = self
            .identity
            .url()
            .join(suffix)
            .map_err(|error| NetError::Config(error.to_string()))?;
        let body = self.read_bounded(self.get(url).await?).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Reads an answer into memory with a ceiling on it, since a peer is not trusted to
    /// stop sending.
    async fn read_bounded(&self, mut response: reqwest::Response) -> Result<Vec<u8>> {
        if let Some(length) = response.content_length()
            && length > MAX_ANSWER_BYTES
        {
            return Err(self.too_large(length));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() as u64 + chunk.len() as u64 > MAX_ANSWER_BYTES {
                return Err(self.too_large(body.len() as u64 + chunk.len() as u64));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn too_large(&self, seen: u64) -> NetError {
        self.bad_digest(format!(
            "an answer of at least {seen} bytes passes the {MAX_ANSWER_BYTES} byte ceiling"
        ))
    }

    fn bad_digest(&self, reason: String) -> NetError {
        NetError::BadDigest {
            peer: self.name().to_owned(),
            reason,
        }
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

/// The state a pull and the gossip writer record, ordered by urgency rather than by the
/// order the design lists them: a running transfer is what a reader wants to see first,
/// bytes that are not here at all cannot be anything but REMOTE, a pin that has moved is
/// actionable, and TRIAGE outranks SERVED because unclassified bytes should not read as
/// ready to use.
///
/// It is not the only writer of a state. `hoard_index::scan`, `hoard_mirror::ingest` and
/// `hoard_index::store::digest` each write one directly and all three sit below this crate,
/// so a scanned file reads LOCAL on the same node where a pulled one reads SERVED, and
/// [`StateFacts::stale`] is decided nowhere at all.
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

    /// Where a pulled file lands: readable, stable, unique per revision, and always inside
    /// the pull root whatever a peer named its repository or its path.
    pub fn destination(&self, request: &PullRequest) -> PathBuf {
        let mut path = self.root.clone();
        for segment in request.repo.qualified().split('/').filter(is_plain) {
            path.push(segment);
        }
        path.push(request.revision.short());
        for segment in request.path.split('/').filter(is_plain) {
            path.push(segment);
        }
        path
    }

    /// Pulls one file, verifies it and files it in the index.
    ///
    /// A job row is created if one is not supplied, so a pull always shows up in the JOBS
    /// tab. The entry shows PULLING with its percentage while bytes are moving.
    pub async fn pull(&self, request: &PullRequest, job: Option<JobId>) -> Result<PullOutcome> {
        let subject = subject_of(request);
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
        let transfer = Transfer {
            request,
            revision,
            destination: &destination,
            job,
        };
        self.mark(transfer, State::Pulling, Some(0.0)).await?;
        match self.fetch(transfer).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.abandon(transfer).await;
                Err(error)
            }
        }
    }

    /// Fetches one file from the peer, verifies it and files it.
    ///
    /// The hash the digest promised is handed to the proxy path as well as checked here, so
    /// the background ingest of the same bytes refuses them too rather than filing whatever
    /// the peer chose to send under this repository and path. That ingest is awaited rather
    /// than left to land later, since it writes the same entry this pull does.
    async fn fetch(&self, transfer: Transfer<'_>) -> Result<PullOutcome> {
        let request = transfer.request;
        let hub = peer_upstream(self.client.clone(), request.peer.url().as_str())
            .map_err(NetError::Mirror)?;
        let mirror = self.mirror.with_upstream(hub);
        let answer = mirror
            .open_file(
                FileRequest::new(&request.repo, &request.revision.to_hex(), &request.path)
                    .with_expected(Some(request.sha256)),
            )
            .await
            .map_err(NetError::Mirror)?;
        let advertised = advertised_sha256(&answer).ok_or_else(|| NetError::Unverifiable {
            peer: request.peer.name().to_owned(),
            path: request.path.clone(),
        })?;
        if advertised != request.sha256 {
            return Err(mismatch(request, advertised));
        }

        let total = match answer.size {
            0 => request.size,
            size => size,
        };
        let stream = read_range(&answer, None).await.map_err(NetError::Mirror)?;
        let sha256 = self.stream_to_disk(transfer, stream, total).await?;
        if sha256 != request.sha256 {
            return Err(mismatch(request, sha256));
        }
        if let Some(ingest) = answer.ingest()
            && let Err(error) = ingest.run().await
        {
            tracing::warn!(%error, "the cached copy of a pulled file was not ingested");
        }
        self.file(transfer, sha256).await
    }

    /// Gives up on a transfer: nothing half-written is left behind and the entry reads
    /// REMOTE again, so a peer that closes mid-body cannot leave a percentage that never
    /// moves.
    async fn abandon(&self, transfer: Transfer<'_>) {
        let destination = transfer.destination;
        for path in [destination.to_path_buf(), partial_path(destination)] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "unverified bytes remain");
                }
            }
        }
        if let Err(error) = self.mark(transfer, State::Remote, None).await {
            tracing::warn!(%error, "an abandoned pull could not be marked remote");
        }
    }

    /// Streams the answer into place, hashing as it goes so the verification costs one pass.
    async fn stream_to_disk(
        &self,
        transfer: Transfer<'_>,
        mut stream: hoard_mirror::resolve::ByteStream,
        total: u64,
    ) -> Result<Sha256> {
        let destination = transfer.destination;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = partial_path(destination);
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
                self.report(transfer, Moved { written, total }, started)
                    .await?;
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, destination).await?;
        Ok(Sha256::from_bytes(hasher.finalize().into()))
    }

    async fn report(&self, transfer: Transfer<'_>, moved: Moved, started: Instant) -> Result<()> {
        let elapsed = started.elapsed().as_secs_f64();
        let written = moved.written;
        let fraction = moved.fraction();
        let progress = JobProgress {
            phase: Some(format!(
                "pulling {} from {}",
                transfer.request.path,
                transfer.request.peer.name()
            )),
            progress: fraction,
            bytes: written,
            total: Some(moved.total),
            rate_bps: (elapsed > 0.0).then(|| (written as f64 / elapsed) as u64),
        };
        let job = transfer.job;
        self.store
            .call_mut(move |conn| update_job(conn, job, &progress))
            .await?;
        self.mark(transfer, State::Pulling, Some(fraction)).await
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
        transfer: Transfer<'_>,
        state: State,
        progress: Option<f32>,
    ) -> Result<()> {
        let path = transfer.request.path.clone();
        let size = transfer.request.size;
        let revision = transfer.revision;
        let abs_path = transfer.destination.to_string_lossy().into_owned();
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
    async fn file(&self, transfer: Transfer<'_>, sha256: Sha256) -> Result<PullOutcome> {
        let path = transfer.destination.to_path_buf();
        let facts = tokio::task::spawn_blocking(move || read_facts(&path))
            .await
            .map_err(|_| NetError::Config("reading the pulled file was cancelled".to_owned()))??;
        let outcome = self.write_entry(transfer, sha256, facts).await?;
        tracing::info!(
            subject = subject_of(transfer.request),
            size = outcome.size,
            "pulled and verified"
        );
        Ok(outcome)
    }

    async fn write_entry(
        &self,
        transfer: Transfer<'_>,
        sha256: Sha256,
        facts: PulledFacts,
    ) -> Result<PullOutcome> {
        let request = transfer.request.clone();
        let revision = transfer.revision;
        let destination = transfer.destination.to_path_buf();
        let served = self.serve;
        let threshold = self.triage_confidence;
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

/// One running transfer: what is being fetched, where it lands, and what reports it.
#[derive(Debug, Clone, Copy)]
struct Transfer<'a> {
    request: &'a PullRequest,
    revision: RevisionId,
    destination: &'a Path,
    job: JobId,
}

/// How much of a transfer has moved.
#[derive(Debug, Clone, Copy)]
struct Moved {
    written: u64,
    total: u64,
}

impl Moved {
    fn fraction(self) -> f32 {
        match self.total {
            0 => 0.0,
            total => (self.written as f64 / total as f64).clamp(0.0, 1.0) as f32,
        }
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

/// Whether a path segment names a file rather than a step out of the directory above it.
fn is_plain(segment: &&str) -> bool {
    !segment.is_empty() && *segment != "." && *segment != ".."
}

/// The file a transfer writes into until its bytes verify.
fn partial_path(destination: &Path) -> PathBuf {
    let mut partial = destination.as_os_str().to_owned();
    partial.push(".");
    partial.push(PARTIAL_SUFFIX);
    PathBuf::from(partial)
}

/// What the JOBS tab calls one pull.
fn subject_of(request: &PullRequest) -> String {
    format!("{} from {}", request.path, request.peer.name())
}

fn mismatch(request: &PullRequest, actual: Sha256) -> NetError {
    NetError::HashMismatch {
        peer: request.peer.name().to_owned(),
        path: request.path.clone(),
        expected: request.sha256.to_hex(),
        actual: actual.to_hex(),
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
        let mut peers = PeerSet::default();
        let mut cancel = stopped.clone();
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let stop_now = tokio::select! {
                changed = stopped.changed() => changed.is_err() || *stopped.borrow(),
                event = arrivals.recv() => match event {
                    Some(event) => {
                        unless_stopped(&mut cancel, on_event(&store, &config, &mut peers, event))
                            .await
                    }
                    None => true,
                },
                _ = ticker.tick() => {
                    unless_stopped(&mut cancel, peers.poll_all(&store, &config)).await
                }
            };
            if stop_now {
                break;
            }
        }
        drop(discovery);
        tracing::info!("node discovery stopped");
    });
    GossipHandle { stop, task }
}

/// Runs `work` unless the loop is asked to stop first, and says whether it was.
///
/// Everything gossip does crosses the network, so shutdown has to be able to interrupt it
/// rather than wait out a peer that is not answering.
async fn unless_stopped<F: Future<Output = ()>>(
    stopped: &mut watch::Receiver<bool>,
    work: F,
) -> bool {
    tokio::select! {
        changed = stopped.changed() => changed.is_err() || *stopped.borrow(),
        () = work => false,
    }
}

/// One peer as the loop tracks it.
#[derive(Debug)]
struct Peer {
    client: PeerClient,
    row: i64,
    /// The id discovery knew it by, which is not always the id it answers to.
    discovered: NodeId,
    /// Consecutive failed polls, which is how long the next one is put off for.
    failures: u32,
    due: Instant,
}

impl Peer {
    fn new(client: PeerClient, row: i64, discovered: NodeId) -> Self {
        Self {
            client,
            row,
            discovered,
            failures: 0,
            due: Instant::now(),
        }
    }

    fn id(&self) -> &NodeId {
        self.client.identity().id()
    }

    fn is_due(&self, now: Instant) -> bool {
        now >= self.due
    }

    fn answered(&mut self) {
        self.failures = 0;
    }

    /// Puts the next poll off by one doubling of the interval per consecutive failure, so a
    /// peer answering with a digest this node cannot read is not asked again every tick.
    fn failed(&mut self, interval: Duration, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let steps = self.failures.min(MAX_BACKOFF_STEPS);
        self.due = now + interval.saturating_mul(1u32 << steps);
    }
}

/// Every peer this node tracks, bounded so the LAN cannot fill this node's memory.
#[derive(Debug, Default)]
struct PeerSet {
    peers: HashMap<NodeId, Peer>,
    refused: bool,
}

impl PeerSet {
    fn contains(&self, id: &NodeId) -> bool {
        self.peers.contains_key(id)
    }

    fn is_full(&self) -> bool {
        self.peers.len() >= MAX_PEERS
    }

    /// Records a peer, or says the set is full. The refusal is logged once per filling, not
    /// once per advertisement.
    fn insert(&mut self, peer: Peer) -> bool {
        let id = peer.id().clone();
        if !self.contains(&id) && self.is_full() {
            if !self.refused {
                self.refused = true;
                tracing::warn!(
                    limit = MAX_PEERS,
                    "the peer limit is reached; a peer was ignored"
                );
            }
            return false;
        }
        self.peers.insert(id, peer);
        true
    }

    /// Forgets whatever a departure names, by the id the peer answers to or the one
    /// discovery knew it by, and returns the node rows to mark offline.
    fn remove(&mut self, id: &NodeId) -> Vec<NodeId> {
        let gone: Vec<NodeId> = self
            .peers
            .values()
            .filter(|peer| peer.id() == id || &peer.discovered == id)
            .map(|peer| peer.id().clone())
            .collect();
        for id in &gone {
            self.peers.remove(id);
        }
        self.refused = false;
        gone
    }

    /// Polls every peer that is due, several at a time, so one unreachable peer costs the
    /// others nothing but a slot.
    async fn poll_all(&mut self, store: &Store, config: &GossipConfig) {
        let now = Instant::now();
        let interval = config.interval;
        futures::stream::iter(self.peers.values_mut().filter(|peer| peer.is_due(now)))
            .for_each_concurrent(POLL_CONCURRENCY, |peer| async move {
                match poll(store, config, peer).await {
                    Ok(()) => peer.answered(),
                    Err(error) => {
                        tracing::warn!(peer = %peer.client.name(), %error, "peer poll failed");
                        peer.failed(interval, Instant::now());
                        if let Err(error) = forget_peer(store, peer.id()).await {
                            tracing::warn!(%error, "an unreachable peer could not be marked offline");
                        }
                    }
                }
            })
            .await;
    }
}

async fn on_event(store: &Store, config: &GossipConfig, peers: &mut PeerSet, event: PeerEvent) {
    match event {
        PeerEvent::Arrived(identity) => {
            if !peers.contains(identity.id()) && peers.is_full() {
                return;
            }
            match record(store, config, identity).await {
                Ok(Some(peer)) => {
                    let name = peer.client.name().to_owned();
                    if peers.insert(peer) {
                        tracing::info!(peer = %name, "peer found");
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "a discovered peer could not be recorded"),
            }
        }
        PeerEvent::Departed(id) => {
            for gone in peers.remove(&id) {
                if let Err(error) = forget_peer(store, &gone).await {
                    tracing::warn!(%error, "a departed peer could not be marked offline");
                }
            }
        }
    }
}

/// Records a discovered peer under the identity it answers to.
///
/// A static address and an mDNS advertisement of one machine are two discoveries of one
/// node, so the id the peer's own index reports is what it is filed under; without that,
/// the same peer would own two node rows and two copies of everything it holds. A node that
/// answers as this one is not a peer at all and is dropped.
async fn record(
    store: &Store,
    config: &GossipConfig,
    discovered: NodeIdentity,
) -> Result<Option<Peer>> {
    let client = PeerClient::new(discovered.clone(), config.token.clone())?;
    let reported = match client.reported_self().await {
        Ok(reported) => reported,
        Err(error) => {
            tracing::debug!(%error, "a peer did not say what it is");
            PeerSelf::default()
        }
    };
    let identity = NodeIdentity::new(
        reported.id.unwrap_or_else(|| discovered.id().clone()),
        reported
            .name
            .unwrap_or_else(|| discovered.name().to_owned()),
        discovered.url().as_str(),
    )?;
    if answers_as_this_node(&config.identity, &identity) {
        tracing::debug!(peer = %identity.url(), "a discovered peer is this node");
        return Ok(None);
    }
    let identity = identity.disambiguated_from(config.identity.id())?;
    let row = record_peer(store, &identity).await?;
    let client = PeerClient::new(identity, config.token.clone())?;
    Ok(Some(Peer::new(client, row, discovered.id().clone())))
}

/// Whether the node that answered is this one.
///
/// Two nodes started from one machine share the machine's id, so the id alone does not
/// settle it; they cannot also share the port they serve on.
fn answers_as_this_node(local: &NodeIdentity, peer: &NodeIdentity) -> bool {
    peer.id() == local.id() && peer.port() == local.port()
}

/// Reads everything new from one peer and files it, then remembers how far it got.
async fn poll(store: &Store, config: &GossipConfig, peer: &Peer) -> Result<()> {
    peer.client.health().await?;
    let indexed = peer.client.stats().await.map(|stats| stats.indexed_bytes)?;
    touch_peer(store, peer.row, indexed).await?;

    let mut cursor = digest::read_cursor(store, peer.row).await?;
    let mut rewound = false;
    for _ in 0..PAGES_PER_POLL {
        let page = peer.client.digest(cursor, config.digest_limit).await?;
        if page.rows.is_empty() {
            if rewound || !rewind(store, peer, &page, cursor).await? {
                break;
            }
            rewound = true;
            cursor = None;
            continue;
        }
        let applied = digest::apply(store, peer.client.name(), peer.row, &page).await?;
        tracing::debug!(
            peer = %peer.client.name(),
            entries = applied.entries,
            artifacts = applied.artifacts,
            "digest applied"
        );
        let Some(next) = page
            .next_cursor
            .as_deref()
            .map(DigestCursor::parse)
            .transpose()?
        else {
            break;
        };
        if cursor.is_some_and(|current| next <= current) {
            tracing::warn!(peer = %peer.client.name(), "a digest cursor did not advance");
            break;
        }
        cursor = Some(next);
        digest::write_cursor(store, peer.row, cursor).await?;
    }
    Ok(())
}

/// Whether a peer's digest has to be read again from the start, which one poll does once.
///
/// The cursor is a position in the peer's own artifact rows. A peer that rebuilt its index
/// numbers them from one again, so everything it holds falls below the stored cursor and it
/// would answer with an empty page for good; the row count it reports is what says so.
async fn rewind(
    store: &Store,
    peer: &Peer,
    page: &DigestPage,
    cursor: Option<DigestCursor>,
) -> Result<bool> {
    if cursor.is_none() || page.total <= filed_for(store, peer.row).await? {
        return Ok(false);
    }
    tracing::info!(
        peer = %peer.client.name(),
        total = page.total,
        "a peer's digest restarted; reading it again from the beginning"
    );
    digest::write_cursor(store, peer.row, None).await?;
    Ok(true)
}

/// How many artifacts this node has filed for one peer.
async fn filed_for(store: &Store, row: i64) -> Result<u64> {
    let filed = store
        .call(move |conn| {
            let name = node(conn, row)?.name;
            count_rows(conn, "node = ?", &[name.into()])
        })
        .await?;
    Ok(filed)
}

#[cfg(test)]
mod tests {
    use hoard_core::{NodeId, RepoType};
    use hoard_index::store::entry::{EntryRow, entries_for_revision};
    use hoard_index::store::node::{NewNode, list_nodes, upsert_node};
    use hoard_mirror::access::{AccessPolicy, AccessRule};
    use hoard_mirror::cache::BlockCache;
    use hoard_mirror::config::{Geometry, MirrorConfig};
    use hoard_mirror::upstream::Hub;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, path_regex, query_param, query_param_is_missing};
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
        store: Store,
        cache: Arc<BlockCache>,
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
        let mirror = Arc::new(Mirror::new(
            store.clone(),
            Arc::clone(&cache),
            access,
            upstream,
        ));
        let puller = Puller::new(
            store.clone(),
            mirror,
            root.path().join(PULL_DIR),
            true,
            0.85,
        )
        .expect("puller builds");
        World {
            store,
            cache,
            puller,
        }
    }

    /// The entry this node shows for one pulled path, whatever state it is in.
    async fn pulled_entry(store: &Store, path: &str) -> Option<EntryRow> {
        let wanted = path.to_owned();
        store
            .call(move |conn| {
                let node = local_node(conn)?;
                let Some(row) = repo_by_qualified(conn, &repo().qualified(), node.id)? else {
                    return Ok(None);
                };
                let Some(revision) = revision_by_sha(conn, row.id, &commit())? else {
                    return Ok(None);
                };
                Ok(entries_for_revision(conn, revision.id)?
                    .into_iter()
                    .find(|entry| entry.path == wanted))
            })
            .await
            .expect("entries read")
    }

    /// Whether the bytes a peer really served are in the index under their own hash.
    async fn indexed(store: &Store, bytes: &[u8]) -> bool {
        let sha256 = sha256_of(bytes);
        store
            .call(move |conn| Ok(blob_by_sha256(conn, &sha256)?.is_some()))
            .await
            .expect("blob lookup")
    }

    /// Whether the shared block cache holds every block of a file, which is what the Hub
    /// face serves to this node's own clients and re-gossips onward.
    async fn cached(world: &World, path: &str) -> bool {
        let dir = world
            .cache
            .layout()
            .file_entry(&repo(), &commit(), path)
            .expect("a cache path");
        match world.cache.existing_entry(&dir).await {
            Ok(mut entry) => entry.refresh_present().await.is_ok() && entry.is_complete(),
            Err(_) => false,
        }
    }

    /// A peer answering the metadata routes a pull uses, under the etag it chooses.
    async fn peer_answering(server: &MockServer, bytes: &[u8], etag: &str) {
        Mock::given(method("GET"))
            .and(path("/api/models/kohya/film-grain"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "kohya/film-grain",
                "sha": commit().to_hex(),
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/models/kohya/film-grain/revision/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "kohya/film-grain",
                "sha": commit().to_hex(),
            })))
            .mount(server)
            .await;
        Mock::given(method("HEAD"))
            .and(path_regex(r"^/kohya/film-grain/resolve/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", bytes.len().to_string().as_str())
                    .insert_header("x-repo-commit", commit().to_hex().as_str())
                    .insert_header("etag", etag)
                    .insert_header("accept-ranges", "bytes"),
            )
            .mount(server)
            .await;
    }

    /// A peer serving one file over the Hub routes a pull uses.
    async fn peer_serving(bytes: Vec<u8>) -> MockServer {
        let server = MockServer::start().await;
        let etag = format!("\"{}\"", sha256_of(&bytes).to_hex());
        peer_answering(&server, &bytes, &etag).await;
        let length = bytes.len();
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
        assert!(!partial_path(&world.puller.destination(&request)).exists());

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
        assert!(
            !indexed(&world.store, &bytes).await,
            "the bytes the peer served never entered the index either"
        );
        assert!(
            !cached(&world, "loras/film-grain.safetensors").await,
            "the bytes the peer served never entered the shared cache"
        );
        let entry = pulled_entry(&world.store, "loras/film-grain.safetensors")
            .await
            .expect("the refused pull leaves an entry");
        assert_eq!(entry.state, State::Remote);

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
    async fn a_peer_that_names_no_content_hash_is_refused_before_a_byte_moves() {
        let root = TempDir::new().expect("temp dir");
        let world = world(&root).await;
        let bytes = safetensors("lora_unet_down_blocks.lora_down.weight");
        let server = MockServer::start().await;
        peer_answering(&server, &bytes, "\"deadbeef\"").await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/kohya/film-grain/resolve/.*"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header(
                        "content-range",
                        format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()).as_str(),
                    )
                    .insert_header("x-repo-commit", commit().to_hex().as_str())
                    .set_body_bytes(bytes.clone()),
            )
            .mount(&server)
            .await;
        let request = PullRequest::new(
            identity(&server.uri()),
            repo(),
            commit(),
            "loras/film-grain.safetensors",
            sha256_of(&bytes),
            bytes.len() as u64,
        );

        let error = world
            .puller
            .pull(&request, None)
            .await
            .expect_err("an answer nothing can check is refused");
        assert!(matches!(error, NetError::Unverifiable { .. }));
        assert!(!world.puller.destination(&request).exists());
        assert!(!indexed(&world.store, &bytes).await);
        assert!(!cached(&world, "loras/film-grain.safetensors").await);
        assert_eq!(
            pulled_entry(&world.store, "loras/film-grain.safetensors")
                .await
                .expect("the refused pull leaves an entry")
                .state,
            State::Remote
        );
    }

    #[tokio::test]
    async fn a_transfer_that_fails_leaves_nothing_half_written_and_nothing_pulling() {
        let root = TempDir::new().expect("temp dir");
        let world = world(&root).await;
        let bytes = safetensors("lora_unet_down_blocks.lora_down.weight");
        let server = MockServer::start().await;
        let etag = format!("\"{}\"", sha256_of(&bytes).to_hex());
        peer_answering(&server, &bytes, &etag).await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/kohya/film-grain/resolve/.*"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let request = PullRequest::new(
            identity(&server.uri()),
            repo(),
            commit(),
            "loras/film-grain.safetensors",
            sha256_of(&bytes),
            bytes.len() as u64,
        );

        world
            .puller
            .pull(&request, None)
            .await
            .expect_err("a peer that stops sending fails the pull");
        let destination = world.puller.destination(&request);
        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
        let entry = pulled_entry(&world.store, "loras/film-grain.safetensors")
            .await
            .expect("the failed pull leaves an entry");
        assert_eq!(entry.state, State::Remote);
        assert_eq!(entry.pull_progress, None);
        assert!(!entry.present);
    }

    #[test]
    fn a_pull_never_lands_outside_the_pull_root() {
        let dir = TempDir::new().expect("temp dir");
        let store = Store::open_in_memory().expect("store opens");
        let config = Arc::new(MirrorConfig::default());
        let cache = Arc::new(BlockCache::at_root(Arc::clone(&config), dir.path()));
        let upstream = Hub::new(
            UpstreamClient::new(true).expect("client builds"),
            "https://huggingface.co",
        )
        .expect("hub builds");
        let mirror = Arc::new(Mirror::new(
            store,
            cache,
            Arc::new(AccessPolicy::default()),
            upstream,
        ));
        let puller = Puller::new(mirror.store().clone(), mirror, "/srv/pulled", true, 0.85)
            .expect("puller builds");
        let request = PullRequest::new(
            identity("http://laptop:7099"),
            RepoId::new(RepoType::Models, Some("..".to_owned()), ".."),
            commit(),
            "../../etc/passwd",
            sha256_of(SAFETENSORS),
            0,
        );
        assert_eq!(
            puller.destination(&request),
            PathBuf::from(format!(
                "/srv/pulled/models/{}/etc/passwd",
                commit().short()
            ))
        );
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

    /// A peer answering health and stats, which every poll begins with.
    async fn peer_answering_a_poll(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "status": "ok", "peers": 0 })),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "artifacts": 1, "indexed_bytes": 512 })),
            )
            .mount(server)
            .await;
    }

    fn digest_row(path: &str) -> serde_json::Value {
        json!({
            "sha256": sha256_of(path.as_bytes()).to_hex(),
            "size": 12,
            "role": "lora",
            "base": "sdxl",
            "repo": "models/kohya/film-grain",
            "revision_sha": commit().to_hex(),
            "path": path,
            "state": "served"
        })
    }

    /// A store holding this node and one peer row, with the cursor it last read that peer to.
    async fn store_with_peer(cursor: Option<DigestCursor>) -> (Store, i64) {
        let store = Store::open_in_memory().expect("store opens");
        let row = store
            .call_mut(|conn| {
                upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                upsert_node(
                    conn,
                    &NewNode::peer(
                        NodeId::new("laptop-1").expect("id"),
                        "laptop",
                        "http://laptop:7099",
                    ),
                )
            })
            .await
            .expect("nodes record");
        if cursor.is_some() {
            digest::write_cursor(&store, row, cursor)
                .await
                .expect("cursor writes");
        }
        (store, row)
    }

    fn peer_of(server: &MockServer, row: i64) -> Peer {
        let identity = identity(&server.uri());
        let discovered = identity.id().clone();
        Peer::new(
            PeerClient::new(identity, None).expect("client builds"),
            row,
            discovered,
        )
    }

    fn gossip_config() -> GossipConfig {
        GossipConfig::new(
            NodeIdentity::new(
                NodeId::new("workstation-1").expect("id"),
                "workstation",
                "http://127.0.0.1:7099",
            )
            .expect("identity builds"),
        )
    }

    #[tokio::test]
    async fn an_answer_past_the_ceiling_is_refused_before_it_is_parsed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b' ';
                MAX_ANSWER_BYTES as usize
                    + 1
            ]))
            .mount(&server)
            .await;
        let client = PeerClient::new(identity(&server.uri()), None).expect("client builds");
        let error = client
            .digest(None, 10)
            .await
            .expect_err("an endless answer is refused");
        assert!(matches!(error, NetError::BadDigest { peer, .. } if peer == "laptop"));
    }

    #[tokio::test]
    async fn a_page_carrying_more_rows_than_were_asked_for_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [
                    digest_row("loras/a.safetensors"),
                    digest_row("loras/b.safetensors"),
                    digest_row("loras/c.safetensors"),
                ],
                "next_cursor": null,
                "total": 3
            })))
            .mount(&server)
            .await;
        let client = PeerClient::new(identity(&server.uri()), None).expect("client builds");
        let error = client
            .digest(None, 1)
            .await
            .expect_err("a page past the limit is refused");
        assert!(matches!(error, NetError::BadDigest { .. }));
        assert_eq!(
            client
                .digest(None, 3)
                .await
                .expect("a page fits")
                .rows
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn an_empty_page_from_a_peer_that_holds_more_reads_its_digest_again() {
        let server = MockServer::start().await;
        peer_answering_a_poll(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .and(query_param("since", "d1-5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [],
                "next_cursor": null,
                "total": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .and(query_param("since", "d1-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [],
                "next_cursor": null,
                "total": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [digest_row("loras/a.safetensors")],
                "next_cursor": "d1-1",
                "total": 1
            })))
            .mount(&server)
            .await;

        let (store, row) = store_with_peer(Some(DigestCursor::new(5))).await;
        poll(&store, &gossip_config(), &peer_of(&server, row))
            .await
            .expect("the poll reads the peer again from the start");

        let filed = store
            .call(|conn| {
                hoard_index::store::search_exec::count_rows(conn, "node = ?", &["laptop".into()])
            })
            .await
            .expect("count reads");
        assert_eq!(filed, 1, "a rebuilt peer becomes visible again");
        assert_eq!(
            digest::read_cursor(&store, row)
                .await
                .expect("cursor reads"),
            Some(DigestCursor::new(1))
        );
    }

    #[tokio::test]
    async fn a_cursor_that_does_not_advance_ends_the_poll() {
        let server = MockServer::start().await;
        peer_answering_a_poll(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/peers/digest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rows": [digest_row("loras/a.safetensors")],
                "next_cursor": "d1-9",
                "total": 1
            })))
            .mount(&server)
            .await;

        let (store, row) = store_with_peer(None).await;
        poll(&store, &gossip_config(), &peer_of(&server, row))
            .await
            .expect("a repeated cursor is not an error");

        let asked = server
            .received_requests()
            .await
            .expect("requests are recorded")
            .into_iter()
            .filter(|request| request.url.path() == "/api/v1/peers/digest")
            .count();
        assert_eq!(asked, 2, "the peer is not paged in circles");
    }

    #[tokio::test]
    async fn a_node_that_answers_as_this_one_is_never_recorded_as_a_peer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/nodes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!([{ "id": "workstation-1", "name": "workstation", "self": true }]),
            ))
            .mount(&server)
            .await;
        let (store, _) = store_with_peer(None).await;
        let config = GossipConfig::new(
            NodeIdentity::new(
                NodeId::new("workstation-1").expect("id"),
                "workstation",
                &server.uri(),
            )
            .expect("identity builds"),
        );
        let found = NodeIdentity::from_address(&server.uri()).expect("static peer");
        assert!(
            record(&store, &config, found)
                .await
                .expect("recording answers")
                .is_none(),
            "a node never gossips with itself"
        );
    }

    #[tokio::test]
    async fn one_peer_found_twice_is_one_node() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/nodes"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{ "id": "alpha-1", "name": "alpha", "self": true }])),
            )
            .mount(&server)
            .await;
        let (store, _) = store_with_peer(None).await;
        let config = gossip_config();

        let statically = record(
            &store,
            &config,
            NodeIdentity::from_address(&server.uri()).expect("static peer"),
        )
        .await
        .expect("the static peer records")
        .expect("a peer");
        let advertised = record(
            &store,
            &config,
            NodeIdentity::new(
                NodeId::new("guessed-1").expect("id"),
                "guess",
                &server.uri(),
            )
            .expect("identity builds"),
        )
        .await
        .expect("the advertised peer records")
        .expect("a peer");
        assert_eq!(statically.id(), advertised.id());
        assert_eq!(statically.row, advertised.row);

        let mut peers = PeerSet::default();
        assert!(peers.insert(statically));
        assert!(peers.insert(advertised));
        assert_eq!(peers.peers.len(), 1);

        let mut names: Vec<String> = store
            .call(list_nodes)
            .await
            .expect("nodes read")
            .into_iter()
            .map(|node| node.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "laptop", "workstation"]);
    }

    #[test]
    fn the_peer_set_holds_no_more_than_the_lan_is_allowed_to_fill() {
        let mut peers = PeerSet::default();
        for ordinal in 0..MAX_PEERS + 4 {
            let identity = NodeIdentity::new(
                NodeId::new(format!("peer-{ordinal}")).expect("id"),
                format!("peer {ordinal}"),
                &format!("http://peer-{ordinal}.lan:7099"),
            )
            .expect("identity builds");
            let discovered = identity.id().clone();
            let client = PeerClient::new(identity, None).expect("client builds");
            let accepted = peers.insert(Peer::new(client, ordinal as i64, discovered));
            assert_eq!(accepted, ordinal < MAX_PEERS);
        }
        assert_eq!(peers.peers.len(), MAX_PEERS);
        assert!(peers.is_full());

        let gone = peers.remove(&NodeId::new("peer-0").expect("id"));
        assert_eq!(gone.len(), 1);
        assert!(!peers.is_full());
    }

    #[test]
    fn a_peer_that_keeps_failing_is_asked_less_and_less_often() {
        let identity = identity("http://laptop:7099");
        let discovered = identity.id().clone();
        let mut peer = Peer::new(
            PeerClient::new(identity, None).expect("client builds"),
            1,
            discovered,
        );
        let now = Instant::now();
        assert!(peer.is_due(now));

        peer.failed(Duration::from_secs(30), now);
        assert!(!peer.is_due(now));
        let first = peer.due;
        peer.failed(Duration::from_secs(30), now);
        assert!(peer.due > first, "each failure costs another interval");

        peer.answered();
        assert_eq!(peer.failures, 0);
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
    async fn shutdown_does_not_wait_out_a_peer_that_is_not_answering() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/nodes"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{ "id": "alpha-1", "name": "alpha", "self": true }])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(10))
                    .set_body_json(json!({ "status": "ok", "peers": 0 })),
            )
            .mount(&server)
            .await;

        let (store, _) = store_with_peer(None).await;
        let config = gossip_config()
            .with_discovery(DiscoveryMode::Static)
            .with_statics(vec![server.uri()])
            .with_interval(Duration::from_millis(20));
        let handle = spawn(store, config);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let asked = Instant::now();
        handle.shutdown().await;
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "a stopped node does not wait for a poll in flight"
        );
        let polled = server
            .received_requests()
            .await
            .expect("requests are recorded")
            .into_iter()
            .any(|request| request.url.path() == "/healthz");
        assert!(polled, "the poll this shutdown interrupted had started");
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
