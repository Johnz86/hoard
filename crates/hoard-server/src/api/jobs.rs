use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{JobId, SourceId, format_size};
use hoard_index::IndexError;
use hoard_index::config::HashPolicy;
use hoard_index::scan::ScanOptions;
use hoard_index::store::job::{JobRow, JobState};
use hoard_index::store::{blob, job, node, source};

use crate::api::common::authorize;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The job kind a scan runs under, which is what the JOBS tab groups by.
const SCAN_KIND: &str = hoard_index::scan::SCAN_JOB;
/// How long the handler waits for the scanner to open its job row before giving up.
const JOB_ATTEMPTS: u32 = 200;
const JOB_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// One nominated root and what the last scan of it counted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceView {
    pub id: SourceId,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub node: String,
    pub files: u64,
    pub blobs: u64,
    pub bytes: u64,
    pub bytes_human: String,
    pub last_scan: Option<Timestamp>,
    pub pending_hashes: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanBody {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
}

impl ScanBody {
    fn policy(&self) -> ApiResult<HashPolicy> {
        match self.hash.as_deref().map(str::trim) {
            None | Some("") | Some("later") | Some("background") => Ok(HashPolicy::Background),
            Some("now") => Ok(HashPolicy::Now),
            Some("never") => Ok(HashPolicy::Never),
            Some(other) => Err(ApiError::bad_request(format!(
                "{other:?} is not a hash policy"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanStarted {
    pub job_id: JobId,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct JobFilter {
    pub state: Option<String>,
}

pub async fn sources(State(state): State<AppState>) -> ApiResult<Json<Vec<SourceView>>> {
    let rows = state
        .store()
        .call(|conn| {
            let pending = blob::pending_hash_count(conn)?;
            let mut out = Vec::new();
            for record in source::list_sources(conn)? {
                out.push(SourceView {
                    id: record.id,
                    name: record.name,
                    path: record.path.to_string_lossy().into_owned(),
                    kind: record.kind.as_str().to_owned(),
                    node: node::node(conn, record.node_row)?.name,
                    files: record.file_count,
                    blobs: record.blob_count,
                    bytes: record.byte_count,
                    bytes_human: format_size(record.byte_count),
                    last_scan: record.last_scan,
                    pending_hashes: pending,
                });
            }
            Ok(out)
        })
        .await?;
    Ok(Json(rows))
}

pub async fn scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ScanBody>,
) -> ApiResult<Json<ScanStarted>> {
    authorize(&state, &headers)?;
    let hash = body.policy()?;
    let wanted = body
        .source
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let source = match wanted {
        Some(wanted) => Some(resolve_source(&state, &wanted).await?),
        None => None,
    };
    let previous = newest_scan_job(&state).await?;

    let background = state.clone();
    let options = ScanOptions { source, hash };
    tokio::spawn(async move {
        if let Err(error) = hand_off_to_scanner(&background, options).await {
            tracing::error!(%error, "the scan failed");
        }
    });
    let job_id = await_scan_job(&state, previous).await?;
    Ok(Json(ScanStarted { job_id }))
}

/// The single call into `hoard_index::scan`, which owns the job row it reports through.
async fn hand_off_to_scanner(state: &AppState, options: ScanOptions) -> Result<(), IndexError> {
    let config = state.config().index.clone();
    hoard_index::scan::scan(state.store(), &config, options, &|_| {}).await?;
    Ok(())
}

/// The scanner records its own job as its first act, so the handler answers with that row
/// rather than opening a second one beside it.
async fn newest_scan_job(state: &AppState) -> Result<Option<JobId>, IndexError> {
    state
        .store()
        .call(|conn| {
            Ok(job::list_jobs(conn, None)?
                .into_iter()
                .find(|row| row.kind == SCAN_KIND)
                .map(|row| row.id))
        })
        .await
}

async fn await_scan_job(state: &AppState, previous: Option<JobId>) -> ApiResult<JobId> {
    for _ in 0..JOB_ATTEMPTS {
        match newest_scan_job(state).await? {
            Some(job) if Some(job) != previous => return Ok(job),
            _ => tokio::time::sleep(JOB_POLL).await,
        }
    }
    Err(ApiError::internal("the scan did not start"))
}

/// Reads what a scan request named: a configured source name, or the name, id or path of a
/// source the index already knows, resolved back to the name the scanner accepts.
async fn resolve_source(state: &AppState, wanted: &str) -> ApiResult<String> {
    if state.config().index.source(wanted).is_some() {
        return Ok(wanted.to_owned());
    }
    let probe = wanted.to_owned();
    let stored = state
        .store()
        .call(move |conn| {
            Ok(source::list_sources(conn)?.into_iter().find(|record| {
                record.name == probe
                    || record.id.to_string() == probe
                    || record.path.to_string_lossy() == probe
            }))
        })
        .await?;
    stored
        .map(|record| record.name)
        .filter(|name| state.config().index.source(name).is_some())
        .ok_or_else(|| ApiError::not_found(format!("source {wanted}")))
}

pub async fn list(
    State(state): State<AppState>,
    Query(filter): Query<JobFilter>,
) -> ApiResult<Json<Vec<JobRow>>> {
    let wanted = filter
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse::<JobState>)
        .transpose()
        .map_err(ApiError::from)?;
    let rows = state
        .store()
        .call(move |conn| job::list_jobs(conn, wanted))
        .await?;
    Ok(Json(rows))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<JobRow>> {
    let id = job_id(&id)?;
    let row = state.store().call(move |conn| job::job(conn, id)).await?;
    Ok(Json(row))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<JobRow>> {
    authorize(&state, &headers)?;
    let id = job_id(&id)?;
    let row = state
        .store()
        .call_mut(move |conn| {
            let existing = job::job(conn, id)?;
            if existing.state.is_final() {
                return Err(IndexError::Query(format!(
                    "job {id} already finished as {}",
                    existing.state
                )));
            }
            job::cancel_job(conn, id)?;
            job::job(conn, id)
        })
        .await
        .map_err(finished_conflict)?;
    Ok(Json(row))
}

/// Cancelling a job that has already stopped is a conflict with its state, not a bad
/// request.
fn finished_conflict(error: IndexError) -> ApiError {
    match error {
        IndexError::Query(message) if message.contains("already finished") => {
            ApiError::conflict(message)
        }
        other => ApiError::from(other),
    }
}

fn job_id(raw: &str) -> ApiResult<JobId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("job {raw}")))
}
