use std::collections::BTreeMap;
use std::path::PathBuf;

use native_db::ToInput;
use native_db::transaction::RwTransaction;
use serde::Serialize;

use hoard_core::ShelfId;

use crate::discover::NoiseFilter;
use crate::error::{IndexError, Result};

use super::Catalog;
use super::models::{Noise, NoiseKey, Setting, Shelf, ShelfKey};

/// The shelves hoard ships with, in the order the rail lists them.
const BUILTIN_SHELVES: [(&str, &str); 6] = [
    (
        "sdxl \u{b7} lora \u{b7} small",
        "role:lora base:sdxl size:<400mb",
    ),
    ("flux workhorse", "base:flux"),
    ("quantised llm", "role:llm fmt:gguf"),
    ("cold + duplicate", "dupe:true"),
    ("triage queue", "role:?"),
    ("everything on laptop", "node:laptop"),
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
}

impl From<Noise> for NoiseRecord {
    fn from(noise: Noise) -> Self {
        Self {
            id: noise.id,
            path: noise.path,
            reason: noise.reason,
            added_at: noise.added_at,
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

    /// Upserts the six shelves hoard ships with, by name. Idempotent; run on every open.
    pub fn seed_builtin_shelves(&self) -> Result<usize> {
        let transaction = self.database.rw_transaction()?;
        for (position, (name, query)) in BUILTIN_SHELVES.iter().enumerate() {
            let stored: Option<Shelf> = transaction.get().secondary(ShelfKey::name, *name)?;
            let id = match stored {
                Some(shelf) => shelf.id,
                None => next_id::<Shelf>(&transaction)?,
            };
            transaction.upsert(Shelf {
                id,
                name: (*name).to_owned(),
                query: (*query).to_owned(),
                builtin: true,
                position: position as i64,
            })?;
        }
        transaction.commit()?;
        Ok(BUILTIN_SHELVES.len())
    }

    /// Marks a path as noise. Recording the same path again keeps its id, and keeps the
    /// reason it already carries when this one names none.
    pub fn add_noise(&self, path: &str, reason: Option<&str>) -> Result<i64> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Noise> = transaction.get().secondary(NoiseKey::path, path)?;
        let noise = match stored {
            Some(noise) => Noise {
                reason: reason.map(str::to_owned).or(noise.reason),
                ..noise
            },
            None => Noise {
                id: next_id::<Noise>(&transaction)?,
                path: path.to_owned(),
                reason: reason.map(str::to_owned),
                added_at: jiff::Timestamp::now().to_string(),
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

    pub fn remove_noise(&self, id: i64) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Noise> = transaction.get().primary(id)?;
        let removed = match stored {
            Some(noise) => {
                transaction.remove(noise)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(removed)
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
