use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use native_db::Database;
use native_db::transaction::RwTransaction;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};
use serde::Serialize;

use hoard_core::{Artifact, ArtifactFormat, ArtifactId, BlobId, Sha256, State, TensorSummary};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::discover::{
    self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, NoiseFilter, RevisionName,
};
use crate::hash::FileStamp;
use crate::infer::{self, InferInput, Inference};
use crate::scan::{
    InferenceContext, Naming, artifact_name, disambiguate_labels, index_document, inference_path,
    shard_label,
};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};
use crate::sniff::{self, SniffResult};

use super::models::{
    Allocator, ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, Manifest,
    Repository, RepositoryKey, Revision, SearchChange, SearchText, Source, Summary, repository_key,
};
use crate::error::{IndexError, Result};

use super::{Catalog, builder, models};

impl Catalog {
    /// Creates a fresh native catalog directly from configured sources, without opening SQLite.
    /// Existing output directories are refused. Discovery supports the existing source layouts;
    /// a model split across shard files is one artifact anchored on its lowest shard.
    pub fn scan_sources(
        directory: &Path,
        node: &str,
        config: &IndexConfig,
        progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        let (directory, sources) = validate_output(directory, config)?;
        let mut catalog = Self::create(directory)?;
        match catalog
            .fill(node, &sources, config.triage_confidence, progress)
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
        progress: impl FnMut(&str, u64),
    ) -> Result<RescanReport> {
        let sources = available_sources(config)?;
        let mut counters = Counters::load(&self.database)?;
        let mut walk = Walk {
            triage: config.triage_confidence,
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
            Ok(database) => Ok(Self {
                database,
                directory,
                search_cache: std::sync::RwLock::new(None),
            }),
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
        triage: f32,
        progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        let mut counters = Counters::load(&self.database)?;
        let mut walk = Walk {
            triage,
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
            format_version: 5,
            generation: 1,
            scanned_at: jiff::Timestamp::now().to_string(),
            node: node.to_owned(),
            counts: counts(&transaction)?,
            search_directory: None,
            search_generation: 0,
            search_seq: 0,
        })?;
        transaction.commit()?;
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
        for source in sources {
            progress(&format!("discovering {}", source.name), counters.entries);
            let discovered = discover::discover(source, &NoiseFilter::default())?;
            let kind = discovered.kind();
            let repo_ids = self.store_source(source, kind, discovered.repos(), counters)?;
            for (repo, repo_id) in discovered.repos().iter().zip(repo_ids) {
                for (ordinal, revision) in repo.revisions.iter().enumerate() {
                    let context = EntryContext {
                        repo,
                        repo_id,
                        revision,
                        triage: walk.triage,
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
        self.retract_absent(walk, counters)
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
        transaction.upsert(Source {
            name: source.name.clone(),
            config: source.clone(),
            resolved_kind: kind,
        })?;
        let mut ids = Vec::with_capacity(repos.len());
        for repo in repos {
            let key = repository_key(&source.name, &repo.repo);
            let stored: Option<Repository> = transaction
                .get()
                .secondary(RepositoryKey::key, key.as_str())?;
            let repo_id = match stored {
                Some(stored) => stored.id,
                None => counters.next_repo()?,
            };
            transaction.upsert(repository(repo_id, &source.name, repo))?;
            ids.push(repo_id);
        }
        transaction.commit()?;
        Ok(ids)
    }

    /// Every path the catalog already holds for the selected sources, with the stamp that
    /// proves it unchanged and the facts a decision needs before any file is opened.
    fn known_stamps(&self, sources: &[&SourceConfig]) -> Result<HashMap<PathBuf, Known>> {
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
            let entries: Vec<Entry> = read
                .scan()
                .secondary::<Entry>(EntryKey::repo_id)?
                .start_with(repository.id)?
                .collect::<native_db::db_type::Result<_>>()?;
            for entry in entries {
                let (Some(abs_path), Some(mtime), Some(inode)) =
                    (entry.abs_path.as_ref(), entry.mtime, entry.inode)
                else {
                    continue;
                };
                let shard_count = match entry.artifact_id {
                    Some(id) => shard_count_of(&read, &mut shards, id)?,
                    None => None,
                };
                known.insert(
                    PathBuf::from(abs_path),
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
            transaction.upsert(Entry {
                state: State::Remote,
                preference: preference(State::Remote, false),
                mtime: None,
                inode: None,
                ..entry
            })?;
            if let Some(id) = known.artifact_id {
                counters.touched.insert(id);
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
    fn sweep_artifacts(&self, counters: &mut Counters) -> Result<()> {
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
        self.summarise_disk_pressure()
    }

    /// Recounts what duplicated paths cost, which the facet rail reads as DISK PRESSURE.
    /// An artifact reached by more than one path wastes every copy but the first.
    fn summarise_disk_pressure(&self) -> Result<()> {
        let read = self.database.r_transaction()?;
        let mut summary = Summary {
            id: 0,
            reclaimable_bytes: 0,
            duplicate_artifacts: 0,
        };
        for record in read.scan().primary::<ArtifactRecord>()?.all()? {
            let record = record?;
            let copies = u64::from(record.path_count.saturating_sub(1));
            if copies == 0 {
                continue;
            }
            let bytes = record.group_bytes.unwrap_or(record.artifact.size);
            summary.reclaimable_bytes = summary
                .reclaimable_bytes
                .saturating_add(bytes.saturating_mul(copies));
            summary.duplicate_artifacts += 1;
        }
        drop(read);
        let transaction = self.database.rw_transaction()?;
        transaction.upsert(summary)?;
        transaction.commit()?;
        Ok(())
    }
}

/// Whether the sweep changed what the search index should hold for this artifact.
fn sweep_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<bool> {
    let referencing: Vec<Entry> = transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(id))?
        .collect::<native_db::db_type::Result<_>>()?;
    let Some(record) = transaction.get().primary::<ArtifactRecord>(id)? else {
        return Ok(false);
    };
    let present = referencing
        .iter()
        .filter(|entry| entry.state != State::Remote)
        .count();
    let shards = usize::try_from(record.shard_count.unwrap_or(1))
        .unwrap_or(1)
        .max(1);
    match u32::try_from(present.div_ceil(shards)).unwrap_or(u32::MAX) {
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

fn counts(transaction: &RwTransaction<'_>) -> Result<Vec<(String, u64)>> {
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
    triage: f32,
    known: HashMap<PathBuf, Known>,
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
        .and_then(|path| walk.known.get(path))
        .copied()
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
            counters.touched.insert(id);
        }
        if let Some(entry) = transaction.get().primary::<Entry>(known.entry_id)? {
            transaction.remove(entry)?;
        }
    }
    Ok(())
}

struct Counters {
    repos: u64,
    artifacts: u64,
    entries: u64,
    artifacts_by_identity: HashMap<u128, i64>,
    touched: HashSet<i64>,
    pending: Vec<i64>,
}

impl Counters {
    fn load(database: &Database<'static>) -> Result<Self> {
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
            artifacts_by_identity: HashMap::new(),
            touched: HashSet::new(),
            pending: Vec::new(),
        })
    }

    fn save(&self, transaction: &RwTransaction<'_>) -> Result<()> {
        transaction.upsert(Allocator {
            id: 0,
            repos: self.repos,
            artifacts: self.artifacts,
            entries: self.entries,
        })?;
        Ok(())
    }

    fn next_repo(&mut self) -> Result<i64> {
        next_id(&mut self.repos)
    }
}

/// Records, in the transaction that made them, which artifacts the published search index no
/// longer matches. The sequence continues past both the highest key the log still holds and
/// the refresh cursor, because a drained log holds neither the numbers it has already spent
/// nor any proof that a refresh has not already passed them.
fn log_changes(transaction: &RwTransaction<'_>, counters: &mut Counters) -> Result<()> {
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

fn next_id(counter: &mut u64) -> Result<i64> {
    let id = counter
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| IndexError::Catalog("catalog identity space exhausted".into()))?;
    *counter = id as u64;
    Ok(id)
}

fn repository(id: i64, source: &str, repo: &DiscoveredRepo) -> Repository {
    Repository {
        id,
        key: repository_key(source, &repo.repo),
        source: source.to_owned(),
        repo: repo.repo.clone(),
        root_path: repo.root_path.to_string_lossy().into_owned(),
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
                refs: repo
                    .refs
                    .iter()
                    .filter(|reference| reference.revision == ordinal)
                    .map(|reference| reference.name.clone())
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
                reference.kind != crate::store::revision::RefKind::Branch,
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
                lfs_pointer: true,
                pointer_bytes: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone(),
                readable: sniffed.is_some(),
                size: entry.size,
                sha256: entry.sha256,
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
const CHUNK: usize = 1024;

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
/// a README or config beside the weights describes them the way it does in the SQLite
/// scanner. Nothing larger than a unit is read at once.
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
    let presences: Vec<Presence> = positions
        .iter()
        .zip(&sniffed)
        .map(|(position, result)| Presence::of(&entries[*position], result.as_ref()))
        .collect();
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
    triage: f32,
    kind: SourceKind,
    ordinal: u32,
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
            artifact_triage(&prepared, record, context.triage),
        )),
        None => anchor,
    };
    let triage = stored.is_some_and(|(_, triage)| triage);
    let state = entry_state(&prepared, triage);
    let artifact_id = stored.map(|(id, _)| id);
    transaction.insert(Entry {
        id: entry_id,
        repo_id: context.repo_id,
        artifact_id,
        revision: context.ordinal,
        rel_path: prepared.discovered.rel_path.clone(),
        abs_path: prepared
            .presence
            .abs_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        size: prepared.presence.size,
        state,
        triage,
        preference: preference(state, prepared.presence.present()),
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        git_sha1: prepared.discovered.git_sha1,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
    })?;
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

/// The record this entry contributes a path to. A first path writes the record; an identity
/// the catalog already holds keeps its id and takes what this file's header just proved,
/// because only a file that was read again reaches this point twice.
fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    if let Some(id) = counters.artifacts_by_identity.get(&draft.identity).copied() {
        counters.touched.insert(id);
        counters.pending.push(id);
        return transaction
            .get()
            .primary(id)?
            .ok_or_else(|| IndexError::Catalog(format!("artifact {id} vanished mid-scan")));
    }
    let stored: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, draft.identity)?;
    let id = match stored {
        Some(stored) => stored.id,
        None => next_id(&mut counters.artifacts)?,
    };
    let (record, detail) = artifact_record(id, prepared, draft);
    transaction.upsert(SearchText {
        artifact_id: id,
        keys: tensor_keys(detail.tensors.as_ref()),
        card: draft.card.clone().unwrap_or_default(),
    })?;
    transaction.upsert(detail)?;
    transaction.upsert(record.clone())?;
    counters.artifacts_by_identity.insert(draft.identity, id);
    counters.touched.insert(id);
    counters.pending.push(id);
    Ok(record)
}

fn tensor_keys(tensors: Option<&TensorSummary>) -> String {
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

/// The SQLite index scores a candidate row the same way in `schema_v2.sql`: a node is always
/// itself here and an entry's blob is always its artifact's blob, so both constants are fixed.
fn preference(state: State, present: bool) -> i64 {
    let weight = match state {
        State::Served => 3,
        State::Local | State::Triage => 2,
        State::Stale => 1,
        State::Pulling | State::Remote => 0,
    };
    (4 + weight + i64::from(present)) * 2 + 1
}

fn artifact_record(
    id: i64,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
) -> (ArtifactRecord, ArtifactDetail) {
    let inference = &draft.inference;
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
    let record = ArtifactRecord {
        id,
        identity: draft.identity,
        name: prepared.name.clone(),
        org: prepared.org.clone(),
        path_count: 1,
        artifact: Artifact {
            id: ArtifactId::new(id),
            blob: BlobId::new(id),
            sha256: prepared.presence.sha256,
            size: prepared.presence.size,
            format: prepared
                .sniffed
                .as_ref()
                .map_or(ArtifactFormat::Unknown, |sniffed| sniffed.format),
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
            sniff_version: crate::scan::SNIFF_VERSION,
        },
        group_bytes,
        shard_count,
    };
    let detail = ArtifactDetail {
        artifact_id: id,
        tensors,
        evidence: inference.evidence.clone(),
    };
    (record, detail)
}
