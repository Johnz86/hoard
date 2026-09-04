//! Symlinks written by `hoard link` and `hoard apply`.
//!
//! The filesystem stays authoritative for the bytes, but a projection is hoard's own
//! writing, so it is tracked: PATHS ON DISK can then say which paths are real files and
//! which are links this node made, and a reprojection can replace them.

use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::ArtifactId;

use crate::error::Result;
use crate::store::convert::{collect_rows, now, parse_timestamp, timestamp_text, to_u64};

const PROJECTION_COLUMNS: &str = "id, link_path, target_path, profile, artifact_id, written_at";

/// One symlink hoard wrote into a tool's layout.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectionRow {
    pub id: i64,
    pub link_path: String,
    pub target_path: String,
    pub profile: String,
    pub artifact_id: Option<ArtifactId>,
    pub written_at: Timestamp,
}

impl ProjectionRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            link_path: row.get("link_path")?,
            target_path: row.get("target_path")?,
            profile: row.get("profile")?,
            artifact_id: row
                .get::<_, Option<i64>>("artifact_id")?
                .map(ArtifactId::new),
            written_at: parse_timestamp(&row.get::<_, String>("written_at")?)?,
        })
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

/// Records a written symlink. Writing the same link again replaces the record, because the
/// link on disk was replaced too.
pub fn record_projection(conn: &Connection, projection: &NewProjection) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO projection(link_path, target_path, profile, artifact_id, written_at) \
         VALUES(?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(link_path) DO UPDATE SET \
             target_path = excluded.target_path, \
             profile = excluded.profile, \
             artifact_id = excluded.artifact_id, \
             written_at = excluded.written_at \
         RETURNING id",
        (
            &projection.link_path,
            &projection.target_path,
            &projection.profile,
            projection.artifact_id.map(ArtifactId::get),
            timestamp_text(now()),
        ),
        |row| row.get(0),
    )?;
    Ok(id)
}

pub fn list_projections(conn: &Connection, profile: Option<&str>) -> Result<Vec<ProjectionRow>> {
    match profile {
        Some(profile) => {
            let sql = format!(
                "SELECT {PROJECTION_COLUMNS} FROM projection WHERE profile = ?1 ORDER BY link_path"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map([profile], |row| Ok(ProjectionRow::from_row(row)))?;
            collect_rows(rows)
        }
        None => {
            let sql = format!("SELECT {PROJECTION_COLUMNS} FROM projection ORDER BY link_path");
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map([], |row| Ok(ProjectionRow::from_row(row)))?;
            collect_rows(rows)
        }
    }
}

pub fn projections_for_artifact(
    conn: &Connection,
    artifact: ArtifactId,
) -> Result<Vec<ProjectionRow>> {
    let sql = format!(
        "SELECT {PROJECTION_COLUMNS} FROM projection WHERE artifact_id = ?1 ORDER BY link_path"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([artifact.get()], |row| Ok(ProjectionRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn remove_projection(conn: &Connection, link_path: &str) -> Result<bool> {
    let removed = conn.execute("DELETE FROM projection WHERE link_path = ?1", [link_path])?;
    Ok(removed > 0)
}

/// Drops every record for a profile, for when a projection directory is rewritten.
pub fn clear_profile(conn: &Connection, profile: &str) -> Result<u64> {
    let removed = conn.execute("DELETE FROM projection WHERE profile = ?1", [profile])?;
    Ok(to_u64(removed as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn projection(link: &str, target: &str) -> NewProjection {
        NewProjection {
            link_path: link.to_owned(),
            target_path: target.to_owned(),
            profile: "comfyui".to_owned(),
            artifact_id: None,
        }
    }

    #[test]
    fn projections_round_trip_and_replace_by_link_path() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");

        let first = record_projection(
            &conn,
            &projection("/models/loras/a.safetensors", "/vault/a"),
        )
        .expect("recorded");
        let again = record_projection(
            &conn,
            &projection("/models/loras/a.safetensors", "/vault/moved"),
        )
        .expect("re-recorded");
        assert_eq!(first, again, "writing the same link updates one row");

        record_projection(
            &conn,
            &projection("/models/loras/b.safetensors", "/vault/b"),
        )
        .expect("recorded");

        let rows = list_projections(&conn, Some("comfyui")).expect("listed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].target_path, "/vault/moved");
        assert!(
            list_projections(&conn, Some("a1111"))
                .expect("listed")
                .is_empty()
        );

        assert!(remove_projection(&conn, "/models/loras/a.safetensors").expect("removed"));
        assert!(!remove_projection(&conn, "/models/loras/a.safetensors").expect("already gone"));
        assert_eq!(clear_profile(&conn, "comfyui").expect("cleared"), 1);
        assert!(list_projections(&conn, None).expect("listed").is_empty());
    }
}
