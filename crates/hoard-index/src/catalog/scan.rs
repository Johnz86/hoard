use std::collections::HashSet;
use std::path::{Path, PathBuf};

use native_db::Builder;
use native_db::transaction::RwTransaction;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use hoard_core::{Artifact, ArtifactId, BlobId, EntryId, RepoRowId, State};

use crate::config::{IndexConfig, SourceConfig};
use crate::discover::{
    self, DiscoveredEntry, DiscoveredRepo, DiscoveredRevision, NoiseFilter, RevisionName,
};
use crate::infer::{self, InferInput, Inference};
use crate::sniff::{self, SniffResult};
use crate::store::SearchRow;

use super::models::{
    ArtifactRecord, ArtifactRecordKey, Manifest, Repository, Revision, SearchEntry, SearchText,
    Source,
};
use crate::error::{IndexError, Result};

use super::{Catalog, models};

const BATCH_SIZE: usize = 128;

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
            let transaction = self.database.rw_transaction()?;
            transaction.insert(Source {
                name: source.name.clone(),
                config: (*source).clone(),
                resolved_kind: discovered.kind(),
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
                    };
                    for chunk in revision.entries.chunks(BATCH_SIZE) {
                        let prepared = chunk
                            .par_iter()
                            .map(prepare_entry)
                            .collect::<Result<Vec<_>>>()?;
                        let transaction = self.database.rw_transaction()?;
                        for prepared in prepared {
                            store_entry(&transaction, &context, prepared, &mut counters)?;
                        }
                        transaction.commit()?;
                        progress(&source.name, counters.entries);
                    }
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

struct PreparedEntry<'a> {
    discovered: &'a DiscoveredEntry,
    identity: String,
    sniffed: Option<SniffResult>,
    inference: Inference,
}

fn prepare_entry(entry: &DiscoveredEntry) -> Result<PreparedEntry<'_>> {
    let path = entry
        .abs_path
        .as_deref()
        .unwrap_or_else(|| Path::new(&entry.rel_path));
    let sniffed = entry
        .abs_path
        .as_deref()
        .map(sniff::sniff_path)
        .transpose()?;
    let format = sniffed
        .as_ref()
        .map_or(hoard_core::ArtifactFormat::Unknown, |result| result.format);
    let mut input = InferInput::new(format, path, &entry.rel_path, entry.size);
    if let Some(sniffed) = &sniffed {
        input.tensors = sniffed.tensors.as_ref();
        input.config = sniffed.config.as_ref();
        input.frontmatter = sniffed
            .readme
            .as_ref()
            .map(|readme| readme.frontmatter.as_slice());
    }
    let identity = match (entry.sha256, entry.git_sha1, entry.abs_path.as_deref()) {
        (Some(sha), _, _) => format!("sha256:{sha}"),
        (None, Some(sha), _) => format!("git:{sha}"),
        (None, None, Some(path)) => format!("path:{}", path.canonicalize()?.display()),
        (None, None, None) => {
            return Err(IndexError::Catalog(format!(
                "absent entry has no content identity: {}",
                entry.rel_path
            )));
        }
    };
    Ok(PreparedEntry {
        discovered: entry,
        identity,
        inference: infer::infer(&input),
        sniffed,
    })
}

struct EntryContext<'a> {
    node: &'a str,
    source: &'a str,
    repo: &'a DiscoveredRepo,
    repo_id: i64,
    revision: &'a DiscoveredRevision,
    revision_name: String,
    triage: f32,
}

fn store_entry(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: PreparedEntry<'_>,
    counters: &mut Counters,
) -> Result<()> {
    let existing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, prepared.identity.as_str())?;
    let mut artifact = match existing {
        Some(existing) => existing,
        None => artifact_record(next_id(&mut counters.artifacts)?, &prepared),
    };
    artifact.path_count = artifact
        .path_count
        .checked_add(1)
        .ok_or_else(|| IndexError::Catalog("artifact path count overflow".into()))?;
    if let Some(sniffed) = &prepared.sniffed {
        let text = SearchText {
            artifact_id: artifact.id,
            keys: sniffed
                .tensors
                .as_ref()
                .map_or_else(String::new, |summary| {
                    summary
                        .key_prefixes
                        .iter()
                        .map(|(key, _)| key.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                }),
            card: sniffed
                .readme
                .as_ref()
                .map_or_else(String::new, |readme| readme.body.clone()),
        };
        transaction.upsert(text)?;
    }
    let entry_id = next_id(&mut counters.entries)?;
    let row = search_row(entry_id, context, &prepared, &artifact);
    transaction.insert(SearchEntry {
        entry_id,
        artifact_id: artifact.id,
        repo_id: context.repo_id,
        preference: if prepared.discovered.present() { 15 } else { 9 },
        row,
        git_sha1: prepared.discovered.git_sha1.map(|sha| sha.to_string()),
        repo_org: context.repo.repo.org().map(str::to_owned),
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
    })?;
    transaction.upsert(artifact)?;
    Ok(())
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
            sha256: prepared.discovered.sha256,
            size: prepared.discovered.size,
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
        org: context.repo.repo.org().map(str::to_owned),
        name: Path::new(&entry.rel_path).file_name().map_or_else(
            || entry.rel_path.clone(),
            |name| name.to_string_lossy().into_owned(),
        ),
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
        state: if entry.present() {
            State::Local
        } else {
            State::Remote
        },
        pull_progress: None,
        trigger: artifact.trigger.clone(),
        path: entry.rel_path.clone(),
        abs_path: entry
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
        lfs_pointer: entry.lfs_pointer,
    }
}
