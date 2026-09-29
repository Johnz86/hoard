//! Incremental rescans driven by filesystem events.
//!
//! `notify` reports paths, not artifacts. Each reported path is mapped back to the source
//! that holds it, the names are coalesced over a debounce window, and one scan request per
//! burst is submitted to the lane every other producer under `serve` submits to. The lane
//! runs it as any other scan: unchanged files short-circuit on their stamp, so only what
//! actually changed is read again. Nothing is duplicated here, which matters because a loose
//! folder's revision is named by the fingerprints of everything in it and cannot be updated
//! one file at a time.
//!
//! A watch is best-effort by nature: a kernel queue overflows, a watch descriptor runs out,
//! a source root is not there. None of those may lose a change, so each one marks the
//! watched sources dirty and the next request covers every source rather than the ones an
//! event happened to name.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};

use crate::config::{IndexConfig, SourceConfig, source_for_path};
use crate::coordinate::{Coordinator, Origin, ScanRequest};
use crate::error::{IndexError, Result};

/// How long a burst of events is collected before the sources it touched are rescanned.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(750);

/// How many debounce windows a burst may extend across before the rescan runs anyway.
///
/// Copying a large model in produces events continuously for as long as the copy takes, so
/// a window that only ever restarts would never close and nothing would be indexed until
/// the copy finished.
const MAX_DEBOUNCE_WINDOWS: u32 = 20;

/// How many unhandled signals the watcher buffers.
///
/// An unbounded queue lets a runaway writer grow the process without limit. Overflow costs
/// nothing in practice: a burst large enough to fill this has already named every source it
/// touches many times over, and a burst that lost a signal reconciles every source anyway.
const EVENT_CAPACITY: usize = 4096;

/// What a watcher is told beyond the sources: how long a burst is collected, the roots whose
/// events are Hoard's own writes, and how often to reconcile without an event.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub debounce: Duration,
    pub excluded: Vec<PathBuf>,
    /// The reconciliation period. The first reconciliation runs at once, which is what
    /// indexes whatever changed while this node was not running.
    pub reconcile_every: Option<Duration>,
}

/// What the watcher can report about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchStatus {
    pub watching: Vec<PathBuf>,
    /// Roots the platform refused to watch. Manual scans still index them, and this is the
    /// only thing that makes a watching node degraded.
    pub failed: Vec<WatchFailure>,
    /// Configured roots that are not directories. A scan skips these with a warning too, so
    /// a node that cannot watch one is no worse off than a node that scans by hand.
    pub missing: Vec<PathBuf>,
    pub last_overflow: Option<Timestamp>,
    pub last_reconcile: Option<Timestamp>,
}

/// A root this node meant to watch and the platform refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WatchFailure {
    pub path: PathBuf,
    pub error: String,
}

/// Watches every configured source root and submits what changes to the scan lane.
///
/// The watcher must be created inside a tokio runtime; it holds a background task that lives
/// until [`Watcher::shutdown`] is called or the watcher is dropped.
#[derive(Debug)]
pub struct Watcher {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    failed: Vec<WatchFailure>,
    missing: Vec<PathBuf>,
    task: JoinHandle<()>,
    submitted: Arc<AtomicU64>,
    marks: Arc<Mutex<Marks>>,
}

impl Watcher {
    /// Starts watching every source of `config` that is a directory this platform can watch.
    ///
    /// A root that is neither is reported by [`Watcher::status`] rather than failing the
    /// call: the rest of the sources are still worth watching, and a watcher with none at
    /// all still reconciles on its interval.
    ///
    /// Every root, watched or excluded, is resolved to the absolute path an event will carry
    /// first, so a data directory spelled relative to the working directory is still
    /// recognised as Hoard's own.
    pub fn start(
        coordinator: Arc<Coordinator>,
        config: &IndexConfig,
        options: WatchOptions,
    ) -> Result<Self> {
        Self::start_with_reconciliation(coordinator, config, options, true)
    }

    /// Registers the current roots after a source change; the caller queues the new root's
    /// first scan, so periodic reconciliation begins after its normal interval.
    pub fn restart(
        coordinator: Arc<Coordinator>,
        config: &IndexConfig,
        options: WatchOptions,
    ) -> Result<Self> {
        Self::start_with_reconciliation(coordinator, config, options, false)
    }

    fn start_with_reconciliation(
        coordinator: Arc<Coordinator>,
        config: &IndexConfig,
        options: WatchOptions,
        reconcile_immediately: bool,
    ) -> Result<Self> {
        let WatchOptions {
            debounce,
            excluded,
            reconcile_every,
        } = options;
        let (sender, receiver) = channel(EVENT_CAPACITY);
        let lost = Arc::new(AtomicBool::new(false));
        let dispatch = Dispatch {
            events: sender,
            excluded: resolved(excluded.iter().map(PathBuf::as_path)),
            lost: Arc::clone(&lost),
        };
        let mut watcher = notify::recommended_watcher(move |event| dispatch.forward(event))
            .map_err(watch_error)?;

        let config = config.over(
            config
                .sources
                .iter()
                .map(|source| SourceConfig {
                    path: rooted(&source.path),
                    ..source.clone()
                })
                .collect(),
        );
        let mut roots = Vec::with_capacity(config.sources.len());
        let mut failed = Vec::new();
        let mut missing = Vec::new();
        for source in &config.sources {
            if !source.path.is_dir() {
                tracing::warn!(path = %source.path.display(), "watched source is missing");
                missing.push(source.path.clone());
                continue;
            }
            match watcher.watch(&source.path, RecursiveMode::Recursive) {
                Ok(()) => roots.push(source.path.clone()),
                Err(error) => {
                    tracing::warn!(
                        path = %source.path.display(),
                        %error,
                        "this source cannot be watched; manual scans still index it"
                    );
                    failed.push(WatchFailure {
                        path: source.path.clone(),
                        error: error.to_string(),
                    });
                }
            }
        }

        let submitted = Arc::new(AtomicU64::new(0));
        let marks = Arc::new(Mutex::new(Marks::default()));
        let task = tokio::spawn(
            Pump {
                coordinator,
                config,
                debounce,
                reconcile_every,
                reconcile_immediately,
                submitted: Arc::clone(&submitted),
                lost,
                marks: Arc::clone(&marks),
            }
            .run(receiver),
        );
        Ok(Self {
            watcher,
            roots,
            failed,
            missing,
            task,
            submitted,
            marks,
        })
    }

    /// How many requests this watcher has submitted to the lane.
    pub fn submitted(&self) -> u64 {
        self.submitted.load(Ordering::Relaxed)
    }

    /// What is watched, what could not be, and when the last overflow and reconciliation
    /// happened.
    pub fn status(&self) -> WatchStatus {
        let marks = self.marks.lock();
        WatchStatus {
            watching: self.roots.clone(),
            failed: self.failed.clone(),
            missing: self.missing.clone(),
            last_overflow: marks.last_overflow,
            last_reconcile: marks.last_reconcile,
        }
    }

    /// Stops watching and waits for the pump to exit; a request already submitted is left
    /// to the lane, which the caller stops.
    pub async fn shutdown(self) {
        let Self {
            mut watcher,
            roots,
            task,
            ..
        } = self;
        for root in &roots {
            if let Err(error) = watcher.unwatch(root) {
                tracing::debug!(path = %root.display(), %error, "root was already unwatched");
            }
        }
        drop(watcher);
        if let Err(error) = task.await {
            tracing::warn!(%error, "the watch pump did not stop cleanly");
        }
    }
}

#[derive(Debug, Default)]
struct Marks {
    last_overflow: Option<Timestamp>,
    last_reconcile: Option<Timestamp>,
}

#[derive(Debug)]
enum Signal {
    Changed(PathBuf),
    Overflow,
}

struct Dispatch {
    events: Sender<Signal>,
    excluded: Vec<PathBuf>,
    lost: Arc<AtomicBool>,
}

impl Dispatch {
    fn forward(&self, event: notify::Result<notify::Event>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "a watch failed; the sources will be reconciled");
                self.send(Signal::Overflow);
                return;
            }
        };
        if event.need_rescan() {
            self.send(Signal::Overflow);
            return;
        }
        if !mutates(event.kind) {
            return;
        }
        for path in event.paths {
            if self.owned_by_hoard(&path) {
                continue;
            }
            self.send(Signal::Changed(path));
        }
    }

    fn send(&self, signal: Signal) {
        if self.events.try_send(signal).is_err() {
            self.lose();
        }
    }

    fn lose(&self) {
        if !self.lost.swap(true, Ordering::Relaxed) {
            tracing::warn!("a watch signal was lost; the next burst reconciles every source");
        }
    }

    fn owned_by_hoard(&self, path: &Path) -> bool {
        self.excluded.iter().any(|root| path.starts_with(root))
    }
}

struct Pump {
    coordinator: Arc<Coordinator>,
    config: IndexConfig,
    debounce: Duration,
    reconcile_every: Option<Duration>,
    reconcile_immediately: bool,
    submitted: Arc<AtomicU64>,
    lost: Arc<AtomicBool>,
    marks: Arc<Mutex<Marks>>,
}

enum Wake {
    Signal(Option<Signal>),
    Interval,
}

impl Pump {
    async fn run(self, mut events: Receiver<Signal>) {
        let ceiling = self.debounce.saturating_mul(MAX_DEBOUNCE_WINDOWS);
        let mut timer = self
            .reconcile_every
            .map(|period| reconciliation_timer(period, self.reconcile_immediately));
        loop {
            let woken = match timer.as_mut() {
                Some(timer) => tokio::select! {
                    signal = events.recv() => Wake::Signal(signal),
                    _ = timer.tick() => Wake::Interval,
                },
                None => Wake::Signal(events.recv().await),
            };
            match woken {
                Wake::Signal(None) => return,
                Wake::Interval => {
                    self.lost.swap(false, Ordering::Relaxed);
                    self.marks.lock().last_reconcile = Some(Timestamp::now());
                    self.submit(everything(&self.config)).await;
                }
                Wake::Signal(Some(signal)) => {
                    let burst =
                        collect_burst(&self.config, &mut events, signal, self.debounce, ceiling)
                            .await;
                    let lost = self.lost.swap(false, Ordering::Relaxed);
                    match burst.overflowed || lost {
                        true => {
                            self.marks.lock().last_overflow = Some(Timestamp::now());
                            self.submit(everything(&self.config)).await;
                        }
                        false => self.submit(named(&self.config, burst.pending)).await,
                    }
                }
            }
        }
    }

    async fn submit(&self, request: Option<ScanRequest>) {
        let Some(request) = request else {
            return;
        };
        let origin = request.origin;
        match self.coordinator.request(request).await {
            Ok(job) => {
                self.submitted.fetch_add(1, Ordering::Relaxed);
                tracing::info!(%job, ?origin, "the watcher submitted a scan to the lane");
            }
            Err(error) => tracing::warn!(%error, ?origin, "the watcher's scan was refused"),
        }
    }
}

/// The request a burst that lost a signal asks for: every source, under the configured
/// policy, because what changed is no longer known.
fn everything(config: &IndexConfig) -> Option<ScanRequest> {
    Some(ScanRequest {
        sources: None,
        hash: config.hash_policy,
        origin: Origin::Reconcile,
    })
}

/// The request a burst that heard every event asks for: the sources it named, or nothing
/// when it named none of them.
fn named(config: &IndexConfig, pending: BTreeSet<String>) -> Option<ScanRequest> {
    match pending.is_empty() {
        true => None,
        false => Some(ScanRequest {
            sources: Some(pending),
            hash: config.hash_policy,
            origin: Origin::Watched,
        }),
    }
}

fn reconciliation_timer(period: Duration, immediate: bool) -> Interval {
    let start = tokio::time::Instant::now() + if immediate { Duration::ZERO } else { period };
    let mut timer = tokio::time::interval_at(start, period);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer
}

/// The absolute path an event under `root` will carry: symlinks resolved when it exists,
/// otherwise made absolute against the working directory. A root left as spelled never
/// prefixes what notify reports, so a relative one would exclude and name nothing.
fn rooted(root: &Path) -> PathBuf {
    if let Ok(canonical) = root.canonicalize() {
        return canonical;
    }
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

fn resolved<'a>(roots: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut resolved: Vec<PathBuf> = roots.into_iter().map(rooted).collect();
    resolved.sort_unstable();
    resolved.dedup();
    resolved
}

fn watch_error(error: notify::Error) -> IndexError {
    IndexError::Config(format!("filesystem watch failed: {error}"))
}

const fn mutates(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

#[derive(Debug, Default)]
struct Burst {
    pending: BTreeSet<String>,
    overflowed: bool,
}

/// What one burst of signals asks for: the sources it named, and whether it lost any.
///
/// The window closes when the signals stop for `debounce`, or when `window` has passed since
/// the first of them, whichever comes first. Without that ceiling a long copy would keep
/// restarting the debounce and no rescan would ever run while it was in progress.
async fn collect_burst(
    config: &IndexConfig,
    events: &mut Receiver<Signal>,
    first: Signal,
    debounce: Duration,
    window: Duration,
) -> Burst {
    let mut burst = Burst::default();
    absorb(config, first, &mut burst);
    let deadline = Instant::now() + window;
    loop {
        let remaining = debounce.min(deadline.saturating_duration_since(Instant::now()));
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(signal)) => absorb(config, signal, &mut burst),
            Ok(None) | Err(_) => return burst,
        }
    }
}

fn absorb(config: &IndexConfig, signal: Signal, burst: &mut Burst) {
    match signal {
        Signal::Changed(path) => note(config, &path, &mut burst.pending),
        Signal::Overflow => burst.overflowed = true,
    }
}

fn note(config: &IndexConfig, path: &Path, pending: &mut BTreeSet<String>) {
    if let Some(source) = source_for_path(config, path) {
        pending.insert(source.name.clone());
    }
}

/// The source names a burst of changed paths asks to be rescanned, each named once.
pub fn pending_sources<'a>(
    config: &IndexConfig,
    paths: impl IntoIterator<Item = &'a Path>,
) -> BTreeSet<String> {
    let mut pending = BTreeSet::new();
    for path in paths {
        note(config, path, &mut pending);
    }
    pending
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Instant;

    use notify::event::{CreateKind, Flag};
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::{Catalog, JobRecord};
    use crate::config::{HashPolicy, SourceKind};

    fn source(name: &str, path: &Path) -> SourceConfig {
        SourceConfig {
            name: name.to_owned(),
            path: path.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: true,
        }
    }

    fn options(debounce: Duration) -> WatchOptions {
        WatchOptions {
            debounce,
            excluded: Vec::new(),
            reconcile_every: None,
        }
    }

    fn safetensors() -> Vec<u8> {
        let header = br#"{"weight":{"dtype":"F16","shape":[2,2],"data_offsets":[0,8]}}"#;
        let mut file = Vec::with_capacity(8 + header.len() + 8);
        file.extend_from_slice(&(header.len() as u64).to_le_bytes());
        file.extend_from_slice(header);
        file.extend_from_slice(&[0u8; 8]);
        file
    }

    /// A catalog over one empty loose source, scanned before anything is written into it.
    fn watched_world(dir: &TempDir, hash: HashPolicy) -> (IndexConfig, Arc<Catalog>) {
        let root = dir.path().join("models");
        fs::create_dir_all(root.join("loras")).expect("source root is creatable");
        let config = IndexConfig {
            data_dir: dir.path().join("data"),
            hash_policy: hash,
            sources: vec![source("comfy", &root)],
            ..IndexConfig::default()
        };
        let catalog = Arc::new(
            Catalog::open_or_scan(&config, "watched", |_, _| {}).expect("the catalog opens"),
        );
        (config, catalog)
    }

    /// The same world over two sources, so a request over every source is distinguishable
    /// from one over the source an event named.
    fn two_source_world(dir: &TempDir) -> (IndexConfig, Arc<Catalog>) {
        let comfy = dir.path().join("comfy");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&comfy).expect("comfy is creatable");
        fs::create_dir_all(&vault).expect("vault is creatable");
        let config = IndexConfig {
            data_dir: dir.path().join("data"),
            hash_policy: HashPolicy::Never,
            sources: vec![source("comfy", &comfy), source("vault", &vault)],
            ..IndexConfig::default()
        };
        let catalog = Arc::new(
            Catalog::open_or_scan(&config, "watched", |_, _| {}).expect("the catalog opens"),
        );
        (config, catalog)
    }

    fn changed(path: &Path) -> notify::Result<notify::Event> {
        Ok(notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_path_buf()))
    }

    fn overflowed() -> notify::Result<notify::Event> {
        Ok(notify::Event::new(EventKind::Other).set_flag(Flag::Rescan))
    }

    /// Runs the pump over signals a test dispatched, to completion, and answers the jobs the
    /// lane settled, oldest first, and how many requests reached it.
    async fn pumped(
        config: &IndexConfig,
        catalog: &Arc<Catalog>,
        events: Receiver<Signal>,
        lost: Arc<AtomicBool>,
    ) -> (Vec<JobRecord>, u64) {
        let coordinator = Arc::new(Coordinator::new(Arc::clone(catalog), config.clone()));
        let submitted = Arc::new(AtomicU64::new(0));
        Pump {
            coordinator: Arc::clone(&coordinator),
            config: config.clone(),
            debounce: Duration::from_millis(20),
            reconcile_every: None,
            reconcile_immediately: true,
            submitted: Arc::clone(&submitted),
            lost,
            marks: Arc::new(Mutex::new(Marks::default())),
        }
        .run(events)
        .await;
        coordinator.shutdown().await.expect("the lane drains");
        let mut jobs = catalog.list_jobs(None).expect("the jobs read back");
        jobs.reverse();
        (jobs, submitted.load(Ordering::Relaxed))
    }

    async fn wait_for_submissions(watcher: &Watcher, wanted: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while watcher.submitted() < wanted {
            assert!(
                Instant::now() < deadline,
                "the watcher submitted {} of {wanted} scans",
                watcher.submitted()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Writes one artifact under the watched source and drains everything the burst
    /// submitted, leaving the catalog in the state the lane settled it to.
    async fn index_a_new_file(config: &IndexConfig, catalog: &Arc<Catalog>) {
        let coordinator = Arc::new(Coordinator::new(Arc::clone(catalog), config.clone()));
        let watcher = Watcher::start(
            Arc::clone(&coordinator),
            config,
            options(Duration::from_millis(50)),
        )
        .expect("the watcher starts");

        let root = &config.sources[0].path;
        fs::write(root.join("loras/new.safetensors"), safetensors()).expect("fixture is writable");
        wait_for_submissions(&watcher, 1).await;

        watcher.shutdown().await;
        for _ in 0..600 {
            if !coordinator.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        coordinator.shutdown().await.expect("the lane stops");
    }

    #[test]
    fn a_burst_of_paths_names_each_touched_source_once() {
        let dir = TempDir::new().expect("temp dir");
        let comfy = dir.path().join("comfy");
        let vault = dir.path().join("vault");
        let config = IndexConfig {
            sources: vec![source("comfy", &comfy), source("vault", &vault)],
            ..IndexConfig::default()
        };
        let paths = [
            comfy.join("loras/a.safetensors"),
            comfy.join("loras/b.safetensors"),
            vault.join("x.gguf"),
            PathBuf::from("/elsewhere/y.bin"),
        ];
        let pending = pending_sources(&config, paths.iter().map(PathBuf::as_path));
        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            ["comfy".to_owned(), "vault".to_owned()]
        );
    }

    #[test]
    fn a_lost_signal_asks_for_every_source_and_a_heard_one_for_what_it_named() {
        let config = IndexConfig {
            hash_policy: HashPolicy::Never,
            ..IndexConfig::default()
        };
        let lost = everything(&config).expect("a lost signal always asks for a scan");
        assert_eq!(lost.sources, None, "every configured source is walked");
        assert_eq!(lost.origin, Origin::Reconcile);
        assert_eq!(
            lost.hash,
            HashPolicy::Never,
            "the configured policy is kept"
        );

        let heard = named(&config, BTreeSet::from(["comfy".to_owned()]))
            .expect("a burst that named a source asks for it");
        assert_eq!(heard.sources, Some(BTreeSet::from(["comfy".to_owned()])));
        assert_eq!(heard.origin, Origin::Watched);
        assert!(
            named(&config, BTreeSet::new()).is_none(),
            "a burst outside every source asks for nothing"
        );
    }

    #[tokio::test]
    async fn an_unending_burst_still_closes_its_window() {
        let dir = TempDir::new().expect("temp dir");
        let comfy = dir.path().join("comfy");
        let config = IndexConfig {
            sources: vec![source("comfy", &comfy)],
            ..IndexConfig::default()
        };

        let debounce = Duration::from_millis(5);
        let window = debounce * MAX_DEBOUNCE_WINDOWS;
        let (sender, mut events) = channel(EVENT_CAPACITY);
        let path = comfy.join("loras/copying.safetensors");
        let writer = tokio::spawn({
            let path = path.clone();
            async move {
                while sender.send(Signal::Changed(path.clone())).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });

        let started = Instant::now();
        let burst = tokio::time::timeout(
            Duration::from_secs(10),
            collect_burst(
                &config,
                &mut events,
                Signal::Changed(path),
                debounce,
                window,
            ),
        )
        .await
        .expect("a burst that never pauses must still be acted on");
        let elapsed = started.elapsed();
        writer.abort();

        assert_eq!(
            burst.pending.into_iter().collect::<Vec<_>>(),
            ["comfy".to_owned()]
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the window closed after {elapsed:?}, so a long copy would never be indexed"
        );
    }

    #[tokio::test]
    async fn a_quiet_gap_closes_the_window_before_the_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let comfy = dir.path().join("comfy");
        let config = IndexConfig {
            sources: vec![source("comfy", &comfy)],
            ..IndexConfig::default()
        };

        let (sender, mut events) = channel(EVENT_CAPACITY);
        sender
            .send(Signal::Changed(comfy.join("loras/b.safetensors")))
            .await
            .expect("the queue accepts a path");
        drop(sender);

        let burst = collect_burst(
            &config,
            &mut events,
            Signal::Changed(comfy.join("loras/a.safetensors")),
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await;
        assert_eq!(
            burst.pending.into_iter().collect::<Vec<_>>(),
            ["comfy".to_owned()]
        );
        assert!(!burst.overflowed, "no signal was lost in this burst");
    }

    #[tokio::test]
    async fn an_overflow_inside_a_burst_is_carried_out_of_it() {
        let dir = TempDir::new().expect("temp dir");
        let comfy = dir.path().join("comfy");
        let config = IndexConfig {
            sources: vec![source("comfy", &comfy)],
            ..IndexConfig::default()
        };

        let (sender, mut events) = channel(EVENT_CAPACITY);
        sender
            .send(Signal::Overflow)
            .await
            .expect("the queue accepts a signal");
        drop(sender);

        let burst = collect_burst(
            &config,
            &mut events,
            Signal::Changed(comfy.join("loras/a.safetensors")),
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await;
        assert!(
            burst.overflowed,
            "an overflow that arrives mid-burst still reconciles rather than being swallowed"
        );
    }

    #[test]
    fn only_mutating_events_reach_the_pump() {
        assert!(mutates(EventKind::Create(CreateKind::File)));
        assert!(mutates(EventKind::Remove(notify::event::RemoveKind::File)));
        assert!(!mutates(EventKind::Access(
            notify::event::AccessKind::Open(notify::event::AccessMode::Read)
        )));
    }

    #[tokio::test]
    async fn a_new_file_is_indexed_without_a_manual_scan() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = watched_world(&dir, HashPolicy::Never);
        index_a_new_file(&config, &catalog).await;

        let totals = catalog.totals().expect("the catalog reads");
        assert!(
            totals.artifacts > 0,
            "the watched scan indexed the new file"
        );
        assert!(
            catalog.pending_hashes().expect("the catalog reads") > 0,
            "the watched scan ran under the configured policy rather than a forced one"
        );
    }

    #[tokio::test]
    async fn a_watched_rescan_hashes_under_the_background_policy() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = watched_world(&dir, HashPolicy::Background);
        index_a_new_file(&config, &catalog).await;

        let totals = catalog.totals().expect("the catalog reads");
        assert!(
            totals.artifacts > 0,
            "the watched scan indexed the new file"
        );
        assert_eq!(
            catalog.pending_hashes().expect("the catalog reads"),
            0,
            "a background policy hashes what the watched scan indexed"
        );
    }

    #[tokio::test]
    async fn an_overflow_event_reconciles_every_watched_source() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = two_source_world(&dir);
        let (sender, events) = channel(EVENT_CAPACITY);
        let lost = Arc::new(AtomicBool::new(false));
        let dispatch = Dispatch {
            events: sender,
            excluded: Vec::new(),
            lost: Arc::clone(&lost),
        };

        dispatch.forward(overflowed());
        dispatch.forward(changed(&config.sources[0].path.join("a.gguf")));
        drop(dispatch);
        let (jobs, submitted) = pumped(&config, &catalog, events, lost).await;

        assert_eq!(
            submitted, 1,
            "the burst behind the overflow is covered by the reconciliation, not a second scan"
        );
        assert_eq!(
            jobs[0].subject, None,
            "a subject of None is the scan over every configured source"
        );
    }

    #[tokio::test]
    async fn a_full_queue_marks_the_sources_dirty() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = two_source_world(&dir);
        let (sender, events) = channel(EVENT_CAPACITY);
        let lost = Arc::new(AtomicBool::new(false));
        let dispatch = Dispatch {
            events: sender,
            excluded: Vec::new(),
            lost: Arc::clone(&lost),
        };

        let path = config.sources[0].path.join("a.gguf");
        for _ in 0..=EVENT_CAPACITY {
            dispatch.forward(changed(&path));
        }
        assert!(
            lost.load(Ordering::Relaxed),
            "a queue filled past its capacity records that a signal was lost"
        );
        drop(dispatch);
        let (jobs, submitted) = pumped(&config, &catalog, events, lost).await;

        assert_eq!(submitted, 1, "the whole burst asked for one scan");
        assert_eq!(
            jobs[0].subject, None,
            "a burst that lost a signal walks every source rather than the one it heard"
        );
    }

    #[tokio::test]
    async fn an_event_under_an_excluded_root_submits_nothing() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = watched_world(&dir, HashPolicy::Never);
        let root = &config.sources[0].path;
        let excluded = root.join("hoard-data");
        fs::create_dir_all(&excluded).expect("the excluded root is creatable");

        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config.clone()));
        let debounce = Duration::from_millis(50);
        let watcher = Watcher::start(
            Arc::clone(&coordinator),
            &config,
            WatchOptions {
                debounce,
                excluded: vec![excluded.clone()],
                reconcile_every: None,
            },
        )
        .expect("the watcher starts");

        fs::write(excluded.join("catalog.redb"), b"hoard's own write").expect("write lands");
        tokio::time::sleep(debounce * 4).await;
        assert_eq!(
            watcher.submitted(),
            0,
            "Hoard's own catalog directory never asks for a rescan"
        );

        fs::write(root.join("loras/new.safetensors"), safetensors()).expect("fixture is writable");
        wait_for_submissions(&watcher, 1).await;
        watcher.shutdown().await;
        for _ in 0..600 {
            if !coordinator.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        coordinator.shutdown().await.expect("the lane stops");
    }

    #[tokio::test]
    async fn the_timer_reconciles_on_its_interval() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = watched_world(&dir, HashPolicy::Never);
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config.clone()));
        let watcher = Watcher::start(
            Arc::clone(&coordinator),
            &config,
            WatchOptions {
                debounce: Duration::from_millis(50),
                excluded: Vec::new(),
                reconcile_every: Some(Duration::from_millis(100)),
            },
        )
        .expect("the watcher starts");

        tokio::time::timeout(Duration::from_secs(1), wait_for_submissions(&watcher, 2))
            .await
            .expect("two reconciliations run within a second at a hundred millisecond period");
        assert!(
            watcher.status().last_reconcile.is_some(),
            "a reconciliation on the timer is reported"
        );

        watcher.shutdown().await;
        for _ in 0..600 {
            if !coordinator.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        coordinator.shutdown().await.expect("the lane stops");
    }

    #[tokio::test]
    async fn a_source_root_that_is_not_a_directory_is_reported_and_the_rest_are_watched() {
        let dir = TempDir::new().expect("temp dir");
        let (mut config, catalog) = watched_world(&dir, HashPolicy::Never);
        let not_a_directory = dir.path().join("a-file-not-a-directory");
        fs::write(&not_a_directory, b"not a source root").expect("the file is writable");
        config.sources.push(source("broken", &not_a_directory));

        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config.clone()));
        let watcher = Watcher::start(
            Arc::clone(&coordinator),
            &config,
            options(Duration::from_millis(50)),
        )
        .expect("a root that is not a directory does not fail the watcher");

        let status = watcher.status();
        assert_eq!(
            status.missing,
            vec![rooted(&not_a_directory)],
            "a root a scan would skip is reported as missing, not as a failure"
        );
        assert!(
            status.failed.is_empty(),
            "only a watch the platform refused is a failure"
        );
        assert_eq!(
            status.watching,
            vec![rooted(&config.sources[0].path)],
            "the source that can be watched still is"
        );

        fs::write(
            config.sources[0].path.join("loras/new.safetensors"),
            safetensors(),
        )
        .expect("fixture is writable");
        wait_for_submissions(&watcher, 1).await;
        watcher.shutdown().await;
        for _ in 0..600 {
            if !coordinator.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        coordinator.shutdown().await.expect("the lane stops");
    }

    #[test]
    fn a_root_spelled_relatively_resolves_to_what_an_event_will_carry() {
        let working = std::env::current_dir().expect("a working directory");
        assert_eq!(
            rooted(Path::new("./data/native")),
            working.join("data/native"),
            "a relative root would prefix nothing notify reports"
        );

        let dir = TempDir::new().expect("temp dir");
        let nested = dir.path().join("models/../hoard-data");
        fs::create_dir_all(dir.path().join("models")).expect("the source root is creatable");
        fs::create_dir_all(dir.path().join("hoard-data")).expect("the data root is creatable");
        assert_eq!(
            rooted(&nested),
            rooted(&dir.path().join("hoard-data")),
            "two spellings of one directory resolve to one root"
        );
    }

    #[tokio::test]
    async fn a_hoard_root_spelled_differently_is_still_excluded() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = watched_world(&dir, HashPolicy::Never);
        let root = config.sources[0].path.clone();
        let cache = root.join("mirror");
        fs::create_dir_all(cache.join("blocks")).expect("the cache root is creatable");

        let coordinator = Arc::new(Coordinator::new(Arc::clone(&catalog), config.clone()));
        let debounce = Duration::from_millis(50);
        let watcher = Watcher::start(
            Arc::clone(&coordinator),
            &config,
            WatchOptions {
                debounce,
                excluded: vec![root.join("loras/../mirror")],
                reconcile_every: None,
            },
        )
        .expect("the watcher starts");

        fs::write(cache.join("blocks/0.bin"), b"a block hoard cached").expect("write lands");
        tokio::time::sleep(debounce * 4).await;
        assert_eq!(
            watcher.submitted(),
            0,
            "a root spelled another way is still Hoard's own and must not ask for a scan"
        );

        fs::write(root.join("loras/new.safetensors"), safetensors()).expect("fixture is writable");
        wait_for_submissions(&watcher, 1).await;
        watcher.shutdown().await;
        for _ in 0..600 {
            if !coordinator.busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        coordinator.shutdown().await.expect("the lane stops");
    }

    #[tokio::test]
    async fn a_watch_error_reconciles_rather_than_waiting_for_the_next_change() {
        let dir = TempDir::new().expect("temp dir");
        let (config, catalog) = two_source_world(&dir);
        let (sender, events) = channel(EVENT_CAPACITY);
        let lost = Arc::new(AtomicBool::new(false));
        let dispatch = Dispatch {
            events: sender,
            excluded: Vec::new(),
            lost: Arc::clone(&lost),
        };

        dispatch.forward(Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch)));
        drop(dispatch);
        let (jobs, submitted) = pumped(&config, &catalog, events, lost).await;

        assert_eq!(
            submitted, 1,
            "a watch that could not be added wakes the pump rather than sitting in a flag"
        );
        assert_eq!(
            jobs[0].subject, None,
            "the reconciliation covers every source, including the one that was not watched"
        );
    }
}
