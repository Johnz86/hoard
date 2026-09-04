use rusqlite::Connection;
use rusqlite_migration::{M, Migrations, SchemaVersion};

use crate::error::Result;
use crate::store::SCHEMA_SQL;

fn migrations() -> Migrations<'static> {
    Migrations::from_iter([M::up(SCHEMA_SQL)])
}

/// Applies every pending migration. Safe to call on an already current database.
pub fn apply(conn: &mut Connection) -> Result<()> {
    migrations().to_latest(conn)?;
    Ok(())
}

/// The `user_version` the database currently carries, zero before any migration ran.
pub fn schema_version(conn: &Connection) -> Result<usize> {
    let version = migrations().current_version(conn)?;
    Ok(match version {
        SchemaVersion::NoneSet => 0,
        SchemaVersion::Inside(value) | SchemaVersion::Outside(value) => value.get(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn the_schema_validates_and_applies_once() {
        migrations()
            .validate()
            .expect("schema is a valid migration set");
        let store = Store::open_in_memory().expect("store opens");
        let mut conn = store.write().expect("writer");
        assert_eq!(schema_version(&conn).expect("version"), 1);
        apply(&mut conn).expect("re-applying is a no-op");
        assert_eq!(schema_version(&conn).expect("version"), 1);
    }

    #[test]
    fn a_reopened_database_keeps_its_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("hoard.db");
        let first = Store::open(&path).expect("store opens");
        drop(first);
        let second = Store::open(&path).expect("store reopens");
        let conn = second.read().expect("reader");
        assert_eq!(schema_version(&conn).expect("version"), 1);
    }

    #[test]
    fn the_applied_schema_matches_its_snapshot() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.read().expect("reader");
        let mut statement = conn
            .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY name")
            .expect("sqlite_master query prepares");
        let rows: Vec<String> = statement
            .query_map([], |row| {
                let kind: String = row.get(0)?;
                let name: String = row.get(1)?;
                let sql: Option<String> = row.get(2)?;
                Ok(format!("{kind} {name}\n{}", sql.unwrap_or_default()))
            })
            .expect("rows map")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("rows read");
        insta::assert_snapshot!("applied_schema", rows.join("\n\n"));
    }
}
