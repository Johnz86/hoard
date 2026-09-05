use native_db::{Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::{Artifact, CommitSha, Evidence, GitSha1, RepoId, State, TensorSummary};

use crate::config::{SourceConfig, SourceKind};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1)]
#[native_db]
pub(super) struct Source {
    #[primary_key]
    pub(super) name: String,
    pub(super) config: SourceConfig,
    pub(super) resolved_kind: SourceKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 1)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision {
    pub(super) name: String,
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<String>,
}

/// The identity a repository keeps across scans: its source and its qualified name.
pub(super) fn repository_key(source: &str, repo: &RepoId) -> String {
    format!("{source}\0{}", repo.qualified())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 1)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1)]
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
    pub(super) git_sha1: Option<GitSha1>,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
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

/// Registers every record type. `native_db` compares each definition against every model
/// already registered rather than only against the other versions of the same model, so a
/// version 1 model must be defined last: it is what leaves each migrated model marked
/// current and each superseded one marked legacy.
pub(super) fn define_models() -> super::Result<Models> {
    let mut models = Models::new();
    models.define::<Source>()?;
    models.define::<Repository>()?;
    models.define::<ArtifactRecord>()?;
    models.define::<ArtifactDetail>()?;
    models.define::<SearchText>()?;
    models.define::<ManifestV1>()?;
    models.define::<Manifest>()?;
    models.define::<SummaryV1>()?;
    models.define::<Summary>()?;
    models.define::<Entry>()?;
    models.define::<Allocator>()?;
    models.define::<SearchChange>()?;
    Ok(models)
}
