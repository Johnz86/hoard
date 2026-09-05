use std::collections::HashSet;
use std::path::{Path, PathBuf};

use native_db::Builder;
use native_db::transaction::RwTransaction;
use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

use hoard_core::{Artifact, ArtifactFormat, ArtifactId, BlobId, EntryId, RepoRowId, Sha256, State};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::discover::{
    self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, NoiseFilter, RevisionName,
};
use crate::infer::{self, InferInput, Inference};
use crate::scan::{InferenceContext, Naming, artifact_name, inference_path};
use crate::shard::file_name;
use crate::sniff::{self, SniffResult};
use crate::store::SearchRow;

use super::models::{
    ArtifactRecord, ArtifactRecordKey, Entry, Manifest, Repository, Revision, SearchEntry,
    SearchText, Source,
};
use crate::error::{IndexError, Result};

use super::{Catalog, models};

impl Catalog {
    /// Creates a fresh native catalog directly from configured sources, without opening SQLite.
    /// Existing output directories are refused. Discovery supports the existing source layouts;
    /// this initial native path indexes files individually and does not group model shards.
    pub fn scan_sources(
        directory: &Path,
        node: &str,
        config: &IndexConfig,
        progress: impl FnMut(&str, u64),
    ) -> Result<Self> {
        let (directory, sources) = validate_output(directory, config)?;
        std::fs::create_dir(&directory)?;
        let catalog = Self {
            database: Builder::new().create(models()?, directory.join("catalog.redb"))?,
            directory,
        };
        match catalog.fill(node, &sources, config.triage_confidence, progress) {
            Ok(()) => Ok(catalog),
            Err(error) => {
                let directory = catalog.directory.clone();
                drop(catalog);
                if let Err(cleanup) = std::fs::remove_dir_all(&directory) {
                    tracing::warn!(
                        path = %directory.display(),
                        %cleanup,
                        "partial catalog left behind"
                    );
                }
                Err(error)
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
            transaction.commit()?;
            for repo in discovered.repos() {
                let repo_id = counters.next_repo()?;
                let transaction = self.database.rw_transaction()?;
                transaction.insert(repository(repo_id, &source.name, repo))?;
                transaction.commit()?;
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
                    let prepared = prepare_revision(&context)?;
                    let transaction = self.database.rw_transaction()?;
                    for prepared in prepared {
                        store_entry(&transaction, &context, prepared, &mut counters)?;
                    }
                    transaction.commit()?;
                    progress(&source.name, counters.entries);
                }
            }
        }
        let transaction = self.database.rw_transaction()?;
        transaction.insert(Manifest {
            id: 0,
            format_version: 1,
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

/// What this disk actually holds for one discovered path, after sniffing has run: an
/// unreadable file and a git-lfs pointer both stand for bytes that are not here.
struct Presence {
    abs_path: Option<PathBuf>,
    size: u64,
    sha256: Option<Sha256>,
    lfs_pointer: bool,
}

impl Presence {
    fn of(entry: &DiscoveredEntry, sniffed: Option<&SniffResult>) -> Self {
        let pointer = sniffed
            .and_then(|result| result.lfs_pointer.as_ref())
            .filter(|_| entry.sha256.is_none());
        match pointer {
            Some(pointer) => Self {
                abs_path: None,
                size: pointer.size,
                sha256: Some(pointer.sha256),
                lfs_pointer: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone().filter(|_| sniffed.is_some()),
                size: entry.size,
                sha256: entry.sha256,
                lfs_pointer: entry.lfs_pointer,
            },
        }
    }

    fn present(&self) -> bool {
        self.abs_path.is_some()
    }
}

struct PreparedEntry<'a> {
    discovered: &'a DiscoveredEntry,
    identity: String,
    sniffed: Option<SniffResult>,
    inference: Inference,
    presence: Presence,
    creates_artifact: bool,
    org: Option<String>,
    name: String,
    card: Option<String>,
}

/// Sniffs a whole revision in parallel, then prepares every file against the revision's own
/// documents, so a README or config beside the weights describes them the way it does in the
/// SQLite scanner.
fn prepare_revision<'a>(context: &EntryContext<'a>) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let sniffed: Vec<Option<SniffResult>> = entries.par_iter().map(sniff_entry).collect();
    let inference_context = InferenceContext::from_results(
        entries.iter().zip(&sniffed).filter_map(|(entry, result)| {
            result
                .as_ref()
                .map(|result| (entry.rel_path.as_str(), result))
        }),
    );
    let artifacts = sniffed
        .iter()
        .filter(|result| {
            result
                .as_ref()
                .is_some_and(|result| result.format.is_artifact())
        })
        .count();
    let naming = Naming {
        repo: &context.repo.repo,
        root_path: &context.repo.root_path,
        kind: context.kind,
        single: artifacts <= 1,
    };
    entries
        .par_iter()
        .zip(sniffed.into_par_iter())
        .map(|(entry, sniffed)| prepare_entry(entry, sniffed, &naming, &inference_context))
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
    naming: &Naming<'_>,
    context: &InferenceContext,
) -> Result<PreparedEntry<'a>> {
    let presence = Presence::of(entry, sniffed.as_ref());
    let format = sniffed
        .as_ref()
        .map_or(ArtifactFormat::Unknown, |result| result.format);
    let path = inference_path(naming.root_path, &entry.rel_path, entry.abs_path.as_deref());
    let file = file_name(&entry.rel_path);
    let mut input = InferInput::new(format, &path, file, presence.size);
    if let Some(sniffed) = &sniffed {
        input.tensors = sniffed.tensors.as_ref();
        input.config = sniffed
            .config
            .as_ref()
            .or_else(|| context.config_for(&entry.rel_path));
    }
    input.frontmatter = context.frontmatter();
    let (org, name) = artifact_name(naming, &entry.rel_path, None);
    let identity = identity(entry, &presence)?;
    Ok(PreparedEntry {
        discovered: entry,
        identity,
        inference: infer::infer(&input),
        creates_artifact: format.is_artifact(),
        name,
        org,
        card: context.card().map(str::to_owned),
        presence,
        sniffed,
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

fn store_entry(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: PreparedEntry<'_>,
    counters: &mut Counters,
) -> Result<()> {
    let entry_id = next_id(&mut counters.entries)?;
    let artifact = match prepared.creates_artifact {
        true => Some(store_artifact(transaction, &prepared, counters)?),
        false => None,
    };
    let state = entry_state(&prepared, artifact.as_ref(), context.triage);
    if let Some(artifact) = &artifact {
        transaction.insert(SearchEntry {
            entry_id,
            artifact_id: artifact.id,
            repo_id: context.repo_id,
            preference: preference(state, prepared.presence.present()),
            row: search_row(entry_id, context, &prepared, artifact, state),
            git_sha1: prepared.discovered.git_sha1.map(|sha| sha.to_string()),
        })?;
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
        artifact_id: artifact.as_ref().map(|artifact| artifact.id),
        state,
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
    })?;
    Ok(())
}

fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    let existing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, prepared.identity.as_str())?;
    let mut artifact = match existing {
        Some(existing) => existing,
        None => {
            let record = artifact_record(next_id(&mut counters.artifacts)?, prepared);
            transaction.insert(SearchText {
                artifact_id: record.id,
                keys: tensor_keys(prepared.sniffed.as_ref()),
                card: prepared.card.clone().unwrap_or_default(),
            })?;
            record
        }
    };
    artifact.path_count = artifact
        .path_count
        .checked_add(1)
        .ok_or_else(|| IndexError::Catalog("artifact path count overflow".into()))?;
    transaction.upsert(artifact.clone())?;
    Ok(artifact)
}

fn tensor_keys(sniffed: Option<&SniffResult>) -> String {
    sniffed
        .and_then(|result| result.tensors.as_ref())
        .map_or_else(String::new, |summary| {
            summary
                .key_prefixes
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
}

fn entry_state(
    prepared: &PreparedEntry<'_>,
    artifact: Option<&ArtifactRecord>,
    triage: f32,
) -> State {
    match (prepared.presence.present(), artifact) {
        (false, _) => State::Remote,
        (true, Some(artifact)) if artifact.artifact.needs_triage(triage) => State::Triage,
        (true, _) => State::Local,
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

fn artifact_record(id: i64, prepared: &PreparedEntry<'_>) -> ArtifactRecord {
    let inference = &prepared.inference;
    ArtifactRecord {
        id,
        identity: prepared.identity.clone(),
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
        tensors: prepared
            .sniffed
            .as_ref()
            .and_then(|sniffed| sniffed.tensors.clone()),
        evidence: inference.evidence.clone(),
    }
}

fn search_row(
    entry_id: i64,
    context: &EntryContext<'_>,
    prepared: &PreparedEntry<'_>,
    record: &ArtifactRecord,
    state: State,
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
        size: artifact.size,
        shard_count: 1,
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
        triage: artifact.needs_triage(context.triage),
        last_used: None,
        repo: context.repo.repo.qualified(),
        repo_id: RepoRowId::new(context.repo_id),
        entry_id: EntryId::new(entry_id),
        source: Some(context.source.to_owned()),
        is_symlink: entry.is_symlink,
        lfs_pointer: prepared.presence.lfs_pointer,
    }
}
