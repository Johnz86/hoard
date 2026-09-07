use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::{Span, Timestamp, tz::TimeZone};
use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{ArtifactUse, RepoAtKey, RepoUsage, Usage, UsageKey};
use super::scan::{CHUNK, Counters, log_changes};
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

/// How many UTC days of per-day counts one repository's record keeps, which is longer than
/// any window the UI draws. A window longer than this is counted from the log.
pub(super) const HISTORY_DAYS: usize = 31;

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
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
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
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
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
    ///
    /// Read off the repository's own record, which every usage write moves, so the chart
    /// costs one point read rather than a decode of every row in the window. A window
    /// longer than the record keeps is counted from the log instead.
    pub fn usage_days(&self, repo: RepoRowId, days: u32) -> Result<Vec<UsageDay>> {
        let window = Window::of(days)?;
        let stored = self.repo_usage(repo)?;
        self.chart(repo, &window, stored.as_ref())
    }

    fn chart(
        &self,
        repo: RepoRowId,
        window: &Window,
        stored: Option<&RepoUsage>,
    ) -> Result<Vec<UsageDay>> {
        match window.days() > HISTORY_DAYS {
            true => Ok(window.count(&self.usage_in(repo, window)?)),
            false => Ok(window.tallied(stored.map_or(&[], |stored| stored.days.as_slice()))),
        }
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

    /// Everything the USAGE tab draws for one repository, from one read of the repository's
    /// record and one range of the newest rows of its log: the chart the record maintains,
    /// the client count it maintains, and the newest pulls.
    pub fn usage_report(&self, repo: RepoRowId, days: u32, limit: u32) -> Result<UsageReport> {
        let window = Window::of(days)?;
        let stored = self.repo_usage(repo)?;
        Ok(UsageReport {
            days: self.chart(repo, &window, stored.as_ref())?,
            clients: stored.map_or(0, |stored| stored.clients.len() as u64),
            pulls: self.recent_pulls(repo, limit)?,
        })
    }

    /// Removes every row one repository ever served, in `CHUNK`-sized transactions, as one
    /// range of the `repo_at` key. A retired repository's history goes with it, because
    /// nothing names the repository it counted any more, and a log of a million pulls is not
    /// a transaction to hold in memory to say so.
    pub(super) fn forget_usage_of(&self, repo_id: i64) -> Result<()> {
        loop {
            let read = self.database.r_transaction()?;
            let batch: Vec<Usage> = read
                .scan()
                .secondary(UsageKey::repo_at)?
                .range(RepoAtKey::repository(repo_id)..RepoAtKey::after(repo_id))?
                .take(CHUNK)
                .collect::<native_db::db_type::Result<_>>()?;
            drop(read);
            if batch.is_empty() {
                return Ok(());
            }
            let transaction = self.database.rw_transaction()?;
            for row in batch {
                transaction.remove(row)?;
            }
            transaction.commit()?;
        }
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

/// Removes the maintained record of one repository's log, for the transaction that removes
/// the repository itself. The rows it counted are removed by [`Catalog::forget_usage_of`]
/// before it.
pub(super) fn remove_repo_usage(transaction: &RwTransaction<'_>, repo_id: i64) -> Result<()> {
    if let Some(stored) = transaction.get().primary::<RepoUsage>(repo_id)? {
        transaction.remove(stored)?;
    }
    Ok(())
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
    days: BTreeMap<String, u64>,
    last: Timestamp,
}

impl RepoTally {
    fn add(&mut self, event: &UsageEvent, at: &str) {
        self.pulls += 1;
        if let Some(client) = &event.client {
            self.clients.insert(client.clone());
        }
        *self
            .days
            .entry(at.get(..DATE_WIDTH).unwrap_or(at).to_owned())
            .or_default() += 1;
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
                days: BTreeMap::new(),
                last: event.at,
            })
            .add(event, &stamp(event.at));
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
        let (pulls, mut clients, days) = stored.map_or((0, Vec::new(), Vec::new()), |stored| {
            (stored.pulls, stored.clients, stored.days)
        });
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
            days: kept_days(days, tally.days),
            clients,
            last_pull: last_pull.to_string(),
        })?;
    }
    Ok(())
}

/// The most recent [`HISTORY_DAYS`] days of one repository's chart, newest first: what the
/// record held merged with what this write added, and nothing older.
fn kept_days(held: Vec<(String, u64)>, added: BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut merged: BTreeMap<String, u64> = held.into_iter().collect();
    for (date, pulls) in added {
        *merged.entry(date).or_default() += pulls;
    }
    let mut days: Vec<(String, u64)> = merged.into_iter().rev().collect();
    days.truncate(HISTORY_DAYS);
    days
}

/// The text a row's time is stored as, which is what its day is read from.
fn stamp(at: Timestamp) -> String {
    format!("{at:.9}")
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

    /// How many days the chart draws.
    fn days(&self) -> usize {
        self.dates.len()
    }

    /// The chart the repository's own record answers: every day of the window, filled from
    /// the counts the record keeps and left at zero where it keeps none.
    fn tallied(&self, kept: &[(String, u64)]) -> Vec<UsageDay> {
        self.dates
            .iter()
            .map(|date| UsageDay {
                date: date.clone(),
                pulls: kept
                    .iter()
                    .find(|(held, _)| held == date)
                    .map_or(0, |(_, pulls)| *pulls),
            })
            .collect()
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
