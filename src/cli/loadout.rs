//! `hoard loadout`: read, validate and export the portable pinned document.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Subcommand;

use hoard_core::{Loadout, LoadoutPin};
use hoard_index::store::Store;
use hoard_index::store::loadout::{list_loadouts, loadout_by_name, upsert_loadout};

use super::node::Context;

#[derive(Debug, Subcommand)]
pub enum LoadoutCommand {
    /// Print a loadout, or list the ones this node knows.
    Show { name: Option<String> },
    /// Report whether each pinned line is present here, on a peer, or missing.
    Resolve { name: String },
    /// Write a loadout out as TOML.
    Export { name: String },
    /// Read a TOML document into this node's index.
    Import { file: PathBuf },
    /// Check the compatibility rules without changing anything.
    Validate { file: PathBuf },
}

pub async fn run(context: Context, command: LoadoutCommand) -> Result<()> {
    let Context { store, .. } = context;
    match command {
        LoadoutCommand::Show { name } => show(&store, name).await,
        LoadoutCommand::Resolve { name } => resolve(&store, &name).await,
        LoadoutCommand::Export { name } => export(&store, &name).await,
        LoadoutCommand::Import { file } => import(&store, &file).await,
        LoadoutCommand::Validate { file } => validate(&file),
    }
}

async fn show(store: &Store, name: Option<String>) -> Result<()> {
    match name {
        Some(name) => {
            let loadout = read(store, &name).await?;
            let mut out = format!("{}\n", loadout.name);
            for (slot, pin) in loadout.pins() {
                out.push_str(&format!(
                    "  {:<11} {:<40} {}\n",
                    slot.as_str(),
                    pin.display_name(),
                    pin.weight_label()
                ));
            }
            tracing::info!("{out}");
        }
        None => {
            let rows = store
                .call(list_loadouts)
                .await
                .context("reading loadouts")?;
            let mut out = String::new();
            for row in &rows {
                out.push_str(&format!("{}\n", row.name));
            }
            if rows.is_empty() {
                out.push_str("no loadouts on this node\n");
            }
            tracing::info!("{out}");
        }
    }
    Ok(())
}

async fn resolve(store: &Store, name: &str) -> Result<()> {
    let loadout = read(store, name).await?;
    let mut out = String::new();
    for (slot, pin) in loadout.pins() {
        let status = locate(store, pin).await?;
        out.push_str(&format!(
            "  {:<11} {:<40} {status}\n",
            slot.as_str(),
            pin.display_name()
        ));
    }
    tracing::info!("{out}");
    Ok(())
}

/// A pin resolves by content hash first and by repo plus commit plus path second, which is
/// what lets the same document resolve on a node that stores the bytes somewhere else.
async fn locate(store: &Store, pin: &LoadoutPin) -> Result<&'static str> {
    let Some(sha) = pin.sha256 else {
        return Ok("missing");
    };
    let blob = store
        .call(move |conn| hoard_index::store::blob::blob_by_sha256(conn, &sha))
        .await
        .context("reading the blob")?;
    let Some(blob) = blob else {
        return Ok("missing");
    };
    let entries = store
        .call(move |conn| hoard_index::store::entry::entries_for_blob(conn, blob.id))
        .await
        .context("reading entries")?;
    Ok(if entries.iter().any(|entry| entry.abs_path.is_some()) {
        "present"
    } else {
        "elsewhere"
    })
}

async fn export(store: &Store, name: &str) -> Result<()> {
    let loadout = read(store, name).await?;
    tracing::info!("{}", loadout.to_toml().context("rendering the loadout")?);
    Ok(())
}

async fn import(store: &Store, file: &PathBuf) -> Result<()> {
    let loadout = parse(file)?;
    let stored = loadout.clone();
    store
        .call_mut(move |conn| upsert_loadout(conn, &stored))
        .await
        .context("storing the loadout")?;
    tracing::info!(name = %loadout.name, pins = loadout.len(), "loadout imported");
    Ok(())
}

fn validate(file: &PathBuf) -> Result<()> {
    let loadout = parse(file)?;
    let violations = loadout.violations();
    if violations.is_empty() {
        tracing::info!(name = %loadout.name, pins = loadout.len(), "loadout is valid");
        return Ok(());
    }
    for violation in &violations {
        tracing::error!(path = %violation.path, reason = %violation.reason, "incompatible pin");
    }
    anyhow::bail!("{} incompatible pins", violations.len())
}

fn parse(file: &PathBuf) -> Result<Loadout> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    Loadout::parse(&text).with_context(|| format!("parsing {}", file.display()))
}

async fn read(store: &Store, name: &str) -> Result<Loadout> {
    let owned = name.to_owned();
    let row = store
        .call(move |conn| loadout_by_name(conn, &owned))
        .await
        .context("reading the loadout")?
        .ok_or_else(|| anyhow::anyhow!("no loadout named {name} on this node"))?;
    row.parse().context("parsing the stored loadout")
}
