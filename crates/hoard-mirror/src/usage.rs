//! The usage log: what every served response records, and the aggregates the USAGE tab,
//! the `used:` key and the `pulls on lan` fact read back.

use std::time::Duration;

use hoard_core::{ArtifactId, RepoRowId};
use hoard_index::store::Store;
use hoard_index::store::usage::{
    PullRow, UsageDay, UsageEvent, bump_artifact_use, distinct_clients, log_usage, recent_pulls,
    usage_days,
};
use jiff::Timestamp;
use serde::Serialize;

use crate::error::Result;
use crate::local::index_repo::{read_index, write_index};

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

/// Appends one served response to the log and marks its artifact as used.
pub async fn log_served(store: &Store, request: &ServedRequest) -> Result<()> {
    let event = request.event();
    write_index(store, move |conn| {
        log_usage(conn, &event)?;
        if let Some(artifact) = event.artifact {
            bump_artifact_use(conn, artifact, event.at)?;
        }
        Ok(())
    })
    .await
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
    store: &Store,
    repo: RepoRowId,
    days: u32,
    limit: u32,
) -> Result<UsageSummary> {
    read_index(store, move |conn| {
        Ok(UsageSummary {
            days: usage_days(conn, repo, days.max(1))?,
            pulls: recent_pulls(conn, repo, limit)?,
            clients: distinct_clients(conn, repo)?,
        })
    })
    .await
}

/// How many times the LAN pulled from a repository over the default window.
pub async fn pulls_on_lan(store: &Store, repo: RepoRowId) -> Result<u64> {
    Ok(
        usage_summary(store, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await?
            .pulls_on_lan(),
    )
}

#[cfg(test)]
mod tests {
    use hoard_core::{NodeId, RepoId, RepoType};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};

    use super::*;
    use hoard_index::config::SourceKind;

    fn store() -> Store {
        Store::open_in_memory().expect("store opens")
    }

    fn repo_row(store: &Store) -> RepoRowId {
        let conn = store.write().expect("writer");
        let node = upsert_node(
            &conn,
            &NewNode {
                node_id: NodeId::new("node-a").expect("node id"),
                name: "hoard".to_owned(),
                url: None,
                is_self: true,
            },
        )
        .expect("node inserts");
        upsert_repo(
            &conn,
            &NewRepo::new(
                RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper"),
                node,
                SourceKind::HfCache,
            ),
        )
        .expect("repo inserts")
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
        let store = store();
        let repo = repo_row(&store);

        log_served(
            &store,
            &ServedRequest::new("openai/whisper/resolve/main/model.bin", 200)
                .from_client(Some("10.0.0.7".to_owned()))
                .for_repo(Some(repo))
                .served(2_000_000, Duration::from_secs(1)),
        )
        .await
        .expect("logs");

        let summary = usage_summary(&store, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await
            .expect("summary reads");
        assert_eq!(summary.days.len(), USAGE_WINDOW_DAYS as usize);
        assert_eq!(summary.pulls_on_lan(), 1);
        assert_eq!(summary.clients, 1);
        let pull = summary.pulls.first().expect("one pull");
        assert_eq!(pull.who.as_deref(), Some("10.0.0.7"));
        assert_eq!(pull.bytes, 2_000_000);
        assert_eq!(pull.rate_bps, Some(2_000_000));
        assert_eq!(pulls_on_lan(&store, repo).await.expect("count"), 1);
    }

    #[tokio::test]
    async fn a_repository_nobody_pulled_reports_empty_bars() {
        let store = store();
        let repo = repo_row(&store);
        let summary = usage_summary(&store, repo, USAGE_WINDOW_DAYS, RECENT_PULL_LIMIT)
            .await
            .expect("summary reads");
        assert_eq!(summary.pulls_on_lan(), 0);
        assert!(summary.pulls.is_empty());
        assert_eq!(summary.clients, 0);
    }
}
