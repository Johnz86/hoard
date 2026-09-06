use std::path::Path as StdPath;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactId, JobId, Loadout, LoadoutId, LoadoutPin, LoadoutSlot, LoadoutViolation,
    PinResolution, RepoId, Role, State as ArtifactState, format_size,
};
use hoard_index::IndexError;
use hoard_index::catalog::{Catalog, JobProgress, LoadoutRecord, NewProjection};
use hoard_index::row::SearchRow;
use hoard_net::identity::NodeIdentity;
use hoard_net::{PullRequest, Puller, PullerConfig};

use crate::api::artifacts::artifact_id;
use crate::api::common::{authorize, blocking};
use crate::api::link::{self, Profile};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const APPLY_KIND: &str = "apply";
/// The slot an upscaler occupies, which is the one that reads a factor rather than a weight.
const FACTOR_SLOT: LoadoutSlot = LoadoutSlot::Upscaler;
/// The tool layout an apply writes when none is named.
const DEFAULT_PROFILE: Profile = Profile::ComfyUi;

/// One line of the loadout drawer, with whether the target node can satisfy it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutItem {
    pub slot: LoadoutSlot,
    pub role: Role,
    pub role_code: String,
    pub artifact_id: Option<ArtifactId>,
    pub name: String,
    pub repo: String,
    pub path: String,
    pub revision: Option<String>,
    pub weight: Option<f32>,
    pub weight_label: String,
    pub size: u64,
    pub size_human: String,
    pub state: Option<ArtifactState>,
    pub resolution: PinResolution,
}

/// What applying the loadout to a node would do.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ApplyPreview {
    pub present: u32,
    pub pulling: u32,
    pub missing: u32,
    pub bytes: u64,
    pub bytes_human: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutSummary {
    pub id: LoadoutId,
    pub name: String,
    pub items: usize,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoadoutView {
    pub id: LoadoutId,
    pub name: String,
    pub document: String,
    pub items: Vec<LoadoutItem>,
    pub apply_preview: ApplyPreview,
    pub violations: Vec<LoadoutViolation>,
    pub node: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationView {
    pub id: LoadoutId,
    pub valid: bool,
    pub violations: Vec<LoadoutViolation>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApplyStarted {
    pub job_id: hoard_core::JobId,
    pub node: String,
    pub apply_preview: ApplyPreview,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportView {
    pub format: String,
    pub filename: String,
    pub text: String,
}

/// A new loadout, given either as the document text or as the document itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CreateBody {
    Text { text: String },
    Document(Box<Loadout>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemBody {
    pub artifact_id: String,
    #[serde(default)]
    pub weight: Option<f32>,
}

/// What an apply names: the node whose disk is written and the tool layout to write.
///
/// Where the links land is not part of the request. An apply writes this node's own
/// `profiles/<profile>` directory, which is what the contract documents.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApplyBody {
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExportParams {
    pub format: Option<String>,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<LoadoutSummary>>> {
    let catalog = Arc::clone(state.catalog());
    let rows = blocking(move || {
        let mut out = Vec::new();
        for record in catalog.list_loadouts()? {
            let items = record.parse()?.len();
            out.push(LoadoutSummary {
                id: record.id,
                name: record.name,
                items,
                created_at: record.created_at,
                updated_at: record.updated_at,
            });
        }
        Ok(out)
    })
    .await?;
    Ok(Json(rows))
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<LoadoutView>> {
    let id = loadout_id(&id)?;
    let view = view(&state, id).await?;
    Ok(Json(view))
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<LoadoutView>> {
    authorize(&state, &headers)?;
    let document = match body {
        CreateBody::Text { text } => Loadout::parse(&text).map_err(ApiError::from)?,
        CreateBody::Document(document) => *document,
    };
    if document.name.trim().is_empty() {
        return Err(ApiError::bad_request("a loadout needs a name"));
    }
    let node_name = state.node_name().to_owned();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let id = catalog.upsert_loadout(&document)?;
        view_of(&catalog, id, &node_name)
    })
    .await?;
    Ok(Json(view))
}

pub async fn add_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ItemBody>,
) -> ApiResult<Json<LoadoutView>> {
    authorize(&state, &headers)?;
    let id = loadout_id(&id)?;
    let artifact = artifact_id(&body.artifact_id)?;
    let weight = body.weight;
    let node_name = state.node_name().to_owned();
    let catalog = Arc::clone(state.catalog());
    let view = tokio::task::spawn_blocking(move || {
        let row = require_catalog_row(&catalog, artifact)?;
        let mut document = stored(&catalog, id)?.parse()?;
        let slot = LoadoutSlot::for_role(row.role).ok_or_else(|| {
            IndexError::Query(format!("role {} cannot be pinned in a loadout", row.role))
        })?;
        document.remove_path(&row.path);
        document.add(row.role, pin_for(&row, slot, weight)?)?;
        catalog.update_loadout(id, &document.to_toml()?)?;
        view_of(&catalog, id, &node_name)
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(unpinnable)?;
    Ok(Json(view))
}

pub async fn remove_item(
    State(state): State<AppState>,
    Path((id, artifact)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<LoadoutView>> {
    authorize(&state, &headers)?;
    let id = loadout_id(&id)?;
    let artifact = artifact_id(&artifact)?;
    let node_name = state.node_name().to_owned();
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let row = require_catalog_row(&catalog, artifact)?;
        let mut document = stored(&catalog, id)?.parse()?;
        if document.remove_path(&row.path) == 0 {
            return Err(IndexError::not_found("loadout line", row.path.clone()));
        }
        catalog.update_loadout(id, &document.to_toml()?)?;
        view_of(&catalog, id, &node_name)
    })
    .await?;
    Ok(Json(view))
}

pub async fn validate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<ValidationView>> {
    authorize(&state, &headers)?;
    let id = loadout_id(&id)?;
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        let violations = stored(&catalog, id)?.parse()?.violations();
        Ok(ValidationView {
            id,
            valid: violations.is_empty(),
            violations,
        })
    })
    .await?;
    Ok(Json(view))
}

pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ApplyBody>,
) -> ApiResult<Json<ApplyStarted>> {
    authorize(&state, &headers)?;
    let id = loadout_id(&id)?;
    let target = body
        .node
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(state.node_name())
        .to_owned();
    let profile = match body
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(named) => Profile::parse(named)?,
        None => DEFAULT_PROFILE,
    };
    let node_name = target.clone();
    let known = node_name.clone();
    let catalog = Arc::clone(state.catalog());
    blocking(move || match catalog.node_by_name(&known)? {
        Some(_) => Ok(()),
        None => Err(IndexError::not_found("node", known.clone())),
    })
    .await?;
    let view = view_for(&state, id, &node_name).await?;
    let subject = format!("{} \u{2192} {node_name}", view.name);
    let catalog = Arc::clone(state.catalog());
    let job_id = blocking(move || catalog.create_job(APPLY_KIND, Some(&subject))).await?;

    let preview = view.apply_preview.clone();
    let dir = link::projection_dir(&state, profile, None)?;
    let local = target == state.node_name();
    let background = state.clone();
    tokio::spawn(async move {
        let settled = match local {
            true => run_apply(&background, id, job_id, profile, &dir).await,
            false => report_only(&background, job_id, &preview, profile).await,
        };
        if let Err(error) = settled {
            tracing::error!(code = error.code(), "the apply job could not be settled");
        }
    });
    Ok(Json(ApplyStarted {
        job_id,
        node: target,
        apply_preview: view.apply_preview,
    }))
}

/// Applying a loadout to a different node is a resolution report: only the node that owns
/// the disk can pull to it and write its links.
async fn report_only(
    state: &AppState,
    job: JobId,
    preview: &ApplyPreview,
    profile: Profile,
) -> ApiResult<()> {
    let phase = format!(
        "{} present, {} elsewhere, {} missing for {}",
        preview.present,
        preview.pulling,
        preview.missing,
        profile.as_str()
    );
    let bytes = preview.bytes;
    let missing = preview.missing;
    let catalog = Arc::clone(state.catalog());
    blocking(move || {
        catalog.update_job(
            job,
            &JobProgress {
                phase: Some(phase),
                progress: 1.0,
                bytes,
                total: Some(bytes),
                rate_bps: None,
            },
        )?;
        match missing {
            0 => catalog.finish_job(job),
            missing => {
                catalog.fail_job(job, &format!("{missing} pinned lines are not on this LAN"))
            }
        }
    })
    .await?;
    Ok(())
}

async fn run_apply(
    state: &AppState,
    id: LoadoutId,
    job: JobId,
    profile: Profile,
    dir: &StdPath,
) -> ApiResult<()> {
    let plan = plan_of(state, id).await?;
    let total = plan.len().max(1) as f32;
    let puller = puller(state)?;
    let mut ready: Vec<ReadyLine> = Vec::new();
    let mut missing = Vec::new();
    let mut bytes = 0u64;

    for (index, line) in plan.iter().enumerate() {
        let phase = format!("{} ({})", line.name, profile.as_str());
        let progress = index as f32 / total;
        let pulled = bytes;
        let catalog = Arc::clone(state.catalog());
        blocking(move || {
            catalog.update_job(
                job,
                &JobProgress {
                    phase: Some(phase),
                    progress,
                    bytes: pulled,
                    total: None,
                    rate_bps: None,
                },
            )
        })
        .await?;
        match line.source.clone() {
            LineSource::Held(path) => ready.push(ReadyLine {
                artifact: line.artifact,
                role: line.role,
                name: line.name.clone(),
                target: path,
            }),
            LineSource::Peer(request) => match puller.pull(&request, None).await {
                Ok(outcome) => {
                    bytes += outcome.size;
                    ready.push(ReadyLine {
                        artifact: Some(outcome.artifact),
                        role: line.role,
                        name: line.name.clone(),
                        target: outcome.abs_path.to_string_lossy().into_owned(),
                    });
                }
                Err(error) => {
                    tracing::warn!(line = %line.name, %error, "a pinned line could not be pulled");
                    missing.push(line.name.clone());
                }
            },
            LineSource::Nowhere => missing.push(line.name.clone()),
        }
    }

    let written = write_links(state, profile, dir, &ready).await?;
    let phase = format!("{written} links written to {}", dir.display());
    let catalog = Arc::clone(state.catalog());
    blocking(move || {
        catalog.update_job(
            job,
            &JobProgress {
                phase: Some(phase),
                progress: 1.0,
                bytes,
                total: Some(bytes),
                rate_bps: None,
            },
        )?;
        match missing.is_empty() {
            true => catalog.finish_job(job),
            false => catalog.fail_job(
                job,
                &format!("{} pinned lines are not on this LAN", missing.len()),
            ),
        }
    })
    .await?;
    Ok(())
}

fn puller(state: &AppState) -> ApiResult<Puller> {
    Puller::new(PullerConfig {
        catalog: Arc::clone(state.catalog()),
        mirror: Arc::clone(state.mirror()),
        root: state
            .config()
            .server
            .data_dir
            .join(hoard_net::peer::PULL_DIR),
        serve: state.config().node.serve,
        triage_confidence: state.config().index.triage_confidence,
    })
    .map_err(|error| ApiError::internal(error.to_string()))
}

#[derive(Debug, Clone)]
enum LineSource {
    Held(String),
    Peer(Box<PullRequest>),
    Nowhere,
}

#[derive(Debug, Clone)]
struct PlannedLine {
    artifact: Option<ArtifactId>,
    role: Role,
    name: String,
    source: LineSource,
}

#[derive(Debug, Clone)]
struct ReadyLine {
    artifact: Option<ArtifactId>,
    role: Role,
    name: String,
    target: String,
}

async fn plan_of(state: &AppState, id: LoadoutId) -> ApiResult<Vec<PlannedLine>> {
    let catalog = Arc::clone(state.catalog());
    let resolved = blocking(move || {
        let document = stored(&catalog, id)?.parse()?;
        let mut resolved = Vec::with_capacity(document.len());
        for (_, pin) in document.pins() {
            resolved.push((pin.clone(), catalog.resolve_pin(pin)?));
        }
        Ok(resolved)
    })
    .await?;
    let local = state.node_name().to_owned();
    let catalog = Arc::clone(state.catalog());
    let plan = blocking(move || {
        let mut lines = Vec::with_capacity(resolved.len());
        for (pin, candidates) in &resolved {
            lines.push(plan_line(&catalog, pin, candidates, &local)?);
        }
        Ok(lines)
    })
    .await?;
    Ok(plan)
}

fn plan_line(
    catalog: &Catalog,
    pin: &LoadoutPin,
    candidates: &[SearchRow],
    local: &str,
) -> Result<PlannedLine, IndexError> {
    let held = candidates
        .iter()
        .find(|row| row.node == local && row.state.has_bytes() && row.abs_path.is_some());
    let elsewhere = candidates
        .iter()
        .find(|row| row.node != local && row.blob_sha256.is_some());
    let row = held.or(elsewhere).or_else(|| candidates.first());
    let name = row.map_or_else(
        || pin.display_name().to_owned(),
        |row| file_name(&row.path).to_owned(),
    );
    let source = match (held, elsewhere) {
        (Some(row), _) => LineSource::Held(row.abs_path.clone().unwrap_or_default()),
        (None, Some(row)) => match pull_request(catalog, row)? {
            Some(request) => LineSource::Peer(Box::new(request)),
            None => LineSource::Nowhere,
        },
        (None, None) => LineSource::Nowhere,
    };
    Ok(PlannedLine {
        artifact: row.map(|row| row.artifact_id),
        role: row.map_or(Role::Unk, |row| row.role),
        name,
        source,
    })
}

fn pull_request(catalog: &Catalog, row: &SearchRow) -> Result<Option<PullRequest>, IndexError> {
    let Some(sha256) = row.blob_sha256 else {
        return Ok(None);
    };
    let Some(owner) = catalog.node_by_name(&row.node)? else {
        return Ok(None);
    };
    let Some(url) = owner.url.as_deref() else {
        return Ok(None);
    };
    let Some(revision) = row.revision_sha else {
        return Ok(None);
    };
    let peer = NodeIdentity::new(owner.node_id.clone(), owner.name.clone(), url)
        .map_err(|error| IndexError::Config(error.to_string()))?;
    let repo = RepoId::parse_qualified(&row.repo)?;
    Ok(Some(PullRequest::new(
        peer,
        repo,
        revision,
        row.path.clone(),
        sha256,
        row.size,
    )))
}

/// Writes the profile's layout and records what it wrote, so PATHS ON DISK can tell a link
/// hoard made from a file that was always there.
async fn write_links(
    state: &AppState,
    profile: Profile,
    dir: &StdPath,
    lines: &[ReadyLine],
) -> ApiResult<usize> {
    let mut written = Vec::new();
    for line in lines {
        if line.target.is_empty() {
            continue;
        }
        let link = dir.join(profile.directory(line.role)).join(&line.name);
        match link::write_link(StdPath::new(&line.target), &link).await {
            Ok(_) => written.push(NewProjection {
                link_path: link.to_string_lossy().into_owned(),
                target_path: line.target.clone(),
                profile: profile.as_str().to_owned(),
                artifact_id: line.artifact,
            }),
            Err(error) => {
                tracing::warn!(link = %link.display(), %error, "an apply link was not written");
            }
        }
    }
    let count = written.len();
    if count > 0 {
        let catalog = Arc::clone(state.catalog());
        blocking(move || catalog.record_projections(&written)).await?;
    }
    Ok(count)
}

fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

pub async fn export(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ExportParams>,
) -> ApiResult<Json<ExportView>> {
    let id = loadout_id(&id)?;
    let format = params
        .format
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("toml")
        .to_ascii_lowercase();
    let public = state.public_url().trim_end_matches('/').to_owned();
    let view = view(&state, id).await?;
    let text = match format.as_str() {
        "toml" => view.document.clone(),
        "hf-script" => hf_script(&view, &public),
        "comfy" => comfy_script(&view),
        other => {
            return Err(ApiError::bad_request(format!(
                "{other:?} is not an export format"
            )));
        }
    };
    let filename = match format.as_str() {
        "toml" => format!("{}.toml", view.name),
        "hf-script" => format!("{}.sh", view.name),
        _ => format!("{}-comfyui.sh", view.name),
    };
    Ok(Json(ExportView {
        format,
        filename,
        text,
    }))
}

/// The drawer's view of one loadout, resolved against this node.
async fn view(state: &AppState, id: LoadoutId) -> ApiResult<LoadoutView> {
    let node_name = state.node_name().to_owned();
    view_for(state, id, &node_name).await
}

/// The same view resolved against another node, which is what an apply previews.
async fn view_for(state: &AppState, id: LoadoutId, node_name: &str) -> ApiResult<LoadoutView> {
    let catalog = Arc::clone(state.catalog());
    let node_name = node_name.to_owned();
    blocking(move || view_of(&catalog, id, &node_name)).await
}

fn stored(catalog: &Catalog, id: LoadoutId) -> Result<LoadoutRecord, IndexError> {
    catalog
        .loadout(id)?
        .ok_or_else(|| IndexError::not_found("loadout", id.to_string()))
}

fn require_catalog_row(catalog: &Catalog, id: ArtifactId) -> Result<SearchRow, IndexError> {
    catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::not_found("artifact", id.to_string()))
}

fn view_of(catalog: &Catalog, id: LoadoutId, node_name: &str) -> Result<LoadoutView, IndexError> {
    let record = stored(catalog, id)?;
    let document = record.parse()?;
    let mut items = Vec::with_capacity(document.len());
    for (slot, pin) in document.pins() {
        items.push(item_of(catalog, slot, pin, node_name)?);
    }
    let preview = preview_of(&items);
    Ok(LoadoutView {
        id: record.id,
        name: record.name,
        document: record.document,
        apply_preview: preview,
        violations: document.violations(),
        node: node_name.to_owned(),
        items,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn preview_of(items: &[LoadoutItem]) -> ApplyPreview {
    let mut preview = ApplyPreview::default();
    for item in items {
        match &item.resolution {
            PinResolution::Present => preview.present += 1,
            PinResolution::Elsewhere { .. } => {
                preview.pulling += 1;
                preview.bytes += item.size;
            }
            PinResolution::Missing => preview.missing += 1,
        }
    }
    preview.bytes_human = format_size(preview.bytes);
    preview
}

/// Resolves one pinned line: by content hash first, then by repository, commit and path.
///
/// A row filed against another node is REMOTE from here, which is exactly what "elsewhere"
/// means: that node holds the bytes and this one does not.
fn item_of(
    catalog: &Catalog,
    slot: LoadoutSlot,
    pin: &LoadoutPin,
    node_name: &str,
) -> Result<LoadoutItem, IndexError> {
    let candidates = catalog.resolve_pin(pin)?;
    let held = candidates
        .iter()
        .find(|row| row.node == node_name && row.state.has_bytes());
    let elsewhere = candidates
        .iter()
        .find(|row| row.node != node_name && row.blob_sha256.is_some());
    let resolution = match (held, elsewhere) {
        (Some(_), _) => PinResolution::Present,
        (None, Some(row)) => PinResolution::Elsewhere {
            node: row.node.clone(),
        },
        (None, None) => PinResolution::Missing,
    };
    let row = held.or(elsewhere).or_else(|| candidates.first());
    let size = row.map_or(0, |row| row.size);
    Ok(LoadoutItem {
        slot,
        role: row.map_or(slot.role(), |row| row.role),
        role_code: row.map_or(slot.role(), |row| row.role).code().to_owned(),
        artifact_id: row.map(|row| row.artifact_id),
        name: pin.display_name().to_owned(),
        repo: pin.repo.qualified(),
        path: pin.path.clone(),
        revision: row
            .map(|row| row.revision.clone())
            .or_else(|| pin.sha.map(|sha| sha.short())),
        weight: pin.weight.or(pin.factor),
        weight_label: pin.weight_label(),
        size,
        size_human: format_size(size),
        state: row.map(|row| row.state),
        resolution,
    })
}

/// The pin an artifact becomes: hashed identity first so the same document resolves on any
/// node, with the repository and path as the fallback.
fn pin_for(
    row: &SearchRow,
    slot: LoadoutSlot,
    weight: Option<f32>,
) -> Result<LoadoutPin, IndexError> {
    let repo = RepoId::parse_qualified(&row.repo)?;
    let mut pin = LoadoutPin::new(repo, row.path.clone());
    pin.sha = row.revision_sha;
    pin.sha256 = row.blob_sha256;
    pin.name = Some(row.name.clone());
    pin.base = (!row.base.is_empty()).then(|| row.base.clone());
    match slot {
        FACTOR_SLOT => pin.factor = weight,
        _ => pin.weight = weight,
    }
    Ok(pin)
}

/// A role with nowhere to sit in a tool layout is a conflict with the loadout's shape.
fn unpinnable(error: IndexError) -> ApiError {
    match error {
        IndexError::Query(message) if message.contains("cannot be pinned") => {
            ApiError::conflict(message)
        }
        IndexError::Domain(domain) => ApiError::conflict(domain.to_string()),
        other => ApiError::from(other),
    }
}

fn hf_script(view: &LoadoutView, public: &str) -> String {
    let mut text = format!(
        "#!/bin/sh\n# {} \u{2014} generated by hoard\nexport HF_ENDPOINT={}\n\n",
        view.name,
        shell_quoted(public)
    );
    for item in &view.items {
        let reference = item
            .repo
            .split_once('/')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_else(|| item.repo.clone());
        text.push_str(&format!(
            "hf download {} {}\n",
            shell_quoted(&reference),
            shell_quoted(&item.path)
        ));
    }
    text
}

fn comfy_script(view: &LoadoutView) -> String {
    let mut text = format!(
        "#!/bin/sh\n# {} \u{2014} ComfyUI layout written by hoard\n\n",
        view.name
    );
    for item in &view.items {
        let directory = link::comfy_directory(item.role);
        text.push_str(&format!(
            "mkdir -p \"$COMFYUI/models/{directory}\" && ln -sf {} \"$COMFYUI/models/{directory}\"/{}\n",
            shell_quoted(&item.path),
            shell_quoted(&item.name)
        ));
    }
    text
}

/// One argument of a generated script, quoted so a name carrying a space, a quote or a
/// shell metacharacter is one word rather than several.
///
/// Single quotes, because nothing inside them is expanded. `$COMFYUI` is the one thing the
/// script does expand, so it is written outside these quotes and the untrusted half of the
/// path is concatenated to it.
fn shell_quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        match character {
            '\'' => quoted.push_str("'\\''"),
            other => quoted.push(other),
        }
    }
    quoted.push('\'');
    quoted
}

fn loadout_id(raw: &str) -> ApiResult<LoadoutId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("loadout {raw}")))
}

#[cfg(test)]
mod tests {
    use hoard_core::{CommitSha, NodeId, RepoType, Sha256};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// One `search_row` as the peer holding it would be reported, built through the shape
    /// the API already serializes so the test cannot drift from the view.
    fn row(node: &str, sha256: Option<Sha256>) -> SearchRow {
        serde_json::from_value(json!({
            "artifact_id": 1, "blob_id": 1,
            "blob_sha256": sha256.map(Sha256::to_hex),
            "role": "lora", "sub_role": null, "role_confidence": 0.98,
            "org": "kohya", "name": "film-grain", "base": "sdxl", "base_label": "SDXL 1.0",
            "format": "safetensors", "precision": "fp16", "quant": null,
            "size": 239_075_328u64, "rank": 32, "alpha": 16.0, "params": null, "context": null,
            "revision": "main", "revision_sha": CommitSha::from_bytes([3u8; 20]).to_hex(),
            "node": node, "state": "remote", "pull_progress": null, "trigger": null,
            "path": "loras/film-grain.safetensors", "abs_path": null,
            "path_count": 1, "dupe": false, "triage": false, "last_used": null,
            "repo": "models/kohya/film-grain", "repo_id": 1, "entry_id": 1,
            "source": null, "is_symlink": false, "lfs_pointer": false
        }))
        .expect("a search row parses")
    }

    /// A catalog knowing only this node and one peer, reachable or not.
    fn catalog_with_peer(dir: &TempDir, url: Option<&str>) -> Arc<Catalog> {
        let catalog = crate::testing::empty_catalog(dir.path(), "workstation");
        catalog
            .ensure_local_node(&NodeId::new("workstation-1").expect("id"), "workstation")
            .expect("local node");
        catalog
            .upsert_peer(&NodeId::new("laptop-1").expect("id"), "laptop", url)
            .expect("peer node");
        catalog
    }

    #[test]
    fn a_generated_script_quotes_every_name_it_interpolates() {
        assert_eq!(shell_quoted("plain.safetensors"), "'plain.safetensors'");
        assert_eq!(shell_quoted("a b.safetensors"), "'a b.safetensors'");
        assert_eq!(
            shell_quoted("$(rm -rf ~)"),
            "'$(rm -rf ~)'",
            "a command substitution must survive as text"
        );
        assert_eq!(shell_quoted("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_link_is_named_after_the_file_the_pin_points_at() {
        assert_eq!(
            file_name("loras/film-grain.safetensors"),
            "film-grain.safetensors"
        );
        assert_eq!(file_name("model.gguf"), "model.gguf");
        assert_eq!(file_name("a/b/"), "b");
    }

    #[test]
    fn a_row_on_a_reachable_peer_becomes_a_pull() {
        let dir = TempDir::new().expect("temp dir");
        let catalog = catalog_with_peer(&dir, Some("http://laptop.lan:7099"));
        let sha256 = Sha256::from_bytes([9u8; 32]);
        let request = pull_request(&catalog, &row("laptop", Some(sha256)))
            .expect("resolution runs")
            .expect("a reachable peer is pullable");
        assert_eq!(request.sha256, sha256);
        assert_eq!(request.peer.url().as_str(), "http://laptop.lan:7099/");
        assert_eq!(
            request.repo,
            RepoId::new(RepoType::Models, Some("kohya".to_owned()), "film-grain")
        );
        assert_eq!(request.path, "loras/film-grain.safetensors");
        assert_eq!(request.revision, CommitSha::from_bytes([3u8; 20]));
    }

    #[test]
    fn a_row_without_a_hash_or_an_address_is_not_pullable() {
        let dir = TempDir::new().expect("temp dir");
        let reachable = catalog_with_peer(&dir, Some("http://laptop.lan:7099"));
        assert!(
            pull_request(&reachable, &row("laptop", None))
                .expect("resolution runs")
                .is_none()
        );

        let other = TempDir::new().expect("temp dir");
        let addressless = catalog_with_peer(&other, None);
        assert!(
            pull_request(
                &addressless,
                &row("laptop", Some(Sha256::from_bytes([9u8; 32])))
            )
            .expect("resolution runs")
            .is_none()
        );
    }
}
