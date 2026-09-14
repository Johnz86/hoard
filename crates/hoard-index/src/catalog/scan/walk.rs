use std::collections::{HashMap, HashSet};
use std::path::Path;

use native_db::transaction::RwTransaction;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use crate::catalog::models::Entry;
use crate::discover::DiscoveredEntry;
use crate::error::Result;
use crate::hash::FileStamp;

use super::RescanReport;
use super::counters::Counters;
use super::plan::{PreparedEntry, Unit};
use super::store::ScanSettings;

/// What one pass over the sources has settled so far: which stored rows it recognised, and
/// how many files it left alone, read again, added or retracted.
pub(super) struct Walk {
    pub(super) settings: ScanSettings,
    pub(super) known: KnownPaths,
    /// The paths of the one repository a fresh revision slot is being refiled from, and
    /// which repository they are, so no more than one repository's index is ever held.
    pub(super) paths: PathIndex,
    pub(super) paths_of: Option<i64>,
    pub(super) sizes: RevisionSizes,
    /// Every revision slot this walk discovered, by repository and ordinal, so a row
    /// sitting on a revision the walk is about to reach is not refiled onto another.
    pub(super) discovered: HashSet<(i64, u32)>,
    pub(super) seen: HashSet<i64>,
    pub(super) reached: HashSet<i64>,
    pub(super) missing: Vec<Known>,
    /// Entries whose revision moved to another ordinal under them, with the ordinal it
    /// moved to. Written once at the end of the pass rather than once per unit.
    pub(super) moved: Vec<(i64, u32)>,
    pub(super) report: RescanReport,
}

impl Walk {
    pub(super) fn new(settings: ScanSettings) -> Self {
        Self {
            settings,
            known: HashMap::new(),
            paths: HashMap::new(),
            paths_of: None,
            sizes: HashMap::new(),
            discovered: HashSet::new(),
            seen: HashSet::new(),
            reached: HashSet::new(),
            missing: Vec::new(),
            moved: Vec::new(),
            report: RescanReport::default(),
        }
    }

    pub(super) fn set_aside_unseen(&mut self) {
        let seen = &self.seen;
        self.missing.extend(
            self.known
                .drain()
                .map(|(_, known)| known)
                .filter(|known| !seen.contains(&known.entry_id)),
        );
        self.sizes.clear();
        self.seen.clear();
    }
}

/// One repository of a walked source: the row it was filed under, and the ordinal each of
/// its discovered revisions takes in that row.
pub(super) struct WalkedRepo {
    pub(super) id: i64,
    pub(super) ordinals: Vec<u32>,
}

/// The discovered entries of one revision of one repository, with the identity the catalog
/// files them under. A stored row is recognised by where it sits, not by the absolute path
/// its bytes were last found at, which for an hf-cache entry is a blob a rewrite replaces.
#[derive(Clone, Copy)]
pub(super) struct RevisionEntries<'a> {
    pub(super) repo_id: i64,
    pub(super) ordinal: u32,
    pub(super) entries: &'a [DiscoveredEntry],
}

impl RevisionEntries<'_> {
    fn location(&self, position: usize) -> u128 {
        location_hash(self.repo_id, self.ordinal, &self.entries[position].rel_path)
    }
}

/// What the catalog already holds for one path, so a walk can decide without opening it.
///
/// The two absent cases are spelled as zero rather than as an `Option` discriminant: ids
/// come from the allocator and are positive, and a set of no shards is not a set.
#[derive(Clone, Copy)]
pub(super) struct Known {
    pub(super) entry_id: i64,
    pub(super) artifact_id: i64,
    pub(super) stamp: FileStamp,
    pub(super) revision: u32,
    pub(super) shard_count: u32,
}

impl Known {
    pub(super) fn artifact(self) -> Option<i64> {
        (self.artifact_id != 0).then_some(self.artifact_id)
    }

    fn shards(self) -> Option<u32> {
        (self.shard_count != 0).then_some(self.shard_count)
    }
}

/// Every path of the repository being walked the catalog holds bytes for, by
/// [`location_hash`].
pub(super) type KnownPaths = HashMap<u128, Known>;

/// Where a repository holds one relative path, by [`path_hash`]: the location of the one
/// row that holds it, or nothing when more than one revision of the repository does. It is
/// what a revision rediscovered at a fresh ordinal is refiled from, and it holds a key
/// rather than a [`Known`] so the rows themselves are kept once.
pub(super) type PathIndex = HashMap<u128, Option<u128>>;

/// How many of those paths each stored revision holds, by repository and ordinal.
pub(super) type RevisionSizes = HashMap<(i64, u32), usize>;

/// What the catalog already holds for the repository a walk is about to enter.
pub(super) struct KnownRows {
    pub(super) known: KnownPaths,
    pub(super) sizes: RevisionSizes,
}

pub(super) enum UnitDecision {
    Skip,
    Prepare {
        prepare: Vec<usize>,
        unchanged: Vec<usize>,
    },
}

fn known_of(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> Option<Known> {
    match walk.known.get(&found.location(position)) {
        Some(known) => Some(*known),
        None => refiled_of(walk, found, position),
    }
}

/// The row another revision of the same repository holds this path at, when the catalog
/// holds no row at all for the revision the walk is in.
///
/// An entry names its revision by ordinal, so a revision the catalog had not seen - a clone
/// whose branch advanced onto a fresh commit - takes a fresh slot while every file it holds
/// is the file the catalog already read. Recognising those rows is what refiles them onto
/// the new ordinal rather than reading them again and retracting the ones they replace.
///
/// The stamp has to match, so only a file that did not change is refiled, and a path more
/// than one stored revision holds is never refiled: which of them moved is not knowable
/// from the path, and taking the wrong one would empty the revision it is filed under. A
/// row another revision of this walk has already accounted for is left to it, and so is a
/// row whose own revision this walk discovered: that revision is still on this disk, so its
/// row moved nowhere, however alike the two paths look. Two hf-cache snapshots linking one
/// blob are exactly that pair.
fn refiled_of(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> Option<Known> {
    if walk.sizes.contains_key(&(found.repo_id, found.ordinal)) {
        return None;
    }
    let entry = &found.entries[position];
    let stamp = entry.stamp?;
    let held = (*walk.paths.get(&path_hash(found.repo_id, &entry.rel_path))?)?;
    let known = walk.known.get(&held)?;
    (known.stamp == stamp
        && !walk.seen.contains(&known.entry_id)
        && !walk.discovered.contains(&(found.repo_id, known.revision)))
    .then_some(*known)
}

/// Where a row sits, as the key a rescan looks it up by: the repository, the revision
/// ordinal and the relative path, which is the identity the catalog files an entry under
/// and the one a file rewritten in place keeps. Two distinct locations colliding on 128
/// bits is not a case this workspace budgets for.
pub(in crate::catalog) fn location_hash(repo_id: i64, ordinal: u32, rel_path: &str) -> u128 {
    let seed = repo_id.cast_unsigned().rotate_left(32) ^ u64::from(ordinal);
    xxhash_rust::xxh3::xxh3_128_with_seed(rel_path.as_bytes(), seed)
}

/// One relative path of one repository, whichever of its revisions holds it, as the key the
/// refile lookup uses.
pub(super) fn path_hash(repo_id: i64, rel_path: &str) -> u128 {
    xxhash_rust::xxh3::xxh3_128_with_seed(rel_path.as_bytes(), repo_id.cast_unsigned())
}

/// Whether nothing in a revision needs opening: it holds every path the catalog holds bytes
/// for and no others, each at the same size, inode and mtime. The count is as much of the
/// condition as the stamps are, because a revision that lost a file holds only unchanged
/// paths too.
pub(super) fn unchanged_revision(walk: &Walk, found: RevisionEntries<'_>) -> bool {
    walk.sizes
        .get(&(found.repo_id, found.ordinal))
        .copied()
        .unwrap_or(0)
        == found.entries.len()
        && (0..found.entries.len()).all(|position| unchanged_entry(walk, found, position))
}

fn unchanged_entry(walk: &Walk, found: RevisionEntries<'_>, position: usize) -> bool {
    match (
        known_of(walk, found, position),
        found.entries[position].stamp,
    ) {
        (Some(known), Some(stamp)) => known.stamp == stamp,
        (Some(_), None) | (None, _) => false,
    }
}

/// What a unit costs this walk: nothing when every file it holds is the file the catalog
/// already read, otherwise the positions to prepare again. A set is prepared whole, because
/// its artifact is the union of its shards.
pub(super) fn decide_unit(walk: &Walk, found: RevisionEntries<'_>, unit: &Unit) -> UnitDecision {
    match unit {
        Unit::Set(set) => {
            let members = u32::try_from(set.members.len()).unwrap_or(u32::MAX);
            let grouped = set
                .members
                .first()
                .and_then(|position| known_of(walk, found, *position))
                .is_some_and(|known| known.shards() == Some(members));
            let intact = grouped
                && set
                    .members
                    .iter()
                    .all(|position| unchanged_entry(walk, found, *position));
            match intact {
                true => UnitDecision::Skip,
                false => UnitDecision::Prepare {
                    prepare: set.members.clone(),
                    unchanged: Vec::new(),
                },
            }
        }
        Unit::Chunk(positions) => {
            let (unchanged, prepare): (Vec<usize>, Vec<usize>) = positions
                .iter()
                .copied()
                .partition(|position| unchanged_entry(walk, found, *position));
            match prepare.is_empty() {
                true => UnitDecision::Skip,
                false => UnitDecision::Prepare { prepare, unchanged },
            }
        }
    }
}

/// Whether a file of a prepared unit was rewritten while the unit was being read.
///
/// Preparation sniffs and hashes outside the write transaction, so between the walk's
/// `stat` and the row that replaces it a file can change; writing the row anyway would
/// attach the size and hashes of bytes that are gone. A file that has since disappeared is
/// not a change here: preparation already saw it absent and the row it writes says so.
///
/// This narrows a window rather than closing one. It never was closed: redb locks the
/// database, not the filesystem, so a file could always change between the `stat` that
/// decided to read it and the commit that recorded what was read. The re-`stat` runs on the
/// same pool the sniff either side of it runs on, so a unit's files are read once each in
/// parallel rather than a thousand times in series.
pub(super) fn changed_while_prepared(prepared: &[PreparedEntry<'_>]) -> bool {
    prepared
        .par_iter()
        .any(|entry| stamp_moved(entry.discovered.abs_path.as_deref(), entry.discovered.stamp))
}

/// Whether the file at `path` reads differently now from the stamp the walk took. A path
/// with no stamp and a path that no longer stats both answer false: neither is a file that
/// changed under a read of it.
pub(in crate::catalog) fn stamp_moved(path: Option<&Path>, stamp: Option<FileStamp>) -> bool {
    let (Some(path), Some(stamp)) = (path, stamp) else {
        return false;
    };
    matches!(crate::hash::stamp(path), Ok(current) if current != stamp)
}

/// Marks the paths this walk left alone as seen, and records the ones whose revision has
/// moved to another ordinal since the row was written.
pub(super) fn mark_unchanged(
    walk: &mut Walk,
    found: RevisionEntries<'_>,
    positions: impl IntoIterator<Item = usize>,
) {
    for position in positions {
        if let Some(known) = known_of(walk, found, position) {
            walk.seen.insert(known.entry_id);
            walk.report.unchanged += 1;
            if known.revision != found.ordinal {
                walk.moved.push((known.entry_id, found.ordinal));
            }
        }
    }
}

/// Retires the rows the walk is about to write again, so a changed file keeps neither its
/// old entry nor a claim on an artifact its new bytes may no longer establish. Returns the
/// artifacts a path was taken from, which the same transaction recounts once the rows that
/// replace them are written.
pub(super) fn replace_known(
    transaction: &RwTransaction<'_>,
    walk: &mut Walk,
    counters: &mut Counters,
    found: RevisionEntries<'_>,
    positions: &[usize],
) -> Result<Vec<i64>> {
    let mut taken = Vec::new();
    for position in positions {
        let Some(known) = known_of(walk, found, *position) else {
            walk.report.added += 1;
            continue;
        };
        walk.seen.insert(known.entry_id);
        walk.report.changed += 1;
        if let Some(id) = known.artifact() {
            counters.touch(transaction, id)?;
            taken.push(id);
        }
        if let Some(entry) = transaction.get().primary::<Entry>(known.entry_id)? {
            counters.derived.removed(&entry);
            transaction.remove(entry)?;
        }
    }
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_id_and_an_absent_shard_count_are_spelled_zero() {
        let known = Known {
            entry_id: 7,
            artifact_id: 0,
            stamp: FileStamp {
                inode: 1,
                mtime: 2,
                size: 3,
            },
            revision: 0,
            shard_count: 0,
        };
        assert_eq!(known.artifact(), None);
        assert_eq!(known.shards(), None);
        let filed = Known {
            artifact_id: 9,
            shard_count: 4,
            ..known
        };
        assert_eq!(filed.artifact(), Some(9));
        assert_eq!(filed.shards(), Some(4));
    }
}
