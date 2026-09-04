use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::ShelfId;

use crate::error::Result;
use crate::store::convert::{collect_rows, optional_row, required_row};

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

const SHELF_COLUMNS: &str = "id, name, query, builtin, position";

/// A saved query. Built-in shelves are seeded and keep their position.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShelfRow {
    pub id: ShelfId,
    pub name: String,
    pub query: String,
    pub builtin: bool,
    pub position: i64,
}

impl ShelfRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: ShelfId::new(row.get("id")?),
            name: row.get("name")?,
            query: row.get("query")?,
            builtin: row.get::<_, i64>("builtin")? != 0,
            position: row.get("position")?,
        })
    }
}

pub fn create_shelf(conn: &Connection, name: &str, query: &str, position: i64) -> Result<ShelfId> {
    let id = conn.query_row(
        "INSERT INTO shelf(name, query, builtin, position) VALUES(?1, ?2, 0, ?3) \
         ON CONFLICT(name) DO UPDATE SET query = excluded.query, position = excluded.position \
         RETURNING id",
        (name, query, position),
        |row| row.get::<_, i64>(0),
    )?;
    Ok(ShelfId::new(id))
}

pub fn update_shelf(
    conn: &Connection,
    id: ShelfId,
    name: &str,
    query: &str,
    position: i64,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE shelf SET name = ?2, query = ?3, position = ?4 WHERE id = ?1",
        (id.get(), name, query, position),
    )?;
    if updated == 0 {
        return Err(crate::error::IndexError::not_found("shelf", id.to_string()));
    }
    Ok(())
}

pub fn delete_shelf(conn: &Connection, id: ShelfId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM shelf WHERE id = ?1", [id.get()])? > 0)
}

pub fn shelf(conn: &Connection, id: ShelfId) -> Result<ShelfRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {SHELF_COLUMNS} FROM shelf WHERE id = ?1"),
            [id.get()],
            |row| Ok(ShelfRow::from_row(row)),
        ),
        "shelf",
        id,
    )
}

pub fn shelf_by_name(conn: &Connection, name: &str) -> Result<Option<ShelfRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {SHELF_COLUMNS} FROM shelf WHERE name = ?1"),
        [name],
        |row| Ok(ShelfRow::from_row(row)),
    ))
}

pub fn list_shelves(conn: &Connection) -> Result<Vec<ShelfRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {SHELF_COLUMNS} FROM shelf ORDER BY position, id"
    ))?;
    let rows = statement.query_map([], |row| Ok(ShelfRow::from_row(row)))?;
    collect_rows(rows)
}

/// Seeds the shelves the design ships with. Re-running it refreshes their queries and
/// order without duplicating them or disturbing user shelves.
pub fn seed_builtin_shelves(conn: &Connection) -> Result<usize> {
    let mut statement = conn.prepare(
        "INSERT INTO shelf(name, query, builtin, position) VALUES(?1, ?2, 1, ?3) \
         ON CONFLICT(name) DO UPDATE SET \
             query = excluded.query, builtin = 1, position = excluded.position",
    )?;
    for (position, (name, query)) in BUILTIN_SHELVES.iter().enumerate() {
        statement.execute((name, query, position as i64))?;
    }
    Ok(BUILTIN_SHELVES.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn seeding_is_idempotent_and_yields_the_six_designed_shelves() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        assert_eq!(seed_builtin_shelves(&conn).expect("shelves seed"), 6);
        seed_builtin_shelves(&conn).expect("shelves re-seed");

        let shelves = list_shelves(&conn).expect("shelves list");
        assert_eq!(shelves.len(), 6);
        assert!(shelves.iter().all(|shelf| shelf.builtin));
        assert_eq!(shelves[0].name, "sdxl \u{b7} lora \u{b7} small");
        assert_eq!(shelves[0].query, "role:lora base:sdxl size:<400mb");
        assert_eq!(shelves[5].query, "node:laptop");
        assert_eq!(
            shelves
                .iter()
                .map(|shelf| shelf.position)
                .collect::<Vec<i64>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn user_shelves_are_created_updated_and_deleted() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        seed_builtin_shelves(&conn).expect("shelves seed");
        let id = create_shelf(&conn, "wan video", "base:wan", 10).expect("shelf inserts");

        update_shelf(&conn, id, "wan video", "base:wan role:ckpt", 9).expect("shelf updates");
        let row = shelf(&conn, id).expect("shelf reads");
        assert_eq!(row.query, "base:wan role:ckpt");
        assert_eq!(row.position, 9);
        assert!(!row.builtin);
        assert_eq!(
            shelf_by_name(&conn, "wan video").expect("lookup"),
            Some(row)
        );
        assert_eq!(list_shelves(&conn).expect("shelves list").len(), 7);

        assert!(delete_shelf(&conn, id).expect("shelf deletes"));
        assert!(!delete_shelf(&conn, id).expect("deleting twice is a no-op"));
        assert!(update_shelf(&conn, id, "gone", "role:unk", 0).is_err());
        assert_eq!(list_shelves(&conn).expect("shelves list").len(), 6);
    }
}
