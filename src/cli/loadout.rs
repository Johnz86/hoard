use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Subcommand;

use hoard_core::{Loadout, LoadoutPin};
use hoard_index::catalog::Catalog;

use super::node::Context;

#[derive(Debug, Subcommand)]
pub enum LoadoutCommand {
    /// Print a loadout, or list the ones this node knows.
    Show { name: Option<String> },
    /// Report whether each pinned line is present here, on a peer, or missing.
    Resolve { name: String },
    /// Write a loadout out as TOML.
    Export { name: String },
    /// Read a TOML document into this node's index, replacing a loadout of the same name.
    Import { file: PathBuf },
    /// Check the compatibility rules without changing anything.
    Validate { file: PathBuf },
}

pub async fn run(context: Context, command: LoadoutCommand) -> Result<()> {
    let Context { catalog, .. } = context;
    match command {
        LoadoutCommand::Show { name } => show(&catalog, name).await,
        LoadoutCommand::Resolve { name } => resolve(&catalog, &name).await,
        LoadoutCommand::Export { name } => export(&catalog, &name).await,
        LoadoutCommand::Import { file } => import(&catalog, &file).await,
        LoadoutCommand::Validate { file } => validate(&file),
    }
}

/// Runs one catalog read or write off the reactor, since redb is file work.
async fn on_catalog<T, F>(catalog: &Arc<Catalog>, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Catalog) -> hoard_index::Result<T> + Send + 'static,
{
    let catalog = Arc::clone(catalog);
    Ok(tokio::task::spawn_blocking(move || work(&catalog))
        .await
        .context("the catalog task was cancelled")??)
}

async fn show(catalog: &Arc<Catalog>, name: Option<String>) -> Result<()> {
    match name {
        Some(name) => {
            let loadout = read(catalog, &name).await?;
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
            let records = on_catalog(catalog, Catalog::list_loadouts)
                .await
                .context("reading loadouts")?;
            let mut out = String::new();
            for record in &records {
                out.push_str(&format!("{}\n", record.name));
            }
            if records.is_empty() {
                out.push_str("no loadouts on this node\n");
            }
            tracing::info!("{out}");
        }
    }
    Ok(())
}

async fn resolve(catalog: &Arc<Catalog>, name: &str) -> Result<()> {
    let loadout = read(catalog, name).await?;
    let mut out = String::new();
    for (slot, pin) in loadout.pins() {
        let status = locate(catalog, pin).await?;
        out.push_str(&format!(
            "  {:<11} {:<40} {status}\n",
            slot.as_str(),
            pin.display_name()
        ));
    }
    tracing::info!("{out}");
    Ok(())
}

async fn locate(catalog: &Arc<Catalog>, pin: &LoadoutPin) -> Result<&'static str> {
    let wanted = pin.clone();
    let rows = on_catalog(catalog, move |catalog| catalog.resolve_pin(&wanted))
        .await
        .context("resolving the pin")?;
    Ok(match rows.first() {
        None => "missing",
        Some(_) if rows.iter().any(|row| row.abs_path.is_some()) => "present",
        Some(_) => "elsewhere",
    })
}

async fn export(catalog: &Arc<Catalog>, name: &str) -> Result<()> {
    let loadout = read(catalog, name).await?;
    tracing::info!("{}", loadout.to_toml().context("rendering the loadout")?);
    Ok(())
}

async fn import(catalog: &Arc<Catalog>, file: &PathBuf) -> Result<()> {
    let loadout = parse(file)?;
    let stored = loadout.clone();
    on_catalog(catalog, move |catalog| catalog.upsert_loadout(&stored))
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

async fn read(catalog: &Arc<Catalog>, name: &str) -> Result<Loadout> {
    let wanted = name.to_owned();
    let record = on_catalog(catalog, move |catalog| catalog.loadout_by_name(&wanted))
        .await
        .context("reading the loadout")?
        .ok_or_else(|| anyhow::anyhow!("no loadout named {name} on this node"))?;
    record.parse().context("parsing the stored loadout")
}
