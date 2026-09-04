use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{NodeId, Role, Sha256, State as ArtifactState};
use hoard_index::store::node::{self, NewNode};
use hoard_index::store::search_exec;

use crate::api::common::{authorize, clamp_limit, decode_cursor, next_cursor};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Digest rows are read in artifact order so a peer's cursor never skips or repeats one.
const DIGEST_ORDER: &str = "artifact_id ASC";
const DIGEST_WHERE: &str = "node_is_self = 1 AND blob_sha256 IS NOT NULL";
const DIGEST_KEY: &str = "peers-digest";

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

/// One compact row of the gossip digest another node reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DigestRow {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: String,
    pub repo: String,
    pub revision_sha: Option<hoard_core::CommitSha>,
    pub path: String,
    pub state: ArtifactState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DigestView {
    pub rows: Vec<DigestRow>,
    pub next_cursor: Option<String>,
    pub total: u64,
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
            Ok(node::list_nodes(conn)?
                .into_iter()
                .map(|row| NodeView {
                    id: row.node_id,
                    name: row.name,
                    url: row.url,
                    online: row.online,
                    indexed_bytes: row.indexed_bytes,
                    last_seen: row.last_seen,
                    is_self: row.is_self,
                })
                .collect())
        })
        .await?;
    Ok(Json(rows))
}

pub async fn digest(
    State(state): State<AppState>,
    Query(params): Query<DigestParams>,
) -> ApiResult<Json<DigestView>> {
    let limit = clamp_limit(params.limit);
    let offset = decode_cursor(params.since.as_deref(), DIGEST_KEY)?;
    let view = state
        .store()
        .call(move |conn| {
            let rows =
                search_exec::query_rows(conn, DIGEST_WHERE, &[], DIGEST_ORDER, limit, offset)?;
            let total = search_exec::count_rows(conn, DIGEST_WHERE, &[])?;
            let rows: Vec<DigestRow> = rows
                .iter()
                .filter_map(|row| {
                    Some(DigestRow {
                        sha256: row.blob_sha256?,
                        size: row.size,
                        role: row.role,
                        base: row.base.to_string(),
                        repo: row.repo.clone(),
                        revision_sha: row.revision_sha,
                        path: row.path.clone(),
                        state: row.state,
                    })
                })
                .collect();
            Ok(DigestView {
                next_cursor: next_cursor(offset, limit, rows.len(), total, DIGEST_KEY),
                rows,
                total,
            })
        })
        .await?;
    Ok(Json(view))
}

pub async fn add_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PeerBody>,
) -> ApiResult<Json<NodeView>> {
    authorize(&state, &headers)?;
    let url = body.url.trim().to_owned();
    let parsed = url::Url::parse(&url)
        .map_err(|error| ApiError::bad_request(format!("peer url {url:?}: {error}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::bad_request(format!("peer url {url:?} names no host")))?
        .to_owned();
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| host.clone());
    let node_id = NodeId::new(peer_identity(&parsed)).map_err(ApiError::from)?;
    let view = state
        .store()
        .call_mut(move |conn| {
            let row = node::upsert_node(conn, &NewNode::peer(node_id, name, url))?;
            let record = node::node(conn, row)?;
            Ok(NodeView {
                id: record.node_id,
                name: record.name,
                url: record.url,
                online: record.online,
                indexed_bytes: record.indexed_bytes,
                last_seen: record.last_seen,
                is_self: record.is_self,
            })
        })
        .await?;
    Ok(Json(view))
}

/// A stable identity for a statically configured peer, derived from the address it is
/// reachable at until the peer answers with its own node id.
fn peer_identity(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or("peer");
    let port = url
        .port()
        .map(|port| format!("-{port}"))
        .unwrap_or_default();
    let raw = format!("{host}{port}");
    let identity: String = raw
        .chars()
        .map(|character| match character.is_ascii_alphanumeric() {
            true => character.to_ascii_lowercase(),
            false => '-',
        })
        .collect();
    match identity.is_empty() {
        true => "peer".to_owned(),
        false => identity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_identity_is_derived_from_its_address() {
        let url = url::Url::parse("http://laptop.lan:7099").expect("url parses");
        assert_eq!(peer_identity(&url), "laptop-lan-7099");
        assert!(NodeId::new(peer_identity(&url)).is_ok());
    }
}
