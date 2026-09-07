use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use native_db::transaction::RwTransaction;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;

use hoard_core::{GitSha1, Sha256, State};

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, Override, Repository,
    SearchText, Source,
};
use super::naming::content_hashes;
use super::scan::{Counters, log_changes, owes_a_hash, preferred_entry};
use super::verdict::apply_override;

/// How many files one hashing pass reads and records at a time.
const HASH_BATCH: usize = 64;

/// How many digests may wait to be written before a reader has to wait for the writer.
const DIGEST_QUEUE: usize = 256;

/// What one hashing run did: the files it read, the artifacts two copies of one file
/// collapsed into, and the files it could not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HashReport {
    pub hashed: u64,
    pub merged: u64,
    pub unreadable: u64,
}

/// The content identity a sha256 makes, the same text an artifact identity is hashed from.
/// Two copies of one file share a record because they hash to this same key.
pub fn content_key(sha: &Sha256) -> u128 {
    xxhash_rust::xxh3::xxh3_128(format!("sha256:{sha}").as_bytes())
}

impl Catalog {
    /// Hashes every present entry that has no sha256 yet and records what it read.
    ///
    /// Reading and writing run at the same time: a Rayon pool reads the files and sends
    /// each digest down a bounded channel, and one writer drains it and commits every 64
    /// digests or whenever the channel runs dry, so a multi-gigabyte file holds up nothing
    /// but itself. An artifact identified by its path is re-keyed to its sha256 and merged
    /// into an artifact that already carries that identity. Returns what it did.
    pub fn hash_pending(
        &self,
        config: &IndexConfig,
        mut progress: impl FnMut(u64),
    ) -> Result<HashReport> {
        self.open_pass()?;
        let mut counters = Counters::load(&self.database, config.triage_confidence)?;
        let mut report = HashReport::default();
        let (sender, receiver) = mpsc::sync_channel::<Digest>(DIGEST_QUEUE);
        let recording = &mut counters;
        let merged = std::thread::scope(|scope| -> Result<u64> {
            let writing = scope.spawn(move || self.record_digests(receiver, recording));
            let read = self.read_digests(config, &sender, &mut progress);
            drop(sender);
            let merged = writing
                .join()
                .map_err(|_| IndexError::Catalog("the digest writer panicked".into()))??;
            report = read?;
            Ok(merged)
        })?;
        report.merged = merged;
        self.sweep_artifacts(&mut counters)?;
        self.close_pass()?;
        Ok(report)
    }

    /// Reads every file owing a hash, sending each digest on as it is read. Returns the
    /// files it read and the files it could not.
    fn read_digests(
        &self,
        config: &IndexConfig,
        sender: &mpsc::SyncSender<Digest>,
        progress: &mut impl FnMut(u64),
    ) -> Result<HashReport> {
        let mut report = HashReport::default();
        let mut after = 0;
        loop {
            let batch = self.pending_batch(after)?;
            let Some((last, _)) = batch.last() else {
                break;
            };
            after = last.id;
            let sent = AtomicU64::new(0);
            batch.par_iter().for_each(|(entry, path)| {
                match content_hashes(path, entry.size, config.lfs_threshold) {
                    Ok((sha256, git_sha1)) => {
                        let digest = Digest {
                            entry_id: entry.id,
                            sha256,
                            git_sha1,
                        };
                        if sender.send(digest).is_ok() {
                            sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(error) => {
                        tracing::debug!(path = %path.display(), %error, "file is unhashable");
                    }
                }
            });
            let read = sent.load(Ordering::Relaxed);
            report.hashed += read;
            report.unreadable += batch.len() as u64 - read;
            progress(report.hashed);
        }
        Ok(report)
    }

    /// Commits digests as they arrive, in batches of at most [`HASH_BATCH`], and returns
    /// how many artifacts two copies of one file collapsed into.
    fn record_digests(
        &self,
        receiver: mpsc::Receiver<Digest>,
        counters: &mut Counters,
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
            batch.clear();
        }
        Ok(merged)
    }

    /// How many present files are still waiting for a content hash, as the writers that
    /// stored them left each source's count.
    pub fn pending_hashes(&self) -> Result<u64> {
        let read = self.database.r_transaction()?;
        let mut owed = 0_u64;
        for source in read.scan().primary::<Source>()?.all()? {
            let source: Source = source?;
            owed = owed.saturating_add(source.pending_hashes);
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
                    Some(id) => reader
                        .record(id)?
                        .is_some_and(|record| record.path_count > 1),
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
                    let outcome = rekey_artifact(&transaction, id, &digest.sha256, counters)?;
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
        transaction.commit()?;
        Ok(merged)
    }
}

/// One file's content hashes, against the entry they were read for.
struct Digest {
    entry_id: i64,
    sha256: Sha256,
    git_sha1: Option<GitSha1>,
}

/// Which artifact the hashed entry belongs to now, and whether reaching it collapsed two
/// artifacts into one.
struct Rekeyed {
    artifact_id: i64,
    merged: bool,
}

/// Gives an artifact the identity its content proves. A record that already carries a
/// sha256 keeps it, and a model split across shards keeps the identity its anchor gave it,
/// because no single shard's hash names the union of them.
fn rekey_artifact(
    transaction: &RwTransaction<'_>,
    artifact_id: i64,
    sha: &Sha256,
    counters: &mut Counters,
) -> Result<Rekeyed> {
    let unchanged = Ok(Rekeyed {
        artifact_id,
        merged: false,
    });
    let Some(record) = transaction.get().primary::<ArtifactRecord>(artifact_id)? else {
        return unchanged;
    };
    if record.artifact.sha256.is_some() || record.shard_count.is_some() {
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
        artifact: hoard_core::Artifact {
            sha256: Some(*sha),
            ..record.artifact
        },
        ..record
    }
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
