use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{ArtifactId, Role, ShelfId};
use hoard_index::IndexError;
use hoard_index::search::{SortKey, SortSpec, compile, parse};
use hoard_index::store::projection::{NewProjection, record_projection};
use hoard_index::store::{search_exec, shelf};

use crate::api::common::{authorize, clamp_limit, compile_context};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The tool layouts `hoard link --profile` knows.
const PROFILES: [&str; 3] = ["comfyui", "a1111", "llama"];

/// One symlink the projection wrote.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkRow {
    pub artifact_id: ArtifactId,
    pub role: Role,
    pub name: String,
    pub target: String,
    pub link: String,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkView {
    pub profile: String,
    pub dir: String,
    pub written: Vec<LinkRow>,
    pub skipped: Vec<String>,
    pub total: usize,
}

/// What to project, where, and in whose layout.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkBody {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub shelf_id: Option<String>,
    pub profile: String,
    pub dir: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LinkBody>,
) -> ApiResult<Json<LinkView>> {
    authorize(&state, &headers)?;
    let profile = body.profile.trim().to_ascii_lowercase();
    if !PROFILES.contains(&profile.as_str()) {
        return Err(ApiError::bad_request(format!(
            "{profile:?} is not a projection profile"
        )));
    }
    let dir = PathBuf::from(body.dir.trim());
    if dir.as_os_str().is_empty() {
        return Err(ApiError::bad_request("a projection needs a directory"));
    }
    let limit = clamp_limit(body.limit);
    let shelf_id = body
        .shelf_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse::<ShelfId>)
        .transpose()
        .map_err(ApiError::from)?;
    let raw = body.query.clone();
    let node = state.node_name().to_owned();
    let now = Timestamp::now();

    let rows = state
        .store()
        .call(move |conn| {
            let text = match shelf_id {
                Some(id) => shelf::shelf(conn, id)?.query,
                None => raw.unwrap_or_default(),
            };
            let query = parse(&text);
            let context = compile_context(conn, &node, &query, now)?;
            let compiled = compile(&query, SortSpec::new(SortKey::Size, true), &context)?;
            search_exec::query_rows(
                conn,
                &compiled.where_sql,
                &compiled.params,
                &compiled.order_sql,
                limit,
                0,
            )
        })
        .await?;

    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for row in &rows {
        let Some(target) = row.abs_path.clone() else {
            skipped.push(row.name.clone());
            continue;
        };
        if !row.state.has_bytes() {
            skipped.push(row.name.clone());
            continue;
        }
        let file = target.rsplit('/').next().unwrap_or(&row.name).to_owned();
        let link = dir.join(directory(&profile, row.role)).join(&file);
        match write_link(Path::new(&target), &link).await {
            Ok(created) => written.push(LinkRow {
                artifact_id: row.artifact_id,
                role: row.role,
                name: row.name.clone(),
                target,
                link: link.to_string_lossy().into_owned(),
                created,
            }),
            Err(error) => {
                tracing::warn!(link = %link.display(), %error, "projection link was not written");
                skipped.push(row.name.clone());
            }
        }
    }
    record(&state, &profile, &written).await?;

    Ok(Json(LinkView {
        profile,
        dir: dir.to_string_lossy().into_owned(),
        total: written.len(),
        written,
        skipped,
    }))
}

/// Records the links this projection wrote, so PATHS ON DISK can tell a file hoard linked
/// from a file that was always there, and a later projection can replace its own links.
async fn record(state: &AppState, profile: &str, written: &[LinkRow]) -> ApiResult<()> {
    if written.is_empty() {
        return Ok(());
    }
    let entries: Vec<NewProjection> = written
        .iter()
        .map(|row| NewProjection {
            link_path: row.link.clone(),
            target_path: row.target.clone(),
            profile: profile.to_owned(),
            artifact_id: Some(row.artifact_id),
        })
        .collect();
    state
        .store()
        .call_mut(move |conn| {
            let transaction = conn.transaction()?;
            for entry in &entries {
                record_projection(&transaction, entry)?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Replaces an existing link rather than failing, so a projection can be re-run.
#[cfg(unix)]
async fn write_link(target: &Path, link: &Path) -> Result<bool, IndexError> {
    if let Some(parent) = link.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| IndexError::io(parent, error))?;
    }
    let existing = tokio::fs::read_link(link).await.ok();
    if existing.as_deref() == Some(target) {
        return Ok(false);
    }
    if existing.is_some() {
        tokio::fs::remove_file(link)
            .await
            .map_err(|error| IndexError::io(link, error))?;
    }
    tokio::fs::symlink(target, link)
        .await
        .map_err(|error| IndexError::io(link, error))?;
    Ok(true)
}

#[cfg(not(unix))]
async fn write_link(_target: &Path, link: &Path) -> Result<bool, IndexError> {
    Err(IndexError::io(
        link,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "hoard writes projections as symlinks, which this platform does not offer",
        ),
    ))
}

/// The directory a role lands in, per tool layout.
pub fn directory(profile: &str, role: Role) -> &'static str {
    match profile {
        "a1111" => a1111_directory(role),
        "llama" => "models",
        _ => comfy_directory(role),
    }
}

pub fn comfy_directory(role: Role) -> &'static str {
    match role {
        Role::Ckpt => "checkpoints",
        Role::Lora => "loras",
        Role::Ctrl => "controlnet",
        Role::Upsc => "upscale_models",
        Role::Vae => "vae",
        Role::Emb => "embeddings",
        Role::Llm => "llm",
        Role::Ds => "datasets",
        Role::Unk => "unclassified",
    }
}

fn a1111_directory(role: Role) -> &'static str {
    match role {
        Role::Ckpt => "Stable-diffusion",
        Role::Lora => "Lora",
        Role::Ctrl => "ControlNet",
        Role::Upsc => "ESRGAN",
        Role::Vae => "VAE",
        Role::Emb => "embeddings",
        Role::Llm => "llm",
        Role::Ds => "datasets",
        Role::Unk => "unclassified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_lands_somewhere_in_every_profile() {
        for profile in PROFILES {
            for role in [
                Role::Ckpt,
                Role::Lora,
                Role::Ctrl,
                Role::Upsc,
                Role::Vae,
                Role::Emb,
                Role::Llm,
                Role::Ds,
                Role::Unk,
            ] {
                assert!(!directory(profile, role).is_empty());
            }
        }
        assert_eq!(directory("comfyui", Role::Lora), "loras");
        assert_eq!(directory("a1111", Role::Ckpt), "Stable-diffusion");
        assert_eq!(directory("llama", Role::Llm), "models");
    }
}
