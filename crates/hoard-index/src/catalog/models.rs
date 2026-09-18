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
use crate::shard::{covers, directory_of};

use super::jobs::JobState;

/// A nominated root, with what the last walk of it counted. A source no scan has reached
/// yet reports no last scan and zeroes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 1, version = 1, with = super::codec::Compact)]
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
    /// [`super::scan::counters::DerivedShift`]; a writer added later must do the same.
    pub(super) pending_hashes: u64,
    /// The bytes those files hold, so a hashing pass can report what it still owes rather
    /// than how many files are left. Moved by the same writers, through
    /// [`super::scan::counters::DerivedShift`].
    pub(super) pending_bytes: u64,
}

/// A repository as a scan found it, with the clone directory the git face serves from and
/// what each revision's commit says about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 2, version = 1, with = super::codec::Compact)]
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
    /// The URL a clone's `origin` names, for a repository discovery read one from.
    pub(super) remote: Option<String>,
    /// The tool whose models directory this repository is a category folder of, as loose
    /// discovery recognized it. Such a folder holds whatever a person put in it and is
    /// nobody's repository; everything else carries nothing here.
    pub(super) tool_layout: Option<String>,
    pub(super) revisions: Vec<Revision>,
    /// The entries of this repository whose bytes are on a node, and the bytes they hold.
    ///
    /// Every writer that changes an entry's presence, size or repository must move these
    /// through [`super::scan::counters::DerivedShift`]; a writer added later must do the same, or
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
    /// A slot no ref reaches and no entry names any more. An entry names its revision by
    /// ordinal, so the slot cannot be reclaimed; everything in it is given up instead, and
    /// nothing resolves to it.
    pub(super) retired: bool,
}

impl Revision {
    /// The slot a revision leaves behind: its ordinal, and nothing else.
    pub(super) fn tombstone() -> Self {
        Self {
            name: String::new(),
            sha: None,
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
            retired: true,
        }
    }
}

/// The identity a repository keeps across scans: its source and its qualified name.
pub(super) fn repository_key(source: &str, repo: &RepoId) -> String {
    format!("{}{}", source_prefix(source), repo.qualified())
}

/// What every repository of one source shares, ending in a byte no source or repository
/// name holds, so one source's repositories are a prefix of the identity key and no source
/// is a prefix of another.
pub(super) fn source_prefix(source: &str) -> String {
    format!("{source}\0")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 3, version = 1, with = super::codec::Compact)]
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
    /// How many distinct copies of the bytes those paths amount to. Two paths reaching one
    /// inode are one copy, so nothing is reclaimable by deleting either of them.
    pub(super) copies: u32,
    pub(super) group_bytes: Option<u64>,
    /// True when the set this artifact stands for found fewer files than it declares.
    pub(super) incomplete: bool,
    /// True when `name` is the name of the repository this naming was chosen in, decided
    /// when it was chosen. Comparing two namings has to read each one's own answer: the
    /// repository a later pass happens to be walking says nothing about either of them.
    pub(super) named_by_repo: bool,
    pub(super) shard_count: Option<u32>,
    pub(super) triage_cleared: bool,
    /// True once the owner rejected this artifact from triage: it wants nobody and counts in
    /// no triage total, whatever else is true of it.
    pub(super) rejected: bool,
    pub(super) first_seen: String,
}

impl ArtifactRecord {
    /// Whether this record still wants a person, for every reader of the queue, the entry
    /// state a scan writes and the summary count. A set that found fewer files than it
    /// declares cannot be served whatever its header says, so an incomplete set queues
    /// however confident its inference was; a rejection is the one answer that ends that,
    /// because it says the files are not wanted rather than that they are all here.
    pub(super) fn needs_triage(&self, threshold: f32) -> bool {
        !self.rejected && (self.artifact.needs_triage(threshold) || self.incomplete)
    }

    /// Whether the triage queue still holds this record: it wants a person, and nothing that
    /// could answer it has. A verdict answers what an artifact is and cannot make a set
    /// complete, so an incomplete set stays queued whatever was cleared on it; the missing
    /// files landing, a rejection, or a rescan dropping a path rejected as noise, is what
    /// takes it out. The summary count and the entry state a scan writes read the same
    /// predicate, so all three move together.
    pub(super) fn is_queued(&self, threshold: f32) -> bool {
        self.needs_triage(threshold) && (self.incomplete || !self.triage_cleared)
    }
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

/// The one path a revision of a repository holds, as a key a point lookup and a prefix scan
/// can both use.
///
/// The repository id and the revision ordinal are written big endian ahead of the relative
/// path's own bytes, so the first 8 bytes are exactly one repository and the first 12 are
/// exactly one revision. Ids come from the allocator and are positive, which is what makes
/// the big-endian order the numeric one. The path is not folded or canonicalized: two rows
/// spelling a path differently are two locations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LocationKey(Vec<u8>);

/// Everything a location key holds before the relative path.
const LOCATION_PREFIX: usize = size_of::<i64>() + size_of::<u32>();

impl LocationKey {
    /// One entry of one revision of one repository.
    pub(super) fn at(repo_id: i64, revision: u32, rel_path: &str) -> Self {
        let mut key = Vec::with_capacity(LOCATION_PREFIX + rel_path.len());
        key.extend_from_slice(&repo_id.to_be_bytes());
        key.extend_from_slice(&revision.to_be_bytes());
        key.extend_from_slice(rel_path.as_bytes());
        Self(key)
    }

    /// What every entry of one repository shares, whichever revision holds it.
    pub(super) fn repository(repo_id: i64) -> Self {
        Self(repo_id.to_be_bytes().to_vec())
    }

    /// What every entry of one revision shares.
    pub(super) fn revision(repo_id: i64, revision: u32) -> Self {
        let mut key = Vec::with_capacity(LOCATION_PREFIX);
        key.extend_from_slice(&repo_id.to_be_bytes());
        key.extend_from_slice(&revision.to_be_bytes());
        Self(key)
    }
}

impl ToKey for LocationKey {
    fn to_key(&self) -> native_db::db_type::Key {
        native_db::db_type::Key::new(self.0.clone())
    }

    fn key_names() -> Vec<String> {
        vec!["LocationKey".to_owned()]
    }
}

/// One path, with the content hashes read from it and the node that holds it.
///
/// `content` is the identity a sha256 makes, so a lookup by content hash is a secondary key
/// read rather than a scan. [`Entry::location`] is computed from the three fields that name
/// where the row sits, so finding one path is one key read, reading a whole revision is one
/// 12 byte prefix scan and reading a whole repository is one 8 byte prefix scan; no
/// repository id is indexed separately and no location is written into the row. `node` is
/// not indexed either: every reader that used to scan by it is scoped by source or
/// repository instead, and how many rows a node holds is on the node's own record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 7, version = 1, with = super::codec::Compact)]
#[native_db(secondary_key(location -> LocationKey, unique))]
pub(super) struct Entry {
    #[primary_key]
    pub(super) id: i64,
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
    /// The cheap xxh3-of-edges identity of these bytes, kept only for an entry of a loose
    /// folder, whose revision sha is derived from it. A rescan reuses it for every path
    /// whose stamp did not move, so an unchanged folder reads no file at all.
    pub(super) fingerprint: Option<String>,
    pub(super) node: i64,
    pub(super) pass: u64,
    pub(super) pull_progress: Option<f32>,
    /// Where the bytes sit. Every writer that moves them must write it again.
    pub(super) placed: Placement,
}

impl Entry {
    /// The unique index native_db maintains over this row, read from the three fields that
    /// say where it sits. A writer that moves one of them moves the key with it, so there
    /// is nothing to keep in step by hand.
    pub(super) fn location(&self) -> LocationKey {
        LocationKey::at(self.repo_id, self.revision, &self.rel_path)
    }
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
        LocationKey::at(repo_id, revision, rel_path),
    )?)
}

/// A user's verdict on what an artifact is. It outranks inference: the artifact record
/// carries what the verdict says, and a rescan re-applies it over freshly inferred facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 12, version = 1, with = super::codec::Compact)]
#[native_db]
pub(super) struct Override {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) role: Option<Role>,
    pub(super) sub_role: Option<SubRole>,
    pub(super) base: Option<Base>,
    pub(super) decided_at: String,
}

/// The most tensor samples a stored header keeps, out of the `sniff::MAX_SAMPLES` the sniff
/// draws.
///
/// The sniff draws its samples so that inference can read shapes, ranks and keyword keys out
/// of a file it will not open again. What is read back afterwards is the triage queue's
/// hidden dimension, the most common trailing dimension among the samples of rank two or
/// more; nothing prints a sample by name. So the row thins the draw by a stride rather than
/// keeping its head: a mode is a fact about the spread, and the front of a name-sorted header
/// is one end of the model. A full draw of 64 keeps every fourth; a header already inside the
/// bound keeps all of it.
const DETAIL_SAMPLES: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 8, version = 1, with = super::codec::Cold)]
#[native_db]
pub(super) struct ArtifactDetail {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) evidence: Vec<Evidence>,
}

impl ArtifactDetail {
    /// The row as it is stored: the header the sniff measured with its samples cut to
    /// [`DETAIL_SAMPLES`], and the evidence behind the verdict whole.
    pub(super) fn new(
        artifact_id: i64,
        tensors: Option<TensorSummary>,
        evidence: Vec<Evidence>,
    ) -> Self {
        Self {
            artifact_id,
            tensors: tensors.map(|mut tensors| {
                let drawn = std::mem::take(&mut tensors.samples);
                let step = drawn.len().div_ceil(DETAIL_SAMPLES).max(1);
                tensors.samples = drawn
                    .into_iter()
                    .step_by(step)
                    .take(DETAIL_SAMPLES)
                    .collect();
                tensors
            }),
            evidence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 5, version = 1, with = super::codec::Cold)]
#[native_db]
pub(super) struct SearchText {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) keys: String,
}

/// The card of one revision, stored once however many artifacts the revision holds, with the
/// directory the card sits in: a document carries the card only when the card covers the
/// entry it was written from.
///
/// The key is [`LocationKey::revision`], the same 12 bytes an entry's location starts with,
/// so one revision is a point lookup and one repository's cards are an 8 byte prefix scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 23, version = 1, with = super::codec::Cold)]
#[native_db]
pub(super) struct RevisionText {
    #[primary_key]
    pub(super) key: LocationKey,
    pub(super) directory: String,
    pub(super) card: String,
}

impl RevisionText {
    /// The card text one file carries into its document: the body when the card sits at or
    /// above the file's own directory, and nothing when it sits below it or beside it.
    pub(super) fn text_for(&self, rel_path: &str) -> &str {
        match covers(&self.directory, directory_of(rel_path)) {
            true => &self.card,
            false => "",
        }
    }
}

/// A card as its writer holds it: the body, and the directory inside the revision it was
/// read from, which is what decides the files it describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CardText<'a> {
    pub(super) directory: &'a str,
    pub(super) body: &'a str,
}

/// Stores the card of one revision, writing only when the text or the directory is not the
/// one already there: a rescan that reaches a revision whose card did not change leaves
/// the row alone, and a revision that lost its card loses its row. Answers whether the row
/// moved, which is what tells the caller the revision's artifacts have to be indexed again.
/// A card that moved to another folder keeping its words moved, because it now describes
/// other files.
pub(super) fn store_revision_card(
    transaction: &native_db::transaction::RwTransaction<'_>,
    repo_id: i64,
    revision: u32,
    card: CardText<'_>,
) -> super::Result<bool> {
    let key = LocationKey::revision(repo_id, revision);
    let stored: Option<RevisionText> = transaction.get().primary(key.clone())?;
    match (stored, card.body.is_empty()) {
        (Some(stored), true) => {
            transaction.remove(stored)?;
            Ok(true)
        }
        (Some(stored), false) if stored.card != card.body || stored.directory != card.directory => {
            transaction.upsert(RevisionText {
                directory: card.directory.to_owned(),
                card: card.body.to_owned(),
                ..stored
            })?;
            Ok(true)
        }
        (Some(_), false) | (None, true) => Ok(false),
        (None, false) => {
            transaction.insert(RevisionText {
                key,
                directory: card.directory.to_owned(),
                card: card.body.to_owned(),
            })?;
            Ok(true)
        }
    }
}

/// Refiles a revision's card onto the ordinal its entries have moved to, for a revision the
/// walk rediscovered at a fresh slot with every file it holds unchanged.
///
/// A card already at the new ordinal is the one the walk just wrote from the revision's own
/// README, and it outranks the one the entries are moving from: the commit the branch
/// advanced onto is what the card of that slot describes. The old row goes either way.
pub(super) fn move_revision_card(
    transaction: &native_db::transaction::RwTransaction<'_>,
    repo_id: i64,
    from: u32,
    to: u32,
) -> super::Result<()> {
    let Some(stored) = transaction
        .get()
        .primary::<RevisionText>(LocationKey::revision(repo_id, from))?
    else {
        return Ok(());
    };
    let held: Option<RevisionText> = transaction
        .get()
        .primary(LocationKey::revision(repo_id, to))?;
    let carried = held
        .is_none()
        .then(|| (stored.directory.clone(), stored.card.clone()));
    transaction.remove(stored)?;
    match carried {
        Some((directory, card)) => store_revision_card(
            transaction,
            repo_id,
            to,
            CardText {
                directory: &directory,
                body: &card,
            },
        )
        .map(|_| ()),
        None => Ok(()),
    }
}

/// Drops every card of one repository, for a repository whose rows are being forgotten.
pub(super) fn remove_revision_cards(
    transaction: &native_db::transaction::RwTransaction<'_>,
    repo_id: i64,
) -> super::Result<()> {
    let cards: Vec<RevisionText> = transaction
        .scan()
        .primary()?
        .start_with(LocationKey::repository(repo_id))?
        .collect::<native_db::db_type::Result<_>>()?;
    for card in cards {
        transaction.remove(card)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 6, version = 1)]
#[native_db]
pub(super) struct Manifest {
    #[primary_key]
    pub(super) id: u8,
    pub(super) generation: u64,
    pub(super) scanned_at: String,
    pub(super) node: String,
    pub(super) counts: Vec<(String, u64)>,
    pub(super) search_directory: Option<String>,
    pub(super) search_generation: u64,
    pub(super) search_seq: u64,
    /// The triage threshold the last scan ran with, which every writer that counts an
    /// artifact into the summary counts against, so a reader's own configuration cannot
    /// restate what a scan settled.
    pub(super) triage_confidence: f32,
    /// Whether a pass that commits many times is running. The derived counts move in the
    /// transaction that moves the entries behind them, but the totals are a running sum a
    /// pass settles once at the end, so a pass that finds this set is resuming after one
    /// that was killed and counts the totals again before it starts.
    pub(super) pass_open: bool,
}

/// One artifact whose documents the published search index no longer matches. The sequence
/// orders the log; a refresh replays every row after the manifest's cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 10, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 11, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 9, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 13, version = 1, with = super::codec::Compact)]
#[native_db]
pub(super) struct Setting {
    #[primary_key]
    pub(super) key: String,
    pub(super) value: String,
}

/// A saved query. The six shelves hoard ships with are seeded on every open and keep the
/// positions the design gives them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 14, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 15, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 16, version = 1, with = super::codec::Compact)]
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

/// Where one served response sits in a repository's log: the repository id in big-endian
/// bytes followed by the row's RFC 3339 time. Ids come from the allocator and are positive,
/// which is what makes the big-endian order the numeric one, so the first 8 bytes are
/// exactly one repository and a day window is one range rather than a scan of the log. The
/// time is compared as the text the row stores, which is the order the pull list already
/// read the log in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepoAtKey(Vec<u8>);

impl RepoAtKey {
    /// One row of one repository's log.
    pub(super) fn at(repo_id: i64, at: &str) -> Self {
        let mut key = Vec::with_capacity(size_of::<i64>() + at.len());
        key.extend_from_slice(&repo_id.to_be_bytes());
        key.extend_from_slice(at.as_bytes());
        Self(key)
    }

    /// What every row of one repository shares, which is where its log starts.
    pub(super) fn repository(repo_id: i64) -> Self {
        Self(repo_id.to_be_bytes().to_vec())
    }

    /// The first key past one repository's log, which bounds a range exclusively.
    pub(super) fn after(repo_id: i64) -> Self {
        Self::repository(repo_id.saturating_add(1))
    }
}

impl ToKey for RepoAtKey {
    fn to_key(&self) -> native_db::db_type::Key {
        native_db::db_type::Key::new(self.0.clone())
    }

    fn key_names() -> Vec<String> {
        vec!["RepoAtKey".to_owned()]
    }
}

/// One response this node served, which is what the USAGE tab reads back.
///
/// The repository is indexed with the time beside it rather than alone, so a window reads
/// the rows inside it instead of every row the repository ever served.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 17, version = 1, with = super::codec::Compact)]
#[native_db(secondary_key(repo_at -> RepoAtKey, optional))]
pub(super) struct Usage {
    #[primary_key]
    pub(super) seq: u64,
    pub(super) repo_id: Option<i64>,
    pub(super) at: String,
    pub(super) client: Option<String>,
    pub(super) artifact_id: Option<i64>,
    pub(super) path: String,
    pub(super) bytes: u64,
    pub(super) rate_bps: Option<u64>,
    pub(super) status: u16,
}

impl Usage {
    /// The index native_db maintains over this row, read from the two fields that say
    /// which log it belongs to and where in it. A row that names no repository is not
    /// indexed.
    pub(super) fn repo_at(&self) -> Option<RepoAtKey> {
        self.repo_id.map(|repo_id| RepoAtKey::at(repo_id, &self.at))
    }
}

/// What the USAGE tab shows above its chart, maintained by every usage write rather than
/// counted from the log: how often this node served one repository, which clients pulled
/// it, and when the last pull was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 22, version = 1, with = super::codec::Compact)]
#[native_db]
pub(super) struct RepoUsage {
    #[primary_key]
    pub(super) repo_id: i64,
    pub(super) pulls: u64,
    /// How many pulls each of the most recent `usage::HISTORY_DAYS` UTC days holds, newest
    /// first, as the chart draws them. Moved by the write that moves `pulls`, so the chart
    /// is a point read rather than a pass over the window.
    pub(super) days: Vec<(String, u64)>,
    /// Sorted, so a client already counted is a binary search rather than a scan. The LAN's
    /// clients bound it, not the log's rows, and `usage::CLIENT_CAP` bounds it absolutely at
    /// 4,096 addresses, past which the count is that cap rather than the true one.
    pub(super) clients: Vec<String>,
    pub(super) last_pull: String,
}

/// How often one artifact has been served and when it last was, which is what `used:` and
/// the last-used column read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 18, version = 1, with = super::codec::Compact)]
#[native_db]
pub(super) struct ArtifactUse {
    #[primary_key]
    pub(super) artifact_id: i64,
    pub(super) count: u64,
    pub(super) last_used: String,
}

/// A saved loadout: the TOML document the drawer edits, keyed by the name inside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[native_model(id = 19, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 20, version = 1, with = super::codec::Compact)]
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
#[native_model(id = 21, version = 1, with = super::codec::Compact)]
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
    /// move a repository's present counts, through
    /// [`super::scan::counters::DerivedShift`]; a writer added later must do the same.
    pub(super) filed: u64,
}

/// Registers every record type.
pub(super) fn define_models() -> super::Result<Models> {
    let mut models = Models::new();
    models.define::<Source>()?;
    models.define::<Repository>()?;
    models.define::<ArtifactRecord>()?;
    models.define::<SearchText>()?;
    models.define::<RevisionText>()?;
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
    models.define::<RepoUsage>()?;
    models.define::<ArtifactUse>()?;
    models.define::<LoadoutDoc>()?;
    models.define::<Job>()?;
    models.define::<Node>()?;
    Ok(models)
}
