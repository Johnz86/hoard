use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::store::convert::{collect_rows, optional_row};

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    optional_row(
        conn.query_row("SELECT value FROM setting WHERE key = ?1", [key], |row| {
            Ok(row.get::<_, String>(0).map_err(Into::into))
        }),
    )
}

/// Writes one setting, replacing any previous value.
pub fn put_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO setting(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

pub fn remove_setting(conn: &Connection, key: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM setting WHERE key = ?1", [key])? > 0)
}

pub fn all_settings(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut statement = conn.prepare("SELECT key, value FROM setting ORDER BY key")?;
    let rows = statement.query_map([], |row| {
        Ok((|| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })())
    })?;
    Ok(collect_rows(rows)?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn settings_round_trip_and_replace() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        assert_eq!(get_setting(&conn, "density").expect("lookup"), None);

        put_setting(&conn, "density", "compact").expect("setting writes");
        put_setting(&conn, "density", "dense").expect("setting replaces");
        put_setting(&conn, "theme", "dark").expect("setting writes");
        assert_eq!(
            get_setting(&conn, "density").expect("lookup").as_deref(),
            Some("dense")
        );

        let all = all_settings(&conn).expect("settings read");
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("theme").map(String::as_str), Some("dark"));
        assert!(remove_setting(&conn, "theme").expect("setting removes"));
        assert!(!remove_setting(&conn, "theme").expect("removing twice is a no-op"));
    }
}
