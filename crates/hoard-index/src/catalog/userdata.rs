use std::collections::BTreeMap;
use std::path::PathBuf;

use native_db::ToInput;
use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::{ArtifactId, ShelfId};

use crate::discover::NoiseFilter;
use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{
    ArtifactRecord, Node, Noise, NoiseKey, Projection, ProjectionKey, Setting, Shelf, ShelfKey,
};
use super::verdict::Verdict;

/// The fixed shelves hoard ships with, in the order the rail lists them. The triage queue
/// leads, because what a node has not classified is the one shelf whose emptiness is the
/// goal. A sixth shelf naming the node itself is built at seed time.
const BUILTIN_SHELVES: [(&str, &str); 5] = [
    ("triage queue", "state:triage"),
    (
        "sdxl \u{b7} lora \u{b7} small",
        "role:lora base:sdxl size:<400mb",
    ),
    ("flux workhorse", "base:flux"),
    ("quantised llm", "role:llm fmt:gguf"),
    ("cold + duplicate", "dupe:true"),
];

/// A saved query as the rail, the SHELVES tab and `hoard ls` read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShelfRecord {
    pub id: ShelfId,
    pub name: String,
    pub query: String,
    pub builtin: bool,
    pub position: i64,
}

impl From<Shelf> for ShelfRecord {
    fn from(shelf: Shelf) -> Self {
        Self {
            id: ShelfId::new(shelf.id),
            name: shelf.name,
            query: shelf.query,
            builtin: shelf.builtin,
            position: shelf.position,
        }
    }
}

/// A rejected path, as the triage screen lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoiseRecord {
    pub id: i64,
    pub path: String,
    pub reason: Option<String>,
    pub added_at: String,
    pub artifact_id: Option<ArtifactId>,
}

impl From<Noise> for NoiseRecord {
    fn from(noise: Noise) -> Self {
        Self {
            id: noise.id,
            path: noise.path,
            reason: noise.reason,
            added_at: noise.added_at,
            artifact_id: noise.artifact_id.map(ArtifactId::new),
        }
    }
}

impl Catalog {
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let stored: Option<Setting> = self.database.r_transaction()?.get().primary(key)?;
        Ok(stored.map(|setting| setting.value))
    }

    /// Writes one setting, replacing any previous value.
    pub fn put_setting(&self, key: &str, value: &str) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        transaction.upsert(Setting {
            key: key.to_owned(),
            value: value.to_owned(),
        })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Setting> = transaction.get().primary(key)?;
        let removed = match stored {
            Some(setting) => {
                transaction.remove(setting)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(removed)
    }

    pub fn all_settings(&self) -> Result<BTreeMap<String, String>> {
        let read = self.database.r_transaction()?;
        let mut settings = BTreeMap::new();
        for setting in read.scan().primary::<Setting>()?.all()? {
            let setting = setting?;
            settings.insert(setting.key, setting.value);
        }
        Ok(settings)
    }

    /// Every shelf, ordered by position and then by id, which is the order the rail draws.
    pub fn list_shelves(&self) -> Result<Vec<ShelfRecord>> {
        let read = self.database.r_transaction()?;
        let mut shelves: Vec<Shelf> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        shelves.sort_by_key(|shelf| (shelf.position, shelf.id));
        Ok(shelves.into_iter().map(ShelfRecord::from).collect())
    }

    pub fn shelf(&self, id: ShelfId) -> Result<Option<ShelfRecord>> {
        let stored: Option<Shelf> = self.database.r_transaction()?.get().primary(id.get())?;
        Ok(stored.map(ShelfRecord::from))
    }

    pub fn create_shelf(&self, name: &str, query: &str, position: i64) -> Result<ShelfRecord> {
        let transaction = self.database.rw_transaction()?;
        let taken: Option<Shelf> = transaction.get().secondary(ShelfKey::name, name)?;
        if taken.is_some() {
            return Err(IndexError::Query(format!("a shelf named {name:?} exists")));
        }
        let shelf = Shelf {
            id: next_id::<Shelf>(&transaction)?,
            name: name.to_owned(),
            query: query.to_owned(),
            builtin: false,
            position,
        };
        transaction.insert(shelf.clone())?;
        transaction.commit()?;
        Ok(ShelfRecord::from(shelf))
    }

    pub fn update_shelf(
        &self,
        id: ShelfId,
        name: &str,
        query: &str,
        position: i64,
    ) -> Result<ShelfRecord> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Shelf> = transaction.get().primary(id.get())?;
        let stored = stored.ok_or_else(|| IndexError::not_found("shelf", id.to_string()))?;
        let shelf = Shelf {
            name: name.to_owned(),
            query: query.to_owned(),
            position,
            ..stored
        };
        transaction.upsert(shelf.clone())?;
        transaction.commit()?;
        Ok(ShelfRecord::from(shelf))
    }

    pub fn delete_shelf(&self, id: ShelfId) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Shelf> = transaction.get().primary(id.get())?;
        let deleted = match stored {
            Some(shelf) => {
                transaction.remove(shelf)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(deleted)
    }

    /// Upserts the six shelves hoard ships with, by name: the five fixed queries and one
    /// naming `node`, the node this catalog runs on. Idempotent; run on every open.
    ///
    /// A node renamed between runs seeds a shelf under its new name and leaves the one its
    /// old name made behind, because a shelf is identified by its name and nothing here
    /// reconciles two of them.
    pub fn seed_builtin_shelves(&self, node: &str) -> Result<usize> {
        let node_shelf = (format!("everything on {node}"), format!("node:{node}"));
        let shelves = BUILTIN_SHELVES
            .iter()
            .map(|(name, query)| (*name, *query))
            .chain(std::iter::once((
                node_shelf.0.as_str(),
                node_shelf.1.as_str(),
            )));
        let transaction = self.database.rw_transaction()?;
        let mut seeded = 0;
        for (position, (name, query)) in shelves.enumerate() {
            let stored: Option<Shelf> = transaction.get().secondary(ShelfKey::name, name)?;
            let id = match stored {
                Some(shelf) => shelf.id,
                None => next_id::<Shelf>(&transaction)?,
            };
            transaction.upsert(Shelf {
                id,
                name: name.to_owned(),
                query: query.to_owned(),
                builtin: true,
                position: position as i64,
            })?;
            seeded += 1;
        }
        transaction.commit()?;
        Ok(seeded)
    }

    /// Marks a path as noise, naming the artifact it was rejected from when one is known.
    /// Recording the same path again keeps its id, and keeps the reason and the artifact it
    /// already carries when this one names none.
    pub fn add_noise(
        &self,
        path: &str,
        reason: Option<&str>,
        artifact: Option<ArtifactId>,
    ) -> Result<i64> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Noise> = transaction.get().secondary(NoiseKey::path, path)?;
        let noise = match stored {
            Some(noise) => Noise {
                reason: reason.map(str::to_owned).or(noise.reason),
                artifact_id: artifact.map(ArtifactId::get).or(noise.artifact_id),
                ..noise
            },
            None => Noise {
                id: next_id::<Noise>(&transaction)?,
                path: path.to_owned(),
                reason: reason.map(str::to_owned),
                added_at: jiff::Timestamp::now().to_string(),
                artifact_id: artifact.map(ArtifactId::get),
            },
        };
        let id = noise.id;
        transaction.upsert(noise)?;
        transaction.commit()?;
        Ok(id)
    }

    /// Every rejected path, ordered by path.
    pub fn list_noise(&self) -> Result<Vec<NoiseRecord>> {
        let read = self.database.r_transaction()?;
        let rows: Vec<Noise> = read
            .scan()
            .secondary(NoiseKey::path)?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        Ok(rows.into_iter().map(NoiseRecord::from).collect())
    }

    /// Removes the row and, when it named an artifact no other noise row still names, clears
    /// that artifact's rejection so it returns to the queue at once. Answers the artifact it
    /// restored, `Some(None)` for a row that named none, `None` for an id that was no row.
    pub fn remove_noise(&self, id: i64) -> Result<Option<Option<ArtifactId>>> {
        let transaction = self.database.rw_transaction()?;
        let Some(noise) = transaction.get().primary::<Noise>(id)? else {
            transaction.commit()?;
            return Ok(None);
        };
        let rejected_from = noise.artifact_id;
        transaction.remove(noise)?;
        let restored = match rejected_from {
            Some(artifact) if restorable(&transaction, artifact)? => {
                Some(ArtifactId::new(artifact))
            }
            Some(_) | None => None,
        };
        transaction.commit()?;
        if let Some(artifact) = restored {
            self.record_verdict(artifact, Verdict::Restored)?;
        }
        Ok(Some(restored))
    }

    pub fn is_noise(&self, path: &str) -> Result<bool> {
        let stored: Option<Noise> = self
            .database
            .r_transaction()?
            .get()
            .secondary(NoiseKey::path, path)?;
        Ok(stored.is_some())
    }

    /// The rejected paths as the walker reads them, so a scan skips what triage refused.
    pub fn noise_filter(&self) -> Result<NoiseFilter> {
        let read = self.database.r_transaction()?;
        let rows: Vec<Noise> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        Ok(NoiseFilter::from_paths(
            rows.into_iter().map(|noise| PathBuf::from(noise.path)),
        ))
    }
}

/// A symlink about to be recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct NewProjection {
    pub link_path: String,
    pub target_path: String,
    pub profile: String,
    pub artifact_id: Option<ArtifactId>,
}

/// One symlink hoard wrote, as PATHS ON DISK and a later reprojection read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectionRecord {
    pub id: i64,
    pub link_path: String,
    pub target_path: String,
    pub profile: String,
    pub artifact_id: Option<ArtifactId>,
    pub written_at: String,
}

impl From<Projection> for ProjectionRecord {
    fn from(projection: Projection) -> Self {
        Self {
            id: projection.id,
            link_path: projection.link_path,
            target_path: projection.target_path,
            profile: projection.profile,
            artifact_id: projection.artifact_id.map(ArtifactId::new),
            written_at: projection.written_at,
        }
    }
}

impl Catalog {
    /// Records every written symlink in one transaction, upserting by link path because a
    /// link that was written again replaced the one on disk. Returns the ids in order.
    pub fn record_projections(&self, projections: &[NewProjection]) -> Result<Vec<i64>> {
        let written_at = jiff::Timestamp::now().to_string();
        let transaction = self.database.rw_transaction()?;
        let mut ids = Vec::with_capacity(projections.len());
        for projection in projections {
            let stored: Option<Projection> = transaction
                .get()
                .secondary(ProjectionKey::link_path, projection.link_path.as_str())?;
            let id = match stored {
                Some(stored) => stored.id,
                None => next_id::<Projection>(&transaction)?,
            };
            transaction.upsert(Projection {
                id,
                link_path: projection.link_path.clone(),
                profile: projection.profile.clone(),
                target_path: projection.target_path.clone(),
                artifact_id: projection.artifact_id.map(ArtifactId::get),
                written_at: written_at.clone(),
            })?;
            ids.push(id);
        }
        transaction.commit()?;
        Ok(ids)
    }

    /// Every projection, or every projection of one profile, ordered by link path.
    pub fn list_projections(&self, profile: Option<&str>) -> Result<Vec<ProjectionRecord>> {
        let read = self.database.r_transaction()?;
        let mut rows: Vec<Projection> = read
            .scan()
            .secondary(ProjectionKey::link_path)?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        if let Some(profile) = profile {
            rows.retain(|row| row.profile == profile);
        }
        Ok(rows.into_iter().map(ProjectionRecord::from).collect())
    }

    pub fn projections_for_artifact(&self, id: ArtifactId) -> Result<Vec<ProjectionRecord>> {
        let mut rows = self.list_projections(None)?;
        rows.retain(|row| row.artifact_id == Some(id));
        Ok(rows)
    }

    pub fn remove_projection(&self, id: i64) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Projection> = transaction.get().primary(id)?;
        let removed = match stored {
            Some(projection) => {
                transaction.remove(projection)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(removed)
    }

    /// Drops every record of a profile, for when a projection directory is rewritten.
    pub fn clear_profile(&self, profile: &str) -> Result<u64> {
        let transaction = self.database.rw_transaction()?;
        let rows: Vec<Projection> = transaction
            .scan()
            .secondary(ProjectionKey::profile)?
            .start_with(profile)?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut removed = 0;
        for row in rows {
            if row.profile == profile {
                transaction.remove(row)?;
                removed += 1;
            }
        }
        transaction.commit()?;
        Ok(removed)
    }
}

/// Whether taking back one rejection is what un-rejects an artifact: it still has a record,
/// that record is rejected, and no other noise row names it. The noise table holds one row
/// per path a person rejected, so the scan is over what triage refused rather than the index.
fn restorable(transaction: &RwTransaction<'_>, artifact: i64) -> Result<bool> {
    let record: Option<ArtifactRecord> = transaction.get().primary(artifact)?;
    if !record.is_some_and(|record| record.rejected) {
        return Ok(false);
    }
    for row in transaction.scan().primary::<Noise>()?.all()? {
        let row: Noise = row?;
        if row.artifact_id == Some(artifact) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The primary key a record was given, so a fresh one can continue the sequence.
pub(super) trait Sequenced {
    fn sequence(&self) -> u64;
}

impl Sequenced for Shelf {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

impl Sequenced for Noise {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

impl Sequenced for Node {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

impl Sequenced for Projection {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

/// The next key of a record type that has no allocator: the highest key in use plus one,
/// read inside the writing transaction, which redb serialises against every other writer.
pub(super) fn next_key<T: ToInput + Sequenced>(transaction: &RwTransaction<'_>) -> Result<u64> {
    let last: Option<T> = transaction
        .scan()
        .primary::<T>()?
        .all()?
        .next_back()
        .transpose()?;
    Ok(last.map_or(1, |last| last.sequence() + 1))
}

pub(super) fn next_id<T: ToInput + Sequenced>(transaction: &RwTransaction<'_>) -> Result<i64> {
    let key = next_key::<T>(transaction)?;
    i64::try_from(key).map_err(|_| IndexError::Catalog(format!("catalog key {key} overflowed")))
}
