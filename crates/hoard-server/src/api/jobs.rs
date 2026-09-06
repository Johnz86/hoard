use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{JobId, SourceId, format_size};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, JobProgress, JobRecord, JobState, RescanReport};
use hoard_index::config::HashPolicy;
use hoard_index::scan::ScanOptions;
use hoard_index::store::{blob, node, source};

use crate::api::common::{authorize, blocking};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const SCAN_KIND: &str = hoard_index::scan::SCAN_JOB;

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

/// Starts a scan and answers with the job it runs under.
///
/// One native job covers the whole run: the SQLite scan the Hub and git faces still read
/// from, then the catalog rescan and the search refresh. The SQLite scanner opens a job row
/// of its own, which stays where it is until that scan goes.
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

    let subject = source.clone();
    let catalog = Arc::clone(state.catalog());
    let job_id = blocking(move || catalog.create_job(SCAN_KIND, subject.as_deref())).await?;
    let background = state.clone();
    let options = ScanOptions { source, hash };
    tokio::spawn(async move {
        if let Err(error) = hand_off_to_scanner(&background, options, job_id).await {
            tracing::error!(%error, "the scan failed");
        }
    });
    Ok(Json(ScanStarted { job_id }))
}

async fn hand_off_to_scanner(
    state: &AppState,
    options: ScanOptions,
    job: JobId,
) -> Result<(), IndexError> {
    match run_scan(state, options, job).await {
        Ok(report) => {
            tracing::info!(
                unchanged = report.unchanged,
                changed = report.changed,
                added = report.added,
                absent = report.absent,
                "native catalog rescanned"
            );
            settle_scan(state, job, None).await
        }
        Err(error) => {
            let message = error.to_string();
            settle_scan(state, job, Some(message)).await?;
            Err(error)
        }
    }
}

async fn run_scan(
    state: &AppState,
    options: ScanOptions,
    job: JobId,
) -> Result<RescanReport, IndexError> {
    let config = state.config().index.clone();
    phase(state, job, "scanning the sources").await?;
    hoard_index::scan::scan(state.store(), &config, options, &|_| {}).await?;
    phase(state, job, "rescanning the native catalog").await?;
    refresh_catalog(state, config).await
}

async fn phase(state: &AppState, job: JobId, phase: &str) -> Result<(), IndexError> {
    let catalog = Arc::clone(state.catalog());
    let progress = JobProgress {
        phase: Some(phase.to_owned()),
        ..JobProgress::default()
    };
    tokio::task::spawn_blocking(move || catalog.update_job(job, &progress)).await?
}

async fn settle_scan(
    state: &AppState,
    job: JobId,
    error: Option<String>,
) -> Result<(), IndexError> {
    let catalog = Arc::clone(state.catalog());
    tokio::task::spawn_blocking(move || match error {
        Some(error) => catalog.fail_job(job, &error),
        None => catalog.finish_job(job),
    })
    .await?
}

/// Walks the sources again into the native catalog and applies what changed to its search
/// index, on a blocking thread because both are file and database work.
async fn refresh_catalog(
    state: &AppState,
    config: hoard_index::config::IndexConfig,
) -> Result<RescanReport, IndexError> {
    let catalog = std::sync::Arc::clone(state.catalog());
    tokio::task::spawn_blocking(move || -> Result<RescanReport, IndexError> {
        let report = catalog.rescan(&config, |_, _| {})?;
        catalog.refresh_search(|_| {})?;
        Ok(report)
    })
    .await?
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
) -> ApiResult<Json<Vec<JobRecord>>> {
    let wanted = filter
        .state
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse::<JobState>)
        .transpose()
        .map_err(ApiError::from)?;
    let catalog = Arc::clone(state.catalog());
    let rows = blocking(move || catalog.list_jobs(wanted)).await?;
    Ok(Json(rows))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<JobRecord>> {
    let id = job_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let row = blocking(move || required_job(&catalog, id)).await?;
    Ok(Json(row))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<JobRecord>> {
    authorize(&state, &headers)?;
    let id = job_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let row = tokio::task::spawn_blocking(move || {
        let existing = required_job(&catalog, id)?;
        if existing.state.is_final() {
            return Err(IndexError::Query(format!(
                "job {id} already finished as {}",
                existing.state
            )));
        }
        catalog.cancel_job(id)?;
        required_job(&catalog, id)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(finished_conflict)?;
    Ok(Json(row))
}

fn required_job(catalog: &Catalog, id: JobId) -> Result<JobRecord, IndexError> {
    catalog
        .job(id)?
        .ok_or_else(|| IndexError::not_found("job", id.to_string()))
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
