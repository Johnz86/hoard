use std::collections::HashSet;

use jiff::{Span, Timestamp, tz::TimeZone};
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{ArtifactUse, Usage, UsageKey};
use super::scan::{Counters, log_changes};
use super::userdata::{Sequenced, next_key};

/// One served request, which is what the USAGE tab and `used:` queries read.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub at: Timestamp,
    pub client: Option<String>,
    pub repo: Option<RepoRowId>,
    pub artifact: Option<ArtifactId>,
    pub path: String,
    pub bytes: u64,
    pub rate_bps: Option<u64>,
    pub status: u16,
}

impl UsageEvent {
    pub fn new(path: impl Into<String>, at: Timestamp) -> Self {
        Self {
            at,
            client: None,
            repo: None,
            artifact: None,
            path: path.into(),
            bytes: 0,
            rate_bps: None,
            status: 200,
        }
    }
}

/// One bar of the seven day usage chart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageDay {
    pub date: String,
    pub pulls: u64,
}

/// One line of the RECENT PULLS list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PullRow {
    pub when: Timestamp,
    pub who: Option<String>,
    pub what: String,
    pub bytes: u64,
    pub rate_bps: Option<u64>,
}

/// How often this node has served one artifact, and when it last did.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ArtifactUsage {
    pub pulls: u64,
    pub last_used: Option<Timestamp>,
}

impl Sequenced for Usage {
    fn sequence(&self) -> u64 {
        self.seq
    }
}

impl Catalog {
    /// Appends one served response to the log and returns the sequence it was given.
    pub fn log_usage(&self, event: &UsageEvent) -> Result<u64> {
        let transaction = self.database.rw_transaction()?;
        let seq = next_key::<Usage>(&transaction)?;
        transaction.insert(Usage {
            seq,
            repo_id: event.repo.map(RepoRowId::get),
            at: event.at.to_string(),
            client: event.client.clone(),
            artifact_id: event.artifact.map(ArtifactId::get),
            path: event.path.clone(),
            bytes: event.bytes,
            rate_bps: event.rate_bps,
            status: event.status,
        })?;
        transaction.commit()?;
        Ok(seq)
    }

    /// Counts one use and logs the artifact for the search index; the index picks the
    /// new `last_used` up on the next refresh.
    pub fn bump_artifact_use(&self, artifact: ArtifactId, at: Timestamp) -> Result<()> {
        let mut counters = Counters::load(&self.database)?;
        let transaction = self.database.rw_transaction()?;
        let stored: Option<ArtifactUse> = transaction.get().primary(artifact.get())?;
        transaction.upsert(ArtifactUse {
            artifact_id: artifact.get(),
            count: stored.map_or(0, |stored| stored.count) + 1,
            last_used: at.to_string(),
        })?;
        counters.pending.push(artifact.get());
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn artifact_usage(&self, artifact: ArtifactId) -> Result<ArtifactUsage> {
        let stored: Option<ArtifactUse> = self
            .database
            .r_transaction()?
            .get()
            .primary(artifact.get())?;
        Ok(stored.map_or(
            ArtifactUsage {
                pulls: 0,
                last_used: None,
            },
            |stored| ArtifactUsage {
                pulls: stored.count,
                last_used: stored.last_used.parse().ok(),
            },
        ))
    }

    /// Pull counts per UTC day for the last `days` days, ending today and including the
    /// days nobody pulled on, oldest first.
    pub fn usage_days(&self, repo: RepoRowId, days: u32) -> Result<Vec<UsageDay>> {
        let days = days.max(1);
        let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
        let logged = self.usage_of(repo)?;
        let mut window = Vec::with_capacity(days as usize);
        for offset in (0..days).rev() {
            let date = today
                .checked_sub(Span::new().days(i64::from(offset)))
                .map_err(|error| IndexError::Catalog(format!("usage window: {error}")))?
                .to_string();
            let pulls = logged
                .iter()
                .filter(|row| row.at.starts_with(&date))
                .count() as u64;
            window.push(UsageDay { date, pulls });
        }
        Ok(window)
    }

    /// The most recent pulls of a repository, newest first.
    pub fn recent_pulls(&self, repo: RepoRowId, limit: u32) -> Result<Vec<PullRow>> {
        let mut logged = self.usage_of(repo)?;
        logged.sort_by(|left, right| {
            right
                .at
                .cmp(&left.at)
                .then_with(|| right.seq.cmp(&left.seq))
        });
        logged.truncate(limit as usize);
        logged
            .into_iter()
            .map(|row| {
                Ok(PullRow {
                    when: row.at.parse().map_err(|error| {
                        IndexError::Catalog(format!("usage row {} has no time: {error}", row.seq))
                    })?,
                    who: row.client,
                    what: row.path,
                    bytes: row.bytes,
                    rate_bps: row.rate_bps,
                })
            })
            .collect()
    }

    /// How many different clients pulled from a repository.
    pub fn distinct_clients(&self, repo: RepoRowId) -> Result<u64> {
        let clients: HashSet<String> = self
            .usage_of(repo)?
            .into_iter()
            .filter_map(|row| row.client)
            .collect();
        Ok(clients.len() as u64)
    }

    fn usage_of(&self, repo: RepoRowId) -> Result<Vec<Usage>> {
        let read = self.database.r_transaction()?;
        let mut rows: Vec<Usage> = read
            .scan()
            .secondary(UsageKey::repo_id)?
            .start_with(Some(repo.get()))?
            .collect::<native_db::db_type::Result<_>>()?;
        rows.retain(|row| row.repo_id == Some(repo.get()));
        Ok(rows)
    }
}

/// When an artifact was last served, read from a snapshot a caller already holds.
pub(super) fn last_used_in(
    snapshot: &native_db::transaction::RTransaction<'_>,
    artifact: i64,
) -> Result<Option<Timestamp>> {
    let stored: Option<ArtifactUse> = snapshot.get().primary(artifact)?;
    Ok(stored.and_then(|stored| stored.last_used.parse().ok()))
}
