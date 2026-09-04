use jiff::Timestamp;
use rusqlite::{Connection, Row, params};
use serde::Serialize;
use strum::IntoEnumIterator;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, Evidence, EvidenceSource, Precision, Role,
    SubRole, TensorSample, TensorSummary,
};

use crate::error::{IndexError, Result};
use crate::store::convert::{
    collect_rows, optional_row, parse_enum, parse_opt_enum, parse_opt_timestamp, parse_timestamp,
    required_row, timestamp_text, to_u32, to_u64,
};

const ARTIFACT_COLUMNS: &str = "a.id AS id, a.blob_id AS blob_id, b.sha256 AS sha256, \
     COALESCE(a.group_bytes, b.size) AS size, a.shard_count AS shard_count, \
     a.sniff_version AS sniff_version, a.format AS format, a.role AS role, \
     a.sub_role AS sub_role, a.role_confidence AS role_confidence, a.base AS base, \
     a.base_label AS base_label, a.base_confidence AS base_confidence, \
     a.precision AS precision, a.quant AS quant, a.rank AS rank, a.alpha AS alpha, \
     a.params AS params, a.context AS context, a.trigger_words AS trigger_words, \
     a.org AS org, a.name AS name, a.triage AS triage, a.last_used AS last_used, \
     a.use_count AS use_count, a.created_at AS created_at";

const ARTIFACT_FROM: &str = "FROM artifact a JOIN blob b ON b.id = a.blob_id";

/// What sniffing and inference decided about a blob, before the index knows where it lives.
#[derive(Debug, Clone, PartialEq)]
pub struct NewArtifact {
    pub blob: BlobId,
    pub sniff_version: u32,
    pub format: ArtifactFormat,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub base: Base,
    pub base_label: Option<String>,
    pub base_confidence: f32,
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub trigger: Option<String>,
    pub org: Option<String>,
    pub name: String,
    /// What the whole model weighs when it is split across files, and how many files that
    /// is. Both are `None` for a model held in one file, which reads its size from its blob.
    pub group_bytes: Option<u64>,
    pub shard_count: Option<u32>,
}

impl NewArtifact {
    /// An unclassified artifact of the given bytes, which is what a scan starts from.
    pub fn new(blob: BlobId, name: impl Into<String>) -> Self {
        Self {
            blob,
            sniff_version: 1,
            format: ArtifactFormat::Unknown,
            role: Role::Unk,
            sub_role: None,
            role_confidence: 0.0,
            base: Base::new(""),
            base_label: None,
            base_confidence: 0.0,
            precision: None,
            quant: None,
            rank: None,
            alpha: None,
            params: None,
            context: None,
            trigger: None,
            org: None,
            name: name.into(),
            group_bytes: None,
            shard_count: None,
        }
    }
}

/// An artifact as stored: the domain record plus the index bookkeeping around it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactRow {
    pub artifact: Artifact,
    /// How many files hold the artifact, one unless a shard set was grouped into it.
    pub shard_count: u32,
    pub org: Option<String>,
    pub name: String,
    pub triage: bool,
    pub last_used: Option<Timestamp>,
    pub use_count: u64,
    pub created_at: Timestamp,
}

impl ArtifactRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        let artifact = Artifact {
            id: ArtifactId::new(row.get("id")?),
            blob: BlobId::new(row.get("blob_id")?),
            sha256: parse_opt_enum(row.get::<_, Option<String>>("sha256")?)?,
            size: to_u64(row.get::<_, i64>("size")?),
            format: parse_enum(&row.get::<_, String>("format")?)?,
            role: parse_enum(&row.get::<_, String>("role")?)?,
            sub_role: parse_opt_enum(row.get::<_, Option<String>>("sub_role")?)?,
            role_confidence: row.get::<_, f64>("role_confidence")? as f32,
            base: Base::new(row.get::<_, String>("base")?),
            base_label: row.get("base_label")?,
            base_confidence: row.get::<_, f64>("base_confidence")? as f32,
            precision: row
                .get::<_, Option<String>>("precision")?
                .map(Precision::new),
            quant: row.get("quant")?,
            rank: row.get::<_, Option<i64>>("rank")?.map(to_u32),
            alpha: row
                .get::<_, Option<f64>>("alpha")?
                .map(|value| value as f32),
            params: row.get::<_, Option<i64>>("params")?.map(to_u64),
            context: row.get::<_, Option<i64>>("context")?.map(to_u32),
            trigger: row.get("trigger_words")?,
            sniff_version: to_u32(row.get::<_, i64>("sniff_version")?),
        };
        Ok(Self {
            artifact,
            shard_count: row
                .get::<_, Option<i64>>("shard_count")?
                .map_or(1, to_u32)
                .max(1),
            org: row.get("org")?,
            name: row.get("name")?,
            triage: row.get::<_, i64>("triage")? != 0,
            last_used: parse_opt_timestamp(row.get::<_, Option<String>>("last_used")?)?,
            use_count: to_u64(row.get::<_, i64>("use_count")?),
            created_at: parse_timestamp(&row.get::<_, String>("created_at")?)?,
        })
    }
}

/// Inserts or refreshes the interpretation of a blob, leaving triage and usage untouched.
pub fn upsert_artifact(
    conn: &Connection,
    input: &NewArtifact,
    at: Timestamp,
) -> Result<ArtifactId> {
    let id: i64 = conn.query_row(
        "INSERT INTO artifact(blob_id, sniff_version, format, role, sub_role, role_confidence, \
             base, base_label, base_confidence, precision, quant, rank, alpha, params, context, \
             trigger_words, org, name, group_bytes, shard_count, created_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20, ?21) \
         ON CONFLICT(blob_id, sniff_version) DO UPDATE SET \
             format = excluded.format, role = excluded.role, sub_role = excluded.sub_role, \
             role_confidence = excluded.role_confidence, base = excluded.base, \
             base_label = excluded.base_label, base_confidence = excluded.base_confidence, \
             precision = excluded.precision, quant = excluded.quant, rank = excluded.rank, \
             alpha = excluded.alpha, params = excluded.params, context = excluded.context, \
             trigger_words = excluded.trigger_words, org = excluded.org, name = excluded.name, \
             group_bytes = excluded.group_bytes, shard_count = excluded.shard_count \
         RETURNING id",
        params![
            input.blob.get(),
            i64::from(input.sniff_version),
            input.format.as_str(),
            input.role.as_str(),
            input.sub_role.map(SubRole::as_str),
            f64::from(input.role_confidence),
            input.base.as_str(),
            input.base_label,
            f64::from(input.base_confidence),
            input.precision.as_ref().map(Precision::as_str),
            input.quant,
            input.rank.map(i64::from),
            input.alpha.map(f64::from),
            input.params.map(|value| value as i64),
            input.context.map(i64::from),
            input.trigger,
            input.org,
            input.name,
            input.group_bytes.map(|bytes| bytes as i64),
            input.shard_count.map(i64::from),
            timestamp_text(at),
        ],
        |row| row.get(0),
    )?;
    let id = ArtifactId::new(id);
    reindex(conn, id, None)?;
    Ok(id)
}

pub fn artifact(conn: &Connection, id: ArtifactId) -> Result<ArtifactRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {ARTIFACT_COLUMNS} {ARTIFACT_FROM} WHERE a.id = ?1"),
            [id.get()],
            |row| Ok(ArtifactRow::from_row(row)),
        ),
        "artifact",
        id,
    )
}

pub fn artifact_by_blob(
    conn: &Connection,
    blob: BlobId,
    sniff_version: u32,
) -> Result<Option<ArtifactRow>> {
    optional_row(conn.query_row(
        &format!(
            "SELECT {ARTIFACT_COLUMNS} {ARTIFACT_FROM} \
             WHERE a.blob_id = ?1 AND a.sniff_version = ?2"
        ),
        (blob.get(), i64::from(sniff_version)),
        |row| Ok(ArtifactRow::from_row(row)),
    ))
}

/// The artifacts waiting in the triage queue, oldest first.
/// How many artifacts are waiting in the triage queue.
///
/// Counted in SQL: the queue is unbounded in principle, and the badge only needs a number.
/// Removes artifacts produced by a superseded sniff version that nothing points at any more.
///
/// Bumping the sniff version writes a second artifact for the same blob rather than editing
/// the first, because the pair `(blob, sniff version)` is the identity. Once the scan has
/// re-pointed every entry at the new row, the old one is unreachable, and leaving it behind
/// would double every count the UI shows.
pub fn delete_superseded(conn: &Connection, current: u32) -> Result<u64> {
    let removed = conn.execute(
        "DELETE FROM artifact WHERE sniff_version <> ?1 \
         AND NOT EXISTS (SELECT 1 FROM entry e WHERE e.artifact_id = artifact.id)",
        [i64::from(current)],
    )?;
    Ok(to_u64(removed as i64))
}

/// Removes artifacts for formats that describe their neighbours rather than standing alone.
///
/// A `config.json` or a README is context for the weights beside it, never a row of its own.
/// The scan has always applied that rule, but the upstream ingest once did not, so an index
/// can hold such rows from before; the entries stay, because the files are still servable.
pub fn delete_describing(conn: &Connection) -> Result<u64> {
    let standalone: Vec<&'static str> = ArtifactFormat::iter()
        .filter(|format| format.is_artifact())
        .map(ArtifactFormat::as_str)
        .collect();
    let placeholders = vec!["?"; standalone.len()].join(", ");
    let params = rusqlite::params_from_iter(standalone.iter());
    conn.execute(
        &format!(
            "UPDATE entry SET artifact_id = NULL WHERE artifact_id IN \
             (SELECT id FROM artifact WHERE format NOT IN ({placeholders}))"
        ),
        params,
    )?;
    let params = rusqlite::params_from_iter(standalone.iter());
    let removed = conn.execute(
        &format!("DELETE FROM artifact WHERE format NOT IN ({placeholders})"),
        params,
    )?;
    Ok(to_u64(removed as i64))
}

pub fn triage_count(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM artifact WHERE triage = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(to_u64(count))
}

pub fn triage_queue(conn: &Connection, limit: u32) -> Result<Vec<ArtifactRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {ARTIFACT_COLUMNS} {ARTIFACT_FROM} WHERE a.triage = 1 ORDER BY a.id LIMIT ?1"
    ))?;
    let rows = statement.query_map([limit], |row| Ok(ArtifactRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn set_triage(conn: &Connection, id: ArtifactId, triage: bool) -> Result<()> {
    conn.execute(
        "UPDATE artifact SET triage = ?2 WHERE id = ?1",
        (id.get(), i64::from(triage)),
    )?;
    Ok(())
}

pub fn set_last_used(conn: &Connection, id: ArtifactId, at: Timestamp) -> Result<()> {
    conn.execute(
        "UPDATE artifact SET last_used = ?2 WHERE id = ?1",
        (id.get(), timestamp_text(at)),
    )?;
    Ok(())
}

/// Stores the header summary and refreshes the tensor keys the search index carries.
pub fn set_tensor_summary(
    conn: &Connection,
    id: ArtifactId,
    summary: &TensorSummary,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tensor_summary(artifact_id, tensor_count, params, dtypes, key_prefixes, \
             samples, metadata) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(artifact_id) DO UPDATE SET \
             tensor_count = excluded.tensor_count, params = excluded.params, \
             dtypes = excluded.dtypes, key_prefixes = excluded.key_prefixes, \
             samples = excluded.samples, metadata = excluded.metadata",
        params![
            id.get(),
            i64::from(summary.tensor_count),
            summary.params as i64,
            serde_json::to_string(&summary.dtypes)?,
            serde_json::to_string(&summary.key_prefixes)?,
            serde_json::to_string(&summary.samples)?,
            serde_json::to_string(&summary.metadata)?,
        ],
    )?;
    reindex(conn, id, None)
}

pub fn tensor_summary(conn: &Connection, id: ArtifactId) -> Result<Option<TensorSummary>> {
    optional_row(conn.query_row(
        "SELECT tensor_count, params, dtypes, key_prefixes, samples, metadata \
         FROM tensor_summary WHERE artifact_id = ?1",
        [id.get()],
        |row| {
            Ok((|| {
                Ok(TensorSummary {
                    tensor_count: to_u32(row.get::<_, i64>(0)?),
                    params: to_u64(row.get::<_, i64>(1)?),
                    dtypes: serde_json::from_str(&row.get::<_, String>(2)?)?,
                    key_prefixes: serde_json::from_str(&row.get::<_, String>(3)?)?,
                    samples: serde_json::from_str::<Vec<TensorSample>>(&row.get::<_, String>(4)?)?,
                    metadata: serde_json::from_str(&row.get::<_, String>(5)?)?,
                })
            })())
        },
    ))
}

/// Replaces every derived fact recorded for an artifact.
pub fn replace_evidence(conn: &Connection, id: ArtifactId, evidence: &[Evidence]) -> Result<()> {
    conn.execute("DELETE FROM evidence WHERE artifact_id = ?1", [id.get()])?;
    let mut statement = conn.prepare(
        "INSERT INTO evidence(artifact_id, field, value, source, detail, confidence) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for fact in evidence {
        statement.execute(params![
            id.get(),
            fact.field,
            fact.value,
            fact.source.as_str(),
            fact.detail,
            f64::from(fact.confidence),
        ])?;
    }
    Ok(())
}

pub fn evidence(conn: &Connection, id: ArtifactId) -> Result<Vec<Evidence>> {
    let mut statement = conn.prepare(
        "SELECT field, value, source, detail, confidence FROM evidence \
         WHERE artifact_id = ?1 ORDER BY confidence DESC, id",
    )?;
    let rows = statement.query_map([id.get()], |row| {
        Ok((|| {
            Ok(Evidence {
                field: row.get(0)?,
                value: row.get(1)?,
                source: parse_evidence_source(&row.get::<_, String>(2)?)?,
                detail: row.get(3)?,
                confidence: row.get::<_, f64>(4)? as f32,
            })
        })())
    })?;
    collect_rows(rows)
}

/// Adds the model card text to the search index for this artifact.
pub fn set_card(conn: &Connection, id: ArtifactId, card: &str) -> Result<()> {
    reindex(conn, id, Some(card))
}

/// Rewrites the full text row for an artifact, keeping the card already indexed when the
/// caller has no fresher one.
pub(crate) fn reindex(conn: &Connection, id: ArtifactId, card: Option<&str>) -> Result<()> {
    let stored_card: Option<String> = optional_row(conn.query_row(
        "SELECT card FROM artifact_fts WHERE artifact_id = ?1",
        [id.get()],
        |row| Ok(row.get::<_, Option<String>>(0).map_err(IndexError::Sqlite)),
    ))?
    .flatten();
    let fields = optional_row(conn.query_row(
        "SELECT a.name, a.org, a.base, a.trigger_words, t.key_prefixes FROM artifact a \
         LEFT JOIN tensor_summary t ON t.artifact_id = a.id WHERE a.id = ?1",
        [id.get()],
        |row| {
            Ok((|| {
                let keys: Option<String> = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    keys.map(|raw| key_text(&raw)).transpose()?,
                ))
            })())
        },
    ))?;
    let Some((name, org, base, trigger_words, keys)) = fields else {
        return Ok(());
    };
    conn.execute(
        "DELETE FROM artifact_fts WHERE artifact_id = ?1",
        [id.get()],
    )?;
    conn.execute(
        "INSERT INTO artifact_fts(artifact_id, name, org, base, trigger_words, keys, card) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id.get(),
            name,
            org.unwrap_or_default(),
            base,
            trigger_words.unwrap_or_default(),
            keys.unwrap_or_default(),
            card.map(str::to_owned).or(stored_card).unwrap_or_default(),
        ],
    )?;
    Ok(())
}

fn key_text(raw: &str) -> Result<String> {
    let prefixes: Vec<(String, u32)> = serde_json::from_str(raw)?;
    Ok(prefixes
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<String>>()
        .join(" "))
}

fn parse_evidence_source(raw: &str) -> Result<EvidenceSource> {
    match raw {
        "tensor" => Ok(EvidenceSource::Tensor),
        "config" => Ok(EvidenceSource::Config),
        "metadata" => Ok(EvidenceSource::Metadata),
        "frontmatter" => Ok(EvidenceSource::Frontmatter),
        "folder_hint" => Ok(EvidenceSource::FolderHint),
        "filename" => Ok(EvidenceSource::Filename),
        "header" => Ok(EvidenceSource::Header),
        "user" => Ok(EvidenceSource::User),
        other => Err(IndexError::Config(format!(
            "unknown evidence source {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::store::Store;
    use crate::store::blob::upsert_blob;

    fn at() -> Timestamp {
        "2026-09-04T09:10:00Z".parse().expect("timestamp")
    }

    fn lora(blob: BlobId) -> NewArtifact {
        NewArtifact {
            format: ArtifactFormat::Safetensors,
            role: Role::Lora,
            role_confidence: 0.98,
            base: Base::new("sdxl 1.0"),
            base_label: Some("sdxl 1.0".to_owned()),
            base_confidence: 0.9,
            precision: Some(Precision::new("fp16")),
            rank: Some(32),
            alpha: Some(16.0),
            trigger: Some("film grain, 35mm".to_owned()),
            org: Some("kohya".to_owned()),
            ..NewArtifact::new(blob, "film-grain-35mm")
        }
    }

    fn summary() -> TensorSummary {
        TensorSummary {
            tensor_count: 264,
            params: 1024,
            dtypes: BTreeMap::from([("F16".to_owned(), 264)]),
            key_prefixes: vec![("lora_unet".to_owned(), 200), ("lora_te".to_owned(), 64)],
            samples: vec![TensorSample {
                name: "lora_unet_down.lora_down.weight".to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![32, 320],
            }],
            metadata: BTreeMap::from([("ss_network_dim".to_owned(), "32".to_owned())]),
        }
    }

    #[test]
    fn an_artifact_round_trips_every_inferred_field() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:a", 239_075_328, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &lora(blob), at()).expect("artifact inserts");

        let row = artifact(&conn, id).expect("artifact reads");
        assert_eq!(row.artifact.role, Role::Lora);
        assert_eq!(row.artifact.base, Base::new("sdxl 1.0"));
        assert_eq!(row.artifact.rank, Some(32));
        assert_eq!(row.artifact.alpha, Some(16.0));
        assert_eq!(row.artifact.size, 239_075_328);
        assert_eq!(row.name, "film-grain-35mm");
        assert_eq!(row.org.as_deref(), Some("kohya"));
        assert!(!row.triage);
        assert_eq!(row.use_count, 0);
        assert_eq!(artifact_by_blob(&conn, blob, 1).expect("lookup"), Some(row));
    }

    #[test]
    fn artifacts_for_describing_formats_are_swept_but_their_files_stay() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let weights = upsert_blob(&conn, "xxh3:weights", 64, at()).expect("blob inserts");
        let config = upsert_blob(&conn, "xxh3:config", 12, at()).expect("blob inserts");

        let kept = upsert_artifact(&conn, &lora(weights), at()).expect("weights write");
        let mut describing = lora(config);
        describing.format = ArtifactFormat::Config;
        let swept = upsert_artifact(&conn, &describing, at()).expect("config writes");

        assert_eq!(delete_describing(&conn).expect("swept"), 1);
        assert!(artifact(&conn, kept).is_ok(), "weights are untouched");
        assert!(
            artifact(&conn, swept).is_err(),
            "a config is not a row of its own"
        );
        assert_eq!(delete_describing(&conn).expect("idempotent"), 0);
    }

    #[test]
    fn a_superseded_artifact_is_dropped_once_nothing_points_at_it() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:superseded", 64, at()).expect("blob inserts");

        let stale = upsert_artifact(&conn, &lora(blob), at()).expect("old artifact writes");
        let next = NewArtifact {
            sniff_version: 2,
            ..lora(blob)
        };
        upsert_artifact(&conn, &next, at()).expect("new artifact writes");

        assert!(artifact(&conn, stale).is_ok());
        assert_eq!(delete_superseded(&conn, 2).expect("swept"), 1);
        assert!(
            artifact(&conn, stale).is_err(),
            "the unreachable row is gone"
        );
        assert_eq!(delete_superseded(&conn, 2).expect("swept again"), 0);
    }

    #[test]
    fn one_blob_yields_one_artifact_per_sniff_version() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:b", 64, at()).expect("blob inserts");
        let first = upsert_artifact(&conn, &lora(blob), at()).expect("artifact inserts");
        let again = upsert_artifact(&conn, &lora(blob), at()).expect("artifact upserts");
        assert_eq!(first, again);

        let next = NewArtifact {
            sniff_version: 2,
            ..lora(blob)
        };
        assert_ne!(
            upsert_artifact(&conn, &next, at()).expect("artifact inserts"),
            first
        );
    }

    #[test]
    fn triage_and_usage_survive_a_rescan() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:c", 64, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &lora(blob), at()).expect("artifact inserts");
        set_triage(&conn, id, true).expect("triage set");
        set_last_used(&conn, id, at()).expect("usage set");

        upsert_artifact(&conn, &lora(blob), at()).expect("artifact upserts");
        let row = artifact(&conn, id).expect("artifact reads");
        assert!(row.triage);
        assert_eq!(row.last_used, Some(at()));
        assert_eq!(triage_queue(&conn, 10).expect("queue").len(), 1);
    }

    #[test]
    fn the_header_summary_and_its_evidence_round_trip() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:d", 64, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &lora(blob), at()).expect("artifact inserts");

        assert_eq!(tensor_summary(&conn, id).expect("summary"), None);
        set_tensor_summary(&conn, id, &summary()).expect("summary stores");
        set_tensor_summary(&conn, id, &summary()).expect("summary replaces");
        assert_eq!(tensor_summary(&conn, id).expect("summary"), Some(summary()));

        let facts = vec![
            Evidence::new("role", "lora", EvidenceSource::Tensor, 0.95)
                .with_detail("lora_unet_ prefix on 200 tensors"),
            Evidence::new("base", "sdxl", EvidenceSource::Config, 0.8),
        ];
        replace_evidence(&conn, id, &facts).expect("evidence stores");
        replace_evidence(&conn, id, &facts).expect("evidence replaces");
        let stored = evidence(&conn, id).expect("evidence reads");
        assert_eq!(stored, facts);
    }

    #[test]
    fn the_full_text_row_carries_keys_and_the_card() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let blob = upsert_blob(&conn, "xxh3:e", 64, at()).expect("blob inserts");
        let id = upsert_artifact(&conn, &lora(blob), at()).expect("artifact inserts");
        set_tensor_summary(&conn, id, &summary()).expect("summary stores");
        set_card(&conn, id, "a film grain lora trained on 35mm scans").expect("card indexes");
        upsert_artifact(&conn, &lora(blob), at()).expect("artifact upserts");

        let matches = |query: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM artifact_fts WHERE artifact_fts MATCH ?1",
                [query],
                |row| row.get(0),
            )
            .expect("fts query")
        };
        assert_eq!(matches("film"), 1);
        assert_eq!(matches("kohya"), 1);
        assert_eq!(matches("lora_unet"), 1);
        assert_eq!(matches("35mm scans"), 1);
        assert_eq!(matches("wan"), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM artifact_fts", [], |row| row
                .get::<_, i64>(0))
                .expect("count"),
            1
        );
    }

    #[test]
    fn every_evidence_source_round_trips_its_spelling() {
        for source in [
            EvidenceSource::Tensor,
            EvidenceSource::Config,
            EvidenceSource::Metadata,
            EvidenceSource::Frontmatter,
            EvidenceSource::FolderHint,
            EvidenceSource::Filename,
            EvidenceSource::Header,
            EvidenceSource::User,
        ] {
            assert_eq!(
                parse_evidence_source(source.as_str()).expect("source parses"),
                source
            );
        }
        assert!(parse_evidence_source("hunch").is_err());
    }
}
