use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::ShelfId;
use hoard_index::IndexError;
use hoard_index::catalog::ShelfRecord;

use crate::api::common::{authorize, blocking};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// A shelf as the search dropdown shows it. `count` is how many resources the
/// shelf's query answers, and null when it was not counted, whatever the reason: the
/// listing counts every shelf it can, and a write answers with the row it wrote rather than
/// re-running its query.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShelfView {
    pub id: ShelfId,
    pub name: String,
    pub query: String,
    pub builtin: bool,
    pub position: i64,
    pub count: Option<u64>,
}

impl ShelfView {
    fn counted(row: ShelfRecord, count: Option<u64>) -> Self {
        Self {
            id: row.id,
            name: row.name,
            query: row.query,
            builtin: row.builtin,
            position: row.position,
            count,
        }
    }

    /// A shelf a write returned, which carries no count because nothing counted it.
    fn written(row: ShelfRecord) -> Self {
        Self::counted(row, None)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShelfBody {
    pub name: String,
    pub query: String,
    #[serde(default)]
    pub position: Option<i64>,
}

impl ShelfBody {
    fn named(&self) -> ApiResult<&str> {
        let name = self.name.trim();
        match name.is_empty() {
            true => Err(ApiError::bad_request("a shelf needs a name")),
            false => Ok(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeletedView {
    pub id: ShelfId,
    pub deleted: bool,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<ShelfView>>> {
    let catalog = Arc::clone(state.catalog());
    let mut rows = blocking(move || {
        Ok(catalog
            .shelves_with_counts(Timestamp::now())?
            .into_iter()
            .map(|(row, count)| ShelfView::counted(row, count))
            .collect::<Vec<ShelfView>>())
    })
    .await?;
    let projects = crate::api::project_search::query_counts(
        &state,
        rows.iter().map(|row| row.query.clone()).collect(),
    )
    .await?;
    for (row, count) in rows.iter_mut().zip(projects) {
        row.count = row
            .count
            .zip(count)
            .map(|(artifacts, projects)| artifacts.saturating_add(projects));
    }
    Ok(Json(rows))
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ShelfBody>,
) -> ApiResult<Json<ShelfView>> {
    authorize(&state, &headers)?;
    let name = body.named()?.to_owned();
    let query = body.query.clone();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let position = match body.position {
            Some(position) => position,
            None => catalog.list_shelves()?.len() as i64,
        };
        Ok(ShelfView::written(
            catalog.create_shelf(&name, &query, position)?,
        ))
    })
    .await?;
    Ok(Json(view))
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ShelfBody>,
) -> ApiResult<Json<ShelfView>> {
    authorize(&state, &headers)?;
    let id = shelf_id(&id)?;
    let name = body.named()?.to_owned();
    let query = body.query.clone();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let existing = require_shelf(&catalog, id)?;
        let position = body.position.unwrap_or(existing.position);
        Ok(ShelfView::written(
            catalog.update_shelf(id, &name, &query, position)?,
        ))
    })
    .await?;
    Ok(Json(view))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<DeletedView>> {
    authorize(&state, &headers)?;
    let id = shelf_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let deleted = blocking(move || {
        let existing = require_shelf(&catalog, id)?;
        if existing.builtin {
            return Err(IndexError::Query(format!(
                "shelf {:?} ships with hoard and cannot be deleted",
                existing.name
            )));
        }
        catalog.delete_shelf(id)
    })
    .await
    .map_err(builtin_conflict)?;
    Ok(Json(DeletedView { id, deleted }))
}

fn require_shelf(
    catalog: &hoard_index::catalog::Catalog,
    id: ShelfId,
) -> Result<ShelfRecord, IndexError> {
    catalog
        .shelf(id)?
        .ok_or_else(|| IndexError::not_found("shelf", id.to_string()))
}

/// A refusal to delete a seeded shelf is a conflict, not a bad request: the query is fine,
/// the shelf is simply not the caller's to remove.
fn builtin_conflict(error: ApiError) -> ApiError {
    match error.message().contains("ships with hoard") {
        true => ApiError::conflict(error.message().to_owned()),
        false => error,
    }
}

fn shelf_id(raw: &str) -> ApiResult<ShelfId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("shelf {raw}")))
}
