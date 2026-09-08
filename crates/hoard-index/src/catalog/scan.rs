use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use native_db::Database;
use native_db::transaction::{RTransaction, RwTransaction};
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
use serde::Serialize;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, BlobId, CommitSha, GitSha1, Sha256, State, TensorSummary,
};

use crate::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use crate::discover::{self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, RevisionName};
use crate::hash::FileStamp;
use crate::infer::{self, InferInput, Inference};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};
use crate::sniff::{self, SniffResult};

use super::hashing::{Stop, content_key};
use super::models::{
    Allocator, ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE,
    LocationKey, Manifest, Node, Override, Placement, Ref, Repository, RepositoryKey, Revision,
    RevisionText, SearchChange, SearchText, Source, Summary, move_revision_card,
    remove_revision_cards, repository_key, store_revision_card,
};
use super::naming::{
    InferenceContext, Naming, artifact_name, content_hashes, disambiguate_labels, index_document,
    inference_path, shard_label,
};
use super::usage::remove_repo_usage;
use super::verdict::apply_override;
use crate::error::{IndexError, Result};

use super::{Catalog, builder, models};

impl Catalog {
    /// Creates a fresh catalog directly from configured sources. Existing output
    /// directories are refused. Discovery supports the existing source layouts;
    /// a model split across shard files is one artifact anchored on its lowest shard.
    pub fn scan_sources(
        directory: &Path,
        node: &str,
        config: &IndexConfig,
        policy: HashPolicy,
        progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        let (directory, sources) = validate_output(directory, config)?;
        let mut catalog = Self::create(directory)?;
        match catalog
            .fill(node, &sources, ScanSettings::new(config, policy), progress)
            .and_then(|()| catalog.compact())
        {
            Ok(()) => Ok(catalog),
            Err(error) => {
                let directory = catalog.directory.clone();
                drop(catalog);
                remove_partial(&directory);
                Err(error)
            }
        }
    }

    /// Walks the configured sources again. An unchanged file (same inode, mtime and size)
    /// is never opened; a changed or new file is prepared as in a fresh scan; a file that
    /// is gone keeps its row with state `Remote`. Ids continue from the allocator, the
    /// generation increments, and the search index must be refreshed afterwards. The file is
    /// not compacted, so a shared catalog can rescan without an exclusive borrow.
    ///
    /// `scope` says whether `config` is the whole configuration or a narrowing of it. Only a
    /// whole-configuration rescan retires a source the configuration no longer names.
    ///
    /// A rescan records the triage threshold it ran with, which every later writer counts
    /// against. A threshold that moved leaves each artifact's stored state alone until this
    /// walk restates it, and counts the summary again so what it holds is what the stored
    /// threshold says.
    pub fn rescan(
        &self,
        config: &IndexConfig,
        policy: HashPolicy,
        scope: RescanScope,
        progress: impl FnMut(&str, u64),
    ) -> Result<RescanReport> {
        self.rescan_until(config, policy, scope, &Stop::default(), progress)
    }

    /// Walks the configured sources again, stopping once `stop` is raised: before each
    /// source is discovered, and between the units of the one it is inside.
    ///
    /// An interrupted walk keeps every unit it committed and retires nothing: the paths it
    /// never reached are paths it never looked for, not paths this node stopped holding. It
    /// leaves the pass mark set, so the next walk settles the counts it did not.
    pub fn rescan_until(
        &self,
        config: &IndexConfig,
        policy: HashPolicy,
        scope: RescanScope,
        stop: &Stop,
        progress: impl FnMut(&str, u64),
    ) -> Result<RescanReport> {
        let sources = available_sources(config)?;
        self.open_pass()?;
        let restated = self.stored_triage()? != config.triage_confidence;
        let mut counters = Counters::load(&self.database, config.triage_confidence)?;
        let KnownRows { known, sizes } = self.known_stamps(&sources)?;
        let mut walk = Walk {
            settings: ScanSettings::new(config, policy),
            known,
            paths: HashMap::new(),
            paths_of: None,
            sizes,
            discovered: HashSet::new(),
            seen: HashSet::new(),
            moved: Vec::new(),
            report: RescanReport::default(),
        };
        self.walk(&sources, &mut walk, &mut counters, stop, progress)?;
        match (scope, walk.report.interrupted) {
            (RescanScope::Configured, false) => {
                self.retire_dropped_sources(config, &mut counters, &mut walk.report)?;
            }
            (RescanScope::Configured, true) | (RescanScope::Narrowed, _) => {}
        }
        self.sweep_artifacts(&mut counters)?;
        let transaction = self.database.rw_transaction()?;
        counters.save(&transaction)?;
        if !walk.report.interrupted {
            let mut manifest: Manifest = transaction
                .get()
                .primary(0_u8)?
                .ok_or_else(|| IndexError::Catalog("manifest missing".into()))?;
            manifest.generation += 1;
            manifest.scanned_at = jiff::Timestamp::now().to_string();
            manifest.counts = counts(&transaction)?;
            manifest.triage_confidence = config.triage_confidence;
            manifest.pass_open = false;
            transaction.upsert(manifest)?;
        }
        transaction.commit()?;
        if restated && !walk.report.interrupted {
            self.recount_summary()?;
        }
        Ok(walk.report)
    }

    /// Claims a fresh directory, leaving nothing behind if the database cannot be created.
    fn create(directory: PathBuf) -> Result<Self> {
        std::fs::create_dir(&directory)?;
        match builder().create(models()?, directory.join("catalog.redb")) {
            Ok(database) => {
                let catalog = Self {
                    database,
                    directory,
                    search_cache: std::sync::RwLock::new(None),
                    search_lane: parking_lot::Mutex::new(()),
                };
                catalog.seed_builtin_shelves()?;
                Ok(catalog)
            }
            Err(error) => {
                remove_partial(&directory);
                Err(error.into())
            }
        }
    }

    fn fill(
        &mut self,
        node: &str,
        sources: &[&SourceConfig],
        settings: ScanSettings,
        progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        self.ensure_self_row(node)?;
        let mut counters = Counters::load(&self.database, settings.triage)?;
        let mut walk = Walk {
            settings,
            known: HashMap::new(),
            paths: HashMap::new(),
            paths_of: None,
            sizes: HashMap::new(),
            discovered: HashSet::new(),
            seen: HashSet::new(),
            moved: Vec::new(),
            report: RescanReport::default(),
        };
        self.walk(
            sources,
            &mut walk,
            &mut counters,
            &Stop::default(),
            progress,
        )?;
        self.sweep_artifacts(&mut counters)?;
        let transaction = self.database.rw_transaction()?;
        counters.save(&transaction)?;
        transaction.insert(Manifest {
            id: 0,
            format_version: super::FORMAT_VERSION,
            generation: 1,
            scanned_at: jiff::Timestamp::now().to_string(),
            node: node.to_owned(),
            counts: counts(&transaction)?,
            search_directory: None,
            search_generation: 0,
            search_seq: 0,
            triage_confidence: settings.triage,
            pass_open: false,
        })?;
        transaction.commit()?;
        self.repair_summary()?;
        Ok(())
    }

    /// The one pass a fresh scan and a rescan share. A fresh scan starts with nothing known,
    /// so every unit is prepared; a rescan starts with the stamps it stored last time.
    fn walk(
        &self,
        sources: &[&SourceConfig],
        walk: &mut Walk,
        counters: &mut Counters,
        stop: &Stop,
        mut progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        let noise = self.noise_filter()?;
        'walking: for source in sources {
            if stop.load(Ordering::Relaxed) {
                walk.report.interrupted = true;
                break 'walking;
            }
            progress(&format!("discovering {}", source.name), counters.entries);
            let discovered = discover::discover(source, &noise)?;
            let kind = discovered.kind();
            let walked = self.store_source(source, kind, discovered.repos(), counters)?;
            walk.discovered.extend(
                walked
                    .iter()
                    .flat_map(|repo| repo.ordinals.iter().map(|ordinal| (repo.id, *ordinal))),
            );
            for (repo, walked) in discovered.repos().iter().zip(walked) {
                for (revision, ordinal) in repo.revisions.iter().zip(walked.ordinals) {
                    let found = RevisionEntries {
                        repo_id: walked.id,
                        ordinal,
                        entries: &revision.entries,
                    };
                    if unchanged_revision(walk, found) {
                        mark_unchanged(walk, found, 0..revision.entries.len());
                        continue;
                    }
                    if !walk.sizes.contains_key(&(walked.id, ordinal))
                        && walk.paths_of != Some(walked.id)
                    {
                        walk.paths = self.repository_paths(walked.id)?;
                        walk.paths_of = Some(walked.id);
                    }
                    let context = EntryContext {
                        repo,
                        repo_id: walked.id,
                        revision,
                        settings: walk.settings,
                        kind,
                        ordinal,
                    };
                    let documents = revision_documents(&revision.entries);
                    let sets = shard_sets_by_name(&revision.entries, &documents);
                    let units = revision_units(&revision.entries, sets);
                    let plan = revision_plan(&context, documents, &units);
                    let labels = set_labels(&plan.naming, &units);
                    let mut card_written = false;
                    for (unit, label) in units.iter().zip(labels) {
                        if stop.load(Ordering::Relaxed) {
                            walk.report.interrupted = true;
                            break 'walking;
                        }
                        let prepare = match decide_unit(walk, found, unit) {
                            UnitDecision::Skip => {
                                mark_unchanged(walk, found, unit.positions().iter().copied());
                                continue;
                            }
                            UnitDecision::Prepare { prepare, unchanged } => {
                                mark_unchanged(walk, found, unchanged);
                                prepare
                            }
                        };
                        let narrowed = match unit {
                            Unit::Set(_) => None,
                            Unit::Chunk(_) => Some(Unit::Chunk(prepare.clone())),
                        };
                        let prepared = prepare_unit(
                            &context,
                            &plan,
                            narrowed.as_ref().unwrap_or(unit),
                            label.as_deref(),
                            stop,
                        )?;
                        if changed_while_prepared(&prepared) {
                            mark_unchanged(walk, found, prepare.iter().copied());
                            continue;
                        }
                        let transaction = self.database.rw_transaction()?;
                        if !std::mem::replace(&mut card_written, true)
                            && store_revision_card(
                                &transaction,
                                walked.id,
                                ordinal,
                                plan.context.card().unwrap_or_default(),
                            )?
                        {
                            log_revision_artifacts(&transaction, walked.id, ordinal, counters)?;
                        }
                        let taken = replace_known(&transaction, walk, counters, found, &prepare)?;
                        store_unit(&transaction, &context, prepared, counters)?;
                        sweep_taken(&transaction, taken, counters)?;
                        log_changes(&transaction, counters)?;
                        counters.derived.flush(&transaction)?;
                        counters.settle_summary(&transaction)?;
                        transaction.commit()?;
                        progress(&source.name, counters.entries);
                    }
                }
            }
        }
        self.settle_moved(walk, counters)?;
        if walk.report.interrupted {
            return Ok(());
        }
        self.retract_absent(walk, counters)?;
        self.record_scan_stats(sources, counters)
    }

    /// Refiles the unchanged entries of a revision the walk found at another ordinal.
    ///
    /// An entry names its revision by ordinal, so a revision the catalog had not seen -
    /// a git clone whose branch advanced onto a fresh commit - takes a new slot while every
    /// file it holds is the file the catalog already read. Only the ordinal changes: the
    /// bytes, the state and the presence are the ones already recorded, so no derived count
    /// moves and only the search document behind the row is logged.
    fn settle_moved(&self, walk: &mut Walk, counters: &mut Counters) -> Result<()> {
        let moved = std::mem::take(&mut walk.moved);
        let mut carried: HashSet<(i64, u32, u32)> = HashSet::new();
        let mut emptied: BTreeSet<(i64, u32)> = BTreeSet::new();
        for batch in moved.chunks(CHUNK) {
            let transaction = self.database.rw_transaction()?;
            for (entry_id, ordinal) in batch {
                let Some(entry) = transaction.get().primary::<Entry>(*entry_id)? else {
                    continue;
                };
                if entry.revision != *ordinal {
                    if carried.insert((entry.repo_id, entry.revision, *ordinal)) {
                        move_revision_card(&transaction, entry.repo_id, entry.revision, *ordinal)?;
                    }
                    emptied.insert((entry.repo_id, entry.revision));
                }
                if let Some(artifact) = entry.artifact_id {
                    counters.pending.push(artifact);
                }
                transaction.upsert(Entry {
                    revision: *ordinal,
                    ..entry
                })?;
            }
            log_changes(&transaction, counters)?;
            transaction.commit()?;
        }
        self.retire_emptied_revisions(&emptied)
    }

    /// Retires each slot the refiling emptied: a revision no ref reaches and no entry names
    /// keeps its ordinal, because an entry names its revision by that ordinal and nothing
    /// can renumber them, but gives up its name, its commit, its metadata and its card. A
    /// repository whose branch advances on every scan then grows one empty slot per old
    /// head rather than a card and a commit per old head.
    fn retire_emptied_revisions(&self, emptied: &BTreeSet<(i64, u32)>) -> Result<()> {
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

    /// Records what each walked source holds now that every unit of it is stored and the
    /// paths the walk never reached have been retracted: the files still on this disk and
    /// the bytes they occupy, summed from the repositories under it.
    fn record_scan_stats(&self, sources: &[&SourceConfig], counters: &mut Counters) -> Result<()> {
        let at = jiff::Timestamp::now().to_string();
        let transaction = self.database.rw_transaction()?;
        counters.derived.flush(&transaction)?;
        counters.settle_summary(&transaction)?;
        let repositories: Vec<Repository> = transaction
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let held = held_by_source(&repositories);
        for source in sources {
            let Some(stored) = transaction.get().primary::<Source>(source.name.clone())? else {
                continue;
            };
            let (file_count, byte_count) =
                held.get(source.name.as_str()).copied().unwrap_or_default();
            transaction.upsert(Source {
                file_count,
                byte_count,
                last_scan: Some(at.clone()),
                ..stored
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Writes each scanned source's held files and bytes again, which the end of a walk does
    /// and a walk that was killed did not reach. A source the mirror, the pull queue or a
    /// peer owns keeps counts of its own and is left alone; `last_scan` is not written
    /// either, because the scan it names did not finish.
    fn recount_source_counts(&self) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let repositories: Vec<Repository> = transaction
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let held = held_by_source(&repositories);
        let sources: Vec<Source> = transaction
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        for stored in sources {
            if !is_scanned_source(&stored.name) {
                continue;
            }
            let (file_count, byte_count) =
                held.get(stored.name.as_str()).copied().unwrap_or_default();
            if (file_count, byte_count) == (stored.file_count, stored.byte_count) {
                continue;
            }
            transaction.upsert(Source {
                file_count,
                byte_count,
                ..stored
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Writes the records a source needs before any of its files are read, and hands back
    /// the identity and revision ordinals given to each repository in discovery order. A
    /// repository the catalog already holds keeps its id and takes the freshly discovered
    /// revisions and refs into the slots [`merged_revisions`] settled.
    fn store_source(
        &self,
        source: &SourceConfig,
        kind: SourceKind,
        repos: &[DiscoveredRepo],
        counters: &mut Counters,
    ) -> Result<Vec<WalkedRepo>> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Source> = transaction.get().primary(source.name.clone())?;
        let carried = stored.unwrap_or_else(|| Source {
            name: source.name.clone(),
            config: source.clone(),
            resolved_kind: kind,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        });
        transaction.upsert(Source {
            config: source.clone(),
            resolved_kind: kind,
            ..carried
        })?;
        let mut walked = Vec::with_capacity(repos.len());
        for repo in repos {
            let key = repository_key(&source.name, &repo.repo);
            let stored: Option<Repository> = transaction
                .get()
                .secondary(RepositoryKey::key, key.as_str())?;
            let (id, present, held) = match stored {
                Some(stored) => (
                    stored.id,
                    (stored.present_files, stored.present_bytes),
                    stored.revisions,
                ),
                None => (counters.next_repo()?, (0, 0), Vec::new()),
            };
            let (revisions, slots) = merged_revisions(&held, repo);
            let ordinals = slots
                .into_iter()
                .map(|slot| {
                    u32::try_from(slot)
                        .map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))
                })
                .collect::<Result<Vec<u32>>>()?;
            transaction.upsert(repository(id, &source.name, repo, present, revisions))?;
            walked.push(WalkedRepo { id, ordinals });
        }
        settle_repositories(&transaction)?;
        transaction.commit()?;
        Ok(walked)
    }

    /// Deletes every stored source the configuration no longer names, with its repositories
    /// and entries.
    ///
    /// A whole-configuration rescan walked everything the configuration has, so a source row
    /// it did not walk is one that was dropped from the configuration rather than one this
    /// run was narrowed away from. The sources the mirror, the pull queue and the peers keep
    /// are theirs, not the configuration's, and are left alone. A source whose root is merely
    /// unmounted stays too: the configuration still names it.
    fn retire_dropped_sources(
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

    /// Every path the catalog already holds for the selected sources, with the stamp that
    /// proves it unchanged and the facts a decision needs before any file is opened, and how
    /// many of them each stored revision holds.
    ///
    /// Keyed by [`location_hash`] rather than by the location itself: a million relative
    /// paths held as text is the largest thing a rescan keeps in memory, and nothing here
    /// reads a location back.
    fn known_stamps(&self, sources: &[&SourceConfig]) -> Result<KnownRows> {
        let names: HashSet<&str> = sources.iter().map(|source| source.name.as_str()).collect();
        let read = self.database.r_transaction()?;
        let repositories: Vec<Repository> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut shards: HashMap<i64, Option<u32>> = HashMap::new();
        let mut known: KnownPaths = HashMap::new();
        let mut sizes: RevisionSizes = HashMap::new();
        for repository in repositories
            .iter()
            .filter(|repository| names.contains(repository.source.as_str()))
        {
            for entry in read
                .scan()
                .secondary::<Entry>(EntryKey::location)?
                .start_with(LocationKey::repository(repository.id))?
            {
                let entry: Entry = entry?;
                if entry.placed.is_absent() {
                    continue;
                }
                let (Some(mtime), Some(inode)) = (entry.mtime, entry.inode) else {
                    continue;
                };
                let shard_count = match entry.artifact_id {
                    Some(id) => shard_count_of(&read, &mut shards, id)?,
                    None => None,
                };
                let location = location_hash(repository.id, entry.revision, &entry.rel_path);
                known.insert(
                    location,
                    Known {
                        entry_id: entry.id,
                        artifact_id: entry.artifact_id.unwrap_or_default(),
                        stamp: FileStamp {
                            inode,
                            mtime,
                            size: entry.size,
                        },
                        revision: entry.revision,
                        shard_count: shard_count.unwrap_or_default(),
                    },
                );
                *sizes.entry((repository.id, entry.revision)).or_default() += 1;
            }
        }
        Ok(KnownRows { known, sizes })
    }

    /// Where one repository holds each relative path it has a row for, by [`path_hash`].
    ///
    /// Only a revision the catalog has no rows for is refiled from, so only a repository
    /// holding one pays for this index, and only one repository's worth of it is held at a
    /// time. A path more than one revision holds maps to nothing, because which of them
    /// moved is not knowable from the path.
    fn repository_paths(&self, repo_id: i64) -> Result<PathIndex> {
        let read = self.database.r_transaction()?;
        let mut paths: PathIndex = HashMap::new();
        for entry in read
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(LocationKey::repository(repo_id))?
        {
            let entry: Entry = entry?;
            if entry.placed.is_absent() || entry.mtime.is_none() || entry.inode.is_none() {
                continue;
            }
            let location = location_hash(repo_id, entry.revision, &entry.rel_path);
            paths
                .entry(path_hash(repo_id, &entry.rel_path))
                .and_modify(|held| *held = None)
                .or_insert(Some(location));
        }
        Ok(paths)
    }

    /// A path the walk never reached is still a row: it keeps the path it had and records
    /// that this node no longer holds the bytes. Written in `CHUNK`-sized transactions,
    /// because a source that was unmounted retracts every path it held and a transaction
    /// holds every row it writes until it commits.
    fn retract_absent(&self, walk: &mut Walk, counters: &mut Counters) -> Result<()> {
        let missing: Vec<Known> = walk
            .known
            .values()
            .filter(|known| !walk.seen.contains(&known.entry_id))
            .copied()
            .collect();
        for batch in missing.chunks(CHUNK) {
            let transaction = self.database.rw_transaction()?;
            let mut taken = Vec::new();
            for known in batch {
                let Some(entry) = transaction.get().primary::<Entry>(known.entry_id)? else {
                    continue;
                };
                counters.derived.removed(&entry);
                transaction.upsert(Entry {
                    state: State::Remote,
                    preference: preference(State::Remote, false),
                    mtime: None,
                    inode: None,
                    ..entry
                })?;
                if let Some(id) = known.artifact() {
                    counters.touch(&transaction, id)?;
                    counters.pending.push(id);
                    taken.push(id);
                }
                walk.report.absent += 1;
            }
            sweep_taken(&transaction, taken, counters)?;
            log_changes(&transaction, counters)?;
            counters.derived.flush(&transaction)?;
            counters.settle_summary(&transaction)?;
            transaction.commit()?;
        }
        Ok(())
    }

    pub(super) fn sweep_artifacts(&self, counters: &mut Counters) -> Result<()> {
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

    pub(super) fn summarise(&self) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        settle_repositories(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// The totals a fresh count of every artifact record gives, from one read snapshot,
    /// without writing anything.
    ///
    /// An artifact reached by more than one path wastes every copy but the first; an
    /// artifact no path reaches at all is not indexed bytes on this node.
    pub fn count_summary(&self) -> Result<super::Totals> {
        Ok(super::totals_of(&self.counted_summary()?))
    }

    /// Recounts and stores the totals, returning what was stored before and what is stored
    /// now. Exclusive by signature: nothing else holds the catalog while it runs.
    pub fn repair_summary(&mut self) -> Result<(super::Totals, super::Totals)> {
        let before = self.totals()?;
        let after = self.recount_summary()?;
        Ok((before, after))
    }

    /// Replaces the running totals with a fresh count of every artifact record.
    fn recount_summary(&self) -> Result<super::Totals> {
        let counted = self.counted_summary()?;
        let totals = super::totals_of(&counted);
        let transaction = self.database.rw_transaction()?;
        transaction.upsert(counted)?;
        transaction.commit()?;
        Ok(totals)
    }

    /// Counts again what the last pass to run did not finish settling, and clears the mark.
    pub(super) fn settle_interrupted_pass(&self) -> Result<()> {
        if !self.manifest()?.pass_open {
            return Ok(());
        }
        self.settle_counts()?;
        self.mark_pass(false).map(|_| ())
    }

    /// Marks a pass as running and settles what a pass killed before it left behind. Called
    /// by every pass that commits more than once.
    pub(super) fn open_pass(&self) -> Result<()> {
        match self.mark_pass(true)? {
            true => self.settle_counts(),
            false => Ok(()),
        }
    }

    /// What a pass settles at its end rather than in the transactions that earn it: the
    /// totals, and what each scanned source holds. Every other count a pass moves is written
    /// in the transaction that moves the rows behind it.
    fn settle_counts(&self) -> Result<()> {
        self.recount_summary()?;
        self.recount_source_counts()
    }

    /// Clears the pass mark, for a pass whose last transaction is not the manifest's.
    pub(super) fn close_pass(&self) -> Result<()> {
        self.mark_pass(false).map(|_| ())
    }

    /// Writes the pass mark, returning what it held before.
    fn mark_pass(&self, open: bool) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let manifest: Manifest =
            transaction
                .get()
                .primary(0_u8)?
                .ok_or_else(|| IndexError::CatalogIncomplete {
                    directory: self.directory.clone(),
                })?;
        let held = manifest.pass_open;
        transaction.upsert(Manifest {
            pass_open: open,
            ..manifest
        })?;
        transaction.commit()?;
        Ok(held)
    }

    fn counted_summary(&self) -> Result<Summary> {
        let triage = self.stored_triage()?;
        let read = self.database.r_transaction()?;
        let mut summary = Summary {
            repositories: read.len().primary::<Repository>()?,
            ..blank_summary()
        };
        for record in read.scan().primary::<ArtifactRecord>()?.all()? {
            let record: ArtifactRecord = record?;
            shift_summary(
                &mut summary,
                SummaryPart::default(),
                SummaryPart::of(Some(&record), triage),
            );
        }
        Ok(summary)
    }
}

/// What each source holds, summed from the repositories under it: the files still on this
/// disk and the bytes they occupy.
fn held_by_source(repositories: &[Repository]) -> HashMap<&str, (u64, u64)> {
    let mut held: HashMap<&str, (u64, u64)> = HashMap::new();
    for repository in repositories {
        let slot = held.entry(repository.source.as_str()).or_default();
        slot.0 += u64::from(repository.present_files);
        slot.1 = slot.1.saturating_add(repository.present_bytes);
    }
    held
}

/// Whether the sweep changed what the search index should hold for this artifact.
pub(super) fn sweep_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<bool> {
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
pub(super) fn log_revision_artifacts(
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
pub(super) fn paths_of_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<Vec<Entry>> {
    Ok(transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(id))?
        .collect::<native_db::db_type::Result<_>>()?)
}

/// How many copies of an artifact its paths amount to: the paths whose bytes are on a node,
/// counted in whole shard sets rather than in files.
pub(super) fn present_paths(referencing: &[Entry], shard_count: Option<u32>) -> u32 {
    let present = referencing
        .iter()
        .filter(|entry| entry.state != State::Remote)
        .count();
    let shards = usize::try_from(shard_count.unwrap_or(1))
        .unwrap_or(1)
        .max(1);
    u32::try_from(present.div_ceil(shards)).unwrap_or(u32::MAX)
}

fn shard_count_of(
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

pub(super) fn counts(transaction: &RwTransaction<'_>) -> Result<Vec<(String, u64)>> {
    Ok(vec![
        ("sources".into(), transaction.len().primary::<Source>()?),
        (
            "repositories".into(),
            transaction.len().primary::<Repository>()?,
        ),
        (
            "artifacts".into(),
            transaction.len().primary::<ArtifactRecord>()?,
        ),
        ("entries".into(), transaction.len().primary::<Entry>()?),
    ])
}

fn remove_partial(directory: &Path) {
    if let Err(cleanup) = std::fs::remove_dir_all(directory) {
        tracing::warn!(
            path = %directory.display(),
            %cleanup,
            "partial catalog left behind"
        );
    }
}

fn validate_output<'a>(
    directory: &Path,
    config: &'a IndexConfig,
) -> Result<(PathBuf, Vec<&'a SourceConfig>)> {
    let selected = available_sources(config)?;
    if directory.exists() {
        return Err(IndexError::Catalog(
            "output directory already exists; choose a fresh directory".into(),
        ));
    }
    let parent = directory
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = directory
        .file_name()
        .ok_or_else(|| IndexError::Catalog("output needs a directory name".into()))?;
    let output = parent.canonicalize()?.join(name);
    for source in &selected {
        let Ok(root) = source.path.canonicalize() else {
            continue;
        };
        if output.starts_with(&root) {
            return Err(IndexError::Catalog(format!(
                "catalog output is inside scanned source {:?}",
                source.name
            )));
        }
    }
    Ok((output, selected))
}

/// The configured sources this node can actually walk, in configuration order. A root that
/// is not mounted is skipped with a warning; two sources of one name are a configuration
/// error, because a repository's identity is its source and its name. A configuration that
/// names no walkable source yields an empty catalog rather than an error, so opening one
/// beside the index never fails on an unmounted disk; a command that means to scan refuses
/// an empty list itself.
fn available_sources(config: &IndexConfig) -> Result<Vec<&SourceConfig>> {
    let mut names = HashSet::with_capacity(config.sources.len());
    let mut selected = Vec::with_capacity(config.sources.len());
    for source in &config.sources {
        if !names.insert(&source.name) {
            return Err(IndexError::Catalog(format!(
                "duplicate source name: {}",
                source.name
            )));
        }
        if let Err(error) = source.path.canonicalize() {
            tracing::warn!(
                source = %source.name,
                path = %source.path.display(),
                %error,
                "source root is unavailable; skipping"
            );
            continue;
        }
        selected.push(source);
    }
    Ok(selected)
}

/// What one pass over the sources has settled so far: which stored rows it recognised, and
/// how many files it left alone, read again, added or retracted.
struct Walk {
    settings: ScanSettings,
    known: KnownPaths,
    /// The paths of the one repository a fresh revision slot is being refiled from, and
    /// which repository they are, so no more than one repository's index is ever held.
    paths: PathIndex,
    paths_of: Option<i64>,
    sizes: RevisionSizes,
    /// Every revision slot this walk discovered, by repository and ordinal, so a row
    /// sitting on a revision the walk is about to reach is not refiled onto another.
    discovered: HashSet<(i64, u32)>,
    seen: HashSet<i64>,
    /// Entries whose revision moved to another ordinal under them, with the ordinal it
    /// moved to. Written once at the end of the pass rather than once per unit.
    moved: Vec<(i64, u32)>,
    report: RescanReport,
}

/// One repository of a walked source: the row it was filed under, and the ordinal each of
/// its discovered revisions takes in that row.
struct WalkedRepo {
    id: i64,
    ordinals: Vec<u32>,
}

/// The discovered entries of one revision of one repository, with the identity the catalog
/// files them under. A stored row is recognised by where it sits, not by the absolute path
/// its bytes were last found at, which for an hf-cache entry is a blob a rewrite replaces.
#[derive(Clone, Copy)]
struct RevisionEntries<'a> {
    repo_id: i64,
    ordinal: u32,
    entries: &'a [DiscoveredEntry],
}

impl RevisionEntries<'_> {
    fn location(&self, position: usize) -> u128 {
        location_hash(self.repo_id, self.ordinal, &self.entries[position].rel_path)
    }
}

/// What the catalog already holds for one path, so a walk can decide without opening it.
///
/// The two absent cases are spelled as zero rather than as an `Option` discriminant: ids
/// come from the allocator and are positive, and a set of no shards is not a set.
#[derive(Clone, Copy)]
struct Known {
    entry_id: i64,
    artifact_id: i64,
    stamp: FileStamp,
    revision: u32,
    shard_count: u32,
}

impl Known {
    fn artifact(self) -> Option<i64> {
        (self.artifact_id != 0).then_some(self.artifact_id)
    }

    fn shards(self) -> Option<u32> {
        (self.shard_count != 0).then_some(self.shard_count)
    }
}

/// Every path of the selected sources the catalog holds bytes for, by [`location_hash`].
type KnownPaths = HashMap<u128, Known>;

/// Where a repository holds one relative path, by [`path_hash`]: the location of the one
/// row that holds it, or nothing when more than one revision of the repository does. It is
/// what a revision rediscovered at a fresh ordinal is refiled from, and it holds a key
/// rather than a [`Known`] so the rows themselves are kept once.
type PathIndex = HashMap<u128, Option<u128>>;

/// How many of those paths each stored revision holds, by repository and ordinal.
type RevisionSizes = HashMap<(i64, u32), usize>;

/// What the catalog already holds for the sources a rescan is about to walk.
struct KnownRows {
    known: KnownPaths,
    sizes: RevisionSizes,
}

/// Whether the configuration a rescan was handed is the whole of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanScope {
    /// Every source the configuration names, so one it does not name has been dropped.
    Configured,
    /// A subset the caller chose, so a source outside it was simply not walked.
    Narrowed,
}

/// Whether a source row belongs to the configured scan rather than to the mirror, the pull
/// queue or a peer, each of which keeps its own source row that no scan owns.
fn is_scanned_source(name: &str) -> bool {
    name != super::MIRROR_SOURCE
        && name != super::PULL_SOURCE
        && !name.starts_with(super::PEER_SOURCE)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RescanReport {
    pub unchanged: u64,
    pub changed: u64,
    pub added: u64,
    pub absent: u64,
    /// The walk stopped between units with sources left unwalked, so it retired nothing.
    pub interrupted: bool,
}

enum UnitDecision {
    Skip,
    Prepare {
        prepare: Vec<usize>,
        unchanged: Vec<usize>,
    },
}

fn known_of(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> Option<Known> {
    match walk.known.get(&found.location(position)) {
        Some(known) => Some(*known),
        None => refiled_of(walk, found, position),
    }
}

/// The row another revision of the same repository holds this path at, when the catalog
/// holds no row at all for the revision the walk is in.
///
/// An entry names its revision by ordinal, so a revision the catalog had not seen - a clone
/// whose branch advanced onto a fresh commit - takes a fresh slot while every file it holds
/// is the file the catalog already read. Recognising those rows is what refiles them onto
/// the new ordinal rather than reading them again and retracting the ones they replace.
///
/// The stamp has to match, so only a file that did not change is refiled, and a path more
/// than one stored revision holds is never refiled: which of them moved is not knowable
/// from the path, and taking the wrong one would empty the revision it is filed under. A
/// row another revision of this walk has already accounted for is left to it, and so is a
/// row whose own revision this walk discovered: that revision is still on this disk, so its
/// row moved nowhere, however alike the two paths look. Two hf-cache snapshots linking one
/// blob are exactly that pair.
fn refiled_of(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> Option<Known> {
    if walk.sizes.contains_key(&(found.repo_id, found.ordinal)) {
        return None;
    }
    let entry = &found.entries[position];
    let stamp = entry.stamp?;
    let held = (*walk.paths.get(&path_hash(found.repo_id, &entry.rel_path))?)?;
    let known = walk.known.get(&held)?;
    (known.stamp == stamp
        && !walk.seen.contains(&known.entry_id)
        && !walk.discovered.contains(&(found.repo_id, known.revision)))
    .then_some(*known)
}

/// Where a row sits, as the key a rescan looks it up by: the repository, the revision
/// ordinal and the relative path, which is the identity the catalog files an entry under
/// and the one a file rewritten in place keeps. Two distinct locations colliding on 128
/// bits is not a case this workspace budgets for.
pub(super) fn location_hash(repo_id: i64, ordinal: u32, rel_path: &str) -> u128 {
    let seed = repo_id.cast_unsigned().rotate_left(32) ^ u64::from(ordinal);
    xxhash_rust::xxh3::xxh3_128_with_seed(rel_path.as_bytes(), seed)
}

/// One relative path of one repository, whichever of its revisions holds it, as the key the
/// refile lookup uses.
fn path_hash(repo_id: i64, rel_path: &str) -> u128 {
    xxhash_rust::xxh3::xxh3_128_with_seed(rel_path.as_bytes(), repo_id.cast_unsigned())
}

/// Whether nothing in a revision needs opening: it holds every path the catalog holds bytes
/// for and no others, each at the same size, inode and mtime. The count is as much of the
/// condition as the stamps are, because a revision that lost a file holds only unchanged
/// paths too.
fn unchanged_revision(walk: &Walk, found: RevisionEntries<'_>) -> bool {
    walk.sizes
        .get(&(found.repo_id, found.ordinal))
        .copied()
        .unwrap_or(0)
        == found.entries.len()
        && (0..found.entries.len()).all(|position| unchanged_entry(walk, found, position))
}

fn unchanged_entry(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> bool {
    match (
        known_of(walk, found, position),
        found.entries[position].stamp,
    ) {
        (Some(known), Some(stamp)) => known.stamp == stamp,
        (Some(_), None) | (None, _) => false,
    }
}

/// What a unit costs this walk: nothing when every file it holds is the file the catalog
/// already read, otherwise the positions to prepare again. A set is prepared whole, because
/// its artifact is the union of its shards.
fn decide_unit(walk: &Walk, found: RevisionEntries<'_>, unit: &Unit) -> UnitDecision {
    match unit {
        Unit::Set(set) => {
            let members = u32::try_from(set.members.len()).unwrap_or(u32::MAX);
            let grouped = set
                .members
                .first()
                .and_then(|position| known_of(walk, found, *position))
                .is_some_and(|known| known.shards() == Some(members));
            let intact = grouped
                && set
                    .members
                    .iter()
                    .all(|position| unchanged_entry(walk, found, *position));
            match intact {
                true => UnitDecision::Skip,
                false => UnitDecision::Prepare {
                    prepare: set.members.clone(),
                    unchanged: Vec::new(),
                },
            }
        }
        Unit::Chunk(positions) => {
            let (unchanged, prepare): (Vec<usize>, Vec<usize>) = positions
                .iter()
                .copied()
                .partition(|position| unchanged_entry(walk, found, *position));
            match prepare.is_empty() {
                true => UnitDecision::Skip,
                false => UnitDecision::Prepare { prepare, unchanged },
            }
        }
    }
}

/// Whether a file of a prepared unit was rewritten while the unit was being read.
///
/// Preparation sniffs and hashes outside the write transaction, so between the walk's
/// `stat` and the row that replaces it a file can change; writing the row anyway would
/// attach the size and hashes of bytes that are gone. A file that has since disappeared is
/// not a change here: preparation already saw it absent and the row it writes says so.
///
/// This narrows a window rather than closing one. It never was closed: redb locks the
/// database, not the filesystem, so a file could always change between the `stat` that
/// decided to read it and the commit that recorded what was read. The re-`stat` runs on the
/// same pool the sniff either side of it runs on, so a unit's files are read once each in
/// parallel rather than a thousand times in series.
fn changed_while_prepared(prepared: &[PreparedEntry<'_>]) -> bool {
    prepared
        .par_iter()
        .any(|entry| stamp_moved(entry.discovered.abs_path.as_deref(), entry.discovered.stamp))
}

/// Whether the file at `path` reads differently now from the stamp the walk took. A path
/// with no stamp and a path that no longer stats both answer false: neither is a file that
/// changed under a read of it.
pub(super) fn stamp_moved(path: Option<&Path>, stamp: Option<FileStamp>) -> bool {
    let (Some(path), Some(stamp)) = (path, stamp) else {
        return false;
    };
    matches!(crate::hash::stamp(path), Ok(current) if current != stamp)
}

/// Marks the paths this walk left alone as seen, and records the ones whose revision has
/// moved to another ordinal since the row was written.
fn mark_unchanged(
    walk: &mut Walk,
    found: RevisionEntries<'_>,
    positions: impl IntoIterator<Item = usize>,
) {
    for position in positions {
        if let Some(known) = known_of(walk, found, position) {
            walk.seen.insert(known.entry_id);
            walk.report.unchanged += 1;
            if known.revision != found.ordinal {
                walk.moved.push((known.entry_id, found.ordinal));
            }
        }
    }
}

/// Retires the rows the walk is about to write again, so a changed file keeps neither its
/// old entry nor a claim on an artifact its new bytes may no longer establish. Returns the
/// artifacts a path was taken from, which the same transaction recounts once the rows that
/// replace them are written.
fn replace_known(
    transaction: &RwTransaction<'_>,
    walk: &mut Walk,
    counters: &mut Counters,
    found: RevisionEntries<'_>,
    positions: &[usize],
) -> Result<Vec<i64>> {
    let mut taken = Vec::new();
    for position in positions {
        let Some(known) = known_of(walk, found, *position) else {
            walk.report.added += 1;
            continue;
        };
        walk.seen.insert(known.entry_id);
        walk.report.changed += 1;
        if let Some(id) = known.artifact() {
            counters.touch(transaction, id)?;
            taken.push(id);
        }
        if let Some(entry) = transaction.get().primary::<Entry>(known.entry_id)? {
            counters.derived.removed(&entry);
            transaction.remove(entry)?;
        }
    }
    Ok(taken)
}

/// Recounts what each artifact a transaction took a path from still holds, in that same
/// transaction. The sweep at the end of the pass would do it, but a pass killed before it
/// reaches that sweep would leave a record counting a path no entry backs, and the next run
/// has nothing to tell it to look: the rows the killed pass wrote are the rows the walk now
/// skips. Sweeping here makes the sweep at the end of the pass a repeat that finds nothing.
fn sweep_taken(
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

pub(super) struct Counters {
    repos: u64,
    pub(super) artifacts: u64,
    pub(super) entries: u64,
    touched: HashSet<i64>,
    pub(super) pending: Vec<i64>,
    pub(super) derived: DerivedShift,
    triage: f32,
    before: HashMap<i64, SummaryPart>,
    /// Whether a published search index exists for the change log to be a log of.
    ///
    /// Read once, when the pass starts. A rebuild reads every artifact record rather than
    /// the log, and cannot run beside a pass anyway: redb takes one writer at a time and
    /// `open_or_scan` runs the scan to completion before it builds.
    logging: bool,
}

/// What one artifact record contributes to the summary. A record that does not exist
/// contributes nothing, which is what makes a created or removed record a plain difference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SummaryPart {
    artifacts: u64,
    present: u64,
    triage: u64,
    indexed_bytes: u64,
    advertised: u64,
    reclaimable_bytes: u64,
    duplicates: u64,
}

impl SummaryPart {
    fn of(record: Option<&ArtifactRecord>, triage: f32) -> Self {
        let Some(record) = record else {
            return Self::default();
        };
        let bytes = record.group_bytes.unwrap_or(record.artifact.size);
        let present = record.path_count > 0;
        let copies = u64::from(record.path_count.saturating_sub(1));
        Self {
            artifacts: 1,
            present: u64::from(present),
            triage: u64::from(record.artifact.needs_triage(triage)),
            indexed_bytes: match present {
                true => bytes,
                false => 0,
            },
            advertised: u64::from(present && record.artifact.sha256.is_some()),
            reclaimable_bytes: bytes.saturating_mul(copies),
            duplicates: u64::from(copies > 0),
        }
    }
}

/// Moves the summary by the difference one record made, so nothing but the records a pass
/// touched is ever read.
fn shift_summary(summary: &mut Summary, before: SummaryPart, after: SummaryPart) {
    let moved = |total: u64, less: u64, more: u64| total.saturating_sub(less).saturating_add(more);
    summary.artifacts = moved(summary.artifacts, before.artifacts, after.artifacts);
    summary.present_artifacts = moved(summary.present_artifacts, before.present, after.present);
    summary.triage_artifacts = moved(summary.triage_artifacts, before.triage, after.triage);
    summary.indexed_bytes = moved(
        summary.indexed_bytes,
        before.indexed_bytes,
        after.indexed_bytes,
    );
    summary.advertised_artifacts = moved(
        summary.advertised_artifacts,
        before.advertised,
        after.advertised,
    );
    summary.reclaimable_bytes = moved(
        summary.reclaimable_bytes,
        before.reclaimable_bytes,
        after.reclaimable_bytes,
    );
    summary.duplicate_artifacts = moved(
        summary.duplicate_artifacts,
        before.duplicates,
        after.duplicates,
    );
}

pub(super) fn blank_summary() -> Summary {
    Summary {
        id: 0,
        reclaimable_bytes: 0,
        duplicate_artifacts: 0,
        artifacts: 0,
        present_artifacts: 0,
        triage_artifacts: 0,
        indexed_bytes: 0,
        repositories: 0,
        advertised_artifacts: 0,
    }
}

/// What one entry contributes to the counts kept on the records above it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Contribution {
    files: i64,
    bytes: i64,
    owed: i64,
}

/// What a pass moved in the counts derived from its entries: each repository's present
/// files and bytes, each source's owed hashes, and each node's filed rows. Accumulating
/// them means a record above the entries is rewritten once per pass rather than once per
/// entry.
#[derive(Debug, Default)]
pub(super) struct DerivedShift {
    repositories: HashMap<i64, Contribution>,
    nodes: HashMap<i64, i64>,
}

impl DerivedShift {
    /// Counts an entry the caller is about to write. An entry holding no bytes on any node
    /// moves only that node's filed count.
    pub(super) fn added(&mut self, entry: &Entry) {
        self.shift(entry, 1);
    }

    /// Counts an entry the caller is about to remove or move out of presence.
    pub(super) fn removed(&mut self, entry: &Entry) {
        self.shift(entry, -1);
    }

    fn shift(&mut self, entry: &Entry, sign: i64) {
        let filed = self.nodes.entry(entry.node).or_default();
        *filed = filed.saturating_add(sign);
        if entry.state == State::Remote {
            return;
        }
        let bytes = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let slot = self.repositories.entry(entry.repo_id).or_default();
        slot.files = slot.files.saturating_add(sign);
        slot.bytes = slot.bytes.saturating_add(sign.saturating_mul(bytes));
        if owes_a_hash(entry) {
            slot.owed = slot.owed.saturating_add(sign);
        }
    }

    /// Writes what has accumulated onto the repositories, sources and nodes it names, and
    /// starts again empty.
    pub(super) fn flush(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        let mut owed: HashMap<String, i64> = HashMap::new();
        for (repo_id, moved) in self.repositories.drain() {
            if moved == Contribution::default() {
                continue;
            }
            let Some(stored) = transaction.get().primary::<Repository>(repo_id)? else {
                continue;
            };
            if moved.owed != 0 {
                *owed.entry(stored.source.clone()).or_default() += moved.owed;
            }
            if moved.files == 0 && moved.bytes == 0 {
                continue;
            }
            transaction.upsert(Repository {
                present_files: shifted_u32(stored.present_files, moved.files),
                present_bytes: shifted_u64(stored.present_bytes, moved.bytes),
                ..stored
            })?;
        }
        for (name, moved) in owed {
            let Some(stored) = transaction.get().primary::<Source>(name)? else {
                continue;
            };
            transaction.upsert(Source {
                pending_hashes: shifted_u64(stored.pending_hashes, moved),
                ..stored
            })?;
        }
        for (node, moved) in self.nodes.drain() {
            if moved == 0 {
                continue;
            }
            let Some(stored) = transaction.get().primary::<Node>(node)? else {
                continue;
            };
            transaction.upsert(Node {
                filed: shifted_u64(stored.filed, moved),
                ..stored
            })?;
        }
        Ok(())
    }
}

/// Whether this row still owes the catalog a sha256: bytes on this disk that nothing has
/// read yet.
pub(super) fn owes_a_hash(entry: &Entry) -> bool {
    entry.sha256.is_none() && entry.state != State::Remote && !entry.placed.is_absent()
}

fn shifted_u64(current: u64, by: i64) -> u64 {
    match by.is_negative() {
        true => current.saturating_sub(by.unsigned_abs()),
        false => current.saturating_add(by.unsigned_abs()),
    }
}

fn shifted_u32(current: u32, by: i64) -> u32 {
    u32::try_from(shifted_u64(u64::from(current), by)).unwrap_or(u32::MAX)
}

impl Counters {
    pub(super) fn load(database: &Database<'static>, triage: f32) -> Result<Self> {
        let read = database.r_transaction()?;
        let stored: Option<Allocator> = read.get().primary(0_u8)?;
        let manifest: Option<Manifest> = read.get().primary(0_u8)?;
        let allocator = stored.unwrap_or(Allocator {
            id: 0,
            repos: 0,
            artifacts: 0,
            entries: 0,
        });
        Ok(Self {
            repos: allocator
                .repos
                .max(highest_id(&read, |row: &Repository| row.id)?),
            artifacts: allocator
                .artifacts
                .max(highest_id(&read, |row: &ArtifactRecord| row.id)?),
            entries: allocator
                .entries
                .max(highest_id(&read, |row: &Entry| row.id)?),
            touched: HashSet::new(),
            pending: Vec::new(),
            derived: DerivedShift::default(),
            triage,
            before: HashMap::new(),
            logging: manifest.is_some_and(|manifest| manifest.search_directory.is_some()),
        })
    }

    pub(super) fn touch(&mut self, transaction: &RwTransaction<'_>, id: i64) -> Result<()> {
        self.touched.insert(id);
        self.capture_summary(transaction, id)
    }

    pub(super) fn take_touched(&mut self) -> HashSet<i64> {
        std::mem::take(&mut self.touched)
    }

    pub(super) fn capture_summary(
        &mut self,
        transaction: &RwTransaction<'_>,
        id: i64,
    ) -> Result<()> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            let record: Option<ArtifactRecord> = transaction.get().primary(id)?;
            slot.insert(SummaryPart::of(record.as_ref(), self.triage));
        }
        Ok(())
    }

    pub(super) fn settle_summary(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        if self.before.is_empty() {
            return Ok(());
        }
        let stored: Option<Summary> = transaction.get().primary(0_u8)?;
        let mut summary = stored.clone().unwrap_or_else(blank_summary);
        for (id, before) in self.before.drain() {
            let record: Option<ArtifactRecord> = transaction.get().primary(id)?;
            let after = SummaryPart::of(record.as_ref(), self.triage);
            shift_summary(&mut summary, before, after);
        }
        if stored.as_ref() != Some(&summary) {
            transaction.upsert(summary)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn holds_no_summary_snapshot(&self) -> bool {
        self.before.is_empty()
    }

    pub(super) fn save(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        self.derived.flush(transaction)?;
        self.settle_summary(transaction)?;
        transaction.upsert(Allocator {
            id: 0,
            repos: self.repos,
            artifacts: self.artifacts,
            entries: self.entries,
        })?;
        Ok(())
    }

    pub(super) fn next_repo(&mut self) -> Result<i64> {
        next_id(&mut self.repos)
    }
}

/// Records, in the transaction that made them, which artifacts the published search index no
/// longer matches. The sequence continues past both the highest key the log still holds and
/// the refresh cursor, because a drained log holds neither the numbers it has already spent
/// nor any proof that a refresh has not already passed them.
///
/// A pass that runs before any index is published writes nothing. There is no index for the
/// rows to be a difference against: the build that follows reads every artifact record, and
/// publishing prunes the whole log unread. A fresh scan of a million artifacts logged and
/// then deleted a million rows, and redb keeps the pages it grew the file by.
pub(super) fn log_changes(transaction: &RwTransaction<'_>, counters: &mut Counters) -> Result<()> {
    if !counters.logging {
        counters.pending.clear();
        return Ok(());
    }
    if counters.pending.is_empty() {
        return Ok(());
    }
    counters.pending.sort_unstable();
    counters.pending.dedup();
    let last: Option<SearchChange> = transaction
        .scan()
        .primary::<SearchChange>()?
        .all()?
        .next_back()
        .transpose()?;
    let manifest: Option<Manifest> = transaction.get().primary(0_u8)?;
    let spent = last
        .map_or(0, |change| change.seq)
        .max(manifest.map_or(0, |manifest| manifest.search_seq));
    for (offset, artifact_id) in counters.pending.drain(..).enumerate() {
        transaction.insert(SearchChange {
            seq: spent + offset as u64 + 1,
            artifact_id,
        })?;
    }
    Ok(())
}

/// The highest id one table holds, which a pass killed before it saved the allocator left
/// above the allocator. Handing that id out again would write over the row already holding
/// it, so a pass starts from whichever of the two is higher.
fn highest_id<T>(read: &RTransaction<'_>, id: fn(&T) -> i64) -> Result<u64>
where
    T: native_db::ToInput,
{
    let last: Option<T> = read.scan().primary()?.all()?.next_back().transpose()?;
    Ok(last.map_or(0, |row| u64::try_from(id(&row)).unwrap_or(0)))
}

pub(super) fn next_id(counter: &mut u64) -> Result<i64> {
    let id = counter
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| IndexError::Catalog("catalog identity space exhausted".into()))?;
    *counter = id as u64;
    Ok(id)
}

/// The stored shape of a discovered repository. `present` is what the copy already on
/// record counted, because rediscovering a repository does not move a single file, and
/// `revisions` is what [`merged_revisions`] settled, not what discovery listed.
fn repository(
    id: i64,
    source: &str,
    repo: &DiscoveredRepo,
    present: (u32, u64),
    revisions: Vec<Revision>,
) -> Repository {
    Repository {
        id,
        present_files: present.0,
        present_bytes: present.1,
        key: repository_key(source, &repo.repo),
        source: source.to_owned(),
        repo: repo.repo.clone(),
        root_path: repo.root_path.to_string_lossy().into_owned(),
        git_dir: repo
            .git_dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        default_ref: repo.default_ref.clone(),
        remote: repo.remote.clone(),
        revisions,
    }
}

/// What a revision is matched by across scans: its commit, or its name when it has none.
#[derive(PartialEq, Eq, Hash)]
enum RevisionKey {
    Commit(CommitSha),
    Named(String),
}

fn revision_key(sha: Option<CommitSha>, name: &str) -> RevisionKey {
    match sha {
        Some(sha) => RevisionKey::Commit(sha),
        None => RevisionKey::Named(name.to_owned()),
    }
}

/// The shape discovery just gave one revision of a repository, before it is matched against
/// what the catalog already holds.
fn discovered_revision(repo: &DiscoveredRepo, position: usize) -> Revision {
    let revision = &repo.revisions[position];
    Revision {
        name: revision_name(repo, position),
        sha: match revision.name {
            RevisionName::Commit(sha) => Some(sha),
            RevisionName::Synthetic => None,
        },
        title: revision.title.clone(),
        author: revision.author.clone(),
        committed: revision.committed.map(|stamp| stamp.to_string()),
        refs: repo
            .refs
            .iter()
            .filter(|reference| reference.revision == position)
            .map(|reference| Ref {
                name: reference.name.clone(),
                kind: reference.kind,
            })
            .collect(),
        retired: false,
    }
}

/// The stored revisions of a repository merged with what discovery found now, and the
/// ordinal each discovered revision takes in the merged list.
///
/// An entry names its revision by ordinal, so an ordinal is an identity and cannot follow
/// discovery order: a snapshot whose commit sorts ahead of the stored ones would otherwise
/// move every entry of the repository onto the wrong revision. A rediscovered revision
/// keeps its slot and takes the fresh refs and metadata; a new one is appended; one the
/// walk no longer found keeps its slot so the entries filed under it still resolve, and
/// gives up its refs, because a ref is a pointer discovery just re-enumerated and a
/// revision no ref reaches must not answer to a name that now belongs to another.
fn merged_revisions(stored: &[Revision], repo: &DiscoveredRepo) -> (Vec<Revision>, Vec<usize>) {
    let mut slots: HashMap<RevisionKey, usize> = HashMap::with_capacity(stored.len());
    for (slot, revision) in stored.iter().enumerate() {
        slots
            .entry(revision_key(revision.sha, &revision.name))
            .or_insert(slot);
    }
    let mut merged: Vec<Revision> = stored
        .iter()
        .map(|revision| Revision {
            refs: Vec::new(),
            ..revision.clone()
        })
        .collect();
    let mut ordinals = Vec::with_capacity(repo.revisions.len());
    for position in 0..repo.revisions.len() {
        let found = discovered_revision(repo, position);
        let slot = match slots.remove(&revision_key(found.sha, &found.name)) {
            Some(slot) => {
                merged[slot] = found;
                slot
            }
            None => {
                merged.push(found);
                merged.len() - 1
            }
        };
        ordinals.push(slot);
    }
    (merged, ordinals)
}

fn revision_name(repo: &DiscoveredRepo, ordinal: usize) -> String {
    repo.refs
        .iter()
        .filter(|reference| reference.revision == ordinal)
        .min_by_key(|reference| {
            (
                reference.kind != crate::discover::RefKind::Branch,
                &reference.name,
            )
        })
        .map(|reference| reference.name.clone())
        .unwrap_or_else(|| match repo.revisions[ordinal].name {
            RevisionName::Commit(sha) => sha.to_string().chars().take(7).collect(),
            RevisionName::Synthetic => repo.default_ref.clone(),
        })
}

/// What this disk holds for one discovered path once sniffing has run. A file can be on
/// disk yet unreadable, and an LFS-tracked file can be a fetched weight or a pointer stub.
pub(super) struct Presence {
    pub(super) abs_path: Option<PathBuf>,
    pub(super) readable: bool,
    pub(super) size: u64,
    pub(super) sha256: Option<Sha256>,
    pub(super) git_sha1: Option<GitSha1>,
    pub(super) lfs_pointer: bool,
    pub(super) pointer_bytes: bool,
}

impl Presence {
    pub(super) fn of(entry: &DiscoveredEntry, sniffed: Option<&SniffResult>) -> Self {
        let pointer = sniffed
            .and_then(|result| result.lfs_pointer.as_ref())
            .filter(|_| entry.sha256.is_none());
        match pointer {
            Some(pointer) => Self {
                abs_path: None,
                readable: false,
                size: pointer.size,
                sha256: Some(pointer.sha256),
                git_sha1: entry.git_sha1,
                lfs_pointer: true,
                pointer_bytes: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone(),
                readable: sniffed.is_some(),
                size: entry.size,
                sha256: entry.sha256,
                git_sha1: entry.git_sha1,
                lfs_pointer: entry.lfs_pointer,
                pointer_bytes: false,
            },
        }
    }

    fn present(&self) -> bool {
        self.readable
    }
}

pub(super) fn creates_artifact(
    format: ArtifactFormat,
    presence: &Presence,
    role: &EntryRole,
) -> bool {
    format.is_artifact() && !presence.pointer_bytes && !matches!(role, EntryRole::Member)
}

/// What one file that becomes an artifact contributes: only such a file pays for inference
/// and a content identity.
struct ArtifactDraft {
    identity: u128,
    inference: Inference,
}

struct PreparedEntry<'a> {
    discovered: &'a DiscoveredEntry,
    sniffed: Option<SniffResult>,
    presence: Presence,
    artifact: Option<ArtifactDraft>,
    role: EntryRole,
    org: Option<String>,
    name: String,
}

/// One model split across files, as the revision's grouping pass settled it.
pub(super) struct GroupFacts {
    label: String,
    bytes: u64,
    shard_count: u32,
    tensors: Option<TensorSummary>,
    incomplete: bool,
}

/// Which part one file plays: a file of its own, the shard the artifact rides on, or one
/// more piece of a model another shard already stands for.
pub(super) enum EntryRole {
    Standalone,
    Anchor(GroupFacts),
    Member,
}

/// The largest number of standalone files one transaction covers.
pub(super) const CHUNK: usize = 1024;

/// Everything the per entry pass of one revision reads besides the entry itself.
struct RevisionPlan<'a> {
    naming: Naming<'a>,
    context: InferenceContext,
}

/// The documents a revision holds and the inference context they make. They are few and
/// small, so every later unit is described without the revision's weights being held.
struct RevisionDocuments {
    context: InferenceContext,
    index_documents: HashMap<usize, serde_json::Value>,
}

/// Reads only the documents that describe a revision: cards, configs and index files.
fn revision_documents(entries: &[DiscoveredEntry]) -> RevisionDocuments {
    let positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_document(entry))
        .map(|(position, _)| position)
        .collect();
    let sniffed: Vec<Option<SniffResult>> = positions
        .par_iter()
        .map(|position| sniff_entry(&entries[*position]))
        .collect();
    let context = InferenceContext::from_results(positions.iter().zip(&sniffed).filter_map(
        |(position, result)| {
            result
                .as_ref()
                .map(|result| (entries[*position].rel_path.as_str(), result))
        },
    ));
    let index_documents = positions
        .iter()
        .zip(&sniffed)
        .filter(|(position, _)| shard::is_index_document(file_name(&entries[**position].rel_path)))
        .filter_map(|(position, result)| {
            index_document(&entries[*position], result.as_ref())
                .map(|document| (*position, document))
        })
        .collect();
    RevisionDocuments {
        context,
        index_documents,
    }
}

fn is_document(entry: &DiscoveredEntry) -> bool {
    let name = file_name(&entry.rel_path);
    shard::is_index_document(name)
        || matches!(
            sniff::name_format(name),
            Some(
                ArtifactFormat::Readme
                    | ArtifactFormat::Config
                    | ArtifactFormat::Diffusers
                    | ArtifactFormat::Transformers
            )
        )
}

fn names_an_artifact(entry: &DiscoveredEntry) -> bool {
    sniff::name_format(file_name(&entry.rel_path)).is_some_and(ArtifactFormat::is_artifact)
}

/// Shard sets from names and index documents alone. Whether a set's anchor really holds
/// weights is decided when the set is sniffed as a unit.
fn shard_sets_by_name(entries: &[DiscoveredEntry], documents: &RevisionDocuments) -> Vec<ShardSet> {
    let mut positions = Vec::new();
    let mut candidates = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if entry.abs_path.is_none() || !shard::is_shard_file(file_name(&entry.rel_path)) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.index_documents.get(&position) {
            candidate = candidate.with_index_document(document);
        }
        positions.push(position);
        candidates.push(candidate);
    }
    shard::group_shards(&candidates)
        .into_iter()
        .map(|set| ShardSet {
            members: set
                .members
                .iter()
                .map(|member| positions[*member])
                .collect(),
            ..set
        })
        .collect()
}

/// The largest amount of a revision that is sniffed, prepared and committed at once.
enum Unit {
    Set(ShardSet),
    Chunk(Vec<usize>),
}

impl Unit {
    fn positions(&self) -> &[usize] {
        match self {
            Unit::Set(set) => &set.members,
            Unit::Chunk(positions) => positions,
        }
    }
}

fn revision_units(entries: &[DiscoveredEntry], sets: Vec<ShardSet>) -> Vec<Unit> {
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let standalone: Vec<usize> = (0..entries.len())
        .filter(|position| !grouped.contains(position))
        .collect();
    sets.into_iter()
        .map(Unit::Set)
        .chain(
            standalone
                .chunks(CHUNK)
                .map(|chunk| Unit::Chunk(chunk.to_vec())),
        )
        .collect()
}

/// How many artifacts a revision will hold, decided from file names before any weight is
/// read, because whether the repository names its single artifact must be settled once for
/// every unit.
fn revision_plan<'a>(
    context: &EntryContext<'a>,
    documents: RevisionDocuments,
    units: &[Unit],
) -> RevisionPlan<'a> {
    let entries = &context.revision.entries;
    let artifacts: usize = units
        .iter()
        .map(|unit| match unit {
            Unit::Set(_) => 1,
            Unit::Chunk(positions) => positions
                .iter()
                .filter(|position| names_an_artifact(&entries[**position]))
                .count(),
        })
        .sum();
    RevisionPlan {
        naming: Naming {
            repo: &context.repo.repo,
            root_path: &context.repo.root_path,
            kind: context.kind,
            single: artifacts <= 1,
        },
        context: documents.context,
    }
}

/// One label per unit, in unit order: a set's disambiguated label, and nothing for a chunk
/// of files that each stand for themselves.
fn set_labels(naming: &Naming<'_>, units: &[Unit]) -> Vec<Option<String>> {
    let sets: Vec<&ShardSet> = units
        .iter()
        .filter_map(|unit| match unit {
            Unit::Set(set) => Some(set),
            Unit::Chunk(_) => None,
        })
        .collect();
    let mut labels: Vec<String> = sets.iter().map(|set| shard_label(naming, set)).collect();
    let stems: Vec<String> = sets.iter().map(|set| set.stem.clone()).collect();
    disambiguate_labels(&mut labels, &stems);
    let mut per_unit: Vec<Option<String>> = Vec::with_capacity(units.len());
    per_unit.extend(labels.into_iter().map(Some));
    per_unit.resize(units.len(), None);
    per_unit
}

/// One artifact per set, described by the union of its shard headers and weighing them all.
fn group_facts(
    set: &ShardSet,
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    label: String,
) -> GroupFacts {
    let parts: Vec<&TensorSummary> = sniffed
        .iter()
        .filter_map(|result| result.as_ref())
        .filter_map(|result| result.tensors.as_ref())
        .collect();
    GroupFacts {
        label,
        bytes: presences.iter().map(|presence| presence.size).sum(),
        shard_count: u32::try_from(presences.len()).unwrap_or(u32::MAX),
        tensors: (!parts.is_empty()).then(|| shard::merge_tensor_summaries(&parts)),
        incomplete: set.is_incomplete(),
    }
}

/// A set whose anchor turned out to hold no weights is not a model: its files go back to
/// standing for themselves.
fn unit_roles(
    unit: &Unit,
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    label: Option<&str>,
) -> Vec<EntryRole> {
    let alone = || (0..sniffed.len()).map(|_| EntryRole::Standalone).collect();
    let set = match unit {
        Unit::Set(set) => set,
        Unit::Chunk(_) => return alone(),
    };
    let anchored = sniffed
        .first()
        .and_then(Option::as_ref)
        .is_some_and(|result| result.format.is_artifact())
        && presences
            .first()
            .is_some_and(|presence| !presence.pointer_bytes);
    if !anchored {
        return alone();
    }
    let facts = group_facts(
        set,
        sniffed,
        presences,
        label.unwrap_or_default().to_owned(),
    );
    std::iter::once(EntryRole::Anchor(facts))
        .chain(std::iter::repeat_with(|| EntryRole::Member).take(sniffed.len().saturating_sub(1)))
        .collect()
}

/// Sniffs one unit in parallel and prepares its files against the revision's documents, so
/// a README or config beside the weights describes them. Nothing larger than a unit is read
/// at once.
fn prepare_unit<'a>(
    context: &EntryContext<'a>,
    plan: &RevisionPlan<'_>,
    unit: &Unit,
    label: Option<&str>,
    stop: &Stop,
) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let positions = unit.positions();
    let sniffed: Vec<Option<SniffResult>> = positions
        .par_iter()
        .map(|position| sniff_entry(&entries[*position]))
        .collect();
    let mut presences: Vec<Presence> = positions
        .iter()
        .zip(&sniffed)
        .map(|(position, result)| Presence::of(&entries[*position], result.as_ref()))
        .collect();
    match context.settings.policy {
        HashPolicy::Now => hash_presences(&mut presences, context.settings.threshold, stop),
        HashPolicy::Background | HashPolicy::Never => {}
    }
    let roles = unit_roles(unit, &sniffed, &presences, label);
    positions
        .par_iter()
        .zip(sniffed.into_par_iter())
        .zip(presences.into_par_iter())
        .zip(roles.into_par_iter())
        .map(|(((position, sniffed), presence), role)| {
            prepare_entry(&entries[*position], sniffed, presence, role, plan)
        })
        .collect()
}

/// Reads the content hashes of every file of one unit that does not already carry them, so
/// the identity the unit's artifacts take is a content identity from this scan onwards.
fn hash_presences(presences: &mut [Presence], threshold: u64, stop: &Stop) {
    let digests: Vec<Option<(Sha256, Option<GitSha1>)>> = presences
        .par_iter()
        .map(|presence| {
            let path = presence
                .abs_path
                .as_deref()
                .filter(|_| presence.sha256.is_none())?;
            match content_hashes(path, presence.size, threshold, stop) {
                Ok(hashes) => hashes,
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "file is unhashable");
                    None
                }
            }
        })
        .collect();
    for (presence, digest) in presences.iter_mut().zip(digests) {
        let Some((sha256, git_sha1)) = digest else {
            continue;
        };
        presence.sha256 = Some(sha256);
        presence.git_sha1 = presence.git_sha1.or(git_sha1);
    }
}

fn sniff_entry(entry: &DiscoveredEntry) -> Option<SniffResult> {
    let path = entry.abs_path.as_deref()?;
    match sniff::sniff_named(path, file_name(&entry.rel_path)) {
        Ok(result) => Some(result),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "candidate is unreadable");
            None
        }
    }
}

fn prepare_entry<'a>(
    entry: &'a DiscoveredEntry,
    sniffed: Option<SniffResult>,
    presence: Presence,
    role: EntryRole,
    plan: &RevisionPlan<'_>,
) -> Result<PreparedEntry<'a>> {
    let format = sniffed
        .as_ref()
        .map_or(ArtifactFormat::Unknown, |result| result.format);
    let creates_artifact = creates_artifact(format, &presence, &role);
    let path = inference_path(
        plan.naming.root_path,
        &entry.rel_path,
        entry.abs_path.as_deref(),
    );
    let file = file_name(&entry.rel_path);
    let (size, tensors, label) = match &role {
        EntryRole::Anchor(facts) => (
            facts.bytes,
            facts.tensors.as_ref(),
            Some(facts.label.as_str()),
        ),
        EntryRole::Standalone | EntryRole::Member => (
            presence.size,
            sniffed.as_ref().and_then(|result| result.tensors.as_ref()),
            None,
        ),
    };
    let artifact = creates_artifact
        .then(|| -> Result<ArtifactDraft> {
            let mut input = InferInput::new(format, &path, file, size);
            input.tensors = tensors;
            if let Some(sniffed) = &sniffed {
                input.config = sniffed
                    .config
                    .as_ref()
                    .or_else(|| plan.context.config_for(&entry.rel_path));
            }
            input.frontmatter = plan.context.frontmatter();
            Ok(ArtifactDraft {
                identity: identity(entry, &presence)?,
                inference: infer::infer(&input),
            })
        })
        .transpose()?;
    let (org, name) = artifact_name(&plan.naming, &entry.rel_path, label);
    Ok(PreparedEntry {
        discovered: entry,
        artifact,
        name,
        org,
        presence,
        sniffed,
        role,
    })
}

/// What the catalog knows one file's content by: its sha256, else its git blob sha, else
/// the canonical path it sits at, hashed from the path's own bytes. A path outside UTF-8
/// therefore hashes differently from the lossy text that renders it, once the walker admits
/// one.
pub(super) fn identity(entry: &DiscoveredEntry, presence: &Presence) -> Result<u128> {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    match (presence.sha256, entry.git_sha1, entry.abs_path.as_deref()) {
        (Some(sha), _, _) => hasher.update(format!("sha256:{sha}").as_bytes()),
        (None, Some(sha), _) => hasher.update(format!("git:{sha}").as_bytes()),
        (None, None, Some(path)) => {
            hasher.update(b"path:");
            hasher.update(
                path.canonicalize()
                    .unwrap_or_else(|_| path.to_path_buf())
                    .as_os_str()
                    .as_encoded_bytes(),
            );
        }
        (None, None, None) => {
            return Err(IndexError::Catalog(format!(
                "absent entry has no content identity: {}",
                entry.rel_path
            )));
        }
    }
    Ok(hasher.digest128())
}

struct EntryContext<'a> {
    repo: &'a DiscoveredRepo,
    repo_id: i64,
    revision: &'a DiscoveredRevision,
    settings: ScanSettings,
    kind: SourceKind,
    ordinal: u32,
}

/// What one walk was asked for beyond the roots themselves: where inference stops being
/// trusted, and whether the files it reads are hashed as it goes.
#[derive(Debug, Clone, Copy)]
struct ScanSettings {
    triage: f32,
    policy: HashPolicy,
    threshold: u64,
}

impl ScanSettings {
    fn new(config: &IndexConfig, policy: HashPolicy) -> Self {
        Self {
            triage: config.triage_confidence,
            policy,
            threshold: config.lfs_threshold,
        }
    }
}

/// Writes one unit. A shard member carries no artifact of its own, so it is stored after
/// the anchor that establishes the artifact and the triage verdict it shares.
fn store_unit(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: Vec<PreparedEntry<'_>>,
    counters: &mut Counters,
) -> Result<()> {
    let mut anchor = None;
    let mut members = Vec::new();
    for prepared in prepared {
        match &prepared.role {
            EntryRole::Member => members.push(prepared),
            EntryRole::Anchor(_) => {
                anchor = store_entry(transaction, context, prepared, None, counters)?;
            }
            EntryRole::Standalone => {
                store_entry(transaction, context, prepared, None, counters)?;
            }
        }
    }
    for prepared in members {
        store_entry(transaction, context, prepared, anchor, counters)?;
    }
    Ok(())
}

/// Writes one file. A shard member carries no artifact of its own: it is handed the
/// artifact and the triage verdict its anchor already established.
fn store_entry(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: PreparedEntry<'_>,
    anchor: Option<(i64, bool)>,
    counters: &mut Counters,
) -> Result<Option<(i64, bool)>> {
    let entry_id = next_id(&mut counters.entries)?;
    let record = match prepared.artifact.as_ref() {
        Some(draft) => Some(store_artifact(transaction, &prepared, draft, counters)?),
        None => None,
    };
    let stored = match &record {
        Some(record) => Some((
            record.id,
            artifact_triage(&prepared, record, context.settings.triage),
        )),
        None => anchor,
    };
    let triage = stored.is_some_and(|(_, triage)| triage);
    let state = entry_state(&prepared, triage);
    let artifact_id = stored.map(|(id, _)| id);
    let entry = Entry {
        id: entry_id,
        repo_id: context.repo_id,
        artifact_id,
        revision: context.ordinal,
        rel_path: prepared.discovered.rel_path.clone(),
        placed: Placement::of(
            prepared
                .presence
                .abs_path
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref(),
            &context.repo.root_path.to_string_lossy(),
            &prepared.discovered.rel_path,
        ),
        size: prepared.presence.size,
        state,
        triage,
        preference: preference(state, prepared.presence.present()),
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        sha256: prepared.presence.sha256,
        content: prepared.presence.sha256.as_ref().map(content_key),
        git_sha1: prepared.presence.git_sha1,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
        node: LOCAL_NODE,
        pass: 0,
        pull_progress: None,
    };
    counters.derived.added(&entry);
    transaction.insert(entry)?;
    Ok(stored)
}

/// A set that found fewer files than it declares cannot be served whatever its header says.
fn artifact_triage(prepared: &PreparedEntry<'_>, record: &ArtifactRecord, confidence: f32) -> bool {
    let incomplete = match &prepared.role {
        EntryRole::Anchor(facts) => facts.incomplete,
        EntryRole::Standalone | EntryRole::Member => false,
    };
    record.artifact.needs_triage(confidence) || incomplete
}

/// Which of two namings of one identity the record keeps.
///
/// The unit holding more of the content names it, because a shard set describes a model its
/// anchor's file name does not; equal content is settled by the smaller name, so that two
/// revisions naming one file differently agree whichever of them the walk reaches first.
fn names_better(candidate: &ArtifactRecord, stored: &ArtifactRecord) -> bool {
    naming_key(candidate) < naming_key(stored)
}

fn naming_key(record: &ArtifactRecord) -> (Reverse<u64>, &str, &str) {
    (
        Reverse(record.group_bytes.unwrap_or(record.artifact.size)),
        record.name.as_str(),
        record.org.as_deref().unwrap_or_default(),
    )
}

/// The record this entry contributes a path to.
///
/// A first path writes the record; an identity the catalog already holds keeps its id, when
/// it was first seen and its cleared verdict, takes what this file's header just proved, and
/// keeps the better of the two names by [`names_better`] rather than the first one written.
/// The identity is a unique secondary key, and a read inside the writing transaction sees
/// the rows that transaction has already written, so two copies of one file in one unit
/// reach the same record without a cache of what this pass has written.
fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    let stored: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, draft.identity)?;
    let id = match &stored {
        Some(stored) => stored.id,
        None => next_id(&mut counters.artifacts)?,
    };
    counters.touch(transaction, id)?;
    let (mut record, detail) = artifact_record(
        id,
        prepared,
        draft,
        stored.as_ref().map(|stored| stored.first_seen.clone()),
    );
    if let Some(stored) = &stored {
        record.triage_cleared = stored.triage_cleared;
        if !names_better(&record, stored) {
            record.name = stored.name.clone();
            record.org = stored.org.clone();
        }
    }
    let verdict: Option<Override> = transaction.get().primary(id)?;
    if let Some(verdict) = &verdict {
        apply_override(&mut record, verdict);
    }
    let text = SearchText {
        artifact_id: id,
        keys: tensor_keys(detail.tensors.as_ref()),
    };
    match stored {
        Some(_) => {
            transaction.upsert(text)?;
            transaction.upsert(detail)?;
            transaction.upsert(record.clone())?;
        }
        None => {
            transaction.insert(text)?;
            transaction.insert(detail)?;
            transaction.insert(record.clone())?;
        }
    }
    counters.pending.push(id);
    Ok(record)
}

pub(super) fn tensor_keys(tensors: Option<&TensorSummary>) -> String {
    tensors.map_or_else(String::new, |summary| {
        summary
            .key_prefixes
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn entry_state(prepared: &PreparedEntry<'_>, triage: bool) -> State {
    match (prepared.presence.present(), triage) {
        (false, _) => State::Remote,
        (true, true) => State::Triage,
        (true, false) => State::Local,
    }
}

/// How an entry ranks when several hold the same artifact: served bytes outrank local ones,
/// and a path this disk holds outranks one it does not.
pub(super) fn preference(state: State, present: bool) -> i64 {
    let weight = match state {
        State::Served => 3,
        State::Local | State::Triage => 2,
        State::Stale => 1,
        State::Pulling | State::Remote => 0,
    };
    (4 + weight + i64::from(present)) * 2 + 1
}

/// How one path of an artifact ranks against another when a reader keeps one of them: the
/// higher availability preference, and on a tie the lower id. The read API, the search
/// index's `primary` mark and the query path's fold all keep the same path because they all
/// order by this.
pub(super) fn collapse_key(preference: i64, id: i64) -> (i64, Reverse<i64>) {
    (preference, Reverse(id))
}

/// The one entry a collapse keeps out of the entries of a single artifact.
pub(super) fn preferred_entry(entries: impl IntoIterator<Item = Entry>) -> Option<Entry> {
    entries
        .into_iter()
        .max_by_key(|entry| collapse_key(entry.preference, entry.id))
}

/// One artifact as the catalog is about to record it, whether a scan read its header or the
/// mirror only knows its name, size and hash.
pub(super) struct RecordFacts<'a> {
    pub(super) id: i64,
    pub(super) identity: u128,
    pub(super) name: String,
    pub(super) org: Option<String>,
    pub(super) sha256: Option<Sha256>,
    pub(super) size: u64,
    pub(super) format: ArtifactFormat,
    pub(super) inference: &'a Inference,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) group_bytes: Option<u64>,
    pub(super) shard_count: Option<u32>,
    /// When the catalog first recorded this artifact. A record being rewritten passes what
    /// it already carries, so nothing reads the clock for a row that has been seen before.
    pub(super) first_seen: Option<String>,
}

fn artifact_record(
    id: i64,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    first_seen: Option<String>,
) -> (ArtifactRecord, ArtifactDetail) {
    let (tensors, group_bytes, shard_count) = match &prepared.role {
        EntryRole::Anchor(facts) => (
            facts.tensors.clone(),
            Some(facts.bytes),
            Some(facts.shard_count),
        ),
        EntryRole::Standalone | EntryRole::Member => (
            prepared
                .sniffed
                .as_ref()
                .and_then(|sniffed| sniffed.tensors.clone()),
            None,
            None,
        ),
    };
    record_of(RecordFacts {
        id,
        identity: draft.identity,
        name: prepared.name.clone(),
        org: prepared.org.clone(),
        sha256: prepared.presence.sha256,
        size: prepared.presence.size,
        format: prepared
            .sniffed
            .as_ref()
            .map_or(ArtifactFormat::Unknown, |sniffed| sniffed.format),
        inference: &draft.inference,
        tensors,
        group_bytes,
        shard_count,
        first_seen,
    })
}

pub(super) fn record_of(facts: RecordFacts<'_>) -> (ArtifactRecord, ArtifactDetail) {
    let RecordFacts {
        id,
        identity,
        name,
        org,
        sha256,
        size,
        format,
        inference,
        tensors,
        group_bytes,
        shard_count,
        first_seen,
    } = facts;
    let record = ArtifactRecord {
        id,
        identity,
        name,
        org,
        path_count: 1,
        artifact: Artifact {
            id: ArtifactId::new(id),
            blob: BlobId::new(id),
            sha256,
            size,
            format,
            role: inference.role,
            sub_role: inference.sub_role,
            role_confidence: inference.role_confidence,
            base: inference.base.clone(),
            base_label: inference.base_label.clone(),
            base_confidence: inference.base_confidence,
            precision: inference.precision.clone(),
            quant: inference.quant.clone(),
            rank: inference.rank,
            alpha: inference.alpha,
            params: inference.params,
            context: inference.context,
            trigger: inference.trigger.clone(),
        },
        group_bytes,
        shard_count,
        triage_cleared: false,
        first_seen: first_seen.unwrap_or_else(|| jiff::Timestamp::now().to_string()),
    };
    let detail = ArtifactDetail::new(id, tensors, inference.evidence.clone());
    (record, detail)
}

pub(super) fn settle_repositories(transaction: &RwTransaction<'_>) -> Result<()> {
    let stored: Option<Summary> = transaction.get().primary(0_u8)?;
    let mut summary = stored.clone().unwrap_or_else(blank_summary);
    summary.repositories = transaction.len().primary::<Repository>()?;
    if stored.as_ref() != Some(&summary) {
        transaction.upsert(summary)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_id_and_an_absent_shard_count_are_spelled_zero() {
        let known = Known {
            entry_id: 7,
            artifact_id: 0,
            stamp: FileStamp {
                inode: 1,
                mtime: 2,
                size: 3,
            },
            revision: 0,
            shard_count: 0,
        };
        assert_eq!(known.artifact(), None);
        assert_eq!(known.shards(), None);
        let filed = Known {
            artifact_id: 9,
            shard_count: 4,
            ..known
        };
        assert_eq!(filed.artifact(), Some(9));
        assert_eq!(filed.shards(), Some(4));
    }
}
