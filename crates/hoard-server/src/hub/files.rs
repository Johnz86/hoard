//! File bytes: the resolve routes, the CDN pass-through and the usage log every served
//! response writes.
//!
//! What a resolve answers with is decided by `hoard-mirror`: the status, the headers and
//! the framing come back from `served_response` and the body from `read_range`, so this
//! module frames them as HTTP and measures what actually left the node.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::TryStreamExt;

use hoard_core::{ArtifactId, CommitSha, RepoId, RepoRowId, Sha256};
use hoard_index::catalog::Catalog;
use hoard_mirror::MirrorError;
use hoard_mirror::git::LfsLocation;
use hoard_mirror::resolve::{FileAnswer, FileRequest, IngestTrigger, read_range, served_response};
use hoard_mirror::usage::{ServedRequest, log_served};

use crate::error::{HubError, HubResult};
use crate::hub::meta::REPO_COMMIT;
use crate::hub::{
    Client, PathParams, authorization, is_object_name, on_catalog, param, path_of, repo_of,
    revision_of,
};
use crate::state::AppState;

/// `HEAD`, `GET /{ref}/resolve/{revision}/{path}` in all three reference forms.
pub(crate) async fn resolve(
    State(state): State<AppState>,
    method: Method,
    client: Client,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let repo = repo_of(&params)?;
    let path = path_of(&params)?;
    let auth = authorization(&headers);
    let head = method == Method::HEAD;
    let revision = revision_of(&params);
    let opened = state.note_upstream(
        state
            .mirror()
            .open_file(
                FileRequest::new(&repo, revision, path)
                    .with_auth(auth.as_ref())
                    .head(head),
            )
            .await,
    );
    let answer = match opened {
        Ok(answer) => answer,
        Err(error) => return Err(refine(&state, &repo, revision, error).await),
    };
    serve(
        &state,
        &answer,
        Served::new(&repo, path, head, client),
        &headers,
    )
    .await
}

/// A repository this node holds answers for a path it does not have with `EntryNotFound`.
///
/// The mirror reports a file it cannot reach upstream as a denied repository, which is the
/// right answer for a repository this node has never seen and the wrong one for a path
/// that is simply not in a revision it does hold.
async fn refine(state: &AppState, repo: &RepoId, revision: &str, error: MirrorError) -> HubError {
    match error {
        MirrorError::RepoNotFound(_) if holds(state, repo, revision).await => {
            HubError::entry_not_found()
        }
        other => other.into(),
    }
}

async fn holds(state: &AppState, repo: &RepoId, revision: &str) -> bool {
    let wanted = repo.clone();
    let named = revision.to_owned();
    on_catalog(state.catalog(), move |catalog| {
        Ok(catalog
            .repository(&wanted)?
            .is_some_and(|record| catalog.resolve_revision(&record, &named).is_some()))
    })
    .await
    .unwrap_or(false)
}

/// `HEAD`, `GET /repos/{d1}/{d2}/{hash_repo}/{hash_file}`, the CDN URL shape a client is
/// redirected to when it asks the Hub itself.
pub(crate) async fn cdn_object(
    State(state): State<AppState>,
    method: Method,
    client: Client,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let hash = param(&params, "hash_file")?.to_owned();
    by_content_hash(&state, method, client, &hash, &headers).await
}

/// `HEAD`, `GET /{ref}/{hash_file}`, the three segment CDN shape.
///
/// This route is registered last and matches only a git object name or a content hash, so
/// an ordinary two segment repository route and every longer route are reached first.
pub(crate) async fn cdn_catch_all(
    State(state): State<AppState>,
    method: Method,
    client: Client,
    RoutePath(params): RoutePath<PathParams>,
    headers: HeaderMap,
) -> HubResult<Response> {
    let hash = param(&params, "hash_file")?.to_owned();
    if !is_object_name(&hash) {
        return Err(HubError::page_not_found());
    }
    by_content_hash(&state, method, client, &hash, &headers).await
}

/// Serves the bytes a content hash names, from wherever this node holds them.
///
/// A completed cache entry is ingested into the index, so a hash the block cache holds is
/// a hash the index can name a repository, a revision and a path for.
async fn by_content_hash(
    state: &AppState,
    method: Method,
    client: Client,
    hash: &str,
    headers: &HeaderMap,
) -> HubResult<Response> {
    let hash = hash
        .parse::<Sha256>()
        .map_err(|_| HubError::entry_not_found())?;
    let location = locate_content(state.catalog(), &hash)
        .await?
        .ok_or_else(HubError::entry_not_found)?;
    let auth = authorization(headers);
    let head = method == Method::HEAD;
    let revision = location.revision.to_hex();
    let answer = state.note_upstream(
        state
            .mirror()
            .open_file(
                FileRequest::new(&location.repo, &revision, &location.path)
                    .with_auth(auth.as_ref())
                    .head(head),
            )
            .await,
    )?;
    let served = Served::new(&location.repo, &location.path, head, client).without_commit();
    serve(state, &answer, served, headers).await
}

/// Where in Hub terms the bytes of one content hash live, which is what both the CDN
/// routes and the git LFS batch answer with.
pub(crate) async fn locate_content(
    catalog: &Arc<Catalog>,
    hash: &Sha256,
) -> HubResult<Option<LfsLocation>> {
    Ok(locate_contents(catalog, vec![*hash]).await?.remove(hash))
}

/// Where the bytes of every named content hash live, resolved in one pass over the catalog.
///
/// An LFS batch names many objects at once, and one round trip to the catalog per object is
/// one blocking task per object for a request that reads a single snapshot anyway.
pub(crate) async fn locate_contents(
    catalog: &Arc<Catalog>,
    hashes: Vec<Sha256>,
) -> HubResult<HashMap<Sha256, LfsLocation>> {
    let located = on_catalog(catalog, move |catalog| catalog.entries_by_sha256(&hashes)).await?;
    let mut out = HashMap::with_capacity(located.len());
    for (hash, entry) in located {
        let Some(revision) = entry.revision_sha else {
            continue;
        };
        let repo =
            RepoId::parse_qualified(&entry.repo).map_err(|_| HubError::proxy_invalid_data())?;
        out.insert(hash, LfsLocation::new(repo, revision, entry.rel_path));
    }
    Ok(out)
}

/// What one served response has to record once its body has left the node.
struct Served {
    repo: RepoId,
    path: String,
    head: bool,
    client: Option<String>,
    /// The CDN routes are keyed by content hash rather than by revision, so they carry no
    /// `x-repo-commit`.
    commit_header: bool,
}

impl Served {
    fn new(repo: &RepoId, path: &str, head: bool, client: Client) -> Self {
        Self {
            repo: repo.clone(),
            path: path.to_owned(),
            head,
            client: client.0,
            commit_header: true,
        }
    }

    fn without_commit(mut self) -> Self {
        self.commit_header = false;
        self
    }
}

/// Frames one answer as an HTTP response: the mirror's status and headers, and a body that
/// is streamed rather than buffered.
///
/// A `HEAD` produces the identical status and headers with no body and no read, which is
/// also why it never creates a cache entry.
async fn serve(
    state: &AppState,
    answer: &FileAnswer,
    served: Served,
    headers: &HeaderMap,
) -> HubResult<Response> {
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let rendered = served_response(answer, range);
    let status =
        StatusCode::from_u16(rendered.status).map_err(|_| HubError::proxy_invalid_data())?;
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Ok(HubError::range_not_satisfiable(answer.size).into_response());
    }
    let body = match served.head {
        true => Body::empty(),
        false => {
            let meter = Meter::new(state, &served, answer, rendered.status);
            let stream = read_range(answer, range).await?;
            Body::from_stream(stream.inspect_ok(move |chunk| meter.wrote(chunk.len() as u64)))
        }
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in &rendered.headers {
        if !served.commit_header && name == REPO_COMMIT {
            continue;
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| HubError::proxy_invalid_data())?;
        let value = HeaderValue::from_str(value).map_err(|_| HubError::proxy_invalid_data())?;
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}

/// Counts what a body delivered and appends it to the usage log once the response ends.
///
/// Bytes and elapsed time are only known when the stream finishes or the client gives up,
/// so the row is written on drop rather than when the headers were decided.
struct Meter {
    inner: Option<Record>,
    bytes: Arc<AtomicU64>,
    started: Instant,
}

impl Meter {
    fn new(state: &AppState, served: &Served, answer: &FileAnswer, status: u16) -> Self {
        Self {
            inner: Some(Record {
                catalog: Arc::clone(state.catalog()),
                repo: served.repo.clone(),
                commit: answer.commit,
                path: served.path.clone(),
                client: served.client.clone(),
                status,
                bytes: 0,
                elapsed: Duration::ZERO,
                ingest: answer.ingest().cloned(),
            }),
            bytes: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
        }
    }

    fn wrote(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        let Some(mut record) = self.inner.take() else {
            return;
        };
        record.bytes = self.bytes.load(Ordering::Relaxed);
        record.elapsed = self.started.elapsed();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(record.run());
        }
    }
}

struct Record {
    catalog: Arc<Catalog>,
    repo: RepoId,
    commit: CommitSha,
    path: String,
    client: Option<String>,
    status: u16,
    bytes: u64,
    elapsed: Duration,
    ingest: Option<IngestTrigger>,
}

impl Record {
    async fn run(self) {
        if let Some(ingest) = &self.ingest
            && let Err(error) = ingest.run().await
        {
            tracing::warn!(repo = %self.repo, %error, "a cached file was not ingested");
        }
        let (repo, artifact) = attribute(&self.catalog, &self.repo, &self.commit, &self.path).await;
        let request = ServedRequest::new(self.path.clone(), self.status)
            .from_client(self.client.clone())
            .for_repo(repo)
            .for_artifact(artifact)
            .served(self.bytes, self.elapsed);
        if let Err(error) = log_served(&self.catalog, &request).await {
            tracing::warn!(repo = %self.repo, %error, "a served response was not logged");
        }
    }
}

/// Which repository row and artifact a served path belongs to, so the USAGE tab and the
/// `used:` key see the pull.
async fn attribute(
    catalog: &Arc<Catalog>,
    repo: &RepoId,
    commit: &CommitSha,
    path: &str,
) -> (Option<RepoRowId>, Option<ArtifactId>) {
    let catalog = Arc::clone(catalog);
    let wanted = repo.clone();
    let commit = commit.to_hex();
    let path = path.to_owned();
    let found = tokio::task::spawn_blocking(move || {
        let Some(record) = catalog.repository(&wanted)? else {
            return Ok(None);
        };
        let artifact = match catalog.resolve_revision(&record, &commit) {
            Some(revision) => catalog
                .revision_entries(record.id, revision.ordinal)?
                .into_iter()
                .find(|entry| entry.rel_path == path)
                .and_then(|entry| entry.artifact_id),
            None => None,
        };
        Ok::<_, hoard_index::IndexError>(Some((record.id, artifact)))
    })
    .await;
    match found {
        Ok(Ok(Some((repo, artifact)))) => (Some(repo), artifact),
        Ok(Ok(None)) => (None, None),
        Ok(Err(error)) => {
            tracing::warn!(%error, "a served response could not be attributed");
            (None, None)
        }
        Err(error) => {
            tracing::warn!(%error, "attributing a served response was cancelled");
            (None, None)
        }
    }
}
