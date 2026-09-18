//! One lane for every scan a running server performs.
//!
//! Scans walk the filesystem and write the catalog, so two of them over the same source
//! duplicate the work and race each other's derived counts. Every producer under `serve`
//! submits a [`ScanRequest`] here instead of spawning: the lane runs them one at a time in
//! the order asked, and a request for a source set that is still waiting merges into it.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use hoard_core::JobId;

use crate::catalog::{Catalog, HashProgress, JobProgress, RescanReport, RescanScope, Stop};
use crate::config::{HashPolicy, IndexConfig};
use crate::error::{IndexError, Result};

/// The `job.kind` every scan the lane runs records.
pub const SCAN_KIND: &str = "scan";

/// What a job whose pass the closing lane stopped carries in place of its last phase.
const INTERRUPTED: &str = "interrupted by shutdown; resumes on the next scan";

/// What a job that never started carries when the closing lane drops it.
const DROPPED: &str = "dropped when the lane closed; resumes on the next scan";

/// The phase a job carries while its files are read for their content hashes.
const HASHING: &str = "hashing";

/// How often the hashing phase is allowed to write its job row. Every write is a committed
/// transaction, and the pass reports each file it hands to a worker.
const JOB_UPDATE_EVERY: Duration = Duration::from_millis(250);

/// Who asked for a scan, which decides how it is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Manual,
    Watched,
    Reconcile,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Watched => "watched",
            Self::Reconcile => "reconcile",
        }
    }
}

/// One request: the sources to walk, every configured source when `None`, and the policy
/// the walk hashes under.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub sources: Option<BTreeSet<String>>,
    pub hash: HashPolicy,
    pub origin: Origin,
}

/// Runs every scan under `serve`, one at a time, in the order asked, merging requests that
/// name the same sources while one is still waiting.
#[derive(Debug)]
pub struct Coordinator {
    lane: Arc<Lane>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Coordinator {
    /// Prepares the lane. The task that runs it starts with the first request, so a
    /// coordinator can be built outside a tokio runtime and costs nothing until one.
    pub fn new(catalog: Arc<Catalog>, config: IndexConfig) -> Self {
        Self {
            lane: Arc::new(Lane {
                catalog,
                config,
                queue: Mutex::new(Queue::default()),
                wake: Notify::new(),
                admission: tokio::sync::Mutex::new(()),
                stop: Stop::default(),
                finished: AtomicU64::new(0),
            }),
            worker: Mutex::new(None),
        }
    }

    /// Queues a scan and answers the job it runs under at once.
    ///
    /// A request whose source set equals one already queued and not yet started answers
    /// that job's id, and the queued request takes the stronger of the two hash policies.
    /// A request naming a source the configuration does not have is refused before any job
    /// row is written.
    pub async fn request(&self, request: ScanRequest) -> Result<JobId> {
        let sources = self.lane.checked(request.sources)?;
        let _admission = self.lane.admission.lock().await;
        if let Some(merged) = self.lane.merge(sources.as_ref(), request.hash)? {
            return Ok(merged);
        }
        self.start();
        let catalog = Arc::clone(&self.lane.catalog);
        let subject = subject_of(sources.as_ref());
        let job =
            tokio::task::spawn_blocking(move || catalog.create_job(SCAN_KIND, subject.as_deref()))
                .await
                .map_err(lane_stopped)??;
        self.lane.enqueue(Queued {
            job,
            sources,
            hash: request.hash,
            origin: request.origin,
        })?;
        Ok(job)
    }

    fn start(&self) {
        let mut worker = self.worker.lock();
        if worker.is_none() {
            *worker = Some(tokio::spawn(run_lane(Arc::clone(&self.lane))));
        }
    }

    /// Whether a scan is running or queued.
    pub fn busy(&self) -> bool {
        let queue = self.lane.queue.lock();
        queue.running || !queue.waiting.is_empty()
    }

    /// How many scans the lane has run to their end since it was built, whatever the
    /// outcome, so a reader polling it can tell that one finished between two looks.
    pub fn finished(&self) -> u64 {
        self.lane.finished.load(Ordering::Acquire)
    }

    /// Stops the lane within a batch of work rather than within a whole pass.
    ///
    /// The running scan is asked to stop between units and before each file a hashing pass
    /// hands out, and keeps what it committed; what is still waiting is dropped with its
    /// job row cancelled. A request after this is refused at once rather than waiting for
    /// the stop, and the next start rescans what neither finished.
    pub async fn shutdown(&self) -> Result<()> {
        let dropped = {
            let _admission = self.lane.admission.lock().await;
            self.lane.stop.store(true, Ordering::Relaxed);
            let mut queue = self.lane.queue.lock();
            queue.closed = true;
            let dropped: Vec<JobId> = queue.waiting.drain(..).map(|item| item.job).collect();
            drop(queue);
            self.lane.wake.notify_one();
            dropped
        };
        self.lane.drop_jobs(dropped).await;
        let worker = self.worker.lock().take();
        match worker {
            Some(worker) => worker.await.map_err(lane_stopped),
            None => Ok(()),
        }
    }
}

#[derive(Debug)]
struct Lane {
    catalog: Arc<Catalog>,
    config: IndexConfig,
    queue: Mutex<Queue>,
    wake: Notify,
    admission: tokio::sync::Mutex<()>,
    stop: Stop,
    finished: AtomicU64,
}

#[derive(Debug, Default)]
struct Queue {
    waiting: VecDeque<Queued>,
    running: bool,
    closed: bool,
}

#[derive(Debug)]
struct Queued {
    job: JobId,
    sources: Option<BTreeSet<String>>,
    hash: HashPolicy,
    origin: Origin,
}

impl Lane {
    fn checked(&self, sources: Option<BTreeSet<String>>) -> Result<Option<BTreeSet<String>>> {
        let Some(names) = sources.filter(|names| !names.is_empty()) else {
            return Ok(None);
        };
        for name in &names {
            if self.config.source(name).is_none() {
                return Err(IndexError::Config(format!("no source named {name}")));
            }
        }
        Ok(Some(names))
    }

    fn merge(&self, sources: Option<&BTreeSet<String>>, hash: HashPolicy) -> Result<Option<JobId>> {
        let mut queue = self.queue.lock();
        if queue.closed {
            return Err(closed());
        }
        let waiting = queue
            .waiting
            .iter_mut()
            .find(|item| item.sources.as_ref() == sources);
        Ok(waiting.map(|waiting| {
            waiting.hash = stronger(waiting.hash, hash);
            waiting.job
        }))
    }

    fn enqueue(&self, item: Queued) -> Result<()> {
        let mut queue = self.queue.lock();
        if queue.closed {
            return Err(closed());
        }
        queue.waiting.push_back(item);
        drop(queue);
        self.wake.notify_one();
        Ok(())
    }

    async fn run(&self, item: Queued) {
        let job = item.job;
        let origin = item.origin.as_str();
        let catalog = Arc::clone(&self.catalog);
        let config = narrowed(&self.config, item.sources.as_ref());
        let stop = Stop::clone(&self.stop);
        tracing::info!(%job, origin, "scan started");
        match tokio::task::spawn_blocking(move || settle(&catalog, &config, &item, &stop)).await {
            Ok(Ok(report)) => tracing::info!(
                %job,
                origin,
                unchanged = report.unchanged,
                changed = report.changed,
                added = report.added,
                absent = report.absent,
                "scan finished"
            ),
            Ok(Err(error)) => tracing::error!(%job, origin, %error, "the scan failed"),
            Err(error) => self.abandon(job, &error.to_string()).await,
        }
    }

    /// Settles the job rows of the requests the closing lane will never run.
    async fn drop_jobs(&self, jobs: Vec<JobId>) {
        for job in jobs {
            let catalog = Arc::clone(&self.catalog);
            let settled = tokio::task::spawn_blocking(move || {
                catalog.update_job(
                    job,
                    &JobProgress {
                        phase: Some(DROPPED.to_owned()),
                        ..JobProgress::default()
                    },
                )?;
                catalog.cancel_job(job)
            })
            .await;
            match settled {
                Ok(Ok(())) => tracing::info!(%job, "the queued scan was dropped by the shutdown"),
                Ok(Err(error)) => {
                    tracing::error!(%job, %error, "the dropped job could not be settled")
                }
                Err(error) => {
                    tracing::error!(%job, %error, "the dropped job lost its worker thread")
                }
            }
        }
    }

    async fn abandon(&self, job: JobId, message: &str) {
        tracing::error!(%job, message, "the scan lane lost its worker thread");
        let catalog = Arc::clone(&self.catalog);
        let message = message.to_owned();
        let settled = tokio::task::spawn_blocking(move || catalog.fail_job(job, &message)).await;
        if let Ok(Err(error)) = settled {
            tracing::error!(%job, %error, "the abandoned job could not be settled");
        }
    }
}

async fn run_lane(lane: Arc<Lane>) {
    loop {
        let next = {
            let mut queue = lane.queue.lock();
            match queue.waiting.pop_front() {
                Some(item) => {
                    queue.running = true;
                    Some(item)
                }
                None => {
                    if queue.closed {
                        return;
                    }
                    None
                }
            }
        };
        match next {
            Some(item) => {
                lane.run(item).await;
                lane.finished.fetch_add(1, Ordering::Release);
                lane.queue.lock().running = false;
            }
            None => lane.wake.notified().await,
        }
    }
}

fn settle(
    catalog: &Catalog,
    config: &IndexConfig,
    item: &Queued,
    stop: &Stop,
) -> Result<RescanReport> {
    let outcome = run_scan(catalog, config, item, stop);
    let settled = match &outcome {
        Ok(_) => catalog.finish_job(item.job),
        Err(error) => catalog.fail_job(item.job, &error.to_string()),
    };
    if let Err(error) = settled {
        tracing::error!(job = %item.job, %error, "the scan job row could not be settled");
    }
    outcome
}

fn run_scan(
    catalog: &Catalog,
    config: &IndexConfig,
    item: &Queued,
    stop: &Stop,
) -> Result<RescanReport> {
    phase(catalog, item.job, "rescanning the sources")?;
    let scope = match item.sources {
        Some(_) => RescanScope::Narrowed,
        None => RescanScope::Configured,
    };
    let report = catalog.rescan_until(config, item.hash, scope, stop, |_, _| {})?;
    let hashing = match (item.hash, report.interrupted) {
        (HashPolicy::Now | HashPolicy::Background, false) => {
            phase(catalog, item.job, HASHING)?;
            let mut written = Instant::now();
            let mut finished = false;
            catalog
                .hash_pending_until(config, stop, |update| {
                    let due = written.elapsed() >= JOB_UPDATE_EVERY
                        || (!finished && update.files_done >= update.files_total);
                    if due {
                        written = Instant::now();
                        finished = update.files_done >= update.files_total;
                        hashing_progress(catalog, item.job, update);
                    }
                })?
                .interrupted
        }
        (HashPolicy::Now | HashPolicy::Background, true) | (HashPolicy::Never, _) => false,
    };
    phase(catalog, item.job, "refreshing search")?;
    let refreshed = catalog.refresh_search_reporting(|_| {})?;
    if refreshed.applied > 0 {
        tracing::info!(
            job = %item.job,
            applied = refreshed.applied,
            documents = refreshed.documents,
            bytes = refreshed.bytes,
            commits = refreshed.commits,
            "search refreshed"
        );
    }
    if report.interrupted || hashing {
        phase(catalog, item.job, INTERRUPTED)?;
    }
    Ok(report)
}

fn phase(catalog: &Catalog, job: JobId, phase: &str) -> Result<()> {
    catalog.update_job(
        job,
        &JobProgress {
            phase: Some(phase.to_owned()),
            ..JobProgress::default()
        },
    )
}

fn hashing_progress(catalog: &Catalog, job: JobId, update: HashProgress) {
    let written = catalog.update_job(
        job,
        &JobProgress {
            phase: Some(HASHING.to_owned()),
            progress: (update.files_done as f32 / update.files_total.max(1) as f32).min(1.0),
            bytes: update.bytes_done,
            total: Some(update.bytes_total),
            rate_bps: update.rate_bps,
        },
    );
    if let Err(error) = written {
        tracing::error!(%job, %error, "the hashing job row could not be updated");
    }
}

fn narrowed(config: &IndexConfig, sources: Option<&BTreeSet<String>>) -> IndexConfig {
    match sources {
        Some(names) => config.over(
            names
                .iter()
                .filter_map(|name| config.source(name))
                .cloned()
                .collect(),
        ),
        None => config.clone(),
    }
}

fn subject_of(sources: Option<&BTreeSet<String>>) -> Option<String> {
    sources.map(|names| {
        names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn strength(policy: HashPolicy) -> u8 {
    match policy {
        HashPolicy::Now => 2,
        HashPolicy::Background => 1,
        HashPolicy::Never => 0,
    }
}

fn stronger(left: HashPolicy, right: HashPolicy) -> HashPolicy {
    std::cmp::max_by_key(right, left, |policy| strength(*policy))
}

fn closed() -> IndexError {
    IndexError::Catalog("the scan lane is closed".into())
}

fn lane_stopped(error: tokio::task::JoinError) -> IndexError {
    IndexError::Catalog(format!("the scan lane stopped badly: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tempfile::TempDir;

    use crate::catalog::JobState;
    use crate::config::{SourceConfig, SourceKind};

    use super::*;

    fn source(path: &Path, name: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_owned(),
            path: path.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        }
    }

    fn world() -> Result<(TempDir, Arc<Catalog>, IndexConfig)> {
        let directory = tempfile::tempdir()?;
        let alpha = directory.path().join("alpha");
        let beta = directory.path().join("beta");
        fs::create_dir(&alpha)?;
        fs::create_dir(&beta)?;
        fs::write(alpha.join("one.csv"), "prompt,response\nhello,world\n")?;
        fs::write(beta.join("two.jsonl"), "{\"text\":\"hello\"}\n")?;
        let config = IndexConfig {
            sources: vec![source(&alpha, "alpha"), source(&beta, "beta")],
            ..IndexConfig::default()
        };
        let catalog = Catalog::scan_sources(
            &directory.path().join("native"),
            "test-node",
            &config,
            HashPolicy::Never,
            |_, _| {},
        )?;
        catalog.rebuild_search(|_| {})?;
        Ok((directory, Arc::new(catalog), config))
    }

    fn over(name: &str, hash: HashPolicy) -> ScanRequest {
        ScanRequest {
            sources: Some(BTreeSet::from([name.to_owned()])),
            hash,
            origin: Origin::Manual,
        }
    }

    fn hold_the_search_lane(
        catalog: Arc<Catalog>,
        pause: Duration,
    ) -> (
        std::thread::JoinHandle<Result<()>>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (sender, held) = tokio::sync::oneshot::channel();
        let holder = std::thread::spawn(move || {
            let mut sender = Some(sender);
            catalog.rebuild_search(|_| {
                if let Some(sender) = sender.take() {
                    sender.send(()).ok();
                }
                std::thread::sleep(pause);
            })
        });
        (holder, held)
    }

    async fn released(holder: std::thread::JoinHandle<Result<()>>) -> Result<()> {
        tokio::task::spawn_blocking(move || holder.join())
            .await
            .map_err(lane_stopped)?
            .map_err(|_| IndexError::Catalog("the search lane holder panicked".into()))?
    }

    fn pending_hashes(catalog: &Catalog, name: &str) -> Result<u64> {
        catalog
            .sources()?
            .into_iter()
            .find(|record| record.name == name)
            .map(|record| record.pending_hashes)
            .ok_or_else(|| IndexError::Catalog(format!("no source record for {name}")))
    }

    /// Waits for the lane to have taken a job, which the first phase it records proves, so
    /// a shutdown after this stops a scan that is running rather than one still waiting.
    async fn started(catalog: &Catalog, job: JobId) -> Result<()> {
        for _ in 0..600 {
            if settled(catalog, job)?.phase.is_some() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(IndexError::Catalog(format!("job {job} never started")))
    }

    /// Waits for the lane to run what it has, so a shutdown after this stops nothing.
    async fn drained(coordinator: &Coordinator) {
        for _ in 0..600 {
            if !coordinator.busy() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn wide(directory: &Path, files: usize) -> Result<PathBuf> {
        let root = directory.join("wide");
        fs::create_dir(&root)?;
        for index in 0..files {
            fs::write(
                root.join(format!("shard{index:05}.csv")),
                format!("prompt,response\nhello,{index}\n"),
            )?;
        }
        Ok(root)
    }

    fn settled(catalog: &Catalog, job: JobId) -> Result<crate::catalog::JobRecord> {
        catalog
            .job(job)?
            .ok_or_else(|| IndexError::Catalog(format!("job {job} was not written")))
    }

    #[test]
    fn a_hashing_update_leaves_the_job_row_naming_its_phase_and_fraction() -> Result<()> {
        let (_directory, catalog, _config) = world()?;
        let job = catalog.create_job(SCAN_KIND, None)?;

        hashing_progress(
            &catalog,
            job,
            HashProgress {
                files_done: 3,
                files_total: 4,
                bytes_done: 4096,
                bytes_total: 16_384,
                rate_bps: Some(512),
            },
        );

        let record = settled(&catalog, job)?;
        assert_eq!(record.phase.as_deref(), Some(HASHING));
        assert_eq!(record.progress, 0.75);
        assert_eq!(record.bytes, 4096);
        assert_eq!(record.rate_bps, Some(512));
        assert_eq!(
            record.total,
            Some(16_384),
            "the job row carries the bytes the pass owes"
        );
        assert_eq!(record.state, JobState::Running);

        hashing_progress(
            &catalog,
            job,
            HashProgress {
                files_done: 9,
                files_total: 4,
                bytes_done: 8192,
                bytes_total: 16_384,
                rate_bps: None,
            },
        );

        let over = settled(&catalog, job)?;
        assert_eq!(
            over.progress, 1.0,
            "a pass that finds more files than it was owed still reports a full fraction"
        );
        assert_eq!(
            over.rate_bps, None,
            "a tick with no rate clears the last one"
        );
        Ok(())
    }

    #[test]
    fn a_hashing_update_of_nothing_owed_reports_no_fraction_rather_than_dividing_by_zero()
    -> Result<()> {
        let (_directory, catalog, _config) = world()?;
        let job = catalog.create_job(SCAN_KIND, None)?;

        hashing_progress(&catalog, job, HashProgress::default());

        assert_eq!(settled(&catalog, job)?.progress, 0.0);
        Ok(())
    }

    #[test]
    fn the_stronger_hash_policy_wins_a_merge() {
        assert_eq!(
            stronger(HashPolicy::Never, HashPolicy::Now),
            HashPolicy::Now
        );
        assert_eq!(
            stronger(HashPolicy::Now, HashPolicy::Never),
            HashPolicy::Now
        );
        assert_eq!(
            stronger(HashPolicy::Background, HashPolicy::Now),
            HashPolicy::Now
        );
        assert_eq!(
            stronger(HashPolicy::Never, HashPolicy::Background),
            HashPolicy::Background
        );
        assert_eq!(
            stronger(HashPolicy::Never, HashPolicy::Never),
            HashPolicy::Never
        );
    }

    #[test]
    fn a_coordinator_is_built_outside_a_runtime_and_starts_idle() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Coordinator::new(Arc::clone(&catalog), config);
        assert!(
            !coordinator.busy(),
            "an unstarted lane has nothing in flight"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_for_a_queued_source_merges_into_it() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Coordinator::new(Arc::clone(&catalog), config);
        let (holder, held) = hold_the_search_lane(Arc::clone(&catalog), Duration::from_millis(200));
        held.await
            .map_err(|_| IndexError::Catalog("the search lane was never taken".into()))?;

        let running = coordinator
            .request(over("alpha", HashPolicy::Never))
            .await?;
        let queued = coordinator.request(over("beta", HashPolicy::Never)).await?;
        let merged = coordinator.request(over("beta", HashPolicy::Now)).await?;
        assert_eq!(
            queued, merged,
            "a second request for a waiting source set answers the job it merged into"
        );
        assert_ne!(running, queued, "a different source set queues its own job");

        drained(&coordinator).await;
        coordinator.shutdown().await?;
        released(holder).await?;

        assert_eq!(
            catalog.list_jobs(None)?.len(),
            2,
            "the merged request wrote no job row of its own"
        );
        assert_eq!(settled(&catalog, running)?.state, JobState::Done);
        assert_eq!(settled(&catalog, queued)?.state, JobState::Done);
        assert_eq!(
            pending_hashes(&catalog, "beta")?,
            0,
            "the merge raised the queued policy above Never, so the run hashed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_scan_that_ran_is_counted_by_the_lane() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = IndexConfig {
            data_dir: directory.path().join("data"),
            sources: Vec::new(),
            ..IndexConfig::default()
        };
        let catalog = Arc::new(Catalog::open_or_scan(&config, "test-node", |_, _| {})?);
        let coordinator = Coordinator::new(catalog, config);
        assert_eq!(
            coordinator.finished(),
            0,
            "a lane that never ran counts none"
        );

        let everything = || ScanRequest {
            sources: None,
            hash: HashPolicy::Never,
            origin: Origin::Manual,
        };
        coordinator.request(everything()).await?;
        drained(&coordinator).await;
        assert_eq!(coordinator.finished(), 1);

        coordinator.request(everything()).await?;
        drained(&coordinator).await;
        coordinator.request(everything()).await?;
        drained(&coordinator).await;
        assert_eq!(coordinator.finished(), 3);
        coordinator.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn requests_run_in_the_order_asked_and_settle_their_jobs() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Coordinator::new(Arc::clone(&catalog), config);
        let alpha = coordinator
            .request(over("alpha", HashPolicy::Background))
            .await?;
        let beta = coordinator.request(over("beta", HashPolicy::Never)).await?;
        let everything = coordinator
            .request(ScanRequest {
                sources: None,
                hash: HashPolicy::Now,
                origin: Origin::Reconcile,
            })
            .await?;
        drained(&coordinator).await;
        coordinator.shutdown().await?;

        assert!(!coordinator.busy(), "a drained lane is idle");
        assert!(
            alpha.get() < beta.get() && beta.get() < everything.get(),
            "job rows are numbered in the order the requests were asked"
        );
        for job in [alpha, beta, everything] {
            let record = settled(&catalog, job)?;
            assert_eq!(record.state, JobState::Done);
            assert!(record.finished.is_some());
            assert_eq!(record.phase.as_deref(), Some("refreshing search"));
            assert_eq!(record.kind, SCAN_KIND);
        }
        assert_eq!(settled(&catalog, alpha)?.subject.as_deref(), Some("alpha"));
        assert_eq!(settled(&catalog, everything)?.subject, None);
        Ok(())
    }

    #[tokio::test]
    async fn a_failing_scan_fails_its_job_and_the_lane_continues() -> Result<()> {
        let (directory, catalog, config) = world()?;
        let broken = directory.path().join("broken.txt");
        fs::write(&broken, "a source root that is not a directory")?;
        let mut widened = config.clone();
        widened.sources.push(source(&broken, "broken"));
        let coordinator = Coordinator::new(Arc::clone(&catalog), widened);

        let failing = coordinator
            .request(over("broken", HashPolicy::Never))
            .await?;
        let following = coordinator
            .request(over("alpha", HashPolicy::Never))
            .await?;
        drained(&coordinator).await;
        coordinator.shutdown().await?;

        let failed = settled(&catalog, failing)?;
        assert_eq!(failed.state, JobState::Failed);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|text| text.contains("is not a directory")),
            "the job carries the error that stopped it, not a bare failure"
        );
        assert_eq!(
            settled(&catalog, following)?.state,
            JobState::Done,
            "the lane runs the next request after one fails"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_request_naming_an_unknown_source_is_refused_before_a_job_exists() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Coordinator::new(Arc::clone(&catalog), config);
        let refused = coordinator
            .request(over("nowhere", HashPolicy::Never))
            .await;
        assert!(
            matches!(refused, Err(IndexError::Config(ref text)) if text == "no source named nowhere"),
            "an unknown source is refused, got {refused:?}"
        );
        assert!(
            catalog.list_jobs(None)?.is_empty(),
            "a refused request writes no job row"
        );
        coordinator.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_request_during_a_shutdown_is_refused_without_waiting_for_it() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config));
        let (holder, held) = hold_the_search_lane(Arc::clone(&catalog), Duration::from_millis(400));
        held.await
            .map_err(|_| IndexError::Catalog("the search lane was never taken".into()))?;

        let running = coordinator
            .request(over("alpha", HashPolicy::Never))
            .await?;
        let queued = coordinator.request(over("beta", HashPolicy::Never)).await?;
        started(&catalog, running).await?;

        let asking = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let asked = std::time::Instant::now();
                let outcome = coordinator.request(over("alpha", HashPolicy::Never)).await;
                (outcome, asked.elapsed())
            }
        });

        let started = std::time::Instant::now();
        coordinator.shutdown().await?;
        let stopping = started.elapsed();
        released(holder).await?;
        let (refused, waited) = asking.await.map_err(lane_stopped)?;

        assert!(
            matches!(refused, Err(IndexError::Catalog(ref text)) if text == "the scan lane is closed"),
            "a request during a shutdown is refused, got {refused:?}"
        );
        assert!(
            waited < Duration::from_millis(100),
            "the refusal answered in {waited:?} rather than parking on the shutdown"
        );
        assert!(
            stopping > Duration::from_millis(300),
            "the shutdown this refusal did not wait for took {stopping:?}"
        );
        assert_eq!(settled(&catalog, running)?.state, JobState::Done);
        assert_eq!(
            settled(&catalog, queued)?.state,
            JobState::Cancelled,
            "what had not started was dropped"
        );
        assert_eq!(
            catalog.list_jobs(None)?.len(),
            2,
            "the refused request wrote no job row"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_finishes_the_running_scan_drops_what_waits_and_refuses_what_comes_after()
    -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Coordinator::new(Arc::clone(&catalog), config);
        let (holder, held) = hold_the_search_lane(Arc::clone(&catalog), Duration::from_millis(200));
        held.await
            .map_err(|_| IndexError::Catalog("the search lane was never taken".into()))?;

        let running = coordinator
            .request(over("alpha", HashPolicy::Never))
            .await?;
        let queued = coordinator.request(over("beta", HashPolicy::Never)).await?;
        assert!(coordinator.busy(), "both requests are still in flight");
        started(&catalog, running).await?;

        coordinator.shutdown().await?;
        released(holder).await?;

        assert!(!coordinator.busy(), "shutdown emptied the queue");
        assert_eq!(
            settled(&catalog, running)?.state,
            JobState::Done,
            "the scan already running was finished"
        );
        let dropped = settled(&catalog, queued)?;
        assert_eq!(
            dropped.state,
            JobState::Cancelled,
            "shutdown dropped what was still waiting"
        );
        assert_eq!(dropped.phase.as_deref(), Some(DROPPED));

        let refused = coordinator.request(over("alpha", HashPolicy::Never)).await;
        assert!(
            matches!(refused, Err(IndexError::Catalog(ref text)) if text == "the scan lane is closed"),
            "a request after shutdown is refused, got {refused:?}"
        );
        assert_eq!(
            catalog.list_jobs(None)?.len(),
            2,
            "the refused request wrote no job row"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_stops_the_hashing_pass_within_a_queue_of_files_and_the_next_scan_finishes_it()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = wide(directory.path(), 1500)?;
        let config = IndexConfig {
            sources: vec![source(&root, "wide")],
            ..IndexConfig::default()
        };
        let catalog = Arc::new(Catalog::scan_sources(
            &directory.path().join("native"),
            "test-node",
            &config,
            HashPolicy::Never,
            |_, _| {},
        )?);
        catalog.rebuild_search(|_| {})?;
        let owed = catalog.pending_hashes()?;
        assert_eq!(owed, 1500, "the scan hashed nothing");

        let coordinator = Coordinator::new(Arc::clone(&catalog), config.clone());
        let reconcile = ScanRequest {
            sources: None,
            hash: HashPolicy::Background,
            origin: Origin::Reconcile,
        };
        let job = coordinator.request(reconcile.clone()).await?;
        let asked = std::time::Instant::now();
        coordinator.shutdown().await?;
        let stopping = asked.elapsed();

        assert!(
            stopping < Duration::from_secs(5),
            "the shutdown waited {stopping:?} for the hashing pass"
        );
        assert!(
            settled(&catalog, job)?.state.is_final(),
            "the interrupted job settled rather than staying in flight"
        );

        let resumed = Coordinator::new(Arc::clone(&catalog), config);
        let next = resumed.request(reconcile).await?;
        drained(&resumed).await;
        resumed.shutdown().await?;
        assert_eq!(settled(&catalog, next)?.state, JobState::Done);
        assert_eq!(
            catalog.pending_hashes()?,
            0,
            "the next scan hashed whatever the shutdown left"
        );
        Ok(())
    }
}
