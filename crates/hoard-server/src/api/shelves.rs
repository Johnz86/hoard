use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use hoard_core::ShelfId;
use hoard_index::IndexError;
use hoard_index::catalog::ShelfRecord;

use crate::api::common::{authorize, blocking};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// A shelf as the rail and the SHELVES tab show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShelfView {
    pub id: ShelfId,
    pub name: String,
    pub query: String,
    pub builtin: bool,
    pub position: i64,
}

impl From<ShelfRecord> for ShelfView {
    fn from(row: ShelfRecord) -> Self {
        Self {
            id: row.id,
            name: row.name,
            query: row.query,
            builtin: row.builtin,
            position: row.position,
        }
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
    let rows = blocking(move || {
        Ok(catalog
            .list_shelves()?
            .into_iter()
            .map(ShelfView::from)
            .collect::<Vec<ShelfView>>())
    })
    .await?;
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
        Ok(ShelfView::from(
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
        Ok(ShelfView::from(
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
