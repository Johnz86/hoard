use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::{GitSha1, Sha256, State};

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};
use crate::hash::FileStamp;

use super::Catalog;
use super::hash_memo::{HashMemo, Remembered};
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, Override, Repository,
    SearchText, Source,
};
use super::naming::content_hashes;
use super::scan::{Counters, log_changes, owes_a_hash, preferred_entry};
use super::verdict::apply_override;

pub(super) const HASH_BATCH: usize = 64;

/// Raised by whoever wants a running pass to stop early: a hashing pass stops before each
/// file is handed to a worker, and once per megabyte of the file a worker is already
/// reading through `content_digests`, and a walk stops between units, keeping everything
/// they had already committed.
pub type Stop = Arc<AtomicBool>;

const DIGEST_QUEUE: usize = 256;

const PROGRESS_TICK: Duration = Duration::from_millis(500);

const RATE_SAMPLE_FLOOR: Duration = Duration::from_millis(250);

const RATE_UNKNOWN: u64 = u64::MAX;

/// What one hashing run did: the files it read, the artifacts two copies of one file
/// collapsed into, and the files it could not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HashReport {
    pub hashed: u64,
    pub merged: u64,
    pub unreadable: u64,
    /// The pass stopped with files still owing a hash.
    pub interrupted: bool,
}

/// Where a hashing pass has got to while it is still running: the files it has finished
/// and the bytes it has settled, each against what it was owed when it started, and what
/// it read them at over the last half second.
///
/// `bytes_done` counts a file the digest memo answered for as settled without reading it,
/// so a pass whose every file is recalled still reaches `bytes_total`. `rate_bps` counts
/// only bytes streamed off the disk, so a recall never inflates it, and it is absent until
/// the first timed sample of the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HashProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub rate_bps: Option<u64>,
}

/// The content identity a sha256 makes, the same text an artifact identity is hashed from.
/// Two copies of one file share a record because they hash to this same key.
pub fn content_key(sha: &Sha256) -> u128 {
    xxhash_rust::xxh3::xxh3_128(format!("sha256:{sha}").as_bytes())
}

#[derive(Debug)]
struct TickerStop<'a> {
    ending: &'a Mutex<bool>,
    ended: &'a Condvar,
}

impl Drop for TickerStop<'_> {
    fn drop(&mut self) {
        *held(self.ending) = true;
        self.ended.notify_all();
    }
}

fn held<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn per_second(moved: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs_f64();
    match seconds > 0.0 {
        true => (moved as f64 / seconds) as u64,
        false => 0,
    }
}

impl Catalog {
    /// Hashes every present entry that has no sha256 yet and records what it read.
    ///
    /// Uses `[index] hash_threads` workers, or the count `IndexConfig::hash_workers` derives
    /// when it is zero.
    /// Completed digests are committed while other files are still being read. Matching
    /// content identities merge into one artifact.
    pub fn hash_pending(
        &self,
        config: &IndexConfig,
        progress: impl FnMut(HashProgress) + Send,
    ) -> Result<HashReport> {
        self.hash_pending_until(config, &Stop::default(), progress)
    }

    /// Hashes every present entry owing a sha256, stopping once `stop` is raised.
    ///
    /// Checks cancellation before opening a pass, between files, and once per megabyte
    /// read by each worker. Queued digests and artifact bookkeeping finish before returning.
    /// An interrupted pass preserves committed work and can be resumed.
    ///
    /// `progress` is called as each file is handed to a worker and on a half-second tick
    /// while they read, so a pass moving one enormous file still reports what it is doing.
    pub fn hash_pending_until(
        &self,
        config: &IndexConfig,
        stop: &Stop,
        mut progress: impl FnMut(HashProgress) + Send,
    ) -> Result<HashReport> {
        if stop.load(Ordering::Relaxed) {
            return Ok(HashReport {
                interrupted: true,
                ..HashReport::default()
            });
        }
        self.open_pass()?;
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let mut report = HashReport::default();
        let owed = self.pending_debt()?;
        let opened = match owed.files {
            0 => None,
            _ => Some(HashMemo::open(self.memo_directory())?),
        };
        let memo = opened.as_ref();
        let (sender, receiver) = mpsc::sync_channel::<Digest>(DIGEST_QUEUE);
        let recording = &mut counters;
        let merged = std::thread::scope(|scope| -> Result<u64> {
            let writing = scope.spawn(move || self.record_digests(receiver, recording, memo));
            let read = self.read_digests(
                config,
                &Pass {
                    stop,
                    sender: &sender,
                    memo,
                    owed,
                },
                &mut progress,
            );
            drop(sender);
            let merged = writing
                .join()
                .map_err(|_| IndexError::Catalog("the digest writer panicked".into()))??;
            report = read?;
            Ok(merged)
        })?;
        report.merged = merged;
        self.sweep_artifacts(&mut counters)?;
        if !report.interrupted {
            self.close_pass()?;
        }
        Ok(report)
    }

    fn read_digests(
        &self,
        config: &IndexConfig,
        pass: &Pass<'_>,
        progress: &mut (impl FnMut(HashProgress) + Send),
    ) -> Result<HashReport> {
        let workers = config.hash_workers();
        let threshold = config.lfs_threshold;
        let stop = pass.stop;
        let files_total = pass.owed.files;
        let bytes_total = pass.owed.bytes;
        let (work_tx, work_rx) = mpsc::sync_channel::<Pending>(workers);
        let work_rx = Arc::new(Mutex::new(work_rx));
        let hashed = AtomicU64::new(0);
        let unreadable = AtomicU64::new(0);
        let bytes = AtomicU64::new(0);
        let recalled_bytes = AtomicU64::new(0);
        let rate = AtomicU64::new(RATE_UNKNOWN);
        let interrupted = AtomicBool::new(false);
        let reporting = Mutex::new(progress);
        let ending = Mutex::new(false);
        let ended = Condvar::new();
        let snapshot = || HashProgress {
            files_done: hashed.load(Ordering::Relaxed),
            files_total,
            bytes_done: bytes
                .load(Ordering::Relaxed)
                .saturating_add(recalled_bytes.load(Ordering::Relaxed)),
            bytes_total,
            rate_bps: match rate.load(Ordering::Relaxed) {
                RATE_UNKNOWN => None,
                measured => Some(measured),
            },
        };
        let report_progress = || {
            let mut sink = held(&reporting);
            (*sink)(snapshot());
        };
        let nudge_progress = || {
            if let Ok(mut sink) = reporting.try_lock() {
                (*sink)(snapshot());
            }
        };

        let paging = std::thread::scope(|scope| -> Result<()> {
            let mut crew = Vec::with_capacity(workers);
            for _ in 0..workers {
                let receiver = Arc::clone(&work_rx);
                let worker_sender = pass.sender.clone();
                let hashed = &hashed;
                let unreadable = &unreadable;
                let bytes = &bytes;
                let interrupted = &interrupted;
                crew.push(scope.spawn(move || {
                    loop {
                        let next = match receiver.lock() {
                            Ok(guard) => guard.recv(),
                            Err(poisoned) => poisoned.into_inner().recv(),
                        };
                        let Ok((entry, path, stamp)) = next else {
                            break;
                        };
                        if stop.load(Ordering::Relaxed) {
                            interrupted.store(true, Ordering::Relaxed);
                            break;
                        }
                        match content_hashes(&path, entry.size, threshold, stop, bytes) {
                            Ok(Some((sha256, git_sha1))) => {
                                let digest = Digest {
                                    entry_id: entry.id,
                                    sha256,
                                    git_sha1,
                                    stamp,
                                };
                                match worker_sender.send(digest) {
                                    Ok(()) => {
                                        hashed.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(_) => break,
                                }
                            }
                            Ok(None) => {
                                interrupted.store(true, Ordering::Relaxed);
                                break;
                            }
                            Err(error) => {
                                unreadable.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(
                                    path = %path.display(),
                                    %error,
                                    "file is unhashable"
                                );
                            }
                        }
                    }
                }));
            }
            drop(work_rx);
            scope.spawn(|| {
                let mut sampled = Instant::now();
                let mut seen = 0_u64;
                loop {
                    let finished = {
                        let waiting = held(&ending);
                        let waited =
                            match ended
                                .wait_timeout_while(waiting, PROGRESS_TICK, |ending| !*ending)
                            {
                                Ok((waited, _)) => waited,
                                Err(poisoned) => poisoned.into_inner().0,
                            };
                        *waited
                    };
                    let read = bytes.load(Ordering::Relaxed);
                    let elapsed = sampled.elapsed();
                    if elapsed >= RATE_SAMPLE_FLOOR {
                        rate.store(
                            per_second(read.saturating_sub(seen), elapsed),
                            Ordering::Relaxed,
                        );
                        sampled = Instant::now();
                        seen = read;
                    }
                    report_progress();
                    if finished {
                        break;
                    }
                }
            });
            let _stopping = TickerStop {
                ending: &ending,
                ended: &ended,
            };

            let pager = Pager {
                stop,
                work_tx: &work_tx,
                sender: pass.sender,
                memo: pass.memo,
                threshold,
                hashed: &hashed,
                recalled_bytes: &recalled_bytes,
                interrupted: &interrupted,
            };
            let walked = self.page_pending(&pager, nudge_progress);
            drop(work_tx);
            let lost = crew
                .into_iter()
                .fold(false, |lost, worker| lost | worker.join().is_err());
            walked?;
            match lost {
                true => Err(IndexError::Catalog("a hashing worker panicked".into())),
                false => Ok(()),
            }
        });
        paging?;
        report_progress();

        Ok(HashReport {
            hashed: hashed.load(Ordering::Relaxed),
            merged: 0,
            unreadable: unreadable.load(Ordering::Relaxed),
            interrupted: interrupted.load(Ordering::Relaxed),
        })
    }

    fn page_pending(&self, pager: &Pager<'_>, report: impl Fn()) -> Result<()> {
        let mut after = 0;
        'paging: loop {
            if pager.stop.load(Ordering::Relaxed) {
                pager.interrupted.store(true, Ordering::Relaxed);
                break 'paging;
            }
            let batch = self.pending_batch(after)?;
            let Some((last, _)) = batch.last() else {
                break 'paging;
            };
            after = last.id;
            for (entry, path) in batch {
                if pager.stop.load(Ordering::Relaxed) {
                    pager.interrupted.store(true, Ordering::Relaxed);
                    break 'paging;
                }
                let stamp = crate::hash::stamp(&path).ok();
                let listening = pager.queue(entry, path, stamp);
                if !listening {
                    pager.interrupted.store(true, Ordering::Relaxed);
                    break 'paging;
                }
                report();
            }
        }
        Ok(())
    }

    pub(super) fn memo_directory(&self) -> &Path {
        self.directory.parent().unwrap_or(&self.directory)
    }

    /// Commits digests as they arrive, in batches of at most [`HASH_BATCH`], and returns
    /// how many artifacts two copies of one file collapsed into.
    fn record_digests(
        &self,
        receiver: mpsc::Receiver<Digest>,
        counters: &mut Counters,
        memo: Option<&HashMemo>,
    ) -> Result<u64> {
        let mut merged = 0;
        let mut batch: Vec<Digest> = Vec::with_capacity(HASH_BATCH);
        while let Ok(digest) = receiver.recv() {
            batch.push(digest);
            while batch.len() < HASH_BATCH {
                match receiver.try_recv() {
                    Ok(next) => batch.push(next),
                    Err(_) => break,
                }
            }
            merged += self.record_batch(&batch, counters)?;
            remember_digests(memo, &batch);
            batch.clear();
        }
        Ok(merged)
    }

    /// How many present files are still waiting for a content hash, as the writers that
    /// stored them left each source's count.
    pub fn pending_hashes(&self) -> Result<u64> {
        Ok(self.pending_debt()?.files)
    }

    /// How many bytes those files hold, which is what a hashing pass actually has to read.
    pub fn pending_hash_bytes(&self) -> Result<u64> {
        Ok(self.pending_debt()?.bytes)
    }

    /// Both counts from one scan of one snapshot, so a writer committing between two reads
    /// cannot leave a pass reporting bytes and files that disagree about what it was owed.
    fn pending_debt(&self) -> Result<PendingDebt> {
        let read = self.database.r_transaction()?;
        let mut owed = PendingDebt::default();
        for source in read.scan().primary::<Source>()?.all()? {
            let source: Source = source?;
            owed.files = owed.files.saturating_add(source.pending_hashes);
            owed.bytes = owed.bytes.saturating_add(source.pending_bytes);
        }
        Ok(owed)
    }

    /// The present entry holding each named content hash, one per hash, preferring the
    /// highest availability preference. A hash nobody holds is absent from the result.
    pub fn entries_by_sha256(
        &self,
        hashes: &[Sha256],
    ) -> Result<Vec<(Sha256, super::EntryDetail)>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = super::read::Reader::new(&snapshot, &names);
        let mut found = Vec::with_capacity(hashes.len());
        for sha in hashes {
            let holders: Vec<Entry> = snapshot
                .scan()
                .secondary::<Entry>(EntryKey::content)?
                .start_with(Some(content_key(sha)))?
                .collect::<native_db::db_type::Result<_>>()?;
            let best = preferred_entry(holders.into_iter().filter(|entry| {
                entry.sha256.as_ref() == Some(sha) && entry.state != State::Remote
            }));
            if let Some(entry) = best {
                let deduped = match entry.artifact_id {
                    Some(id) => reader.record(id)?.is_some_and(|record| record.copies > 1),
                    None => false,
                };
                found.push((*sha, reader.detail(&entry, deduped)?));
            }
        }
        Ok(found)
    }

    /// The next files owing a hash, in primary key order, with the path to read them from.
    /// A row that carries no path of its own is placed under its repository's root, so the
    /// roots the batch spans are read once each rather than once per file.
    fn pending_batch(&self, after: i64) -> Result<Vec<(Entry, PathBuf)>> {
        let read = self.database.r_transaction()?;
        let mut roots: HashMap<i64, String> = HashMap::new();
        let mut batch = Vec::with_capacity(HASH_BATCH);
        for entry in read.scan().primary::<Entry>()?.range((after + 1)..)? {
            let entry: Entry = entry?;
            if !owes_a_hash(&entry) {
                continue;
            }
            let root = match roots.entry(entry.repo_id) {
                std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let stored: Option<Repository> = read.get().primary(entry.repo_id)?;
                    slot.insert(stored.map(|stored| stored.root_path).unwrap_or_default())
                }
            };
            let Some(path) = entry.placed.resolve(root, &entry.rel_path) else {
                continue;
            };
            batch.push((entry, PathBuf::from(path)));
            if batch.len() == HASH_BATCH {
                break;
            }
        }
        Ok(batch)
    }

    /// Writes one batch of digests, re-keying every path identity the hashes settle and
    /// merging the artifacts two copies of one file turn out to be. Returns the merges.
    fn record_batch(&self, digests: &[Digest], counters: &mut Counters) -> Result<u64> {
        let transaction = self.database.rw_transaction()?;
        let mut merged = 0;
        for digest in digests {
            let Some(entry) = transaction.get().primary::<Entry>(digest.entry_id)? else {
                continue;
            };
            let artifact_id = match entry.artifact_id {
                Some(id) => {
                    counters.touch(&transaction, id)?;
                    let outcome =
                        rekey_artifact(&transaction, id, &digest.sha256, entry.id, counters)?;
                    merged += u64::from(outcome.merged);
                    Some(outcome.artifact_id)
                }
                None => None,
            };
            counters.derived.removed(&entry);
            let hashed = Entry {
                artifact_id,
                sha256: Some(digest.sha256),
                content: Some(content_key(&digest.sha256)),
                git_sha1: entry.git_sha1.or(digest.git_sha1),
                ..entry
            };
            counters.derived.added(&hashed);
            transaction.upsert(hashed)?;
        }
        log_changes(&transaction, counters)?;
        counters.derived.flush(&transaction)?;
        counters.settle_summary(&transaction)?;
        transaction.commit()?;
        Ok(merged)
    }
}

/// One file's content hashes, against the entry they were read for. `stamp` is what the
/// digests should be remembered against, and is absent both for a file that could not be
/// stat'd and for a digest the memo was the source of, which has nothing to learn.
struct Digest {
    entry_id: i64,
    sha256: Sha256,
    git_sha1: Option<GitSha1>,
    stamp: Option<FileStamp>,
}

/// A file handed to a worker: the entry owing the hash, the path to read it from, and the
/// stamp the digest it produces is remembered against, absent when the file could not be
/// stat'd.
type Pending = (Entry, PathBuf, Option<FileStamp>);

/// What a pass is owed when it opens: the files still carrying no sha256 and the bytes
/// they hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PendingDebt {
    files: u64,
    bytes: u64,
}

/// What every stage of one pass shares: where a finished digest goes, the memo it is
/// recalled from and remembered in, and what the pass was owed when it started.
struct Pass<'a> {
    stop: &'a Stop,
    sender: &'a mpsc::SyncSender<Digest>,
    memo: Option<&'a HashMemo>,
    owed: PendingDebt,
}

/// What a pass's pager hands each candidate to: the queue the workers read, the channel a
/// digest the memo already knows skips straight to, and the counts both of them raise.
///
/// `recalled_bytes` carries what the memo answered for, which no worker streams and so no
/// read loop counts. Progress owes those bytes to the total either way, while the rate a
/// pass reports stays a measure of bytes actually read off the disk.
struct Pager<'a> {
    stop: &'a Stop,
    work_tx: &'a mpsc::SyncSender<Pending>,
    sender: &'a mpsc::SyncSender<Digest>,
    memo: Option<&'a HashMemo>,
    threshold: u64,
    hashed: &'a AtomicU64,
    recalled_bytes: &'a AtomicU64,
    interrupted: &'a AtomicBool,
}

impl Pager<'_> {
    fn queue(&self, entry: Entry, path: PathBuf, stamp: Option<FileStamp>) -> bool {
        let Some((sha256, git_sha1)) = self.recalled(&entry, &path, stamp) else {
            return self.work_tx.send((entry, path, stamp)).is_ok();
        };
        let sent = self
            .sender
            .send(Digest {
                entry_id: entry.id,
                sha256,
                git_sha1,
                stamp: None,
            })
            .is_ok();
        if sent {
            self.hashed.fetch_add(1, Ordering::Relaxed);
            self.recalled_bytes.fetch_add(entry.size, Ordering::Relaxed);
        }
        sent
    }

    fn recalled(
        &self,
        entry: &Entry,
        path: &Path,
        stamp: Option<FileStamp>,
    ) -> Option<(Sha256, Option<GitSha1>)> {
        match self.memo?.recall(stamp?, entry.size < self.threshold) {
            Ok(recalled) => recalled,
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    %error,
                    "the digest memo could not be read"
                );
                None
            }
        }
    }
}

/// Remembers what a batch read, in one transaction, after the catalog committed it. A memo
/// that cannot be written does not fail the pass: the files it forgot are read again.
fn remember_digests(memo: Option<&HashMemo>, batch: &[Digest]) {
    let Some(memo) = memo else {
        return;
    };
    let rows: Vec<Remembered> = batch
        .iter()
        .filter_map(|digest| {
            digest
                .stamp
                .map(|stamp| (stamp, digest.sha256, digest.git_sha1))
        })
        .collect();
    if let Err(error) = memo.remember_batch(&rows) {
        tracing::debug!(rows = rows.len(), %error, "the digest memo could not be written");
    }
}

/// Which artifact the hashed entry belongs to now, and whether reaching it collapsed two
/// artifacts into one.
struct Rekeyed {
    artifact_id: i64,
    merged: bool,
}

/// Gives an artifact the identity its content proves. A record that already carries a
/// sha256 keeps it, and a model split across shards keeps the identity its anchor gave it,
/// because no single shard's hash names the union of them; the anchor's digest is still
/// written onto such a record, because that is the sha every row of a set shows.
fn rekey_artifact(
    transaction: &RwTransaction<'_>,
    artifact_id: i64,
    sha: &Sha256,
    entry_id: i64,
    counters: &mut Counters,
) -> Result<Rekeyed> {
    let unchanged = Ok(Rekeyed {
        artifact_id,
        merged: false,
    });
    let Some(record) = transaction.get().primary::<ArtifactRecord>(artifact_id)? else {
        return unchanged;
    };
    if record.artifact.sha256.is_some() {
        return unchanged;
    }
    if record.shard_count.is_some() {
        if anchored_by(transaction, record.id, entry_id)? {
            counters.touch(transaction, record.id)?;
            transaction.upsert(digested(record, sha))?;
            counters.pending.push(artifact_id);
        }
        return unchanged;
    }
    let identity = content_key(sha);
    let claimed: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, identity)?;
    let Some(claimed) = claimed.filter(|claimed| claimed.id != record.id) else {
        counters.touch(transaction, record.id)?;
        counters.pending.push(record.id);
        transaction.upsert(identified(record, identity, sha))?;
        return unchanged;
    };
    let (survivor, loser) = match record.id < claimed.id {
        true => (record, claimed),
        false => (claimed, record),
    };
    let survivor_id = survivor.id;
    let loser_id = loser.id;
    counters.touch(transaction, survivor_id)?;
    counters.touch(transaction, loser_id)?;
    absorb(transaction, survivor, loser, identity, sha)?;
    counters.pending.push(survivor_id);
    counters.pending.push(loser_id);
    Ok(Rekeyed {
        artifact_id: survivor_id,
        merged: true,
    })
}

fn identified(record: ArtifactRecord, identity: u128, sha: &Sha256) -> ArtifactRecord {
    ArtifactRecord {
        identity,
        ..digested(record, sha)
    }
}

/// The same record carrying the digest of its bytes, keeping the identity it was filed
/// under: a set is identified by what its anchor was, not by what one file hashes to.
fn digested(record: ArtifactRecord, sha: &Sha256) -> ArtifactRecord {
    ArtifactRecord {
        artifact: hoard_core::Artifact {
            sha256: Some(*sha),
            ..record.artifact
        },
        ..record
    }
}

/// Whether this entry is the set's anchor: its lowest-numbered shard, which a scan stores
/// before every member of the set and which therefore holds the lowest entry id of them.
fn anchored_by(transaction: &RwTransaction<'_>, artifact_id: i64, entry_id: i64) -> Result<bool> {
    let members: Vec<Entry> = transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(artifact_id))?
        .collect::<native_db::db_type::Result<_>>()?;
    Ok(members.iter().map(|entry| entry.id).min() == Some(entry_id))
}

/// Moves every path of the losing artifact onto the surviving one and removes what hung off
/// it. A verdict the loser carried follows its paths unless the survivor has one already.
fn absorb(
    transaction: &RwTransaction<'_>,
    survivor: ArtifactRecord,
    loser: ArtifactRecord,
    identity: u128,
    sha: &Sha256,
) -> Result<()> {
    let paths: Vec<Entry> = transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(loser.id))?
        .collect::<native_db::db_type::Result<_>>()?;
    for entry in paths {
        transaction.upsert(Entry {
            artifact_id: Some(survivor.id),
            ..entry
        })?;
    }
    let verdict: Option<Override> = transaction.get().primary(loser.id)?;
    if let Some(detail) = transaction.get().primary::<ArtifactDetail>(loser.id)? {
        transaction.remove(detail)?;
    }
    if let Some(text) = transaction.get().primary::<SearchText>(loser.id)? {
        transaction.remove(text)?;
    }
    if let Some(verdict) = &verdict {
        transaction.remove(verdict.clone())?;
    }
    transaction.remove(loser)?;
    let mut survivor = identified(survivor, identity, sha);
    let carried: Option<Override> = transaction.get().primary(survivor.id)?;
    if let (Some(verdict), None) = (verdict, carried) {
        let verdict = Override {
            artifact_id: survivor.id,
            ..verdict
        };
        apply_override(&mut survivor, &verdict);
        transaction.upsert(verdict)?;
    }
    transaction.upsert(survivor)?;
    Ok(())
}
