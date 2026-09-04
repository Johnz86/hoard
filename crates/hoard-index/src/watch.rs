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
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};
use crate::scan::{ScanOptions, ScanProgress, scan, source_for_path};
use crate::store::Store;

/// How long a burst of events is collected before the sources it touched are rescanned.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(750);

/// Watches every configured source root and rescans what changes.
///
/// The watcher must be created inside a tokio runtime; it holds a background task that
/// lives until [`Watcher::stop`] is called or the watcher is dropped.
#[derive(Debug)]
pub struct Watcher {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    task: JoinHandle<()>,
}

impl Watcher {
    /// Starts watching every source of `config` that exists on disk.
    pub fn start(store: &Store, config: &IndexConfig, debounce: Duration) -> Result<Self> {
        let (sender, receiver) = unbounded_channel();
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

        let task = tokio::spawn(pump(store.clone(), config.clone(), debounce, receiver));
        Ok(Self {
            watcher,
            roots,
            task,
        })
    }

    /// Stops watching and cancels the pending rescan. Dropping the watcher does the same.
    pub fn stop(&mut self) {
        for root in self.roots.drain(..) {
            if let Err(error) = self.watcher.unwatch(&root) {
                tracing::debug!(path = %root.display(), %error, "root was already unwatched");
            }
        }
        self.task.abort();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn watch_error(error: notify::Error) -> IndexError {
    IndexError::Config(format!("filesystem watch failed: {error}"))
}

/// Passes on the paths of events that can change what the index holds.
fn forward(sender: &UnboundedSender<PathBuf>, event: notify::Result<notify::Event>) {
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
        let _ = sender.send(path);
    }
}

const fn mutates(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

async fn pump(
    store: Store,
    config: IndexConfig,
    debounce: Duration,
    mut events: UnboundedReceiver<PathBuf>,
) {
    let quiet: &(dyn Fn(ScanProgress) + Send + Sync) = &|_| {};
    while let Some(first) = events.recv().await {
        let mut pending = BTreeSet::new();
        note(&config, &first, &mut pending);
        while let Ok(Some(path)) = tokio::time::timeout(debounce, events.recv()).await {
            note(&config, &path, &mut pending);
        }
        for name in pending {
            let options = ScanOptions::from_config(&config).for_source(name.clone());
            match scan(&store, &config, options, quiet).await {
                Ok(report) => {
                    tracing::info!(
                        source = name,
                        files = report.files,
                        "rescanned after a change"
                    );
                }
                Err(error) => tracing::warn!(source = name, %error, "watched rescan failed"),
            }
        }
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
    use crate::store::blob::disk_pressure;

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
        let store = Store::open(&dir.path().join("hoard.db")).expect("store opens");
        let config = IndexConfig {
            data_dir: dir.path().to_path_buf(),
            sources: vec![source("comfy", &root)],
            ..IndexConfig::default()
        };

        let mut watcher =
            Watcher::start(&store, &config, Duration::from_millis(50)).expect("the watcher starts");
        fs::write(root.join("loras/new.safetensors"), safetensors()).expect("fixture is writable");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let pressure = store.call(disk_pressure).await.expect("index reads");
            let indexed = store
                .call(|conn| crate::store::blob::unhashed_blobs(conn, 10).map(|rows| rows.len()))
                .await
                .expect("index reads");
            if indexed > 0 {
                assert_eq!(pressure.duplicate_blobs, 0);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the watcher never indexed the file"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        watcher.stop();
    }
}
