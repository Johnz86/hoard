use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::format_size;
use hoard_index::IndexError;
use hoard_index::catalog::{QueryPage, QueryRequest, Totals};
use hoard_index::search::SortSpec;
use hoard_index::store::row::FacetBucket;

use crate::api::common::{authorize, blocking};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const DENSITY_KEY: &str = "density";
const ROLE_COLOR_KEY: &str = "role_color_mode";
const INSPECTOR_KEY: &str = "show_inspector";
const THEME_KEY: &str = "theme";

/// The row heights the design names, in the order the settings sheet offers them.
const DENSITIES: [&str; 2] = ["Condensed", "Roomy"];
/// Whether the role column is coloured, as the design names it.
const ROLE_COLORS: [&str; 2] = ["Color-coded", "Monochrome"];
/// The themes the shell can be in; `system` follows the operating system.
const THEMES: [&str; 3] = ["system", "dark", "light"];

const DEFAULT_DENSITY: &str = DENSITIES[0];
const DEFAULT_ROLE_COLOR: &str = ROLE_COLORS[0];
const DEFAULT_THEME: &str = THEMES[0];

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

    /// Rejects a value the shell has no spelling for, so a setting round trips as one of
    /// the choices the design names rather than as whatever text was posted.
    fn validated(self) -> ApiResult<Self> {
        for (key, value, allowed) in [
            (DENSITY_KEY, self.density.as_str(), DENSITIES.as_slice()),
            (
                ROLE_COLOR_KEY,
                self.role_color_mode.as_str(),
                ROLE_COLORS.as_slice(),
            ),
            (THEME_KEY, self.theme.as_str(), THEMES.as_slice()),
        ] {
            if !allowed.contains(&value) {
                return Err(ApiError::bad_request(format!(
                    "{value:?} is not a {key}; it is one of {}",
                    allowed.join(", ")
                )));
            }
        }
        Ok(self)
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

/// Answers what this node can do rather than failing.
///
/// The peer count is the index probe: an index that cannot answer it is reported as
/// degraded, because a health route that returns an error tells a watchdog nothing about
/// which half of the node is down.
pub async fn healthz(State(state): State<AppState>) -> ApiResult<Json<HealthView>> {
    let catalog = Arc::clone(state.catalog());
    let probe = tokio::task::spawn_blocking(move || {
        Ok(catalog
            .list_nodes()?
            .into_iter()
            .filter(|row| !row.is_self)
            .count() as u64)
    })
    .await
    .unwrap_or_else(|error| Err(IndexError::Catalog(error.to_string())));
    let (status, index, peers) = match probe {
        Ok(peers) => ("ok", "ok", peers),
        Err(error) => {
            tracing::error!(%error, "the index did not answer the health probe");
            ("degraded", "error", 0)
        }
    };
    Ok(Json(HealthView {
        status,
        index,
        upstream: state.upstream_health().as_str(),
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
    let (totals, page) = catalog_totals(&state).await?;
    Ok(Json(StatsView {
        artifacts: totals.artifacts,
        blobs: totals.present_artifacts,
        repos: totals.repositories,
        triage: totals.triage_artifacts,
        indexed_bytes: totals.indexed_bytes,
        indexed_bytes_human: format_size(totals.indexed_bytes),
        roles: page.facets.role,
        formats: page.facets.format,
        nodes: page.facets.node,
        cache: CacheView {
            bytes,
            bytes_human: format_size(bytes),
            limit,
            limit_human: limit.map(format_size),
        },
    }))
}

/// The counts the sweep measured and the unnarrowed facets beside them, both read from the
/// native catalog on a blocking thread.
async fn catalog_totals(state: &AppState) -> Result<(Totals, QueryPage), IndexError> {
    let catalog = Arc::clone(state.catalog());
    let now = Timestamp::now();
    tokio::task::spawn_blocking(move || {
        let totals = catalog.totals()?;
        let context = catalog.compile_context("", now, Vec::new())?;
        let page = catalog.query(QueryRequest {
            raw: "",
            sort: SortSpec::default(),
            limit: 1,
            offset: 0,
            context: &context,
        })?;
        Ok((totals, page))
    })
    .await?
}

pub async fn settings(State(state): State<AppState>) -> ApiResult<Json<SettingsView>> {
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || Ok(SettingsView::from_stored(&catalog.all_settings()?))).await?;
    Ok(Json(view))
}

pub async fn put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SettingsView>,
) -> ApiResult<Json<SettingsView>> {
    authorize(&state, &headers)?;
    let body = body.validated()?;
    let catalog = Arc::clone(state.catalog());
    let view = blocking(move || {
        for (key, value) in body.pairs() {
            catalog.put_setting(key, &value)?;
        }
        Ok(SettingsView::from_stored(&catalog.all_settings()?))
    })
    .await?;
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_the_shell_cannot_spell_is_refused() {
        let stored = SettingsView::default();
        assert_eq!(
            stored.clone().validated().expect("defaults are valid"),
            stored
        );
        for broken in [
            SettingsView {
                density: "Tiny".to_owned(),
                ..SettingsView::default()
            },
            SettingsView {
                role_color_mode: "Rainbow".to_owned(),
                ..SettingsView::default()
            },
            SettingsView {
                theme: "<script>".to_owned(),
                ..SettingsView::default()
            },
        ] {
            let error = broken.validated().expect_err("the value is not a choice");
            assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        }
    }
}
