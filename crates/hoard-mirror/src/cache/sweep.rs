//! The background sweep: one eviction pass plus the index reconciliation it makes necessary.

use std::sync::Arc;
use std::time::Duration;

use hoard_index::catalog::Catalog;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::batch::SearchRefresher;
use crate::cache::BlockCache;
use crate::cache::policy::EvictionReport;
use crate::cache::reconcile::reconcile_index;
use crate::error::Result;

pub const DEFAULT_EVICTION_INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub eviction: EvictionReport,
    /// Index rows whose bytes were gone from the cache tree.
    pub retired: usize,
}

impl SweepReport {
    pub const fn changed(&self) -> bool {
        self.eviction.removed > 0 || self.retired > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionConfig {
    interval: Duration,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_EVICTION_INTERVAL,
        }
    }
}

impl EvictionConfig {
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

#[derive(Debug)]
pub struct EvictionHandle {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl EvictionHandle {
    /// Asks the loop to finish after the pass it may be running.
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn shutdown(self) {
        self.stop();
        if let Err(error) = self.task.await {
            tracing::debug!(%error, "the cache sweep ended abruptly");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// One eviction pass followed by index reconciliation, which runs even when nothing was
/// evicted.
pub async fn sweep(
    catalog: &Arc<Catalog>,
    cache: &BlockCache,
    refresh: &SearchRefresher,
) -> Result<SweepReport> {
    let eviction = cache.run_eviction().await?;
    let retired = reconcile_index(catalog, cache.layout(), refresh).await?;
    Ok(SweepReport { eviction, retired })
}

/// Runs the first sweep at once and then every interval; a failed sweep is logged and the
/// loop continues.
pub fn spawn_eviction(
    catalog: Arc<Catalog>,
    cache: BlockCache,
    refresh: Arc<SearchRefresher>,
    config: EvictionConfig,
) -> EvictionHandle {
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => sweep_once(&catalog, &cache, &refresh).await,
            }
        }
        tracing::debug!("the cache sweep stopped");
    });
    EvictionHandle { stop, task }
}

async fn sweep_once(catalog: &Arc<Catalog>, cache: &BlockCache, refresh: &SearchRefresher) {
    match sweep(catalog, cache, refresh).await {
        Ok(report) if report.changed() => tracing::info!(
            units = report.eviction.units,
            pinned = report.eviction.pinned,
            removed = report.eviction.removed,
            freed_bytes = report.eviction.freed_bytes,
            total_bytes = report.eviction.total_bytes,
            retired = report.retired,
            "the cache was swept"
        ),
        Ok(report) => tracing::debug!(
            units = report.eviction.units,
            total_bytes = report.eviction.total_bytes,
            "the cache is within its limit"
        ),
        Err(error) => tracing::error!(%error, "a cache sweep failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_core::{RepoId, RepoType, State};
    use hoard_index::catalog::Catalog;
    use hoard_index::config::IndexConfig;
    use tempfile::TempDir;

    use super::*;
    use crate::batch::test_refresher;
    use crate::cache::CacheEntry;
    use crate::config::{Geometry, MirrorConfig};
    use crate::ingest::{CachedFile, ingest_cached_file};

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn catalog(directory: &Path) -> Arc<Catalog> {
        Arc::new(
            Catalog::open_or_scan(
                &IndexConfig {
                    data_dir: directory.to_path_buf(),
                    sources: Vec::new(),
                    ..IndexConfig::default()
                },
                "node-a",
                |_, _| {},
            )
            .expect("catalog opens"),
        )
    }

    fn cache(root: &Path, limit: u64) -> BlockCache {
        let config = MirrorConfig {
            cache_size_limit: Some(limit),
            geometry: Geometry::new(64, 16).expect("valid geometry"),
            ..MirrorConfig::default()
        };
        BlockCache::at_root(Arc::new(config), root.join("repos"))
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    async fn filled(cache: &BlockCache) -> CacheEntry {
        let mut entry = cache
            .file_entry(
                &repo(),
                &SHA.parse().expect("sha parses"),
                "model.safetensors",
                64,
                Some(b"\"etag\""),
            )
            .await
            .expect("entry opens");
        entry
            .write_block(0, &[7u8; 64])
            .await
            .expect("block writes");
        entry
    }

    #[tokio::test]
    async fn the_scheduler_sweeps_on_every_tick_until_it_is_shut_down() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(root.path(), 1);
        let dir = filled(&cache).await.dir().to_path_buf();

        let indexed = catalog(&root.path().join("native"));
        let refresh = Arc::new(test_refresher(&indexed));
        let handle = spawn_eviction(
            indexed,
            cache,
            refresh,
            EvictionConfig::default().with_interval(Duration::from_millis(5)),
        );
        for _ in 0..100 {
            if !dir.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(!dir.exists(), "the pass at startup evicts the entry");
        assert!(
            !handle.is_finished(),
            "the loop keeps running between ticks"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_sweep_reports_the_eviction_and_the_rows_it_reconciled() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(root.path(), u64::MAX);
        let dir = filled(&cache).await.dir().to_path_buf();

        let indexed = catalog(&root.path().join("native"));
        let report = sweep(&indexed, &cache, &test_refresher(&indexed))
            .await
            .expect("the sweep runs");

        assert_eq!(report.eviction.units, 1);
        assert_eq!(report.eviction.removed, 0);
        assert_eq!(report.retired, 0);
        assert!(!report.changed());
        assert!(dir.exists(), "a cache within its limit is left alone");
    }

    #[tokio::test]
    async fn a_sweep_stops_the_index_advertising_bytes_it_evicted() {
        let root = TempDir::new().expect("temp dir");
        let cache = cache(root.path(), 1);
        let catalog = catalog(&root.path().join("native"));
        let refresh = test_refresher(&catalog);
        let mut cached = filled(&cache).await;
        let outcome = ingest_cached_file(
            &catalog,
            &cache,
            &mut cached,
            CachedFile {
                repo: &repo(),
                commit: SHA.parse().expect("sha parses"),
                path: "model.safetensors",
                expected: None,
            },
            &refresh,
        )
        .await
        .expect("ingest runs")
        .expect("the entry is complete");
        drop(cached);

        let report = sweep(&catalog, &cache, &refresh)
            .await
            .expect("the sweep runs");

        assert_eq!(report.eviction.removed, 1);
        assert_eq!(report.retired, 1);
        assert!(report.changed());
        let row = catalog
            .entries_of(outcome.artifact.expect("the weight became an artifact"))
            .expect("entries read")
            .into_iter()
            .find(|row| row.entry_id == outcome.entry)
            .expect("the row survives its bytes");
        assert!(!row.present, "nothing advertises bytes the cache dropped");
        assert_eq!(row.state, State::Remote);
        let mut served = 0;
        catalog
            .served_cache_entries_in(64, |rows| {
                served += rows.len();
                Ok(())
            })
            .expect("served rows read");
        assert_eq!(served, 0, "nothing is left serving out of the cache");
    }
}
