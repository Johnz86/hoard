use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use hoard_core::{RepoRowId, format_size};
use hoard_index::catalog::Dedupe;
use hoard_index::projects::{ProjectDocument, ProjectIndex};
use hoard_mirror::local::GitRepo;
use serde::{Deserialize, Serialize};

use crate::api::common::{clamp_limit, decode_cursor, next_cursor};
use crate::api::projects::{clone_command, is_readme};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const README_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectRow {
    pub id: RepoRowId,
    pub name: String,
    pub reference: String,
    pub source: String,
    pub node: String,
    pub revision: String,
    pub files: usize,
    pub bytes_human: String,
    pub clone_command: String,
    pub available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProjectList {
    pub rows: Vec<ProjectRow>,
    pub total: u64,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProjectSearchParams {
    pub q: Option<String>,
    pub source: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct CachedProjects {
    generation: u64,
    index: ProjectIndex,
    rows: Vec<ProjectRow>,
}

impl CachedProjects {
    fn build(state: &AppState, generation: u64) -> ApiResult<Self> {
        let records = state.catalog().git_repositories()?;
        let mut rows = Vec::with_capacity(records.len());
        let documents = records.into_iter().map(|record| {
            let reference = record.repo.reference();
            let mut text = format!("{} {} {}", record.repo.name(), reference, record.source);
            let git = record
                .git_dir
                .as_deref()
                .and_then(|path| GitRepo::open(Path::new(path)).ok());
            let revision = state
                .catalog()
                .resolve_revision(&record, &record.default_ref);
            let mut files = 0;
            let mut bytes = 0_u64;
            if let Some(revision) = revision {
                let entries = state.catalog().revision_entries(
                    record.id,
                    revision.ordinal,
                    Dedupe::Ignore,
                )?;
                files = entries.len();
                bytes = entries
                    .iter()
                    .fold(0_u64, |bytes, entry| bytes.saturating_add(entry.size));
                for entry in &entries {
                    text.push(' ');
                    text.push_str(&entry.rel_path);
                }
                let mut remaining = README_LIMIT;
                for root in [true, false] {
                    for entry in entries.iter().filter(|entry| {
                        is_readme(&entry.rel_path) && entry.rel_path.contains('/') != root
                    }) {
                        if remaining == 0 {
                            break;
                        }
                        if let (Some(git), Some(sha)) = (&git, revision.sha)
                            && let Ok(blob) = git.committed_blob(sha, &entry.rel_path, remaining)
                            && let Some(data) = blob.data
                            && let Ok(readme) = std::str::from_utf8(&data)
                        {
                            remaining = remaining.saturating_sub(data.len() as u64);
                            text.push(' ');
                            text.push_str(readme);
                        }
                    }
                }
            }
            let document = ProjectDocument {
                text,
                source: record.source.clone(),
                node: record.node.clone(),
                reference: reference.clone(),
                org: record.repo.org().unwrap_or_default().to_owned(),
                revision: record.default_ref.clone(),
            };
            rows.push(ProjectRow {
                id: record.id,
                name: record.repo.name().to_owned(),
                reference,
                source: record.source,
                node: record.node,
                revision: record.default_ref,
                files,
                bytes_human: format_size(bytes),
                clone_command: clone_command(state.public_url(), record.id, record.repo.name()),
                available: git.is_some(),
            });
            Ok(document)
        });
        let index = ProjectIndex::build(documents)?;
        Ok(Self {
            generation,
            index,
            rows,
        })
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<ProjectSearchParams>,
) -> ApiResult<Json<ProjectList>> {
    let raw = params.q.unwrap_or_default();
    let key = format!("{raw}\0{}", params.source.as_deref().unwrap_or_default());
    let offset = decode_cursor(params.cursor.as_deref(), &key)?;
    Ok(Json(
        find(
            &state,
            raw,
            params.source,
            offset,
            clamp_limit(params.limit),
        )
        .await?,
    ))
}

pub async fn find(
    state: &AppState,
    raw: String,
    source: Option<String>,
    offset: u32,
    limit: u32,
) -> ApiResult<ProjectList> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let cache = {
            let mut cache = state.project_index().lock();
            let generation = state.coordinator().finished();
            if cache
                .as_ref()
                .is_none_or(|cache| cache.generation != generation)
            {
                *cache = Some(Arc::new(CachedProjects::build(&state, generation)?));
            }
            cache
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| ApiError::internal("project index is missing"))?
        };
        let found = cache
            .index
            .search(&raw, source.as_deref(), offset as usize, limit as usize)?;
        let rows: Vec<ProjectRow> = found
            .slots
            .iter()
            .map(|slot| cache.rows[*slot].clone())
            .collect();
        Ok(ProjectList {
            next_cursor: next_cursor(
                offset,
                limit,
                rows.len(),
                found.total,
                &format!("{raw}\0{}", source.as_deref().unwrap_or_default()),
            ),
            rows,
            total: found.total,
        })
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}
