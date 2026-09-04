//! The routes hoard forwards rather than answers: the identity call and the two Xet
//! endpoints `hf_xet` follows.

use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;

use hoard_mirror::upstream::ProxyResponse;

use crate::error::{HubError, HubResult};
use crate::hub::{PathParams, authorization, param, repo_of, revision_of};
use crate::state::AppState;

/// Headers that describe one hop and would misdescribe the next one.
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "transfer-encoding",
    "upgrade",
];

/// `GET /api/whoami-v2`. Offline this answers with a synthetic anonymous identity, so
/// `hf auth whoami` answers instead of hanging.
pub(crate) async fn whoami(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let auth = authorization(&headers);
    let answer = state.mirror().whoami(auth.as_ref()).await?;
    forward(answer)
}

/// `HEAD`, `GET /api/{repo_type}/{ref}/xet-read-token/{revision}`.
pub(crate) async fn xet_read_token(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let auth = authorization(&headers);
    let answer = state
        .mirror()
        .xet_read_token(&repo, revision_of(&params), auth.as_ref())
        .await?;
    forward(answer)
}

/// `HEAD`, `GET /xet-bridge-us/{repo_hash}/{hash}`.
pub(crate) async fn xet_bridge(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let repo_hash = param(&params, "repo_hash")?;
    let hash = param(&params, "hash")?;
    let auth = authorization(&headers);
    let answer = state
        .mirror()
        .xet_bridge(repo_hash, hash, auth.as_ref())
        .await?;
    forward(answer)
}

/// An upstream answer as this node returns it: the status, the headers and the body it
/// gave, minus the framing of the hop it arrived over.
fn forward(answer: ProxyResponse) -> HubResult<Response> {
    let status = StatusCode::from_u16(answer.status).map_err(|_| HubError::proxy_invalid_data())?;
    let mut response = Response::new(Body::from(answer.body));
    *response.status_mut() = status;
    for (name, value) in &answer.headers {
        if HOP_BY_HOP.contains(&name.as_str()) || name == header::CONTENT_LENGTH.as_str() {
            continue;
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| HubError::proxy_invalid_data())?;
        let value = HeaderValue::from_str(value).map_err(|_| HubError::proxy_invalid_data())?;
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}
