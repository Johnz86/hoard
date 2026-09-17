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
    /// True when the set this artifact stands for holds fewer files than it declares.
    /// `shard_count` then counts the files present, which can be one.
    #[serde(default)]
    pub incomplete: bool,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub node: String,
    /// The folder discovery filed the entry's repository under: a ComfyUI or A1111 layout
    /// folder, or `hub repo`, `git clone` or `(unplaced)`.
    pub layout: String,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub trigger: Option<String>,
    pub path: String,
    /// Where the bytes sit. Every reader that opens the file, links to it or hands it to a
    /// loadout uses this; for an hf-cache entry it is the blob, not the snapshot link.
    pub abs_path: Option<String>,
    /// The path a person walked to the file, when that is not [`SearchRow::abs_path`].
    /// Screens show this; nothing opens it.
    #[serde(default)]
    pub walked_path: Option<String>,
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

impl SearchRow {
    /// Whether the artifact is a shard set, whole or not: more than one file present, or
    /// fewer present than declared. A set is pulled by its globs; a single file by its path.
    pub fn is_set(&self) -> bool {
        self.shard_count > 1 || self.incomplete
    }
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
    /// where it holds several the key is the only honest label for the group. A brand row
    /// restates its key here rather than disagreeing with it, so the rail names a brand the
    /// way its members spell it.
    pub label: Option<String>,
    pub count: u64,
    pub active: bool,
    /// The exact base values this row stands for.
    ///
    /// Empty on a bucket the rail lists on its own; on a brand row, every value the rail would
    /// otherwise have shown separately, in the order they would have appeared.
    pub members: Vec<String>,
}

/// The DISK PRESSURE summary under the facet rail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskPressure {
    pub reclaimable_bytes: u64,
    pub duplicate_blobs: u64,
}
