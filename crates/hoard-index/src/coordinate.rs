//! One lane for every scan a running server performs.
//!
//! Scans walk the filesystem and write the catalog, so two of them over the same source
//! duplicate the work and race each other's derived counts. Every producer under `serve`
//! submits a [`ScanRequest`] here instead of spawning: the lane runs them one at a time in
//! the order asked, and a request for a source set that is still waiting merges into it.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use hoard_core::JobId;

use crate::catalog::{Catalog, JobProgress, RescanReport, RescanScope};
use crate::config::{HashPolicy, IndexConfig};
use crate::error::{IndexError, Result};

/// The `job.kind` every scan the lane runs records.
pub const SCAN_KIND: &str = "scan";

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

    /// Runs what is queued to completion, then stops the lane. A request after this is
    /// refused as soon as the lane is marked closed, rather than waiting for the drain.
    pub async fn shutdown(&self) -> Result<()> {
        {
            let _admission = self.lane.admission.lock().await;
            self.lane.queue.lock().closed = true;
            self.lane.wake.notify_one();
        }
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
        tracing::info!(%job, origin, "scan started");
        match tokio::task::spawn_blocking(move || settle(&catalog, &config, &item)).await {
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
                lane.queue.lock().running = false;
            }
            None => lane.wake.notified().await,
        }
    }
}

fn settle(catalog: &Catalog, config: &IndexConfig, item: &Queued) -> Result<RescanReport> {
    let outcome = run_scan(catalog, config, item);
    let settled = match &outcome {
        Ok(_) => catalog.finish_job(item.job),
        Err(error) => catalog.fail_job(item.job, &error.to_string()),
    };
    if let Err(error) = settled {
        tracing::error!(job = %item.job, %error, "the scan job row could not be settled");
    }
    outcome
}

fn run_scan(catalog: &Catalog, config: &IndexConfig, item: &Queued) -> Result<RescanReport> {
    phase(catalog, item.job, "rescanning the sources")?;
    let scope = match item.sources {
        Some(_) => RescanScope::Narrowed,
        None => RescanScope::Configured,
    };
    let report = catalog.rescan(config, item.hash, scope, |_, _| {})?;
    match item.hash {
        HashPolicy::Now | HashPolicy::Background => {
            phase(catalog, item.job, "hashing")?;
            catalog.hash_pending(config, |_| {})?;
        }
        HashPolicy::Never => {}
    }
    phase(catalog, item.job, "refreshing search")?;
    catalog.refresh_search(|_| {})?;
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
    use std::path::Path;
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

    fn settled(catalog: &Catalog, job: JobId) -> Result<crate::catalog::JobRecord> {
        catalog
            .job(job)?
            .ok_or_else(|| IndexError::Catalog(format!("job {job} was not written")))
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
    async fn a_request_during_a_drain_is_refused_without_waiting_for_the_drain() -> Result<()> {
        let (_directory, catalog, config) = world()?;
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config));
        let (holder, held) = hold_the_search_lane(Arc::clone(&catalog), Duration::from_millis(400));
        held.await
            .map_err(|_| IndexError::Catalog("the search lane was never taken".into()))?;

        let running = coordinator
            .request(over("alpha", HashPolicy::Never))
            .await?;
        let queued = coordinator.request(over("beta", HashPolicy::Never)).await?;

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
        let drain = started.elapsed();
        released(holder).await?;
        let (refused, waited) = asking.await.map_err(lane_stopped)?;

        assert!(
            matches!(refused, Err(IndexError::Catalog(ref text)) if text == "the scan lane is closed"),
            "a request during a drain is refused, got {refused:?}"
        );
        assert!(
            waited < Duration::from_millis(100),
            "the refusal answered in {waited:?} rather than parking on the drain"
        );
        assert!(
            drain > Duration::from_millis(300),
            "the drain this refusal did not wait for took {drain:?}"
        );
        assert_eq!(settled(&catalog, running)?.state, JobState::Done);
        assert_eq!(
            settled(&catalog, queued)?.state,
            JobState::Done,
            "the drain still ran what was queued"
        );
        assert_eq!(
            catalog.list_jobs(None)?.len(),
            2,
            "the refused request wrote no job row"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_what_is_queued_and_refuses_what_comes_after() -> Result<()> {
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

        coordinator.shutdown().await?;
        released(holder).await?;

        assert!(!coordinator.busy(), "shutdown drained the queue");
        assert_eq!(settled(&catalog, running)?.state, JobState::Done);
        assert_eq!(
            settled(&catalog, queued)?.state,
            JobState::Done,
            "shutdown ran what was still waiting"
        );

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
}
