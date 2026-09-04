use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::{ArtifactId, Base, Role, SubRole};

use crate::error::Result;
use crate::store::artifact::reindex;
use crate::store::convert::{
    collect_rows, now, optional_row, parse_opt_enum, parse_timestamp, timestamp_text,
};
use crate::triage::check_verdict;

const APPLY_OVERRIDE: &str = "UPDATE artifact SET \
     role = COALESCE((SELECT o.role FROM artifact_override o WHERE o.artifact_id = artifact.id), \
         role), \
     sub_role = COALESCE((SELECT o.sub_role FROM artifact_override o \
         WHERE o.artifact_id = artifact.id), sub_role), \
     base = COALESCE((SELECT o.base FROM artifact_override o WHERE o.artifact_id = artifact.id), \
         base), \
     role_confidence = CASE WHEN (SELECT o.role FROM artifact_override o \
         WHERE o.artifact_id = artifact.id) IS NOT NULL THEN 1.0 ELSE role_confidence END, \
     base_confidence = CASE WHEN (SELECT o.base FROM artifact_override o \
         WHERE o.artifact_id = artifact.id) IS NOT NULL THEN 1.0 ELSE base_confidence END, \
     triage = 0 ";

/// A user decision that outranks inference and survives every rescan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverrideRow {
    pub artifact: ArtifactId,
    pub role: Option<Role>,
    pub sub_role: Option<SubRole>,
    pub base: Option<Base>,
    pub decided_at: Timestamp,
}

impl OverrideRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            artifact: ArtifactId::new(row.get("artifact_id")?),
            role: parse_opt_enum(row.get::<_, Option<String>>("role")?)?,
            sub_role: parse_opt_enum(row.get::<_, Option<String>>("sub_role")?)?,
            base: row.get::<_, Option<String>>("base")?.map(Base::new),
            decided_at: parse_timestamp(&row.get::<_, String>("decided_at")?)?,
        })
    }
}

/// A path a rejection marked as noise, which later scans skip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoiseRow {
    pub id: i64,
    pub path: String,
    pub reason: Option<String>,
    pub decided_at: Timestamp,
}

impl NoiseRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            path: row.get("path")?,
            reason: row.get("reason")?,
            decided_at: parse_timestamp(&row.get::<_, String>("decided_at")?)?,
        })
    }
}

/// Records a decision and applies it to the artifact at once. A role and a sub-role that
/// disagree are refused rather than stored; see [`crate::triage::check_verdict`].
pub fn put_override(
    conn: &Connection,
    id: ArtifactId,
    role: Option<Role>,
    sub_role: Option<SubRole>,
    base: Option<Base>,
) -> Result<()> {
    check_verdict(role, sub_role)?;
    conn.execute(
        "INSERT INTO artifact_override(artifact_id, role, sub_role, base, decided_at) \
         VALUES(?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(artifact_id) DO UPDATE SET \
             role = excluded.role, sub_role = excluded.sub_role, base = excluded.base, \
             decided_at = excluded.decided_at",
        (
            id.get(),
            role.map(Role::as_str),
            sub_role.map(SubRole::as_str),
            base.as_ref().map(Base::as_str),
            timestamp_text(now()),
        ),
    )?;
    conn.execute(&format!("{APPLY_OVERRIDE} WHERE id = ?1"), [id.get()])?;
    reindex(conn, id, None)
}

pub fn clear_override(conn: &Connection, id: ArtifactId) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM artifact_override WHERE artifact_id = ?1",
        [id.get()],
    )?;
    Ok(removed > 0)
}

pub fn artifact_override(conn: &Connection, id: ArtifactId) -> Result<Option<OverrideRow>> {
    optional_row(conn.query_row(
        "SELECT artifact_id, role, sub_role, base, decided_at FROM artifact_override \
         WHERE artifact_id = ?1",
        [id.get()],
        |row| Ok(OverrideRow::from_row(row)),
    ))
}

/// Re-applies every decision, which is how user choices survive a rescan that re-inferred
/// the same artifacts.
pub fn apply_overrides(conn: &Connection) -> Result<usize> {
    let ids: Vec<i64> = {
        let mut statement = conn.prepare("SELECT artifact_id FROM artifact_override")?;
        let rows = statement.query_map([], |row| Ok(row.get::<_, i64>(0).map_err(Into::into)))?;
        collect_rows(rows)?
    };
    conn.execute(
        &format!("{APPLY_OVERRIDE} WHERE id IN (SELECT artifact_id FROM artifact_override)"),
        [],
    )?;
    for id in &ids {
        reindex(conn, ArtifactId::new(*id), None)?;
    }
    Ok(ids.len())
}

pub fn add_noise(conn: &Connection, path: &str, reason: Option<&str>) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO noise(path, reason, decided_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(path) DO UPDATE SET reason = COALESCE(excluded.reason, noise.reason) \
         RETURNING id",
        (path, reason, timestamp_text(now())),
        |row| row.get(0),
    )?;
    Ok(id)
}

pub fn list_noise(conn: &Connection) -> Result<Vec<NoiseRow>> {
    let mut statement =
        conn.prepare("SELECT id, path, reason, decided_at FROM noise ORDER BY path")?;
    let rows = statement.query_map([], |row| Ok(NoiseRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn remove_noise(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM noise WHERE id = ?1", [id])? > 0)
}

pub fn is_noise(conn: &Connection, path: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM noise WHERE path = ?1",
        [path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifact::{NewArtifact, artifact, upsert_artifact};
    use crate::store::blob::upsert_blob;
    use crate::store::fixture::at;
    use hoard_core::ArtifactFormat;

    fn inferred(blob: hoard_core::BlobId) -> NewArtifact {
        NewArtifact {
            format: ArtifactFormat::Safetensors,
            role: Role::Unk,
            role_confidence: 0.2,
            base: Base::new("sd15"),
            ..NewArtifact::new(blob, "sd_xl_refiner_FINAL_v2")
        }
    }

    #[test]
    fn a_user_decision_outranks_a_later_rescan() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:triage", 64, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &inferred(blob), at()).expect("artifact inserts");

        put_override(
            &conn,
            id,
            Some(Role::Ckpt),
            None,
            Some(Base::new("sdxl 1.0")),
        )
        .expect("override records");
        let decided = artifact(&conn, id).expect("artifact reads");
        assert_eq!(decided.artifact.role, Role::Ckpt);
        assert_eq!(decided.artifact.base, Base::new("sdxl 1.0"));
        assert_eq!(decided.artifact.role_confidence, 1.0);
        assert!(!decided.triage);

        upsert_artifact(&conn, &inferred(blob), at()).expect("rescan re-infers");
        assert_eq!(
            artifact(&conn, id).expect("artifact reads").artifact.role,
            Role::Unk
        );

        assert_eq!(apply_overrides(&conn).expect("overrides re-apply"), 1);
        let restored = artifact(&conn, id).expect("artifact reads");
        assert_eq!(restored.artifact.role, Role::Ckpt);
        assert_eq!(restored.artifact.base, Base::new("sdxl 1.0"));
        assert_eq!(
            artifact_override(&conn, id)
                .expect("override reads")
                .map(|row| row.role),
            Some(Some(Role::Ckpt))
        );
    }

    #[test]
    fn an_override_leaves_the_fields_it_does_not_name() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:partial", 64, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &inferred(blob), at()).expect("artifact inserts");

        put_override(&conn, id, None, Some(SubRole::Ipa), None).expect("override records");
        let row = artifact(&conn, id).expect("artifact reads");
        assert_eq!(row.artifact.sub_role, Some(SubRole::Ipa));
        assert_eq!(row.artifact.base, Base::new("sd15"));
        assert_eq!(row.artifact.role_confidence, 0.2);

        assert!(clear_override(&conn, id).expect("override clears"));
        assert!(!clear_override(&conn, id).expect("clearing twice is a no-op"));
        assert_eq!(apply_overrides(&conn).expect("nothing to apply"), 0);
    }

    #[test]
    fn rejected_paths_are_remembered_as_noise() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = add_noise(&conn, "/data/tmp/partial.safetensors", Some("truncated"))
            .expect("noise records");
        assert_eq!(
            add_noise(&conn, "/data/tmp/partial.safetensors", None).expect("noise upserts"),
            id
        );
        assert!(is_noise(&conn, "/data/tmp/partial.safetensors").expect("lookup"));
        assert!(!is_noise(&conn, "/data/keep.safetensors").expect("lookup"));

        let listed = list_noise(&conn).expect("noise lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].reason.as_deref(), Some("truncated"));
        assert!(remove_noise(&conn, id).expect("noise removes"));
        assert!(list_noise(&conn).expect("noise lists").is_empty());
    }
}
