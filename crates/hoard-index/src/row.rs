//! The shapes a search answers with, independent of what store produced them.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, EntryId, Precision, RepoRowId, Role,
    Sha256, State, SubRole,
};

/// One artifact seen through one of its entries. This is the shape the table, the cards,
/// the inspector and `hoard ls --json` all render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRow {
    pub artifact_id: ArtifactId,
    pub blob_id: BlobId,
    pub blob_sha256: Option<Sha256>,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub org: Option<String>,
    pub name: String,
    pub base: Base,
    pub base_label: Option<String>,
    pub format: ArtifactFormat,
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub size: u64,
    /// How many files the artifact is split across; one for a model that is a single file.
    #[serde(default = "single_shard")]
    pub shard_count: u32,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub node: String,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub trigger: Option<String>,
    pub path: String,
    pub abs_path: Option<String>,
    pub path_count: u32,
    pub dupe: bool,
    pub triage: bool,
    pub last_used: Option<Timestamp>,
    pub repo: String,
    pub repo_id: RepoRowId,
    pub entry_id: EntryId,
    pub source: Option<String>,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
}

fn single_shard() -> u32 {
    1
}

/// One facet bucket: a value, how many artifacts carry it, and whether the query selects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetBucket {
    pub value: String,
    /// The spelling to show, when every artifact in the bucket agrees on one.
    ///
    /// `value` is the normalized matching key, so a base family reads `nemotronh` while the
    /// architecture is written `nemotron_h`. Where a bucket holds one spelling it is shown;
    /// where it holds several the key is the only honest label for the group.
    pub label: Option<String>,
    pub count: u64,
    pub active: bool,
}

/// The DISK PRESSURE summary under the facet rail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskPressure {
    pub reclaimable_bytes: u64,
    pub duplicate_blobs: u64,
}
