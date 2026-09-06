use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{JobId, SourceId, format_size};
use hoard_index::IndexError;
use hoard_index::catalog::{
    Catalog, HashReport, JobProgress, JobRecord, JobState, MIRROR_SOURCE, PEER_SOURCE, PULL_SOURCE,
    RescanReport, SourceRecord,
};
use hoard_index::config::{HashPolicy, IndexConfig};

use crate::api::common::{authorize, blocking};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The `job.kind` a scan records.
const SCAN_KIND: &str = "scan";

/// One source the catalog holds and what the last scan of it counted.
///
/// Sources carry no identity of their own in the catalog yet, so `id` is the source's
/// one-based position in this listing, which is also how a scan request naming an id is
/// read back. `blobs` reads zero because the catalog counts paths, not blobs. Sources this
/// node does not scan are listed too, saying who does write them.
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
    /// Whether `POST /api/v1/scan` will accept this source: it is named in the
    /// configuration.
    pub can_scan: bool,
    /// Who writes this source when it is not the scanner: "downloads", "pulls", "peer", or
    /// "nobody" for a stored source the configuration no longer names. Absent when
    /// `can_scan` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<&'static str>,
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
    Ok(Json(source_views(&state).await?))
}

async fn source_views(state: &AppState) -> ApiResult<Vec<SourceView>> {
    let catalog = Arc::clone(state.catalog());
    let records = blocking(move || catalog.sources()).await?;
    let node = state.node_name();
    let index = &state.config().index;
    Ok(records
        .into_iter()
        .enumerate()
        .map(|(position, record)| source_view(position, record, node, index))
        .collect())
}

fn source_view(
    position: usize,
    record: SourceRecord,
    node: &str,
    index: &IndexConfig,
) -> SourceView {
    let can_scan = index.source(&record.name).is_some();
    let managed_by = match can_scan {
        true => None,
        false => Some(manager_of(&record.name)),
    };
    SourceView {
        id: SourceId::new(position as i64 + 1),
        name: record.name,
        path: record.path.to_string_lossy().into_owned(),
        kind: record.kind.as_str().to_owned(),
        node: node.to_owned(),
        files: record.file_count,
        blobs: 0,
        bytes: record.byte_count,
        bytes_human: format_size(record.byte_count),
        last_scan: record.last_scan,
        pending_hashes: record.pending_hashes,
        can_scan,
        managed_by,
    }
}

fn manager_of(name: &str) -> &'static str {
    if name == MIRROR_SOURCE {
        return "downloads";
    }
    if name == PULL_SOURCE {
        return "pulls";
    }
    if name.starts_with(PEER_SOURCE) {
        return "peer";
    }
    "nobody"
}

/// Starts a scan and answers with the job it runs under.
///
/// One job covers the whole run: the catalog rescan, the hashing it owes and the search
/// refresh that publishes what changed.
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
    let config = narrowed(&state.config().index, source.as_deref());
    tokio::spawn(async move {
        if let Err(error) = hand_off_to_scanner(&background, config, hash, job_id).await {
            tracing::error!(%error, "the scan failed");
        }
    });
    Ok(Json(ScanStarted { job_id }))
}

/// What one run covers: every configured root, or the one the request named.
fn narrowed(config: &IndexConfig, source: Option<&str>) -> IndexConfig {
    match source.and_then(|name| config.source(name)) {
        Some(named) => config.over(vec![named.clone()]),
        None => config.clone(),
    }
}

async fn hand_off_to_scanner(
    state: &AppState,
    config: IndexConfig,
    hash: HashPolicy,
    job: JobId,
) -> Result<(), IndexError> {
    match run_scan(state, config, hash, job).await {
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
    config: IndexConfig,
    hash: HashPolicy,
    job: JobId,
) -> Result<RescanReport, IndexError> {
    phase(state, job, "rescanning the sources").await?;
    let report = rescan_catalog(state, config.clone(), hash).await?;
    match hash {
        HashPolicy::Now | HashPolicy::Background => {
            phase(state, job, "hashing").await?;
            hash_catalog(state, config).await?;
        }
        HashPolicy::Never => {}
    }
    phase(state, job, "refreshing search").await?;
    refresh_catalog(state).await?;
    Ok(report)
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

/// Walks the sources again into the native catalog, on a blocking thread because it is file
/// and database work.
async fn rescan_catalog(
    state: &AppState,
    config: IndexConfig,
    policy: HashPolicy,
) -> Result<RescanReport, IndexError> {
    let catalog = Arc::clone(state.catalog());
    tokio::task::spawn_blocking(move || catalog.rescan(&config, policy, |_, _| {})).await?
}

/// Hashes what the walk left unhashed, which is what gives copies of one file one artifact.
async fn hash_catalog(state: &AppState, config: IndexConfig) -> Result<HashReport, IndexError> {
    let catalog = Arc::clone(state.catalog());
    tokio::task::spawn_blocking(move || catalog.hash_pending(&config, |_| {})).await?
}

/// Applies to the search index what the walk and the hashing changed.
async fn refresh_catalog(state: &AppState) -> Result<u64, IndexError> {
    let catalog = Arc::clone(state.catalog());
    tokio::task::spawn_blocking(move || catalog.refresh_search(|_| {})).await?
}

/// Reads what a scan request named: a configured source name, or the name, id or path of a
/// source the index already knows, resolved back to the name the scanner accepts.
async fn resolve_source(state: &AppState, wanted: &str) -> ApiResult<String> {
    if state.config().index.source(wanted).is_some() {
        return Ok(wanted.to_owned());
    }
    let stored = source_views(state)
        .await?
        .into_iter()
        .find(|view| view.name == wanted || view.id.to_string() == wanted || view.path == wanted);
    stored
        .map(|view| view.name)
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
