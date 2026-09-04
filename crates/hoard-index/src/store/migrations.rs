use rusqlite::Connection;
use rusqlite_migration::{M, Migrations, SchemaVersion};

use crate::error::Result;
use crate::store::{SCHEMA_SQL, SCHEMA_V2_SQL};

/// The schema version this binary expects a database to carry.
pub const LATEST_VERSION: usize = 2;

fn migrations() -> Migrations<'static> {
    Migrations::from_iter([M::up(SCHEMA_SQL), M::up(SCHEMA_V2_SQL)])
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
        assert_eq!(schema_version(&conn).expect("version"), LATEST_VERSION);
        apply(&mut conn).expect("re-applying is a no-op");
        assert_eq!(schema_version(&conn).expect("version"), LATEST_VERSION);
    }

    #[test]
    fn a_reopened_database_keeps_its_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("hoard.db");
        let first = Store::open(&path).expect("store opens");
        drop(first);
        let second = Store::open(&path).expect("store reopens");
        let conn = second.read().expect("reader");
        assert_eq!(schema_version(&conn).expect("version"), LATEST_VERSION);
    }

    /// One artifact reachable through one entry, written the way migration 1 spelled it.
    const SEED_V1: &str = "\
        INSERT INTO node(id, node_id, name, is_self) VALUES(1, 'workstation-1', 'box', 1); \
        INSERT INTO blob(id, fingerprint, size, first_seen) \
            VALUES(1, 'fp', 2048, '2026-01-01T00:00:00Z'); \
        INSERT INTO artifact(id, blob_id, name, role, created_at) \
            VALUES(1, 1, 'film-grain', 'lora', '2026-01-01T00:00:00Z'); \
        INSERT INTO repo(id, qualified, repo_type, name, node_id, kind) \
            VALUES(1, 'models/kohya/film-grain', 'models', 'film-grain', 1, 'hf-cache'); \
        INSERT INTO revision(id, repo_id, sha) VALUES(1, 1, 'abcdef'); \
        INSERT INTO entry(id, revision_id, path, blob_id, artifact_id, node_id, size, last_seen) \
            VALUES(1, 1, 'film-grain.safetensors', 1, 1, 1, 2048, '2026-01-01T00:00:00Z');";

    fn row(conn: &Connection) -> (String, i64, i64) {
        conn.query_row(
            "SELECT name, size, shard_count FROM search_row",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the view reads the migrated rows")
    }

    #[test]
    fn the_shard_columns_land_on_a_database_migration_one_built() {
        let mut conn = Connection::open_in_memory().expect("connection opens");
        Migrations::from_iter([M::up(SCHEMA_SQL)])
            .to_latest(&mut conn)
            .expect("migration one applies");
        conn.execute_batch(SEED_V1).expect("the seed rows insert");

        apply(&mut conn).expect("migration two applies");

        assert_eq!(schema_version(&conn).expect("version"), LATEST_VERSION);
        assert_eq!(row(&conn), ("film-grain".to_owned(), 2048, 1));
        conn.execute(
            "UPDATE artifact SET group_bytes = 8192, shard_count = 4 WHERE id = 1",
            [],
        )
        .expect("the new columns are writable");
        assert_eq!(row(&conn), ("film-grain".to_owned(), 8192, 4));
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
