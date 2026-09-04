use jiff::Timestamp;
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use hoard_core::{ArtifactId, BlobId, CommitSha, EntryId, RepoRowId, RevisionId, SourceId, State};

use crate::error::Result;
use crate::store::blob::{refresh_all_path_counts, refresh_path_count};
use crate::store::convert::{
    collect_rows, optional_row, parse_enum, parse_timestamp, timestamp_text, to_u64,
};

const ENTRY_COLUMNS: &str = "e.id AS id, e.revision_id AS revision_id, v.sha AS revision_sha, \
     v.repo_id AS repo_id, r.qualified AS repo_qualified, e.path AS path, e.blob_id AS blob_id, \
     e.artifact_id AS artifact_id, e.node_id AS node_id, n.name AS node_name, \
     e.source_id AS source_id, e.abs_path AS abs_path, e.size AS size, \
     e.is_symlink AS is_symlink, e.lfs_pointer AS lfs_pointer, e.state AS state, \
     e.pull_progress AS pull_progress, e.present AS present, e.mtime AS mtime, \
     e.inode AS inode, e.last_seen AS last_seen";

const ENTRY_FROM: &str = "FROM entry e JOIN revision v ON v.id = e.revision_id \
     JOIN repo r ON r.id = v.repo_id JOIN node n ON n.id = e.node_id";

/// One path inside a revision, on one node, pointing at the bytes it holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryRow {
    pub id: EntryId,
    pub revision: RevisionId,
    pub revision_sha: CommitSha,
    pub repo: RepoRowId,
    pub repo_qualified: String,
    pub path: String,
    pub blob: Option<BlobId>,
    pub artifact: Option<ArtifactId>,
    pub node_row: i64,
    pub node: String,
    pub source: Option<SourceId>,
    pub abs_path: Option<String>,
    pub size: u64,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub present: bool,
    pub mtime: Option<i64>,
    pub inode: Option<i64>,
    pub last_seen: Timestamp,
}

impl EntryRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: EntryId::new(row.get("id")?),
            revision: RevisionId::new(row.get("revision_id")?),
            revision_sha: parse_enum(&row.get::<_, String>("revision_sha")?)?,
            repo: RepoRowId::new(row.get("repo_id")?),
            repo_qualified: row.get("repo_qualified")?,
            path: row.get("path")?,
            blob: row.get::<_, Option<i64>>("blob_id")?.map(BlobId::new),
            artifact: row
                .get::<_, Option<i64>>("artifact_id")?
                .map(ArtifactId::new),
            node_row: row.get("node_id")?,
            node: row.get("node_name")?,
            source: row.get::<_, Option<i64>>("source_id")?.map(SourceId::new),
            abs_path: row.get("abs_path")?,
            size: to_u64(row.get::<_, i64>("size")?),
            is_symlink: row.get::<_, i64>("is_symlink")? != 0,
            lfs_pointer: row.get::<_, i64>("lfs_pointer")? != 0,
            state: parse_enum(&row.get::<_, String>("state")?)?,
            pull_progress: row
                .get::<_, Option<f64>>("pull_progress")?
                .map(|value| value as f32),
            present: row.get::<_, i64>("present")? != 0,
            mtime: row.get("mtime")?,
            inode: row.get("inode")?,
            last_seen: parse_timestamp(&row.get::<_, String>("last_seen")?)?,
        })
    }
}

/// A path as one scan saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEntry {
    pub revision: RevisionId,
    pub path: String,
    pub node_row: i64,
    pub blob: Option<BlobId>,
    pub artifact: Option<ArtifactId>,
    pub source: Option<SourceId>,
    pub abs_path: Option<String>,
    pub size: u64,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub present: bool,
    pub mtime: Option<i64>,
    pub inode: Option<i64>,
}

impl NewEntry {
    pub fn new(revision: RevisionId, path: impl Into<String>, node_row: i64) -> Self {
        Self {
            revision,
            path: path.into(),
            node_row,
            blob: None,
            artifact: None,
            source: None,
            abs_path: None,
            size: 0,
            is_symlink: false,
            lfs_pointer: false,
            state: State::Local,
            pull_progress: None,
            present: true,
            mtime: None,
            inode: None,
        }
    }
}

/// Inserts or refreshes a path, then recounts the paths reaching its bytes.
pub fn upsert_entry(conn: &Connection, input: &NewEntry, seen: Timestamp) -> Result<EntryId> {
    let id: i64 = conn.query_row(
        "INSERT INTO entry(revision_id, path, blob_id, artifact_id, node_id, source_id, \
             abs_path, size, is_symlink, lfs_pointer, state, pull_progress, present, mtime, \
             inode, last_seen) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
         ON CONFLICT(revision_id, path, node_id) DO UPDATE SET \
             blob_id = excluded.blob_id, artifact_id = excluded.artifact_id, \
             source_id = excluded.source_id, abs_path = excluded.abs_path, \
             size = excluded.size, is_symlink = excluded.is_symlink, \
             lfs_pointer = excluded.lfs_pointer, state = excluded.state, \
             pull_progress = excluded.pull_progress, present = excluded.present, \
             mtime = excluded.mtime, inode = excluded.inode, last_seen = excluded.last_seen \
         RETURNING id",
        params![
            input.revision.get(),
            input.path,
            input.blob.map(BlobId::get),
            input.artifact.map(ArtifactId::get),
            input.node_row,
            input.source.map(SourceId::get),
            input.abs_path,
            input.size as i64,
            i64::from(input.is_symlink),
            i64::from(input.lfs_pointer),
            input.state.as_str(),
            input.pull_progress.map(f64::from),
            i64::from(input.present),
            input.mtime,
            input.inode,
            timestamp_text(seen),
        ],
        |row| row.get(0),
    )?;
    if let Some(blob) = input.blob {
        refresh_path_count(conn, blob)?;
    }
    Ok(EntryId::new(id))
}

pub fn entry(conn: &Connection, id: EntryId) -> Result<Option<EntryRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {ENTRY_COLUMNS} {ENTRY_FROM} WHERE e.id = ?1"),
        [id.get()],
        |row| Ok(EntryRow::from_row(row)),
    ))
}

pub fn entries_for_revision(conn: &Connection, revision: RevisionId) -> Result<Vec<EntryRow>> {
    entries_where(conn, "e.revision_id = ?1", revision.get())
}

pub fn entries_for_blob(conn: &Connection, blob: BlobId) -> Result<Vec<EntryRow>> {
    entries_where(conn, "e.blob_id = ?1", blob.get())
}

pub fn entries_for_artifact(conn: &Connection, artifact: ArtifactId) -> Result<Vec<EntryRow>> {
    entries_where(conn, "e.artifact_id = ?1", artifact.get())
}

fn entries_where(conn: &Connection, clause: &str, key: i64) -> Result<Vec<EntryRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} {ENTRY_FROM} WHERE {clause} ORDER BY e.node_id, e.path"
    ))?;
    let rows = statement.query_map([key], |row| Ok(EntryRow::from_row(row)))?;
    collect_rows(rows)
}

/// Marks every path of a source the current scan did not see as absent. Rows survive so
/// the index still remembers what the disk used to hold.
pub fn mark_absent(conn: &Connection, source: SourceId, seen_before: Timestamp) -> Result<usize> {
    let marked = conn.execute(
        "UPDATE entry SET present = 0 WHERE source_id = ?1 AND present = 1 AND last_seen < ?2",
        (source.get(), timestamp_text(seen_before)),
    )?;
    if marked > 0 {
        refresh_all_path_counts(conn)?;
    }
    Ok(marked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::blob::{blob, upsert_blob};
    use crate::store::fixture::{Fixture, at};

    #[test]
    fn a_duplicate_blob_counts_both_of_its_paths() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);

        assert_eq!(
            blob(&conn, world.shared_blob)
                .expect("blob reads")
                .path_count,
            2
        );
        let entries = entries_for_blob(&conn, world.shared_blob).expect("entries list");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].repo_qualified, "models/org/name");
        assert_eq!(entries[0].node, "workstation");
        assert_eq!(entries[0].state, State::Served);

        let first = entries_for_revision(&conn, world.first_revision).expect("entries list");
        assert_eq!(first.len(), 2);
        assert_eq!(
            entries_for_artifact(&conn, world.shared_artifact)
                .expect("entries list")
                .len(),
            2
        );
    }

    #[test]
    fn rescanning_a_path_updates_it_in_place() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let replacement = upsert_blob(&conn, "xxh3:replacement", 8, at()).expect("blob inserts");

        let entry_id = upsert_entry(
            &conn,
            &NewEntry {
                blob: Some(replacement),
                size: 8,
                ..NewEntry::new(
                    world.first_revision,
                    "unet/model.safetensors",
                    world.node_row,
                )
            },
            at(),
        )
        .expect("entry upserts");

        let row = entry(&conn, entry_id)
            .expect("entry reads")
            .expect("entry exists");
        assert_eq!(row.blob, Some(replacement));
        assert_eq!(row.size, 8);
        assert_eq!(
            entries_for_revision(&conn, world.first_revision)
                .expect("list")
                .len(),
            2
        );
    }

    #[test]
    fn a_vanished_path_is_marked_absent_and_kept() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let later: Timestamp = "2026-09-05T09:10:00Z".parse().expect("timestamp");

        upsert_entry(
            &conn,
            &NewEntry {
                blob: Some(world.shared_blob),
                source: Some(world.source),
                abs_path: Some("/data/hub/a.safetensors".to_owned()),
                state: State::Served,
                ..NewEntry::new(
                    world.first_revision,
                    "unet/model.safetensors",
                    world.node_row,
                )
            },
            later,
        )
        .expect("entry is seen again");

        let marked = mark_absent(&conn, world.source, later).expect("absent marks");
        assert_eq!(marked, 2);

        let rows = entries_for_revision(&conn, world.first_revision).expect("entries list");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.present));
        assert!(rows.iter().any(|row| !row.present));
        assert_eq!(
            blob(&conn, world.shared_blob)
                .expect("blob reads")
                .path_count,
            1
        );
    }
}
