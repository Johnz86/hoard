use std::path::Path;

use native_db::{Models, ToKey, native_db};
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

use hoard_core::{
    Artifact, Base, CommitSha, Evidence, GitSha1, RepoId, Role, Sha256, State, SubRole,
    TensorSummary,
};

use crate::config::{SourceConfig, SourceKind};
use crate::discover::RefKind;

use super::jobs::JobState;

/// A nominated root, with what the last walk of it counted. A source no scan has reached
/// yet reports no last scan and zeroes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1)]
#[native_db]
pub(super) struct Source {
    #[primary_key]
    pub(super) name: String,
    pub(super) config: SourceConfig,
    pub(super) resolved_kind: SourceKind,
    pub(super) file_count: u64,
    pub(super) byte_count: u64,
    pub(super) last_scan: Option<String>,
    /// The present files under this root that carry no sha256 yet. Moved by the same
    /// writers that move a repository's present counts, through
    /// [`super::scan::DerivedShift`]; a writer added later must do the same.
    pub(super) pending_hashes: u64,
}

/// A repository as a scan found it, with the clone directory the git face serves from and
/// what each revision's commit says about itself.
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
    pub(super) git_dir: Option<String>,
    pub(super) default_ref: String,
    pub(super) revisions: Vec<Revision>,
    /// The entries of this repository whose bytes are on a node, and the bytes they hold.
    ///
    /// Every writer that changes an entry's presence, size or repository must move these
    /// through [`super::scan::DerivedShift`]; a writer added later must do the same, or
    /// the repositories listing and the Hub face will report counts that never settle.
    pub(super) present_files: u32,
    pub(super) present_bytes: u64,
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
    pub(super) title: Option<String>,
    pub(super) author: Option<String>,
    /// RFC 3339, the way every other catalog timestamp is stored.
    pub(super) committed: Option<String>,
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
    pub(super) triage_cleared: bool,
    pub(super) first_seen: String,
}

/// The node an entry's bytes sit on. Every writer names this node until peers land.
pub const LOCAL_NODE: i64 = 1;

/// Where the bytes of an entry sit relative to its repository root.
///
/// Almost every entry a scan writes sits at its repository's root joined with its own
/// relative path, and writing that whole absolute path out again is the largest redundant
/// field of the largest table. An hf-cache blob, an LFS object and a cache entry do not,
/// and carry the path they do sit at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) enum Placement {
    /// No node holds these bytes.
    Absent,
    /// The repository's root joined with the entry's relative path.
    UnderRoot,
    /// Somewhere else, named in full.
    At(String),
}

impl Placement {
    /// Where an absolute path sits, given the repository root and relative path it belongs
    /// to. A path equal to the two joined is recorded as sitting under the root.
    pub(super) fn of(abs_path: Option<&str>, root_path: &str, rel_path: &str) -> Self {
        match abs_path {
            None => Placement::Absent,
            Some(path) if Path::new(path) == Path::new(root_path).join(rel_path) => {
                Placement::UnderRoot
            }
            Some(path) => Placement::At(path.to_owned()),
        }
    }

    /// The absolute path these bytes sit at, rebuilt from the repository root when the row
    /// does not carry one.
    pub(super) fn resolve(&self, root_path: &str, rel_path: &str) -> Option<String> {
        match self {
            Placement::Absent => None,
            Placement::UnderRoot => Some(
                Path::new(root_path)
                    .join(rel_path)
                    .to_string_lossy()
                    .into_owned(),
            ),
            Placement::At(path) => Some(path.clone()),
        }
    }

    pub(super) fn is_absent(&self) -> bool {
        matches!(self, Placement::Absent)
    }
}

/// One path, with the content hashes read from it and the node that holds it.
///
/// `content` is the identity a sha256 makes, so a lookup by content hash is a secondary key
/// read rather than a scan; `location` names the one path a revision of a repository holds,
/// so finding it is one key read and reading a whole revision is one prefix scan. `node` is
/// not indexed: every reader that used to scan by it is scoped by source or repository
/// instead, and how many rows a node holds is on the node's own record.
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
    pub(super) node: i64,
    pub(super) pass: u64,
    pub(super) pull_progress: Option<f32>,
    /// [`location_key`] of this row's repository, revision and relative path. Every writer
    /// that changes one of those three must write it again.
    #[secondary_key(unique)]
    pub(super) location: String,
    /// Where the bytes sit. Every writer that moves them must write it again.
    pub(super) placed: Placement,
}

/// The one path a revision of a repository holds, as a key a lookup and a prefix scan can
/// both use. The two numbers are zero padded to a fixed width so a prefix of the first two
/// fields never reaches a longer number.
pub(super) fn location_key(repo_id: i64, revision: u32, rel_path: &str) -> String {
    let mut key = revision_prefix(repo_id, revision);
    key.push_str(rel_path);
    key
}

/// Everything in a [`location_key`] before the relative path, which is what every entry of
/// one revision shares.
pub(super) fn revision_prefix(repo_id: i64, revision: u32) -> String {
    format!("{repo_id:020}\u{1}{revision:010}\u{1}")
}

/// Where a writer's absolute path sits, read against the repository the row belongs to. A
/// repository the transaction cannot see yet leaves the path named in full.
pub(super) fn placement_in(
    transaction: &native_db::transaction::RwTransaction<'_>,
    repo_id: i64,
    abs_path: Option<&str>,
    rel_path: &str,
) -> super::Result<Placement> {
    let stored: Option<Repository> = transaction.get().primary(repo_id)?;
    Ok(match stored {
        Some(repository) => Placement::of(abs_path, &repository.root_path, rel_path),
        None => match abs_path {
            Some(path) => Placement::At(path.to_owned()),
            None => Placement::Absent,
        },
    })
}

/// The one entry a revision of a repository holds at `rel_path`, as one key read.
pub(super) fn entry_at(
    transaction: &native_db::transaction::RwTransaction<'_>,
    repo_id: i64,
    revision: u32,
    rel_path: &str,
) -> super::Result<Option<Entry>> {
    Ok(transaction.get().secondary(
        EntryKey::location,
        location_key(repo_id, revision, rel_path),
    )?)
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

/// What the artifact sweep last measured, so the facet rail and the stats route read one
/// record rather than scanning every artifact. `advertised_artifacts` is what a peer's
/// digest reports as its total: the hashed records this node holds a path to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 11, version = 1)]
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
    pub(super) advertised_artifacts: u64,
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

/// A machine this install knows about: this one, which is always [`LOCAL_NODE`], plus every
/// peer discovered or configured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 21, version = 1)]
#[native_db]
pub(super) struct Node {
    #[primary_key]
    pub(super) id: i64,
    #[secondary_key(unique)]
    pub(super) node_id: String,
    pub(super) name: String,
    pub(super) url: Option<String>,
    pub(super) is_self: bool,
    pub(super) online: bool,
    pub(super) indexed_bytes: u64,
    /// RFC 3339, like every other catalog timestamp.
    pub(super) last_seen: Option<String>,
    /// How far this node has read the peer's digest, as the peer's own cursor text.
    pub(super) cursor: Option<String>,
    pub(super) synced: Option<String>,
    /// The resync pass the peer's rows were last confirmed in; rows stamped with an older
    /// pass are what the peer stopped advertising.
    pub(super) pass: u64,
    /// The entries this catalog files against this node. Moved by the same writers that
    /// move a repository's present counts, through [`super::scan::DerivedShift`]; a writer
    /// added later must do the same.
    pub(super) filed: u64,
}

/// Registers every record type.
pub(super) fn define_models() -> super::Result<Models> {
    let mut models = Models::new();
    models.define::<Source>()?;
    models.define::<Repository>()?;
    models.define::<ArtifactRecord>()?;
    models.define::<SearchText>()?;
    models.define::<Manifest>()?;
    models.define::<Entry>()?;
    models.define::<ArtifactDetail>()?;
    models.define::<Allocator>()?;
    models.define::<SearchChange>()?;
    models.define::<Summary>()?;
    models.define::<Override>()?;
    models.define::<Setting>()?;
    models.define::<Shelf>()?;
    models.define::<Noise>()?;
    models.define::<Projection>()?;
    models.define::<Usage>()?;
    models.define::<ArtifactUse>()?;
    models.define::<LoadoutDoc>()?;
    models.define::<Job>()?;
    models.define::<Node>()?;
    Ok(models)
}
