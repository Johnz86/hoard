use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use native_db::Database;
use native_db::transaction::RwTransaction;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
use serde::Serialize;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, BlobId, GitSha1, Sha256, State, TensorSummary,
};

use crate::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
use crate::discover::{self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, RevisionName};
use crate::hash::FileStamp;
use crate::infer::{self, InferInput, Inference};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};
use crate::sniff::{self, SniffResult};

use super::hashing::content_key;
use super::models::{
    Allocator, ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE,
    Manifest, Node, Override, Placement, Ref, Repository, RepositoryKey, Revision, SearchChange,
    SearchText, Source, Summary, location_key, repository_key,
};
use super::naming::{
    InferenceContext, Naming, artifact_name, content_hashes, disambiguate_labels, index_document,
    inference_path, shard_label,
};
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
    pub fn rescan(
        &self,
        config: &IndexConfig,
        policy: HashPolicy,
        progress: impl FnMut(&str, u64),
    ) -> Result<RescanReport> {
        let sources = available_sources(config)?;
        let mut counters = Counters::load(&self.database, config.triage_confidence)?;
        let mut walk = Walk {
            settings: ScanSettings::new(config, policy),
            known: self.known_stamps(&sources)?,
            seen: HashSet::new(),
            report: RescanReport::default(),
        };
        self.walk(&sources, &mut walk, &mut counters, progress)?;
        self.sweep_artifacts(&mut counters)?;
        let transaction = self.database.rw_transaction()?;
        counters.save(&transaction)?;
        let mut manifest: Manifest = transaction
            .get()
            .primary(0_u8)?
            .ok_or_else(|| IndexError::Catalog("manifest missing".into()))?;
        manifest.generation += 1;
        manifest.scanned_at = jiff::Timestamp::now().to_string();
        manifest.counts = counts(&transaction)?;
        transaction.upsert(manifest)?;
        transaction.commit()?;
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
        &self,
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
            seen: HashSet::new(),
            report: RescanReport::default(),
        };
        self.walk(sources, &mut walk, &mut counters, progress)?;
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
        })?;
        transaction.commit()?;
        self.recount_summary()?;
        Ok(())
    }

    /// The one pass a fresh scan and a rescan share. A fresh scan starts with nothing known,
    /// so every unit is prepared; a rescan starts with the stamps it stored last time.
    fn walk(
        &self,
        sources: &[&SourceConfig],
        walk: &mut Walk,
        counters: &mut Counters,
        mut progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        let noise = self.noise_filter()?;
        for source in sources {
            progress(&format!("discovering {}", source.name), counters.entries);
            let discovered = discover::discover(source, &noise)?;
            let kind = discovered.kind();
            let repo_ids = self.store_source(source, kind, discovered.repos(), counters)?;
            for (repo, repo_id) in discovered.repos().iter().zip(repo_ids) {
                for (ordinal, revision) in repo.revisions.iter().enumerate() {
                    let context = EntryContext {
                        repo,
                        repo_id,
                        revision,
                        settings: walk.settings,
                        kind,
                        ordinal: u32::try_from(ordinal)
                            .map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))?,
                    };
                    let documents = revision_documents(&revision.entries);
                    let sets = shard_sets_by_name(&revision.entries, &documents);
                    let units = revision_units(&revision.entries, sets);
                    let plan = revision_plan(&context, documents, &units);
                    let labels = set_labels(&plan.naming, &units);
                    for (unit, label) in units.iter().zip(labels) {
                        let prepare = match decide_unit(walk, &revision.entries, unit) {
                            UnitDecision::Skip => {
                                mark_unchanged(walk, &revision.entries, unit.positions());
                                continue;
                            }
                            UnitDecision::Prepare { prepare, unchanged } => {
                                mark_unchanged(walk, &revision.entries, &unchanged);
                                prepare
                            }
                        };
                        let transaction = self.database.rw_transaction()?;
                        replace_known(&transaction, walk, counters, &revision.entries, &prepare)?;
                        let narrowed = match unit {
                            Unit::Set(_) => None,
                            Unit::Chunk(_) => Some(Unit::Chunk(prepare)),
                        };
                        let prepared = prepare_unit(
                            &context,
                            &plan,
                            narrowed.as_ref().unwrap_or(unit),
                            label.as_deref(),
                        )?;
                        store_unit(&transaction, &context, prepared, counters)?;
                        log_changes(&transaction, counters)?;
                        transaction.commit()?;
                        progress(&source.name, counters.entries);
                    }
                }
            }
        }
        self.retract_absent(walk, counters)?;
        self.record_scan_stats(sources, counters)
    }

    /// Records what each walked source holds now that every unit of it is stored and the
    /// paths the walk never reached have been retracted: the files still on this disk and
    /// the bytes they occupy, summed from the repositories under it.
    fn record_scan_stats(&self, sources: &[&SourceConfig], counters: &mut Counters) -> Result<()> {
        let at = jiff::Timestamp::now().to_string();
        let transaction = self.database.rw_transaction()?;
        counters.derived.flush(&transaction)?;
        let repositories: Vec<Repository> = transaction
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        for source in sources {
            let Some(stored) = transaction.get().primary::<Source>(source.name.clone())? else {
                continue;
            };
            let mut file_count = 0;
            let mut byte_count = 0u64;
            for repository in repositories
                .iter()
                .filter(|repository| repository.source == source.name)
            {
                file_count += u64::from(repository.present_files);
                byte_count = byte_count.saturating_add(repository.present_bytes);
            }
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

    /// Writes the records a source needs before any of its files are read, and hands back
    /// the identity given to each repository in discovery order. A repository the catalog
    /// already holds keeps its id and takes the freshly discovered revisions and refs.
    fn store_source(
        &self,
        source: &SourceConfig,
        kind: SourceKind,
        repos: &[DiscoveredRepo],
        counters: &mut Counters,
    ) -> Result<Vec<i64>> {
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
        let mut ids = Vec::with_capacity(repos.len());
        for repo in repos {
            let key = repository_key(&source.name, &repo.repo);
            let stored: Option<Repository> = transaction
                .get()
                .secondary(RepositoryKey::key, key.as_str())?;
            let (repo_id, present) = match stored {
                Some(stored) => (stored.id, (stored.present_files, stored.present_bytes)),
                None => (counters.next_repo()?, (0, 0)),
            };
            transaction.upsert(repository(repo_id, &source.name, repo, present))?;
            ids.push(repo_id);
        }
        transaction.commit()?;
        Ok(ids)
    }

    /// Every path the catalog already holds for the selected sources, with the stamp that
    /// proves it unchanged and the facts a decision needs before any file is opened.
    ///
    /// Keyed by [`path_hash`] rather than by the path: a million paths held as text is the
    /// largest thing a rescan keeps in memory, and nothing here reads the path back.
    fn known_stamps(&self, sources: &[&SourceConfig]) -> Result<HashMap<u128, Known>> {
        let names: HashSet<&str> = sources.iter().map(|source| source.name.as_str()).collect();
        let read = self.database.r_transaction()?;
        let repositories: Vec<Repository> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut shards: HashMap<i64, Option<u32>> = HashMap::new();
        let mut known = HashMap::new();
        for repository in repositories
            .iter()
            .filter(|repository| names.contains(repository.source.as_str()))
        {
            for entry in read
                .scan()
                .secondary::<Entry>(EntryKey::repo_id)?
                .start_with(repository.id)?
            {
                let entry: Entry = entry?;
                let (Some(abs_path), Some(mtime), Some(inode)) = (
                    entry.placed.resolve(&repository.root_path, &entry.rel_path),
                    entry.mtime,
                    entry.inode,
                ) else {
                    continue;
                };
                let shard_count = match entry.artifact_id {
                    Some(id) => shard_count_of(&read, &mut shards, id)?,
                    None => None,
                };
                known.insert(
                    path_hash(Path::new(&abs_path)),
                    Known {
                        entry_id: entry.id,
                        stamp: FileStamp {
                            inode,
                            mtime,
                            size: entry.size,
                        },
                        artifact_id: entry.artifact_id,
                        shard_count,
                    },
                );
            }
        }
        Ok(known)
    }

    /// A path the walk never reached is still a row: it keeps the path it had and records
    /// that this node no longer holds the bytes.
    fn retract_absent(&self, walk: &mut Walk, counters: &mut Counters) -> Result<()> {
        let missing: Vec<Known> = walk
            .known
            .values()
            .filter(|known| !walk.seen.contains(&known.entry_id))
            .copied()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let transaction = self.database.rw_transaction()?;
        for known in missing {
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
            if let Some(id) = known.artifact_id {
                counters.touch(&transaction, id)?;
                counters.pending.push(id);
            }
            walk.report.absent += 1;
        }
        log_changes(&transaction, counters)?;
        transaction.commit()?;
        Ok(())
    }

    /// Recounts, for every artifact this walk touched, how many present paths reach it, and
    /// deletes the artifact and everything hanging off it once none does. A model split
    /// across shards is reached once per complete copy of its files, not once per file.
    /// The sweep commits in batches, because a fresh scan touches every artifact it wrote.
    pub(super) fn sweep_artifacts(&self, counters: &mut Counters) -> Result<()> {
        let ids: Vec<i64> = counters.touched.iter().copied().collect();
        for batch in ids.chunks(CHUNK) {
            let transaction = self.database.rw_transaction()?;
            for id in batch {
                if sweep_artifact(&transaction, *id)? {
                    counters.pending.push(*id);
                }
            }
            log_changes(&transaction, counters)?;
            transaction.commit()?;
        }
        self.summarise(counters)
    }

    /// Moves what the facet rail reads as DISK PRESSURE and what the stats route prints as
    /// its totals by the difference this pass made, reading only the records it touched. A
    /// pass that touched nothing writes nothing.
    pub(super) fn summarise(&self, counters: &Counters) -> Result<()> {
        let read = self.database.r_transaction()?;
        let stored: Option<Summary> = read.get().primary(0_u8)?;
        let mut summary = stored.clone().unwrap_or_else(blank_summary);
        for (id, before) in &counters.before {
            let record: Option<ArtifactRecord> = read.get().primary(*id)?;
            let after = SummaryPart::of(record.as_ref(), counters.triage);
            shift_summary(&mut summary, *before, after);
        }
        summary.repositories = read.len().primary::<Repository>()?;
        drop(read);
        if stored.as_ref() == Some(&summary) {
            return Ok(());
        }
        let transaction = self.database.rw_transaction()?;
        transaction.upsert(summary)?;
        transaction.commit()?;
        Ok(())
    }

    /// Counts every artifact record again and writes what they add up to, which is what a
    /// fresh scan, a migration and `hoard doctor` use to prove the incremental totals.
    /// An artifact reached by more than one path wastes every copy but the first; an
    /// artifact no path reaches at all is not indexed bytes on this node.
    pub fn recount_summary(&self) -> Result<super::Totals> {
        let triage = IndexConfig::default().triage_confidence;
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
        drop(read);
        let totals = super::Totals {
            artifacts: summary.artifacts,
            present_artifacts: summary.present_artifacts,
            triage_artifacts: summary.triage_artifacts,
            indexed_bytes: summary.indexed_bytes,
            repositories: summary.repositories,
            advertised_artifacts: summary.advertised_artifacts,
        };
        let transaction = self.database.rw_transaction()?;
        transaction.upsert(summary)?;
        transaction.commit()?;
        Ok(totals)
    }
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
    known: HashMap<u128, Known>,
    seen: HashSet<i64>,
    report: RescanReport,
}

/// What the catalog already holds for one path, so a walk can decide without opening it.
#[derive(Clone, Copy)]
struct Known {
    entry_id: i64,
    stamp: FileStamp,
    artifact_id: Option<i64>,
    shard_count: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RescanReport {
    pub unchanged: u64,
    pub changed: u64,
    pub added: u64,
    pub absent: u64,
}

enum UnitDecision {
    Skip,
    Prepare {
        prepare: Vec<usize>,
        unchanged: Vec<usize>,
    },
}

fn known_of(walk: &Walk, entry: &DiscoveredEntry) -> Option<Known> {
    entry
        .abs_path
        .as_ref()
        .and_then(|path| walk.known.get(&path_hash(path)))
        .copied()
}

/// A path as the key a rescan looks it up by. Two distinct paths colliding on 128 bits is
/// not a case this workspace budgets for.
fn path_hash(path: &Path) -> u128 {
    xxhash_rust::xxh3::xxh3_128(path.as_os_str().as_encoded_bytes())
}

fn unchanged_entry(walk: &Walk, entry: &DiscoveredEntry) -> bool {
    match (known_of(walk, entry), entry.stamp) {
        (Some(known), Some(stamp)) => known.stamp == stamp,
        (Some(_), None) | (None, _) => false,
    }
}

/// What a unit costs this walk: nothing when every file it holds is the file the catalog
/// already read, otherwise the positions to prepare again. A set is prepared whole, because
/// its artifact is the union of its shards.
fn decide_unit(walk: &Walk, entries: &[DiscoveredEntry], unit: &Unit) -> UnitDecision {
    match unit {
        Unit::Set(set) => {
            let members = u32::try_from(set.members.len()).unwrap_or(u32::MAX);
            let grouped = set
                .members
                .first()
                .and_then(|position| known_of(walk, &entries[*position]))
                .is_some_and(|known| known.shard_count == Some(members));
            let intact = grouped
                && set
                    .members
                    .iter()
                    .all(|position| unchanged_entry(walk, &entries[*position]));
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
                .partition(|position| unchanged_entry(walk, &entries[*position]));
            match prepare.is_empty() {
                true => UnitDecision::Skip,
                false => UnitDecision::Prepare { prepare, unchanged },
            }
        }
    }
}

fn mark_unchanged(walk: &mut Walk, entries: &[DiscoveredEntry], positions: &[usize]) {
    for position in positions {
        if let Some(known) = known_of(walk, &entries[*position]) {
            walk.seen.insert(known.entry_id);
            walk.report.unchanged += 1;
        }
    }
}

/// Retires the rows the walk is about to write again, so a changed file keeps neither its
/// old entry nor a claim on an artifact its new bytes may no longer establish.
fn replace_known(
    transaction: &RwTransaction<'_>,
    walk: &mut Walk,
    counters: &mut Counters,
    entries: &[DiscoveredEntry],
    positions: &[usize],
) -> Result<()> {
    for position in positions {
        let Some(known) = known_of(walk, &entries[*position]) else {
            walk.report.added += 1;
            continue;
        };
        walk.seen.insert(known.entry_id);
        walk.report.changed += 1;
        if let Some(id) = known.artifact_id {
            counters.touch(transaction, id)?;
        }
        if let Some(entry) = transaction.get().primary::<Entry>(known.entry_id)? {
            counters.derived.removed(&entry);
            transaction.remove(entry)?;
        }
    }
    Ok(())
}

pub(super) struct Counters {
    repos: u64,
    pub(super) artifacts: u64,
    pub(super) entries: u64,
    pub(super) touched: HashSet<i64>,
    pub(super) pending: Vec<i64>,
    pub(super) derived: DerivedShift,
    triage: f32,
    before: HashMap<i64, SummaryPart>,
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
        let stored: Option<Allocator> = database.r_transaction()?.get().primary(0_u8)?;
        let allocator = stored.unwrap_or(Allocator {
            id: 0,
            repos: 0,
            artifacts: 0,
            entries: 0,
        });
        Ok(Self {
            repos: allocator.repos,
            artifacts: allocator.artifacts,
            entries: allocator.entries,
            touched: HashSet::new(),
            pending: Vec::new(),
            derived: DerivedShift::default(),
            triage,
            before: HashMap::new(),
        })
    }

    /// Records that this pass is about to change an artifact, keeping what its record
    /// contributed to the summary beforehand so the pass moves the summary by a difference
    /// rather than counting every record again. Call it before the record is written.
    pub(super) fn touch(&mut self, transaction: &RwTransaction<'_>, id: i64) -> Result<()> {
        self.touched.insert(id);
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            let record: Option<ArtifactRecord> = transaction.get().primary(id)?;
            slot.insert(SummaryPart::of(record.as_ref(), self.triage));
        }
        Ok(())
    }

    /// Writes the allocator and the present counts this pass moved, which is why every
    /// writer commits through it.
    pub(super) fn save(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        self.derived.flush(transaction)?;
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
pub(super) fn log_changes(transaction: &RwTransaction<'_>, counters: &mut Counters) -> Result<()> {
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

pub(super) fn next_id(counter: &mut u64) -> Result<i64> {
    let id = counter
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| IndexError::Catalog("catalog identity space exhausted".into()))?;
    *counter = id as u64;
    Ok(id)
}

/// The stored shape of a discovered repository. `present` is what the copy already on
/// record counted, because rediscovering a repository does not move a single file.
fn repository(id: i64, source: &str, repo: &DiscoveredRepo, present: (u32, u64)) -> Repository {
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
        revisions: repo
            .revisions
            .iter()
            .enumerate()
            .map(|(ordinal, revision)| Revision {
                name: revision_name(repo, ordinal),
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
                    .filter(|reference| reference.revision == ordinal)
                    .map(|reference| Ref {
                        name: reference.name.clone(),
                        kind: reference.kind,
                    })
                    .collect(),
            })
            .collect(),
    }
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

/// What one file that becomes an artifact contributes: only such a file pays for inference,
/// a copy of the revision's card, and a content identity.
struct ArtifactDraft {
    identity: u128,
    inference: Inference,
    card: Option<String>,
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
            sniff::name_format(Path::new(name)),
            Some(
                ArtifactFormat::Readme
                    | ArtifactFormat::Config
                    | ArtifactFormat::Diffusers
                    | ArtifactFormat::Transformers
            )
        )
}

fn names_an_artifact(entry: &DiscoveredEntry) -> bool {
    sniff::name_format(Path::new(file_name(&entry.rel_path)))
        .is_some_and(ArtifactFormat::is_artifact)
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
        HashPolicy::Now => hash_presences(&mut presences, context.settings.threshold),
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
fn hash_presences(presences: &mut [Presence], threshold: u64) {
    let digests: Vec<Option<(Sha256, Option<GitSha1>)>> = presences
        .par_iter()
        .map(|presence| {
            let path = presence
                .abs_path
                .as_deref()
                .filter(|_| presence.sha256.is_none())?;
            match content_hashes(path, presence.size, threshold) {
                Ok(hashes) => Some(hashes),
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
    match sniff::sniff_path(path) {
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
                card: plan.context.card().map(str::to_owned),
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

fn identity(entry: &DiscoveredEntry, presence: &Presence) -> Result<u128> {
    let text = match (presence.sha256, entry.git_sha1, entry.abs_path.as_deref()) {
        (Some(sha), _, _) => format!("sha256:{sha}"),
        (None, Some(sha), _) => format!("git:{sha}"),
        (None, None, Some(path)) => format!(
            "path:{}",
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .display()
        ),
        (None, None, None) => {
            return Err(IndexError::Catalog(format!(
                "absent entry has no content identity: {}",
                entry.rel_path
            )));
        }
    };
    Ok(xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
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
        location: location_key(
            context.repo_id,
            context.ordinal,
            &prepared.discovered.rel_path,
        ),
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

/// The record this entry contributes a path to.
///
/// A first path writes the record and names it; an identity the catalog already holds keeps
/// its id, its name and when it was first seen, and takes what this file's header just
/// proved. The identity is a unique secondary key, and a read inside the writing
/// transaction sees the rows that transaction has already written, so two copies of one
/// file in one unit reach the same record without a cache of what this pass has written.
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
        record.name = stored.name.clone();
        record.org = stored.org.clone();
    }
    let verdict: Option<Override> = transaction.get().primary(id)?;
    if let Some(verdict) = &verdict {
        apply_override(&mut record, verdict);
    }
    transaction.upsert(SearchText {
        artifact_id: id,
        keys: tensor_keys(detail.tensors.as_ref()),
        card: draft.card.clone().unwrap_or_default(),
    })?;
    transaction.upsert(detail)?;
    match stored {
        Some(_) => {
            transaction.upsert(record.clone())?;
        }
        None => transaction.insert(record.clone())?,
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
            sniff_version: crate::sniff::SNIFF_VERSION,
        },
        group_bytes,
        shard_count,
        triage_cleared: false,
        first_seen: first_seen.unwrap_or_else(|| jiff::Timestamp::now().to_string()),
    };
    let detail = ArtifactDetail {
        artifact_id: id,
        tensors,
        evidence: inference.evidence.clone(),
    };
    (record, detail)
}
