use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::{BlobId, GitSha1, Sha256};

use crate::error::Result;
use crate::store::DiskPressure;
use crate::store::convert::{
    collect_rows, optional_row, parse_opt_enum, parse_opt_timestamp, parse_timestamp, required_row,
    timestamp_text, to_u32, to_u64,
};

const BLOB_COLUMNS: &str =
    "id, sha256, git_sha1, fingerprint, size, path_count, first_seen, hashed_at";

const PRESENT_PATH_COUNT: &str = "SELECT COUNT(DISTINCT e.node_id || char(31) \
     || COALESCE(e.abs_path, e.path)) FROM entry e WHERE e.blob_id = blob.id AND e.present = 1";

/// Unique bytes. The sha256 arrives after the scan that created the row, so identity is the
/// cheap fingerprint until then.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlobRow {
    pub id: BlobId,
    pub sha256: Option<Sha256>,
    pub git_sha1: Option<GitSha1>,
    pub fingerprint: String,
    pub size: u64,
    pub path_count: u32,
    pub first_seen: Timestamp,
    pub hashed_at: Option<Timestamp>,
}

impl BlobRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: BlobId::new(row.get("id")?),
            sha256: parse_opt_enum(row.get::<_, Option<String>>("sha256")?)?,
            git_sha1: parse_opt_enum(row.get::<_, Option<String>>("git_sha1")?)?,
            fingerprint: row.get("fingerprint")?,
            size: to_u64(row.get::<_, i64>("size")?),
            path_count: to_u32(row.get::<_, i64>("path_count")?),
            first_seen: parse_timestamp(&row.get::<_, String>("first_seen")?)?,
            hashed_at: parse_opt_timestamp(row.get::<_, Option<String>>("hashed_at")?)?,
        })
    }
}

/// Returns the blob with these bytes, inserting it when the fingerprint and size are new.
pub fn upsert_blob(
    conn: &Connection,
    fingerprint: &str,
    size: u64,
    at: Timestamp,
) -> Result<BlobId> {
    if let Some(existing) = blob_by_fingerprint(conn, fingerprint, size)? {
        return Ok(existing.id);
    }
    let id = conn.query_row(
        "INSERT INTO blob(fingerprint, size, first_seen) VALUES(?1, ?2, ?3) RETURNING id",
        (fingerprint, size as i64, timestamp_text(at)),
        |row| row.get::<_, i64>(0),
    )?;
    Ok(BlobId::new(id))
}

/// Records the content hashes once the background hasher has read the bytes.
pub fn set_sha256(
    conn: &Connection,
    id: BlobId,
    sha256: Sha256,
    git_sha1: Option<GitSha1>,
    at: Timestamp,
) -> Result<()> {
    conn.execute(
        "UPDATE blob SET sha256 = ?2, git_sha1 = COALESCE(?3, git_sha1), hashed_at = ?4 \
         WHERE id = ?1",
        (
            id.get(),
            sha256.to_hex(),
            git_sha1.map(GitSha1::to_hex),
            timestamp_text(at),
        ),
    )?;
    Ok(())
}

pub fn blob(conn: &Connection, id: BlobId) -> Result<BlobRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {BLOB_COLUMNS} FROM blob WHERE id = ?1"),
            [id.get()],
            |row| Ok(BlobRow::from_row(row)),
        ),
        "blob",
        id,
    )
}

pub fn blob_by_sha256(conn: &Connection, sha256: &Sha256) -> Result<Option<BlobRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {BLOB_COLUMNS} FROM blob WHERE sha256 = ?1"),
        [sha256.to_hex()],
        |row| Ok(BlobRow::from_row(row)),
    ))
}

pub fn blob_by_fingerprint(
    conn: &Connection,
    fingerprint: &str,
    size: u64,
) -> Result<Option<BlobRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {BLOB_COLUMNS} FROM blob WHERE fingerprint = ?1 AND size = ?2"),
        (fingerprint, size as i64),
        |row| Ok(BlobRow::from_row(row)),
    ))
}

/// The oldest blobs still waiting for a sha256, which is the hashing queue.
pub fn unhashed_blobs(conn: &Connection, limit: u32) -> Result<Vec<BlobRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {BLOB_COLUMNS} FROM blob WHERE sha256 IS NULL ORDER BY id LIMIT ?1"
    ))?;
    let rows = statement.query_map([limit], |row| Ok(BlobRow::from_row(row)))?;
    collect_rows(rows)
}

/// Recounts the present paths reaching one blob, which is what `dupe:` and DISK PRESSURE
/// read.
pub fn refresh_path_count(conn: &Connection, id: BlobId) -> Result<u32> {
    let count: i64 = conn.query_row(
        &format!(
            "UPDATE blob SET path_count = ({PRESENT_PATH_COUNT}) WHERE id = ?1 \
             RETURNING path_count"
        ),
        [id.get()],
        |row| row.get(0),
    )?;
    Ok(to_u32(count))
}

/// Recounts every blob, after a scan marked entries absent.
pub fn refresh_all_path_counts(conn: &Connection) -> Result<usize> {
    let changed = conn.execute(
        &format!("UPDATE blob SET path_count = ({PRESENT_PATH_COUNT})"),
        [],
    )?;
    Ok(changed)
}

/// What deduplicating the index would give back: every extra copy of a duplicated blob.
pub fn disk_pressure(conn: &Connection) -> Result<DiskPressure> {
    let (reclaimable_bytes, duplicate_blobs) = conn.query_row(
        "SELECT COALESCE(SUM(size * (path_count - 1)), 0), COUNT(*) FROM blob WHERE path_count > 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    Ok(DiskPressure {
        reclaimable_bytes: to_u64(reclaimable_bytes),
        duplicate_blobs: to_u64(duplicate_blobs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn at() -> Timestamp {
        "2026-09-04T09:10:00Z".parse().expect("timestamp")
    }

    fn sha(byte: u8) -> Sha256 {
        Sha256::from_bytes([byte; 32])
    }

    #[test]
    fn blobs_are_keyed_on_fingerprint_and_size() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let first = upsert_blob(&conn, "xxh3:1", 1024, at()).expect("blob inserts");
        let same = upsert_blob(&conn, "xxh3:1", 1024, at()).expect("blob reused");
        let resized = upsert_blob(&conn, "xxh3:1", 2048, at()).expect("blob inserts");
        assert_eq!(first, same);
        assert_ne!(first, resized);

        let row = blob(&conn, first).expect("blob reads");
        assert_eq!(row.size, 1024);
        assert_eq!(row.path_count, 0);
        assert!(row.sha256.is_none());
        assert_eq!(row.first_seen, at());
    }

    #[test]
    fn hashing_fills_in_the_content_identity() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = upsert_blob(&conn, "xxh3:2", 64, at()).expect("blob inserts");
        assert_eq!(unhashed_blobs(&conn, 10).expect("queue").len(), 1);

        let git_sha1 = GitSha1::from_bytes([7; 20]);
        set_sha256(&conn, id, sha(0x9f), Some(git_sha1), at()).expect("hash records");
        assert!(unhashed_blobs(&conn, 10).expect("queue").is_empty());

        let found = blob_by_sha256(&conn, &sha(0x9f))
            .expect("lookup")
            .expect("blob exists");
        assert_eq!(found.id, id);
        assert_eq!(found.git_sha1, Some(git_sha1));
        assert_eq!(found.hashed_at, Some(at()));
        assert!(blob_by_sha256(&conn, &sha(0x11)).expect("lookup").is_none());
        assert!(
            blob_by_fingerprint(&conn, "xxh3:2", 64)
                .expect("lookup")
                .is_some()
        );
    }

    #[test]
    fn an_index_without_duplicates_has_no_disk_pressure() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        upsert_blob(&conn, "xxh3:3", 4096, at()).expect("blob inserts");
        assert_eq!(
            disk_pressure(&conn).expect("pressure"),
            DiskPressure::default()
        );
    }
}
