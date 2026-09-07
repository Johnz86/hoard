use std::collections::{BTreeMap, BTreeSet, HashMap, btree_map};
use std::time::Duration;

use jiff::{Span, Timestamp, tz::TimeZone};
use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{ArtifactUse, RepoUsage, Usage, UsageLogKey};
use super::scan::{CHUNK, Counters, log_changes};

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

impl Catalog {
    /// Appends one served response to the log and returns the sequence it was given inside
    /// its repository's own log.
    pub fn log_usage(&self, event: &UsageEvent) -> Result<u64> {
        let transaction = self.database.rw_transaction()?;
        let repo_id = event.repo.map_or(0, RepoRowId::get);
        let stored = read_repo_usage(&transaction, [repo_id])?;
        let seq = sequence_after(
            &transaction,
            repo_id,
            stored.get(&repo_id).and_then(Option::as_ref),
        )? + 1;
        transaction.insert(usage_row(repo_id, seq, event))?;
        write_repo_usage(
            &transaction,
            tally_repos(std::slice::from_ref(event)),
            stored,
        )?;
        transaction.commit()?;
        Ok(seq)
    }

    /// Records many served responses in one transaction: every usage row, the maintained
    /// record of each repository they served, one use bump per artifact carrying that
    /// artifact's count and latest time, and one search change per distinct artifact.
    /// Returns how many rows were appended.
    ///
    /// Rows are numbered by repository in time order rather than the order the caller gave
    /// them, so a repository's sequence is chronological inside this batch and the reverse
    /// range [`Catalog::recent_pulls`] takes reads a page already close to time order. It is
    /// not exact across two batches, whose relative arrival order the key does not record,
    /// which is why `recent_pulls` sorts its small page rather than trusting the key alone,
    /// and why `usage_in`'s window read keeps a minute of slack rather than none.
    pub fn log_usage_batch(&self, events: &[UsageEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let tallies = tally_repos(events);
        let stored = read_repo_usage(&transaction, touched_repo_ids(events))?;
        let bumps = insert_usage_rows(&transaction, events, &stored)?;
        write_repo_usage(&transaction, tallies, stored)?;
        apply_bumps(&transaction, &mut counters, bumps)?;
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        Ok(events.len() as u64)
    }

    /// As [`Catalog::log_usage_batch`], but also drains the counts a sink's fold
    /// accumulated while its writer fell behind: the overflow's repository tallies merge
    /// into the batch's own before the one write both apply through, and its artifact bumps
    /// land beside the batch's own, all inside the one transaction the batch's own commit
    /// already pays for. Applying the overflow as a second transaction right after the
    /// first would cost a second fsync every cycle an overload is already lengthening,
    /// which folds more responses into the very overflow the second transaction exists to
    /// drain; one transaction removes that loop. Returns how many rows the batch itself
    /// appended; the overflow's responses left no row, which is the whole reason they are
    /// in the overflow rather than the batch.
    pub fn log_usage_batch_with_overflow(
        &self,
        events: &[UsageEvent],
        overflow_repos: BTreeMap<RepoRowId, RepoTally>,
        overflow_artifacts: BTreeMap<ArtifactId, ArtifactOverflow>,
    ) -> Result<u64> {
        if events.is_empty() && overflow_repos.is_empty() && overflow_artifacts.is_empty() {
            return Ok(0);
        }
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let mut tallies = tally_repos(events);
        for (repo, tally) in overflow_repos {
            match tallies.entry(repo.get()) {
                btree_map::Entry::Occupied(mut slot) => slot.get_mut().merge(tally),
                btree_map::Entry::Vacant(slot) => {
                    slot.insert(tally);
                }
            }
        }
        let repo_ids: BTreeSet<i64> = touched_repo_ids(events)
            .into_iter()
            .chain(tallies.keys().copied())
            .collect();
        let stored = read_repo_usage(&transaction, repo_ids)?;
        let bumps = insert_usage_rows(&transaction, events, &stored)?;
        write_repo_usage(&transaction, tallies, stored)?;
        apply_bumps(&transaction, &mut counters, bumps)?;
        for (artifact, overflow) in overflow_artifacts {
            if bump_artifact_counts(
                &transaction,
                artifact.get(),
                overflow.count,
                overflow.last_used,
            )? {
                counters.pending.push(artifact.get());
            }
        }
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        Ok(events.len() as u64)
    }

    /// Counts one use and, when the floored minute it carries moved, logs the artifact for
    /// the search index; the index picks the new `last_used` up on the next refresh.
    pub fn bump_artifact_use(&self, artifact: ArtifactId, at: Timestamp) -> Result<()> {
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        if bump_artifact_counts(&transaction, artifact.get(), 1, at)? {
            counters.pending.push(artifact.get());
        }
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
    /// lazy range of the repository's own primary keys: the scan reads only the rows each
    /// chunk removes, not the whole match a secondary scan would materialise first. A
    /// retired repository's history goes with it, because nothing names the repository it
    /// counted any more, and a log of a million pulls is not a transaction to hold in
    /// memory to say so.
    pub(super) fn forget_usage_of(&self, repo_id: i64) -> Result<()> {
        loop {
            let read = self.database.r_transaction()?;
            let batch: Vec<Usage> = read
                .scan()
                .primary()?
                .range(UsageLogKey::repository(repo_id)..UsageLogKey::after(repo_id))?
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

    /// The rows one repository served inside a window: a lazy reverse range of the
    /// repository's own primary keys, stopped once a row is definitely too old rather than
    /// read to the end. Only used for a window longer than the repository's record keeps,
    /// which the log itself answers.
    ///
    /// The key orders by sequence, which is chronological inside one batch (`log_usage_batch`
    /// sorts before it numbers) but only insertion order across batches, so a row's `at` is
    /// not a strictly falling sequence as the range is walked backwards. Stopping at the
    /// first row older than the window's open date would risk dropping a row that arrived
    /// late but happened inside the window; stopping a full minute earlier than that instead
    /// costs at most a few extra rows kept, which is free, because [`Window::count`] tallies
    /// into a map keyed by date, ignores whatever falls outside the window's own dates and
    /// does not care what order it was handed the rows in.
    pub(super) fn usage_in(&self, repo: RepoRowId, window: &Window) -> Result<Vec<Usage>> {
        let read = self.database.r_transaction()?;
        let cutoff = window.cutoff()?;
        let mut kept: Vec<Usage> = Vec::new();
        for row in read
            .scan()
            .primary::<Usage>()?
            .range(UsageLogKey::repository(repo.get())..UsageLogKey::after(repo.get()))?
            .rev()
        {
            let row = row?;
            if row.at < cutoff {
                break;
            }
            kept.push(row);
        }
        Ok(kept)
    }

    /// The newest `limit` rows of one repository's log, newest first: a lazy reverse range
    /// of the repository's own primary keys, where only the rows kept are decoded.
    fn newest_usage(&self, repo: RepoRowId, limit: u32) -> Result<Vec<Usage>> {
        let read = self.database.r_transaction()?;
        Ok(read
            .scan()
            .primary()?
            .range(UsageLogKey::repository(repo.get())..UsageLogKey::after(repo.get()))?
            .rev()
            .take(limit as usize)
            .collect::<native_db::db_type::Result<_>>()?)
    }
}

/// One artifact's use inside a sink's overflow: how many folded responses touched it and
/// the latest instant among them, applied the same path a written batch's own bump takes.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactOverflow {
    pub count: u64,
    pub last_used: Timestamp,
}

/// Inserts every event as one row numbered inside its repository's log, sorted by time so
/// the sequence stays chronological within the batch, and returns the count and latest
/// instant each artifact among them was used. `stored` is what [`read_repo_usage`] already
/// read for the repositories these events touch.
fn insert_usage_rows(
    transaction: &RwTransaction<'_>,
    events: &[UsageEvent],
    stored: &BTreeMap<i64, Option<RepoUsage>>,
) -> Result<BTreeMap<i64, (u64, Timestamp)>> {
    let mut by_repo: BTreeMap<i64, Vec<&UsageEvent>> = BTreeMap::new();
    for event in events {
        by_repo
            .entry(event.repo.map_or(0, RepoRowId::get))
            .or_default()
            .push(event);
    }
    let mut bumps: BTreeMap<i64, (u64, Timestamp)> = BTreeMap::new();
    for (repo_id, mut ordered) in by_repo {
        ordered.sort_by_key(|event| event.at);
        let mut seq = sequence_after(
            transaction,
            repo_id,
            stored.get(&repo_id).and_then(Option::as_ref),
        )?;
        for event in ordered {
            seq += 1;
            transaction.insert(usage_row(repo_id, seq, event))?;
            if let Some(artifact) = event.artifact {
                let slot = bumps.entry(artifact.get()).or_insert((0, event.at));
                slot.0 += 1;
                slot.1 = slot.1.max(event.at);
            }
        }
    }
    Ok(bumps)
}

/// Applies every artifact bump a batch's own rows earned, logging each for the index where
/// its floored minute moved.
fn apply_bumps(
    transaction: &RwTransaction<'_>,
    counters: &mut Counters,
    bumps: BTreeMap<i64, (u64, Timestamp)>,
) -> Result<()> {
    for (artifact_id, (count, last_used)) in bumps {
        if bump_artifact_counts(transaction, artifact_id, count, last_used)? {
            counters.pending.push(artifact_id);
        }
    }
    Ok(())
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

/// One served response as the log stores it, keyed by the repository that served it (0 for
/// a response naming none) and the sequence it was given inside that repository's log. Both
/// are recoverable from the key, so the row itself does not repeat them.
///
/// `at` is written at fixed nanosecond precision rather than jiff's default, which trims
/// trailing zeros, so a trimmed second does not sort after a fractional one inside the same
/// second and put the window and the pull list out of time order.
fn usage_row(repo_id: i64, seq: u64, event: &UsageEvent) -> Usage {
    Usage {
        key: UsageLogKey::at(repo_id, seq),
        at: format!("{:.9}", event.at),
        client: event.client.clone(),
        artifact_id: event.artifact.map(ArtifactId::get),
        path: event.path.clone(),
        bytes: event.bytes,
        rate_bps: event.rate_bps,
        status: event.status,
    }
}

/// Every repository's maintained record among `repo_ids`, read once so the sequence it
/// seeds and the tally that moves it decode the same row rather than each other's copy of
/// it: a repository holding thousands of client strings otherwise pays for that decode
/// twice in the same write.
fn read_repo_usage(
    transaction: &RwTransaction<'_>,
    repo_ids: impl IntoIterator<Item = i64>,
) -> Result<BTreeMap<i64, Option<RepoUsage>>> {
    repo_ids
        .into_iter()
        .map(|repo_id| Ok((repo_id, transaction.get().primary(repo_id)?)))
        .collect()
}

/// The repository, or id 0, every event in a batch names.
fn touched_repo_ids(events: &[UsageEvent]) -> BTreeSet<i64> {
    events
        .iter()
        .map(|event| event.repo.map_or(0, RepoRowId::get))
        .collect()
}

/// The sequence a repository's log continues from, given the record [`read_repo_usage`]
/// already read for it: the higher of its `pulls` and the highest key its own rows already
/// carry. A repository never served, or one naming no repository (id 0, which the allocator
/// never hands out to a real one), starts at 0 either way.
///
/// The record alone is not enough: `Catalog::retire_source` removes a repository's rows
/// through `forget_usage_of` and then, in a separate transaction, its record through
/// `remove_repo_usage`. A batch landing in the gap between them writes rows and recreates
/// the record from nothing, which the second transaction then deletes; the next batch for
/// that id must not restart at 1 on top of rows that already exist. Probing the highest key
/// directly is the same high-water-mark defence `Counters::load` gives every other id in the
/// catalog, and it is one lazy reverse range rather than a second decode of the record.
fn sequence_after(
    transaction: &RwTransaction<'_>,
    repo_id: i64,
    stored: Option<&RepoUsage>,
) -> Result<u64> {
    let recorded = stored.map_or(0, |stored| stored.pulls);
    let highest = transaction
        .scan()
        .primary::<Usage>()?
        .range(UsageLogKey::repository(repo_id)..UsageLogKey::after(repo_id))?
        .next_back()
        .transpose()?
        .map_or(0, |row| row.key.sequence());
    Ok(recorded.max(highest))
}

/// Adds `added` pulls to one artifact's maintained use and moves `last_used` towards `at` if
/// it is newer, returning whether the floored minute the row now carries differs from the
/// one it carried before, which is what tells the caller to log the artifact for the index.
fn bump_artifact_counts(
    transaction: &RwTransaction<'_>,
    artifact_id: i64,
    added: u64,
    at: Timestamp,
) -> Result<bool> {
    let stored: Option<ArtifactUse> = transaction.get().primary(artifact_id)?;
    let previous = stored
        .as_ref()
        .and_then(|stored| stored.last_used.parse::<Timestamp>().ok());
    let latest = previous.map_or(at, |seen| seen.max(at));
    transaction.upsert(ArtifactUse {
        artifact_id,
        count: stored.map_or(0, |stored| stored.count) + added,
        last_used: latest.to_string(),
    })?;
    Ok(previous.map(floored_minute) != Some(floored_minute(latest)))
}

/// What one write adds to one repository's maintained record: built up by `tally_repos`
/// from the rows a batch is about to write, or by a caller across a sink boundary that
/// folded some of its rows into counts instead. Public so a sink outside this crate can
/// accumulate one without touching the log itself, and apply it later through
/// [`Catalog::log_usage_batch_with_overflow`].
#[derive(Debug, Clone)]
pub struct RepoTally {
    pulls: u64,
    clients: BTreeSet<String>,
    days: BTreeMap<String, u64>,
    last: Timestamp,
    today: Option<(i64, String)>,
}

impl RepoTally {
    /// An empty tally seeded with the first event's time, for the repository's `last_pull`
    /// to start from if nothing else moves it.
    pub fn new(first: Timestamp) -> Self {
        Self {
            pulls: 0,
            clients: BTreeSet::new(),
            days: BTreeMap::new(),
            last: first,
            today: None,
        }
    }

    /// Folds one event's time and client into the tally: `pulls`, the day it falls on and,
    /// up to `CLIENT_CAP`, the client's address.
    ///
    /// The cap is enforced here rather than only where a tally merges into the stored
    /// record, because a written batch is bounded by the sink's capacity but a tally an
    /// overload folds is not: it accumulates for the whole episode, and the client is the
    /// first forwarded address that parses, so it is remote-chosen. Checking `contains`
    /// before inserting, and keeping the day just computed rather than reformatting a
    /// timestamp that repeats for every event inside it, are what keep this cheap to call
    /// while a sink's fold holds a lock every recorder past the bound waits on.
    pub fn add(&mut self, at: Timestamp, client: Option<&str>) {
        self.pulls += 1;
        if let Some(client) = client
            && self.clients.len() < CLIENT_CAP
            && !self.clients.contains(client)
        {
            self.clients.insert(client.to_owned());
        }
        let day = at.as_second().div_euclid(SECONDS_PER_DAY);
        let date = match &self.today {
            Some((cached, date)) if *cached == day => date.clone(),
            _ => {
                let date = at.to_zoned(TimeZone::UTC).date().to_string();
                self.today = Some((day, date.clone()));
                date
            }
        };
        *self.days.entry(date).or_default() += 1;
        self.last = self.last.max(at);
    }

    /// Folds another tally's counts into this one, for a batch's own tally and a sink's
    /// overflow landing on the same repository in the one write that applies both.
    fn merge(&mut self, other: RepoTally) {
        self.pulls += other.pulls;
        for client in other.clients {
            if self.clients.len() >= CLIENT_CAP {
                break;
            }
            self.clients.insert(client);
        }
        for (date, pulls) in other.days {
            *self.days.entry(date).or_default() += pulls;
        }
        self.last = self.last.max(other.last);
    }
}

/// One day in seconds, which is how a UTC day boundary is told from a timestamp's own
/// second without formatting it.
const SECONDS_PER_DAY: i64 = 86_400;

/// Tallies every event that names a repository. An event naming none moves no maintained
/// record: `next_repo_sequence` still numbers its row, from the highest key already under
/// repository id 0 alone, but nothing ever reads a record under that id, so one is never
/// created for it.
fn tally_repos(events: &[UsageEvent]) -> BTreeMap<i64, RepoTally> {
    let mut tallies: BTreeMap<i64, RepoTally> = BTreeMap::new();
    for event in events {
        let Some(repo) = event.repo else {
            continue;
        };
        tallies
            .entry(repo.get())
            .or_insert_with(|| RepoTally::new(event.at))
            .add(event.at, event.client.as_deref());
    }
    tallies
}

/// Moves each repository's record by what the write added, in the transaction that wrote
/// the rows, so the count the USAGE tab reads is never derived from the log again. `stored`
/// is what [`read_repo_usage`] already read for these repositories, so this does not decode
/// a row a caller numbering the same write's sequence has already paid to decode.
fn write_repo_usage(
    transaction: &RwTransaction<'_>,
    tallies: BTreeMap<i64, RepoTally>,
    mut stored: BTreeMap<i64, Option<RepoUsage>>,
) -> Result<()> {
    for (repo_id, tally) in tallies {
        let stored = stored.remove(&repo_id).flatten();
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

    /// A full minute before the window opens, formatted the way a row's `at` is. A reverse
    /// range of a repository's log can stop as soon as it passes this text without risking a
    /// row that belongs inside the window: a minute of slack absorbs a row numbered by an
    /// earlier batch but stamped later than a row from a batch that landed after it, which
    /// the sequence-ordered key does not itself rule out across batch boundaries.
    fn cutoff(&self) -> Result<String> {
        let midnight: Timestamp = format!("{}T00:00:00Z", self.first())
            .parse()
            .map_err(|error| IndexError::Catalog(format!("usage window cutoff: {error}")))?;
        Ok(format!("{:.9}", midnight - Duration::from_secs(60)))
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

/// The rows a reader already selected by sequence, sorted newest first by their own time:
/// the key orders by insertion, which is chronological inside one batch but only insertion
/// order across two, so a page this small is worth an explicit sort rather than a promise
/// the key alone cannot keep.
fn pull_rows(rows: Vec<Usage>) -> Result<Vec<PullRow>> {
    let mut pulls: Vec<PullRow> = rows
        .into_iter()
        .map(|row| {
            let when = row.at.parse().map_err(|error| {
                IndexError::Catalog(format!(
                    "usage row {} has no time: {error}",
                    row.key.sequence()
                ))
            })?;
            Ok(PullRow {
                when,
                who: row.client,
                what: row.path,
                bytes: row.bytes,
                rate_bps: row.rate_bps,
            })
        })
        .collect::<Result<_>>()?;
    pulls.sort_by_key(|pull| std::cmp::Reverse(pull.when));
    Ok(pulls)
}

/// When an artifact was last served, read from a snapshot a caller already holds.
pub(super) fn last_used_in(
    snapshot: &native_db::transaction::RTransaction<'_>,
    artifact: i64,
) -> Result<Option<Timestamp>> {
    let stored: Option<ArtifactUse> = snapshot.get().primary(artifact)?;
    Ok(stored.and_then(|stored| stored.last_used.parse().ok()))
}

/// The minute an instant falls in, which is the grain the search index carries: an artifact
/// served all through one minute is logged for the index once rather than once per batch,
/// because `used:` compares an age in days and `sort=used` orders by the column, and neither
/// notices the difference. [`ArtifactUse::last_used`] itself keeps the exact instant.
pub(super) fn floored_minute(at: Timestamp) -> i64 {
    at.as_second() / 60 * 60
}
