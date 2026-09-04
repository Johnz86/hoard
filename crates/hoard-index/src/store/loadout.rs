use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::{Loadout, LoadoutId};

use crate::error::Result;
use crate::store::convert::{
    collect_rows, now, optional_row, parse_timestamp, required_row, timestamp_text,
};

const LOADOUT_COLUMNS: &str = "id, name, document, created_at, updated_at";

/// A stored loadout. The document is the TOML the user edits and exports; the parsed form
/// is recovered with [`LoadoutRow::parse`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutRow {
    pub id: LoadoutId,
    pub name: String,
    pub document: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl LoadoutRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: LoadoutId::new(row.get("id")?),
            name: row.get("name")?,
            document: row.get("document")?,
            created_at: parse_timestamp(&row.get::<_, String>("created_at")?)?,
            updated_at: parse_timestamp(&row.get::<_, String>("updated_at")?)?,
        })
    }

    /// Parses the stored document into the domain loadout.
    pub fn parse(&self) -> Result<Loadout> {
        Ok(Loadout::parse(&self.document)?)
    }
}

pub fn create_loadout(conn: &Connection, name: &str, document: &str) -> Result<LoadoutId> {
    let at = timestamp_text(now());
    let id = conn.query_row(
        "INSERT INTO loadout(name, document, created_at, updated_at) VALUES(?1, ?2, ?3, ?3) \
         ON CONFLICT(name) DO UPDATE SET document = excluded.document, updated_at = excluded.updated_at \
         RETURNING id",
        (name, document, at),
        |row| row.get::<_, i64>(0),
    )?;
    Ok(LoadoutId::new(id))
}

/// Stores a loadout by its own name, which is how import and the drawer both save.
pub fn upsert_loadout(conn: &Connection, loadout: &Loadout) -> Result<LoadoutId> {
    create_loadout(conn, &loadout.name, &loadout.to_toml()?)
}

pub fn update_loadout(conn: &Connection, id: LoadoutId, document: &str) -> Result<()> {
    let updated = conn.execute(
        "UPDATE loadout SET document = ?2, updated_at = ?3 WHERE id = ?1",
        (id.get(), document, timestamp_text(now())),
    )?;
    if updated == 0 {
        return Err(crate::error::IndexError::not_found(
            "loadout",
            id.to_string(),
        ));
    }
    Ok(())
}

pub fn delete_loadout(conn: &Connection, id: LoadoutId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM loadout WHERE id = ?1", [id.get()])? > 0)
}

pub fn loadout(conn: &Connection, id: LoadoutId) -> Result<LoadoutRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {LOADOUT_COLUMNS} FROM loadout WHERE id = ?1"),
            [id.get()],
            |row| Ok(LoadoutRow::from_row(row)),
        ),
        "loadout",
        id,
    )
}

pub fn loadout_by_name(conn: &Connection, name: &str) -> Result<Option<LoadoutRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {LOADOUT_COLUMNS} FROM loadout WHERE name = ?1"),
        [name],
        |row| Ok(LoadoutRow::from_row(row)),
    ))
}

pub fn list_loadouts(conn: &Connection) -> Result<Vec<LoadoutRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {LOADOUT_COLUMNS} FROM loadout ORDER BY name"
    ))?;
    let rows = statement.query_map([], |row| Ok(LoadoutRow::from_row(row)))?;
    collect_rows(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use hoard_core::{LoadoutPin, RepoId, RepoType, Role};

    fn document() -> Loadout {
        let mut loadout = Loadout::new("night shoot");
        let repo = RepoId::parse_ref(RepoType::Models, "org/sdxl").expect("reference parses");
        loadout
            .add(Role::Ckpt, LoadoutPin::new(repo, "unet/model.safetensors"))
            .expect("host pins");
        loadout
    }

    #[test]
    fn a_loadout_round_trips_through_its_document() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = upsert_loadout(&conn, &document()).expect("loadout stores");

        let row = loadout(&conn, id).expect("loadout reads");
        assert_eq!(row.name, "night shoot");
        assert_eq!(row.parse().expect("document parses"), document());
        assert_eq!(row.created_at, row.updated_at);
        assert_eq!(
            loadout_by_name(&conn, "night shoot").expect("lookup"),
            Some(row)
        );
    }

    #[test]
    fn loadouts_update_by_name_and_by_id() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = upsert_loadout(&conn, &document()).expect("loadout stores");
        assert_eq!(
            upsert_loadout(&conn, &document()).expect("loadout upserts"),
            id
        );

        let mut edited = document();
        edited.vae = Some(LoadoutPin::new(
            RepoId::parse_ref(RepoType::Models, "org/vae").expect("reference parses"),
            "vae.safetensors",
        ));
        update_loadout(&conn, id, &edited.to_toml().expect("document renders"))
            .expect("loadout updates");
        assert_eq!(
            loadout(&conn, id)
                .expect("loadout reads")
                .parse()
                .expect("parses"),
            edited
        );
        assert_eq!(list_loadouts(&conn).expect("loadouts list").len(), 1);

        assert!(delete_loadout(&conn, id).expect("loadout deletes"));
        assert!(!delete_loadout(&conn, id).expect("deleting twice is a no-op"));
        assert!(update_loadout(&conn, id, "name = 'gone'").is_err());
    }
}
