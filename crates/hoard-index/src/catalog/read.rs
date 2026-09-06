use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;

use native_db::transaction::RTransaction;
use serde::Serialize;

use hoard_core::{
    ArtifactId, CommitSha, EntryId, Evidence, RepoId, RepoRowId, RepoType, Role, Sha256, State,
    TensorSummary,
};

use crate::config::{IndexConfig, SourceKind};
use crate::discover::RefKind;
use crate::error::{IndexError, Result};
use crate::row::SearchRow;
use crate::search::{SortKey, SortSpec};
use crate::time::parse_opt_timestamp;
use crate::triage::{TriageHeader, header_facts};

use super::models::{
    ArtifactDetail, ArtifactRecord, Entry, EntryKey, Repository, RepositoryKey, Revision, Source,
    location_key, repository_key, revision_prefix,
};
use super::nodes::{NodeNames, PEER_SOURCE};
use super::search::{EntryFacts, assemble_row, revision_of};
use super::usage::last_used_in;
use super::{Catalog, QueryRequest};

/// One path an artifact occupies, with the repository and revision it sits in resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryDetail {
    pub entry_id: EntryId,
    pub repo: String,
    pub repo_id: RepoRowId,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub rel_path: String,
    pub abs_path: Option<String>,
    pub node: String,
    pub size: u64,
    pub state: State,
    pub present: bool,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
    pub artifact_id: Option<ArtifactId>,
    pub deduped: bool,
    pub sha256: Option<Sha256>,
    pub git_sha1: Option<CommitSha>,
    pub pull_progress: Option<f32>,
}

/// What only the inspector reads: the header the sniffer measured, the evidence behind the
/// verdict, and when the catalog first recorded the artifact.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactDetailView {
    pub tensors: Option<TensorSummary>,
    pub evidence: Vec<Evidence>,
    pub first_seen: Option<jiff::Timestamp>,
    pub triage_cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefDetail {
    pub name: String,
    pub kind: RefKind,
    pub sha: Option<CommitSha>,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RevisionDetail {
    pub ordinal: u32,
    pub name: String,
    pub sha: Option<CommitSha>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub committed: Option<jiff::Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepositoryDetail {
    pub id: RepoRowId,
    pub repo: RepoId,
    pub source: String,
    pub kind: SourceKind,
    pub root_path: String,
    pub git_dir: Option<String>,
    pub default_ref: String,
    pub node: String,
    pub revisions: Vec<RevisionDetail>,
    pub refs: Vec<RefDetail>,
    /// The files of this repository whose bytes are on a node, and the bytes they hold, as
    /// the writers that changed them left the counts.
    pub present_files: u32,
    pub present_bytes: u64,
}

/// What narrows the repositories listing: a case-insensitive substring of the qualified
/// name, a namespace, and whether a repository must hold at least one file on this disk.
#[derive(Debug, Clone, Default)]
pub struct RepositoryFilter {
    pub text: Option<String>,
    pub repo_type: Option<RepoType>,
    pub present_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepositoryPage {
    pub rows: Vec<RepositoryDetail>,
    pub total: u64,
}

/// A nominated root and what the last scan of it counted. The counts are of the files the
/// scan found on this disk; a path it no longer reaches is not counted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceRecord {
    pub name: String,
    pub path: PathBuf,
    pub kind: SourceKind,
    pub file_count: u64,
    pub byte_count: u64,
    pub last_scan: Option<jiff::Timestamp>,
    pub pending_hashes: u64,
}

impl SourceRecord {
    fn of(stored: Source) -> Result<Self> {
        Ok(Self {
            name: stored.name,
            path: stored.config.path,
            kind: stored.resolved_kind,
            file_count: stored.file_count,
            byte_count: stored.byte_count,
            last_scan: parse_opt_timestamp(stored.last_scan)?,
            pending_hashes: stored.pending_hashes,
        })
    }
}

/// One row of the triage queue: the artifact as the table shows it, the guess inference
/// recorded, and the header that produced the guess.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageItem {
    pub row: SearchRow,
    pub guess_role: Role,
    pub guess_base: Option<String>,
    pub confidence: f32,
    pub header: TriageHeader,
}

impl Catalog {
    /// Every nominated root this catalog holds, ordered by name, with the files each still
    /// owes a content hash. A peer's namespace is not a root on this node, so it is left out.
    pub fn sources(&self) -> Result<Vec<SourceRecord>> {
        let read = self.database.r_transaction()?;
        let rows: Vec<Source> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        rows.into_iter()
            .filter(|row| !row.name.starts_with(PEER_SOURCE))
            .map(SourceRecord::of)
            .collect()
    }

    /// The artifact's preferred entry as a row: the highest availability preference, and on
    /// a tie the lowest entry id, which is what the search index also collapses to.
    pub fn artifact_row(&self, id: ArtifactId) -> Result<Option<SearchRow>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let Some(record) = reader.record(id.get())? else {
            return Ok(None);
        };
        reader.row(&record)
    }

    /// The artifact one content hash names, through the grammar so a hash a caller typed is
    /// matched exactly the way the search bar matches it.
    pub fn artifact_by_sha256(&self, sha: &Sha256) -> Result<Option<SearchRow>> {
        let raw = format!("hash:{}", sha.to_hex());
        let context = self.compile_context(&raw, jiff::Timestamp::now(), Vec::new())?;
        let page = self.query(QueryRequest {
            raw: &raw,
            sort: SortSpec::new(SortKey::default(), false),
            limit: 1,
            offset: 0,
            context: &context,
        })?;
        Ok(page.rows.into_iter().next())
    }

    /// Every path of the artifact one content hash names, as rows.
    pub(super) fn artifact_rows_by_sha256(&self, sha: &Sha256) -> Result<Vec<SearchRow>> {
        let Some(found) = self.artifact_by_sha256(sha)? else {
            return Ok(Vec::new());
        };
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let id = found.artifact_id.get();
        let Some(record) = reader.record(id)? else {
            return Ok(Vec::new());
        };
        let last_used = last_used_in(&snapshot, id)?;
        let entries = reader.entries_of_artifact(id)?;
        let mut rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            rows.push(reader.row_of(&record, entry, last_used)?);
        }
        Ok(rows)
    }

    /// The rows one repository holds at one relative path. A file that is not an artifact
    /// has no row and is skipped.
    pub(super) fn rows_by_repo_path(&self, repo: &RepoId, path: &str) -> Result<Vec<SearchRow>> {
        let Some(repository) = self.repository(repo)? else {
            return Ok(Vec::new());
        };
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let mut entries = Vec::with_capacity(repository.revisions.len());
        for revision in &repository.revisions {
            if let Some(entry) = reader.entry_at(repository.id.get(), revision.ordinal, path)? {
                entries.push(entry);
            }
        }
        let mut rows = Vec::new();
        for entry in &entries {
            let Some(artifact) = entry.artifact_id else {
                continue;
            };
            let Some(record) = reader.record(artifact)? else {
                continue;
            };
            let last_used = last_used_in(&snapshot, artifact)?;
            rows.push(reader.row_of(&record, entry, last_used)?);
        }
        Ok(rows)
    }

    pub fn artifact_detail(&self, id: ArtifactId) -> Result<Option<ArtifactDetailView>> {
        let snapshot = self.database.r_transaction()?;
        let Some(record) = snapshot.get().primary::<ArtifactRecord>(id.get())? else {
            return Ok(None);
        };
        let detail: Option<ArtifactDetail> = snapshot.get().primary(id.get())?;
        let (tensors, evidence) = detail.map_or_else(
            || (None, Vec::new()),
            |detail| (detail.tensors, detail.evidence),
        );
        Ok(Some(ArtifactDetailView {
            tensors,
            evidence,
            first_seen: record.first_seen.parse().ok(),
            triage_cleared: record.triage_cleared,
        }))
    }

    /// Every path the artifact occupies, in entry order.
    pub fn entries_of(&self, id: ArtifactId) -> Result<Vec<EntryDetail>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let Some(record) = reader.record(id.get())? else {
            return Ok(Vec::new());
        };
        let entries = reader.entries_of_artifact(id.get())?;
        let deduped = record.path_count > 1;
        let mut details = Vec::with_capacity(entries.len());
        for entry in &entries {
            details.push(reader.detail(entry, deduped)?);
        }
        Ok(details)
    }

    /// The repository one qualified name points at, preferring the source whose copy of it
    /// holds files on this disk.
    pub fn repository(&self, repo: &RepoId) -> Result<Option<RepositoryDetail>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let sources: Vec<Source> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut best: Option<(bool, RepositoryDetail)> = None;
        for source in &sources {
            let key = repository_key(&source.name, repo);
            let stored: Option<Repository> =
                snapshot.get().secondary(RepositoryKey::key, key.as_str())?;
            let Some(stored) = stored else {
                continue;
            };
            let present = stored.present_files > 0;
            let detail = repository_detail(&stored, source.resolved_kind, &names);
            if present {
                return Ok(Some(detail));
            }
            if best.is_none() {
                best = Some((present, detail));
            }
        }
        Ok(best.map(|(_, detail)| detail))
    }

    /// One page of the repositories listing, ordered by identity so a cursor names the last
    /// row rather than an offset. `total` counts every repository the filter admits.
    pub fn repositories(
        &self,
        filter: &RepositoryFilter,
        after: Option<RepoRowId>,
        limit: u32,
    ) -> Result<RepositoryPage> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let sources: Vec<Source> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let kinds: HashMap<&str, SourceKind> = sources
            .iter()
            .map(|source| (source.name.as_str(), source.resolved_kind))
            .collect();
        let mut repositories: Vec<Repository> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        repositories.sort_unstable_by_key(|repository| repository.id);
        let text = filter.text.as_deref().map(str::to_lowercase);
        let mut total = 0;
        let mut rows = Vec::new();
        for repository in &repositories {
            if !admits(repository, filter, text.as_deref()) {
                continue;
            }
            if filter.present_only && repository.present_files == 0 {
                continue;
            }
            total += 1;
            if after.is_some_and(|after| repository.id <= after.get()) {
                continue;
            }
            if rows.len() < limit as usize {
                let kind = kinds
                    .get(repository.source.as_str())
                    .copied()
                    .unwrap_or(SourceKind::Loose);
                rows.push(repository_detail(repository, kind, &names));
            }
        }
        Ok(RepositoryPage { rows, total })
    }

    /// The revision `wanted` names: a ref name, a sha or its prefix, or the default when
    /// `wanted` is empty or `main`; `None` when nothing matches.
    pub fn resolve_revision(
        &self,
        repository: &RepositoryDetail,
        wanted: &str,
    ) -> Option<RevisionDetail> {
        let wanted = wanted.trim();
        let by_ref = |name: &str| {
            repository
                .refs
                .iter()
                .find(|reference| reference.name == name)
                .and_then(|reference| {
                    repository
                        .revisions
                        .iter()
                        .find(|revision| revision.ordinal == reference.revision)
                })
        };
        if let Some(revision) = by_ref(wanted) {
            return Some(revision.clone());
        }
        if !wanted.is_empty()
            && let Some(revision) = repository.revisions.iter().find(|revision| {
                revision
                    .sha
                    .is_some_and(|sha| sha.to_string().starts_with(wanted))
            })
        {
            return Some(revision.clone());
        }
        if !(wanted.is_empty() || wanted == "main" || wanted == repository.default_ref) {
            return None;
        }
        by_ref(&repository.default_ref)
            .or(match repository.revisions.as_slice() {
                [only] => Some(only),
                _ => None,
            })
            .cloned()
    }

    /// Every file one revision of a repository holds.
    pub fn revision_entries(&self, repo: RepoRowId, revision: u32) -> Result<Vec<EntryDetail>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let entries = reader.entries_of_revision(repo.get(), revision)?;
        let mut details = Vec::with_capacity(entries.len());
        for entry in &entries {
            let deduped = match entry.artifact_id {
                Some(id) => reader
                    .record(id)?
                    .is_some_and(|record| record.path_count > 1),
                None => false,
            };
            details.push(reader.detail(entry, deduped)?);
        }
        Ok(details)
    }

    /// The one file a revision holds at `rel_path`, or nothing when it holds none.
    pub fn revision_entry(
        &self,
        repo: RepoRowId,
        revision: u32,
        rel_path: &str,
    ) -> Result<Option<EntryDetail>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let Some(entry) = reader.entry_at(repo.get(), revision, rel_path)? else {
            return Ok(None);
        };
        let deduped = match entry.artifact_id {
            Some(id) => reader
                .record(id)?
                .is_some_and(|record| record.path_count > 1),
            None => false,
        };
        reader.detail(&entry, deduped).map(Some)
    }

    /// Every artifact reachable through one repository, most bytes first.
    pub fn repository_rows(&self, repo: RepoRowId, limit: u32) -> Result<Vec<SearchRow>> {
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let mut artifacts: Vec<i64> = reader
            .entries_of_repo(repo.get())?
            .into_iter()
            .filter_map(|entry| entry.artifact_id)
            .collect();
        artifacts.sort_unstable();
        artifacts.dedup();
        let mut rows = Vec::with_capacity(artifacts.len());
        for id in artifacts {
            if let Some(record) = reader.record(id)?
                && let Some(row) = reader.row(&record)?
            {
                rows.push(row);
            }
        }
        rows.sort_by_key(|row| (Reverse(row.size), row.artifact_id.get()));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    /// Every artifact still waiting for a verdict, most recently numbered last.
    ///
    /// The confidence a queued artifact is measured against is [`IndexConfig::default`]'s;
    /// the manifest does not yet carry the threshold the scan actually ran with.
    pub fn triage_queue(&self, limit: u32) -> Result<Vec<TriageItem>> {
        let threshold = IndexConfig::default().triage_confidence;
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let records: Vec<ArtifactRecord> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut items = Vec::new();
        for record in &records {
            if items.len() >= limit as usize {
                break;
            }
            if record.triage_cleared || !record.artifact.needs_triage(threshold) {
                continue;
            }
            let Some(row) = reader.row(record)? else {
                continue;
            };
            let detail: Option<ArtifactDetail> = snapshot.get().primary(record.id)?;
            items.push(triage_item(row, detail));
        }
        Ok(items)
    }
}

/// The queue's guess: the strongest evidence recorded for a field, falling back to what the
/// artifact itself carries, with the confidence of whichever answer produced the guess.
fn triage_item(row: SearchRow, detail: Option<ArtifactDetail>) -> TriageItem {
    let (tensors, evidence) = detail.map_or_else(
        || (None, Vec::new()),
        |detail| (detail.tensors, detail.evidence),
    );
    let (guess_role, confidence) = match strongest(&evidence, "role") {
        Some(found) => (
            Role::from_alias(&found.value).unwrap_or(row.role),
            found.confidence,
        ),
        None => (row.role, row.role_confidence),
    };
    let guess_base = strongest(&evidence, "base")
        .map(|found| found.value.clone())
        .or_else(|| {
            row.base_label
                .clone()
                .or_else(|| (!row.base.is_empty()).then(|| row.base.to_string()))
        });
    TriageItem {
        row,
        guess_role,
        guess_base,
        confidence,
        header: tensors
            .map(|tensors| header_facts(&tensors))
            .unwrap_or_default(),
    }
}

fn strongest<'a>(evidence: &'a [Evidence], field: &str) -> Option<&'a Evidence> {
    evidence
        .iter()
        .filter(|item| item.field == field)
        .reduce(|best, next| match next.confidence > best.confidence {
            true => next,
            false => best,
        })
}

fn admits(repository: &Repository, filter: &RepositoryFilter, text: Option<&str>) -> bool {
    if filter
        .repo_type
        .is_some_and(|repo_type| repository.repo.repo_type() != repo_type)
    {
        return false;
    }
    match text {
        Some(text) => repository.repo.qualified().to_lowercase().contains(text),
        None => true,
    }
}

fn repository_detail(
    repository: &Repository,
    kind: SourceKind,
    names: &NodeNames,
) -> RepositoryDetail {
    let revisions: Vec<RevisionDetail> = repository
        .revisions
        .iter()
        .enumerate()
        .map(|(ordinal, revision)| RevisionDetail {
            ordinal: ordinal as u32,
            name: revision.name.clone(),
            sha: revision.sha,
            title: revision.title.clone(),
            author: revision.author.clone(),
            committed: revision
                .committed
                .as_deref()
                .and_then(|stamp| stamp.parse().ok()),
        })
        .collect();
    let refs = repository
        .revisions
        .iter()
        .enumerate()
        .flat_map(|(ordinal, revision)| {
            revision.refs.iter().map(move |reference| RefDetail {
                name: reference.name.clone(),
                kind: reference.kind,
                sha: revision.sha,
                revision: ordinal as u32,
            })
        })
        .collect();
    RepositoryDetail {
        id: RepoRowId::new(repository.id),
        repo: repository.repo.clone(),
        source: repository.source.clone(),
        kind,
        root_path: repository.root_path.clone(),
        git_dir: repository.git_dir.clone(),
        default_ref: repository.default_ref.clone(),
        node: names.of_source(&repository.source).to_owned(),
        revisions,
        refs,
        present_files: repository.present_files,
        present_bytes: repository.present_bytes,
    }
}

/// One read transaction with the repositories it has already resolved, so a screen that
/// walks many entries of one repository reads that record once.
pub(super) struct Reader<'a, 'db> {
    snapshot: &'a RTransaction<'db>,
    names: &'a NodeNames,
    repositories: HashMap<i64, Repository>,
}

impl<'a, 'db> Reader<'a, 'db> {
    pub(super) fn new(snapshot: &'a RTransaction<'db>, names: &'a NodeNames) -> Self {
        Self {
            snapshot,
            names,
            repositories: HashMap::new(),
        }
    }

    pub(super) fn record(&self, id: i64) -> Result<Option<ArtifactRecord>> {
        Ok(self.snapshot.get().primary(id)?)
    }

    pub(super) fn entries_of_artifact(&self, id: i64) -> Result<Vec<Entry>> {
        Ok(self
            .snapshot
            .scan()
            .secondary::<Entry>(EntryKey::artifact_id)?
            .start_with(Some(id))?
            .collect::<native_db::db_type::Result<_>>()?)
    }

    fn entries_of_repo(&self, repo: i64) -> Result<Vec<Entry>> {
        Ok(self
            .snapshot
            .scan()
            .secondary::<Entry>(EntryKey::repo_id)?
            .start_with(repo)?
            .collect::<native_db::db_type::Result<_>>()?)
    }

    /// Every file one revision holds, in path order, as one prefix scan of the location key.
    pub(super) fn entries_of_revision(&self, repo: i64, revision: u32) -> Result<Vec<Entry>> {
        Ok(self
            .snapshot
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(revision_prefix(repo, revision).as_str())?
            .collect::<native_db::db_type::Result<_>>()?)
    }

    /// The one file a revision holds at `rel_path`, as one key read.
    pub(super) fn entry_at(
        &self,
        repo: i64,
        revision: u32,
        rel_path: &str,
    ) -> Result<Option<Entry>> {
        Ok(self
            .snapshot
            .get()
            .secondary(EntryKey::location, location_key(repo, revision, rel_path))?)
    }

    fn repository(&mut self, id: i64) -> Result<&Repository> {
        if !self.repositories.contains_key(&id) {
            let stored: Repository = self.snapshot.get().primary(id)?.ok_or_else(|| {
                IndexError::Catalog(format!("an entry references missing repository {id}"))
            })?;
            self.repositories.insert(id, stored);
        }
        self.repositories
            .get(&id)
            .ok_or_else(|| IndexError::Catalog("resolution cache went empty".into()))
    }

    fn row(&mut self, record: &ArtifactRecord) -> Result<Option<SearchRow>> {
        let Some(entry) = self
            .entries_of_artifact(record.id)?
            .into_iter()
            .max_by_key(|entry| (entry.preference, Reverse(entry.id)))
        else {
            return Ok(None);
        };
        let last_used = last_used_in(self.snapshot, record.id)?;
        Ok(Some(self.row_of(record, &entry, last_used)?))
    }

    pub(super) fn row_of(
        &mut self,
        record: &ArtifactRecord,
        entry: &Entry,
        last_used: Option<jiff::Timestamp>,
    ) -> Result<SearchRow> {
        let names = self.names;
        let node = names.of(entry.node);
        let repository = self.repository(entry.repo_id)?;
        let revision = revision_of(repository, entry)?;
        Ok(assemble_row(&EntryFacts {
            entry,
            repository,
            revision,
            artifact: record,
            node,
            primary: true,
            last_used,
        }))
    }

    pub(super) fn detail(&mut self, entry: &Entry, deduped: bool) -> Result<EntryDetail> {
        let names = self.names;
        let node = names.of(entry.node);
        let repository = self.repository(entry.repo_id)?;
        let revision = revision_of(repository, entry)?;
        Ok(entry_detail(entry, repository, revision, node, deduped))
    }
}

fn entry_detail(
    entry: &Entry,
    repository: &Repository,
    revision: &Revision,
    node: &str,
    deduped: bool,
) -> EntryDetail {
    EntryDetail {
        entry_id: EntryId::new(entry.id),
        repo: repository.repo.qualified(),
        repo_id: RepoRowId::new(repository.id),
        revision: revision.name.clone(),
        revision_sha: revision.sha,
        rel_path: entry.rel_path.clone(),
        abs_path: entry.abs_path.clone(),
        node: node.to_owned(),
        size: entry.size,
        state: entry.state,
        present: entry.state != State::Remote,
        is_symlink: entry.is_symlink,
        lfs_pointer: entry.lfs_pointer,
        artifact_id: entry.artifact_id.map(ArtifactId::new),
        deduped,
        sha256: entry.sha256,
        git_sha1: entry.git_sha1,
        pull_progress: entry.pull_progress,
    }
}
