use native_db::{Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::{
    Artifact, Base, CommitSha, Evidence, GitSha1, RepoId, Role, Sha256, State, SubRole,
    TensorSummary,
};

use crate::config::{SourceConfig, SourceKind};
use crate::store::job::JobState;
use crate::store::revision::RefKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1)]
#[native_db]
pub(super) struct SourceV1 {
    #[primary_key]
    pub(super) name: String,
    pub(super) config: SourceConfig,
    pub(super) resolved_kind: SourceKind,
}

/// A nominated root, with what the last walk of it counted. A source no scan has reached
/// yet reports no last scan and zeroes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 2, from = SourceV1)]
#[native_db]
pub(super) struct Source {
    #[primary_key]
    pub(super) name: String,
    pub(super) config: SourceConfig,
    pub(super) resolved_kind: SourceKind,
    pub(super) file_count: u64,
    pub(super) byte_count: u64,
    pub(super) last_scan: Option<String>,
}

impl From<SourceV1> for Source {
    fn from(source: SourceV1) -> Self {
        Self {
            name: source.name,
            config: source.config,
            resolved_kind: source.resolved_kind,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
        }
    }
}

impl From<Source> for SourceV1 {
    fn from(source: Source) -> Self {
        Self {
            name: source.name,
            config: source.config,
            resolved_kind: source.resolved_kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 1)]
#[native_db]
pub(super) struct RepositoryV1 {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) key: String,
    pub(super) source: String,
    pub(super) repo: RepoId,
    pub(super) root_path: String,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<RevisionV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RevisionV1 {
    pub(super) name: String,
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 2, from = RepositoryV1)]
#[native_db]
pub(super) struct Repository {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) key: String,
    pub(super) source: String,
    pub(super) repo: RepoId,
    pub(super) root_path: String,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<Revision>,
}

/// One name a revision answers to, with what the name is: a moving branch or a fixed tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Ref {
    pub(super) name: String,
    pub(super) kind: RefKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision {
    pub(super) name: String,
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<Ref>,
}

impl From<RepositoryV1> for Repository {
    fn from(repository: RepositoryV1) -> Self {
        Self {
            id: repository.id,
            key: repository.key,
            source: repository.source,
            repo: repository.repo,
            root_path: repository.root_path,
            default_ref: repository.default_ref,
            revisions: repository
                .revisions
                .into_iter()
                .map(|revision| Revision {
                    name: revision.name,
                    sha: revision.sha,
                    refs: revision
                        .refs
                        .into_iter()
                        .map(|name| Ref {
                            name,
                            kind: RefKind::Branch,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<Repository> for RepositoryV1 {
    fn from(repository: Repository) -> Self {
        Self {
            id: repository.id,
            key: repository.key,
            source: repository.source,
            repo: repository.repo,
            root_path: repository.root_path,
            default_ref: repository.default_ref,
            revisions: repository
                .revisions
                .into_iter()
                .map(|revision| RevisionV1 {
                    name: revision.name,
                    sha: revision.sha,
                    refs: revision
                        .refs
                        .into_iter()
                        .map(|reference| reference.name)
                        .collect(),
                })
                .collect(),
        }
    }
}

/// The identity a repository keeps across scans: its source and its qualified name.
pub(super) fn repository_key(source: &str, repo: &RepoId) -> String {
    format!("{source}\0{}", repo.qualified())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 1)]
#[native_db]
pub(super) struct ArtifactRecordV1 {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) identity: u128,
    pub(super) artifact: Artifact,
    pub(super) name: String,
    pub(super) org: Option<String>,
    pub(super) path_count: u32,
    pub(super) group_bytes: Option<u64>,
    pub(super) shard_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 2, from = ArtifactRecordV1)]
#[native_db]
pub(super) struct ArtifactRecord {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) identity: u128,
    pub(super) artifact: Artifact,
    pub(super) name: String,
    pub(super) org: Option<String>,
    pub(super) path_count: u32,
    pub(super) group_bytes: Option<u64>,
    pub(super) shard_count: Option<u32>,
    pub(super) triage_cleared: bool,
    pub(super) first_seen: String,
}

impl From<ArtifactRecordV1> for ArtifactRecord {
    fn from(record: ArtifactRecordV1) -> Self {
        Self {
            id: record.id,
            identity: record.identity,
            artifact: record.artifact,
            name: record.name,
            org: record.org,
            path_count: record.path_count,
            group_bytes: record.group_bytes,
            shard_count: record.shard_count,
            triage_cleared: false,
            first_seen: String::new(),
        }
    }
}

impl From<ArtifactRecord> for ArtifactRecordV1 {
    fn from(record: ArtifactRecord) -> Self {
        Self {
            id: record.id,
            identity: record.identity,
            artifact: record.artifact,
            name: record.name,
            org: record.org,
            path_count: record.path_count,
            group_bytes: record.group_bytes,
            shard_count: record.shard_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1)]
#[native_db]
pub(super) struct EntryV1 {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    #[secondary_key(optional)]
    pub(super) artifact_id: Option<i64>,
    pub(super) revision: u32,
    pub(super) rel_path: String,
    pub(super) abs_path: Option<String>,
    pub(super) size: u64,
    pub(super) state: State,
    pub(super) triage: bool,
    pub(super) preference: i64,
    pub(super) lfs_pointer: bool,
    pub(super) is_symlink: bool,
    pub(super) git_sha1: Option<GitSha1>,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
}

/// The node an entry's bytes sit on. Every writer names this node until peers land.
pub(super) const LOCAL_NODE: i64 = 1;

/// One path, with the content hashes read from it and the node that holds it. `content` is
/// the identity a sha256 makes, so a lookup by content hash is a secondary key read rather
/// than a scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 2, from = EntryV1)]
#[native_db]
pub(super) struct Entry {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    #[secondary_key(optional)]
    pub(super) artifact_id: Option<i64>,
    pub(super) revision: u32,
    pub(super) rel_path: String,
    pub(super) abs_path: Option<String>,
    pub(super) size: u64,
    pub(super) state: State,
    pub(super) triage: bool,
    pub(super) preference: i64,
    pub(super) lfs_pointer: bool,
    pub(super) is_symlink: bool,
    pub(super) sha256: Option<Sha256>,
    #[secondary_key(optional)]
    pub(super) content: Option<u128>,
    pub(super) git_sha1: Option<GitSha1>,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
    #[secondary_key]
    pub(super) node: i64,
    pub(super) pass: u64,
    pub(super) pull_progress: Option<f32>,
}

impl From<EntryV1> for Entry {
    fn from(entry: EntryV1) -> Self {
        Self {
            id: entry.id,
            repo_id: entry.repo_id,
            artifact_id: entry.artifact_id,
            revision: entry.revision,
            rel_path: entry.rel_path,
            abs_path: entry.abs_path,
            size: entry.size,
            state: entry.state,
            triage: entry.triage,
            preference: entry.preference,
            lfs_pointer: entry.lfs_pointer,
            is_symlink: entry.is_symlink,
            sha256: None,
            content: None,
            git_sha1: entry.git_sha1,
            mtime: entry.mtime,
            inode: entry.inode,
            node: LOCAL_NODE,
            pass: 0,
            pull_progress: None,
        }
    }
}

impl From<Entry> for EntryV1 {
    fn from(entry: Entry) -> Self {
        Self {
            id: entry.id,
            repo_id: entry.repo_id,
            artifact_id: entry.artifact_id,
            revision: entry.revision,
            rel_path: entry.rel_path,
            abs_path: entry.abs_path,
            size: entry.size,
            state: entry.state,
            triage: entry.triage,
            preference: entry.preference,
            lfs_pointer: entry.lfs_pointer,
            is_symlink: entry.is_symlink,
            git_sha1: entry.git_sha1,
            mtime: entry.mtime,
            inode: entry.inode,
        }
    }
}

/// A user's verdict on what an artifact is. It outranks inference: the artifact record
/// carries what the verdict says, and a rescan re-applies it over freshly inferred facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 12, version = 1)]
#[native_db]
pub(super) struct Override {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) role: Option<Role>,
    pub(super) sub_role: Option<SubRole>,
    pub(super) base: Option<Base>,
    pub(super) decided_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 8, version = 1)]
#[native_db]
pub(super) struct ArtifactDetail {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 5, version = 1)]
#[native_db]
pub(super) struct SearchText {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) keys: String,
    pub(super) card: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 6, version = 1)]
#[native_db]
pub(super) struct ManifestV1 {
    #[primary_key]
    pub(super) id: u8,
    pub(super) format_version: u32,
    pub(super) generation: u64,
    pub(super) scanned_at: String,
    pub(super) node: String,
    pub(super) counts: Vec<(String, u64)>,
    pub(super) search_directory: Option<String>,
    pub(super) search_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 6, version = 2, from = ManifestV1)]
#[native_db]
pub(super) struct Manifest {
    #[primary_key]
    pub(super) id: u8,
    pub(super) format_version: u32,
    pub(super) generation: u64,
    pub(super) scanned_at: String,
    pub(super) node: String,
    pub(super) counts: Vec<(String, u64)>,
    pub(super) search_directory: Option<String>,
    pub(super) search_generation: u64,
    pub(super) search_seq: u64,
}

impl From<Manifest> for ManifestV1 {
    fn from(manifest: Manifest) -> Self {
        Self {
            id: manifest.id,
            format_version: manifest.format_version,
            generation: manifest.generation,
            scanned_at: manifest.scanned_at,
            node: manifest.node,
            counts: manifest.counts,
            search_directory: manifest.search_directory,
            search_generation: manifest.search_generation,
        }
    }
}

impl From<ManifestV1> for Manifest {
    fn from(manifest: ManifestV1) -> Self {
        Self {
            id: manifest.id,
            format_version: manifest.format_version,
            generation: manifest.generation,
            scanned_at: manifest.scanned_at,
            node: manifest.node,
            counts: manifest.counts,
            search_directory: manifest.search_directory,
            search_generation: manifest.search_generation,
            search_seq: 0,
        }
    }
}

/// One artifact whose documents the published search index no longer matches. The sequence
/// orders the log; a refresh replays every row after the manifest's cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 10, version = 1)]
#[native_db]
pub(super) struct SearchChange {
    #[primary_key]
    pub(super) seq: u64,
    pub(super) artifact_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 11, version = 1)]
#[native_db]
pub(super) struct SummaryV1 {
    #[primary_key]
    pub(super) id: u8,
    pub(super) reclaimable_bytes: u64,
    pub(super) duplicate_artifacts: u64,
}

/// What the artifact sweep last measured, so the facet rail and the stats route read one
/// record rather than scanning every artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 11, version = 2, from = SummaryV1)]
#[native_db]
pub(super) struct Summary {
    #[primary_key]
    pub(super) id: u8,
    pub(super) reclaimable_bytes: u64,
    pub(super) duplicate_artifacts: u64,
    pub(super) artifacts: u64,
    pub(super) present_artifacts: u64,
    pub(super) triage_artifacts: u64,
    pub(super) indexed_bytes: u64,
    pub(super) repositories: u64,
}

impl From<SummaryV1> for Summary {
    fn from(summary: SummaryV1) -> Self {
        Self {
            id: summary.id,
            reclaimable_bytes: summary.reclaimable_bytes,
            duplicate_artifacts: summary.duplicate_artifacts,
            artifacts: 0,
            present_artifacts: 0,
            triage_artifacts: 0,
            indexed_bytes: 0,
            repositories: 0,
        }
    }
}

impl From<Summary> for SummaryV1 {
    fn from(summary: Summary) -> Self {
        Self {
            id: summary.id,
            reclaimable_bytes: summary.reclaimable_bytes,
            duplicate_artifacts: summary.duplicate_artifacts,
        }
    }
}

/// The high-water marks a later scan continues from, so ids stay stable across runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 9, version = 1)]
#[native_db]
pub(super) struct Allocator {
    #[primary_key]
    pub(super) id: u8,
    pub(super) repos: u64,
    pub(super) artifacts: u64,
    pub(super) entries: u64,
}

/// One key of the shell's presentation state, as the settings sheet writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 13, version = 1)]
#[native_db]
pub(super) struct Setting {
    #[primary_key]
    pub(super) key: String,
    pub(super) value: String,
}

/// A saved query. The six shelves hoard ships with are seeded on every open and keep the
/// positions the design gives them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 14, version = 1)]
#[native_db]
pub(super) struct Shelf {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) name: String,
    pub(super) query: String,
    pub(super) builtin: bool,
    pub(super) position: i64,
}

/// A path a rejection marked as noise, which later scans walk past.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 15, version = 1)]
#[native_db]
pub(super) struct Noise {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) path: String,
    pub(super) reason: Option<String>,
    pub(super) added_at: String,
}

/// One symlink `hoard link` or `hoard apply` wrote into a tool's layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 16, version = 1)]
#[native_db]
pub(super) struct Projection {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) link_path: String,
    #[secondary_key]
    pub(super) profile: String,
    pub(super) target_path: String,
    pub(super) artifact_id: Option<i64>,
    pub(super) written_at: String,
}

/// One response this node served, which is what the USAGE tab reads back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 17, version = 1)]
#[native_db]
pub(super) struct Usage {
    #[primary_key]
    pub(super) seq: u64,
    #[secondary_key(optional)]
    pub(super) repo_id: Option<i64>,
    pub(super) at: String,
    pub(super) client: Option<String>,
    pub(super) artifact_id: Option<i64>,
    pub(super) path: String,
    pub(super) bytes: u64,
    pub(super) rate_bps: Option<u64>,
    pub(super) status: u16,
}

/// How often one artifact has been served and when it last was, which is what `used:` and
/// the last-used column read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 18, version = 1)]
#[native_db]
pub(super) struct ArtifactUse {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) count: u64,
    pub(super) last_used: String,
}

/// A saved loadout: the TOML document the drawer edits, keyed by the name inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 19, version = 1)]
#[native_db]
pub(super) struct LoadoutDoc {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) name: String,
    pub(super) document: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
}

/// A scan, hash run, pull or apply as the JOBS tab shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 20, version = 1)]
#[native_db]
pub(super) struct Job {
    #[primary_key]
    pub(super) id: i64,
    pub(super) kind: String,
    pub(super) state: JobState,
    pub(super) phase: Option<String>,
    pub(super) subject: Option<String>,
    pub(super) progress: f32,
    pub(super) bytes: u64,
    pub(super) total: Option<u64>,
    pub(super) rate_bps: Option<u64>,
    pub(super) error: Option<String>,
    pub(super) started: String,
    pub(super) finished: Option<String>,
}

/// Registers every record type. `native_db` compares each definition against every model
/// already registered rather than only against the other versions of the same model, so a
/// version 1 model must be defined last: it is what leaves each migrated model marked
/// current and each superseded one marked legacy.
pub(super) fn define_models() -> super::Result<Models> {
    let mut models = Models::new();
    models.define::<RepositoryV1>()?;
    models.define::<Repository>()?;
    models.define::<ArtifactRecordV1>()?;
    models.define::<ArtifactRecord>()?;
    models.define::<ManifestV1>()?;
    models.define::<Manifest>()?;
    models.define::<SummaryV1>()?;
    models.define::<Summary>()?;
    models.define::<SourceV1>()?;
    models.define::<Source>()?;
    models.define::<EntryV1>()?;
    models.define::<Entry>()?;
    models.define::<ArtifactDetail>()?;
    models.define::<SearchText>()?;
    models.define::<Allocator>()?;
    models.define::<Override>()?;
    models.define::<SearchChange>()?;
    models.define::<Setting>()?;
    models.define::<Shelf>()?;
    models.define::<Noise>()?;
    models.define::<Projection>()?;
    models.define::<Usage>()?;
    models.define::<ArtifactUse>()?;
    models.define::<LoadoutDoc>()?;
    models.define::<Job>()?;
    Ok(models)
}
