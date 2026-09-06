//! The HuggingFace Hub face: the routes `huggingface_hub`, the `hf` CLI and every loader
//! that speaks `HF_ENDPOINT` call, mapped onto `hoard-mirror`.
//!
//! The handlers here parse a route, hand the request to the mirror and render what it
//! returns. Nothing decides what a response says: revision resolution, byte sources, the
//! resolve headers and the block cache all live one crate down.

pub(crate) mod files;
pub(crate) mod meta;
pub(crate) mod proxy;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::routing::{get, post};

use hoard_core::{RepoId, RepoType, clean_repo_path};
use hoard_index::IndexError;
use hoard_index::catalog::Catalog;
use hoard_mirror::upstream::Authorization;

use crate::error::{HubError, HubResult};
use crate::state::AppState;

/// The revision every route without one means.
pub(crate) const DEFAULT_REVISION: &str = "main";

/// The leading path segments of a repo reference, named the same way in every router so
/// the Hub face and the git face can be merged into one application.
const REFERENCE_PARAMS: [&str; 3] = ["p1", "p2", "p3"];

/// A git object name is forty hex characters and a content hash is sixty-four; the CDN
/// catch-all matches nothing else, which is what keeps it from swallowing real routes.
const OBJECT_NAME_LENGTHS: [usize; 2] = [40, 64];

/// Every Hub route, named by its full path.
///
/// Registration order carries meaning in one place only: the three segment CDN catch-all
/// `/{p1}/{p2}/{hash_file}` comes last, and its handler answers 404 unless the final
/// segment is a forty or sixty-four character hex name.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/whoami-v2", get(proxy::whoami))
        .route("/api/{repo_type}", get(meta::list))
        .route("/api/{repo_type}/{p1}", get(meta::repo_info))
        .route("/api/{repo_type}/{p1}/{p2}", get(meta::repo_info))
        .route(
            "/api/{repo_type}/{p1}/revision/{revision}",
            get(meta::repo_info),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/revision/{revision}",
            get(meta::repo_info),
        )
        .route("/api/{repo_type}/{p1}/tree/{revision}", get(meta::tree))
        .route(
            "/api/{repo_type}/{p1}/tree/{revision}/{*path}",
            get(meta::tree),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/tree/{revision}",
            get(meta::tree),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/tree/{revision}/{*path}",
            get(meta::tree),
        )
        .route(
            "/api/{repo_type}/{p1}/paths-info/{revision}",
            post(meta::paths_info).head(meta::paths_info),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/paths-info/{revision}",
            post(meta::paths_info).head(meta::paths_info),
        )
        .route(
            "/api/{repo_type}/{p1}/commits/{revision}",
            get(meta::commits),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/commits/{revision}",
            get(meta::commits),
        )
        .route(
            "/api/{repo_type}/{p1}/xet-read-token/{revision}",
            get(proxy::xet_read_token),
        )
        .route(
            "/api/{repo_type}/{p1}/{p2}/xet-read-token/{revision}",
            get(proxy::xet_read_token),
        )
        .route("/xet-bridge-us/{repo_hash}/{hash}", get(proxy::xet_bridge))
        .route("/{p1}/resolve/{revision}/{*path}", get(files::resolve))
        .route("/{p1}/{p2}/resolve/{revision}/{*path}", get(files::resolve))
        .route(
            "/{p1}/{p2}/{p3}/resolve/{revision}/{*path}",
            get(files::resolve),
        )
        .route(
            "/repos/{d1}/{d2}/{hash_repo}/{hash_file}",
            get(files::cdn_object),
        )
        .route("/{p1}/{p2}/{hash_file}", get(files::cdn_catch_all))
}

/// The path parameters of one request, read by name rather than by position so every
/// spelling of a repo reference reaches the same handler.
pub(crate) type PathParams = HashMap<String, String>;

/// The repository a route names, in whichever of the accepted forms it was spelled.
///
/// `{repo_type}` is explicit under `/api`; at the root the first segment is a repo type
/// when it parses as one and an organization otherwise, which is the ambiguity the Hub
/// resolves by hand.
pub(crate) fn repo_of(params: &PathParams) -> HubResult<RepoId> {
    let segments: Vec<&str> = REFERENCE_PARAMS
        .iter()
        .filter_map(|name| params.get(*name))
        .map(String::as_str)
        .collect();
    match params.get("repo_type") {
        Some(repo_type) => typed(repo_type, &segments),
        None => match segments.as_slice() {
            [name] => named(RepoType::Models, &[name]),
            [head, tail] => match head.parse::<RepoType>() {
                Ok(repo_type) => named(repo_type, &[tail]),
                Err(_) => named(RepoType::Models, &[head, tail]),
            },
            [repo_type, org, name] => typed(repo_type, &[org, name]),
            _ => Err(HubError::page_not_found()),
        },
    }
}

fn typed(repo_type: &str, reference: &[&str]) -> HubResult<RepoId> {
    let repo_type = repo_type
        .parse::<RepoType>()
        .map_err(|_| HubError::page_not_found())?;
    named(repo_type, reference)
}

fn named(repo_type: RepoType, reference: &[&str]) -> HubResult<RepoId> {
    let mut joined = String::new();
    for segment in reference {
        if !is_clean(segment) {
            return Err(HubError::page_not_found());
        }
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(segment);
    }
    RepoId::parse_ref(repo_type, &joined).map_err(|_| HubError::page_not_found())
}

/// Whether a segment survives Hub path cleaning unchanged, which rejects `..` rather than
/// stripping it.
fn is_clean(segment: &str) -> bool {
    !segment.is_empty() && clean_repo_path(segment).as_deref() == Some(segment)
}

/// One required path parameter.
pub(crate) fn param<'a>(params: &'a PathParams, name: &str) -> HubResult<&'a str> {
    params
        .get(name)
        .map(String::as_str)
        .ok_or_else(HubError::page_not_found)
}

/// The revision a route asked for, defaulting to the tip of the default branch.
pub(crate) fn revision_of(params: &PathParams) -> &str {
    params
        .get("revision")
        .map(String::as_str)
        .map(str::trim)
        .filter(|revision| !revision.is_empty())
        .unwrap_or(DEFAULT_REVISION)
}

/// The path inside a revision, empty for the root of a tree.
pub(crate) fn path_of(params: &PathParams) -> HubResult<&str> {
    let path = params.get("path").map(String::as_str).unwrap_or_default();
    match clean_repo_path(path) {
        Some(_) => Ok(path),
        None => Err(HubError::entry_not_found()),
    }
}

/// Whether a segment is a git object name or a content hash.
pub(crate) fn is_object_name(value: &str) -> bool {
    OBJECT_NAME_LENGTHS.contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The credentials a client presented, forwarded upstream and never used locally.
pub(crate) fn authorization(headers: &HeaderMap) -> Option<Authorization> {
    Authorization::from_header(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

/// Who asked, as the usage log records it: the forwarded address when a proxy named one,
/// otherwise the peer this connection came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Client(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let forwarded = parts
            .headers
            .get("x-forwarded-for")
            .or_else(|| parts.headers.get("x-real-ip"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Ok(Client(forwarded.or_else(|| {
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(address)| address.ip().to_string())
        })))
    }
}

/// Runs one catalog read off the reactor. The catalog is a file, so a Hub route reads it on
/// a blocking thread and reports a failed read the way every other index failure is.
pub(crate) async fn on_catalog<T, F>(catalog: &Arc<Catalog>, f: F) -> HubResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Catalog) -> Result<T, IndexError> + Send + 'static,
{
    let catalog = Arc::clone(catalog);
    tokio::task::spawn_blocking(move || f(&catalog))
        .await
        .map_err(|_| HubError::from(IndexError::TaskJoin))?
        .map_err(HubError::from)
}
