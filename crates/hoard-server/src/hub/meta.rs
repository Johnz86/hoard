//! Repository metadata: repo info, tree, paths-info, commits and the listing endpoints.
//!
//! Every one of these is answered from the index in a single query, which is the whole
//! point of holding an index: a repository this node already has never costs a network
//! round trip to describe.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use hoard_core::{CommitSha, RepoId, RepoType};
use hoard_index::store::node::local_node;
use hoard_index::store::repo::repo_by_qualified;
use hoard_index::store::revision::resolve_revision;
use hoard_mirror::local::index_repo::{self, MAX_COMMITS, RepoListQuery};

use crate::error::{HubError, HubResult};
use crate::hub::{PathParams, param, path_of, repo_of, revision_of};
use crate::state::AppState;

/// The header every response that resolved a revision carries.
pub(crate) const REPO_COMMIT: &str = "x-repo-commit";

/// The 422 a paths-info request without a `paths` field answers with.
const MISSING_PATHS: &str = "field required: paths";

/// Repositories a listing returns when the request names no limit.
const DEFAULT_LIST_LIMIT: u32 = 100;
/// The most a listing returns however large a limit is asked for.
const MAX_LIST_LIMIT: u32 = 1_000;

const JSON_CONTENT_TYPE: &str = "application/json";

/// `recursive` and `expand`, both spelled as flags rather than as strict booleans because
/// clients send `?recursive=1` and a bare `?expand` as readily as `true`.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TreeParams {
    recursive: Option<String>,
    expand: Option<String>,
}

impl TreeParams {
    fn recursive(&self) -> bool {
        flag(self.recursive.as_deref())
    }

    fn expand(&self) -> bool {
        flag(self.expand.as_deref())
    }
}

fn flag(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("true" | "1" | "yes" | ""))
}

/// The query the listing endpoints hoard adds beyond olah accept.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ListParams {
    search: Option<String>,
    author: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PathsInfoBody {
    paths: Vec<String>,
}

/// `HEAD`, `GET /api/{repo_type}/{ref}` and `.../revision/{revision}`.
pub(crate) async fn repo_info(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let info = index_repo::repo_info(state.store(), &repo, revision_of(&params)).await?;
    let commit = info.sha.parse::<CommitSha>().ok();
    json(&info, commit)
}

/// `HEAD`, `GET /api/{repo_type}/{ref}/tree/{revision}/{path}`.
pub(crate) async fn tree(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
    Query(query): Query<TreeParams>,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let path = path_of(&params)?;
    let sha = revision_sha(&state, &repo, revision_of(&params)).await?;
    let entries = index_repo::tree(
        state.store(),
        &repo,
        &sha,
        path,
        query.recursive(),
        query.expand(),
    )
    .await?;
    json(&entries, Some(sha))
}

/// `HEAD`, `POST /api/{repo_type}/{ref}/paths-info/{revision}`.
///
/// A `HEAD` carries no body and so names no paths; it answers with the headers a `POST`
/// would return, which is what a client probing the route is asking for.
pub(crate) async fn paths_info(
    State(state): State<AppState>,
    method: Method,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
    body: Bytes,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let sha = revision_sha(&state, &repo, revision_of(&params)).await?;
    if method == Method::HEAD {
        return json(&Vec::<serde_json::Value>::new(), Some(sha));
    }
    let paths = requested_paths(&headers, &body)?;
    let entries = index_repo::paths_info(state.store(), &repo, &sha, &paths).await?;
    json(&entries, Some(sha))
}

/// `HEAD`, `GET /api/{repo_type}/{ref}/commits/{revision}`.
pub(crate) async fn commits(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let sha = revision_sha(&state, &repo, revision_of(&params)).await?;
    let commits = index_repo::commits(state.store(), &repo, &sha, MAX_COMMITS).await?;
    json(&commits, Some(sha))
}

/// `GET /api/models`, `/api/datasets` and `/api/spaces`, backed by the index.
pub(crate) async fn list(
    State(state): State<AppState>,
    RoutePath(params): RoutePath<PathParams>,
    Query(query): Query<ListParams>,
) -> HubResult<Response> {
    let repo_type = param(&params, "repo_type")?
        .parse::<RepoType>()
        .map_err(|_| HubError::page_not_found())?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let listing = RepoListQuery {
        search: text(query.search.as_deref()),
        author: text(query.author.as_deref()),
        ..RepoListQuery::new(repo_type, limit)
    };
    let repos = index_repo::list_repos(state.store(), &listing).await?;
    json(&repos, None)
}

/// The commit a revision names, resolved against the index alone.
///
/// The metadata routes serve what this node holds, so a repository the index does not know
/// is the documented 401 rather than a network round trip that would answer the same way.
async fn revision_sha(state: &AppState, repo: &RepoId, revision: &str) -> HubResult<CommitSha> {
    let qualified = repo.qualified();
    let wanted = revision.to_owned();
    let resolved = state
        .store()
        .call(move |conn| {
            let node = local_node(conn)?;
            let Some(row) = repo_by_qualified(conn, &qualified, node.id)? else {
                return Ok(None);
            };
            Ok(Some(
                resolve_revision(conn, row.id, &wanted).map(|revision| revision.sha),
            ))
        })
        .await?;
    match resolved {
        Some(Ok(sha)) => Ok(sha),
        Some(Err(_)) => Err(HubError::revision_not_found(revision)),
        None => Err(HubError::repo_not_found()),
    }
}

/// The paths a paths-info request named, as a form or as the JSON body the `hf` client
/// sends. Neither spelling carrying the field at all is the documented 422.
fn requested_paths(headers: &HeaderMap, body: &Bytes) -> HubResult<Vec<String>> {
    if is_json(headers) {
        let parsed: PathsInfoBody =
            serde_json::from_slice(body).map_err(|_| HubError::unprocessable(MISSING_PATHS))?;
        return Ok(parsed.paths);
    }
    let paths: Vec<String> = url::form_urlencoded::parse(body)
        .filter(|(key, _)| key == "paths")
        .map(|(_, value)| value.into_owned())
        .collect();
    if paths.is_empty() {
        return Err(HubError::unprocessable(MISSING_PATHS));
    }
    Ok(paths)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with(JSON_CONTENT_TYPE))
}

fn text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A metadata answer, carrying the commit it was read at.
fn json<T: Serialize>(body: &T, commit: Option<CommitSha>) -> HubResult<Response> {
    let mut response = Json(body).into_response();
    if let Some(commit) = commit {
        let value =
            HeaderValue::from_str(&commit.to_hex()).map_err(|_| HubError::proxy_invalid_data())?;
        response.headers_mut().insert(REPO_COMMIT, value);
    }
    Ok(response)
}
