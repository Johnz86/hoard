use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::NodeId;
use hoard_index::store::blob;
use hoard_index::store::node::{self, NewNode};
use hoard_net::digest::{self, DEFAULT_LIMIT, DigestCursor, MAX_LIMIT};
use hoard_net::identity::NodeIdentity;
use hoard_net::{DigestPage, NetError};

use crate::api::common::authorize;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// One node of the LAN as the NODES tab and the pulse show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeView {
    pub id: NodeId,
    pub name: String,
    pub url: Option<String>,
    pub online: bool,
    pub indexed_bytes: u64,
    pub last_seen: Option<Timestamp>,
    #[serde(rename = "self")]
    pub is_self: bool,
}

impl From<node::NodeRow> for NodeView {
    fn from(row: node::NodeRow) -> Self {
        Self {
            id: row.node_id,
            name: row.name,
            url: row.url,
            online: row.online,
            indexed_bytes: row.indexed_bytes,
            last_seen: row.last_seen,
            is_self: row.is_self,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DigestParams {
    pub since: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerBody {
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<NodeView>>> {
    let rows = state
        .store()
        .call(|conn| {
            let (_, held) = blob::local_totals(conn)?;
            Ok(node::list_nodes(conn)?
                .into_iter()
                .map(|row| {
                    let is_self = row.is_self;
                    let mut view = NodeView::from(row);
                    if is_self {
                        view.indexed_bytes = held;
                    }
                    view
                })
                .collect())
        })
        .await?;
    Ok(Json(rows))
}

/// `GET /api/v1/peers/digest`: what this node holds, one keyset page at a time.
pub async fn digest(
    State(state): State<AppState>,
    Query(params): Query<DigestParams>,
) -> ApiResult<Json<DigestPage>> {
    let since = DigestCursor::from_param(params.since.as_deref()).map_err(net_error)?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let page = digest::collect(state.catalog(), since, limit)
        .await
        .map_err(net_error)?;
    Ok(Json(page))
}

/// `POST /api/v1/peers`: adopt a peer by address, which is the fallback for a LAN where
/// multicast never arrives.
pub async fn add_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PeerBody>,
) -> ApiResult<Json<NodeView>> {
    authorize(&state, &headers)?;
    let peer = NodeIdentity::from_address(&body.url).map_err(net_error)?;
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(peer.name())
        .to_owned();
    let node_id = peer.id().clone();
    let url = peer.url().to_string();
    let view = state
        .store()
        .call_mut(move |conn| {
            let row = node::upsert_node(conn, &NewNode::peer(node_id, name, url))?;
            Ok(NodeView::from(node::node(conn, row)?))
        })
        .await?;
    Ok(Json(view))
}

/// The HTTP shape of a networking failure. A malformed cursor or address is the caller's
/// mistake; a peer that cannot be read is this node's problem to report.
fn net_error(error: NetError) -> ApiError {
    match error {
        NetError::BadCursor(cursor) => {
            ApiError::bad_request(format!("cursor {cursor:?} is not from this digest"))
        }
        NetError::Config(message) => ApiError::bad_request(message),
        NetError::BadDigest { peer, reason } => {
            ApiError::bad_request(format!("peer {peer}: {reason}"))
        }
        NetError::Index(index) => ApiError::from(index),
        NetError::Mirror(mirror) => ApiError::from(mirror),
        other => {
            tracing::error!(error = %other, "a node route failed");
            ApiError::internal(other.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_is_adopted_under_the_address_it_answers_at() {
        let peer = NodeIdentity::from_address("laptop.lan:7099").expect("address parses");
        assert_eq!(peer.id().as_str(), "laptop-lan-7099");
        assert_eq!(peer.name(), "laptop.lan");
        assert!(NodeIdentity::from_address("not a url").is_err());
    }

    #[test]
    fn a_cursor_from_another_query_is_a_bad_request() {
        let error = net_error(NetError::BadCursor("abc".to_owned()));
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.code(), "BadRequest");
    }
}
