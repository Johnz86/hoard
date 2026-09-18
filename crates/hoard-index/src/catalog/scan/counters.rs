use std::collections::{HashMap, HashSet};

use native_db::Database;
use native_db::transaction::{RTransaction, RwTransaction};

use hoard_core::State;

use crate::catalog::models::{
    Allocator, ArtifactRecord, Entry, Manifest, Node, Repository, SearchChange, Source, Summary,
};
use crate::error::{IndexError, Result};

pub(in crate::catalog) struct Counters {
    repos: u64,
    pub(in crate::catalog) artifacts: u64,
    pub(in crate::catalog) entries: u64,
    touched: HashSet<i64>,
    pub(in crate::catalog) pending: Vec<i64>,
    pub(in crate::catalog) derived: DerivedShift,
    triage: f32,
    before: HashMap<i64, SummaryPart>,
    /// Whether a published search index exists for the change log to be a log of.
    ///
    /// Read once, when the pass starts. A rebuild reads every artifact record rather than
    /// the log, and cannot run beside a pass anyway: redb takes one writer at a time and
    /// `open_or_scan` runs the scan to completion before it builds.
    logging: bool,
}

/// What one artifact record contributes to the summary. A record that does not exist
/// contributes nothing, which is what makes a created or removed record a plain difference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SummaryPart {
    artifacts: u64,
    present: u64,
    triage: u64,
    indexed_bytes: u64,
    advertised: u64,
    reclaimable_bytes: u64,
    duplicates: u64,
}

impl SummaryPart {
    pub(super) fn of(record: Option<&ArtifactRecord>, triage: f32) -> Self {
        let Some(record) = record else {
            return Self::default();
        };
        let bytes = record.group_bytes.unwrap_or(record.artifact.size);
        let present = record.path_count > 0;
        let copies = u64::from(record.copies.saturating_sub(1));
        Self {
            artifacts: 1,
            present: u64::from(present),
            triage: u64::from(present && record.needs_triage(triage)),
            indexed_bytes: match present {
                true => bytes,
                false => 0,
            },
            advertised: u64::from(present && record.artifact.sha256.is_some()),
            reclaimable_bytes: bytes.saturating_mul(copies),
            duplicates: u64::from(copies > 0),
        }
    }
}

/// Moves the summary by the difference one record made, so nothing but the records a pass
/// touched is ever read.
pub(super) fn shift_summary(summary: &mut Summary, before: SummaryPart, after: SummaryPart) {
    let moved = |total: u64, less: u64, more: u64| total.saturating_sub(less).saturating_add(more);
    summary.artifacts = moved(summary.artifacts, before.artifacts, after.artifacts);
    summary.present_artifacts = moved(summary.present_artifacts, before.present, after.present);
    summary.triage_artifacts = moved(summary.triage_artifacts, before.triage, after.triage);
    summary.indexed_bytes = moved(
        summary.indexed_bytes,
        before.indexed_bytes,
        after.indexed_bytes,
    );
    summary.advertised_artifacts = moved(
        summary.advertised_artifacts,
        before.advertised,
        after.advertised,
    );
    summary.reclaimable_bytes = moved(
        summary.reclaimable_bytes,
        before.reclaimable_bytes,
        after.reclaimable_bytes,
    );
    summary.duplicate_artifacts = moved(
        summary.duplicate_artifacts,
        before.duplicates,
        after.duplicates,
    );
}

pub(in crate::catalog) fn blank_summary() -> Summary {
    Summary {
        id: 0,
        reclaimable_bytes: 0,
        duplicate_artifacts: 0,
        artifacts: 0,
        present_artifacts: 0,
        triage_artifacts: 0,
        indexed_bytes: 0,
        repositories: 0,
        advertised_artifacts: 0,
    }
}

/// What one entry contributes to the counts kept on the records above it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Contribution {
    files: i64,
    bytes: i64,
    owed: i64,
    owed_bytes: i64,
}

/// What a pass moved in one source's hashing debt: the files still owing a sha256 and the
/// bytes they hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OwedShift {
    files: i64,
    bytes: i64,
}

/// What a pass moved in the counts derived from its entries: each repository's present
/// files and bytes, each source's owed hashes, and each node's filed rows. Accumulating
/// them means a record above the entries is rewritten once per pass rather than once per
/// entry.
#[derive(Debug, Default)]
pub(in crate::catalog) struct DerivedShift {
    repositories: HashMap<i64, Contribution>,
    nodes: HashMap<i64, i64>,
}

impl DerivedShift {
    /// Counts an entry the caller is about to write. An entry holding no bytes on any node
    /// moves only that node's filed count.
    pub(in crate::catalog) fn added(&mut self, entry: &Entry) {
        self.shift(entry, 1);
    }

    /// Counts an entry the caller is about to remove or move out of presence.
    pub(in crate::catalog) fn removed(&mut self, entry: &Entry) {
        self.shift(entry, -1);
    }

    fn shift(&mut self, entry: &Entry, sign: i64) {
        let filed = self.nodes.entry(entry.node).or_default();
        *filed = filed.saturating_add(sign);
        if entry.state == State::Remote {
            return;
        }
        let bytes = i64::try_from(entry.size).unwrap_or(i64::MAX);
        let slot = self.repositories.entry(entry.repo_id).or_default();
        slot.files = slot.files.saturating_add(sign);
        slot.bytes = slot.bytes.saturating_add(sign.saturating_mul(bytes));
        if owes_a_hash(entry) {
            slot.owed = slot.owed.saturating_add(sign);
            slot.owed_bytes = slot.owed_bytes.saturating_add(sign.saturating_mul(bytes));
        }
    }

    /// Writes what has accumulated onto the repositories, sources and nodes it names, and
    /// starts again empty.
    pub(in crate::catalog) fn flush(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        let mut owed: HashMap<String, OwedShift> = HashMap::new();
        for (repo_id, moved) in self.repositories.drain() {
            if moved == Contribution::default() {
                continue;
            }
            let Some(stored) = transaction.get().primary::<Repository>(repo_id)? else {
                continue;
            };
            if moved.owed != 0 || moved.owed_bytes != 0 {
                let debt = owed.entry(stored.source.clone()).or_default();
                debt.files = debt.files.saturating_add(moved.owed);
                debt.bytes = debt.bytes.saturating_add(moved.owed_bytes);
            }
            if moved.files == 0 && moved.bytes == 0 {
                continue;
            }
            transaction.upsert(Repository {
                present_files: shifted_u32(stored.present_files, moved.files),
                present_bytes: shifted_u64(stored.present_bytes, moved.bytes),
                ..stored
            })?;
        }
        for (name, moved) in owed {
            let Some(stored) = transaction.get().primary::<Source>(name)? else {
                continue;
            };
            transaction.upsert(Source {
                pending_hashes: shifted_u64(stored.pending_hashes, moved.files),
                pending_bytes: shifted_u64(stored.pending_bytes, moved.bytes),
                ..stored
            })?;
        }
        for (node, moved) in self.nodes.drain() {
            if moved == 0 {
                continue;
            }
            let Some(stored) = transaction.get().primary::<Node>(node)? else {
                continue;
            };
            transaction.upsert(Node {
                filed: shifted_u64(stored.filed, moved),
                ..stored
            })?;
        }
        Ok(())
    }
}

/// Whether this row still owes the catalog a sha256: bytes on this disk that nothing has
/// read yet.
pub(in crate::catalog) fn owes_a_hash(entry: &Entry) -> bool {
    entry.sha256.is_none() && entry.state != State::Remote && !entry.placed.is_absent()
}

fn shifted_u64(current: u64, by: i64) -> u64 {
    match by.is_negative() {
        true => current.saturating_sub(by.unsigned_abs()),
        false => current.saturating_add(by.unsigned_abs()),
    }
}

fn shifted_u32(current: u32, by: i64) -> u32 {
    u32::try_from(shifted_u64(u64::from(current), by)).unwrap_or(u32::MAX)
}

impl Counters {
    pub(in crate::catalog) fn load(database: &Database<'static>, triage: f32) -> Result<Self> {
        let read = database.r_transaction()?;
        let stored: Option<Allocator> = read.get().primary(0_u8)?;
        let manifest: Option<Manifest> = read.get().primary(0_u8)?;
        let allocator = stored.unwrap_or(Allocator {
            id: 0,
            repos: 0,
            artifacts: 0,
            entries: 0,
        });
        Ok(Self {
            repos: allocator
                .repos
                .max(highest_id(&read, |row: &Repository| row.id)?),
            artifacts: allocator
                .artifacts
                .max(highest_id(&read, |row: &ArtifactRecord| row.id)?),
            entries: allocator
                .entries
                .max(highest_id(&read, |row: &Entry| row.id)?),
            touched: HashSet::new(),
            pending: Vec::new(),
            derived: DerivedShift::default(),
            triage,
            before: HashMap::new(),
            logging: manifest.is_some_and(|manifest| manifest.search_directory.is_some()),
        })
    }

    pub(in crate::catalog) fn touch(
        &mut self,
        transaction: &RwTransaction<'_>,
        id: i64,
    ) -> Result<()> {
        self.touched.insert(id);
        self.capture_summary(transaction, id)
    }

    pub(in crate::catalog) fn take_touched(&mut self) -> HashSet<i64> {
        std::mem::take(&mut self.touched)
    }

    pub(in crate::catalog) fn capture_summary(
        &mut self,
        transaction: &RwTransaction<'_>,
        id: i64,
    ) -> Result<()> {
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            let record: Option<ArtifactRecord> = transaction.get().primary(id)?;
            slot.insert(SummaryPart::of(record.as_ref(), self.triage));
        }
        Ok(())
    }

    pub(in crate::catalog) fn settle_summary(
        &mut self,
        transaction: &RwTransaction<'_>,
    ) -> Result<()> {
        if self.before.is_empty() {
            return Ok(());
        }
        let stored: Option<Summary> = transaction.get().primary(0_u8)?;
        let mut summary = stored.clone().unwrap_or_else(blank_summary);
        for (id, before) in self.before.drain() {
            let record: Option<ArtifactRecord> = transaction.get().primary(id)?;
            let after = SummaryPart::of(record.as_ref(), self.triage);
            shift_summary(&mut summary, before, after);
        }
        if stored.as_ref() != Some(&summary) {
            transaction.upsert(summary)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::catalog) fn holds_no_summary_snapshot(&self) -> bool {
        self.before.is_empty()
    }

    pub(in crate::catalog) fn save(&mut self, transaction: &RwTransaction<'_>) -> Result<()> {
        self.derived.flush(transaction)?;
        self.settle_summary(transaction)?;
        transaction.upsert(Allocator {
            id: 0,
            repos: self.repos,
            artifacts: self.artifacts,
            entries: self.entries,
        })?;
        Ok(())
    }

    pub(in crate::catalog) fn next_repo(&mut self) -> Result<i64> {
        next_id(&mut self.repos)
    }
}

/// Records, in the transaction that made them, which artifacts the published search index no
/// longer matches. The sequence continues past both the highest key the log still holds and
/// the refresh cursor, because a drained log holds neither the numbers it has already spent
/// nor any proof that a refresh has not already passed them.
///
/// A pass that runs before any index is published writes nothing. There is no index for the
/// rows to be a difference against: the build that follows reads every artifact record, and
/// publishing prunes the whole log unread. A fresh scan of a million artifacts logged and
/// then deleted a million rows, and redb keeps the pages it grew the file by.
pub(in crate::catalog) fn log_changes(
    transaction: &RwTransaction<'_>,
    counters: &mut Counters,
) -> Result<()> {
    if !counters.logging {
        counters.pending.clear();
        return Ok(());
    }
    if counters.pending.is_empty() {
        return Ok(());
    }
    counters.pending.sort_unstable();
    counters.pending.dedup();
    let last: Option<SearchChange> = transaction
        .scan()
        .primary::<SearchChange>()?
        .all()?
        .next_back()
        .transpose()?;
    let manifest: Option<Manifest> = transaction.get().primary(0_u8)?;
    let spent = last
        .map_or(0, |change| change.seq)
        .max(manifest.map_or(0, |manifest| manifest.search_seq));
    for (offset, artifact_id) in counters.pending.drain(..).enumerate() {
        transaction.insert(SearchChange {
            seq: spent + offset as u64 + 1,
            artifact_id,
        })?;
    }
    Ok(())
}

/// The highest id one table holds, which a pass killed before it saved the allocator left
/// above the allocator. Handing that id out again would write over the row already holding
/// it, so a pass starts from whichever of the two is higher.
fn highest_id<T>(read: &RTransaction<'_>, id: fn(&T) -> i64) -> Result<u64>
where
    T: native_db::ToInput,
{
    let last: Option<T> = read.scan().primary()?.all()?.next_back().transpose()?;
    Ok(last.map_or(0, |row| u64::try_from(id(&row)).unwrap_or(0)))
}

pub(in crate::catalog) fn next_id(counter: &mut u64) -> Result<i64> {
    let id = counter
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| IndexError::Catalog("catalog identity space exhausted".into()))?;
    *counter = id as u64;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use hoard_core::{Artifact, ArtifactFormat, ArtifactId, Base, BlobId, Role};

    use super::*;

    fn record(path_count: u32, copies: u32) -> ArtifactRecord {
        ArtifactRecord {
            id: 1,
            identity: 7,
            artifact: Artifact {
                id: ArtifactId::new(1),
                blob: BlobId::new(1),
                sha256: None,
                size: 4_096,
                format: ArtifactFormat::Safetensors,
                role: Role::Lora,
                sub_role: None,
                role_confidence: 0.99,
                base: Base::any(),
                base_label: None,
                base_confidence: 0.9,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
            },
            name: "lora.safetensors".to_owned(),
            org: None,
            path_count,
            copies,
            group_bytes: None,
            shard_count: None,
            declared_total: None,
            incomplete: false,
            named_by_repo: false,
            rejected: false,
            first_seen: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn summary_part_reclaimable_bytes_follows_copies_not_path_count() {
        let three_paths_two_files = SummaryPart::of(Some(&record(3, 2)), 0.5);
        assert_eq!(
            three_paths_two_files.reclaimable_bytes, 4_096,
            "one of the two real files is reclaimable, not two of the three paths"
        );
        assert_eq!(three_paths_two_files.duplicates, 1);

        let two_paths_one_file = SummaryPart::of(Some(&record(2, 1)), 0.5);
        assert_eq!(two_paths_one_file.reclaimable_bytes, 0);
        assert_eq!(two_paths_one_file.duplicates, 0);
    }
}
