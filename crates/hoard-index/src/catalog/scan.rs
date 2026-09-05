use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use native_db::Builder;
use native_db::transaction::RwTransaction;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, BlobId, EntryId, RepoRowId, Sha256, State, TensorSummary,
};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::discover::{
    self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, NoiseFilter, RevisionName,
};
use crate::infer::{self, InferInput, Inference};
use crate::scan::{
    InferenceContext, Naming, artifact_name, disambiguate_labels, index_document, inference_path,
    shard_label,
};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};
use crate::sniff::{self, SniffResult};
use crate::store::SearchRow;

use super::models::{
    ArtifactRecord, ArtifactRecordKey, Entry, Manifest, Repository, Revision, SearchEntry,
    SearchEntryKey, SearchText, Source,
};
use crate::error::{IndexError, Result};

use super::{Catalog, models};

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
        let catalog = Self::create(directory)?;
        match catalog.fill(node, &sources, config.triage_confidence, progress) {
            Ok(()) => Ok(catalog),
            Err(error) => {
                let directory = catalog.directory.clone();
                drop(catalog);
                remove_partial(&directory);
                Err(error)
            }
        }
    }

    /// Claims a fresh directory, leaving nothing behind if the database cannot be created.
    fn create(directory: PathBuf) -> Result<Self> {
        std::fs::create_dir(&directory)?;
        match Builder::new().create(models()?, directory.join("catalog.redb")) {
            Ok(database) => Ok(Self {
                database,
                directory,
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
        mut progress: impl FnMut(&str, u64),
    ) -> Result<()> {
        let mut counters = Counters::default();
        for source in sources {
            progress(&format!("discovering {}", source.name), counters.entries);
            let discovered = discover::discover(source, &NoiseFilter::default())?;
            let kind = discovered.kind();
            let transaction = self.database.rw_transaction()?;
            transaction.insert(Source {
                name: source.name.clone(),
                config: (*source).clone(),
                resolved_kind: kind,
            })?;
            for repo in discovered.repos() {
                let repo_id = counters.next_repo()?;
                transaction.insert(repository(repo_id, &source.name, repo))?;
                for (ordinal, revision) in repo.revisions.iter().enumerate() {
                    let context = EntryContext {
                        node,
                        source: &source.name,
                        repo,
                        repo_id,
                        revision,
                        revision_name: revision_name(repo, ordinal),
                        triage,
                        kind,
                        ordinal: u32::try_from(ordinal)
                            .map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))?,
                    };
                    let mut anchors: HashMap<usize, (i64, bool)> = HashMap::new();
                    let mut members = Vec::new();
                    for prepared in prepare_revision(&context)? {
                        match &prepared.role {
                            EntryRole::Member { group } => members.push((*group, prepared)),
                            EntryRole::Anchor(facts) => {
                                let group = facts.group;
                                let stored = store_entry(
                                    &transaction,
                                    &context,
                                    prepared,
                                    None,
                                    &mut counters,
                                )?;
                                if let Some(stored) = stored {
                                    anchors.insert(group, stored);
                                }
                            }
                            EntryRole::Standalone => {
                                store_entry(&transaction, &context, prepared, None, &mut counters)?;
                            }
                        }
                    }
                    for (group, prepared) in members {
                        store_entry(
                            &transaction,
                            &context,
                            prepared,
                            anchors.get(&group).copied(),
                            &mut counters,
                        )?;
                    }
                    progress(&source.name, counters.entries);
                }
            }
            transaction.commit()?;
        }
        self.finalize_path_counts()?;
        let transaction = self.database.rw_transaction()?;
        transaction.insert(Manifest {
            id: 0,
            format_version: 2,
            generation: 1,
            scanned_at: jiff::Timestamp::now().to_string(),
            node: node.to_owned(),
            counts: vec![
                ("sources".into(), sources.len() as u64),
                ("repositories".into(), counters.repos),
                ("artifacts".into(), counters.artifacts),
                ("entries".into(), counters.entries),
            ],
            search_directory: None,
            search_generation: 0,
        })?;
        transaction.commit()?;
        Ok(())
    }

    /// A row records how many paths its artifact spans, and the last path is only known once
    /// every source has been walked, so the shared artifacts are revisited once at the end.
    fn finalize_path_counts(&self) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let shared: Vec<ArtifactRecord> = transaction
            .scan()
            .primary::<ArtifactRecord>()?
            .all()?
            .filter(|record| record.as_ref().map_or(true, |record| record.path_count > 1))
            .collect::<native_db::db_type::Result<_>>()?;
        for record in shared {
            let entries: Vec<SearchEntry> = transaction
                .scan()
                .secondary::<SearchEntry>(SearchEntryKey::artifact_id)?
                .range(record.id..=record.id)?
                .collect::<native_db::db_type::Result<_>>()?;
            for mut entry in entries {
                entry.row.path_count = record.path_count;
                entry.row.dupe = true;
                transaction.upsert(entry)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
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
    if config.sources.is_empty() {
        return Err(IndexError::Catalog("no sources are configured".into()));
    }
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
    let mut names = HashSet::with_capacity(config.sources.len());
    let mut selected = Vec::with_capacity(config.sources.len());
    for source in &config.sources {
        if !names.insert(&source.name) {
            return Err(IndexError::Catalog(format!(
                "duplicate source name: {}",
                source.name
            )));
        }
        let root = match source.path.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(
                    source = %source.name,
                    path = %source.path.display(),
                    %error,
                    "source root is unavailable; skipping"
                );
                continue;
            }
        };
        if output.starts_with(&root) {
            return Err(IndexError::Catalog(format!(
                "catalog output is inside scanned source {:?}",
                source.name
            )));
        }
        selected.push(source);
    }
    if selected.is_empty() {
        return Err(IndexError::Catalog(
            "no configured source is available".into(),
        ));
    }
    Ok((output, selected))
}

#[derive(Default)]
struct Counters {
    repos: u64,
    artifacts: u64,
    entries: u64,
}

impl Counters {
    fn next_repo(&mut self) -> Result<i64> {
        next_id(&mut self.repos)
    }
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
        source: source.to_owned(),
        repo: repo.repo.clone(),
        root_path: repo.root_path.to_string_lossy().into_owned(),
        default_ref: repo.default_ref.clone(),
        revisions: repo
            .revisions
            .iter()
            .enumerate()
            .map(|(ordinal, revision)| Revision {
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
    format.is_artifact() && !presence.pointer_bytes && !matches!(role, EntryRole::Member { .. })
}

/// What one file that becomes an artifact contributes: only such a file pays for inference,
/// a copy of the revision's card, and a content identity.
struct ArtifactDraft {
    identity: String,
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
#[derive(Clone)]
pub(super) struct GroupFacts {
    group: usize,
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
    Member { group: usize },
}

/// Everything the per entry pass of one revision reads besides the entry itself.
struct RevisionPlan<'a> {
    naming: Naming<'a>,
    context: InferenceContext,
}

/// The shard sets of one revision whose anchor was read and holds an artifact, addressed by
/// entry position. Members of any other set stay the files they are.
fn shard_sets(entries: &[DiscoveredEntry], sniffed: &[Option<SniffResult>]) -> Vec<ShardSet> {
    let documents: HashMap<usize, serde_json::Value> = entries
        .iter()
        .zip(sniffed)
        .enumerate()
        .filter(|(_, (entry, _))| shard::is_index_document(file_name(&entry.rel_path)))
        .filter_map(|(position, (entry, result))| {
            index_document(entry, result.as_ref()).map(|document| (position, document))
        })
        .collect();
    let mut positions = Vec::new();
    let mut candidates = Vec::new();
    for (position, (entry, result)) in entries.iter().zip(sniffed).enumerate() {
        let readable = result
            .as_ref()
            .is_some_and(|result| result.lfs_pointer.is_none());
        if !readable || !shard::is_shard_file(file_name(&entry.rel_path)) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.get(&position) {
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
        .filter(|set| {
            sniffed[set.anchor()]
                .as_ref()
                .is_some_and(|result| result.format.is_artifact())
        })
        .collect()
}

/// One artifact per set, described by the union of its shard headers and weighing them all.
fn group_facts(
    sets: &[ShardSet],
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    naming: &Naming<'_>,
) -> Vec<GroupFacts> {
    let mut labels: Vec<String> = sets.iter().map(|set| shard_label(naming, set)).collect();
    let stems: Vec<String> = sets.iter().map(|set| set.stem.clone()).collect();
    disambiguate_labels(&mut labels, &stems);
    sets.iter()
        .zip(labels)
        .enumerate()
        .map(|(group, (set, label))| {
            let parts: Vec<&TensorSummary> = set
                .members
                .iter()
                .filter_map(|member| sniffed[*member].as_ref())
                .filter_map(|result| result.tensors.as_ref())
                .collect();
            GroupFacts {
                group,
                label,
                bytes: set
                    .members
                    .iter()
                    .map(|member| presences[*member].size)
                    .sum(),
                shard_count: u32::try_from(set.members.len()).unwrap_or(u32::MAX),
                tensors: (!parts.is_empty()).then(|| shard::merge_tensor_summaries(&parts)),
                incomplete: set.is_incomplete(),
            }
        })
        .collect()
}

fn entry_roles(count: usize, sets: &[ShardSet], facts: &[GroupFacts]) -> Vec<EntryRole> {
    let mut roles: Vec<EntryRole> = (0..count).map(|_| EntryRole::Standalone).collect();
    for (set, facts) in sets.iter().zip(facts) {
        for member in &set.members {
            roles[*member] = match *member == set.anchor() {
                true => EntryRole::Anchor(facts.clone()),
                false => EntryRole::Member { group: facts.group },
            };
        }
    }
    roles
}

/// Sniffs a whole revision in parallel, then prepares every file against the revision's own
/// documents, so a README or config beside the weights describes them the way it does in the
/// SQLite scanner.
fn prepare_revision<'a>(context: &EntryContext<'a>) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let sniffed: Vec<Option<SniffResult>> = entries.par_iter().map(sniff_entry).collect();
    let presences: Vec<Presence> = entries
        .iter()
        .zip(&sniffed)
        .map(|(entry, result)| Presence::of(entry, result.as_ref()))
        .collect();
    let inference_context = InferenceContext::from_results(
        entries.iter().zip(&sniffed).filter_map(|(entry, result)| {
            result
                .as_ref()
                .map(|result| (entry.rel_path.as_str(), result))
        }),
    );
    let sets = shard_sets(entries, &sniffed);
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let standalone = sniffed
        .iter()
        .enumerate()
        .filter(|(position, result)| {
            !grouped.contains(position)
                && result
                    .as_ref()
                    .is_some_and(|result| result.format.is_artifact())
        })
        .count();
    let plan = RevisionPlan {
        naming: Naming {
            repo: &context.repo.repo,
            root_path: &context.repo.root_path,
            kind: context.kind,
            single: standalone + sets.len() <= 1,
        },
        context: inference_context,
    };
    let facts = group_facts(&sets, &sniffed, &presences, &plan.naming);
    let roles = entry_roles(entries.len(), &sets, &facts);
    entries
        .par_iter()
        .zip(sniffed.into_par_iter())
        .zip(presences.into_par_iter())
        .zip(roles.into_par_iter())
        .map(|(((entry, sniffed), presence), role)| {
            prepare_entry(entry, sniffed, presence, role, &plan)
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
        EntryRole::Standalone | EntryRole::Member { .. } => (
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

fn identity(entry: &DiscoveredEntry, presence: &Presence) -> Result<String> {
    match (presence.sha256, entry.git_sha1, entry.abs_path.as_deref()) {
        (Some(sha), _, _) => Ok(format!("sha256:{sha}")),
        (None, Some(sha), _) => Ok(format!("git:{sha}")),
        (None, None, Some(path)) => Ok(format!(
            "path:{}",
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .display()
        )),
        (None, None, None) => Err(IndexError::Catalog(format!(
            "absent entry has no content identity: {}",
            entry.rel_path
        ))),
    }
}

struct EntryContext<'a> {
    node: &'a str,
    source: &'a str,
    repo: &'a DiscoveredRepo,
    repo_id: i64,
    revision: &'a DiscoveredRevision,
    revision_name: String,
    triage: f32,
    kind: SourceKind,
    ordinal: u32,
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
    if let Some(record) = record {
        transaction.insert(SearchEntry {
            entry_id,
            artifact_id: record.id,
            repo_id: context.repo_id,
            preference: preference(state, prepared.presence.present()),
            row: search_row(entry_id, context, &prepared, &record, state, triage),
            git_sha1: prepared.discovered.git_sha1,
        })?;
        transaction.upsert(record)?;
    }
    transaction.insert(Entry {
        id: entry_id,
        repo_id: context.repo_id,
        revision: context.ordinal,
        rel_path: prepared.discovered.rel_path.clone(),
        abs_path: prepared
            .presence
            .abs_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        size: prepared.presence.size,
        artifact_id,
        state,
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
    })?;
    Ok(stored)
}

/// A set that found fewer files than it declares cannot be served whatever its header says.
fn artifact_triage(prepared: &PreparedEntry<'_>, record: &ArtifactRecord, confidence: f32) -> bool {
    let incomplete = match &prepared.role {
        EntryRole::Anchor(facts) => facts.incomplete,
        EntryRole::Standalone | EntryRole::Member { .. } => false,
    };
    record.artifact.needs_triage(confidence) || incomplete
}

/// The record this entry contributes a path to, counted but not yet written: the caller
/// reads it to build the search row and then stores it by value.
fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    let existing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, draft.identity.as_str())?;
    let mut artifact = match existing {
        Some(existing) => existing,
        None => {
            let record = artifact_record(next_id(&mut counters.artifacts)?, prepared, draft);
            transaction.insert(SearchText {
                artifact_id: record.id,
                keys: tensor_keys(record.tensors.as_ref()),
                card: draft.card.clone().unwrap_or_default(),
            })?;
            record
        }
    };
    artifact.path_count = artifact
        .path_count
        .checked_add(1)
        .ok_or_else(|| IndexError::Catalog("artifact path count overflow".into()))?;
    Ok(artifact)
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

fn artifact_record(id: i64, prepared: &PreparedEntry<'_>, draft: &ArtifactDraft) -> ArtifactRecord {
    let inference = &draft.inference;
    let (tensors, group_bytes, shard_count) = match &prepared.role {
        EntryRole::Anchor(facts) => (
            facts.tensors.clone(),
            Some(facts.bytes),
            Some(facts.shard_count),
        ),
        EntryRole::Standalone | EntryRole::Member { .. } => (
            prepared
                .sniffed
                .as_ref()
                .and_then(|sniffed| sniffed.tensors.clone()),
            None,
            None,
        ),
    };
    ArtifactRecord {
        id,
        identity: draft.identity.clone(),
        path_count: 0,
        artifact: Artifact {
            id: ArtifactId::new(id),
            blob: BlobId::new(id),
            sha256: prepared.presence.sha256,
            size: prepared.presence.size,
            format: prepared
                .sniffed
                .as_ref()
                .map_or(hoard_core::ArtifactFormat::Unknown, |sniffed| {
                    sniffed.format
                }),
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
        tensors,
        evidence: inference.evidence.clone(),
        group_bytes,
        shard_count,
    }
}

fn search_row(
    entry_id: i64,
    context: &EntryContext<'_>,
    prepared: &PreparedEntry<'_>,
    record: &ArtifactRecord,
    state: State,
    triage: bool,
) -> SearchRow {
    let artifact = &record.artifact;
    let entry = prepared.discovered;
    SearchRow {
        artifact_id: artifact.id,
        blob_id: artifact.blob,
        blob_sha256: artifact.sha256,
        role: artifact.role,
        sub_role: artifact.sub_role,
        role_confidence: artifact.role_confidence,
        org: prepared.org.clone(),
        name: prepared.name.clone(),
        base: artifact.base.clone(),
        base_label: artifact.base_label.clone(),
        format: artifact.format,
        precision: artifact.precision.clone(),
        quant: artifact.quant.clone(),
        size: record.group_bytes.unwrap_or(artifact.size),
        shard_count: record.shard_count.unwrap_or(1),
        rank: artifact.rank,
        alpha: artifact.alpha,
        params: artifact.params,
        context: artifact.context,
        revision: context.revision_name.clone(),
        revision_sha: match context.revision.name {
            RevisionName::Commit(sha) => Some(sha),
            RevisionName::Synthetic => None,
        },
        node: context.node.to_owned(),
        state,
        pull_progress: None,
        trigger: artifact.trigger.clone(),
        path: entry.rel_path.clone(),
        abs_path: prepared
            .presence
            .abs_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        path_count: record.path_count,
        dupe: record.path_count > 1,
        triage,
        last_used: None,
        repo: context.repo.repo.qualified(),
        repo_id: RepoRowId::new(context.repo_id),
        entry_id: EntryId::new(entry_id),
        source: Some(context.source.to_owned()),
        is_symlink: entry.is_symlink,
        lfs_pointer: prepared.presence.lfs_pointer,
    }
}
