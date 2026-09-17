use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as RoutePath, Query, State};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactFormat, ArtifactId, CommitSha, FileState, RepoId, RepoRowId, RepoType, Role,
    State as ArtifactState, format_size,
};
use hoard_index::IndexError;
use hoard_index::catalog::{
    Catalog, Dedupe, EntryDetail, RepositoryDetail, RepositoryFilter, RevisionDetail,
};
use hoard_index::catalog::{PullRow, UsageDay};
use hoard_index::config::SourceKind;
use hoard_index::discover::{DEFAULT_REF, RefKind};
use hoard_index::row::SearchRow;
use hoard_index::shard::parse_shard_name;
use hoard_index::sniff::parse_readme;

use crate::api::artifacts::artifact_id;
use crate::api::common::{
    DEFAULT_LIMIT, EntryView, Fact, MIDDOT, PullArtifact, PullCommands, PullSubject, RowView,
    blocking, clamp_limit, companion_of, decode_row_cursor, encode_row_cursor, entry_views, facts,
    is_category_bin, pull_commands, relative_time, repo_id, shown_path, state_label,
};
use crate::api::compat::{CompatView, compat_for};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const USAGE_DAYS: u32 = 7;
const RECENT_PULLS: u32 = 20;
const REPO_ARTIFACTS: u32 = 64;
const README_PATH: &str = "README.md";
const PULLS_LABEL: &str = "pulls on lan";
const README_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParams {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub repo_type: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// What a repository request names beside the repository itself: the revision to open, and
/// the artifact the caller opened the page on.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RevisionParams {
    pub revision: Option<String>,
    pub artifact: Option<String>,
}

impl RevisionParams {
    fn named(&self) -> Option<&str> {
        self.revision
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn wanted(&self) -> String {
        self.named().unwrap_or(DEFAULT_REF).to_owned()
    }

    fn opened(&self) -> Option<ArtifactId> {
        self.artifact
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|raw| artifact_id(raw).ok())
    }
}

/// What a request resolved to: the artifact it opened the page on, when the id names one
/// this repository holds, and the revision the page reads.
struct Opened {
    row: Option<SearchRow>,
    revision: String,
}

/// An artifact of another repository, one whose revision this record does not hold, an
/// unknown id and an unparsable one are all ignored rather than refused, because the
/// repository page has to open either way. A row is filed against the node holding it, so a
/// peer's copy carries this repository's name and a revision only that peer has.
fn opened_for(
    catalog: &Catalog,
    record: &RepositoryDetail,
    params: &RevisionParams,
) -> Result<Opened, IndexError> {
    let row = params
        .opened()
        .map(|id| catalog.artifact_row(id))
        .transpose()?
        .flatten()
        .filter(|row| row.repo == record.repo.qualified())
        .filter(|row| catalog.resolve_revision(record, &row.revision).is_some());
    Ok(Opened {
        revision: params
            .named()
            .map(str::to_owned)
            .or_else(|| row.as_ref().map(|row| row.revision.clone()))
            .unwrap_or_else(|| params.wanted()),
        row,
    })
}

/// One line of the repositories listing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoListRow {
    pub id: RepoRowId,
    pub repo: String,
    #[serde(rename = "type")]
    pub repo_type: RepoType,
    pub org: Option<String>,
    pub name: String,
    pub node: String,
    pub kind: String,
    pub default_ref: String,
    pub files: u64,
    pub size: u64,
    pub size_human: String,
    pub last_modified: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoListView {
    pub rows: Vec<RepoListRow>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

/// A named pointer into the repository's history, as the REFS tab lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefView {
    pub name: String,
    pub sha: Option<CommitSha>,
    pub short: String,
    pub when: String,
    pub kind: RefKind,
    pub current: bool,
}

/// The repository detail header, its refs and everything the right rail prints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoView {
    pub repo: String,
    #[serde(rename = "type")]
    pub repo_type: RepoType,
    pub org: Option<String>,
    pub name: String,
    pub node: String,
    /// The URL a clone's `origin` names, so the PULL box can say where this copy came from.
    /// A repository the mirror, a peer or a bare folder filed has none.
    pub remote: Option<String>,
    pub artifact_id: Option<ArtifactId>,
    pub role: Role,
    pub role_code: String,
    pub role_full: &'static str,
    pub state: ArtifactState,
    pub state_full: String,
    pub pull_progress: Option<f32>,
    pub meta: String,
    pub revision: String,
    /// What the REVISION chip prints: the full spelling of the ref pointing at the shown
    /// revision, and `613bd8d (detached)` for a revision no ref names.
    pub revision_label: String,
    pub revision_sha: Option<CommitSha>,
    pub revision_summary: String,
    pub refs: Vec<RefView>,
    pub facts: Vec<Fact>,
    pub paths: Vec<EntryView>,
    pub pull: PullCommands,
    pub rows: Vec<RowView>,
}

/// One file of a revision as the FILES tab lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileRow {
    pub path: String,
    pub kind: &'static str,
    pub size: u64,
    pub size_human: String,
    pub state: FileState,
    pub state_label: &'static str,
    pub artifact_id: Option<ArtifactId>,
    /// Whether this file is one the artifact the page was opened on is made of.
    pub opened: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilesView {
    pub revision: String,
    pub rows: Vec<FileRow>,
}

/// The CARD tab: frontmatter split from the rendered body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadmeView {
    pub frontmatter: Vec<FrontmatterEntry>,
    pub html: String,
    pub generated: bool,
    pub meta: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrontmatterEntry {
    pub key: String,
    pub value: String,
}

/// The USAGE tab: seven days of bars, the recent pulls and how many clients pulled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageView {
    pub days: Vec<UsageDay>,
    pub pulls: Vec<PullView>,
    pub clients: u64,
    pub total: u64,
}

/// One line of RECENT PULLS. `when` is the relative spelling the design prints; `at` is the
/// instant behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PullView {
    pub when: String,
    pub at: Timestamp,
    pub who: Option<String>,
    pub what: String,
    pub bytes: u64,
    pub rate_bps: Option<u64>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<RepoListView>> {
    let limit = clamp_limit(params.limit);
    let repo_type = params
        .repo_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse::<RepoType>)
        .transpose()
        .map_err(ApiError::from)?;
    let key = format!(
        "{}|{}",
        params.q.clone().unwrap_or_default(),
        params.repo_type.clone().unwrap_or_default()
    );
    let after = decode_row_cursor(params.cursor.as_deref(), &key)?.map(RepoRowId::new);
    let text = params.q.clone();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let filter = RepositoryFilter {
            text,
            repo_type,
            present_only: true,
        };
        let page = catalog.repositories(&filter, after, limit.saturating_add(1))?;
        let mut listed = page.rows;
        let more = listed.len() > limit as usize;
        listed.truncate(limit as usize);
        let next_cursor = match more {
            true => listed
                .last()
                .map(|record| encode_row_cursor(record.id.get(), &key)),
            false => None,
        };
        let rows = listed.iter().map(list_row).collect();
        Ok(RepoListView {
            next_cursor,
            rows,
            total: page.total,
        })
    })
    .await?;
    Ok(Json(view))
}

fn list_row(record: &RepositoryDetail) -> RepoListRow {
    let size = record.present_bytes;
    RepoListRow {
        id: record.id,
        repo: record.repo.qualified(),
        repo_type: record.repo.repo_type(),
        org: record.repo.org().map(str::to_owned),
        name: record.repo.name().to_owned(),
        node: record.node.clone(),
        kind: record.kind.as_str().to_owned(),
        default_ref: record.default_ref.clone(),
        files: u64::from(record.present_files),
        size,
        size_human: format_size(size),
        last_modified: None,
    }
}

pub async fn detail(
    State(state): State<AppState>,
    RoutePath((repo_type, org, name)): RoutePath<(String, String, String)>,
    Query(params): Query<RevisionParams>,
) -> ApiResult<Json<RepoView>> {
    let repo = repo_id(&repo_type, &org, &name)?;
    let host = state.node_name().to_owned();
    let public = public_host(state.public_url());
    let now = Timestamp::now();
    let url = state.public_url().to_owned();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        describe(
            &catalog,
            &Header {
                repo,
                params,
                host,
                public,
            },
            &url,
            now,
        )
    })
    .await?;
    Ok(Json(view))
}

/// What the header needs that the catalog does not hold: which repository the route named,
/// what the request asked to see, this node's name and the host a served badge prints.
struct Header {
    repo: RepoId,
    params: RevisionParams,
    host: String,
    public: String,
}

fn describe(
    catalog: &Catalog,
    header: &Header,
    url: &str,
    now: Timestamp,
) -> Result<RepoView, IndexError> {
    let record = require_repository(catalog, &header.repo)?;
    let opened = opened_for(catalog, &record, &header.params)?;
    let revision = require_revision(catalog, &record, &opened.revision)?;
    let rows = catalog.repository_rows(record.id, REPO_ARTIFACTS)?;
    let entries = catalog.revision_entries(record.id, revision.ordinal, Dedupe::Ignore)?;
    let primary = opened
        .row
        .as_ref()
        .or_else(|| pick_primary(rows.iter().filter(|row| holds(&entries, row))));
    let size = match opened.row.as_ref() {
        Some(row) => row.size,
        None => entries.iter().map(|entry| entry.size).sum(),
    };
    let refs = ref_views(&record, &revision, now);
    let paths = match primary {
        Some(row) => entry_views(&catalog.entries_of(row.artifact_id)?, &header.host),
        None => entry_views(&entries, &header.host),
    };
    let path = primary.and_then(shown_path);
    Ok(RepoView {
        repo: record.repo.qualified(),
        repo_type: record.repo.repo_type(),
        org: record.repo.org().map(str::to_owned),
        name: record.repo.name().to_owned(),
        node: record.node.clone(),
        remote: record.remote.clone(),
        artifact_id: primary.map(|row| row.artifact_id),
        role: primary.map_or(Role::Unk, |row| row.role),
        role_code: primary.map_or(Role::Unk, |row| row.role).code().to_owned(),
        role_full: primary.map_or(Role::Unk, |row| row.role).label(),
        state: primary.map_or(ArtifactState::Remote, |row| row.state),
        state_full: state_label(
            primary.map_or(ArtifactState::Remote, |row| row.state),
            primary.and_then(|row| row.pull_progress),
            Some(&header.public),
        ),
        pull_progress: primary.and_then(|row| row.pull_progress),
        meta: meta_line(primary, size, now),
        revision: revision_name(&refs, &revision),
        revision_label: revision_label(&refs, &revision),
        revision_sha: revision.sha,
        revision_summary: revision_summary(&revision, entries.len(), &refs),
        facts: repo_facts(catalog, primary, now)?,
        refs,
        paths,
        pull: pull_commands(
            url,
            PullSubject {
                repo: &header.repo,
                kind: record.kind,
                bin: is_category_bin(&record),
                artifact: opened.row.as_ref().map(|row| PullArtifact {
                    companion: companion_of(row, &rows).map(|row| row.path.as_str()),
                    ..PullArtifact::from(row)
                }),
            },
            path,
        ),
        rows: rows.iter().map(RowView::from).collect(),
    })
}

pub async fn files(
    State(state): State<AppState>,
    RoutePath((repo_type, org, name)): RoutePath<(String, String, String)>,
    Query(params): Query<RevisionParams>,
) -> ApiResult<Json<FilesView>> {
    let repo = repo_id(&repo_type, &org, &name)?;
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let record = require_repository(&catalog, &repo)?;
        let opened = opened_for(&catalog, &record, &params)?;
        let revision = require_revision(&catalog, &record, &opened.revision)?;
        let entries = catalog.revision_entries(record.id, revision.ordinal, Dedupe::Resolve)?;
        let single = entries
            .iter()
            .filter(|entry| entry.artifact_id.is_some())
            .count()
            == 1;
        let shown = opened.row.as_ref().map(|row| row.artifact_id);
        let mut rows: Vec<FileRow> = entries
            .iter()
            .map(|item| file_row(item, single, shown))
            .collect();
        rows.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(FilesView {
            revision: opened.revision,
            rows,
        })
    })
    .await?;
    Ok(Json(view))
}

fn file_row(item: &EntryDetail, single: bool, opened: Option<ArtifactId>) -> FileRow {
    let state = file_state(item.present, item.lfs_pointer, item.deduped);
    FileRow {
        kind: file_kind(&item.rel_path, item.artifact_id.is_some(), single),
        size: item.size,
        size_human: format_size(item.size),
        state,
        state_label: state.label(),
        opened: item.artifact_id.is_some() && item.artifact_id == opened,
        artifact_id: item.artifact_id,
        path: item.rel_path.clone(),
    }
}

pub async fn readme(
    State(state): State<AppState>,
    RoutePath((repo_type, org, name)): RoutePath<(String, String, String)>,
    Query(params): Query<RevisionParams>,
) -> ApiResult<Json<ReadmeView>> {
    let repo = repo_id(&repo_type, &org, &name)?;
    let probe = repo.clone();
    let catalog = Arc::clone(state.catalog());
    let card = blocking(move || {
        let record = require_repository(&catalog, &probe)?;
        let opened = opened_for(&catalog, &record, &params)?;
        let resolved = require_revision(&catalog, &record, &opened.revision)?;
        let entries = catalog.revision_entries(record.id, resolved.ordinal, Dedupe::Ignore)?;
        let path = entries
            .iter()
            .find(|entry| entry.rel_path.eq_ignore_ascii_case(README_PATH))
            .and_then(|entry| entry.abs_path.clone())
            .map(PathBuf::from);
        let rows = catalog.repository_rows(record.id, REPO_ARTIFACTS)?;
        Ok(Card {
            path,
            primary: opened.row.or_else(|| pick_primary(&rows).cloned()),
            kind: record.kind,
            bin: is_category_bin(&record),
            revision: opened.revision,
        })
    })
    .await?;

    let source = match &card.path {
        Some(path) => read_card(path).await,
        None => None,
    };
    let (text, generated) = match source {
        Some(text) => (text, false),
        None => (
            generated_card(
                state.public_url(),
                &repo,
                card.kind,
                card.bin,
                card.primary.as_ref(),
            ),
            true,
        ),
    };
    let document = tokio::task::spawn_blocking(move || parse_readme(&text))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok(Json(ReadmeView {
        frontmatter: document
            .frontmatter
            .into_iter()
            .map(|(key, value)| FrontmatterEntry { key, value })
            .collect(),
        html: document.html,
        generated,
        meta: format!(
            "{} {MIDDOT} {} lines",
            format_size(document.bytes),
            document.lines
        ),
        revision: card.revision,
    }))
}

/// What the CARD tab reads on its blocking thread: the card on disk when there is one, the
/// artifact a generated card describes, and what a pull line for this repository looks like.
struct Card {
    path: Option<PathBuf>,
    primary: Option<SearchRow>,
    kind: SourceKind,
    bin: bool,
    revision: String,
}

pub async fn compat(
    State(state): State<AppState>,
    RoutePath((repo_type, org, name)): RoutePath<(String, String, String)>,
    Query(params): Query<RevisionParams>,
) -> ApiResult<Json<CompatView>> {
    let repo = repo_id(&repo_type, &org, &name)?;
    let now = Timestamp::now();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let record = require_repository(&catalog, &repo)?;
        let opened = opened_for(&catalog, &record, &params)?;
        let resolved = require_revision(&catalog, &record, &opened.revision)?;
        let rows = rows_in_revision(&catalog, &record, resolved.ordinal)?;
        let row = opened
            .row
            .as_ref()
            .or_else(|| pick_primary(&rows))
            .ok_or_else(|| IndexError::not_found("artifact", record.repo.qualified()))?;
        compat_for(&catalog, row, DEFAULT_LIMIT, now)
    })
    .await?;
    Ok(Json(view))
}

pub async fn repo_usage(
    State(state): State<AppState>,
    RoutePath((repo_type, org, name)): RoutePath<(String, String, String)>,
) -> ApiResult<Json<UsageView>> {
    let repo = repo_id(&repo_type, &org, &name)?;
    let now = Timestamp::now();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let record = require_repository(&catalog, &repo)?;
        let report = catalog.usage_report(record.id, USAGE_DAYS, RECENT_PULLS)?;
        Ok(UsageView {
            total: report.days.iter().map(|day| day.pulls).sum(),
            days: report.days,
            pulls: report
                .pulls
                .into_iter()
                .map(|pull| pull_view(pull, now))
                .collect(),
            clients: report.clients,
        })
    })
    .await?;
    Ok(Json(view))
}

fn pull_view(pull: PullRow, now: Timestamp) -> PullView {
    PullView {
        when: relative_time(now, pull.when),
        at: pull.when,
        who: pull.who,
        what: pull.what,
        bytes: pull.bytes,
        rate_bps: pull.rate_bps,
    }
}

/// The artifact the header describes when a request named none: the largest weights file,
/// else the largest artifact, of whatever rows the caller offers.
fn pick_primary<'a>(rows: impl IntoIterator<Item = &'a SearchRow>) -> Option<&'a SearchRow> {
    rows.into_iter()
        .max_by_key(|row| (row.format.holds_tensors(), row.size))
}

/// Whether a revision's files include the artifact a row stands for. A repository's
/// artifacts are shared across its revisions, so this is what decides which of them answer
/// for the revision a request named.
fn holds(entries: &[EntryDetail], row: &SearchRow) -> bool {
    entries
        .iter()
        .any(|entry| entry.artifact_id == Some(row.artifact_id))
}

fn require_repository(catalog: &Catalog, repo: &RepoId) -> Result<RepositoryDetail, IndexError> {
    catalog
        .repository(repo)?
        .ok_or_else(|| IndexError::not_found("repo", repo.qualified()))
}

fn require_revision(
    catalog: &Catalog,
    record: &RepositoryDetail,
    wanted: &str,
) -> Result<RevisionDetail, IndexError> {
    catalog
        .resolve_revision(record, wanted)
        .ok_or_else(|| IndexError::not_found("revision", format!("{}@{wanted}", record.id)))
}

/// The REFS tab. `when` is how long ago the ref's revision was committed, and the
/// placeholder the design prints for an unknown time when the revision carries no date.
fn ref_views(record: &RepositoryDetail, current: &RevisionDetail, now: Timestamp) -> Vec<RefView> {
    record
        .refs
        .iter()
        .map(|reference| {
            let sha = reference.sha.or(current.sha);
            let committed = record
                .revisions
                .iter()
                .find(|revision| revision.ordinal == reference.revision)
                .and_then(|revision| revision.committed);
            RefView {
                when: committed
                    .map_or_else(|| "-".to_owned(), |committed| relative_time(now, committed)),
                short: short_sha(sha),
                current: reference.revision == current.ordinal,
                name: reference.name.clone(),
                kind: reference.kind,
                sha,
            }
        })
        .collect()
}

fn short_sha(sha: Option<CommitSha>) -> String {
    sha.map_or_else(|| "-".to_owned(), |sha| sha.short())
}

fn revision_name(refs: &[RefView], revision: &RevisionDetail) -> String {
    refs.iter()
        .find(|reference| reference.current)
        .map(|reference| reference.name.clone())
        .unwrap_or_else(|| short_sha(revision.sha))
}

/// The REVISION chip: a ref pointing at the shown revision, spelled in full, and otherwise
/// the revision's own short sha marked as what it is.
fn revision_label(refs: &[RefView], revision: &RevisionDetail) -> String {
    let Some(reference) = refs.iter().find(|reference| reference.current) else {
        return format!("{} (detached)", short_sha(revision.sha));
    };
    if reference.name.starts_with("refs/") {
        return reference.name.clone();
    }
    match reference.kind {
        RefKind::Branch => format!("refs/heads/{}", reference.name),
        RefKind::Tag => format!("refs/tags/{}", reference.name),
    }
}

/// `462c3cb \u{b7} 14 files \u{b7} 3 branches \u{b7} 2 tags`, the line under the revision chip.
fn revision_summary(revision: &RevisionDetail, files: usize, refs: &[RefView]) -> String {
    let branches = refs
        .iter()
        .filter(|reference| reference.kind == RefKind::Branch)
        .count();
    let tags = refs.len() - branches;
    format!(
        "{} {MIDDOT} {files} files {MIDDOT} {branches} branches {MIDDOT} {tags} tags",
        short_sha(revision.sha)
    )
}

/// `path \u{b7} size \u{b7} last used 2h ago`, the header's third line. `size` is what the
/// header describes: the opened artifact's own bytes, or the revision's when it describes
/// the revision.
fn meta_line(primary: Option<&SearchRow>, size: u64, now: Timestamp) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(path) = primary.and_then(shown_path) {
        parts.push(path);
    }
    parts.push(format_size(size));
    if let Some(used) = primary.and_then(|row| row.last_used) {
        parts.push(format!("last used {}", relative_time(now, used)));
    }
    parts.join(&format!(" {MIDDOT} "))
}

/// DERIVED FACTS: the inspector's facts plus what only a repository knows, which is when
/// the catalog first saw the artifact and how often this node has served it.
fn repo_facts(
    catalog: &Catalog,
    primary: Option<&SearchRow>,
    now: Timestamp,
) -> Result<Vec<Fact>, IndexError> {
    let Some(row) = primary else {
        return Ok(Vec::new());
    };
    let mut list = facts(row);
    let first_seen = catalog
        .artifact_detail(row.artifact_id)?
        .and_then(|detail| detail.first_seen)
        .map_or_else(|| "-".to_owned(), |seen| relative_time(now, seen));
    list.push(Fact::new("first seen", first_seen));
    let pulls = catalog.artifact_usage(row.artifact_id)?.pulls;
    list.push(Fact::new(PULLS_LABEL, pulls.to_string()));
    Ok(list)
}

/// The artifacts one revision of a repository names, most bytes first.
///
/// A repository's artifacts are shared across its revisions, so the revision a request
/// names is what decides which of them answer for it.
fn rows_in_revision(
    catalog: &Catalog,
    record: &RepositoryDetail,
    revision: u32,
) -> Result<Vec<SearchRow>, IndexError> {
    let entries = catalog.revision_entries(record.id, revision, Dedupe::Ignore)?;
    let mut rows = catalog.repository_rows(record.id, REPO_ARTIFACTS)?;
    rows.retain(|row| holds(&entries, row));
    Ok(rows)
}

/// The FILES tab's KIND column.
fn file_kind(path: &str, is_artifact: bool, single: bool) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name.starts_with(".git") {
        return "git";
    }
    if name.ends_with(".md") || name.ends_with(".markdown") {
        return "card";
    }
    if parse_shard_name(&name).is_some() {
        return "group";
    }
    let format = name
        .rsplit_once('.')
        .and_then(|(_, extension)| extension.parse::<ArtifactFormat>().ok());
    match format {
        Some(format) if format.holds_tensors() => match single && is_artifact {
            true => "single-file",
            false => "weights",
        },
        Some(ArtifactFormat::Parquet)
        | Some(ArtifactFormat::Arrow)
        | Some(ArtifactFormat::Jsonl)
        | Some(ArtifactFormat::Csv) => "weights",
        _ => "config",
    }
}

fn file_state(present: bool, lfs_pointer: bool, deduped: bool) -> FileState {
    if !present {
        return FileState::Absent;
    }
    if lfs_pointer {
        return FileState::LfsPtr;
    }
    match deduped {
        true => FileState::Deduped,
        false => FileState::Present,
    }
}

async fn read_card(path: &std::path::Path) -> Option<String> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > README_LIMIT {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok()
}

/// The card hoard writes when upstream never shipped one: what the index knows, and how to
/// pull the repository it describes. A category folder of a tool layout is nobody's
/// repository, so it is offered no line that would pull every file in it.
fn generated_card(
    url: &str,
    repo: &RepoId,
    kind: SourceKind,
    bin: bool,
    primary: Option<&SearchRow>,
) -> String {
    let mut text = format!(
        "# {}\n\nNo upstream card was recovered for this repository. This page is generated \
         from hoard's index record.\n\n## What we know\n\n",
        repo.reference()
    );
    match primary {
        Some(row) => {
            text.push_str(&format!("- role: {}\n", row.role.as_str()));
            text.push_str(&format!(
                "- base: {}\n",
                crate::api::common::base_label(&row.base, row.base_label.as_deref())
                    .unwrap_or_else(|| "unknown".to_owned())
            ));
            text.push_str(&format!("- format: {}\n", row.format.as_str()));
            text.push_str(&format!("- size: {}\n", format_size(row.size)));
            if let Some(trigger) = row.trigger.as_deref() {
                text.push_str(&format!("- trigger words: {trigger}\n"));
            }
        }
        None => text.push_str("- nothing has been classified in this repository yet\n"),
    }
    if bin {
        return text;
    }
    let commands = pull_commands(
        url,
        PullSubject {
            repo,
            kind,
            bin,
            artifact: None,
        },
        None,
    );
    text.push_str("\n## Pull it\n\n```\n");
    if let Some(git) = commands.git.as_deref() {
        text.push_str(git);
        text.push('\n');
    }
    text.push_str(&commands.hf);
    text.push_str("\n```\n");
    text
}

fn public_host(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit_once("//")
        .map(|(_, host)| host.to_owned())
        .unwrap_or_else(|| url.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_row_is_named_by_what_it_holds() {
        assert_eq!(file_kind("README.md", false, false), "card");
        assert_eq!(file_kind(".gitattributes", false, false), "git");
        assert_eq!(file_kind("config.json", false, false), "config");
        assert_eq!(file_kind("unet/model.safetensors", true, false), "weights");
        assert_eq!(file_kind("model.safetensors", true, true), "single-file");
        assert_eq!(
            file_kind("model-00001-of-00003.safetensors", true, false),
            "group"
        );
        assert_eq!(
            file_kind("something-of-note.safetensors", true, false),
            "weights",
            "a file with an unlucky name is not a shard"
        );
    }

    #[test]
    fn a_file_state_reads_the_way_the_design_colours_it() {
        assert_eq!(file_state(true, false, false), FileState::Present);
        assert_eq!(file_state(true, false, true), FileState::Deduped);
        assert_eq!(file_state(true, true, false), FileState::LfsPtr);
        assert_eq!(file_state(false, false, false), FileState::Absent);
    }

    #[test]
    fn the_public_host_is_what_a_served_badge_names() {
        assert_eq!(public_host("http://hoard.lan:7099"), "hoard.lan:7099");
        assert_eq!(public_host("hoard.lan"), "hoard.lan");
    }
}
