use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::{Span, Timestamp, tz::TimeZone};
use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{ArtifactUse, RepoAtKey, RepoUsage, Usage, UsageKey};
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

/// How many distinct clients one repository's record keeps. A LAN holds far fewer, and the
/// extractor only believes a forwarded address that parses, so the cap is what bounds the
/// row when a source spoofs addresses rather than a limit a real network reaches.
const CLIENT_CAP: usize = 4_096;

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
        transaction.insert(usage_row(seq, event))?;
        write_repo_usage(&transaction, tally_repos(std::slice::from_ref(event)))?;
        transaction.commit()?;
        Ok(seq)
    }

    /// Records many served responses in one transaction: every usage row, the maintained
    /// record of each repository they served, one use bump per artifact carrying that
    /// artifact's count and latest time, and one search change per distinct artifact.
    /// Returns how many rows were appended.
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
            transaction.insert(usage_row(seq, event))?;
            if let Some(artifact) = event.artifact {
                let slot = bumps.entry(artifact.get()).or_insert((0, event.at));
                slot.0 += 1;
                slot.1 = slot.1.max(event.at);
            }
        }
        write_repo_usage(&transaction, tally_repos(events))?;
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
        let window = Window::of(days)?;
        Ok(window.count(&self.usage_in(repo, &window)?))
    }

    /// The most recent pulls of a repository, newest first.
    pub fn recent_pulls(&self, repo: RepoRowId, limit: u32) -> Result<Vec<PullRow>> {
        pull_rows(self.newest_usage(repo, limit)?)
    }

    /// How many different clients pulled from a repository, as the repository's own record
    /// has counted them.
    pub fn distinct_clients(&self, repo: RepoRowId) -> Result<u64> {
        Ok(self
            .repo_usage(repo)?
            .map_or(0, |stored| stored.clients.len() as u64))
    }

    /// Everything the USAGE tab draws for one repository: the window the chart shows, read
    /// as one range of the log; the client count the repository's record maintains; and the
    /// newest pulls, taken from the window when it already holds enough of them.
    pub fn usage_report(&self, repo: RepoRowId, days: u32, limit: u32) -> Result<UsageReport> {
        let window = Window::of(days)?;
        let rows = self.usage_in(repo, &window)?;
        let days = window.count(&rows);
        let pulls = match rows.len() >= limit as usize {
            true => pull_rows(newest_of(rows, limit))?,
            false => self.recent_pulls(repo, limit)?,
        };
        Ok(UsageReport {
            days,
            clients: self.distinct_clients(repo)?,
            pulls,
        })
    }

    pub(super) fn repo_usage(&self, repo: RepoRowId) -> Result<Option<RepoUsage>> {
        Ok(self.database.r_transaction()?.get().primary(repo.get())?)
    }

    /// The rows one repository served inside a window, oldest first, as one range of the
    /// `repo_at` key: the scan reads the window rather than the log.
    pub(super) fn usage_in(&self, repo: RepoRowId, window: &Window) -> Result<Vec<Usage>> {
        let read = self.database.r_transaction()?;
        Ok(read
            .scan()
            .secondary(UsageKey::repo_at)?
            .range(RepoAtKey::at(repo.get(), window.first())..RepoAtKey::after(repo.get()))?
            .collect::<native_db::db_type::Result<_>>()?)
    }

    /// The newest `limit` rows of one repository's log, newest first. The range is the
    /// repository's own keys and only the rows kept are decoded.
    fn newest_usage(&self, repo: RepoRowId, limit: u32) -> Result<Vec<Usage>> {
        let read = self.database.r_transaction()?;
        Ok(read
            .scan()
            .secondary(UsageKey::repo_at)?
            .range(RepoAtKey::repository(repo.get())..RepoAtKey::after(repo.get()))?
            .rev()
            .take(limit as usize)
            .collect::<native_db::db_type::Result<_>>()?)
    }
}

/// One served response as the log stores it.
///
/// `at` is written at fixed nanosecond precision rather than jiff's default, which trims
/// trailing zeros: the `repo_at` key ends in this text, and a trimmed second sorts after a
/// fractional one inside the same second, which would put the window and the pull list out
/// of time order.
fn usage_row(seq: u64, event: &UsageEvent) -> Usage {
    Usage {
        seq,
        repo_id: event.repo.map(RepoRowId::get),
        at: format!("{:.9}", event.at),
        client: event.client.clone(),
        artifact_id: event.artifact.map(ArtifactId::get),
        path: event.path.clone(),
        bytes: event.bytes,
        rate_bps: event.rate_bps,
        status: event.status,
    }
}

/// What one write adds to one repository's maintained record.
struct RepoTally {
    pulls: u64,
    clients: BTreeSet<String>,
    last: Timestamp,
}

impl RepoTally {
    fn add(&mut self, event: &UsageEvent) {
        self.pulls += 1;
        if let Some(client) = &event.client {
            self.clients.insert(client.clone());
        }
        self.last = self.last.max(event.at);
    }
}

fn tally_repos(events: &[UsageEvent]) -> BTreeMap<i64, RepoTally> {
    let mut tallies: BTreeMap<i64, RepoTally> = BTreeMap::new();
    for event in events {
        let Some(repo) = event.repo else {
            continue;
        };
        tallies
            .entry(repo.get())
            .or_insert_with(|| RepoTally {
                pulls: 0,
                clients: BTreeSet::new(),
                last: event.at,
            })
            .add(event);
    }
    tallies
}

/// Moves each repository's record by what the write added, in the transaction that wrote
/// the rows, so the count the USAGE tab reads is never derived from the log again.
fn write_repo_usage(
    transaction: &RwTransaction<'_>,
    tallies: BTreeMap<i64, RepoTally>,
) -> Result<()> {
    for (repo_id, tally) in tallies {
        let stored: Option<RepoUsage> = transaction.get().primary(repo_id)?;
        let last_pull = match &stored {
            Some(stored) => stored
                .last_pull
                .parse::<Timestamp>()
                .map_or(tally.last, |seen| seen.max(tally.last)),
            None => tally.last,
        };
        let (pulls, mut clients) =
            stored.map_or((0, Vec::new()), |stored| (stored.pulls, stored.clients));
        for client in tally.clients {
            if clients.len() >= CLIENT_CAP {
                break;
            }
            if let Err(slot) = clients.binary_search(&client) {
                clients.insert(slot, client);
            }
        }
        transaction.upsert(RepoUsage {
            repo_id,
            pulls: pulls + tally.pulls,
            clients,
            last_pull: last_pull.to_string(),
        })?;
    }
    Ok(())
}

/// The pulls per day, the recent pulls and the client count of one repository, which is
/// what the USAGE tab shows and what the `pulls on lan` fact counts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageReport {
    pub days: Vec<UsageDay>,
    pub pulls: Vec<PullRow>,
    pub clients: u64,
}

/// The UTC days the chart draws, oldest first: the dates a reader ranges the log from and
/// the buckets it fills.
pub(super) struct Window {
    dates: Vec<String>,
}

impl Window {
    pub(super) fn of(days: u32) -> Result<Self> {
        let days = days.max(1);
        let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
        let mut dates = Vec::with_capacity(days as usize);
        for offset in (0..days).rev() {
            let date = today
                .checked_sub(Span::new().days(i64::from(offset)))
                .map_err(|error| IndexError::Catalog(format!("usage window: {error}")))?;
            dates.push(date.to_string());
        }
        Ok(Self { dates })
    }

    /// The date the window opens on, which an RFC 3339 time inside it sorts at or after.
    fn first(&self) -> &str {
        self.dates.first().map_or("", String::as_str)
    }

    /// The rows the range read, counted into the days the chart draws. A row outside the
    /// window fills no bucket, so a range wider than the window is still exact.
    fn count(&self, rows: &[Usage]) -> Vec<UsageDay> {
        let mut counted: HashMap<&str, u64> = HashMap::with_capacity(self.dates.len());
        for row in rows {
            let date = row.at.get(..DATE_WIDTH).unwrap_or(row.at.as_str());
            *counted.entry(date).or_default() += 1;
        }
        self.dates
            .iter()
            .map(|date| UsageDay {
                date: date.clone(),
                pulls: counted.get(date.as_str()).copied().unwrap_or_default(),
            })
            .collect()
    }
}

/// The newest `limit` of a window's rows, newest first. The range read them oldest first,
/// so the newest are its tail read backwards.
fn newest_of(rows: Vec<Usage>, limit: u32) -> Vec<Usage> {
    let older = rows.len().saturating_sub(limit as usize);
    rows.into_iter().skip(older).rev().collect()
}

/// The rows a reader already ordered, as the lines the USAGE tab prints.
fn pull_rows(rows: Vec<Usage>) -> Result<Vec<PullRow>> {
    rows.into_iter()
        .map(|row| {
            let when = row.at.parse().map_err(|error| {
                IndexError::Catalog(format!("usage row {} has no time: {error}", row.seq))
            })?;
            Ok(PullRow {
                when,
                who: row.client,
                what: row.path,
                bytes: row.bytes,
                rate_bps: row.rate_bps,
            })
        })
        .collect()
}

/// When an artifact was last served, read from a snapshot a caller already holds.
pub(super) fn last_used_in(
    snapshot: &native_db::transaction::RTransaction<'_>,
    artifact: i64,
) -> Result<Option<Timestamp>> {
    let stored: Option<ArtifactUse> = snapshot.get().primary(artifact)?;
    Ok(stored.and_then(|stored| stored.last_used.parse().ok()))
}
