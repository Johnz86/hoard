use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{
    ArtifactId, Loadout, LoadoutId, LoadoutPin, LoadoutSlot, LoadoutViolation, PinResolution,
    RepoId, Role, State as ArtifactState, format_size,
};
use hoard_index::IndexError;
use hoard_index::store::row::{SearchRow, SqlValue};
use hoard_index::store::{blob, job, loadout, node, search_exec};

use crate::api::artifacts::artifact_id;
use crate::api::common::{Conn, authorize, require_row};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The job kind an apply runs under.
const APPLY_KIND: &str = "apply";
/// The slot an upscaler occupies, which is the one that reads a factor rather than a weight.
const FACTOR_SLOT: LoadoutSlot = LoadoutSlot::Upscaler;

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
    let rows = state
        .store()
        .call(|conn| {
            let mut out = Vec::new();
            for row in loadout::list_loadouts(conn)? {
                let document = row.parse()?;
                out.push(LoadoutSummary {
                    id: row.id,
                    name: row.name,
                    items: document.len(),
                    created_at: row.created_at,
                    updated_at: row.updated_at,
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
    let node_name = state.node_name().to_owned();
    let view = state
        .store()
        .call(move |conn| view_of(conn, id, &node_name))
        .await?;
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
    let view = state
        .store()
        .call_mut(move |conn| {
            let id = loadout::upsert_loadout(conn, &document)?;
            view_of(conn, id, &node_name)
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
    let view = state
        .store()
        .call_mut(move |conn| {
            let row = require_row(conn, artifact)?;
            let mut document = loadout::loadout(conn, id)?.parse()?;
            let slot = LoadoutSlot::for_role(row.role).ok_or_else(|| {
                IndexError::Query(format!("role {} cannot be pinned in a loadout", row.role))
            })?;
            document.remove_path(&row.path);
            document.add(row.role, pin_for(&row, slot, weight)?)?;
            loadout::update_loadout(conn, id, &document.to_toml()?)?;
            view_of(conn, id, &node_name)
        })
        .await
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
    let view = state
        .store()
        .call_mut(move |conn| {
            let row = require_row(conn, artifact)?;
            let mut document = loadout::loadout(conn, id)?.parse()?;
            if document.remove_path(&row.path) == 0 {
                return Err(IndexError::not_found("loadout line", row.path.clone()));
            }
            loadout::update_loadout(conn, id, &document.to_toml()?)?;
            view_of(conn, id, &node_name)
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
    let view = state
        .store()
        .call(move |conn| {
            let violations = loadout::loadout(conn, id)?.parse()?.violations();
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
    let profile = body.profile.clone().unwrap_or_else(|| "comfyui".to_owned());
    let node_name = target.clone();
    let (view, job_id) = state
        .store()
        .call_mut(move |conn| {
            if node::node_by_name(conn, &node_name)?.is_none() {
                return Err(IndexError::not_found("node", node_name.clone()));
            }
            let view = view_of(conn, id, &node_name)?;
            let subject = format!("{} \u{2192} {node_name}", view.name);
            let job = job::create_job(conn, APPLY_KIND, Some(&subject))?;
            Ok((view, job))
        })
        .await?;

    let preview = view.apply_preview.clone();
    let phase = format!(
        "{} present, {} pulling, {} missing for {profile}",
        preview.present, preview.pulling, preview.missing
    );
    let background = state.clone();
    let settle = preview.clone();
    tokio::spawn(async move {
        let outcome = background
            .store()
            .call_mut(move |conn| {
                job::update_job(
                    conn,
                    job_id,
                    &job::JobProgress {
                        phase: Some(phase),
                        progress: match settle.present + settle.pulling + settle.missing {
                            0 => 1.0,
                            total => {
                                f32::from(u16::try_from(settle.present).unwrap_or(u16::MAX))
                                    / f32::from(u16::try_from(total).unwrap_or(u16::MAX))
                            }
                        },
                        bytes: settle.bytes,
                        total: Some(settle.bytes),
                        rate_bps: None,
                    },
                )?;
                match settle.missing {
                    0 => job::finish_job(conn, job_id),
                    missing => job::fail_job(
                        conn,
                        job_id,
                        &format!("{missing} pinned lines are not on this LAN"),
                    ),
                }
            })
            .await;
        if let Err(error) = outcome {
            tracing::error!(%error, "the apply job could not be settled");
        }
    });
    Ok(Json(ApplyStarted {
        job_id,
        node: target,
        apply_preview: preview,
    }))
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
    let node_name = state.node_name().to_owned();
    let public = state.public_url().trim_end_matches('/').to_owned();
    let view = state
        .store()
        .call(move |conn| view_of(conn, id, &node_name))
        .await?;
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

/// The drawer's whole state for one loadout, resolved against the named node.
fn view_of(conn: &Conn, id: LoadoutId, node_name: &str) -> Result<LoadoutView, IndexError> {
    let record = loadout::loadout(conn, id)?;
    let document = record.parse()?;
    let mut items = Vec::with_capacity(document.len());
    for (slot, pin) in document.pins() {
        items.push(item_of(conn, slot, pin, node_name)?);
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
fn item_of(
    conn: &Conn,
    slot: LoadoutSlot,
    pin: &LoadoutPin,
    node_name: &str,
) -> Result<LoadoutItem, IndexError> {
    let candidates = resolve_pin(conn, pin)?;
    let held = candidates
        .iter()
        .find(|row| row.node == node_name && row.state.has_bytes());
    let elsewhere = candidates.iter().find(|row| row.state.has_bytes());
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

fn resolve_pin(conn: &Conn, pin: &LoadoutPin) -> Result<Vec<SearchRow>, IndexError> {
    if let Some(sha256) = pin.sha256
        && let Some(record) = blob::blob_by_sha256(conn, &sha256)?
    {
        let rows = crate::api::common::rows_for_blob(conn, record.id)?;
        if !rows.is_empty() {
            return Ok(rows);
        }
    }
    search_exec::query_rows(
        conn,
        "repo = ? AND path = ?",
        &[
            SqlValue::text(pin.repo.qualified()),
            SqlValue::text(&pin.path),
        ],
        "",
        8,
        0,
    )
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
        "#!/bin/sh\n# {} \u{2014} generated by hoard\nexport HF_ENDPOINT={public}\n\n",
        view.name
    );
    for item in &view.items {
        let reference = item
            .repo
            .split_once('/')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_else(|| item.repo.clone());
        text.push_str(&format!("hf download {reference} {}\n", item.path));
    }
    text
}

fn comfy_script(view: &LoadoutView) -> String {
    let mut text = format!(
        "#!/bin/sh\n# {} \u{2014} ComfyUI layout written by hoard\n\n",
        view.name
    );
    for item in &view.items {
        let directory = crate::api::link::comfy_directory(item.role);
        text.push_str(&format!(
            "mkdir -p \"$COMFYUI/models/{directory}\" && ln -sf \"{}\" \"$COMFYUI/models/{directory}/{}\"\n",
            item.path,
            item.name
        ));
    }
    text
}

fn loadout_id(raw: &str) -> ApiResult<LoadoutId> {
    raw.parse()
        .map_err(|_| ApiError::not_found(format!("loadout {raw}")))
}
