use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{ArtifactId, Base, Evidence, Role, Sha256, SubRole, format_size};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, EntryDetail};
use hoard_index::store::row::SearchRow;

use crate::api::common::{
    EntryView, Fact, PLUGS_INTO_LIMIT, PullCommands, RowView, authorize, blocking, clamp_limit,
    entry_views, facts, pull_commands,
};
use crate::api::compat::{CompatRowView, CompatView, compat_for};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const PULLS_LABEL: &str = "pulls on lan";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LimitParams {
    pub limit: Option<u32>,
}

/// What the inspector and the artifact detail read: the row, the ordered facts, the
/// evidence behind them, every path, and what plugs into it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactView {
    #[serde(flatten)]
    pub row: RowView,
    pub facts: Vec<Fact>,
    pub evidence: Vec<Evidence>,
    pub entries: Vec<EntryView>,
    /// Always empty: the catalog's artifact identity already merges every path holding the
    /// same content into `entries`, so no second list of them exists to show.
    pub duplicates: Vec<EntryView>,
    pub plugs_into: Vec<CompatRowView>,
    pub usage: UsageSummary,
    pub override_applied: bool,
}

/// The usage half of the inspector: how often this node has served the artifact.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageSummary {
    pub pulls: u64,
    pub last_used: Option<Timestamp>,
    pub label: String,
}

/// Every path and node holding one blob.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlobView {
    pub sha256: Sha256,
    pub size: u64,
    pub size_human: String,
    pub path_count: u32,
    pub first_seen: Timestamp,
    pub hashed_at: Option<Timestamp>,
    pub entries: Vec<EntryView>,
}

/// A user's verdict on what an artifact is.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverrideBody {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub sub_role: Option<String>,
}

impl OverrideBody {
    pub fn parts(&self) -> ApiResult<(Option<Role>, Option<SubRole>, Option<Base>)> {
        let role = self
            .role
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::parse::<Role>)
            .transpose()
            .map_err(ApiError::from)?;
        let sub_role = self
            .sub_role
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::parse::<SubRole>)
            .transpose()
            .map_err(ApiError::from)?;
        let base = self
            .base
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(Base::new);
        if role.is_none() && sub_role.is_none() && base.is_none() {
            return Err(ApiError::bad_request(
                "an override has to set role, sub_role or base",
            ));
        }
        Ok((role, sub_role, base))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverrideView {
    pub artifact_id: ArtifactId,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub base: Option<String>,
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClearedView {
    pub artifact_id: ArtifactId,
    pub cleared: bool,
}

/// The catalog's half of the inspector, read on one blocking thread.
struct Inspection {
    row: SearchRow,
    entries: Vec<EntryDetail>,
    evidence: Vec<Evidence>,
    plugs_into: Vec<CompatRowView>,
    override_applied: bool,
    usage: UsageSummary,
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ArtifactView>> {
    let id = artifact_id(&id)?;
    let host = state.node_name().to_owned();
    let now = Timestamp::now();
    let catalog = Arc::clone(state.catalog());
    let inspected = blocking(move || inspect(&catalog, id, now)).await?;
    Ok(Json(ArtifactView {
        facts: facts(&inspected.row),
        evidence: inspected.evidence,
        entries: entry_views(&inspected.entries, &host),
        duplicates: Vec::new(),
        plugs_into: inspected.plugs_into,
        usage: inspected.usage,
        override_applied: inspected.override_applied,
        row: RowView::from(&inspected.row),
    }))
}

fn inspect(catalog: &Catalog, id: ArtifactId, now: Timestamp) -> Result<Inspection, IndexError> {
    let row = require_row(catalog, id)?;
    let plugs = compat_for(catalog, &row, PLUGS_INTO_LIMIT, now)?;
    let usage = catalog.artifact_usage(id)?;
    Ok(Inspection {
        entries: catalog.entries_of(id)?,
        evidence: catalog
            .artifact_detail(id)?
            .map(|detail| detail.evidence)
            .unwrap_or_default(),
        plugs_into: plugs.rows,
        override_applied: catalog.artifact_override(id)?.is_some(),
        usage: UsageSummary {
            pulls: usage.pulls,
            last_used: usage.last_used,
            label: format!("{} {PULLS_LABEL}", usage.pulls),
        },
        row,
    })
}

pub async fn compat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LimitParams>,
) -> ApiResult<Json<CompatView>> {
    let id = artifact_id(&id)?;
    let limit = clamp_limit(params.limit);
    let now = Timestamp::now();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let row = require_row(&catalog, id)?;
        compat_for(&catalog, &row, limit, now)
    })
    .await?;
    Ok(Json(view))
}

pub async fn pull_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<PullCommands>> {
    let id = artifact_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let row = blocking(move || require_row(&catalog, id)).await?;
    let repo = hoard_core::RepoId::parse_qualified(&row.repo).map_err(ApiError::from)?;
    let path = row.abs_path.clone().or(Some(row.path.clone()));
    Ok(Json(pull_commands(&state, &repo, path)))
}

pub async fn put_override(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<OverrideBody>,
) -> ApiResult<Json<OverrideView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let (role, sub_role, base) = body.parts()?;
    let catalog = Arc::clone(state.catalog());
    let row = blocking(move || {
        catalog.put_override(id, role, sub_role, base)?;
        require_row(&catalog, id)
    })
    .await?;
    Ok(Json(OverrideView {
        artifact_id: id,
        role: row.role,
        sub_role: row.sub_role,
        base: crate::api::common::base_label(&row.base, row.base_label.as_deref()),
        facts: facts(&row),
    }))
}

pub async fn delete_override(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<ClearedView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let cleared = blocking(move || {
        require_row(&catalog, id)?;
        catalog.clear_override(id)
    })
    .await?;
    Ok(Json(ClearedView {
        artifact_id: id,
        cleared,
    }))
}

pub async fn blob_detail(
    State(state): State<AppState>,
    Path(sha256): Path<String>,
) -> ApiResult<Json<BlobView>> {
    let sha256: Sha256 = sha256.parse().map_err(ApiError::from)?;
    let host = state.node_name().to_owned();
    let now = Timestamp::now();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let row = catalog
            .artifact_by_sha256(&sha256)?
            .ok_or_else(|| IndexError::not_found("blob", sha256.to_hex()))?;
        let first_seen = catalog
            .artifact_detail(row.artifact_id)?
            .and_then(|detail| detail.first_seen)
            .unwrap_or(now);
        let entries = catalog.entries_of(row.artifact_id)?;
        Ok::<_, IndexError>(BlobView {
            sha256,
            size: row.size,
            size_human: format_size(row.size),
            path_count: row.path_count,
            first_seen,
            hashed_at: None,
            entries: entry_views(&entries, &host),
        })
    })
    .await?;
    Ok(Json(view))
}

/// The artifact one identity names, or the 404 the routes answer with when the catalog
/// holds no such row.
fn require_row(catalog: &Catalog, id: ArtifactId) -> Result<SearchRow, IndexError> {
    catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::not_found("artifact", id.to_string()))
}

/// Reads `a_17` or a bare row id out of a path segment.
pub fn artifact_id(raw: &str) -> ApiResult<ArtifactId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("artifact {raw}")))
}
