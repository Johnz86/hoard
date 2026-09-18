use jiff::Timestamp;
use serde::Serialize;

use hoard_core::{HoardError, Loadout, LoadoutId, LoadoutPin};

use crate::error::{IndexError, Result};
use crate::row::SearchRow;
use crate::time::{now, parse_timestamp, timestamp_text};

use super::Catalog;
use super::models::{LoadoutDoc, LoadoutDocKey};
use super::userdata::{Sequenced, next_id};

/// A stored loadout. The document is the TOML the user edits and exports; the parsed form
/// is recovered with [`LoadoutRecord::parse`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutRecord {
    pub id: LoadoutId,
    pub name: String,
    pub document: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl LoadoutRecord {
    pub fn parse(&self) -> Result<Loadout> {
        Ok(Loadout::parse(&self.document)?)
    }
}

impl TryFrom<LoadoutDoc> for LoadoutRecord {
    type Error = IndexError;

    fn try_from(stored: LoadoutDoc) -> Result<Self> {
        Ok(Self {
            id: LoadoutId::new(stored.id),
            name: stored.name,
            document: stored.document,
            created_at: parse_timestamp(&stored.created_at)?,
            updated_at: parse_timestamp(&stored.updated_at)?,
        })
    }
}

impl Sequenced for LoadoutDoc {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

fn name_taken(name: &str) -> IndexError {
    IndexError::Domain(HoardError::InvalidLoadout(format!(
        "a loadout named {name:?} already exists"
    )))
}

impl Catalog {
    /// Stores a new loadout under the name its document carries. A name another loadout
    /// already holds is refused with [`HoardError::InvalidLoadout`], never merged.
    pub fn create_loadout(&self, loadout: &Loadout) -> Result<LoadoutId> {
        let document = loadout.to_toml()?;
        let at = timestamp_text(now());
        let transaction = self.database.rw_transaction()?;
        let taken: Option<LoadoutDoc> = transaction
            .get()
            .secondary(LoadoutDocKey::name, loadout.name.as_str())?;
        if taken.is_some() {
            return Err(name_taken(&loadout.name));
        }
        let id = next_id::<LoadoutDoc>(&transaction)?;
        transaction.insert(LoadoutDoc {
            id,
            name: loadout.name.clone(),
            document,
            created_at: at.clone(),
            updated_at: at,
        })?;
        transaction.commit()?;
        Ok(LoadoutId::new(id))
    }

    /// Gives one loadout a new name, inside its document as well as on its row. A name
    /// another loadout holds is refused with [`HoardError::InvalidLoadout`]; the loadout's
    /// own current name is accepted and changes nothing but the update time.
    pub fn rename_loadout(&self, id: LoadoutId, name: &str) -> Result<LoadoutRecord> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<LoadoutDoc> = transaction.get().primary(id.get())?;
        let stored = stored.ok_or_else(|| IndexError::not_found("loadout", id.to_string()))?;
        let taken: Option<LoadoutDoc> = transaction.get().secondary(LoadoutDocKey::name, name)?;
        if taken.is_some_and(|other| other.id != stored.id) {
            return Err(name_taken(name));
        }
        let mut document = Loadout::parse(&stored.document)?;
        document.name = name.to_owned();
        let renamed = LoadoutDoc {
            name: name.to_owned(),
            document: document.to_toml()?,
            updated_at: timestamp_text(now()),
            ..stored
        };
        transaction.upsert(renamed.clone())?;
        transaction.commit()?;
        LoadoutRecord::try_from(renamed)
    }

    /// Stores a loadout under the name its document carries, keeping the id and creation
    /// time a loadout of that name already had.
    pub fn upsert_loadout(&self, loadout: &Loadout) -> Result<LoadoutId> {
        let document = loadout.to_toml()?;
        let at = timestamp_text(now());
        let transaction = self.database.rw_transaction()?;
        let stored: Option<LoadoutDoc> = transaction
            .get()
            .secondary(LoadoutDocKey::name, loadout.name.as_str())?;
        let (id, created_at) = match stored {
            Some(stored) => (stored.id, stored.created_at),
            None => (next_id::<LoadoutDoc>(&transaction)?, at.clone()),
        };
        transaction.upsert(LoadoutDoc {
            id,
            name: loadout.name.clone(),
            document,
            created_at,
            updated_at: at,
        })?;
        transaction.commit()?;
        Ok(LoadoutId::new(id))
    }

    /// Rewrites one loadout's document, leaving its name and id alone.
    pub fn update_loadout(&self, id: LoadoutId, document: &str) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<LoadoutDoc> = transaction.get().primary(id.get())?;
        let stored = stored.ok_or_else(|| IndexError::not_found("loadout", id.to_string()))?;
        transaction.upsert(LoadoutDoc {
            document: document.to_owned(),
            updated_at: timestamp_text(now()),
            ..stored
        })?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_loadout(&self, id: LoadoutId) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<LoadoutDoc> = transaction.get().primary(id.get())?;
        let deleted = match stored {
            Some(stored) => {
                transaction.remove(stored)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn loadout(&self, id: LoadoutId) -> Result<Option<LoadoutRecord>> {
        let stored: Option<LoadoutDoc> = self.database.r_transaction()?.get().primary(id.get())?;
        stored.map(LoadoutRecord::try_from).transpose()
    }

    pub fn loadout_by_name(&self, name: &str) -> Result<Option<LoadoutRecord>> {
        let stored: Option<LoadoutDoc> = self
            .database
            .r_transaction()?
            .get()
            .secondary(LoadoutDocKey::name, name)?;
        stored.map(LoadoutRecord::try_from).transpose()
    }

    /// Every loadout, ordered by name, which is the order the drawer lists them.
    pub fn list_loadouts(&self) -> Result<Vec<LoadoutRecord>> {
        let read = self.database.r_transaction()?;
        let rows: Vec<LoadoutDoc> = read
            .scan()
            .secondary(LoadoutDocKey::name)?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        rows.into_iter().map(LoadoutRecord::try_from).collect()
    }

    /// The rows a pin can stand for: every entry row of the artifact with the pin's
    /// sha256, else the rows whose repository and path match.
    pub fn resolve_pin(&self, pin: &LoadoutPin) -> Result<Vec<SearchRow>> {
        if let Some(sha256) = pin.sha256 {
            let rows = self.artifact_rows_by_sha256(&sha256)?;
            if !rows.is_empty() {
                return Ok(rows);
            }
        }
        self.rows_by_repo_path(&pin.repo, &pin.path)
    }
}
