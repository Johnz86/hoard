use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{ArtifactId, Base, Evidence, Role, Sha256, SubRole, format_size};
use hoard_index::IndexError;
use hoard_index::store::entry::{entries_for_artifact, entries_for_blob};
use hoard_index::store::{artifact, blob, overrides};

use crate::api::common::{
    EntryView, Fact, PLUGS_INTO_LIMIT, PullCommands, RowView, authorize, clamp_limit, entry_views,
    facts, pull_commands, require_row, row_by_artifact,
};
use crate::api::compat::{CompatRowView, CompatView, compat_for};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// How many pulls the artifact usage summary counts as one LAN activity line.
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

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ArtifactView>> {
    let id = artifact_id(&id)?;
    let host = state.node_name().to_owned();
    let now = Timestamp::now();
    let view = state
        .store()
        .call(move |conn| {
            let row = require_row(conn, id)?;
            let record = artifact::artifact(conn, id)?;
            let entries = entry_views(conn, &entries_for_artifact(conn, id)?, &host)?;
            let mut duplicates = entry_views(conn, &entries_for_blob(conn, row.blob_id)?, &host)?;
            duplicates.retain(|candidate| {
                !entries
                    .iter()
                    .any(|entry| entry.path == candidate.path && entry.node == candidate.node)
            });
            let plugs = compat_for(conn, &row, PLUGS_INTO_LIMIT, now)?;
            Ok(ArtifactView {
                facts: facts(&row),
                evidence: artifact::evidence(conn, id)?,
                entries,
                duplicates,
                plugs_into: plugs.rows,
                usage: UsageSummary {
                    pulls: record.use_count,
                    last_used: record.last_used,
                    label: format!("{} {PULLS_LABEL}", record.use_count),
                },
                override_applied: overrides::artifact_override(conn, id)?.is_some(),
                row: RowView::from(&row),
            })
        })
        .await?;
    Ok(Json(view))
}

pub async fn compat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LimitParams>,
) -> ApiResult<Json<CompatView>> {
    let id = artifact_id(&id)?;
    let limit = clamp_limit(params.limit);
    let now = Timestamp::now();
    let view = state
        .store()
        .call(move |conn| {
            let row = require_row(conn, id)?;
            compat_for(conn, &row, limit, now)
        })
        .await?;
    Ok(Json(view))
}

pub async fn pull_command(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<PullCommands>> {
    let id = artifact_id(&id)?;
    let row = state
        .store()
        .call(move |conn| require_row(conn, id))
        .await?;
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
    let view = state
        .store()
        .call_mut(move |conn| {
            overrides::put_override(conn, id, role, sub_role, base)?;
            artifact::set_triage(conn, id, false)?;
            let row = require_row(conn, id)?;
            Ok(OverrideView {
                artifact_id: id,
                role: row.role,
                sub_role: row.sub_role,
                base: crate::api::common::base_label(&row.base, row.base_label.as_deref()),
                facts: facts(&row),
            })
        })
        .await?;
    Ok(Json(view))
}

pub async fn delete_override(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<ClearedView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let cleared = state
        .store()
        .call_mut(move |conn| {
            if row_by_artifact(conn, id)?.is_none() {
                return Err(IndexError::not_found("artifact", id.to_string()));
            }
            overrides::clear_override(conn, id)
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
    let view = state
        .store()
        .call(move |conn| {
            let record = blob::blob_by_sha256(conn, &sha256)?
                .ok_or_else(|| IndexError::not_found("blob", sha256.to_hex()))?;
            Ok(BlobView {
                sha256,
                size: record.size,
                size_human: format_size(record.size),
                path_count: record.path_count,
                first_seen: record.first_seen,
                hashed_at: record.hashed_at,
                entries: entry_views(conn, &entries_for_blob(conn, record.id)?, &host)?,
            })
        })
        .await?;
    Ok(Json(view))
}

/// Reads `a_17` or a bare row id out of a path segment.
pub fn artifact_id(raw: &str) -> ApiResult<ArtifactId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("artifact {raw}")))
}
