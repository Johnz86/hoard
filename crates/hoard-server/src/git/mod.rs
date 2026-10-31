//! The git smart HTTP face: `git clone` over the same port as the Hub face.
//!
//! Read-only by design. Only a repository this node holds as a git clone can be cloned;
//! an HF cache snapshot or a loose folder has no pack to serve and answers 404 here while
//! staying reachable on the Hub face.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use hoard_core::{RepoId, RepoRowId, RepoType, Sha256, clean_repo_path};
use hoard_mirror::git::{
    ADVERTISEMENT_CONTENT_TYPE, BatchRequest, CACHE_CONTROL, GitService, PUSH_REFUSED,
    RESULT_CONTENT_TYPE, advertise_refs, build_batch_response, is_clonable, lfs_batch,
    strip_git_suffix, upload_pack,
};

use crate::error::{HubError, HubResult};
use crate::hub::files::locate_contents;
use crate::hub::on_catalog;
use crate::state::AppState;

/// The leading path segments of a repo reference, named as the Hub face names them so both
/// routers merge into one application.
const REFERENCE_PARAMS: [&str; 2] = ["p1", "p2"];

/// How many objects one LFS batch may name.
///
/// A client asks for the objects of one checkout, and git-lfs itself batches in hundreds.
/// A larger request is a way to make this node read the index thousands of times for one
/// call, so it is refused rather than answered slowly.
const MAX_BATCH_OBJECTS: usize = 1_000;

/// Every git face route, for both the `org/repo` and the orgless spelling of a clone URL.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/git/projects/{project}/info/refs", get(advertise))
        .route("/git/projects/{project}/git-upload-pack", post(fetch))
        .route("/git/projects/{project}/git-receive-pack", post(push))
        .route(
            "/git/projects/{project}/info/lfs/objects/batch",
            post(batch),
        )
        .route("/{p1}/info/refs", get(advertise))
        .route("/{p1}/{p2}/info/refs", get(advertise))
        .route("/{p1}/git-upload-pack", post(fetch))
        .route("/{p1}/{p2}/git-upload-pack", post(fetch))
        .route("/{p1}/git-receive-pack", post(push))
        .route("/{p1}/{p2}/git-receive-pack", post(push))
        .route("/{p1}/info/lfs/objects/batch", post(batch))
        .route("/{p1}/{p2}/info/lfs/objects/batch", post(batch))
}

/// The `service` query parameter of the ref advertisement.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AdvertiseParams {
    service: Option<String>,
}

/// `GET /{org}/{repo}[.git]/info/refs?service=git-upload-pack`.
pub(crate) async fn advertise(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<HashMap<String, String>>,
    Query(query): Query<AdvertiseParams>,
) -> HubResult<Response> {
    let service = query
        .service
        .as_deref()
        .map(str::trim)
        .and_then(GitService::parse)
        .ok_or_else(HubError::page_not_found)?;
    if !service.is_read_only() {
        return Ok(refused());
    }
    let git_dir = clone_dir(&state, &params).await?;
    let body = advertise_refs(&git_dir).await?;
    Ok(pack_response(ADVERTISEMENT_CONTENT_TYPE, Body::from(body)))
}

/// `POST /{org}/{repo}[.git]/git-upload-pack`, streamed as the pack is produced.
pub(crate) async fn fetch(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<HashMap<String, String>>,
    body: Bytes,
) -> HubResult<Response> {
    let git_dir = clone_dir(&state, &params).await?;
    let packs = upload_pack(&git_dir, body).await?;
    Ok(pack_response(RESULT_CONTENT_TYPE, Body::from_stream(packs)))
}

/// `POST /{org}/{repo}[.git]/git-receive-pack`.
///
/// The MVP is read-only, so a request that carries the node write token is refused with
/// the reason and one that does not is refused as unauthorized.
pub(crate) async fn push(State(state): State<AppState>, headers: HeaderMap) -> HubResult<Response> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match state.authorize_write(presented) {
        Ok(()) => Ok(refused()),
        Err(error) => Ok(error.into_response()),
    }
}

/// `POST /{org}/{repo}[.git]/info/lfs/objects/batch`, with download hrefs pointing back at
/// this node's Hub resolve route.
pub(crate) async fn batch(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<HashMap<String, String>>,
    body: Bytes,
) -> HubResult<Response> {
    clone_dir(&state, &params).await?;
    let request: BatchRequest =
        serde_json::from_slice(&body).map_err(|_| HubError::unprocessable("invalid batch body"))?;
    if request.objects.len() > MAX_BATCH_OBJECTS {
        return Err(HubError::unprocessable("too many objects in one batch"));
    }
    let mut wanted: Vec<Sha256> = request.objects.iter().map(|object| object.oid).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let located = locate_contents(state.catalog(), wanted).await?;
    let answer = build_batch_response(&request, state.public_url(), &|oid| {
        located.get(oid).cloned()
    });
    let body = serde_json::to_vec(&answer).map_err(|_| HubError::proxy_invalid_data())?;
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(lfs_batch::CONTENT_TYPE),
    );
    Ok(response)
}

/// The repository a git route names. The clone URL carries no repo type, so the git face
/// serves models only; datasets and spaces stay on the Hub face.
fn repo_of(params: &HashMap<String, String>) -> HubResult<RepoId> {
    let mut reference = String::new();
    for name in REFERENCE_PARAMS {
        let Some(segment) = params.get(name) else {
            continue;
        };
        let segment = strip_git_suffix(segment);
        if segment.is_empty() || clean_repo_path(segment).as_deref() != Some(segment) {
            return Err(HubError::page_not_found());
        }
        if !reference.is_empty() {
            reference.push('/');
        }
        reference.push_str(segment);
    }
    RepoId::parse_ref(RepoType::Models, &reference).map_err(|_| HubError::page_not_found())
}

/// The clone directory of a git-source repository this node holds.
async fn clone_dir(state: &AppState, params: &HashMap<String, String>) -> HubResult<PathBuf> {
    if let Some(project) = params.get("project") {
        return project_dir(state, project).await;
    }
    let repo = repo_of(params)?;
    require_repo(state, &repo)
        .await?
        .ok_or_else(HubError::page_not_found)
}

async fn project_dir(state: &AppState, raw: &str) -> HubResult<PathBuf> {
    let id = strip_git_suffix(raw)
        .parse::<RepoRowId>()
        .map_err(|_| HubError::page_not_found())?;
    let record = on_catalog(state.catalog(), move |catalog| catalog.repository_by_id(id))
        .await?
        .filter(|record| is_clonable(record.kind))
        .ok_or_else(HubError::page_not_found)?;
    record
        .git_dir
        .map(PathBuf::from)
        .ok_or_else(HubError::page_not_found)
}

/// The clone directory of a repository this node can serve on the git face, or the
/// documented 404 when it holds the repository some other way or not at all.
async fn require_repo(state: &AppState, repo: &RepoId) -> HubResult<Option<PathBuf>> {
    let wanted = repo.clone();
    let found = on_catalog(state.catalog(), move |catalog| catalog.repository(&wanted)).await?;
    match found {
        Some(record) if is_clonable(record.kind) => Ok(record.git_dir.map(PathBuf::from)),
        Some(_) | None => Err(HubError::page_not_found()),
    }
}

fn pack_response(content_type: &'static str, body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    response
}

/// The one answer every push route gives while the mirror is read-only.
fn refused() -> Response {
    let mut response = Response::new(Body::from(PUSH_REFUSED));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
