use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use hoard_core::ShelfId;
use hoard_index::IndexError;
use hoard_index::store::shelf::{self, ShelfRow};

use crate::api::common::authorize;
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

impl From<ShelfRow> for ShelfView {
    fn from(row: ShelfRow) -> Self {
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
    let rows = state
        .store()
        .call(|conn| {
            Ok(shelf::list_shelves(conn)?
                .into_iter()
                .map(ShelfView::from)
                .collect())
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
    let view = state
        .store()
        .call_mut(move |conn| {
            let position = body
                .position
                .unwrap_or_else(|| shelf::list_shelves(conn).map_or(0, |rows| rows.len() as i64));
            let id = shelf::create_shelf(conn, &name, &query, position)?;
            Ok(ShelfView::from(shelf::shelf(conn, id)?))
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
    let view = state
        .store()
        .call_mut(move |conn| {
            let existing = shelf::shelf(conn, id)?;
            let position = body.position.unwrap_or(existing.position);
            shelf::update_shelf(conn, id, &name, &query, position)?;
            Ok(ShelfView::from(shelf::shelf(conn, id)?))
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
    let deleted = state
        .store()
        .call_mut(move |conn| {
            let existing = shelf::shelf(conn, id)?;
            if existing.builtin {
                return Err(IndexError::Query(format!(
                    "shelf {:?} ships with hoard and cannot be deleted",
                    existing.name
                )));
            }
            shelf::delete_shelf(conn, id)
        })
        .await
        .map_err(builtin_conflict)?;
    Ok(Json(DeletedView { id, deleted }))
}

/// A refusal to delete a seeded shelf is a conflict, not a bad request: the query is fine,
/// the shelf is simply not the caller's to remove.
fn builtin_conflict(error: IndexError) -> ApiError {
    match error {
        IndexError::Query(message) if message.contains("ships with hoard") => {
            ApiError::conflict(message)
        }
        other => ApiError::from(other),
    }
}

fn shelf_id(raw: &str) -> ApiResult<ShelfId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("shelf {raw}")))
}
