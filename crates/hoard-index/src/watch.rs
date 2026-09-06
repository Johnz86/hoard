//! Incremental rescans driven by filesystem events.
//!
//! `notify` reports paths, not artifacts. Each reported path is mapped back to the source
//! that holds it, the names are coalesced over a debounce window and each named source is
//! rescanned once. That rescan is the same pipeline a manual scan runs: unchanged files
//! short-circuit on their stamp, so only what actually changed is read again. Nothing is
//! duplicated here, which matters because a loose folder's revision is named by the
//! fingerprints of everything in it and cannot be updated one file at a time.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::task::JoinHandle;

use crate::catalog::Catalog;
use crate::config::{HashPolicy, IndexConfig, source_for_path};
use crate::error::{IndexError, Result};

/// How long a burst of events is collected before the sources it touched are rescanned.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(750);

/// How many debounce windows a burst may extend across before the rescan runs anyway.
///
/// Copying a large model in produces events continuously for as long as the copy takes, so
/// a window that only ever restarts would never close and nothing would be indexed until
/// the copy finished.
const MAX_DEBOUNCE_WINDOWS: u32 = 20;

/// How many unhandled paths the watcher buffers.
///
/// An unbounded queue lets a runaway writer grow the process without limit. Overflow costs
/// nothing in practice: a burst large enough to fill this has already named every source it
/// touches many times over, and each source is rescanned once per burst regardless.
const EVENT_CAPACITY: usize = 4096;

/// Watches every configured source root and rescans what changes.
///
/// The watcher must be created inside a tokio runtime; it holds a background task that
/// lives until [`Watcher::stop`] is called or the watcher is dropped.
#[derive(Debug)]
pub struct Watcher {
    watcher: Option<RecommendedWatcher>,
    roots: Vec<PathBuf>,
    task: JoinHandle<()>,
    rescans: Arc<AtomicU64>,
}

impl Watcher {
    /// Starts watching every source of `config` that exists on disk.
    pub fn start(catalog: Arc<Catalog>, config: &IndexConfig, debounce: Duration) -> Result<Self> {
        let (sender, receiver) = channel(EVENT_CAPACITY);
        let mut watcher = notify::recommended_watcher(move |event| forward(&sender, event))
            .map_err(watch_error)?;

        let mut roots = Vec::with_capacity(config.sources.len());
        for source in &config.sources {
            if !source.path.is_dir() {
                tracing::warn!(path = %source.path.display(), "watched source is missing");
                continue;
            }
            watcher
                .watch(&source.path, RecursiveMode::Recursive)
                .map_err(watch_error)?;
            roots.push(source.path.clone());
        }

        let rescans = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(pump(
            catalog,
            config.clone(),
            debounce,
            receiver,
            Arc::clone(&rescans),
        ));
        Ok(Self {
            watcher: Some(watcher),
            roots,
            task,
            rescans,
        })
    }

    /// Stops watching. Dropping the watcher does the same.
    ///
    /// A rescan already under way is left to finish rather than cancelled: it owns a
    /// `job` row, and a cancelled task never marks that row finished or failed, so the
    /// JOBS tab would show it running for ever.
    pub fn stop(&mut self) {
        let Some(mut watcher) = self.watcher.take() else {
            return;
        };
        for root in self.roots.drain(..) {
            if let Err(error) = watcher.unwatch(&root) {
                tracing::debug!(path = %root.display(), %error, "root was already unwatched");
            }
        }
    }

    /// True once the pump has drained the rescans it was asked for and exited.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// How many bursts this watcher has finished rescanning, counting the hashing and the
    /// search refresh each of them runs. Something waiting for a change to be indexed waits
    /// for this to move rather than for a row to appear part way through a pass.
    pub fn rescans(&self) -> u64 {
        self.rescans.load(Ordering::Relaxed)
    }
}

fn watch_error(error: notify::Error) -> IndexError {
    IndexError::Config(format!("filesystem watch failed: {error}"))
}

fn forward(sender: &Sender<PathBuf>, event: notify::Result<notify::Event>) {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            tracing::debug!(%error, "watch event dropped");
            return;
        }
    };
    if !mutates(event.kind) {
        return;
    }
    for path in event.paths {
        if sender.try_send(path).is_err() {
            tracing::debug!("the watch queue is full or closed; a path was dropped");
            return;
        }
    }
}

const fn mutates(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

async fn pump(
    catalog: Arc<Catalog>,
    config: IndexConfig,
    debounce: Duration,
    mut events: Receiver<PathBuf>,
    rescans: Arc<AtomicU64>,
) {
    let window = debounce.saturating_mul(MAX_DEBOUNCE_WINDOWS);
    while let Some(first) = events.recv().await {
        let pending = collect_burst(&config, &mut events, first, debounce, window).await;
        if pending.is_empty() {
            continue;
        }
        let narrowed = narrow(&config, &pending);
        let catalog = Arc::clone(&catalog);
        let named = pending.into_iter().collect::<Vec<_>>().join(", ");
        let done = tokio::task::spawn_blocking(move || rescan(&catalog, &narrowed)).await;
        match done {
            Ok(Ok(entries)) => {
                tracing::info!(sources = named, entries, "rescanned after a change")
            }
            Ok(Err(error)) => tracing::warn!(sources = named, %error, "watched rescan failed"),
            Err(error) => tracing::warn!(sources = named, %error, "watched rescan was lost"),
        }
        rescans.fetch_add(1, Ordering::Relaxed);
    }
}

/// The configuration a burst asks for: the same settings over only the sources it named.
fn narrow(config: &IndexConfig, pending: &BTreeSet<String>) -> IndexConfig {
    config.over(
        config
            .sources
            .iter()
            .filter(|source| pending.contains(&source.name))
            .cloned()
            .collect(),
    )
}

/// One rescan of the named sources, its hashing and the search refresh that publishes it.
/// Returns how many entries the walk touched.
fn rescan(catalog: &Catalog, config: &IndexConfig) -> Result<u64> {
    let report = catalog.rescan(config, HashPolicy::Background, |_, _| {})?;
    catalog.hash_pending(config, |_| {})?;
    catalog.refresh_search(|_| {})?;
    Ok(report.unchanged + report.changed + report.added + report.absent)
}

/// The sources one burst of events names.
///
/// The window closes when the events stop for `debounce`, or when `window` has passed since
/// the first of them, whichever comes first. Without that ceiling a long copy would keep
/// restarting the debounce and no rescan would ever run while it was in progress.
async fn collect_burst(
    config: &IndexConfig,
    events: &mut Receiver<PathBuf>,
    first: PathBuf,
    debounce: Duration,
    window: Duration,
) -> BTreeSet<String> {
    let mut pending = BTreeSet::new();
    note(config, &first, &mut pending);
    let deadline = Instant::now() + window;
    loop {
        let remaining = debounce.min(deadline.saturating_duration_since(Instant::now()));
        let Ok(Some(path)) = tokio::time::timeout(remaining, events.recv()).await else {
            return pending;
        };
        note(config, &path, &mut pending);
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

    use tempfile::TempDir;

    use super::*;
    use crate::config::{SourceConfig, SourceKind};

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

    fn safetensors() -> Vec<u8> {
        let header = br#"{"weight":{"dtype":"F16","shape":[2,2],"data_offsets":[0,8]}}"#;
        let mut file = Vec::with_capacity(8 + header.len() + 8);
        file.extend_from_slice(&(header.len() as u64).to_le_bytes());
        file.extend_from_slice(header);
        file.extend_from_slice(&[0u8; 8]);
        file
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
                while sender.send(path.clone()).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });

        let started = Instant::now();
        let pending = tokio::time::timeout(
            Duration::from_secs(10),
            collect_burst(&config, &mut events, path, debounce, window),
        )
        .await
        .expect("a burst that never pauses must still be acted on");
        let elapsed = started.elapsed();
        writer.abort();

        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
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
            .send(comfy.join("loras/b.safetensors"))
            .await
            .expect("the queue accepts a path");
        drop(sender);

        let pending = collect_burst(
            &config,
            &mut events,
            comfy.join("loras/a.safetensors"),
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await;
        assert_eq!(
            pending.into_iter().collect::<Vec<_>>(),
            ["comfy".to_owned()]
        );
    }

    #[test]
    fn only_mutating_events_reach_the_pump() {
        assert!(mutates(EventKind::Create(notify::event::CreateKind::File)));
        assert!(mutates(EventKind::Remove(notify::event::RemoveKind::File)));
        assert!(!mutates(EventKind::Access(
            notify::event::AccessKind::Open(notify::event::AccessMode::Read)
        )));
    }

    #[tokio::test]
    async fn a_new_file_is_indexed_without_a_manual_scan() {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("models");
        fs::create_dir_all(root.join("loras")).expect("source root is creatable");
        let config = IndexConfig {
            data_dir: dir.path().join("data"),
            sources: vec![source("comfy", &root)],
            ..IndexConfig::default()
        };
        let catalog = Arc::new(
            Catalog::open_or_scan(&config, "watched", |_, _| {}).expect("the catalog opens"),
        );

        let mut watcher = Watcher::start(Arc::clone(&catalog), &config, Duration::from_millis(50))
            .expect("the watcher starts");
        fs::write(root.join("loras/new.safetensors"), safetensors()).expect("fixture is writable");

        let deadline = Instant::now() + Duration::from_secs(60);
        while watcher.rescans() == 0 {
            assert!(
                Instant::now() < deadline,
                "the watcher never rescanned the source"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let totals = catalog.totals().expect("the catalog reads");
        assert!(totals.artifacts > 0, "the rescan indexed the new file");
        assert_eq!(
            catalog.pending_hashes().expect("the catalog reads"),
            0,
            "the watched rescan hashes what it indexed"
        );
        watcher.stop();
    }
}
