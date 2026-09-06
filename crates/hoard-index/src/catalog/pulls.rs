use std::path::Path;

use native_db::transaction::RwTransaction;

use hoard_core::{ArtifactId, CommitSha, EntryId, GitSha1, RepoId, RepoRowId, Sha256, State};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::error::{IndexError, Result};
use crate::infer::Inference;
use crate::sniff::SniffResult;

use super::Catalog;
use super::hashing::content_key;
use super::ingest::IngestRecord;
use super::models::{
    ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE, Repository, RepositoryKey,
    Revision, SearchText, Source, Summary, repository_key,
};
use super::scan::{
    Counters, RecordFacts, log_changes, next_id, preference, record_of, tensor_keys,
};

/// The source every file pulled from a peer is filed under.
pub const PULL_SOURCE: &str = "pulls";

/// Where a pull is landing, as the catalog needs to file it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullSlot {
    pub repo: RepoRowId,
    pub revision: u32,
}

/// What a running or abandoned transfer records against the path it is moving into.
#[derive(Debug, Clone, PartialEq)]
pub struct PullMark<'a> {
    pub slot: PullSlot,
    pub rel_path: &'a str,
    pub abs_path: &'a Path,
    pub size: u64,
    pub state: State,
    pub progress: Option<f32>,
}

/// One verified pulled file with everything the scanner would have read from it.
#[derive(Debug, Clone)]
pub struct PulledFile<'a> {
    pub slot: PullSlot,
    pub rel_path: &'a str,
    pub abs_path: &'a Path,
    pub sha256: Sha256,
    pub git_sha1: Option<GitSha1>,
    pub sniff: &'a SniffResult,
    pub inference: &'a Inference,
    /// Whether this node serves what it pulled, which is SERVED rather than LOCAL.
    pub served: bool,
    /// Whether the classification was too weak to stand without a verdict.
    pub triage: bool,
}

impl Catalog {
    /// The repository and revision a pull of `repo` at `revision` lands in, created under
    /// the pull source when absent.
    pub fn pull_slot(&self, repo: &RepoId, revision: CommitSha, root: &Path) -> Result<PullSlot> {
        let mut counters = Counters::load(&self.database)?;
        let transaction = self.database.rw_transaction()?;
        pull_source(&transaction, root)?;
        let repo_id = pull_repository(&transaction, &mut counters, repo, root)?;
        let ordinal = pull_revision(&transaction, repo_id, revision)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        Ok(PullSlot {
            repo: RepoRowId::new(repo_id),
            revision: ordinal,
        })
    }

    /// Writes the entry a running or failed pull shows: its state, its progress and where
    /// the bytes are landing. The hashes and the record the path already carries are left
    /// alone, because nothing has verified the new bytes yet.
    pub fn mark_pull(&self, mark: &PullMark<'_>) -> Result<EntryId> {
        let mut counters = Counters::load(&self.database)?;
        let transaction = self.database.rw_transaction()?;
        let previous = entry_at(&transaction, mark.slot, mark.rel_path)?;
        let id = match &previous {
            Some(entry) => entry.id,
            None => next_id(&mut counters.entries)?,
        };
        if let Some(artifact) = previous.as_ref().and_then(|entry| entry.artifact_id) {
            counters.pending.push(artifact);
        }
        if let Some(previous) = &previous {
            counters.derived.removed(previous);
        }
        let entry = Entry {
            id,
            repo_id: mark.slot.repo.get(),
            revision: mark.slot.revision,
            rel_path: mark.rel_path.to_owned(),
            abs_path: Some(mark.abs_path.to_string_lossy().into_owned()),
            size: mark.size,
            state: mark.state,
            preference: preference(mark.state, mark.state != State::Remote),
            pull_progress: mark.progress,
            node: LOCAL_NODE,
            ..blank_entry(previous)
        };
        counters.derived.added(&entry);
        transaction.upsert(entry)?;
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        Ok(EntryId::new(id))
    }

    /// Files a verified pull as a present entry with its hashes and a classified record,
    /// sharing the record of any copy the catalog already holds by content.
    pub fn file_pulled(&self, file: &PulledFile<'_>) -> Result<IngestRecord> {
        let threshold = IndexConfig::default().triage_confidence;
        let mut counters = Counters::load(&self.database)?;
        let transaction = self.database.rw_transaction()?;
        let identity = content_key(&file.sha256);
        let (artifact_id, created) = filed_artifact(&transaction, &mut counters, file, identity)?;
        counters.pending.push(artifact_id);
        let previous = entry_at(&transaction, file.slot, file.rel_path)?;
        let id = match &previous {
            Some(entry) => entry.id,
            None => next_id(&mut counters.entries)?,
        };
        let state = pulled_state(file);
        if let Some(previous) = &previous {
            counters.derived.removed(previous);
        }
        let entry = Entry {
            id,
            repo_id: file.slot.repo.get(),
            revision: file.slot.revision,
            rel_path: file.rel_path.to_owned(),
            abs_path: Some(file.abs_path.to_string_lossy().into_owned()),
            size: file.sniff.size,
            state,
            triage: file.triage,
            preference: preference(state, true),
            sha256: Some(file.sha256),
            content: Some(identity),
            git_sha1: file.git_sha1,
            artifact_id: Some(artifact_id),
            pull_progress: None,
            node: LOCAL_NODE,
            ..blank_entry(previous)
        };
        counters.derived.added(&entry);
        transaction.upsert(entry)?;
        if created {
            count_artifact(
                &transaction,
                file.sniff.size,
                file.inference.role_confidence < threshold,
            )?;
        }
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        Ok(IngestRecord {
            entry: EntryId::new(id),
            artifact: Some(ArtifactId::new(artifact_id)),
        })
    }
}

/// A pulled file reads the way a scanned one does, minus the two facts a pull settles on
/// its own: the transfer is over, and nothing here pins a commit that could have moved.
fn pulled_state(file: &PulledFile<'_>) -> State {
    match (file.triage, file.served) {
        (true, _) => State::Triage,
        (false, true) => State::Served,
        (false, false) => State::Local,
    }
}

/// The fields a rewritten entry keeps from the row already at that path, so a pull that
/// lands twice does not lose what an earlier one recorded.
fn blank_entry(previous: Option<Entry>) -> Entry {
    previous.unwrap_or(Entry {
        id: 0,
        repo_id: 0,
        artifact_id: None,
        revision: 0,
        rel_path: String::new(),
        abs_path: None,
        size: 0,
        state: State::Remote,
        triage: false,
        preference: 0,
        lfs_pointer: false,
        is_symlink: false,
        sha256: None,
        content: None,
        git_sha1: None,
        mtime: None,
        inode: None,
        node: LOCAL_NODE,
        pass: 0,
        pull_progress: None,
    })
}

/// The root pulled bytes land in, nominated so the repositories under it read back with a
/// kind. A scan never walks it: the configuration does not name it.
fn pull_source(transaction: &RwTransaction<'_>, root: &Path) -> Result<()> {
    let stored: Option<Source> = transaction.get().primary(PULL_SOURCE.to_owned())?;
    if stored.is_some() {
        return Ok(());
    }
    transaction.insert(Source {
        name: PULL_SOURCE.to_owned(),
        config: SourceConfig {
            name: PULL_SOURCE.to_owned(),
            path: root.to_path_buf(),
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        },
        resolved_kind: SourceKind::Loose,
        file_count: 0,
        byte_count: 0,
        last_scan: None,
        pending_hashes: 0,
    })?;
    Ok(())
}

fn pull_repository(
    transaction: &RwTransaction<'_>,
    counters: &mut Counters,
    repo: &RepoId,
    root: &Path,
) -> Result<i64> {
    let key = repository_key(PULL_SOURCE, repo);
    let stored: Option<Repository> = transaction
        .get()
        .secondary(RepositoryKey::key, key.as_str())?;
    if let Some(stored) = stored {
        return Ok(stored.id);
    }
    let id = counters.next_repo()?;
    transaction.insert(Repository {
        id,
        key,
        source: PULL_SOURCE.to_owned(),
        repo: repo.clone(),
        root_path: root.to_string_lossy().into_owned(),
        git_dir: None,
        default_ref: crate::discover::DEFAULT_REF.to_owned(),
        revisions: Vec::new(),
        present_files: 0,
        present_bytes: 0,
    })?;
    Ok(id)
}

fn pull_revision(
    transaction: &RwTransaction<'_>,
    repo_id: i64,
    revision: CommitSha,
) -> Result<u32> {
    let stored: Repository = transaction
        .get()
        .primary(repo_id)?
        .ok_or_else(|| IndexError::Catalog(format!("repository {repo_id} vanished")))?;
    if let Some(ordinal) = stored
        .revisions
        .iter()
        .position(|held| held.sha == Some(revision))
    {
        return ordinal_of(ordinal);
    }
    let ordinal = ordinal_of(stored.revisions.len())?;
    let mut revisions = stored.revisions.clone();
    revisions.push(Revision {
        name: revision.short(),
        sha: Some(revision),
        refs: Vec::new(),
        title: None,
        author: None,
        committed: None,
    });
    transaction.upsert(Repository {
        revisions,
        ..stored
    })?;
    Ok(ordinal)
}

/// The record the pulled bytes belong to, and whether this call wrote it.
fn filed_artifact(
    transaction: &RwTransaction<'_>,
    counters: &mut Counters,
    file: &PulledFile<'_>,
    identity: u128,
) -> Result<(i64, bool)> {
    let stored: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, identity)?;
    if let Some(stored) = stored {
        let path_count = stored.path_count.saturating_add(1);
        transaction.upsert(ArtifactRecord {
            path_count,
            ..stored
        })?;
        return Ok((stored.id, false));
    }
    let id = next_id(&mut counters.artifacts)?;
    let name = file
        .abs_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file.rel_path);
    let (record, detail) = record_of(RecordFacts {
        id,
        identity,
        name: name.to_owned(),
        org: repo_of(transaction, file.slot)?.and_then(|repo| repo.org().map(str::to_owned)),
        sha256: Some(file.sha256),
        size: file.sniff.size,
        format: file.sniff.format,
        inference: file.inference,
        tensors: file.sniff.tensors.clone(),
        group_bytes: None,
        shard_count: None,
    });
    transaction.upsert(SearchText {
        artifact_id: id,
        keys: tensor_keys(file.sniff.tensors.as_ref()),
        card: file
            .sniff
            .readme
            .as_ref()
            .map(|readme| readme.body.clone())
            .unwrap_or_default(),
    })?;
    transaction.upsert(detail)?;
    transaction.upsert(record)?;
    Ok((id, true))
}

fn repo_of(transaction: &RwTransaction<'_>, slot: PullSlot) -> Result<Option<RepoId>> {
    let stored: Option<Repository> = transaction.get().primary(slot.repo.get())?;
    Ok(stored.map(|stored| stored.repo))
}

/// What the stats screen prints, moved by the one record this call wrote rather than
/// recounted: a pull runs once per fetched file.
fn count_artifact(transaction: &RwTransaction<'_>, size: u64, triage: bool) -> Result<()> {
    let stored: Option<Summary> = transaction.get().primary(0_u8)?;
    let mut summary = stored.unwrap_or(Summary {
        id: 0,
        reclaimable_bytes: 0,
        duplicate_artifacts: 0,
        artifacts: 0,
        present_artifacts: 0,
        triage_artifacts: 0,
        indexed_bytes: 0,
        repositories: 0,
        advertised_artifacts: 0,
    });
    summary.artifacts += 1;
    summary.present_artifacts += 1;
    summary.advertised_artifacts += 1;
    summary.indexed_bytes = summary.indexed_bytes.saturating_add(size);
    if triage {
        summary.triage_artifacts += 1;
    }
    summary.repositories = transaction.len().primary::<Repository>()?;
    transaction.upsert(summary)?;
    Ok(())
}

fn entry_at(
    transaction: &RwTransaction<'_>,
    slot: PullSlot,
    rel_path: &str,
) -> Result<Option<Entry>> {
    let scan = transaction.scan();
    let by_repo = scan.secondary::<Entry>(EntryKey::repo_id)?;
    let mut of_repo = by_repo.start_with(slot.repo.get())?;
    of_repo
        .find(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry.repo_id == slot.repo.get()
                    && entry.revision == slot.revision
                    && entry.rel_path == rel_path
            })
        })
        .transpose()
        .map_err(IndexError::from)
}

fn ordinal_of(position: usize) -> Result<u32> {
    u32::try_from(position).map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))
}
