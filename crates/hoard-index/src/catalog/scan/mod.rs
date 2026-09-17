pub(super) mod counters;
mod plan;
mod revisions;
mod sources;
mod store;
mod sweep;
mod walk;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use serde::Serialize;

use hoard_core::State;

use super::hashing::Stop;
use super::models::{
    ArtifactRecord, Entry, EntryKey, LocationKey, Manifest, Repository, Summary,
    move_revision_card, store_revision_card,
};
use super::{Catalog, builder, models};
use crate::config::{HashPolicy, IndexConfig, SourceConfig};
use crate::discover;
use crate::error::{IndexError, Result};
use crate::hash::FileStamp;

pub(super) use self::counters::{Counters, blank_summary, log_changes, next_id, owes_a_hash};
#[cfg(test)]
pub(super) use self::plan::{EntryRole, Presence, creates_artifact, identity};
pub(super) use self::sources::counts;
pub(super) use self::store::{
    RecordFacts, collapse_key, preference, preferred_entry, record_of, settle_repositories,
    tensor_keys,
};
pub(super) use self::sweep::{log_revision_artifacts, recount_artifact, sweep_artifact};
pub(super) use self::walk::location_hash;
#[cfg(test)]
pub(super) use self::walk::stamp_moved;

use self::counters::{SummaryPart, shift_summary};
use self::plan::{
    Unit, prepare_unit, revision_documents, revision_plan, revision_units, set_labels,
    shard_sets_by_name,
};
use self::sources::available_sources;
use self::store::{EntryContext, ScanSettings, store_unit};
use self::sweep::{shard_count_of, sweep_taken};
use self::walk::{
    Known, KnownPaths, KnownRows, PathIndex, RevisionEntries, RevisionSizes, UnitDecision, Walk,
    changed_while_prepared, decide_unit, mark_unchanged, path_hash, replace_known,
    unchanged_revision,
};

/// The largest number of standalone files one transaction covers.
pub(super) const CHUNK: usize = 1024;

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
        let mut walk = Walk::new(ScanSettings::new(config, policy));
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
        let created = std::fs::write(
            directory.join(super::FORMAT_FILE),
            format!("{}\n", super::FORMAT_VERSION),
        )
        .map_err(IndexError::from)
        .and_then(|()| Ok(builder().create(models()?, directory.join("catalog.redb"))?));
        match created {
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
                Err(error)
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
        let mut walk = Walk::new(settings);
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
            let discovery = discover::plan(source, &noise)?;
            let kind = discovery.kind();
            self.store_source(source, kind)?;
            for window in discovery.windows() {
                let repos = discovery.read(window)?;
                let walked = self.store_repositories(source, &repos, counters)?;
                walk.discovered.extend(
                    walked
                        .iter()
                        .flat_map(|repo| repo.ordinals.iter().map(|ordinal| (repo.id, *ordinal))),
                );
                for (repo, walked) in repos.iter().zip(walked) {
                    if walk.reached.insert(walked.id) {
                        walk.set_aside_unseen();
                        let KnownRows { known, sizes } = self.repository_stamps(walked.id)?;
                        walk.known = known;
                        walk.sizes = sizes;
                    }
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
                            fingerprints: &walked.fingerprints,
                            revision,
                            settings: walk.settings,
                            kind,
                            ordinal,
                        };
                        let documents = revision_documents(&revision.entries);
                        let sets = shard_sets_by_name(&revision.entries, &documents);
                        let units = revision_units(&revision.entries, sets);
                        let plan = revision_plan(&context, documents, &units);
                        let labels = set_labels(&plan.naming, &units, &revision.entries);
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
                            let taken =
                                replace_known(&transaction, walk, counters, found, &prepare)?;
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
        }
        self.settle_moved(walk, counters)?;
        if walk.report.interrupted {
            return Ok(());
        }
        walk.set_aside_unseen();
        self.retract_absent(walk, counters)?;
        self.retract_unreached(sources, walk, counters)?;
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

    /// Every path the catalog already holds for one repository, with the stamp that proves it
    /// unchanged and the facts a decision needs before any file is opened, and how many of
    /// them each stored revision holds.
    ///
    /// Keyed by [`location_hash`] rather than by the location itself: a million relative
    /// paths held as text is the largest thing a rescan keeps in memory, and nothing here
    /// reads a location back.
    fn repository_stamps(&self, repo_id: i64) -> Result<KnownRows> {
        let read = self.database.r_transaction()?;
        let present = read
            .get()
            .primary::<Repository>(repo_id)?
            .map_or(0, |repository| repository.present_files);
        let mut shards: HashMap<i64, u32> = HashMap::new();
        let mut known: KnownPaths =
            HashMap::with_capacity(usize::try_from(present).unwrap_or_default());
        let mut sizes: RevisionSizes = HashMap::new();
        for entry in read
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(LocationKey::repository(repo_id))?
        {
            let entry: Entry = entry?;
            let Some((mtime, inode)) = stamped(&entry) else {
                continue;
            };
            let shard_count = match entry.artifact_id {
                Some(id) => shard_count_of(&read, &mut shards, id)?,
                None => None,
            };
            let location = location_hash(repo_id, entry.revision, &entry.rel_path);
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
            *sizes.entry((repo_id, entry.revision)).or_default() += 1;
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
        let missing = std::mem::take(&mut walk.missing);
        for batch in missing.chunks(CHUNK) {
            self.retract_rows(
                batch.iter().map(|known| (known.entry_id, known.artifact())),
                counters,
                &mut walk.report,
            )?;
        }
        Ok(())
    }

    fn retract_unreached(
        &self,
        sources: &[&SourceConfig],
        walk: &mut Walk,
        counters: &mut Counters,
    ) -> Result<()> {
        let mut batch: Vec<(i64, Option<i64>)> = Vec::with_capacity(CHUNK);
        for source in sources {
            for repo_id in self.repositories_of_source(&source.name)? {
                if walk.reached.contains(&repo_id) {
                    continue;
                }
                let read = self.database.r_transaction()?;
                for entry in read
                    .scan()
                    .secondary::<Entry>(EntryKey::location)?
                    .start_with(LocationKey::repository(repo_id))?
                {
                    let entry: Entry = entry?;
                    if stamped(&entry).is_none() {
                        continue;
                    }
                    batch.push((entry.id, entry.artifact_id));
                    if batch.len() == CHUNK {
                        self.retract_rows(batch.drain(..), counters, &mut walk.report)?;
                    }
                }
            }
        }
        match batch.is_empty() {
            true => Ok(()),
            false => self.retract_rows(batch.drain(..), counters, &mut walk.report),
        }
    }

    fn retract_rows(
        &self,
        rows: impl Iterator<Item = (i64, Option<i64>)>,
        counters: &mut Counters,
        report: &mut RescanReport,
    ) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let mut taken = Vec::new();
        for (entry_id, artifact) in rows {
            let Some(entry) = transaction.get().primary::<Entry>(entry_id)? else {
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
            if let Some(id) = artifact {
                counters.touch(&transaction, id)?;
                counters.pending.push(id);
                taken.push(id);
            }
            report.absent += 1;
        }
        sweep_taken(&transaction, taken, counters)?;
        log_changes(&transaction, counters)?;
        counters.derived.flush(&transaction)?;
        counters.settle_summary(&transaction)?;
        transaction.commit()?;
        Ok(())
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

fn stamped(entry: &Entry) -> Option<(i64, u64)> {
    entry
        .mtime
        .zip(entry.inode)
        .filter(|_| !entry.placed.is_absent())
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

/// Whether the configuration a rescan was handed is the whole of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanScope {
    /// Every source the configuration names, so one it does not name has been dropped.
    Configured,
    /// A subset the caller chose, so a source outside it was simply not walked.
    Narrowed,
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
