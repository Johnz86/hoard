use jiff::Timestamp;
use rusqlite::Row;
use rusqlite::types::{ToSql, ToSqlOutput};
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, EntryId, Precision, RepoRowId, Role,
    Sha256, State, SubRole,
};

use crate::error::{IndexError, Result};

/// A bound value in a compiled query. The search grammar produces these; the store binds
/// them positionally so no query is ever built by string interpolation of user input.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Text(String),
    Int(i64),
    Real(f64),
}

impl SqlValue {
    pub fn text(value: impl Into<String>) -> Self {
        SqlValue::Text(value.into())
    }
}

impl ToSql for SqlValue {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match self {
            SqlValue::Text(value) => value.to_sql(),
            SqlValue::Int(value) => value.to_sql(),
            SqlValue::Real(value) => value.to_sql(),
        }
    }
}

impl From<&str> for SqlValue {
    fn from(value: &str) -> Self {
        SqlValue::Text(value.to_owned())
    }
}

impl From<String> for SqlValue {
    fn from(value: String) -> Self {
        SqlValue::Text(value)
    }
}

impl From<i64> for SqlValue {
    fn from(value: i64) -> Self {
        SqlValue::Int(value)
    }
}

impl From<u64> for SqlValue {
    fn from(value: u64) -> Self {
        SqlValue::Int(value as i64)
    }
}

impl From<f64> for SqlValue {
    fn from(value: f64) -> Self {
        SqlValue::Real(value)
    }
}

/// One row of the `search_row` view: an artifact seen through one of its entries. This is
/// the shape the table, the cards, the inspector and `hoard ls --json` all render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRow {
    pub artifact_id: ArtifactId,
    pub blob_id: BlobId,
    pub blob_sha256: Option<Sha256>,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub org: Option<String>,
    pub name: String,
    pub base: Base,
    pub base_label: Option<String>,
    pub format: ArtifactFormat,
    pub precision: Option<Precision>,
    pub quant: Option<String>,
    pub size: u64,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub node: String,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub trigger: Option<String>,
    pub path: String,
    pub abs_path: Option<String>,
    pub path_count: u32,
    pub dupe: bool,
    pub triage: bool,
    pub last_used: Option<Timestamp>,
    pub repo: String,
    pub repo_id: RepoRowId,
    pub entry_id: EntryId,
    pub source: Option<String>,
    pub is_symlink: bool,
    pub lfs_pointer: bool,
}

/// The column list every search query selects, in the order `SearchRow::from_row` reads.
pub const SEARCH_COLUMNS: &str = "artifact_id, blob_id, blob_sha256, role, sub_role, \
     role_confidence, org, name, base, base_label, format, precision, quant, size, rank, alpha, \
     params, context, revision, revision_sha, node, state, pull_progress, trigger_words, path, \
     abs_path, path_count, dupe, triage, last_used, repo, repo_id, entry_id, source, is_symlink, \
     lfs_pointer";

impl SearchRow {
    /// Reads a row selected with [`SEARCH_COLUMNS`], by name so column order cannot drift.
    pub fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            artifact_id: ArtifactId::new(row.get("artifact_id")?),
            blob_id: BlobId::new(row.get("blob_id")?),
            blob_sha256: parse_opt(row.get::<_, Option<String>>("blob_sha256")?)?,
            role: parse_str(row.get::<_, String>("role")?)?,
            sub_role: parse_opt(row.get::<_, Option<String>>("sub_role")?)?,
            role_confidence: row.get::<_, f64>("role_confidence")? as f32,
            org: row.get("org")?,
            name: row.get("name")?,
            base: Base::new(row.get::<_, String>("base")?),
            base_label: row.get("base_label")?,
            format: parse_str(row.get::<_, String>("format")?)?,
            precision: row
                .get::<_, Option<String>>("precision")?
                .map(Precision::new),
            quant: row.get("quant")?,
            size: row.get::<_, i64>("size")?.max(0) as u64,
            rank: row
                .get::<_, Option<i64>>("rank")?
                .map(|value| value.max(0) as u32),
            alpha: row
                .get::<_, Option<f64>>("alpha")?
                .map(|value| value as f32),
            params: row
                .get::<_, Option<i64>>("params")?
                .map(|value| value.max(0) as u64),
            context: row
                .get::<_, Option<i64>>("context")?
                .map(|value| value.max(0) as u32),
            revision: row.get("revision")?,
            revision_sha: parse_opt(row.get::<_, Option<String>>("revision_sha")?)?,
            node: row.get("node")?,
            state: parse_str(row.get::<_, String>("state")?)?,
            pull_progress: row
                .get::<_, Option<f64>>("pull_progress")?
                .map(|value| value as f32),
            trigger: row.get("trigger_words")?,
            path: row.get("path")?,
            abs_path: row.get("abs_path")?,
            path_count: row.get::<_, i64>("path_count")?.max(0) as u32,
            dupe: row.get::<_, i64>("dupe")? != 0,
            triage: row.get::<_, i64>("triage")? != 0,
            last_used: parse_opt_timestamp(row.get::<_, Option<String>>("last_used")?)?,
            repo: row.get("repo")?,
            repo_id: RepoRowId::new(row.get("repo_id")?),
            entry_id: EntryId::new(row.get("entry_id")?),
            source: row.get("source")?,
            is_symlink: row.get::<_, i64>("is_symlink")? != 0,
            lfs_pointer: row.get::<_, i64>("lfs_pointer")? != 0,
        })
    }
}

fn parse_str<T: std::str::FromStr>(value: String) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| IndexError::Config(format!("stored value {value:?}: {error}")))
}

fn parse_opt<T: std::str::FromStr>(value: Option<String>) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    value.map(parse_str).transpose()
}

fn parse_opt_timestamp(value: Option<String>) -> Result<Option<Timestamp>> {
    value
        .map(|raw| {
            raw.parse::<Timestamp>()
                .map_err(|error| IndexError::Config(format!("stored timestamp {raw:?}: {error}")))
        })
        .transpose()
}

/// One facet bucket: a value, how many artifacts carry it, and whether the query selects it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetBucket {
    pub value: String,
    pub count: u64,
    pub active: bool,
}

/// The DISK PRESSURE summary under the facet rail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskPressure {
    pub reclaimable_bytes: u64,
    pub duplicate_blobs: u64,
}
