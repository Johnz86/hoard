use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use hoard_core::{ArtifactId, Base, Role};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, NoiseRecord, TriageItem};
use hoard_index::row::SearchRow;

use crate::api::artifacts::artifact_id;
use crate::api::common::{Fact, PageParams, RowView, authorize, blocking, facts};
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

impl From<&TriageItem> for TriageView {
    fn from(item: &TriageItem) -> Self {
        Self {
            row: RowView::from(&item.row),
            guess: GuessView {
                role: item.guess_role,
                base: item.guess_base.clone(),
                confidence: item.confidence,
            },
            header: HeaderView {
                tensor_count: item.header.tensor_count,
                key_prefixes: item
                    .header
                    .key_prefixes
                    .iter()
                    .map(|(prefix, count)| PrefixView {
                        prefix: prefix.clone(),
                        count: *count,
                    })
                    .collect(),
                hidden_dim: item.header.hidden_dim,
                dtype: item.header.dtype.clone(),
            },
        }
    }
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
    let catalog = Arc::clone(state.catalog());
    let queue = blocking(move || catalog.triage_queue(limit)).await?;
    Ok(Json(queue.iter().map(TriageView::from).collect()))
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
    let catalog = Arc::clone(state.catalog());
    let row = blocking(move || {
        catalog.put_override(id, Some(role), sub_role, base)?;
        require_row(&catalog, id)
    })
    .await?;
    Ok(Json(ConfirmView {
        artifact_id: id,
        role: row.role,
        base: crate::api::common::base_label(&row.base, row.base_label.as_deref()),
        facts: facts(&row),
    }))
}

pub async fn reject(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<RejectView>> {
    authorize(&state, &headers)?;
    let id = artifact_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let noise = blocking(move || {
        let entries = catalog.entries_of(id)?;
        if entries.is_empty() {
            return Err(IndexError::not_found("artifact", id.to_string()));
        }
        Ok(entries
            .into_iter()
            .map(|entry| entry.abs_path.unwrap_or(entry.rel_path))
            .collect::<Vec<_>>())
    })
    .await?;
    let paths = noise.clone();
    let catalog = Arc::clone(state.catalog());
    blocking(move || {
        for path in &paths {
            catalog.add_noise(path, Some("rejected from triage"))?;
        }
        catalog.clear_triage(id)
    })
    .await?;
    Ok(Json(RejectView {
        artifact_id: id,
        noise,
    }))
}

pub async fn noise(State(state): State<AppState>) -> ApiResult<Json<Vec<NoiseRecord>>> {
    let catalog = Arc::clone(state.catalog());
    let rows = blocking(move || catalog.list_noise()).await?;
    Ok(Json(rows))
}

pub async fn delete_noise(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult<Json<NoiseDeleted>> {
    authorize(&state, &headers)?;
    let catalog = Arc::clone(state.catalog());
    let removed = blocking(move || catalog.remove_noise(id)).await?;
    if !removed {
        return Err(ApiError::not_found(format!("noise path {id}")));
    }
    Ok(Json(NoiseDeleted { id, removed }))
}

/// The artifact one identity names, or the 404 the routes answer with when the catalog
/// holds no such row.
fn require_row(catalog: &Catalog, id: ArtifactId) -> Result<SearchRow, IndexError> {
    catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::not_found("artifact", id.to_string()))
}
