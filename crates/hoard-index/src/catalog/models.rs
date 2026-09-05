use native_db::{Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::{Artifact, CommitSha, Evidence, RepoId, State, TensorSummary};

use crate::config::{SourceConfig, SourceKind};
use crate::store::SearchRow;

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
    #[secondary_key]
    pub(super) source: String,
    pub(super) repo: RepoId,
    pub(super) root_path: String,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<Revision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Revision {
    pub(super) sha: Option<CommitSha>,
    pub(super) refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 1)]
#[native_db]
pub(super) struct ArtifactRecord {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) identity: String,
    pub(super) artifact: Artifact,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) evidence: Vec<Evidence>,
    pub(super) path_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 4, version = 1)]
#[native_db]
pub(super) struct SearchEntry {
    #[primary_key]
    pub(super) entry_id: i64,
    #[secondary_key]
    pub(super) artifact_id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    pub(super) row: SearchRow,
    pub(super) preference: i64,
    pub(super) git_sha1: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1)]
#[native_db]
pub(super) struct Entry {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key]
    pub(super) repo_id: i64,
    pub(super) revision: u32,
    pub(super) rel_path: String,
    pub(super) abs_path: Option<String>,
    pub(super) size: u64,
    pub(super) artifact_id: Option<i64>,
    pub(super) state: State,
    pub(super) lfs_pointer: bool,
    pub(super) is_symlink: bool,
    pub(super) mtime: Option<i64>,
    pub(super) inode: Option<u64>,
}

pub(super) fn define_models() -> super::Result<Models> {
    let mut models = Models::new();
    models.define::<Source>()?;
    models.define::<Repository>()?;
    models.define::<ArtifactRecord>()?;
    models.define::<SearchEntry>()?;
    models.define::<SearchText>()?;
    models.define::<Manifest>()?;
    models.define::<Entry>()?;
    Ok(models)
}
