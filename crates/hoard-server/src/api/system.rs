use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::store::row::FacetBucket;
use hoard_index::store::{artifact, node, repo, search_exec, setting, source};

use crate::api::common::{PROBE_LIMIT, authorize};
use crate::error::ApiResult;
use crate::state::AppState;

const DENSITY_KEY: &str = "density";
const ROLE_COLOR_KEY: &str = "role_color_mode";
const INSPECTOR_KEY: &str = "show_inspector";
const THEME_KEY: &str = "theme";

const DEFAULT_DENSITY: &str = "Condensed";
const DEFAULT_ROLE_COLOR: &str = "Color-coded";
const DEFAULT_THEME: &str = "system";

/// What `GET /healthz` answers. `upstream` is `offline` when this node is configured never
/// to leave the LAN.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthView {
    pub status: &'static str,
    pub index: &'static str,
    pub upstream: &'static str,
    pub peers: u64,
}

/// The totals the SCANS and NODES screens print above their tables.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsView {
    pub artifacts: u64,
    pub blobs: u64,
    pub repos: u64,
    pub triage: u64,
    pub indexed_bytes: u64,
    pub indexed_bytes_human: String,
    pub roles: Vec<FacetBucket>,
    pub formats: Vec<FacetBucket>,
    pub nodes: Vec<FacetBucket>,
    pub cache: CacheView,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheView {
    pub bytes: u64,
    pub bytes_human: String,
    pub limit: Option<u64>,
    pub limit_human: Option<String>,
}

/// The presentation choices the UI mirrors into `localStorage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub density: String,
    pub role_color_mode: String,
    pub show_inspector: bool,
    pub theme: String,
}

impl Default for SettingsView {
    fn default() -> Self {
        Self {
            density: DEFAULT_DENSITY.to_owned(),
            role_color_mode: DEFAULT_ROLE_COLOR.to_owned(),
            show_inspector: true,
            theme: DEFAULT_THEME.to_owned(),
        }
    }
}

impl SettingsView {
    fn from_stored(stored: &BTreeMap<String, String>) -> Self {
        let defaults = SettingsView::default();
        Self {
            density: stored.get(DENSITY_KEY).cloned().unwrap_or(defaults.density),
            role_color_mode: stored
                .get(ROLE_COLOR_KEY)
                .cloned()
                .unwrap_or(defaults.role_color_mode),
            show_inspector: stored
                .get(INSPECTOR_KEY)
                .map(|value| value != "false")
                .unwrap_or(defaults.show_inspector),
            theme: stored.get(THEME_KEY).cloned().unwrap_or(defaults.theme),
        }
    }

    fn pairs(&self) -> [(&'static str, String); 4] {
        [
            (DENSITY_KEY, self.density.clone()),
            (ROLE_COLOR_KEY, self.role_color_mode.clone()),
            (INSPECTOR_KEY, self.show_inspector.to_string()),
            (THEME_KEY, self.theme.clone()),
        ]
    }
}

pub async fn healthz(State(state): State<AppState>) -> ApiResult<Json<HealthView>> {
    let peers = state
        .store()
        .call(|conn| {
            Ok(node::list_nodes(conn)?
                .into_iter()
                .filter(|row| !row.is_self)
                .count() as u64)
        })
        .await?;
    Ok(Json(HealthView {
        status: "ok",
        index: "ok",
        upstream: match state.offline() {
            true => "offline",
            false => "ok",
        },
        peers,
    }))
}

pub async fn stats(State(state): State<AppState>) -> ApiResult<Json<StatsView>> {
    let root = state.cache().root().to_path_buf();
    let limit = state.mirror_config().cache_size_limit;
    let bytes = tokio::task::spawn_blocking(move || {
        hoard_mirror::cache::policy::collect(&root)
            .map(|units| units.iter().map(|unit| unit.size).sum::<u64>())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let counts = state
        .store()
        .call(move |conn| {
            let sources = source::list_sources(conn)?;
            let indexed_bytes = sources.iter().map(|record| record.byte_count).sum();
            let repos = repo::list_repos(conn, &repo::RepoFilter::default(), None, u32::MAX)?;
            Ok(StatsView {
                artifacts: search_exec::count_rows(conn, "", &[])?,
                blobs: sources.iter().map(|record| record.blob_count).sum(),
                repos: repos.len() as u64,
                triage: artifact::triage_queue(conn, PROBE_LIMIT)?.len() as u64,
                indexed_bytes,
                indexed_bytes_human: format_size(indexed_bytes),
                roles: search_exec::facet_counts(conn, "role", "", &[])?,
                formats: search_exec::facet_counts(conn, "format", "", &[])?,
                nodes: search_exec::facet_counts(conn, "node", "", &[])?,
                cache: CacheView {
                    bytes,
                    bytes_human: format_size(bytes),
                    limit,
                    limit_human: limit.map(format_size),
                },
            })
        })
        .await?;
    Ok(Json(counts))
}

pub async fn settings(State(state): State<AppState>) -> ApiResult<Json<SettingsView>> {
    let view = state
        .store()
        .call(|conn| Ok(SettingsView::from_stored(&setting::all_settings(conn)?)))
        .await?;
    Ok(Json(view))
}

pub async fn put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SettingsView>,
) -> ApiResult<Json<SettingsView>> {
    authorize(&state, &headers)?;
    let view = state
        .store()
        .call_mut(move |conn| {
            for (key, value) in body.pairs() {
                setting::put_setting(conn, key, &value)?;
            }
            Ok(SettingsView::from_stored(&setting::all_settings(conn)?))
        })
        .await?;
    Ok(Json(view))
}
