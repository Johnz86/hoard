use std::collections::{BTreeSet, HashMap, HashSet};

use native_db::transaction::RwTransaction;

use hoard_core::State;

use crate::catalog::Catalog;
use crate::catalog::models::{
    ArtifactDetail, ArtifactRecord, Entry, EntryKey, LocationKey, Repository, Revision,
    RevisionText, SearchText, Source, remove_revision_cards,
};
use crate::catalog::usage::remove_repo_usage;
use crate::config::IndexConfig;
use crate::error::Result;

use super::counters::{Counters, log_changes};
use super::sources::is_scanned_source;
use super::{CHUNK, RescanReport};

impl Catalog {
    /// Retires each slot the refiling emptied: a revision no ref reaches and no entry names
    /// keeps its ordinal, because an entry names its revision by that ordinal and nothing
    /// can renumber them, but gives up its name, its commit, its metadata and its card. A
    /// repository whose branch advances on every scan then grows one empty slot per old
    /// head rather than a card and a commit per old head.
    pub(super) fn retire_emptied_revisions(&self, emptied: &BTreeSet<(i64, u32)>) -> Result<()> {
        for (repo_id, ordinal) in emptied {
            let transaction = self.database.rw_transaction()?;
            let Some(repository) = transaction.get().primary::<Repository>(*repo_id)? else {
                continue;
            };
            let slot = *ordinal as usize;
            let retiring = repository
                .revisions
                .get(slot)
                .is_some_and(|revision| !revision.retired && revision.refs.is_empty());
            if !retiring || self.revision_is_named(&transaction, *repo_id, *ordinal)? {
                continue;
            }
            let mut revisions = repository.revisions.clone();
            revisions[slot] = Revision::tombstone();
            transaction.upsert(Repository {
                revisions,
                ..repository
            })?;
            let key = LocationKey::revision(*repo_id, *ordinal);
            if let Some(card) = transaction.get().primary::<RevisionText>(key)? {
                transaction.remove(card)?;
            }
            transaction.commit()?;
        }
        Ok(())
    }

    /// Whether any entry is still filed under one revision of one repository.
    fn revision_is_named(
        &self,
        transaction: &RwTransaction<'_>,
        repo_id: i64,
        ordinal: u32,
    ) -> Result<bool> {
        Ok(transaction
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(LocationKey::revision(repo_id, ordinal))?
            .next()
            .transpose()?
            .is_some())
    }

    /// Deletes every stored source the configuration no longer names, with its repositories
    /// and entries.
    ///
    /// A whole-configuration rescan walked everything the configuration has, so a source row
    /// it did not walk is one that was dropped from the configuration rather than one this
    /// run was narrowed away from. The sources the mirror, the pull queue and the peers keep
    /// are theirs, not the configuration's, and are left alone. A source whose root is merely
    /// unmounted stays too: the configuration still names it.
    pub(super) fn retire_dropped_sources(
        &self,
        config: &IndexConfig,
        counters: &mut Counters,
        report: &mut RescanReport,
    ) -> Result<()> {
        let configured: HashSet<&str> = config
            .sources
            .iter()
            .map(|source| source.name.as_str())
            .collect();
        let read = self.database.r_transaction()?;
        let stored: Vec<Source> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        drop(read);
        let dropped: Vec<String> = stored
            .into_iter()
            .map(|source| source.name)
            .filter(|name| is_scanned_source(name) && !configured.contains(name.as_str()))
            .collect();
        for name in dropped {
            self.retire_source(&name, counters, report)?;
        }
        Ok(())
    }

    /// Deletes every row of one source: its entries, its repositories and the source itself.
    ///
    /// A path of a source the configuration dropped is not a path this node has stopped
    /// holding, it is a path this catalog has stopped being about, and a row left behind
    /// outlives the source that explains it: nothing names its repository's source, the
    /// repositories listing renders it under a default kind, the summary still counts it,
    /// and re-adding the source files every path a second time. The artifacts that lose
    /// their last path go with them, through the sweep that follows the pass, and so does
    /// each repository's served history, which nothing could name again.
    fn retire_source(
        &self,
        name: &str,
        counters: &mut Counters,
        report: &mut RescanReport,
    ) -> Result<()> {
        for repo_id in self.repositories_of_source(name)? {
            let read = self.database.r_transaction()?;
            let mut ids: Vec<i64> = Vec::new();
            for entry in read
                .scan()
                .secondary::<Entry>(EntryKey::location)?
                .start_with(LocationKey::repository(repo_id))?
            {
                let entry: Entry = entry?;
                ids.push(entry.id);
            }
            drop(read);
            for batch in ids.chunks(CHUNK) {
                let transaction = self.database.rw_transaction()?;
                let mut taken = Vec::new();
                for id in batch {
                    let Some(entry) = transaction.get().primary::<Entry>(*id)? else {
                        continue;
                    };
                    counters.derived.removed(&entry);
                    if let Some(artifact) = entry.artifact_id {
                        counters.touch(&transaction, artifact)?;
                        counters.pending.push(artifact);
                        taken.push(artifact);
                    }
                    transaction.remove(entry)?;
                    report.absent += 1;
                }
                sweep_taken(&transaction, taken, counters)?;
                log_changes(&transaction, counters)?;
                counters.derived.flush(&transaction)?;
                counters.settle_summary(&transaction)?;
                transaction.commit()?;
            }
            self.forget_usage_of(repo_id)?;
            let transaction = self.database.rw_transaction()?;
            remove_revision_cards(&transaction, repo_id)?;
            remove_repo_usage(&transaction, repo_id)?;
            if let Some(stored) = transaction.get().primary::<Repository>(repo_id)? {
                transaction.remove(stored)?;
            }
            transaction.commit()?;
        }
        let transaction = self.database.rw_transaction()?;
        if let Some(stored) = transaction.get().primary::<Source>(name.to_owned())? {
            transaction.remove(stored)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(in crate::catalog) fn sweep_artifacts(&self, counters: &mut Counters) -> Result<()> {
        let mut ids = counters.take_touched().into_iter();
        while ids.len() > 0 {
            let transaction = self.database.rw_transaction()?;
            for id in ids.by_ref().take(CHUNK) {
                counters.capture_summary(&transaction, id)?;
                if sweep_artifact(&transaction, id)? {
                    counters.pending.push(id);
                }
            }
            log_changes(&transaction, counters)?;
            counters.settle_summary(&transaction)?;
            transaction.commit()?;
        }
        self.summarise()
    }
}

/// Whether the sweep changed what the search index should hold for this artifact.
pub(in crate::catalog) fn sweep_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<bool> {
    let referencing = paths_of_artifact(transaction, id)?;
    let Some(record) = transaction.get().primary::<ArtifactRecord>(id)? else {
        return Ok(false);
    };
    match present_paths(&referencing, record.shard_count) {
        0 => {
            for entry in referencing {
                transaction.upsert(Entry {
                    artifact_id: None,
                    ..entry
                })?;
            }
            transaction.remove(record)?;
            if let Some(detail) = transaction.get().primary::<ArtifactDetail>(id)? {
                transaction.remove(detail)?;
            }
            if let Some(text) = transaction.get().primary::<SearchText>(id)? {
                transaction.remove(text)?;
            }
            return Ok(true);
        }
        paths if paths != record.path_count => {
            transaction.upsert(ArtifactRecord {
                path_count: paths,
                ..record
            })?;
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Marks every artifact of one revision for indexing, which is what a card the revision
/// shares changing means: the text belongs to documents that are otherwise untouched.
pub(in crate::catalog) fn log_revision_artifacts(
    transaction: &RwTransaction<'_>,
    repo_id: i64,
    revision: u32,
    counters: &mut Counters,
) -> Result<()> {
    for entry in transaction
        .scan()
        .secondary::<Entry>(EntryKey::location)?
        .start_with(LocationKey::revision(repo_id, revision))?
    {
        let entry: Entry = entry?;
        if let Some(id) = entry.artifact_id {
            counters.pending.push(id);
        }
    }
    Ok(())
}

/// Every path pointing at one artifact, which is the bound on recounting what it holds.
pub(in crate::catalog) fn paths_of_artifact(
    transaction: &RwTransaction<'_>,
    id: i64,
) -> Result<Vec<Entry>> {
    Ok(transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(id))?
        .collect::<native_db::db_type::Result<_>>()?)
}

/// How many copies of an artifact its paths amount to: the paths whose bytes are on a node,
/// counted in whole shard sets rather than in files.
pub(in crate::catalog) fn present_paths(referencing: &[Entry], shard_count: Option<u32>) -> u32 {
    let present = referencing
        .iter()
        .filter(|entry| entry.state != State::Remote)
        .count();
    let shards = usize::try_from(shard_count.unwrap_or(1))
        .unwrap_or(1)
        .max(1);
    u32::try_from(present.div_ceil(shards)).unwrap_or(u32::MAX)
}

pub(super) fn shard_count_of(
    read: &native_db::transaction::RTransaction<'_>,
    shards: &mut HashMap<i64, Option<u32>>,
    id: i64,
) -> Result<Option<u32>> {
    match shards.entry(id) {
        std::collections::hash_map::Entry::Occupied(slot) => Ok(*slot.get()),
        std::collections::hash_map::Entry::Vacant(slot) => {
            let record: Option<ArtifactRecord> = read.get().primary(id)?;
            Ok(*slot.insert(record.and_then(|record| record.shard_count)))
        }
    }
}

/// Recounts what each artifact a transaction took a path from still holds, in that same
/// transaction. The sweep at the end of the pass would do it, but a pass killed before it
/// reaches that sweep would leave a record counting a path no entry backs, and the next run
/// has nothing to tell it to look: the rows the killed pass wrote are the rows the walk now
/// skips. Sweeping here makes the sweep at the end of the pass a repeat that finds nothing.
pub(super) fn sweep_taken(
    transaction: &RwTransaction<'_>,
    mut taken: Vec<i64>,
    counters: &mut Counters,
) -> Result<()> {
    taken.sort_unstable();
    taken.dedup();
    for id in taken {
        if sweep_artifact(transaction, id)? {
            counters.pending.push(id);
        }
    }
    Ok(())
}
