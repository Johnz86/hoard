use anyhow::{Context as _, Result};
use serde_json::json;

use super::link::post;
use super::node::Context;

pub async fn run(
    context: Context,
    loadout: String,
    node: Option<String>,
    profile: String,
) -> Result<()> {
    let Context { config, .. } = context;
    let target = node.unwrap_or_else(|| config.node.name.clone());
    let base = config.server.resolved_public_url();

    let body = json!({ "node": target, "profile": profile });
    let response = post(&base, &format!("/api/v1/loadouts/{loadout}/apply"), &body)
        .await
        .with_context(|| format!("applying {loadout} on {target}"))?;

    tracing::info!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
