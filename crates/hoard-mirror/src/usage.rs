//! The usage log: what every served response records, and the aggregates the USAGE tab,
//! the `used:` key and the `pulls on lan` fact read back.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hoard_core::{ArtifactId, RepoRowId};
use hoard_index::catalog::{ArtifactOverflow, Catalog, PullRow, RepoTally, UsageDay, UsageEvent};
use jiff::Timestamp;
use serde::Serialize;

use crate::batch::{Debounced, SearchRefresher};
use crate::error::Result;
use crate::local::index_repo::on_catalog;

/// The width of the USAGE tab's bar chart.
pub const USAGE_WINDOW_DAYS: u32 = 7;

/// How many lines RECENT PULLS shows.
pub const RECENT_PULL_LIMIT: u32 = 20;

/// One response this node served, as the log records it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServedRequest {
    pub at: Timestamp,
    pub client: Option<String>,
    pub repo: Option<RepoRowId>,
    pub artifact: Option<ArtifactId>,
    pub path: String,
    pub bytes: u64,
    pub elapsed: Duration,
    pub status: u16,
}

impl ServedRequest {
    pub fn new(path: impl Into<String>, status: u16) -> Self {
        Self {
            at: Timestamp::now(),
            client: None,
            repo: None,
            artifact: None,
            path: path.into(),
            bytes: 0,
            elapsed: Duration::ZERO,
            status,
        }
    }

    /// Who asked, as the address or node name the request arrived from.
    pub fn from_client(mut self, client: Option<String>) -> Self {
        self.client = client;
        self
    }

    pub fn for_repo(mut self, repo: Option<RepoRowId>) -> Self {
        self.repo = repo;
        self
    }

    pub fn for_artifact(mut self, artifact: Option<ArtifactId>) -> Self {
        self.artifact = artifact;
        self
    }

    /// How many bytes left this node and how long they took, which is what the rate column
    /// is derived from.
    pub fn served(mut self, bytes: u64, elapsed: Duration) -> Self {
        self.bytes = bytes;
        self.elapsed = elapsed;
        self
    }

    pub fn at(mut self, at: Timestamp) -> Self {
        self.at = at;
        self
    }

    /// The transfer rate, absent when nothing measurable moved.
    pub fn rate_bps(&self) -> Option<u64> {
        let seconds = self.elapsed.as_secs_f64();
        if self.bytes == 0 || seconds <= 0.0 {
            return None;
        }
        let rate = self.bytes as f64 / seconds;
        if rate.is_finite() {
            Some(rate as u64)
        } else {
            None
        }
    }

    fn event(&self) -> UsageEvent {
        UsageEvent {
            at: self.at,
            client: self.client.clone(),
            repo: self.repo,
            artifact: self.artifact,
            path: self.path.clone(),
            bytes: self.bytes,
            rate_bps: self.rate_bps(),
            status: self.status,
        }
    }
}

/// How long a served response may wait before its row is written.
pub const SINK_INTERVAL: Duration = Duration::from_millis(500);

/// How many served responses wait before the sink writes without waiting for the interval.
pub const SINK_CAPACITY: usize = 256;

/// How many served responses may wait before a recorded one is folded into counts instead of
/// held: past this, an overload costs the pull row a response would have made, never a count.
pub const SINK_BOUND: usize = SINK_CAPACITY * 16;

/// How many distinct artifacts one overload episode counts uses for before it starts
/// dropping the bump instead of holding it, the same way a repository's client list is
/// capped rather than left to grow with whoever the network happens to send.
const OVERFLOW_ARTIFACT_CAP: usize = 4_096;

/// Counts a folded response could not become a row for, held until the next write applies
/// them the way a written row would: the repository tallies and the latest use of each
/// artifact. Draining replaces it with an empty one, which is also what un-warns the next
/// episode.
#[derive(Debug, Default)]
struct UsageOverflow {
    repos: BTreeMap<RepoRowId, RepoTally>,
    artifacts: BTreeMap<ArtifactId, ArtifactOverflow>,
    artifacts_dropped: u64,
    warned: bool,
}

impl UsageOverflow {
    /// Folds one served response directly, without building the [`UsageEvent`] a written
    /// row needs: the overflow reads only the time, the repository, the client and the
    /// artifact, so cloning `path` for it would be wasted, and it runs under the lock every
    /// recorder past the sink's bound waits on.
    fn record(&mut self, request: &ServedRequest) {
        if let Some(repo) = request.repo {
            self.repos
                .entry(repo)
                .or_insert_with(|| RepoTally::new(request.at))
                .add(request.at, request.client.as_deref());
        }
        if let Some(artifact) = request.artifact {
            if let Some(existing) = self.artifacts.get_mut(&artifact) {
                existing.count += 1;
                existing.last_used = existing.last_used.max(request.at);
            } else if self.artifacts.len() < OVERFLOW_ARTIFACT_CAP {
                self.artifacts.insert(
                    artifact,
                    ArtifactOverflow {
                        count: 1,
                        last_used: request.at,
                    },
                );
            } else {
                self.artifacts_dropped += 1;
            }
        }
    }
}

/// The usage log's writer: served responses accumulate here and are written in one
/// transaction every [`SINK_INTERVAL`] or every [`SINK_CAPACITY`] responses. Past
/// [`SINK_BOUND`] waiting responses, a recorded one is folded into an overload's counts
/// instead of held, so the sink's memory is bounded however far behind the writer falls.
#[derive(Debug)]
pub struct UsageSink {
    sink: Debounced<ServedRequest>,
}

impl UsageSink {
    /// Starts the writer. Outside a Tokio runtime nothing is timed and only `flush` writes.
    /// A batch leaves one search change per artifact it counted, so the refresher is poked
    /// rather than the index rebuilt here.
    pub fn spawn(
        catalog: Arc<Catalog>,
        refresh: Arc<SearchRefresher>,
        interval: Duration,
        capacity: usize,
    ) -> Self {
        let overflow: Arc<Mutex<UsageOverflow>> = Arc::default();
        let folding = Arc::clone(&overflow);
        Self {
            sink: Debounced::spawn_bounded(
                interval,
                capacity,
                SINK_BOUND,
                move |served: Vec<ServedRequest>| {
                    let drained = std::mem::take(
                        &mut *overflow.lock().unwrap_or_else(PoisonError::into_inner),
                    );
                    if drained.artifacts_dropped > 0 {
                        tracing::warn!(
                            dropped = drained.artifacts_dropped,
                            cap = OVERFLOW_ARTIFACT_CAP,
                            "usage sink overload touched more distinct artifacts than the \
                             overflow counts; their bumps are what was dropped"
                        );
                    }
                    let events: Vec<UsageEvent> = served.iter().map(ServedRequest::event).collect();
                    catalog.log_usage_batch_with_overflow(
                        &events,
                        drained.repos,
                        drained.artifacts,
                    )?;
                    refresh.poke();
                    Ok(())
                },
                move |request: &ServedRequest| {
                    let warn = {
                        let mut overflow = folding.lock().unwrap_or_else(PoisonError::into_inner);
                        let first = !overflow.warned;
                        overflow.warned = true;
                        overflow.record(request);
                        first
                    };
                    if warn {
                        tracing::warn!(
                            bound = SINK_BOUND,
                            "usage sink overloaded past its bound: pull rows are being \
                             dropped, counts are not"
                        );
                    }
                },
            ),
        }
    }

    /// Records one served response, to be written with the ones beside it.
    pub fn record(&self, request: ServedRequest) {
        self.sink.record(request);
    }

    /// How many served responses are waiting to be written.
    pub fn pending(&self) -> usize {
        self.sink.pending()
    }

    /// How many served responses have been folded into counts rather than held since the
    /// last take.
    pub fn folded(&self) -> u64 {
        self.sink.folded()
    }

    /// Writes every response recorded so far, which is what a reader of the log waits on.
    pub async fn flush(&self) -> Result<()> {
        self.sink.flush().await
    }

    /// Stops the writer and writes what is left.
    pub async fn shutdown(self) -> Result<()> {
        self.sink.shutdown().await
    }
}

/// Everything the USAGE tab draws for one repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageSummary {
    pub days: Vec<UsageDay>,
    pub pulls: Vec<PullRow>,
    pub clients: u64,
}

impl UsageSummary {
    /// The `pulls on lan` fact: every pull inside the window the summary covers.
    pub fn pulls_on_lan(&self) -> u64 {
        self.days.iter().map(|day| day.pulls).sum()
    }
}

/// The seven day bars, the recent pulls and the client count of one repository.
pub async fn usage_summary(
    catalog: &Arc<Catalog>,
    repo: RepoRowId,
    days: u32,
    limit: u32,
) -> Result<UsageSummary> {
    on_catalog(catalog, move |catalog| {
        let report = catalog.usage_report(repo, days.max(1), limit)?;
        Ok(UsageSummary {
            days: report.days,
            pulls: report.pulls,
            clients: report.clients,
        })
    })
    .await
}

/// How many times the LAN pulled from a repository over the default window.
pub async fn pulls_on_lan(catalog: &Arc<Catalog>, repo: RepoRowId) -> Result<u64> {
    Ok(
        usage_summary(catalog, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await?
            .pulls_on_lan(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hoard_index::catalog::RepositoryFilter;
    use hoard_index::config::{IndexConfig, SourceConfig, SourceKind};

    use super::*;
    use crate::batch::test_refresher;

    /// A catalog over one loose file, which is the smallest world that has a repository
    /// row for the usage log to point at.
    fn catalog(directory: &Path) -> Arc<Catalog> {
        let input = directory.join("models");
        std::fs::create_dir_all(&input).expect("source directory");
        std::fs::write(input.join("model.gguf"), b"not really a model").expect("a file to find");
        let config = IndexConfig {
            data_dir: directory.join("index"),
            sources: vec![SourceConfig {
                name: "loose".to_owned(),
                path: input,
                kind: SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            }],
            ..IndexConfig::default()
        };
        Arc::new(Catalog::open_or_scan(&config, "hoard", |_, _| {}).expect("catalog opens"))
    }

    fn only_repo(catalog: &Catalog) -> RepoRowId {
        catalog
            .repositories(&RepositoryFilter::default(), None, 10)
            .expect("repositories list")
            .rows
            .first()
            .expect("the scan found one repository")
            .id
    }

    #[test]
    fn a_rate_needs_both_bytes_and_time() {
        let request = ServedRequest::new("model.bin", 200);
        assert_eq!(request.rate_bps(), None);
        assert_eq!(
            request
                .clone()
                .served(1_000_000, Duration::from_secs(2))
                .rate_bps(),
            Some(500_000)
        );
        assert_eq!(
            request.clone().served(1_000, Duration::ZERO).rate_bps(),
            None
        );
        assert_eq!(request.served(0, Duration::from_secs(1)).rate_bps(), None);
    }

    #[tokio::test]
    async fn a_served_response_lands_in_the_log_and_the_summary() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog = catalog(directory.path());
        let repo = only_repo(&catalog);

        let sink = UsageSink::spawn(
            Arc::clone(&catalog),
            Arc::new(test_refresher(&catalog)),
            SINK_INTERVAL,
            SINK_CAPACITY,
        );
        sink.record(
            ServedRequest::new("openai/whisper/resolve/main/model.bin", 200)
                .from_client(Some("10.0.0.7".to_owned()))
                .for_repo(Some(repo))
                .served(2_000_000, Duration::from_secs(1)),
        );
        sink.flush().await.expect("logs");

        let summary = usage_summary(&catalog, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await
            .expect("summary reads");
        assert_eq!(summary.days.len(), USAGE_WINDOW_DAYS as usize);
        assert_eq!(summary.pulls_on_lan(), 1);
        assert_eq!(summary.clients, 1);
        let pull = summary.pulls.first().expect("one pull");
        assert_eq!(pull.who.as_deref(), Some("10.0.0.7"));
        assert_eq!(pull.bytes, 2_000_000);
        assert_eq!(pull.rate_bps, Some(2_000_000));
        assert_eq!(pulls_on_lan(&catalog, repo).await.expect("count"), 1);
        sink.shutdown().await.expect("sink stops");
    }

    #[tokio::test]
    async fn a_response_nobody_can_be_attributed_to_still_lands_in_the_log() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog = catalog(directory.path());
        let repo = only_repo(&catalog);

        let sink = UsageSink::spawn(
            Arc::clone(&catalog),
            Arc::new(test_refresher(&catalog)),
            SINK_INTERVAL,
            SINK_CAPACITY,
        );
        sink.record(
            ServedRequest::new("unknown/repo/resolve/main/model.bin", 200)
                .from_client(Some("10.0.0.9".to_owned()))
                .served(1_000, Duration::from_secs(1)),
        );
        sink.flush().await.expect("logs");

        let summary = usage_summary(&catalog, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await
            .expect("summary reads");
        assert_eq!(
            summary.pulls_on_lan(),
            0,
            "a response with no repository belongs to no repository's bars"
        );
        assert!(summary.pulls.is_empty());
        assert_eq!(summary.clients, 0);
        sink.shutdown().await.expect("sink stops");
    }

    #[tokio::test]
    async fn a_folded_response_still_counts_towards_the_repository_and_the_artifact() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog = catalog(directory.path());
        let repo = only_repo(&catalog);
        let artifact = on_catalog(&catalog, move |catalog| {
            Ok(catalog
                .repository_rows(repo, 1)?
                .first()
                .expect("the scan found one artifact")
                .artifact_id)
        })
        .await
        .expect("the seeded artifact");

        let sink = UsageSink::spawn(
            Arc::clone(&catalog),
            Arc::new(test_refresher(&catalog)),
            SINK_INTERVAL,
            SINK_CAPACITY,
        );
        for index in 0..5_000_u32 {
            sink.record(
                ServedRequest::new(format!("acme/model/resolve/main/f{index}.bin"), 200)
                    .from_client(Some("10.0.0.7".to_owned()))
                    .for_repo(Some(repo))
                    .for_artifact(Some(artifact))
                    .served(1_000, Duration::from_millis(1)),
            );
        }
        let pending_before = sink.pending();
        let folded_before = sink.folded();
        assert_eq!(
            pending_before as u64 + folded_before,
            5_000,
            "every recorded response is either pending or folded"
        );
        assert!(
            folded_before > 0,
            "5,000 responses overran the sink's bound"
        );

        sink.flush()
            .await
            .expect("flush writes what is pending and folds the rest in");

        let summary = usage_summary(&catalog, repo, USAGE_WINDOW_DAYS, 10_000)
            .await
            .expect("summary reads");
        assert_eq!(
            summary.pulls_on_lan(),
            5_000,
            "the day's bar counts every response, folded or not"
        );
        assert_eq!(
            summary.pulls.len(),
            pending_before,
            "only the rows that were not folded reach the log"
        );
        let usage = on_catalog(&catalog, move |catalog| {
            Ok(catalog.artifact_usage(artifact)?)
        })
        .await
        .expect("artifact usage reads");
        assert_eq!(usage.pulls, 5_000, "the artifact's count survives the fold");

        sink.shutdown().await.expect("sink stops");
    }

    #[tokio::test]
    async fn a_folded_client_flood_cannot_grow_the_overflow_without_limit() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog = catalog(directory.path());
        let repo = only_repo(&catalog);

        let sink = UsageSink::spawn(
            Arc::clone(&catalog),
            Arc::new(test_refresher(&catalog)),
            SINK_INTERVAL,
            SINK_CAPACITY,
        );
        let cap = 4_096_usize;
        let total = SINK_BOUND + cap + 1_000;
        for index in 0..total {
            sink.record(
                ServedRequest::new("acme/model/resolve/main/model.bin", 200)
                    .from_client(Some(format!(
                        "10.{}.{}.{}",
                        index / (256 * 256),
                        (index / 256) % 256,
                        index % 256
                    )))
                    .for_repo(Some(repo))
                    .served(1, Duration::from_millis(1)),
            );
        }
        let pending_before = sink.pending();
        let folded_before = sink.folded();
        assert_eq!(
            pending_before as u64 + folded_before,
            total as u64,
            "every recorded response is either pending or folded"
        );
        assert!(
            folded_before as usize > cap,
            "more distinct clients were folded than the cap allows: {folded_before}"
        );

        sink.flush()
            .await
            .expect("flush writes what is pending and folds the rest in");

        let summary = usage_summary(&catalog, repo, USAGE_WINDOW_DAYS, 1)
            .await
            .expect("summary reads");
        assert_eq!(
            summary.clients, cap as u64,
            "the client count stops exactly at the cap, whatever the fold held onto it"
        );
        assert_eq!(
            summary.pulls_on_lan(),
            total as u64,
            "the pull count still counts every response, capped clients or not"
        );

        sink.shutdown().await.expect("sink stops");
    }

    #[tokio::test]
    async fn an_overloaded_sink_writes_one_transaction_rather_than_two() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog = catalog(directory.path());
        let repo = only_repo(&catalog);
        let artifact = on_catalog(&catalog, move |catalog| {
            Ok(catalog
                .repository_rows(repo, 1)?
                .first()
                .expect("the scan found one artifact")
                .artifact_id)
        })
        .await
        .expect("the seeded artifact");

        let sink = UsageSink::spawn(
            Arc::clone(&catalog),
            Arc::new(test_refresher(&catalog)),
            SINK_INTERVAL,
            SINK_CAPACITY,
        );
        let total = SINK_BOUND + 1_000;
        for index in 0..total {
            sink.record(
                ServedRequest::new(format!("acme/model/resolve/main/f{index}.bin"), 200)
                    .from_client(Some("10.0.0.7".to_owned()))
                    .for_repo(Some(repo))
                    .for_artifact(Some(artifact))
                    .served(1, Duration::from_millis(1)),
            );
        }
        assert!(sink.folded() > 0, "the burst overran the bound");

        sink.flush()
            .await
            .expect("flush writes the pending rows and folds the rest in");

        let summary = usage_summary(&catalog, repo, USAGE_WINDOW_DAYS, 10)
            .await
            .expect("summary reads");
        assert_eq!(
            summary.pulls_on_lan(),
            total as u64,
            "every response counted, whether it was written or folded"
        );
        let usage = on_catalog(&catalog, move |catalog| {
            Ok(catalog.artifact_usage(artifact)?)
        })
        .await
        .expect("artifact usage reads");
        assert_eq!(
            usage.pulls, total as u64,
            "the artifact's count is exact across the fold"
        );

        sink.shutdown().await.expect("sink stops");
    }
}
