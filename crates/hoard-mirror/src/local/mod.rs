//! Repository views backed by this node: a git clone on disk and the index-synthesized
//! form used for HF cache snapshots and loose folders.

pub mod git_repo;
pub mod index_repo;

pub use git_repo::{
    BlobRef, BlobSource, CommitInfo, EntryKind, GitRef, GitRepo, LfsObject, RefKind, TreeEntry,
};
