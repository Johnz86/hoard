use std::path::{Component, Path, PathBuf};

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
///
/// An enum rather than a string, so a role's directory is chosen by an exhaustive match
/// and an unknown profile is refused where it is read rather than silently becoming
/// ComfyUI's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    ComfyUi,
    A1111,
    Llama,
}

impl Profile {
    /// Every layout, in the order the CLI documents them.
    pub const ALL: [Profile; 3] = [Profile::ComfyUi, Profile::A1111, Profile::Llama];

    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::ComfyUi => "comfyui",
            Profile::A1111 => "a1111",
            Profile::Llama => "llama",
        }
    }

    /// Reads what a request named, ignoring case and surrounding space.
    pub fn parse(raw: &str) -> ApiResult<Self> {
        let wanted = raw.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|profile| profile.as_str() == wanted)
            .ok_or_else(|| ApiError::bad_request(format!("{wanted:?} is not a projection profile")))
    }

    /// The directory a role lands in, in this tool's layout.
    pub const fn directory(self, role: Role) -> &'static str {
        match self {
            Profile::ComfyUi => comfy_directory(role),
            Profile::A1111 => a1111_directory(role),
            Profile::Llama => "models",
        }
    }
}

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
    pub profile: Profile,
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
    /// Where the links land. Empty names the profile's own directory under the node's
    /// projection root.
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LinkBody>,
) -> ApiResult<Json<LinkView>> {
    authorize(&state, &headers)?;
    let profile = Profile::parse(&body.profile)?;
    let dir = projection_dir(&state, profile, body.dir.as_deref())?;
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
        let link = dir.join(profile.directory(row.role)).join(&file);
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
    record(&state, profile, &written).await?;

    Ok(Json(LinkView {
        profile,
        dir: dir.to_string_lossy().into_owned(),
        total: written.len(),
        written,
        skipped,
    }))
}

/// Confines a projection to the directories this node is configured to write into.
///
/// A projection creates directories and replaces symlinks, so where it may do that is a
/// property of the node rather than of the request: `data_dir/profiles` always, plus
/// whatever `server.projection_roots` names. A relative directory resolves under the
/// profile's own directory in the projection root.
pub fn projection_dir(state: &AppState, profile: Profile, dir: Option<&str>) -> ApiResult<PathBuf> {
    let root = state.projection_root();
    let Some(wanted) = dir.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(root.join(profile.as_str()));
    };
    let candidate = Path::new(wanted);
    if candidate.is_absolute() {
        let roots = std::iter::once(root.as_path()).chain(
            state
                .config()
                .server
                .projection_roots
                .iter()
                .map(PathBuf::as_path),
        );
        let allowed = roots
            .filter(|allowed| candidate.starts_with(allowed))
            .count()
            > 0;
        return match allowed && !candidate.components().any(is_upward) {
            true => Ok(candidate.to_path_buf()),
            false => Err(outside(wanted, &root)),
        };
    }
    let mut resolved = root.join(profile.as_str());
    for component in candidate.components() {
        match component {
            Component::Normal(segment) => resolved.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(outside(wanted, &root));
            }
        }
    }
    Ok(resolved)
}

fn is_upward(component: Component<'_>) -> bool {
    matches!(component, Component::ParentDir)
}

fn outside(wanted: &str, root: &Path) -> ApiError {
    ApiError::bad_request(format!(
        "{wanted:?} is outside {} and is not a configured projection root",
        root.display()
    ))
}

/// Records the links this projection wrote, so PATHS ON DISK can tell a file hoard linked
/// from a file that was always there, and a later projection can replace its own links.
async fn record(state: &AppState, profile: Profile, written: &[LinkRow]) -> ApiResult<()> {
    if written.is_empty() {
        return Ok(());
    }
    let entries: Vec<NewProjection> = written
        .iter()
        .map(|row| NewProjection {
            link_path: row.link.clone(),
            target_path: row.target.clone(),
            profile: profile.as_str().to_owned(),
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
///
/// Only a symlink is replaced. A regular file already at the link path is left alone and
/// the symlink call fails, because a projection writes links and never deletes content.
#[cfg(unix)]
pub(crate) async fn write_link(target: &Path, link: &Path) -> Result<bool, IndexError> {
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
pub(crate) async fn write_link(_target: &Path, link: &Path) -> Result<bool, IndexError> {
    Err(IndexError::io(
        link,
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "hoard writes projections as symlinks, which this platform does not offer",
        ),
    ))
}

pub const fn comfy_directory(role: Role) -> &'static str {
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

const fn a1111_directory(role: Role) -> &'static str {
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
    use hoard_index::store::Store;

    use super::*;
    use crate::config::HoardConfig;

    /// The projection rules are about paths the node never has to hold, so `data_dir` here
    /// names a directory that does not exist; the catalog beside the state gets a real one.
    fn state_at(data_dir: &Path, roots: Vec<PathBuf>) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open_in_memory().expect("store opens");
        let mut config = HoardConfig::default();
        config.server.data_dir = data_dir.to_path_buf();
        config.server.projection_roots = roots;
        let catalog = crate::testing::empty_catalog(dir.path());
        let state = AppState::new(config, store, catalog).expect("state builds");
        (dir, state)
    }

    #[test]
    fn every_role_lands_somewhere_in_every_profile() {
        for profile in Profile::ALL {
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
                assert!(!profile.directory(role).is_empty());
            }
        }
        assert_eq!(Profile::ComfyUi.directory(Role::Lora), "loras");
        assert_eq!(Profile::A1111.directory(Role::Ckpt), "Stable-diffusion");
        assert_eq!(Profile::Llama.directory(Role::Llm), "models");
    }

    #[test]
    fn an_unknown_profile_is_refused_rather_than_treated_as_comfyui() {
        assert_eq!(
            Profile::parse(" ComfyUI ").expect("known"),
            Profile::ComfyUi
        );
        assert_eq!(Profile::parse("A1111").expect("known"), Profile::A1111);
        let error = Profile::parse("webui").expect_err("webui is not a layout");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_projection_never_escapes_the_directories_this_node_offers() {
        let (_dir, state) = state_at(Path::new("/srv/hoard"), Vec::new());
        assert_eq!(
            projection_dir(&state, Profile::ComfyUi, None).expect("the default is the root"),
            PathBuf::from("/srv/hoard/profiles/comfyui")
        );
        assert_eq!(
            projection_dir(&state, Profile::A1111, Some("bench")).expect("a relative dir"),
            PathBuf::from("/srv/hoard/profiles/a1111/bench")
        );
        assert_eq!(
            projection_dir(&state, Profile::ComfyUi, Some("/srv/hoard/profiles/here"))
                .expect("an absolute dir inside the root"),
            PathBuf::from("/srv/hoard/profiles/here")
        );
        for escape in ["/etc", "../../etc", "/srv/hoard/profiles/../../etc"] {
            let refused = projection_dir(&state, Profile::ComfyUi, Some(escape));
            assert!(refused.is_err(), "{escape} must be refused");
            assert_eq!(
                refused.err().map(|error| error.status()),
                Some(axum::http::StatusCode::BAD_REQUEST)
            );
        }
    }

    #[test]
    fn a_configured_root_is_the_only_way_out_of_the_data_directory() {
        let (_dir, state) = state_at(
            Path::new("/srv/hoard"),
            vec![PathBuf::from("/home/hoarder/ComfyUI/models")],
        );
        assert_eq!(
            projection_dir(
                &state,
                Profile::ComfyUi,
                Some("/home/hoarder/ComfyUI/models")
            )
            .expect("a configured root"),
            PathBuf::from("/home/hoarder/ComfyUI/models")
        );
        assert!(
            projection_dir(&state, Profile::ComfyUi, Some("/home/hoarder/.ssh")).is_err(),
            "a sibling of a configured root is not itself a root"
        );
    }
}
