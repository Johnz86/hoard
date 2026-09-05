use std::path::{Path, PathBuf};

use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::SourceId;

use crate::config::SourceKind;
use crate::error::{IndexError, Result};
use crate::store::convert::{
    collect_rows, optional_row, parse_opt_timestamp, required_row, timestamp_text, to_u64,
};

const SOURCE_COLUMNS: &str =
    "id, name, path, kind, node_id, file_count, blob_count, byte_count, last_scan";

/// A nominated root on one node, with what the last scan of it found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceRow {
    pub id: SourceId,
    pub name: String,
    pub path: PathBuf,
    pub kind: SourceKind,
    pub node_row: i64,
    pub file_count: u64,
    pub blob_count: u64,
    pub byte_count: u64,
    pub last_scan: Option<Timestamp>,
}

impl SourceRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: SourceId::new(row.get("id")?),
            name: row.get("name")?,
            path: PathBuf::from(row.get::<_, String>("path")?),
            kind: source_kind(&row.get::<_, String>("kind")?)?,
            node_row: row.get("node_id")?,
            file_count: to_u64(row.get::<_, i64>("file_count")?),
            blob_count: to_u64(row.get::<_, i64>("blob_count")?),
            byte_count: to_u64(row.get::<_, i64>("byte_count")?),
            last_scan: parse_opt_timestamp(row.get::<_, Option<String>>("last_scan")?)?,
        })
    }
}

/// What one scan of a source counted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ScanStats {
    pub file_count: u64,
    pub blob_count: u64,
    pub byte_count: u64,
    pub at: Timestamp,
}

/// Reads back the stored spelling of a source layout.
pub(crate) fn source_kind(raw: &str) -> Result<SourceKind> {
    match raw {
        "hf-cache" => Ok(SourceKind::HfCache),
        "git" => Ok(SourceKind::Git),
        "loose" => Ok(SourceKind::Loose),
        "auto" => Ok(SourceKind::Auto),
        other => Err(IndexError::Config(format!("unknown source kind {other:?}"))),
    }
}

/// Inserts a source or refreshes its path and kind, returning its id.
pub fn upsert_source(
    conn: &Connection,
    name: &str,
    path: &Path,
    kind: SourceKind,
    node_row: i64,
) -> Result<SourceId> {
    let id = conn.query_row(
        "INSERT INTO source(name, path, kind, node_id) VALUES(?1, ?2, ?3, ?4) \
         ON CONFLICT(name) DO UPDATE SET \
             path = excluded.path, kind = excluded.kind, node_id = excluded.node_id \
         RETURNING id",
        (name, path.to_string_lossy(), kind.as_str(), node_row),
        |row| row.get::<_, i64>(0),
    )?;
    Ok(SourceId::new(id))
}

pub fn list_sources(conn: &Connection) -> Result<Vec<SourceRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {SOURCE_COLUMNS} FROM source ORDER BY name"
    ))?;
    let rows = statement.query_map([], |row| Ok(SourceRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn source_by_name(conn: &Connection, name: &str) -> Result<Option<SourceRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {SOURCE_COLUMNS} FROM source WHERE name = ?1"),
        [name],
        |row| Ok(SourceRow::from_row(row)),
    ))
}

pub fn source(conn: &Connection, id: SourceId) -> Result<SourceRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {SOURCE_COLUMNS} FROM source WHERE id = ?1"),
            [id.get()],
            |row| Ok(SourceRow::from_row(row)),
        ),
        "source",
        id,
    )
}

pub fn record_scan_stats(conn: &Connection, id: SourceId, stats: &ScanStats) -> Result<()> {
    conn.execute(
        "UPDATE source SET file_count = ?2, blob_count = ?3, byte_count = ?4, last_scan = ?5 \
         WHERE id = ?1",
        (
            id.get(),
            stats.file_count as i64,
            stats.blob_count as i64,
            stats.byte_count as i64,
            timestamp_text(stats.at),
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::node::{NewNode, upsert_node};
    use hoard_core::NodeId;

    fn store_with_node() -> (Store, i64) {
        let store = Store::open_in_memory().expect("store opens");
        let node_row = {
            let conn = store.write().expect("writer");
            let node_id = NodeId::new("workstation-1").expect("node id");
            upsert_node(&conn, &NewNode::local(node_id, "workstation")).expect("node inserts")
        };
        (store, node_row)
    }

    #[test]
    fn sources_upsert_by_name_and_record_their_scan() {
        let (store, node_row) = store_with_node();
        let conn = store.write().expect("writer");
        let id = upsert_source(
            &conn,
            "cache",
            Path::new("/data/hub"),
            SourceKind::HfCache,
            node_row,
        )
        .expect("source inserts");
        let again = upsert_source(
            &conn,
            "cache",
            Path::new("/data/hub2"),
            SourceKind::Git,
            node_row,
        )
        .expect("source upserts");
        assert_eq!(id, again);

        let stats = ScanStats {
            file_count: 12,
            blob_count: 9,
            byte_count: 4096,
            at: "2026-09-04T09:10:00Z".parse().expect("timestamp"),
        };
        record_scan_stats(&conn, id, &stats).expect("stats record");

        let row = source(&conn, id).expect("source reads");
        assert_eq!(row.path, PathBuf::from("/data/hub2"));
        assert_eq!(row.kind, SourceKind::Git);
        assert_eq!(row.file_count, 12);
        assert_eq!(row.byte_count, 4096);
        assert_eq!(row.last_scan, Some(stats.at));
        assert_eq!(list_sources(&conn).expect("list").len(), 1);
        assert!(source_by_name(&conn, "missing").expect("lookup").is_none());
    }

    #[test]
    fn every_source_kind_round_trips_its_spelling() {
        for kind in [
            SourceKind::HfCache,
            SourceKind::Git,
            SourceKind::Loose,
            SourceKind::Auto,
        ] {
            assert_eq!(source_kind(kind.as_str()).expect("kind parses"), kind);
        }
        assert!(source_kind("tape").is_err());
    }
}
