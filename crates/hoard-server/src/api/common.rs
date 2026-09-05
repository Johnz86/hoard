use std::hash::{DefaultHasher, Hash, Hasher};

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, Precision, RepoId, RepoRowId, RepoType,
    Role, Sha256, State, SubRole, format_size,
};
use hoard_index::IndexError;
use hoard_index::search::{
    CompatTarget, CompileContext, ParsedQuery, SortKey, SortSpec, TokenKind, compile, quoted,
};
use hoard_index::store::entry::EntryRow;
use hoard_index::store::row::{SearchRow, SqlValue};
use hoard_index::store::{revision, search_exec, shelf};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The SQLite connection the store hands to a blocking closure, named through the store
/// rather than by depending on rusqlite from the HTTP layer.
pub type Conn = <hoard_index::store::Reader as std::ops::Deref>::Target;

/// Rows returned when a request names no limit.
pub const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;
/// How many compatible artifacts the inspector's PLUGS INTO list shows.
pub const PLUGS_INTO_LIMIT: u32 = 4;
pub const MIDDOT: &str = "\u{b7}";
const TIMES: &str = "\u{d7}";

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_MONTH: i64 = 30 * SECONDS_PER_DAY;
const SECONDS_PER_YEAR: i64 = 365 * SECONDS_PER_DAY;

/// Rejects a mutating request that carries no valid node token.
pub fn authorize(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    state.authorize_write(presented)
}

/// The paging arguments every listing accepts.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageParams {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

impl PageParams {
    pub fn limit(&self) -> u32 {
        clamp_limit(self.limit)
    }
}

pub fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// An opaque position in one ordered result set, bound to the query that produced it so a
/// cursor from a different query is refused rather than silently skipping rows.
pub fn encode_cursor(offset: u32, key: &str) -> String {
    format!("{offset:x}-{:016x}", fingerprint(key))
}

pub fn decode_cursor(cursor: Option<&str>, key: &str) -> ApiResult<u32> {
    let Some(cursor) = cursor.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(0);
    };
    let invalid = || ApiError::bad_request(format!("cursor {cursor:?} is not from this query"));
    let (offset, digest) = cursor.split_once('-').ok_or_else(invalid)?;
    let offset = u32::from_str_radix(offset, 16).map_err(|_| invalid())?;
    let digest = u64::from_str_radix(digest, 16).map_err(|_| invalid())?;
    if digest != fingerprint(key) {
        return Err(invalid());
    }
    Ok(offset)
}

/// A cursor for a listing the store resumes by row id rather than by offset.
///
/// It names the last row of the page just returned, so the next page is the rows after it
/// however many rows were inserted or deleted in between.
pub fn encode_row_cursor(after: i64, key: &str) -> String {
    format!("{after:x}-{:016x}", fingerprint(key))
}

pub fn decode_row_cursor(cursor: Option<&str>, key: &str) -> ApiResult<Option<i64>> {
    let Some(cursor) = cursor.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let invalid = || ApiError::bad_request(format!("cursor {cursor:?} is not from this query"));
    let (after, digest) = cursor.split_once('-').ok_or_else(invalid)?;
    let after = i64::from_str_radix(after, 16).map_err(|_| invalid())?;
    let digest = u64::from_str_radix(digest, 16).map_err(|_| invalid())?;
    if digest != fingerprint(key) || after < 0 {
        return Err(invalid());
    }
    Ok(Some(after))
}

fn fingerprint(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// The cursor for the next page, or `None` when this page is the last one.
pub fn next_cursor(
    offset: u32,
    limit: u32,
    returned: usize,
    total: u64,
    key: &str,
) -> Option<String> {
    let seen = u64::from(offset) + returned as u64;
    (returned as u32 == limit && seen < total).then(|| encode_cursor(offset + limit, key))
}

/// The sort arguments the table header sends.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SortParams {
    pub sort: Option<String>,
    pub dir: Option<String>,
}

impl SortParams {
    pub fn spec(&self) -> ApiResult<SortSpec> {
        sort_spec(self.sort.as_deref(), self.dir.as_deref())
    }

    pub fn key(&self) -> String {
        sort_key(self.sort.as_deref(), self.dir.as_deref())
    }
}

pub fn sort_spec(sort: Option<&str>, dir: Option<&str>) -> ApiResult<SortSpec> {
    let key = match sort.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => raw.parse::<SortKey>().map_err(ApiError::from)?,
        None => SortKey::default(),
    };
    let descending = match dir.map(str::trim) {
        Some("desc") | Some("DESC") => true,
        None | Some("") | Some("asc") | Some("ASC") => false,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "{other:?} is not a sort direction"
            )));
        }
    };
    Ok(SortSpec::new(key, descending))
}

pub fn sort_key(sort: Option<&str>, dir: Option<&str>) -> String {
    format!("{}:{}", sort.unwrap_or_default(), dir.unwrap_or_default())
}

/// Everything compilation needs that the query text does not carry: the shelves a
/// `shelf:` token expands to, this node's name and the resolved `compat:` target.
pub fn compile_context(
    conn: &Conn,
    node: &str,
    query: &ParsedQuery,
    now: Timestamp,
) -> Result<CompileContext, IndexError> {
    let shelves = shelf::list_shelves(conn)?
        .into_iter()
        .map(|row| (row.name, row.query))
        .collect();
    let mut context = CompileContext::new(now)
        .with_local_node(node.to_owned())
        .with_shelves(shelves);
    if let Some(token) = query
        .tokens
        .iter()
        .find(|token| token.kind == TokenKind::Compat)
        && let Some(target) = resolve_compat_target(conn, &token.normalized, now)?
    {
        context = context.with_compat(target);
    }
    Ok(context)
}

/// Resolves what a `compat:` value names, because the rule reads a role and a base rather
/// than a string. A repository reference wins over free text.
pub fn resolve_compat_target(
    conn: &Conn,
    value: &str,
    now: Timestamp,
) -> Result<Option<CompatTarget>, IndexError> {
    for probe in [format!("repo:{}", quoted(value)), quoted(value)] {
        let query = hoard_index::search::parse(&probe);
        let context = CompileContext::new(now);
        let compiled = compile(&query, SortSpec::new(SortKey::Size, true), &context)?;
        let rows = search_exec::query_rows(
            conn,
            &compiled.where_sql,
            &compiled.params,
            &compiled.order_sql,
            32,
            0,
        )?;
        if let Some(row) = rows
            .iter()
            .find(|row| row.role.is_host())
            .or_else(|| rows.first())
        {
            return Ok(Some(CompatTarget::new(row.role, row.base.clone())));
        }
    }
    Ok(None)
}

/// One artifact of the `search_row` view, read by its identity rather than by a query.
pub fn row_by_artifact(conn: &Conn, id: ArtifactId) -> Result<Option<SearchRow>, IndexError> {
    let rows = search_exec::query_rows(
        conn,
        "artifact_id = ?",
        &[SqlValue::Int(id.get())],
        "",
        1,
        0,
    )?;
    Ok(rows.into_iter().next())
}

pub fn require_row(conn: &Conn, id: ArtifactId) -> Result<SearchRow, IndexError> {
    row_by_artifact(conn, id)?.ok_or_else(|| IndexError::not_found("artifact", id.to_string()))
}

/// Every artifact reachable through one repository, most bytes first.
pub fn rows_for_repo(
    conn: &Conn,
    repo: RepoRowId,
    limit: u32,
) -> Result<Vec<SearchRow>, IndexError> {
    search_exec::query_rows(
        conn,
        "repo_id = ?",
        &[SqlValue::Int(repo.get())],
        "size DESC, artifact_id ASC",
        limit,
        0,
    )
}

/// Every artifact sharing one blob, which is what a duplicate is.
pub fn rows_for_blob(conn: &Conn, blob: BlobId) -> Result<Vec<SearchRow>, IndexError> {
    search_exec::query_rows(
        conn,
        "blob_id = ?",
        &[SqlValue::Int(blob.get())],
        "",
        MAX_LIMIT,
        0,
    )
}

/// One row of the results table, the cards view and `hoard ls --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RowView {
    pub artifact_id: ArtifactId,
    pub blob_sha256: Option<Sha256>,
    pub role: Role,
    pub sub_role: Option<SubRole>,
    pub role_confidence: f32,
    pub org: Option<String>,
    pub name: String,
    pub base: Option<String>,
    pub format: ArtifactFormat,
    pub precision: Option<Precision>,
    pub size: u64,
    pub rank: Option<u32>,
    pub alpha: Option<f32>,
    pub params: Option<u64>,
    pub context: Option<u32>,
    pub quant: Option<String>,
    pub revision: String,
    pub revision_sha: Option<CommitSha>,
    pub node: String,
    pub state: State,
    pub pull_progress: Option<f32>,
    pub trigger: Option<String>,
    pub path: String,
    pub path_count: u32,
    /// How many files the model spans; 1 unless it is sharded.
    pub shard_count: u32,
    pub last_used: Option<Timestamp>,
    pub repo: String,
    pub repo_type: RepoType,
    pub size_human: String,
    pub state_label: String,
    pub role_code: String,
    pub dupe: bool,
    pub triage: bool,
}

impl From<&SearchRow> for RowView {
    fn from(row: &SearchRow) -> Self {
        Self {
            artifact_id: row.artifact_id,
            blob_sha256: row.blob_sha256,
            role: row.role,
            sub_role: row.sub_role,
            role_confidence: row.role_confidence,
            org: row.org.clone(),
            name: row.name.clone(),
            base: base_label(&row.base, row.base_label.as_deref()),
            format: row.format,
            precision: row.precision.clone(),
            size: row.size,
            rank: row.rank,
            alpha: row.alpha,
            params: row.params,
            context: row.context,
            quant: row.quant.clone(),
            revision: row.revision.clone(),
            revision_sha: row.revision_sha,
            node: row.node.clone(),
            state: row.state,
            pull_progress: row.pull_progress,
            trigger: row.trigger.clone(),
            path: row.abs_path.clone().unwrap_or_else(|| row.path.clone()),
            path_count: row.path_count,
            shard_count: row.shard_count,
            last_used: row.last_used,
            repo: row.repo.clone(),
            repo_type: repo_type_of(&row.repo),
            size_human: format_size(row.size),
            state_label: state_label(row.state, row.pull_progress, None),
            role_code: row.role.code().to_owned(),
            dupe: row.dupe,
            triage: row.triage,
        }
    }
}

/// The namespace of a qualified `type/org/name`, falling back to models.
pub fn repo_type_of(qualified: &str) -> RepoType {
    qualified
        .split_once('/')
        .and_then(|(head, _)| head.parse().ok())
        .unwrap_or(RepoType::Models)
}

/// One path of an artifact as the ENTRIES and PATHS ON DISK lists show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryView {
    pub repo: String,
    pub revision: String,
    pub path: String,
    pub node: String,
    pub label: String,
    pub state: State,
    pub state_label: String,
    pub symlink: bool,
}

/// Renders entries with the ref name each revision answers to, falling back to the short
/// sha exactly as the `search_row` view does.
pub fn entry_views(
    conn: &Conn,
    entries: &[EntryRow],
    host: &str,
) -> Result<Vec<EntryView>, IndexError> {
    let mut labels: Vec<(RepoRowId, Vec<(hoard_core::RevisionId, String)>)> = Vec::new();
    let mut views = Vec::with_capacity(entries.len());
    for entry in entries {
        if !labels.iter().any(|(repo, _)| *repo == entry.repo) {
            let refs = revision::refs_for_repo(conn, entry.repo)?
                .into_iter()
                .map(|reference| (reference.revision, reference.name))
                .collect();
            labels.push((entry.repo, refs));
        }
        let revision = labels
            .iter()
            .find(|(repo, _)| *repo == entry.repo)
            .and_then(|(_, refs)| {
                refs.iter()
                    .find(|(id, _)| *id == entry.revision)
                    .map(|(_, name)| name.clone())
            })
            .unwrap_or_else(|| entry.revision_sha.short());
        views.push(EntryView {
            repo: entry.repo_qualified.clone(),
            revision,
            path: entry.abs_path.clone().unwrap_or_else(|| entry.path.clone()),
            node: entry.node.clone(),
            label: match entry.is_symlink {
                true => format!("{} {MIDDOT} symlink", entry.node),
                false => entry.node.clone(),
            },
            state: entry.state,
            state_label: state_label(entry.state, entry.pull_progress, Some(host)),
            symlink: entry.is_symlink,
        });
    }
    Ok(views)
}

/// The display spelling of a base family, `sdxl 1.0` rather than the `sdxl10` key.
pub fn base_label(base: &Base, label: Option<&str>) -> Option<String> {
    label
        .map(str::to_owned)
        .or_else(|| (!base.is_empty()).then(|| base.to_string()))
}

/// One line of the inspector's facts list and the detail view's DERIVED FACTS panel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fact {
    pub key: String,
    pub value: String,
}

impl Fact {
    pub fn new(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            value: value.into(),
        }
    }
}

/// The inspector facts in the order the design lists them: role, base, format, size, node,
/// blob, then the role specific lines.
pub fn facts(row: &SearchRow) -> Vec<Fact> {
    let mut facts = vec![
        Fact::new("role", role_value(row.role, row.sub_role)),
        Fact::new(
            "base",
            base_label(&row.base, row.base_label.as_deref()).unwrap_or_else(|| "-".to_owned()),
        ),
        Fact::new("format", format_value(row.format, row.precision.as_ref())),
        Fact::new("size", format_size(row.size)),
        Fact::new("node", row.node.clone()),
        Fact::new("blob", blob_value(row.blob_sha256.as_ref())),
    ];
    if row.shard_count > 1 {
        facts.push(Fact::new("files", format!("{} shards", row.shard_count)));
    }
    match row.role {
        Role::Lora => facts.push(Fact::new("rank / alpha", rank_alpha(row.rank, row.alpha))),
        Role::Llm => {
            if let Some(context) = row.context {
                facts.push(Fact::new("context", context.to_string()));
            }
        }
        Role::Ckpt => {
            if let Some(resolution) = resolution(&row.base) {
                facts.push(Fact::new("resolution", resolution));
            }
        }
        Role::Ctrl | Role::Upsc | Role::Vae | Role::Emb | Role::Ds | Role::Unk => {}
    }
    facts
}

fn role_value(role: Role, sub_role: Option<SubRole>) -> String {
    let mut value = role.as_str().to_owned();
    if let Some(sub) = sub_role {
        value.push_str(&format!(" {MIDDOT} {}", sub.as_str()));
    }
    if role == Role::Unk {
        value.push_str(&format!(" {MIDDOT} guess"));
    }
    value
}

fn format_value(format: ArtifactFormat, precision: Option<&Precision>) -> String {
    match precision.filter(|precision| !precision.is_empty()) {
        Some(precision) => format!("{} {MIDDOT} {precision}", format.as_str()),
        None => format.as_str().to_owned(),
    }
}

fn blob_value(sha256: Option<&Sha256>) -> String {
    match sha256 {
        Some(sha) => format!("sha256:{}", sha.short()),
        None => "not hashed yet".to_owned(),
    }
}

fn rank_alpha(rank: Option<u32>, alpha: Option<f32>) -> String {
    match (rank, alpha) {
        (Some(rank), Some(alpha)) => format!("{rank} / {}", decimal(alpha)),
        (Some(rank), None) => rank.to_string(),
        (None, Some(alpha)) => format!("- / {}", decimal(alpha)),
        (None, None) => "-".to_owned(),
    }
}

/// Renders a measured float without a trailing `.0`, the way the design writes weights.
pub fn decimal(value: f32) -> String {
    match value.fract() == 0.0 {
        true => format!("{}", value as i64),
        false => format!("{value}"),
    }
}

/// The native training resolution a checkpoint's family implies. Absent rather than
/// guessed when the family is unknown.
fn resolution(base: &Base) -> Option<String> {
    let key = base.as_str();
    let side = if key.starts_with("sd15") || key.starts_with("sd14") || key == "sd1" {
        512
    } else if key.starts_with("sd2") {
        768
    } else if key.starts_with("sdxl") || key.starts_with("flux") || key.starts_with("sd3") {
        1024
    } else {
        return None;
    };
    Some(format!("{side} {TIMES} {side}"))
}

/// The state badge: `SERVED` in a table cell, `SERVED · hoard.lan` in a detail header.
pub fn state_label(state: State, progress: Option<f32>, host: Option<&str>) -> String {
    match (state, host) {
        (State::Served, Some(host)) => format!("{} {MIDDOT} {host}", state.label()),
        (State::Pulling, _) => match progress {
            Some(progress) => format!("{} {}%", state.label(), (progress * 100.0).round() as i64),
            None => state.label().to_owned(),
        },
        (State::Served, None)
        | (State::Local, _)
        | (State::Stale, _)
        | (State::Remote, _)
        | (State::Triage, _) => state.label().to_owned(),
    }
}

/// `4 months ago`, as the REFS tab and the detail header show times.
pub fn relative_time(now: Timestamp, then: Timestamp) -> String {
    let seconds = now.as_second() - then.as_second();
    if seconds < SECONDS_PER_MINUTE {
        return "just now".to_owned();
    }
    let (count, unit) = if seconds < SECONDS_PER_HOUR {
        (seconds / SECONDS_PER_MINUTE, "m")
    } else if seconds < SECONDS_PER_DAY {
        (seconds / SECONDS_PER_HOUR, "h")
    } else if seconds < SECONDS_PER_MONTH {
        (seconds / SECONDS_PER_DAY, "d")
    } else if seconds < SECONDS_PER_YEAR {
        return plural(seconds / SECONDS_PER_MONTH, "month");
    } else {
        return plural(seconds / SECONDS_PER_YEAR, "year");
    };
    format!("{count}{unit} ago")
}

fn plural(count: i64, unit: &str) -> String {
    match count {
        1 => format!("1 {unit} ago"),
        other => format!("{other} {unit}s ago"),
    }
}

/// The three ways to fetch a repository from this node, as the PULL TO THIS MACHINE box
/// and the `Y` yank both print them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PullCommands {
    pub git: String,
    pub hf: String,
    pub path: Option<String>,
}

pub fn pull_commands(state: &AppState, repo: &RepoId, path: Option<String>) -> PullCommands {
    let url = state.public_url().trim_end_matches('/');
    let reference = repo.reference();
    let repo_type = match repo.repo_type() {
        RepoType::Models => String::new(),
        other => format!(" --repo-type {}", other.singular()),
    };
    PullCommands {
        git: format!("git clone {url}/{reference}"),
        hf: format!("HF_ENDPOINT={url} hf download {reference}{repo_type}"),
        path,
    }
}

/// Reads `type/org/name` back out of the three path segments of a repo route.
pub fn repo_id(repo_type: &str, org: &str, name: &str) -> ApiResult<RepoId> {
    let repo_type: RepoType = repo_type
        .parse()
        .map_err(|_| ApiError::not_found(format!("repo type {repo_type}")))?;
    let reference = match org {
        "-" | "_" => name.to_owned(),
        org => format!("{org}/{name}"),
    };
    RepoId::parse_ref(repo_type, &reference).map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-09-04T09:10:00Z".parse().expect("timestamp parses")
    }

    #[test]
    fn a_cursor_round_trips_only_for_the_query_that_made_it() {
        let cursor = encode_cursor(100, "role:lora|size:desc");
        assert_eq!(
            decode_cursor(Some(&cursor), "role:lora|size:desc").expect("cursor decodes"),
            100
        );
        assert!(decode_cursor(Some(&cursor), "role:ckpt|size:desc").is_err());
        assert_eq!(
            decode_cursor(None, "any").expect("no cursor is page one"),
            0
        );
        assert!(decode_cursor(Some("nonsense"), "any").is_err());
    }

    #[test]
    fn a_row_cursor_names_the_last_row_and_refuses_another_query() {
        let cursor = encode_row_cursor(41, "models|");
        assert_eq!(
            decode_row_cursor(Some(&cursor), "models|").expect("the cursor is ours"),
            Some(41)
        );
        assert!(
            decode_row_cursor(None, "models|")
                .expect("no cursor is page one")
                .is_none()
        );
        assert!(decode_row_cursor(Some(&cursor), "datasets|").is_err());
        assert!(decode_row_cursor(Some("nonsense"), "models|").is_err());
    }

    #[test]
    fn the_last_page_carries_no_cursor() {
        assert_eq!(next_cursor(0, 2, 2, 5, "k"), Some(encode_cursor(2, "k")));
        assert_eq!(next_cursor(0, 2, 1, 1, "k"), None);
        assert_eq!(next_cursor(4, 2, 2, 6, "k"), None);
    }

    #[test]
    fn sort_parameters_reject_an_unknown_column_or_direction() {
        assert_eq!(
            sort_spec(Some("size"), Some("desc")).expect("spec"),
            SortSpec::new(SortKey::Size, true)
        );
        assert_eq!(sort_spec(None, None).expect("spec"), SortSpec::default());
        assert!(sort_spec(Some("colour"), None).is_err());
        assert!(sort_spec(None, Some("sideways")).is_err());
    }

    #[test]
    fn relative_times_read_the_way_the_design_writes_them() {
        let ago = |seconds: i64| {
            relative_time(
                now(),
                Timestamp::from_second(now().as_second() - seconds).expect("time"),
            )
        };
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(300), "5m ago");
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(3 * 86_400), "3d ago");
        assert_eq!(ago(4 * 30 * 86_400), "4 months ago");
        assert_eq!(ago(31 * 86_400), "1 month ago");
        assert_eq!(ago(400 * 86_400), "1 year ago");
    }

    #[test]
    fn checkpoint_resolution_is_derived_only_for_known_families() {
        assert_eq!(
            resolution(&Base::new("sdxl 1.0")).as_deref(),
            Some("1024 \u{d7} 1024")
        );
        assert_eq!(
            resolution(&Base::new("sd 1.5")).as_deref(),
            Some("512 \u{d7} 512")
        );
        assert_eq!(resolution(&Base::new("wan")), None);
    }

    #[test]
    fn the_state_badge_names_the_host_only_where_the_design_shows_it() {
        assert_eq!(
            state_label(State::Served, None, Some("hoard.lan")),
            "SERVED \u{b7} hoard.lan"
        );
        assert_eq!(state_label(State::Served, None, None), "SERVED");
        assert_eq!(state_label(State::Pulling, Some(0.64), None), "PULLING 64%");
        assert_eq!(state_label(State::Remote, None, Some("x")), "REMOTE");
    }

    #[test]
    fn a_repo_route_reads_back_the_repository_it_names() {
        let repo = repo_id("models", "org", "name").expect("route parses");
        assert_eq!(repo.qualified(), "models/org/name");
        assert_eq!(
            repo_id("datasets", "-", "squad")
                .expect("orgless route parses")
                .qualified(),
            "datasets/squad"
        );
        assert!(repo_id("nonsense", "org", "name").is_err());
    }

    #[test]
    fn a_qualified_repo_names_its_namespace() {
        assert_eq!(repo_type_of("datasets/org/name"), RepoType::Datasets);
        assert_eq!(repo_type_of("models/name"), RepoType::Models);
        assert_eq!(repo_type_of("loose-folder"), RepoType::Models);
    }
}
