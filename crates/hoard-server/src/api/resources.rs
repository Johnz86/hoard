use std::cmp::Ordering;

use hoard_core::RepoRowId;
use hoard_index::catalog::QueryPage;
use hoard_index::row::SearchRow;
use hoard_index::search::{SortKey, SortSpec};
use serde::Serialize;

use crate::api::common::{RowView, next_cursor};
use crate::api::project_search::{self, ProjectRow};
use crate::api::search::run_query;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// A table-compatible resource row; project presentation values do not classify artifacts.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ResourceRow {
    Artifact(ArtifactResource),
    Project(ProjectResource),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactResource {
    #[serde(flatten)]
    pub row: RowView,
    pub resource_kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectResource {
    pub project_id: RepoRowId,
    pub artifact_id: Option<hoard_core::ArtifactId>,
    pub resource_kind: &'static str,
    pub role: &'static str,
    pub role_code: &'static str,
    pub name: String,
    pub prefix: String,
    pub base: Option<&'static str>,
    pub format: &'static str,
    pub precision: Option<&'static str>,
    pub size: u64,
    pub size_human: String,
    pub revision: String,
    pub node: String,
    pub state: &'static str,
    pub state_label: &'static str,
    pub path: String,
    pub source: String,
    pub clone_command: String,
    pub files: usize,
}

impl From<ProjectRow> for ProjectResource {
    fn from(row: ProjectRow) -> Self {
        Self {
            project_id: row.id,
            artifact_id: None,
            resource_kind: "git",
            role: "git",
            role_code: "GIT",
            prefix: row
                .reference
                .strip_suffix(&row.name)
                .unwrap_or_default()
                .to_owned(),
            name: row.name,
            base: None,
            format: "git",
            precision: None,
            size: row.bytes,
            size_human: row.bytes_human,
            revision: row.revision,
            node: row.node,
            state: if row.available { "local" } else { "stale" },
            state_label: if row.available { "LOCAL" } else { "STALE" },
            path: row.source.clone(),
            source: row.source,
            clone_command: row.clone_command,
            files: row.files,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResourceRequest {
    pub raw: String,
    pub sort: SortSpec,
    pub offset: u32,
    pub limit: u32,
    pub cursor_key: String,
}

#[derive(Debug)]
pub(crate) struct ResourcePage {
    pub rows: Vec<ResourceRow>,
    pub total: u64,
    pub next_cursor: Option<String>,
}

pub(crate) async fn find(
    state: &AppState,
    request: ResourceRequest,
    legacy: &QueryPage,
    legacy_offset: u32,
) -> ApiResult<ResourcePage> {
    let window = request
        .offset
        .checked_add(request.limit)
        .ok_or_else(|| ApiError::bad_request("resource cursor exceeds the supported range"))?;
    let projects =
        project_search::sorted_prefix(state, request.raw.clone(), request.sort, window).await?;
    let total = legacy.total.saturating_add(projects.total);
    if u64::from(request.offset) >= total {
        return Ok(ResourcePage {
            rows: Vec::new(),
            total,
            next_cursor: None,
        });
    }
    let artifacts = if projects.total == 0 && legacy_offset == request.offset {
        legacy.rows.clone()
    } else {
        artifact_prefix(state, &request, legacy, legacy_offset, window).await?
    };
    let offset = if projects.total == 0 && legacy_offset == request.offset {
        0
    } else {
        request.offset as usize
    };
    let rows = merge(
        artifacts,
        projects.rows,
        request.sort,
        offset,
        request.limit as usize,
    );
    Ok(ResourcePage {
        next_cursor: next_cursor(
            request.offset,
            request.limit,
            rows.len(),
            total,
            &request.cursor_key,
        ),
        rows,
        total,
    })
}

async fn artifact_prefix(
    state: &AppState,
    request: &ResourceRequest,
    legacy: &QueryPage,
    legacy_offset: u32,
    window: u32,
) -> ApiResult<Vec<SearchRow>> {
    let wanted = u64::from(window).min(legacy.total) as usize;
    let mut rows = Vec::with_capacity(wanted);
    let mut offset = 0_u32;
    if legacy_offset == 0 {
        rows.extend(legacy.rows.iter().take(wanted).cloned());
        offset = u32::try_from(crate::api::search::walked(legacy)).unwrap_or(u32::MAX);
    }
    while rows.len() < wanted && u64::from(offset) < legacy.total {
        let limit = (wanted - rows.len()).min(1000) as u32;
        let page = run_query(state, request.raw.clone(), request.sort, limit, offset).await?;
        let walked = crate::api::search::walked(&page) as u32;
        if walked == 0 {
            break;
        }
        offset = offset.saturating_add(walked);
        rows.extend(page.rows);
    }
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SortValue {
    Text(String),
    Number(u64),
}

pub(crate) fn project_key(row: &ProjectRow, key: SortKey) -> SortValue {
    match key {
        SortKey::Role | SortKey::Format => SortValue::Text("git".to_owned()),
        SortKey::Name => SortValue::Text(row.name.to_ascii_lowercase()),
        SortKey::Base | SortKey::Precision => SortValue::Text(String::new()),
        SortKey::Revision => SortValue::Text(row.revision.clone()),
        SortKey::Node => SortValue::Text(row.node.clone()),
        SortKey::State => SortValue::Text(if row.available { "local" } else { "stale" }.to_owned()),
        SortKey::Size => SortValue::Number(row.bytes),
        SortKey::Used | SortKey::Ident => SortValue::Number(0),
    }
}

fn artifact_key(row: &SearchRow, key: SortKey) -> SortValue {
    match key {
        SortKey::Role => SortValue::Text(row.role.as_str().to_owned()),
        SortKey::Name => SortValue::Text(row.name.to_ascii_lowercase()),
        SortKey::Base => SortValue::Text(row.base.as_str().to_owned()),
        SortKey::Format => SortValue::Text(row.format.as_str().to_owned()),
        SortKey::Precision => SortValue::Text(
            row.precision
                .as_ref()
                .map_or("", |precision| precision.as_str())
                .to_owned(),
        ),
        SortKey::Revision => SortValue::Text(row.revision.clone()),
        SortKey::Node => SortValue::Text(row.node.clone()),
        SortKey::State => SortValue::Text(row.state.as_str().to_owned()),
        SortKey::Size => SortValue::Number(row.size),
        SortKey::Used => SortValue::Number(
            row.last_used
                .and_then(|used| u64::try_from(used.as_second()).ok())
                .unwrap_or_default(),
        ),
        SortKey::Ident => SortValue::Number(0),
    }
}

fn merge(
    artifacts: Vec<SearchRow>,
    projects: Vec<ProjectRow>,
    sort: SortSpec,
    offset: usize,
    limit: usize,
) -> Vec<ResourceRow> {
    let mut artifacts = artifacts
        .into_iter()
        .map(|row| (artifact_key(&row, sort.key), row))
        .peekable();
    let mut projects = projects
        .into_iter()
        .map(|row| (project_key(&row, sort.key), row))
        .peekable();
    let mut rows = Vec::with_capacity(limit);
    let take_artifact = |left: &SortValue, right: &SortValue| {
        let order = left.cmp(right);
        let order = if sort.descending {
            order.reverse()
        } else {
            order
        };
        order != Ordering::Greater
    };
    for position in 0..offset.saturating_add(limit) {
        let artifact = match (artifacts.peek(), projects.peek()) {
            (Some((left, _)), Some((right, _))) => take_artifact(left, right),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if artifact {
            if let Some((_, row)) = artifacts.next()
                && position >= offset
            {
                rows.push(ResourceRow::Artifact(ArtifactResource {
                    row: RowView::from(&row),
                    resource_kind: "artifact",
                }));
            }
        } else if let Some((_, row)) = projects.next()
            && position >= offset
        {
            rows.push(ResourceRow::Project(ProjectResource::from(row)));
        }
    }
    rows
}
