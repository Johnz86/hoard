use std::collections::HashMap;
use std::path::PathBuf;

use native_db::transaction::{RTransaction, RwTransaction};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use serde::Serialize;

use hoard_core::{GitSha1, Sha256, State};

use crate::config::IndexConfig;
use crate::error::Result;
use crate::scan::content_hashes;

use super::Catalog;
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, Override, Repository,
    SearchText,
};
use super::scan::{Counters, log_changes};
use super::verdict::apply_override;

/// How many files one hashing pass reads and records at a time.
const HASH_BATCH: usize = 64;

/// What one hashing run did: the files it read, the artifacts two copies of one file
/// collapsed into, and the files it could not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HashReport {
    pub hashed: u64,
    pub merged: u64,
    pub unreadable: u64,
}

/// The content identity a sha256 makes, the same text an artifact identity is hashed from.
pub(super) fn content_key(sha: &Sha256) -> u128 {
    xxhash_rust::xxh3::xxh3_128(format!("sha256:{sha}").as_bytes())
}

/// Whether this row still owes the catalog a sha256: bytes on this disk that nothing has
/// read yet.
fn is_pending(entry: &Entry) -> bool {
    entry.sha256.is_none() && entry.state != State::Remote && entry.abs_path.is_some()
}

impl Catalog {
    /// Hashes every present entry that has no sha256 yet, in batches on a Rayon pool,
    /// recording a sha256 for each and a git blob sha1 for files under the LFS threshold.
    /// An artifact identified by its path is re-keyed to its sha256 and merged into an
    /// artifact that already carries that identity. Returns what it did.
    pub fn hash_pending(
        &self,
        config: &IndexConfig,
        mut progress: impl FnMut(u64),
    ) -> Result<HashReport> {
        let mut counters = Counters::load(&self.database)?;
        let mut report = HashReport::default();
        let mut after = 0;
        loop {
            let batch = self.pending_batch(after)?;
            let Some((last, _)) = batch.last() else {
                break;
            };
            after = last.id;
            let digests = hash_batch(&batch, config.lfs_threshold);
            report.hashed += digests.len() as u64;
            report.unreadable += (batch.len() - digests.len()) as u64;
            report.merged += self.record_batch(&digests, &mut counters)?;
            progress(report.hashed);
        }
        self.sweep_artifacts(&mut counters, config.triage_confidence)?;
        Ok(report)
    }

    /// How many present files are still waiting for a content hash.
    pub fn pending_hashes(&self) -> Result<u64> {
        Ok(self.pending_by_source()?.values().sum())
    }

    /// How many present files each nominated root still owes a content hash, by source name.
    pub(super) fn pending_by_source(&self) -> Result<HashMap<String, u64>> {
        let read = self.database.r_transaction()?;
        let sources = sources_by_repo(&read)?;
        let mut pending: HashMap<String, u64> = HashMap::with_capacity(sources.len());
        for entry in read.scan().primary::<Entry>()?.all()? {
            let entry = entry?;
            if !is_pending(&entry) {
                continue;
            }
            if let Some(source) = sources.get(&entry.repo_id) {
                *pending.entry((*source).to_owned()).or_default() += 1;
            }
        }
        Ok(pending)
    }

    /// The present entry holding each named content hash, one per hash, preferring the
    /// highest availability preference. A hash nobody holds is absent from the result.
    pub fn entries_by_sha256(
        &self,
        hashes: &[Sha256],
    ) -> Result<Vec<(Sha256, super::EntryDetail)>> {
        let snapshot = self.database.r_transaction()?;
        let node = self.manifest()?.node;
        let mut reader = super::read::Reader::new(&snapshot, &node);
        let mut found = Vec::with_capacity(hashes.len());
        for sha in hashes {
            let holders: Vec<Entry> = snapshot
                .scan()
                .secondary::<Entry>(EntryKey::content)?
                .start_with(Some(content_key(sha)))?
                .collect::<native_db::db_type::Result<_>>()?;
            let best = holders
                .into_iter()
                .filter(|entry| entry.sha256.as_ref() == Some(sha) && entry.state != State::Remote)
                .max_by_key(|entry| (entry.preference, std::cmp::Reverse(entry.id)));
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
    fn pending_batch(&self, after: i64) -> Result<Vec<(Entry, PathBuf)>> {
        let read = self.database.r_transaction()?;
        let mut batch = Vec::with_capacity(HASH_BATCH);
        for entry in read.scan().primary::<Entry>()?.range((after + 1)..)? {
            let entry = entry?;
            let Some(path) = entry.abs_path.clone().filter(|_| is_pending(&entry)) else {
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
                    let outcome = rekey_artifact(&transaction, id, &digest.sha256, counters)?;
                    merged += u64::from(outcome.merged);
                    Some(outcome.artifact_id)
                }
                None => None,
            };
            transaction.upsert(Entry {
                artifact_id,
                sha256: Some(digest.sha256),
                content: Some(content_key(&digest.sha256)),
                git_sha1: entry.git_sha1.or(digest.git_sha1),
                ..entry
            })?;
        }
        log_changes(&transaction, counters)?;
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

fn hash_batch(batch: &[(Entry, PathBuf)], threshold: u64) -> Vec<Digest> {
    batch
        .par_iter()
        .filter_map(
            |(entry, path)| match content_hashes(path, entry.size, threshold) {
                Ok((sha256, git_sha1)) => Some(Digest {
                    entry_id: entry.id,
                    sha256,
                    git_sha1,
                }),
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "file is unhashable");
                    None
                }
            },
        )
        .collect()
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
        counters.touched.insert(record.id);
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
    absorb(transaction, survivor, loser, identity, sha)?;
    counters.touched.insert(survivor_id);
    counters.touched.insert(loser_id);
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

/// The source each repository sits under, so a per-entry count reaches a source name
/// without reading its repository again.
fn sources_by_repo(read: &RTransaction<'_>) -> Result<HashMap<i64, String>> {
    let repositories: Vec<Repository> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    Ok(repositories
        .into_iter()
        .map(|repository| (repository.id, repository.source))
        .collect())
}
