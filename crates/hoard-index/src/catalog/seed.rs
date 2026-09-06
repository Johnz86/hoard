use std::collections::HashMap;
use std::path::PathBuf;

use native_db::transaction::RwTransaction;

use hoard_core::{
    Artifact, ArtifactId, BlobId, CommitSha, Evidence, RepoId, Sha256, State, TensorSummary,
};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::discover::RefKind;
use crate::error::{IndexError, Result};

use super::Catalog;
use super::hashing::content_key;
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, Manifest, Ref, Repository,
    RepositoryKey, Revision, SearchText, Source, location_key, placement_in, repository_key,
};
use super::scan::{Counters, counts, log_changes, next_id, preference, tensor_keys};

/// The instant every seeded artifact reports as first seen, so a test that renders it reads
/// one fixed value rather than the wall clock.
const SEEDED_AT: &str = "2026-09-04T09:10:00Z";

/// One repository a seeded catalog holds, under a source named by [`SeedRepository::source`].
#[derive(Debug, Clone)]
pub struct SeedRepository {
    pub repo: RepoId,
    pub source: String,
    pub kind: SourceKind,
    pub root_path: PathBuf,
    pub git_dir: Option<PathBuf>,
    pub revisions: Vec<SeedRevision>,
}

#[derive(Debug, Clone)]
pub struct SeedRevision {
    pub name: String,
    pub sha: Option<CommitSha>,
    pub refs: Vec<(String, RefKind)>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub committed: Option<jiff::Timestamp>,
    pub entries: Vec<SeedEntry>,
}

#[derive(Debug, Clone)]
pub struct SeedEntry {
    pub rel_path: String,
    /// Which node holds the bytes: [`super::models::LOCAL_NODE`] for this one.
    pub node: i64,
    pub abs_path: Option<String>,
    pub size: u64,
    pub state: State,
    pub is_symlink: bool,
    pub sha256: Option<Sha256>,
    pub git_sha1: Option<CommitSha>,
    pub artifact: Option<SeedArtifact>,
}

#[derive(Debug, Clone)]
pub struct SeedArtifact {
    /// Entries seeded with the same identity share one artifact and count as its paths.
    pub identity: u128,
    pub artifact: Artifact,
    pub name: String,
    pub org: Option<String>,
    pub tensors: Option<TensorSummary>,
    pub evidence: Vec<Evidence>,
    pub card: Option<String>,
    pub group_bytes: Option<u64>,
    pub shard_count: Option<u32>,
}

impl Catalog {
    /// Writes records the way a scan would, for tests that need known rows without
    /// inference: ids come from the allocator, every artifact is logged for the search
    /// index, path counts and the summary are swept, and the generation increments.
    /// Call `rebuild_search` afterwards. Returns the identity of each artifact written and
    /// the id it was given, in the order the seed first named them.
    pub fn seed(&self, repositories: &[SeedRepository]) -> Result<Vec<(u128, ArtifactId)>> {
        let triage = IndexConfig::default().triage_confidence;
        let mut counters = Counters::load(&self.database, triage)?;
        let transaction = self.database.rw_transaction()?;
        let written = {
            let mut seeding = Seeding {
                transaction: &transaction,
                counters: &mut counters,
                by_identity: HashMap::new(),
                written: Vec::new(),
                triage,
            };
            for repository in repositories {
                seeding.repository(repository)?;
            }
            seeding.written
        };
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
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
        Ok(written)
    }
}

/// One `seed` call in progress: the transaction every record lands in, the allocator that
/// numbers them, and the artifact ids the identities seen so far were given.
struct Seeding<'a, 'db> {
    transaction: &'a RwTransaction<'db>,
    counters: &'a mut Counters,
    by_identity: HashMap<u128, i64>,
    written: Vec<(u128, ArtifactId)>,
    triage: f32,
}

impl Seeding<'_, '_> {
    fn repository(&mut self, seed: &SeedRepository) -> Result<()> {
        self.source(seed)?;
        let key = repository_key(&seed.source, &seed.repo);
        let stored: Option<Repository> = self
            .transaction
            .get()
            .secondary(RepositoryKey::key, key.as_str())?;
        let (repo_id, present) = match stored {
            Some(stored) => (stored.id, (stored.present_files, stored.present_bytes)),
            None => (self.counters.next_repo()?, (0, 0)),
        };
        self.transaction.upsert(Repository {
            id: repo_id,
            key,
            source: seed.source.clone(),
            repo: seed.repo.clone(),
            root_path: seed.root_path.to_string_lossy().into_owned(),
            git_dir: seed
                .git_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            default_ref: seed
                .revisions
                .first()
                .map_or("main", |revision| revision.name.as_str())
                .to_owned(),
            revisions: seed
                .revisions
                .iter()
                .map(|revision| Revision {
                    name: revision.name.clone(),
                    sha: revision.sha,
                    title: revision.title.clone(),
                    author: revision.author.clone(),
                    committed: revision.committed.map(|stamp| stamp.to_string()),
                    refs: revision
                        .refs
                        .iter()
                        .map(|(name, kind)| Ref {
                            name: name.clone(),
                            kind: *kind,
                        })
                        .collect(),
                })
                .collect(),
            present_files: present.0,
            present_bytes: present.1,
        })?;
        for (ordinal, revision) in seed.revisions.iter().enumerate() {
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))?;
            for entry in &revision.entries {
                self.entry(repo_id, ordinal, entry)?;
            }
        }
        Ok(())
    }

    /// The source this repository sits under, with the files and bytes it seeds counted
    /// into it the way a walk would.
    fn source(&mut self, seed: &SeedRepository) -> Result<()> {
        let stored: Option<Source> = self.transaction.get().primary(seed.source.clone())?;
        let source = stored.unwrap_or_else(|| Source {
            name: seed.source.clone(),
            config: SourceConfig {
                name: seed.source.clone(),
                path: seed.root_path.clone(),
                kind: seed.kind,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            },
            resolved_kind: seed.kind,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        });
        let present = seed
            .revisions
            .iter()
            .flat_map(|revision| revision.entries.iter())
            .filter(|entry| entry.state != State::Remote);
        let mut file_count = source.file_count;
        let mut byte_count = source.byte_count;
        for entry in present {
            file_count += 1;
            byte_count = byte_count.saturating_add(entry.size);
        }
        self.transaction.upsert(Source {
            file_count,
            byte_count,
            last_scan: Some(SEEDED_AT.to_owned()),
            ..source
        })?;
        Ok(())
    }

    fn entry(&mut self, repo_id: i64, revision: u32, seed: &SeedEntry) -> Result<()> {
        let artifact_id = match &seed.artifact {
            Some(artifact) => Some(self.artifact(artifact)?),
            None => None,
        };
        let triage = seed.state == State::Triage
            || seed
                .artifact
                .as_ref()
                .is_some_and(|seed| seed.artifact.needs_triage(self.triage));
        let entry = Entry {
            id: next_id(&mut self.counters.entries)?,
            repo_id,
            artifact_id,
            revision,
            location: location_key(repo_id, revision, &seed.rel_path),
            rel_path: seed.rel_path.clone(),
            placed: placement_in(
                self.transaction,
                repo_id,
                seed.abs_path.as_deref(),
                &seed.rel_path,
            )?,
            size: seed.size,
            state: seed.state,
            triage,
            preference: preference(seed.state, seed.abs_path.is_some()),
            lfs_pointer: false,
            is_symlink: seed.is_symlink,
            sha256: seed.sha256,
            content: seed.sha256.as_ref().map(content_key),
            git_sha1: seed.git_sha1,
            mtime: None,
            inode: None,
            node: seed.node,
            pass: 0,
            pull_progress: None,
        };
        self.counters.derived.added(&entry);
        self.transaction.insert(entry)?;
        Ok(())
    }

    /// The record this entry contributes a path to, written once per identity.
    fn artifact(&mut self, seed: &SeedArtifact) -> Result<i64> {
        if let Some(id) = self.by_identity.get(&seed.identity).copied() {
            self.touch(id)?;
            return Ok(id);
        }
        let stored: Option<ArtifactRecord> = self
            .transaction
            .get()
            .secondary(ArtifactRecordKey::identity, seed.identity)?;
        let id = match stored {
            Some(stored) => stored.id,
            None => next_id(&mut self.counters.artifacts)?,
        };
        self.touch(id)?;
        self.transaction.upsert(SearchText {
            artifact_id: id,
            keys: tensor_keys(seed.tensors.as_ref()),
            card: seed.card.clone().unwrap_or_default(),
        })?;
        self.transaction.upsert(ArtifactDetail {
            artifact_id: id,
            tensors: seed.tensors.clone(),
            evidence: seed.evidence.clone(),
        })?;
        self.transaction.upsert(ArtifactRecord {
            id,
            identity: seed.identity,
            artifact: Artifact {
                id: ArtifactId::new(id),
                blob: BlobId::new(id),
                ..seed.artifact.clone()
            },
            name: seed.name.clone(),
            org: seed.org.clone(),
            path_count: 1,
            group_bytes: seed.group_bytes,
            shard_count: seed.shard_count,
            triage_cleared: false,
            first_seen: SEEDED_AT.to_owned(),
        })?;
        self.by_identity.insert(seed.identity, id);
        self.written.push((seed.identity, ArtifactId::new(id)));
        Ok(id)
    }

    fn touch(&mut self, id: i64) -> Result<()> {
        self.counters.touch(self.transaction, id)?;
        self.counters.pending.push(id);
        Ok(())
    }
}
