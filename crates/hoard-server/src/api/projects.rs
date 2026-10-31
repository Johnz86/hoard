use std::path::Path as FilePath;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use hoard_core::{CommitSha, RepoRowId, clean_repo_path, format_size};
use hoard_index::catalog::{RepositoryDetail, RevisionDetail};
use hoard_index::config::SourceKind;
use hoard_index::sniff::parse_readme;
use hoard_mirror::local::{EntryKind, GitRepo};

use crate::api::common::shell_word;
use crate::api::repos::FrontmatterEntry;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const PREVIEW_LIMIT: u64 = 256 * 1024;
const DOWNLOAD_LIMIT: u64 = 16 * 1024 * 1024;
const README_NAMES: [&str; 5] = [
    "readme.md",
    "readme.markdown",
    "readme",
    "readme.txt",
    "readme.rst",
];

pub(crate) fn is_readme(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    README_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectParams {
    pub revision: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectRef {
    pub name: String,
    pub sha: Option<CommitSha>,
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectView {
    pub id: RepoRowId,
    pub name: String,
    pub reference: String,
    pub source: String,
    pub node: String,
    pub revision: String,
    pub revision_sha: CommitSha,
    pub refs: Vec<ProjectRef>,
    pub clone_command: String,
    pub project_url: String,
    pub files: usize,
    pub bytes: u64,
    pub bytes_human: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectFile {
    pub path: String,
    pub size: u64,
    pub size_human: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectFiles {
    pub rows: Vec<ProjectFile>,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectPreview {
    pub path: String,
    pub text: Option<String>,
    pub kind: &'static str,
    pub binary: bool,
    pub truncated: bool,
    pub size: u64,
    pub size_human: String,
    pub revision: String,
    pub message: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectReadme {
    pub html: String,
    pub frontmatter: Vec<FrontmatterEntry>,
    pub meta: String,
    pub generated: bool,
    pub revision: String,
    pub path: Option<String>,
    pub missing: bool,
    pub message: Option<&'static str>,
}

#[derive(Debug)]
struct OpenedProject {
    record: RepositoryDetail,
    revision: RevisionDetail,
    sha: CommitSha,
    git: GitRepo,
}

async fn on_project<T, F>(
    state: AppState,
    id: String,
    wanted: Option<String>,
    operation: F,
) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(OpenedProject) -> ApiResult<T> + Send + 'static,
{
    let id = id
        .parse::<RepoRowId>()
        .map_err(|_| ApiError::not_found("Git project"))?;
    tokio::task::spawn_blocking(move || {
        let record = state
            .catalog()
            .repository_by_id(id)?
            .filter(|record| record.kind == SourceKind::Git && record.git_dir.is_some())
            .ok_or_else(|| ApiError::not_found(format!("Git project {id}")))?;
        let wanted = wanted
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&record.default_ref);
        let revision = state
            .catalog()
            .resolve_revision(&record, wanted)
            .ok_or_else(|| ApiError::not_found(format!("revision {wanted}")))?;
        let sha = revision
            .sha
            .ok_or_else(|| ApiError::not_found(format!("commit of {wanted}")))?;
        let git_dir = record
            .git_dir
            .as_deref()
            .ok_or_else(|| ApiError::not_found(format!("Git project {id}")))?;
        let git = GitRepo::open(FilePath::new(git_dir))?;
        operation(OpenedProject {
            record,
            revision,
            sha,
            git,
        })
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProjectParams>,
) -> ApiResult<Json<ProjectView>> {
    let url = state.public_url().trim_end_matches('/').to_owned();
    let view = on_project(state, id, params.revision, move |project| {
        let files = project.git.tree(project.sha, "", true)?;
        let bytes = files
            .iter()
            .filter(|file| file.kind == EntryKind::File)
            .fold(0_u64, |total, file| total.saturating_add(file.size));
        let reference = project.record.repo.reference();
        Ok(ProjectView {
            id: project.record.id,
            name: project.record.repo.name().to_owned(),
            reference,
            source: project.record.source,
            node: project.record.node,
            revision: project.revision.name,
            revision_sha: project.sha,
            refs: project
                .record
                .refs
                .into_iter()
                .map(|reference| ProjectRef {
                    name: reference.name,
                    sha: reference.sha,
                    kind: reference.kind.as_str(),
                })
                .collect(),
            clone_command: clone_command(&url, project.record.id, project.record.repo.name()),
            project_url: format!(
                "{}/#/project/{}",
                url.trim_end_matches('/'),
                project.record.id
            ),
            files: files
                .iter()
                .filter(|file| file.kind == EntryKind::File)
                .count(),
            bytes,
            bytes_human: format_size(bytes),
        })
    })
    .await?;
    Ok(Json(view))
}

pub(crate) fn clone_command(url: &str, id: RepoRowId, name: &str) -> String {
    let url = format!("{}/git/projects/{id}.git", url.trim_end_matches('/'));
    format!("git clone -- {} {}", shell_word(&url), shell_word(name))
}

pub async fn files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProjectParams>,
) -> ApiResult<Json<ProjectFiles>> {
    let view = on_project(state, id, params.revision, |project| {
        let rows = project
            .git
            .tree(project.sha, "", true)?
            .into_iter()
            .filter(|file| file.kind == EntryKind::File)
            .map(|file| ProjectFile {
                path: file.path,
                size: file.size,
                size_human: format_size(file.size),
                kind: if file.lfs.is_some() { "lfs" } else { "file" },
            })
            .collect();
        Ok(ProjectFiles {
            rows,
            revision: project.revision.name,
        })
    })
    .await?;
    Ok(Json(view))
}

pub async fn readme(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProjectParams>,
) -> ApiResult<Json<ProjectReadme>> {
    let view = on_project(state, id, params.revision, |project| {
        let files = project.git.tree(project.sha, "", false)?;
        let path = README_NAMES.iter().find_map(|name| {
            files
                .iter()
                .find(|file| file.kind == EntryKind::File && file.path.eq_ignore_ascii_case(name))
                .map(|file| file.path.clone())
        });
        let mut view = ProjectReadme {
            html: String::new(),
            frontmatter: Vec::new(),
            meta: String::new(),
            generated: false,
            revision: project.revision.name.clone(),
            missing: path.is_none(),
            path,
            message: None,
        };
        let Some(path) = &view.path else {
            return Ok(view);
        };
        let preview = preview(&project, path)?;
        view.message = preview.message;
        view.meta = preview.size_human;
        if let Some(text) = preview.text {
            let document = parse_readme(&text);
            view.html = document.html;
            view.meta = format!("{} · {} lines", view.meta, document.lines);
            view.frontmatter = document
                .frontmatter
                .into_iter()
                .map(|(key, value)| FrontmatterEntry { key, value })
                .collect();
        }
        Ok(view)
    })
    .await?;
    Ok(Json(view))
}

pub async fn file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProjectParams>,
) -> ApiResult<Json<ProjectPreview>> {
    let path = checked_path(params.path)?;
    let view = on_project(state, id, params.revision, move |project| {
        preview(&project, &path)
    })
    .await?;
    Ok(Json(view))
}

fn preview(project: &OpenedProject, path: &str) -> ApiResult<ProjectPreview> {
    let blob = project
        .git
        .committed_blob(project.sha, path, PREVIEW_LIMIT)?;
    let text = blob.data.and_then(|data| {
        if data.contains(&0) {
            return None;
        }
        String::from_utf8(data).ok()
    });
    let oversized = blob.size > PREVIEW_LIMIT;
    let binary = !oversized && text.is_none();
    Ok(ProjectPreview {
        path: path.to_owned(),
        text,
        kind: if oversized {
            "too_large"
        } else if binary {
            "binary"
        } else {
            "text"
        },
        binary,
        truncated: oversized,
        size: blob.size,
        size_human: format_size(blob.size),
        revision: project.revision.name.clone(),
        message: if oversized {
            Some("This file exceeds the 256 KiB preview limit. Download it or clone the project.")
        } else if binary {
            Some("This file is binary or is not UTF-8 text. Download it or clone the project.")
        } else {
            None
        },
    })
}

pub async fn raw(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ProjectParams>,
) -> ApiResult<Response> {
    let path = checked_path(params.path)?;
    let name = FilePath::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download")
        .to_owned();
    let data = on_project(state, id, params.revision, move |project| {
        let blob = project
            .git
            .committed_blob(project.sha, &path, DOWNLOAD_LIMIT)?;
        blob.data.ok_or_else(|| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "FileTooLarge",
                "Files over 16 MiB are available by cloning the project.",
            )
        })
    })
    .await?;
    let filename = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC);
    let disposition = HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{filename}"))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let mut response = data.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn checked_path(path: Option<String>) -> ApiResult<String> {
    let path = path.ok_or_else(|| ApiError::bad_request("file path is required"))?;
    if path.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "file path contains control characters",
        ));
    }
    let clean = clean_repo_path(&path)
        .filter(|clean| !clean.is_empty() && clean == &path)
        .ok_or_else(|| ApiError::bad_request("file path must stay inside the Git project"))?;
    Ok(clean)
}
