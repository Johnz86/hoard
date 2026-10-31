use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;
use url::{Host, Url};

use crate::api::common::authorize;
use crate::api::project_search;
use crate::config::NodeSettings;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressState {
    LocalOnly,
    Unusable,
    Configured,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SetupView {
    pub first_run: bool,
    pub configured_sources: usize,
    pub resource_count: u64,
    pub scanning: bool,
    pub node_name: String,
    pub bind: String,
    pub public_url: String,
    pub address_state: AddressState,
    pub address_message: &'static str,
    pub write_token_required: bool,
    pub access_message: &'static str,
    pub pending: Option<NodeSettings>,
    pub restart_required: bool,
}

pub async fn get(State(state): State<AppState>) -> ApiResult<Json<SetupView>> {
    let config = state.config().clone();
    let catalog = Arc::clone(state.catalog());
    let read = async move {
        tokio::task::spawn_blocking(move || -> ApiResult<_> {
            Ok((catalog.totals()?.artifacts, config.managed_node_settings()?))
        })
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?
    };
    let ((artifacts, saved), projects) = tokio::try_join!(
        read,
        project_search::find(&state, String::new(), None, 0, 0)
    )?;
    let resource_count = artifacts.saturating_add(projects.total);
    let configured_sources = state.coordinator().config().sources.len();
    let public_url = state.public_url();
    let pending = saved.filter(|settings| settings.node_name != state.node_name());
    let address_state = address_state(&state, &public_url);
    let address_message = match address_state {
        AddressState::LocalOnly => {
            "The current address works on this machine. Set a reachable address and a LAN listener to share with other machines."
        }
        AddressState::Unusable => {
            "The advertised address is not usable. Set an HTTP or HTTPS address other machines can reach."
        }
        AddressState::Configured => {
            "A sharing address is configured. Open it from another machine to check network access."
        }
    };
    let write_token_required = state.write_token_required();
    Ok(Json(SetupView {
        first_run: configured_sources == 0 && resource_count == 0,
        configured_sources,
        resource_count,
        scanning: state.coordinator().busy(),
        node_name: state.node_name().to_owned(),
        bind: state.config().server.bind.to_string(),
        public_url: public_url.as_str().to_owned(),
        address_state,
        address_message,
        write_token_required,
        access_message: if write_token_required {
            "Reading and downloading are public to anyone who can reach this node. Changes require the owner's node token."
        } else {
            "Reading, downloading and changes are available to anyone who can reach this node. Set HOARD_NODE_TOKEN in the server environment to protect changes."
        },
        restart_required: pending.is_some(),
        pending,
    }))
}

pub async fn put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NodeSettings>,
) -> ApiResult<Json<SetupView>> {
    authorize(&state, &headers)?;
    state.save_node_settings(body.validated()?).await?;
    get(State(state)).await
}

fn address_state(state: &AppState, public_url: &str) -> AddressState {
    let Ok(url) = Url::parse(public_url) else {
        return AddressState::Unusable;
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return AddressState::Unusable;
    }
    let local = match url.host() {
        Some(Host::Ipv4(host)) => {
            if host.is_unspecified() {
                return AddressState::Unusable;
            }
            host.is_loopback()
        }
        Some(Host::Ipv6(host)) => {
            if host.is_unspecified() {
                return AddressState::Unusable;
            }
            host.is_loopback()
        }
        Some(Host::Domain(host)) => {
            if host == "*" {
                return AddressState::Unusable;
            }
            host == "localhost" || host.ends_with(".localhost")
        }
        None => return AddressState::Unusable,
    };
    if local || state.config().server.bind.ip().is_loopback() {
        AddressState::LocalOnly
    } else {
        AddressState::Configured
    }
}
