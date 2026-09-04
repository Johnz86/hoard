use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use hoard_core::{ArtifactId, Base, Role};
use hoard_index::IndexError;
use hoard_index::store::entry::entries_for_artifact;
use hoard_index::store::overrides::NoiseRow;
use hoard_index::store::{artifact, overrides};
use hoard_index::triage::header_facts;

use crate::api::artifacts::artifact_id;
use crate::api::common::{
    Conn, Fact, PageParams, RowView, authorize, facts, require_row, row_by_artifact,
};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// What inference guessed for an artifact it could not classify.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuessView {
    pub role: Role,
    pub base: Option<String>,
    pub confidence: f32,
}

/// The header facts the queue shows so a person can decide without opening the file.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HeaderView {
    pub tensor_count: u32,
    pub key_prefixes: Vec<PrefixView>,
    pub hidden_dim: Option<u64>,
    pub dtype: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrefixView {
    pub prefix: String,
    pub count: u32,
}

/// One row of the triage queue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageView {
    #[serde(flatten)]
    pub row: RowView,
    pub guess: GuessView,
    pub header: HeaderView,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfirmView {
    pub artifact_id: ArtifactId,
    pub role: Role,
    pub base: Option<String>,
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RejectView {
    pub artifact_id: ArtifactId,
    pub noise: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoiseDeleted {
    pub id: i64,
    pub removed: bool,
}

/// A verdict from the queue. A confirmation always names a role; the base is optional
/// because a checkpoint of an unknown family is still a checkpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmBody {
    pub role: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub sub_role: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> ApiResult<Json<Vec<TriageView>>> {
    let limit = params.limit();
    let rows = state
        .store()
        .call(move |conn| {
            let mut out = Vec::new();
            for record in artifact::triage_queue(conn, limit)? {
                let Some(row) = row_by_artifact(conn, record.artifact.id)? else {
                    continue;
                };
                out.push(TriageView {
                    guess: guess(conn, record.artifact.id, &row)?,
                    header: header(conn, record.artifact.id)?,
                    row: RowView::from(&row),
                });
            }
            Ok(out)
        })
        .await?;
    Ok(Json(rows))
}

pub async fn confirm(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> ApiResult<Json<ConfirmView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let role: Role = body.role.parse().map_err(ApiError::from)?;
    let sub_role = body
        .sub_role
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .transpose()
        .map_err(ApiError::from)?;
    let base = body
        .base
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(Base::new);
    let view = state
        .store()
        .call_mut(move |conn| {
            overrides::put_override(conn, id, Some(role), sub_role, base)?;
            artifact::set_triage(conn, id, false)?;
            let row = require_row(conn, id)?;
            Ok(ConfirmView {
                artifact_id: id,
                role: row.role,
                base: crate::api::common::base_label(&row.base, row.base_label.as_deref()),
                facts: facts(&row),
            })
        })
        .await?;
    Ok(Json(view))
}

pub async fn reject(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<RejectView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let view = state
        .store()
        .call_mut(move |conn| {
            let entries = entries_for_artifact(conn, id)?;
            if entries.is_empty() {
                return Err(IndexError::not_found("artifact", id.to_string()));
            }
            let mut noise = Vec::with_capacity(entries.len());
            for entry in &entries {
                let path = entry.abs_path.clone().unwrap_or_else(|| entry.path.clone());
                overrides::add_noise(conn, &path, Some("rejected from triage"))?;
                noise.push(path);
            }
            artifact::set_triage(conn, id, false)?;
            Ok(RejectView {
                artifact_id: id,
                noise,
            })
        })
        .await?;
    Ok(Json(view))
}

pub async fn noise(State(state): State<AppState>) -> ApiResult<Json<Vec<NoiseRow>>> {
    let rows = state.store().call(overrides::list_noise).await?;
    Ok(Json(rows))
}

pub async fn delete_noise(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult<Json<NoiseDeleted>> {
    authorize(&state, &headers)?;
    let removed = state
        .store()
        .call_mut(move |conn| overrides::remove_noise(conn, id))
        .await?;
    if !removed {
        return Err(ApiError::not_found(format!("noise path {id}")));
    }
    Ok(Json(NoiseDeleted { id, removed }))
}

/// The queue's guess: the strongest recorded evidence for a field, falling back to what the
/// artifact itself carries.
fn guess(
    conn: &Conn,
    id: ArtifactId,
    row: &hoard_index::store::row::SearchRow,
) -> Result<GuessView, IndexError> {
    let evidence = artifact::evidence(conn, id)?;
    let strongest = |field: &str| {
        evidence
            .iter()
            .find(|item| item.field == field)
            .map(|item| (item.value.clone(), item.confidence))
    };
    let (role, confidence) = match strongest("role") {
        Some((value, confidence)) => (
            Role::from_alias(&value).unwrap_or(row.role),
            confidence.max(row.role_confidence),
        ),
        None => (row.role, row.role_confidence),
    };
    let base = strongest("base")
        .map(|(value, _)| value)
        .or_else(|| crate::api::common::base_label(&row.base, row.base_label.as_deref()));
    Ok(GuessView {
        role,
        base,
        confidence,
    })
}

fn header(conn: &Conn, id: ArtifactId) -> Result<HeaderView, IndexError> {
    let Some(summary) = artifact::tensor_summary(conn, id)? else {
        return Ok(HeaderView::default());
    };
    let header = header_facts(&summary);
    Ok(HeaderView {
        tensor_count: header.tensor_count,
        key_prefixes: header
            .key_prefixes
            .into_iter()
            .map(|(prefix, count)| PrefixView { prefix, count })
            .collect(),
        hidden_dim: header.hidden_dim,
        dtype: header.dtype,
    })
}
