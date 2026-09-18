//! Asking a listing service by sha256 what a hashed artifact is: once per hash, and once more
//! every thirty days while the service does not know it.
//!
//! Off unless `[lookup] civitai = true`, and never while the mirror is offline. Nothing about
//! a file but its content hash leaves this machine: no path, no name, no token, and no
//! redirect followed, so the hash reaches the host the configuration names and no other. A
//! pass runs only while no scan is running or queued, again as soon as one has finished, and
//! otherwise every quarter of an hour: a scan is what restores the hashes a rebuilt catalog
//! is asked about, so asking before one has run asks about nothing.

use std::sync::Arc;
use std::time::Duration;

use hoard_core::Sha256;
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, JobProgress, Listed, LookupAnswer};
use hoard_index::coordinate::Coordinator;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use url::Url;

use crate::error::{MirrorError, Result};
use crate::upstream::{RetryPolicy, UpstreamClient, UpstreamRequest, read_capped};

/// The service the lookup asks unless `[lookup] civitai_url` names another; redirects are not
/// followed.
pub const DEFAULT_CIVITAI_URL: &str = "https://civitai.com";

/// How many times one hash is asked about in one pass. A service that is rate limiting has
/// answered; asking it again with the same hash is what the pass ends rather than does.
const ATTEMPTS: u32 = 1;

/// The route that answers what a sha256 is, relative to the configured base.
const BY_HASH_PATH: &str = "api/v1/model-versions/by-hash";

/// How long between passes. A pass asks about what the previous one did not reach and about
/// files that landed since, so this is a trickle rather than a schedule to meet.
const PASS_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often the worker looks at the scan lane while it waits for it, and between passes.
const SCAN_WAIT: Duration = Duration::from_secs(10);

/// The least time between two requests, so one node is never a burst.
const REQUEST_GAP: Duration = Duration::from_secs(1);

/// How many candidates one pass holds at a time.
const WINDOW: usize = 256;

/// How many answers are remembered and applied at once, so a stop loses at most this many
/// requests.
const BATCH: usize = 16;

/// The most an answer about one hash may be. The document is a few kilobytes of JSON; more
/// than this is an upstream that lost the plot.
const MAX_ANSWER_BYTES: u64 = 1024 * 1024;

const JOB_KIND: &str = "lookup";

const JOB_SUBJECT: &str = "civitai";

fn default_civitai_url() -> String {
    DEFAULT_CIVITAI_URL.to_owned()
}

/// The `[lookup]` section of `hoard.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupConfig {
    /// Whether the sha256 of each checkpoint, LoRA, ControlNet, VAE, embedding and upscaler on
    /// this node may be sent to civitai.com: once per file, and once more every thirty days
    /// while the service does not know it.
    #[serde(default)]
    pub civitai: bool,
    #[serde(default = "default_civitai_url")]
    pub civitai_url: String,
}

impl Default for LookupConfig {
    fn default() -> Self {
        Self {
            civitai: false,
            civitai_url: default_civitai_url(),
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LookupReport {
    /// Answers the memo already held, applied without a request.
    pub remembered: usize,
    /// Requests this pass sent.
    pub asked: usize,
    /// Stored bases the pass changed.
    pub applied: usize,
    /// Whether the service asked the pass to stop, by rate limiting it or failing.
    pub halted: bool,
    /// Whether the node was shutting down before the pass reached the end of its window.
    pub stopped: bool,
}

#[derive(Debug)]
pub struct LookupHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl LookupHandle {
    /// Asks the loop to finish, dropping the request it may be waiting on rather than waiting
    /// for a service that has stopped answering.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn shutdown(self) {
        self.stop();
        if let Err(error) = self.task.await {
            tracing::debug!(%error, "the lookup ended abruptly");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// Runs a pass when the node starts and no scan is running or queued, again as soon as a
/// scan has finished, and otherwise every quarter of an hour; a failed pass is logged and the
/// loop continues.
pub fn spawn_lookup(
    catalog: Arc<Catalog>,
    client: UpstreamClient,
    config: LookupConfig,
    scans: Arc<Coordinator>,
) -> LookupHandle {
    let (stop, mut stopped) = watch::channel(false);
    let lookup = Lookup::new(catalog, client, config.civitai_url, stopped.clone());
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SCAN_WAIT);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut cadence = Cadence::new(scans.finished(), Instant::now());
        loop {
            tokio::select! {
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if cadence.ready(scans.busy(), scans.finished(), Instant::now()) {
                        let before = scans.finished();
                        pass_once(&lookup).await;
                        cadence.passed(before, Instant::now());
                    }
                }
            }
        }
        tracing::debug!("the lookup stopped");
    });
    LookupHandle { stop, task }
}

/// When a pass runs: never while a scan runs or waits, as soon as one has finished since the
/// last pass, and otherwise every [`PASS_INTERVAL`].
#[derive(Debug)]
struct Cadence {
    seen: u64,
    due: Instant,
}

impl Cadence {
    fn new(finished: u64, now: Instant) -> Self {
        Self {
            seen: finished,
            due: now,
        }
    }

    /// Whether this look at the lane starts a pass.
    fn ready(&self, busy: bool, finished: u64, now: Instant) -> bool {
        !busy && (finished != self.seen || now >= self.due)
    }

    /// Records that a pass ran, so the next waits for a scan to finish or the interval.
    /// `finished` is the count read before the pass, so a scan that started and ended while
    /// the pass ran is one the next look still sees.
    fn passed(&mut self, finished: u64, now: Instant) {
        self.seen = finished;
        self.due = now + PASS_INTERVAL;
    }
}

async fn pass_once(lookup: &Lookup) {
    match lookup.pass().await {
        Ok(report) if report.applied > 0 || report.asked > 0 => tracing::info!(
            asked = report.asked,
            remembered = report.remembered,
            applied = report.applied,
            halted = report.halted,
            "the lookup refined what it was told"
        ),
        Ok(report) => tracing::debug!(
            remembered = report.remembered,
            "the lookup had nothing to ask about"
        ),
        Err(error) => tracing::error!(%error, "a lookup pass failed"),
    }
}

/// One pass of the lookup, and everything it needs to run one.
struct Lookup {
    catalog: Arc<Catalog>,
    client: UpstreamClient,
    base: String,
    stopped: watch::Receiver<bool>,
}

/// What one question about one hash produced.
enum Answered {
    Listed(Listed),
    Unknown,
    Skipped,
    Halted(String),
}

impl Lookup {
    /// The worker as it runs: one attempt per hash, because a service that is rate limiting
    /// is not asked the same question three times, and no redirect followed, because the
    /// path carries a hash of a file on this machine.
    fn new(
        catalog: Arc<Catalog>,
        client: UpstreamClient,
        base: String,
        stopped: watch::Receiver<bool>,
    ) -> Self {
        Self {
            catalog,
            client: client.with_retry(RetryPolicy::immediate(ATTEMPTS)),
            base,
            stopped,
        }
    }

    /// Applies what the memo already knows, then asks about the rest one at a time.
    ///
    /// Once the job exists every way out of this function settles it: the asking is one
    /// fallible block, and its error fails the job before it is returned.
    async fn pass(&self) -> Result<LookupReport> {
        let (remembered, unasked) = self.window().await?;
        let mut report = LookupReport {
            remembered: remembered.len(),
            ..LookupReport::default()
        };
        report.applied += self.apply_in_batches(remembered).await?;
        if unasked.is_empty() {
            return Ok(report);
        }
        let url = self.service_url()?;
        let total = unasked.len();
        let job = self.start_job(total).await?;
        match self.ask_each(&url, unasked, job, &mut report).await {
            Ok(halted) => {
                report.halted = halted.is_some();
                self.settle_job(job, halted, report.stopped).await?;
                Ok(report)
            }
            Err(error) => {
                self.fail_quietly(job, &error).await;
                Err(error)
            }
        }
    }

    /// Asks about each hash in turn, remembering and applying every [`BATCH`] answers, and
    /// returns why the pass stopped early when it did.
    async fn ask_each(
        &self,
        url: &Url,
        unasked: Vec<Sha256>,
        job: hoard_core::JobId,
        report: &mut LookupReport,
    ) -> Result<Option<String>> {
        let total = unasked.len();
        let mut batch: Vec<(Sha256, LookupAnswer)> = Vec::with_capacity(BATCH);
        let mut halted = None;
        let mut stopped = self.stopped.clone();
        for (asked, sha256) in unasked.into_iter().enumerate() {
            if asked > 0 {
                tokio::time::sleep(REQUEST_GAP).await;
            }
            let answered = tokio::select! {
                biased;
                () = stop_asked(&mut stopped) => {
                    report.stopped = true;
                    break;
                }
                answered = self.ask(url, &sha256) => answered,
            };
            report.asked += 1;
            match answered {
                Answered::Listed(listed) => {
                    batch.push((sha256, LookupAnswer::asked_now(Some(listed))));
                }
                Answered::Unknown => batch.push((sha256, LookupAnswer::asked_now(None))),
                Answered::Skipped => {}
                Answered::Halted(reason) => {
                    halted = Some(reason);
                    break;
                }
            }
            if batch.len() == BATCH {
                report.applied += self.apply(std::mem::take(&mut batch)).await?;
                self.report_progress(job, asked + 1, total).await?;
            }
        }
        report.applied += self.apply(batch).await?;
        Ok(halted)
    }

    /// The candidates of one window, split into what the memo already answers and what has to
    /// be asked about.
    async fn window(&self) -> Result<(Vec<(Sha256, LookupAnswer)>, Vec<Sha256>)> {
        let catalog = Arc::clone(&self.catalog);
        blocking(move || {
            let candidates = catalog.lookup_candidates(WINDOW)?;
            let mut remembered = Vec::new();
            let mut unasked = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                match catalog.lookup_answer(&candidate.sha256)? {
                    Some(held) => remembered.push((candidate.sha256, held)),
                    None => unasked.push(candidate.sha256),
                }
            }
            Ok((remembered, unasked))
        })
        .await
    }

    /// Applies a whole group [`BATCH`] at a time, so what the memo already holds costs the
    /// same bounded transaction an asked batch does however wide the window is.
    async fn apply_in_batches(&self, answers: Vec<(Sha256, LookupAnswer)>) -> Result<usize> {
        let mut applied = 0;
        let mut rest = answers;
        while !rest.is_empty() {
            let tail = rest.split_off(BATCH.min(rest.len()));
            applied += self.apply(rest).await?;
            rest = tail;
        }
        Ok(applied)
    }

    /// Remembers a batch before it applies it, so a crash between the two costs a write and
    /// never a second request.
    async fn apply(&self, answers: Vec<(Sha256, LookupAnswer)>) -> Result<usize> {
        if answers.is_empty() {
            return Ok(0);
        }
        let catalog = Arc::clone(&self.catalog);
        blocking(move || {
            catalog.remember_lookups(&answers)?;
            catalog.apply_lookups(&answers)
        })
        .await
    }

    async fn ask(&self, url: &Url, sha256: &Sha256) -> Answered {
        let Ok(target) = url.join(&sha256.to_hex()) else {
            return Answered::Skipped;
        };
        let response = match self
            .client
            .send(UpstreamRequest::new(Method::GET, &target))
            .await
        {
            Ok(response) => response,
            Err(MirrorError::ProxyTimeout) => {
                return Answered::Halted("the service is rate limiting or unreachable".to_owned());
            }
            Err(MirrorError::Offline(_)) => {
                return Answered::Halted("the mirror is offline".to_owned());
            }
            Err(error) => {
                tracing::debug!(%error, "one lookup request failed and was left for a later pass");
                return Answered::Skipped;
            }
        };
        match response.status() {
            StatusCode::OK => match read_capped(response, MAX_ANSWER_BYTES, &target).await {
                Ok(body) => match listed_in(&body) {
                    Some(listed) => Answered::Listed(listed),
                    None => Answered::Unknown,
                },
                Err(error) => {
                    tracing::debug!(%error, "a lookup answer could not be read");
                    Answered::Skipped
                }
            },
            StatusCode::NOT_FOUND => Answered::Unknown,
            other => {
                tracing::debug!(
                    status = other.as_u16(),
                    "the lookup was answered with nothing"
                );
                Answered::Skipped
            }
        }
    }

    fn service_url(&self) -> Result<Url> {
        let base = format!("{}/{BY_HASH_PATH}/", self.base.trim_end_matches('/'));
        Url::parse(&base)
            .map_err(|error| MirrorError::Config(format!("[lookup] civitai_url: {error}")))
    }

    /// Opens the pass's job and reports what it holds. A job that cannot be told its own size
    /// is failed here rather than left running by a pass that never started.
    async fn start_job(&self, total: usize) -> Result<hoard_core::JobId> {
        let catalog = Arc::clone(&self.catalog);
        let id = blocking(move || catalog.create_job(JOB_KIND, Some(JOB_SUBJECT))).await?;
        if let Err(error) = self.report_progress(id, 0, total).await {
            self.fail_quietly(id, &error).await;
            return Err(error);
        }
        Ok(id)
    }

    /// Settles a job as failed for a caller that is already returning an error, so a second
    /// failure cannot replace the first.
    async fn fail_quietly(&self, id: hoard_core::JobId, error: &MirrorError) {
        if let Err(unsettled) = self.settle_job(id, Some(error.to_string()), false).await {
            tracing::debug!(%unsettled, "the failed lookup job could not be settled");
        }
    }

    async fn report_progress(
        &self,
        id: hoard_core::JobId,
        asked: usize,
        total: usize,
    ) -> Result<()> {
        let catalog = Arc::clone(&self.catalog);
        let progress = JobProgress {
            phase: Some("asking by sha256".to_owned()),
            progress: asked as f32 / total.max(1) as f32,
            bytes: 0,
            total: Some(total as u64),
            rate_bps: None,
        };
        blocking(move || catalog.update_job(id, &progress)).await
    }

    async fn settle_job(
        &self,
        id: hoard_core::JobId,
        halted: Option<String>,
        stopped: bool,
    ) -> Result<()> {
        let catalog = Arc::clone(&self.catalog);
        blocking(move || match (halted, stopped) {
            (Some(reason), _) => catalog.fail_job(id, &reason),
            (None, true) => catalog.cancel_job(id),
            (None, false) => catalog.finish_job(id),
        })
        .await
    }
}

/// Resolves as soon as the node is shutting down, and at once when it already is.
async fn stop_asked(stopped: &mut watch::Receiver<bool>) {
    while !*stopped.borrow_and_update() {
        if stopped.changed().await.is_err() {
            return;
        }
    }
}

/// The listing a 200 carries, or nothing when the body names no base model, which is an
/// answer of "nothing useful" rather than a reason to ask again.
fn listed_in(body: &[u8]) -> Option<Listed> {
    let version: ByHash = serde_json::from_slice(body).ok()?;
    let base_model = version.base_model.filter(|value| !value.is_empty())?;
    let (model, kind) = version
        .model
        .map(|model| {
            (
                model.name.unwrap_or_default(),
                model.kind.unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    Some(Listed {
        base_model,
        model,
        version: version.name.unwrap_or_default(),
        kind,
    })
}

#[derive(Debug, Deserialize)]
struct ByHash {
    #[serde(rename = "baseModel")]
    base_model: Option<String>,
    name: Option<String>,
    model: Option<ByHashModel>,
}

#[derive(Debug, Deserialize)]
struct ByHashModel {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Runs one piece of catalog work off the reactor.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> hoard_index::Result<T> + Send + 'static,
    T: Send + 'static,
{
    Ok(tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| IndexError::Catalog(format!("the lookup task ended: {error}")))??)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use hoard_core::{
        Artifact, ArtifactFormat, ArtifactId, Base, BlobId, RepoId, RepoType, Role, Sha256, State,
    };
    use hoard_index::catalog::{
        JobState, LOCAL_NODE, SeedArtifact, SeedEntry, SeedRepository, SeedRevision, content_key,
    };
    use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
    use hoard_index::coordinate::{Origin, ScanRequest};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const ILLUSTRIOUS: &str = r#"{"id":1,"name":"v24","baseModel":"Illustrious",
        "model":{"name":"One obsession","type":"Checkpoint"}}"#;

    fn sha(byte: u8) -> Sha256 {
        Sha256::from_bytes([byte; 32])
    }

    fn by_hash(sha256: &Sha256) -> String {
        format!("/{BY_HASH_PATH}/{}", sha256.to_hex())
    }

    fn seeded_artifact(sha256: &Sha256, name: &str, base: &str) -> SeedArtifact {
        SeedArtifact {
            identity: content_key(sha256),
            artifact: Artifact {
                id: ArtifactId::new(0),
                blob: BlobId::new(0),
                sha256: Some(*sha256),
                size: 4096,
                format: ArtifactFormat::Safetensors,
                role: Role::Ckpt,
                sub_role: None,
                role_confidence: 0.9,
                base: Base::new(base),
                base_label: Some(base.to_owned()),
                base_confidence: 0.8,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
            },
            name: name.to_owned(),
            org: None,
            tensors: None,
            evidence: Vec::new(),
            card: None,
            group_bytes: None,
            shard_count: None,
        }
    }

    /// A catalog holding one hashed checkpoint per named file, each present on this node.
    fn seeded(directory: &Path, files: &[(Sha256, &str, &str)]) -> Arc<Catalog> {
        let catalog = Catalog::open_or_scan(
            &IndexConfig {
                data_dir: directory.join("data"),
                sources: Vec::new(),
                ..IndexConfig::default()
            },
            "node-a",
            |_, _| {},
        )
        .expect("catalog opens");
        seed_repo(&catalog, directory, "checkpoints", files);
        catalog.rebuild_search(|_| {}).expect("the index builds");
        Arc::new(catalog)
    }

    /// One repository of hashed checkpoints written into a catalog that is already open.
    fn seed_repo(catalog: &Catalog, directory: &Path, repo: &str, files: &[(Sha256, &str, &str)]) {
        let entries: Vec<SeedEntry> = files
            .iter()
            .map(|(sha256, name, base)| SeedEntry {
                node: LOCAL_NODE,
                rel_path: format!("{repo}/{name}.safetensors"),
                abs_path: None,
                size: 4096,
                state: State::Local,
                is_symlink: false,
                sha256: Some(*sha256),
                git_sha1: None,
                artifact: Some(seeded_artifact(sha256, name, base)),
            })
            .collect();
        catalog
            .seed(&[SeedRepository {
                repo: RepoId::new(RepoType::Models, None, repo),
                source: "files".to_owned(),
                kind: SourceKind::Loose,
                root_path: directory.join("files"),
                git_dir: None,
                tool_layout: None,
                revisions: vec![SeedRevision {
                    name: "main".to_owned(),
                    sha: None,
                    refs: Vec::new(),
                    title: None,
                    author: None,
                    committed: None,
                    entries,
                }],
            }])
            .expect("the catalog is seeded");
    }

    /// The worker exactly as `spawn_lookup` builds it: the same client, the same one attempt
    /// per hash, so what these tests pin is what a node runs.
    fn lookup(catalog: &Arc<Catalog>, base: &str) -> (Lookup, watch::Sender<bool>) {
        let (stop, stopped) = watch::channel(false);
        (
            Lookup::new(
                Arc::clone(catalog),
                lookup_client(),
                base.to_owned(),
                stopped,
            ),
            stop,
        )
    }

    /// The client `hoard_server::start_lookup` hands the worker: no redirect followed, and
    /// the single attempt `Lookup::new` applies on top of it.
    fn lookup_client() -> UpstreamClient {
        UpstreamClient::direct(false).expect("client builds")
    }

    fn only_job(catalog: &Catalog) -> hoard_index::catalog::JobRecord {
        catalog
            .list_jobs(None)
            .expect("jobs read back")
            .into_iter()
            .next()
            .expect("the pass opened a job")
    }

    fn base_of(catalog: &Catalog, sha256: &Sha256) -> Base {
        catalog
            .artifact_by_sha256(sha256)
            .expect("the artifact reads back")
            .expect("the artifact is indexed")
            .base
    }

    async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
        server.received_requests().await.unwrap_or_default()
    }

    /// A lane over the catalog the worker reads, with nothing queued on it.
    fn idle_lane(directory: &Path, catalog: &Arc<Catalog>) -> Arc<Coordinator> {
        Arc::new(Coordinator::new(
            Arc::clone(catalog),
            IndexConfig {
                data_dir: directory.join("data"),
                sources: Vec::new(),
                ..IndexConfig::default()
            },
        ))
    }

    async fn asked(server: &MockServer, count: usize, looks: usize) -> Vec<wiremock::Request> {
        for _ in 0..looks {
            let seen = requests(server).await;
            if seen.len() >= count {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        requests(server).await
    }

    /// A synthetic LoRA header, written by the test and never a real weight file.
    fn lora_header() -> Vec<u8> {
        let header = concat!(
            r#"{"__metadata__":{"ss_network_dim":"16","ss_network_alpha":"8"},"#,
            r#""lora_unet_down_blocks_0_attn.lora_down.weight":"#,
            r#"{"dtype":"F16","shape":[16,768],"data_offsets":[0,24576]},"#,
            r#""lora_unet_down_blocks_0_attn.lora_up.weight":"#,
            r#"{"dtype":"F16","shape":[768,16],"data_offsets":[24576,49152]}}"#
        );
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes
    }

    /// A lane over one source holding one synthetic LoRA, hashing what it finds, and the
    /// catalog it writes into, which starts empty as a rebuilt one does.
    fn lane_over_a_source(directory: &Path) -> (Arc<Catalog>, Arc<Coordinator>) {
        let files = directory.join("files");
        fs::create_dir_all(files.join("loras")).expect("the source directory");
        fs::write(
            files.join("loras").join("one-obsession.safetensors"),
            lora_header(),
        )
        .expect("the fixture is written");
        let data_dir = directory.join("data");
        let catalog = Arc::new(
            Catalog::open_or_scan(
                &IndexConfig {
                    data_dir: data_dir.clone(),
                    sources: Vec::new(),
                    ..IndexConfig::default()
                },
                "node-a",
                |_, _| {},
            )
            .expect("catalog opens"),
        );
        let scans = Arc::new(Coordinator::new(
            Arc::clone(&catalog),
            IndexConfig {
                data_dir,
                hash_policy: HashPolicy::Now,
                sources: vec![SourceConfig {
                    name: "files".to_owned(),
                    path: files,
                    kind: SourceKind::Loose,
                    include: Vec::new(),
                    exclude: Vec::new(),
                    follow_symlinks: false,
                    repo_per_folder: false,
                }],
                ..IndexConfig::default()
            },
        ));
        (catalog, scans)
    }

    #[tokio::test]
    async fn the_worker_passes_again_after_the_lane_finishes_a_scan() {
        let directory = TempDir::new().expect("temp dir");
        let (catalog, scans) = lane_over_a_source(directory.path());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ILLUSTRIOUS, "application/json"))
            .mount(&server)
            .await;
        let handle = spawn_lookup(
            Arc::clone(&catalog),
            lookup_client(),
            LookupConfig {
                civitai: true,
                civitai_url: server.uri(),
            },
            Arc::clone(&scans),
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            requests(&server).await.is_empty(),
            "a catalog holding no hash asks about nothing"
        );

        scans
            .request(ScanRequest {
                sources: None,
                hash: HashPolicy::Now,
                origin: Origin::Manual,
            })
            .await
            .expect("the lane takes the request");
        for _ in 0..600 {
            if !scans.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let sent = asked(&server, 1, 150).await;
        assert_eq!(
            sent.len(),
            1,
            "a finished scan starts a pass rather than the quarter hour"
        );
        let hash: Sha256 = sent[0]
            .url
            .path()
            .strip_prefix(&format!("/{BY_HASH_PATH}/"))
            .expect("the pass asks on the by-hash route")
            .parse()
            .expect("the route carries a sha256");
        let refined = Base::new("illustrious");
        let mut stored = base_of(&catalog, &hash);
        for _ in 0..200 {
            if stored == refined {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            stored = base_of(&catalog, &hash);
        }
        assert_eq!(
            stored, refined,
            "the pass asked about the hash the scan produced and applied the answer"
        );

        handle.shutdown().await;
        scans.shutdown().await.expect("the lane stops");
    }

    #[test]
    fn the_first_look_on_an_idle_lane_starts_a_pass() {
        let start = Instant::now();
        assert!(Cadence::new(0, start).ready(false, 0, start));
    }

    #[test]
    fn no_pass_starts_while_a_scan_runs_or_waits() {
        let start = Instant::now();
        let cadence = Cadence::new(0, start);
        assert!(!cadence.ready(true, 0, start));
        assert!(!cadence.ready(true, 1, start + PASS_INTERVAL));
    }

    #[test]
    fn a_pass_starts_as_soon_as_a_scan_has_finished() {
        let start = Instant::now();
        let mut cadence = Cadence::new(0, start);
        cadence.passed(0, start);
        assert!(!cadence.ready(false, 0, start + SCAN_WAIT));
        assert!(cadence.ready(false, 1, start + SCAN_WAIT));
    }

    #[test]
    fn a_scan_that_ends_during_a_pass_starts_the_next_one() {
        let start = Instant::now();
        let mut cadence = Cadence::new(0, start);
        cadence.passed(0, start);
        assert!(cadence.ready(false, 1, start + SCAN_WAIT));
    }

    #[test]
    fn a_pass_starts_at_the_interval_when_no_scan_ran() {
        let start = Instant::now();
        let mut cadence = Cadence::new(3, start);
        cadence.passed(3, start);
        assert!(!cadence.ready(false, 3, start + PASS_INTERVAL - SCAN_WAIT));
        assert!(cadence.ready(false, 3, start + PASS_INTERVAL));
    }

    #[tokio::test]
    async fn a_known_hash_is_asked_once_and_refines_the_base() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x11);
        let catalog = seeded(directory.path(), &[(hash, "one-obsession", "sdxl")]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher(by_hash(&hash)))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ILLUSTRIOUS, "application/json"))
            .mount(&server)
            .await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        let first = worker.pass().await.expect("the pass runs");

        assert_eq!(first.asked, 1);
        assert_eq!(first.applied, 1);
        assert_eq!(base_of(&catalog, &hash), Base::new("illustrious"));
        assert_eq!(requests(&server).await.len(), 1);
        assert_eq!(
            only_job(&catalog).state,
            JobState::Done,
            "a pass that ran to the end settles its job"
        );

        let second = worker.pass().await.expect("the second pass runs");

        assert_eq!(second.asked, 0, "one request per hash, ever");
        assert_eq!(second.remembered, 0, "an applied artifact is not revisited");
        assert_eq!(
            requests(&server).await.len(),
            1,
            "the second pass opens no socket"
        );
    }

    #[tokio::test]
    async fn an_unknown_hash_is_remembered_and_not_asked_again() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x22);
        let catalog = seeded(directory.path(), &[(hash, "home-made", "sdxl")]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_raw(r#"{"error":"Model not found"}"#, "application/json"),
            )
            .mount(&server)
            .await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        let first = worker.pass().await.expect("the pass runs");
        let second = worker.pass().await.expect("the second pass runs");

        assert_eq!(first.asked, 1);
        assert_eq!(first.applied, 0);
        assert_eq!(second.asked, 0, "an unknown hash is an answer too");
        assert_eq!(requests(&server).await.len(), 1);
        assert_eq!(base_of(&catalog, &hash), Base::new("sdxl"));
    }

    #[tokio::test]
    async fn a_rate_limit_ends_the_pass_and_the_hash_is_asked_next_time() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x33);
        let catalog = seeded(directory.path(), &[(hash, "one-obsession", "sdxl")]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        let first = worker.pass().await.expect("the pass runs");

        assert!(first.halted, "a rate limit ends the pass");
        assert_eq!(first.applied, 0);
        assert_eq!(
            requests(&server).await.len(),
            1,
            "a service that is rate limiting is asked once, not once per retry"
        );
        let jobs = catalog.list_jobs(None).expect("jobs read back");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, JOB_KIND);
        assert_eq!(jobs[0].subject.as_deref(), Some(JOB_SUBJECT));
        assert_eq!(jobs[0].state, JobState::Failed);

        let second = worker.pass().await.expect("the second pass runs");

        assert_eq!(
            second.asked, 1,
            "a hash that was never answered is asked again"
        );
    }

    #[tokio::test]
    async fn a_memo_answer_is_applied_without_a_request() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x44);
        let catalog = seeded(directory.path(), &[(hash, "one-obsession", "sdxl")]);
        catalog
            .remember_lookups(&[(
                hash,
                LookupAnswer::asked_now(Some(Listed {
                    base_model: "Illustrious".to_owned(),
                    model: "One obsession".to_owned(),
                    version: "v24".to_owned(),
                    kind: "Checkpoint".to_owned(),
                })),
            )])
            .expect("the memo is seeded");
        let server = MockServer::start().await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        let report = worker.pass().await.expect("the pass runs");

        assert_eq!(report.remembered, 1);
        assert_eq!(report.applied, 1);
        assert_eq!(report.asked, 0);
        assert!(
            requests(&server).await.is_empty(),
            "what a rebuilt catalog forgot is restored without a socket"
        );
        assert_eq!(base_of(&catalog, &hash), Base::new("illustrious"));
    }

    #[tokio::test]
    async fn a_stop_between_requests_cancels_the_job() {
        let directory = TempDir::new().expect("temp dir");
        let first = sha(0x55);
        let second = sha(0x56);
        let catalog = seeded(
            directory.path(),
            &[
                (first, "one-obsession", "sdxl"),
                (second, "other-mix", "sdxl"),
            ],
        );
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ILLUSTRIOUS, "application/json"))
            .mount(&server)
            .await;
        let (worker, stop) = lookup(&catalog, &server.uri());

        let running = tokio::spawn(async move { worker.pass().await });
        for _ in 0..200 {
            if !requests(&server).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = stop.send(true);
        let report = running
            .await
            .expect("the pass ends")
            .expect("the pass runs");

        assert!(report.stopped);
        assert_eq!(report.asked, 1, "the request after the stop is never sent");
        assert_eq!(requests(&server).await.len(), 1);
        let jobs = catalog.list_jobs(None).expect("jobs read back");
        assert_eq!(jobs[0].state, JobState::Cancelled);
        assert_eq!(base_of(&catalog, &second), Base::new("sdxl"));
    }

    #[tokio::test]
    async fn only_the_hash_leaves_the_machine() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x66);
        let catalog = seeded(
            directory.path(),
            &[(hash, "a-name-nobody-else-sees", "sdxl")],
        );
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ILLUSTRIOUS, "application/json"))
            .mount(&server)
            .await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        worker.pass().await.expect("the pass runs");

        let sent = requests(&server).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url.path(), by_hash(&hash));
        assert_eq!(sent[0].url.query(), None, "nothing rides in a query string");
        assert!(
            sent[0].headers.get("authorization").is_none(),
            "no credential is sent to a listing service"
        );
        assert!(sent[0].body.is_empty());
        let text = format!("{:?}", sent[0]);
        assert!(
            !text.contains("a-name-nobody-else-sees"),
            "the name of the file never leaves the machine: {text}"
        );
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x77);
        let catalog = seeded(directory.path(), &[(hash, "one-obsession", "sdxl")]);
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ILLUSTRIOUS, "application/json"))
            .mount(&elsewhere)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}{}", elsewhere.uri(), by_hash(&hash))),
            )
            .mount(&server)
            .await;
        let (worker, _stop) = lookup(&catalog, &server.uri());

        let report = worker.pass().await.expect("the pass runs");

        assert_eq!(report.asked, 1);
        assert!(
            requests(&elsewhere).await.is_empty(),
            "a hash is never carried on to the host a redirect names"
        );
        assert_eq!(report.applied, 0, "a redirect answers nothing");
        assert_eq!(base_of(&catalog, &hash), Base::new("sdxl"));
        assert_eq!(
            catalog
                .lookup_answer(&hash)
                .expect("the memo reads back")
                .map(|answer| answer.found),
            None,
            "nothing is remembered for a hash that was never answered"
        );
    }

    #[tokio::test]
    async fn a_stop_during_a_request_returns_promptly() {
        let directory = TempDir::new().expect("temp dir");
        let hash = sha(0x88);
        let catalog = seeded(directory.path(), &[(hash, "one-obsession", "sdxl")]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(ILLUSTRIOUS, "application/json")
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let scans = idle_lane(directory.path(), &catalog);
        let handle = spawn_lookup(
            Arc::clone(&catalog),
            lookup_client(),
            LookupConfig {
                civitai: true,
                civitai_url: server.uri(),
            },
            Arc::clone(&scans),
        );
        for _ in 0..200 {
            if !requests(&server).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !handle.is_finished(),
            "the worker is inside the request it is about to be stopped during"
        );

        let asked_to_stop = std::time::Instant::now();
        handle.shutdown().await;
        let waited = asked_to_stop.elapsed();

        assert!(
            waited < Duration::from_secs(10),
            "a node stopping does not wait out a hung request: {waited:?}"
        );
        assert_eq!(
            only_job(&catalog).state,
            JobState::Cancelled,
            "the job the stopped pass opened is settled"
        );
        assert_eq!(base_of(&catalog, &hash), Base::new("sdxl"));
    }

    #[test]
    fn the_lookup_is_off_and_points_at_one_host_by_default() {
        let config = LookupConfig::default();
        assert!(!config.civitai);
        assert_eq!(config.civitai_url, DEFAULT_CIVITAI_URL);
    }

    #[test]
    fn a_body_with_no_base_model_is_an_answer_of_nothing() {
        assert_eq!(listed_in(br#"{"name":"v1","model":{"name":"x"}}"#), None);
        assert_eq!(listed_in(b"not json at all"), None);
        let listed = listed_in(ILLUSTRIOUS.as_bytes()).expect("a base model is listed");
        assert_eq!(listed.base_model, "Illustrious");
        assert_eq!(listed.model, "One obsession");
        assert_eq!(listed.version, "v24");
        assert_eq!(listed.kind, "Checkpoint");
    }
}
