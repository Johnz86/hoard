use std::path::{Path, PathBuf};

use native_db::transaction::RwTransaction;

use hoard_core::{ArtifactFormat, ArtifactId, CommitSha, EntryId, RepoId, Sha256, State};

use crate::config::{SourceConfig, SourceKind};
use crate::error::{IndexError, Result};
use crate::infer::{InferInput, infer};

use super::hashing::content_key;
use super::models::{
    ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE, LocationKey, Placement,
    Repository, RepositoryKey, Revision, SearchText, Source, Summary, entry_at, repository_key,
};
use super::read::Reader;
use super::scan::{
    CHUNK, Counters, RecordFacts, log_changes, next_id, paths_of_artifact, preference,
    present_paths, record_of, tensor_keys,
};
use super::{Catalog, EntryDetail};

/// The source every file the mirror fetched from upstream is filed under.
pub const MIRROR_SOURCE: &str = "mirror";

/// One fetched file the block cache holds in full, with the hashes read from its blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedFile {
    pub repo: RepoId,
    pub commit: CommitSha,
    pub rel_path: String,
    pub size: u64,
    pub sha256: Sha256,
    pub git_sha1: Option<CommitSha>,
    /// The cache root, which is what the mirror source's path and the repository's root are.
    pub cache_root: PathBuf,
}

/// Which rows one fetched file landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestRecord {
    pub entry: EntryId,
    pub artifact: Option<ArtifactId>,
}

impl Catalog {
    /// Files one fetched file under the mirror source as a served entry: the repository and
    /// the revision named by the commit are created when absent, an entry already at that
    /// path in that revision is replaced, and a file whose name makes it an artifact gets a
    /// record classified from its name, size and hash, sharing the record of any copy the
    /// catalog already holds by content. The search index is refreshed by the caller.
    pub fn ingest_served(&self, file: &ServedFile) -> Result<IngestRecord> {
        let triage = self.stored_triage()?;
        let mut counters = Counters::load(&self.database, triage)?;
        let transaction = self.database.rw_transaction()?;
        let record = {
            let mut ingesting = Ingesting {
                transaction: &transaction,
                counters: &mut counters,
                triage,
            };
            ingesting.file(file)?
        };
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Visits every path this node serves whose bytes live only in the block cache - a
    /// SERVED entry of the local node with no file of its own on disk - in chunks of at
    /// most `chunk`, one repository at a time. The sweep reads the paths and not the
    /// artifacts behind them, so [`EntryDetail::deduped`] is not resolved.
    ///
    /// Each repository is read in its own snapshot, and each chunk is taken from the scan
    /// rather than from a list of the repository's rows, so a sweep of the mirror holds one
    /// chunk of details rather than every served path the catalog knows. The chunks of one
    /// repository arrive in the order the location key files them, and the repositories in
    /// repository id order.
    pub fn served_cache_entries_in(
        &self,
        chunk: usize,
        mut visit: impl FnMut(Vec<EntryDetail>) -> Result<()>,
    ) -> Result<()> {
        let chunk = chunk.max(1);
        for repo_id in self.repositories_of_source(MIRROR_SOURCE)? {
            let snapshot = self.database.r_transaction()?;
            let names = self.node_names(&snapshot)?;
            let mut reader = Reader::new(&snapshot, &names);
            let scan = snapshot.scan();
            let secondary = scan.secondary::<Entry>(EntryKey::location)?;
            let mut rows = secondary.start_with(LocationKey::repository(repo_id))?;
            loop {
                let mut details = Vec::with_capacity(chunk);
                for entry in rows.by_ref() {
                    let entry: Entry = entry?;
                    if entry.repo_id != repo_id
                        || entry.node != LOCAL_NODE
                        || entry.state != State::Served
                        || !entry.placed.is_absent()
                    {
                        continue;
                    }
                    details.push(reader.detail(&entry, false)?);
                    if details.len() == chunk {
                        break;
                    }
                }
                if details.is_empty() {
                    break;
                }
                visit(details)?;
            }
        }
        Ok(())
    }

    /// Moves entries back to REMOTE with no pull in flight, keeping the rows so the catalog
    /// still remembers the path, and logs each artifact behind them for the search index.
    /// Returns how many rows changed; the caller refreshes the search index.
    ///
    /// Each artifact a retired path belonged to is swept afterwards, so its path count and
    /// the totals count the paths that are still present rather than the ones that were.
    pub fn retire_entries(&self, ids: &[EntryId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let mut retired = 0;
        for id in ids {
            let stored: Option<Entry> = transaction.get().primary(id.get())?;
            let Some(stored) = stored.filter(|entry| entry.state != State::Remote) else {
                continue;
            };
            if let Some(artifact) = stored.artifact_id {
                counters.touch(&transaction, artifact)?;
                counters.pending.push(artifact);
            }
            counters.derived.removed(&stored);
            transaction.upsert(Entry {
                state: State::Remote,
                pull_progress: None,
                preference: preference(State::Remote, false),
                ..stored
            })?;
            retired += 1;
        }
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        self.recount_retired_paths(&mut counters)?;
        Ok(retired)
    }

    /// Recounts what each artifact a retirement touched still holds, and moves the totals by
    /// the difference. The record stays whatever the count reaches: a retired path is a path
    /// this catalog still remembers, which is the difference between evicting bytes and
    /// forgetting a file.
    fn recount_retired_paths(&self, counters: &mut Counters) -> Result<()> {
        let ids: Vec<i64> = counters.touched.iter().copied().collect();
        for batch in ids.chunks(CHUNK) {
            let transaction = self.database.rw_transaction()?;
            for id in batch {
                let stored: Option<ArtifactRecord> = transaction.get().primary(*id)?;
                let Some(record) = stored else {
                    continue;
                };
                let paths = present_paths(
                    &paths_of_artifact(&transaction, record.id)?,
                    record.shard_count,
                );
                if paths != record.path_count {
                    transaction.upsert(ArtifactRecord {
                        path_count: paths,
                        ..record
                    })?;
                }
            }
            transaction.commit()?;
        }
        self.summarise(counters)
    }
}

/// One `ingest_served` call in progress: the transaction every record lands in and the
/// allocator that numbers the rows it creates.
struct Ingesting<'a, 'db> {
    transaction: &'a RwTransaction<'db>,
    counters: &'a mut Counters,
    triage: f32,
}

/// What classifying one fetched file settled: which record it belongs to, whether that
/// record was written by this call, and whether it needs a verdict.
struct Classified {
    id: i64,
    created: bool,
    triage: bool,
}

impl Ingesting<'_, '_> {
    fn file(&mut self, file: &ServedFile) -> Result<IngestRecord> {
        self.source(file)?;
        let repo_id = self.repository(file)?;
        let ordinal = self.revision(repo_id, file.commit)?;
        let classified = self.artifact(file)?;
        let previous = entry_at(self.transaction, repo_id, ordinal, &file.rel_path)?;
        let previous_artifact = previous.as_ref().and_then(|entry| entry.artifact_id);
        let artifact_id = classified.as_ref().map(|found| found.id);
        if artifact_id != previous_artifact {
            if let Some(id) = previous_artifact {
                self.count_path(id, -1)?;
            }
            if let Some(found) = classified.as_ref().filter(|found| !found.created) {
                self.count_path(found.id, 1)?;
            }
        }
        let entry_id = match &previous {
            Some(entry) => entry.id,
            None => next_id(&mut self.counters.entries)?,
        };
        let triage = classified.as_ref().is_some_and(|found| found.triage);
        let entry = Entry {
            id: entry_id,
            repo_id,
            artifact_id,
            revision: ordinal,
            rel_path: file.rel_path.clone(),
            placed: Placement::Absent,
            size: file.size,
            state: State::Served,
            triage,
            preference: preference(State::Served, true),
            lfs_pointer: false,
            is_symlink: false,
            sha256: Some(file.sha256),
            content: Some(content_key(&file.sha256)),
            git_sha1: file.git_sha1,
            mtime: None,
            inode: None,
            node: LOCAL_NODE,
            pass: 0,
            pull_progress: None,
        };
        if let Some(previous) = &previous {
            self.counters.derived.removed(previous);
        }
        self.counters.derived.added(&entry);
        self.transaction.upsert(entry)?;
        if let Some(found) = classified.as_ref().filter(|found| found.created) {
            self.count_artifact(file.size, found.triage)?;
        }
        Ok(IngestRecord {
            entry: EntryId::new(entry_id),
            artifact: artifact_id.map(ArtifactId::new),
        })
    }

    /// The root the mirror fills, nominated so the repositories under it read back with a
    /// kind. A scan never walks it: the configuration does not name it.
    fn source(&mut self, file: &ServedFile) -> Result<()> {
        let stored: Option<Source> = self.transaction.get().primary(MIRROR_SOURCE.to_owned())?;
        if stored.is_some() {
            return Ok(());
        }
        self.transaction.insert(Source {
            name: MIRROR_SOURCE.to_owned(),
            config: SourceConfig {
                name: MIRROR_SOURCE.to_owned(),
                path: file.cache_root.clone(),
                kind: SourceKind::HfCache,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            },
            resolved_kind: SourceKind::HfCache,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        })?;
        Ok(())
    }

    fn repository(&mut self, file: &ServedFile) -> Result<i64> {
        let key = repository_key(MIRROR_SOURCE, &file.repo);
        let stored: Option<Repository> = self
            .transaction
            .get()
            .secondary(RepositoryKey::key, key.as_str())?;
        if let Some(stored) = stored {
            return Ok(stored.id);
        }
        let id = self.counters.next_repo()?;
        self.transaction.insert(Repository {
            id,
            key,
            source: MIRROR_SOURCE.to_owned(),
            repo: file.repo.clone(),
            root_path: file.cache_root.to_string_lossy().into_owned(),
            git_dir: None,
            default_ref: crate::discover::DEFAULT_REF.to_owned(),
            remote: None,
            revisions: Vec::new(),
            present_files: 0,
            present_bytes: 0,
        })?;
        Ok(id)
    }

    /// The ordinal of the revision this commit names, appended when the repository does not
    /// hold it yet.
    fn revision(&mut self, repo_id: i64, commit: CommitSha) -> Result<u32> {
        let stored: Repository = self
            .transaction
            .get()
            .primary(repo_id)?
            .ok_or_else(|| IndexError::Catalog(format!("repository {repo_id} vanished")))?;
        if let Some(ordinal) = stored
            .revisions
            .iter()
            .position(|revision| revision.sha == Some(commit))
        {
            return ordinal_of(ordinal);
        }
        let ordinal = ordinal_of(stored.revisions.len())?;
        let mut revisions = stored.revisions.clone();
        revisions.push(Revision {
            name: commit.to_string().chars().take(7).collect(),
            sha: Some(commit),
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
            retired: false,
        });
        self.transaction.upsert(Repository {
            revisions,
            ..stored
        })?;
        Ok(ordinal)
    }

    /// The record this file belongs to, or nothing when its name does not make it one.
    fn artifact(&mut self, file: &ServedFile) -> Result<Option<Classified>> {
        let path = Path::new(&file.rel_path);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&file.rel_path);
        let format = path
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| extension.parse::<ArtifactFormat>().ok())
            .unwrap_or(ArtifactFormat::Unknown);
        if !format.is_artifact() {
            return Ok(None);
        }
        let identity = content_key(&file.sha256);
        let stored: Option<ArtifactRecord> = self
            .transaction
            .get()
            .secondary(ArtifactRecordKey::identity, identity)?;
        if let Some(stored) = stored {
            self.touch(stored.id)?;
            return Ok(Some(Classified {
                id: stored.id,
                created: false,
                triage: stored.artifact.needs_triage(self.triage),
            }));
        }
        let id = next_id(&mut self.counters.artifacts)?;
        self.touch(id)?;
        let inference = infer(&InferInput::new(format, path, file_name, file.size));
        let (record, detail) = record_of(RecordFacts {
            id,
            identity,
            name: file_name.to_owned(),
            org: file.repo.org().map(str::to_owned),
            sha256: Some(file.sha256),
            size: file.size,
            format,
            inference: &inference,
            tensors: None,
            group_bytes: None,
            shard_count: None,
            first_seen: None,
        });
        let triage = record.artifact.needs_triage(self.triage);
        self.transaction.upsert(SearchText {
            artifact_id: id,
            keys: tensor_keys(None),
        })?;
        self.transaction.upsert(detail)?;
        self.transaction.upsert(record)?;
        Ok(Some(Classified {
            id,
            created: true,
            triage,
        }))
    }

    /// Moves one record's path count, which is what marks an artifact deduplicated and what
    /// the sweep would otherwise have to recount.
    fn count_path(&mut self, id: i64, by: i32) -> Result<()> {
        let Some(stored) = self.transaction.get().primary::<ArtifactRecord>(id)? else {
            return Ok(());
        };
        let path_count = match by.is_negative() {
            true => stored.path_count.saturating_sub(1),
            false => stored.path_count.saturating_add(1),
        };
        self.touch(id)?;
        self.transaction.upsert(ArtifactRecord {
            path_count,
            ..stored
        })?;
        Ok(())
    }

    /// What the stats screen prints, moved by the one record this call wrote rather than
    /// recounted: an ingest runs once per fetched file.
    fn count_artifact(&mut self, size: u64, triage: bool) -> Result<()> {
        let stored: Option<Summary> = self.transaction.get().primary(0_u8)?;
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
        summary.repositories = self.transaction.len().primary::<Repository>()?;
        self.transaction.upsert(summary)?;
        Ok(())
    }

    fn touch(&mut self, id: i64) -> Result<()> {
        self.counters.touch(self.transaction, id)?;
        self.counters.pending.push(id);
        Ok(())
    }
}

fn ordinal_of(position: usize) -> Result<u32> {
    u32::try_from(position).map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))
}
