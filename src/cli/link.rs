use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::node::Context;

pub async fn run(
    context: Context,
    dir: std::path::PathBuf,
    profile: String,
    query: Option<String>,
    shelf: Option<String>,
) -> Result<()> {
    if query.is_none() && shelf.is_none() {
        anyhow::bail!("give --query or --shelf to say what to project");
    }
    let Context { config, .. } = context;
    let base = config.server.resolved_public_url();

    let mut body = json!({ "profile": profile, "dir": dir });
    if let Some(query) = query {
        body["query"] = Value::String(query);
    }
    if let Some(shelf) = shelf {
        body["shelf"] = Value::String(shelf);
    }

    let response = post(&base, "/api/v1/link", &body)
        .await
        .with_context(|| format!("projecting into {}", dir.display()))?;
    tracing::info!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

/// POSTs JSON to a node's `/api/v1` route, authenticating with `HOARD_NODE_TOKEN` when set.
pub async fn post(base: &str, route: &str, body: &Value) -> Result<Value> {
    let url = format!("{}{route}", base.trim_end_matches('/'));
    let mut request = reqwest::Client::new().post(&url).json(body);
    if let Ok(token) = std::env::var("HOARD_NODE_TOKEN")
        && !token.is_empty()
    {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("posting to {url}; is `hoard serve` running on this node?"))?;
    let status = response.status();
    let value: Value = response.json().await.context("reading the response")?;
    if !status.is_success() {
        anyhow::bail!("{status}: {value}");
    }
    Ok(value)
}
