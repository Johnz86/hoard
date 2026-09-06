use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use jiff::{Span, Timestamp, tz::TimeZone};
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::config::IndexConfig;
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

/// The `YYYY-MM-DD` prefix a usage row's timestamp starts with, which is the day it counts
/// towards.
const DATE_WIDTH: usize = 10;

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

    /// Records many served responses in one transaction: every usage row, one use bump per
    /// artifact carrying that artifact's count and latest time, and one search change per
    /// distinct artifact. Returns how many rows were appended.
    pub fn log_usage_batch(&self, events: &[UsageEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let transaction = self.database.rw_transaction()?;
        let first = next_key::<Usage>(&transaction)?;
        let mut bumps: BTreeMap<i64, (u64, Timestamp)> = BTreeMap::new();
        for (seq, event) in (first..).zip(events) {
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
            if let Some(artifact) = event.artifact {
                let slot = bumps.entry(artifact.get()).or_insert((0, event.at));
                slot.0 += 1;
                slot.1 = slot.1.max(event.at);
            }
        }
        for (artifact_id, (count, last_used)) in bumps {
            let stored: Option<ArtifactUse> = transaction.get().primary(artifact_id)?;
            let latest = match &stored {
                Some(stored) => stored
                    .last_used
                    .parse::<Timestamp>()
                    .map_or(last_used, |seen| seen.max(last_used)),
                None => last_used,
            };
            transaction.upsert(ArtifactUse {
                artifact_id,
                count: stored.map_or(0, |stored| stored.count) + count,
                last_used: latest.to_string(),
            })?;
            counters.pending.push(artifact_id);
        }
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        Ok(events.len() as u64)
    }

    /// Counts one use and logs the artifact for the search index; the index picks the
    /// new `last_used` up on the next refresh.
    pub fn bump_artifact_use(&self, artifact: ArtifactId, at: Timestamp) -> Result<()> {
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
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
        day_window(&self.usage_of(repo)?, days)
    }

    /// The most recent pulls of a repository, newest first.
    pub fn recent_pulls(&self, repo: RepoRowId, limit: u32) -> Result<Vec<PullRow>> {
        newest_pulls(self.usage_of(repo)?, limit)
    }

    /// How many different clients pulled from a repository.
    pub fn distinct_clients(&self, repo: RepoRowId) -> Result<u64> {
        Ok(distinct_clients_of(&self.usage_of(repo)?))
    }

    /// Everything the USAGE tab draws for one repository, from one read of its log rather
    /// than the three the three views used to take separately.
    pub fn usage_report(&self, repo: RepoRowId, days: u32, limit: u32) -> Result<UsageReport> {
        let rows = self.usage_of(repo)?;
        Ok(UsageReport {
            days: day_window(&rows, days)?,
            clients: distinct_clients_of(&rows),
            pulls: newest_pulls(rows, limit)?,
        })
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

/// The pulls per day, the recent pulls and the client count of one repository, which is
/// what the USAGE tab shows and what the `pulls on lan` fact counts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageReport {
    pub days: Vec<UsageDay>,
    pub pulls: Vec<PullRow>,
    pub clients: u64,
}

/// The last `days` UTC days, oldest first, counted in one pass over the log rather than one
/// pass per day.
fn day_window(rows: &[Usage], days: u32) -> Result<Vec<UsageDay>> {
    let days = days.max(1);
    let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
    let mut counted: HashMap<&str, u64> = HashMap::with_capacity(days as usize);
    for row in rows {
        let date = row.at.get(..DATE_WIDTH).unwrap_or(row.at.as_str());
        *counted.entry(date).or_default() += 1;
    }
    let mut window = Vec::with_capacity(days as usize);
    for offset in (0..days).rev() {
        let date = today
            .checked_sub(Span::new().days(i64::from(offset)))
            .map_err(|error| IndexError::Catalog(format!("usage window: {error}")))?
            .to_string();
        let pulls = counted.get(date.as_str()).copied().unwrap_or_default();
        window.push(UsageDay { date, pulls });
    }
    Ok(window)
}

/// The newest `limit` rows, newest first, kept in a heap so the whole log is never sorted.
fn newest_pulls(rows: Vec<Usage>, limit: u32) -> Result<Vec<PullRow>> {
    let limit = (limit as usize).min(rows.len());
    let mut kept: BinaryHeap<Reverse<(String, u64, usize)>> = BinaryHeap::with_capacity(limit + 1);
    for (index, row) in rows.iter().enumerate() {
        kept.push(Reverse((row.at.clone(), row.seq, index)));
        if kept.len() > limit {
            kept.pop();
        }
    }
    let mut ordered = kept.into_sorted_vec();
    ordered.reverse();
    ordered
        .into_iter()
        .map(|Reverse((_, seq, index))| {
            let row = &rows[index];
            Ok(PullRow {
                when: row.at.parse().map_err(|error| {
                    IndexError::Catalog(format!("usage row {seq} has no time: {error}"))
                })?,
                who: row.client.clone(),
                what: row.path.clone(),
                bytes: row.bytes,
                rate_bps: row.rate_bps,
            })
        })
        .collect()
}

fn distinct_clients_of(rows: &[Usage]) -> u64 {
    rows.iter()
        .filter_map(|row| row.client.as_deref())
        .collect::<HashSet<&str>>()
        .len() as u64
}

/// When an artifact was last served, read from a snapshot a caller already holds.
pub(super) fn last_used_in(
    snapshot: &native_db::transaction::RTransaction<'_>,
    artifact: i64,
) -> Result<Option<Timestamp>> {
    let stored: Option<ArtifactUse> = snapshot.get().primary(artifact)?;
    Ok(stored.and_then(|stored| stored.last_used.parse().ok()))
}
