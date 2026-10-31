use std::collections::{BTreeSet, HashMap, HashSet};

use native_db::transaction::RwTransaction;

use hoard_core::State;

use crate::catalog::Catalog;
use crate::catalog::models::{
    ArtifactDetail, ArtifactRecord, Entry, EntryKey, LocationKey, Manifest, Repository, Revision,
    RevisionText, SearchText, Source, remove_revision_cards,
};
use crate::catalog::usage::remove_repo_usage;
use crate::config::IndexConfig;
use crate::error::{IndexError, Result};

use super::counters::{Counters, log_changes};
use super::sources::is_scanned_source;
use super::{CHUNK, RescanReport};

impl Catalog {
    /// Removes a nominated source and its catalog metadata without touching disk files or
    /// scanning other sources. Callers must serialize this with scans of the same catalog.
    pub fn remove_source(&self, name: &str) -> Result<()> {
        if !is_scanned_source(name) {
            return Err(IndexError::Config(format!(
                "source {name} is managed by Hoard"
            )));
        }
        self.settle_interrupted_pass()?;
        if self
            .database
            .r_transaction()?
            .get()
            .primary::<Source>(name.to_owned())?
            .is_none()
        {
            self.refresh_search(|_| {})?;
            return Ok(());
        }
        self.open_pass()?;
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        self.retire_source(name, &mut counters, &mut RescanReport::default())?;
        self.sweep_artifacts(&mut counters)?;
        let transaction = self.database.rw_transaction()?;
        counters.save(&transaction)?;
        let mut manifest: Manifest = transaction
            .get()
            .primary(0_u8)?
            .ok_or_else(|| IndexError::Catalog("manifest missing".into()))?;
        manifest.generation += 1;
        manifest.counts = super::counts(&transaction)?;
        manifest.pass_open = false;
        transaction.upsert(manifest)?;
        transaction.commit()?;
        self.refresh_search(|_| {})?;
        Ok(())
    }

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
///
/// An artifact no present path backs is removed and its entries are orphaned, which is what
/// a scan that no longer finds a file means. [`recount_artifact`] is the half of this a
/// writer wants when a path was retired rather than lost.
pub(in crate::catalog) fn sweep_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<bool> {
    let referencing = paths_of_artifact(transaction, id)?;
    let Some(record) = transaction.get().primary::<ArtifactRecord>(id)? else {
        return Ok(false);
    };
    if present_paths(&referencing, record.shard_count) == 0 {
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
    if recount(transaction, record, &referencing)? {
        return Ok(true);
    }
    Ok(false)
}

/// Rewrites one artifact's path and copy counts from the entries that reference it, leaving
/// the record itself alone however few paths are left.
///
/// A retired path is a path this node stopped serving, not one that stopped existing: a peer
/// may still hold every copy, and the artifact has to stay in the index to say so. That is
/// the whole difference from [`sweep_artifact`].
pub(in crate::catalog) fn recount_artifact(
    transaction: &RwTransaction<'_>,
    id: i64,
) -> Result<bool> {
    let referencing = paths_of_artifact(transaction, id)?;
    let Some(record) = transaction.get().primary::<ArtifactRecord>(id)? else {
        return Ok(false);
    };
    recount(transaction, record, &referencing)
}

/// Whether the record's stored counts had to move to match its entries.
fn recount(
    transaction: &RwTransaction<'_>,
    record: ArtifactRecord,
    referencing: &[Entry],
) -> Result<bool> {
    let paths = present_paths(referencing, record.shard_count);
    let copies = distinct_paths(referencing, record.shard_count);
    if paths == record.path_count && copies == record.copies {
        return Ok(false);
    }
    transaction.upsert(ArtifactRecord {
        path_count: paths,
        copies,
        ..record
    })?;
    Ok(true)
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
fn present_paths(referencing: &[Entry], shard_count: Option<u32>) -> u32 {
    let present = referencing
        .iter()
        .filter(|entry| entry.state != State::Remote)
        .count();
    in_whole_sets(present, shard_count)
}

/// How many real copies of an artifact its present paths amount to: two paths sharing one
/// inode are one file reached twice, and deleting either reclaims nothing. A path with no
/// captured inode counts as its own copy rather than being merged with another's.
fn distinct_paths(referencing: &[Entry], shard_count: Option<u32>) -> u32 {
    let mut inodes: HashSet<u64> = HashSet::new();
    let mut unstamped = 0usize;
    for entry in referencing
        .iter()
        .filter(|entry| entry.state != State::Remote)
    {
        match entry.inode {
            Some(inode) => {
                inodes.insert(inode);
            }
            None => unstamped += 1,
        }
    }
    in_whole_sets(inodes.len() + unstamped, shard_count)
}

fn in_whole_sets(files: usize, shard_count: Option<u32>) -> u32 {
    let shards = usize::try_from(shard_count.unwrap_or(1))
        .unwrap_or(1)
        .max(1);
    u32::try_from(files.div_ceil(shards)).unwrap_or(u32::MAX)
}

pub(super) fn shard_count_of(
    read: &native_db::transaction::RTransaction<'_>,
    shards: &mut HashMap<i64, u32>,
    id: i64,
) -> Result<Option<u32>> {
    if let Some(count) = shards.get(&id) {
        return Ok(Some(*count));
    }
    let record: Option<ArtifactRecord> = read.get().primary(id)?;
    let count = record.and_then(|record| record.shard_count);
    if let Some(count) = count {
        shards.insert(id, count);
    }
    Ok(count)
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

#[cfg(test)]
mod tests {
    use crate::catalog::models::Placement;

    use super::*;

    fn entry(id: i64, inode: Option<u64>, state: State) -> Entry {
        Entry {
            id,
            repo_id: 1,
            artifact_id: Some(1),
            revision: 0,
            rel_path: format!("weights-{id}.safetensors"),
            fingerprint: None,
            placed: Placement::UnderRoot,
            size: 1_024,
            state,
            triage: false,
            preference: 0,
            lfs_pointer: false,
            is_symlink: false,
            sha256: None,
            content: None,
            git_sha1: None,
            mtime: None,
            inode,
            node: 1,
            pass: 0,
            pull_progress: None,
        }
    }

    #[test]
    fn a_path_with_no_captured_inode_counts_as_its_own_copy() {
        let unstamped = [
            entry(1, None, State::Served),
            entry(2, None, State::Served),
            entry(3, None, State::Local),
        ];
        assert_eq!(
            distinct_paths(&unstamped, None),
            3,
            "nothing proves two unstamped paths are one file, so neither is merged away"
        );

        let mixed = [
            entry(1, Some(90), State::Local),
            entry(2, Some(90), State::Local),
            entry(3, None, State::Served),
        ];
        assert_eq!(
            distinct_paths(&mixed, None),
            2,
            "one inode reached twice is one copy, and the unstamped path is another"
        );
    }

    #[test]
    fn a_remote_path_is_neither_a_copy_nor_a_present_path() {
        let entries = [
            entry(1, Some(90), State::Local),
            entry(2, None, State::Remote),
            entry(3, Some(91), State::Remote),
        ];
        assert_eq!(distinct_paths(&entries, None), 1);
        assert_eq!(present_paths(&entries, None), 1);
    }

    #[test]
    fn copies_are_counted_in_whole_shard_sets() {
        let two_sets_of_three: Vec<Entry> = (1..=6)
            .map(|id| entry(id, Some(100 + id as u64), State::Local))
            .collect();
        assert_eq!(distinct_paths(&two_sets_of_three, Some(3)), 2);

        let one_set_reached_twice: Vec<Entry> = (1..=6)
            .map(|id| entry(id, Some(100 + (id as u64 % 3)), State::Local))
            .collect();
        assert_eq!(
            distinct_paths(&one_set_reached_twice, Some(3)),
            1,
            "a three shard set reached through two directories is one copy of the model"
        );
    }
}
