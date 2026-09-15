use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};

use hoard_core::NodeId;
use hoard_index::IndexError;
use hoard_index::catalog::Catalog;
use hoard_server::HoardConfig;

const DEFAULT_CONFIG: &str = "hoard.toml";

/// The configuration and the catalog it points at, which every subcommand needs.
pub struct Context {
    pub config: HoardConfig,
    pub catalog: Arc<Catalog>,
}

/// Why a catalog could not be opened for inspection, in the terms `hoard doctor` reports.
#[derive(Debug)]
pub enum Finding {
    Missing(PathBuf),
    Format {
        directory: PathBuf,
        found: Option<u32>,
        expected: u32,
    },
    Locked(String),
    Unreadable(String),
}

impl std::fmt::Display for Finding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::Missing(directory) => write!(
                formatter,
                "no catalog at {}; run `hoard scan`",
                directory.display()
            ),
            Finding::Format {
                directory,
                found: Some(found),
                expected,
            } => write!(
                formatter,
                "catalog at {} is format {found}, this build reads {expected}; \
                 `hoard scan` rebuilds it",
                directory.display()
            ),
            Finding::Format {
                directory,
                found: None,
                expected,
            } => write!(
                formatter,
                "catalog at {} predates format stamps, this build reads {expected}; \
                 `hoard scan` rebuilds it",
                directory.display()
            ),
            Finding::Locked(message) | Finding::Unreadable(message) => formatter.write_str(message),
        }
    }
}

/// The configuration and, when one exists in this build's format, the catalog, opened
/// without creating, scanning, refreshing or writing anything.
#[derive(Debug)]
pub struct Inspection {
    pub config: HoardConfig,
    pub catalog: Result<Catalog, Finding>,
}

/// Loads `hoard.toml` and opens the catalog it names for reading only: no directory is
/// created, no source is scanned, no search index is built and no catalog row is written.
/// Every reason a catalog cannot be opened comes back as a [`Finding`] rather than an error,
/// so a diagnosis can report it beside the checks that did run.
pub async fn inspect(config_path: Option<&Path>) -> Result<Inspection> {
    let config = load_config(config_path)?;
    let directory = config.index.native_path();
    if !directory.join("catalog.redb").exists() {
        let catalog = Err(Finding::Missing(directory));
        return Ok(Inspection { config, catalog });
    }
    let opening = directory.clone();
    let catalog = tokio::task::spawn_blocking(move || Catalog::inspect(&opening))
        .await
        .with_context(|| format!("inspecting the native catalog at {}", directory.display()))?
        .map_err(finding_of);
    Ok(Inspection { config, catalog })
}

fn finding_of(error: IndexError) -> Finding {
    let message = error.to_string();
    match error {
        IndexError::CatalogFormat {
            directory,
            found,
            expected,
        } => Finding::Format {
            directory,
            found,
            expected,
        },
        IndexError::CatalogBusy => Finding::Locked(message),
        _ => Finding::Unreadable(message),
    }
}

/// Loads `hoard.toml`, opens the catalog and makes sure this node exists in it under the
/// name the configuration chose. A data directory without a catalog gets one scanned into
/// it, which is why this reports its progress.
pub async fn open(config_path: Option<&Path>) -> Result<Context> {
    let config = load_config(config_path)?;
    std::fs::create_dir_all(&config.index.data_dir).with_context(|| {
        format!(
            "creating the data directory {}",
            config.index.data_dir.display()
        )
    })?;
    let catalog = open_catalog(&config).await?;
    ensure_local_node(&catalog, &config.node.name)?;
    Ok(Context { config, catalog })
}

async fn open_catalog(config: &HoardConfig) -> Result<Arc<Catalog>> {
    let index = config.index.clone();
    let node = config.node.name.clone();
    tokio::task::spawn_blocking(move || {
        Catalog::open_or_scan(&index, &node, |phase, count| {
            tracing::info!(phase, count, "building the native catalog");
        })
    })
    .await
    .context("opening the native catalog")?
    .map(Arc::new)
    .with_context(|| {
        format!(
            "opening the native catalog at {}",
            config.index.native_path().display()
        )
    })
}

pub fn load_config(config_path: Option<&Path>) -> Result<HoardConfig> {
    match config_path {
        Some(path) => HoardConfig::load(path)
            .map_err(|error| anyhow::anyhow!("{}: {}", path.display(), error_text(&error))),
        None => {
            let default = PathBuf::from(DEFAULT_CONFIG);
            if default.exists() {
                HoardConfig::load(&default)
                    .map_err(|error| anyhow::anyhow!("{DEFAULT_CONFIG}: {}", error_text(&error)))
            } else {
                let mut config = HoardConfig::default();
                config.normalize(std::env::var_os("HOME").map(PathBuf::from).as_deref());
                Ok(config)
            }
        }
    }
}

fn error_text(error: &hoard_server::ApiError) -> String {
    error.code().to_owned()
}

/// Records this machine under the configured name.
///
/// The identity is derived from the machine, not from the name, so renaming a node in
/// `hoard.toml` renames the existing rows rather than creating a second node that appears
/// to hold the same bytes twice.
pub fn ensure_local_node(catalog: &Catalog, name: &str) -> Result<i64> {
    let id = NodeId::new(machine_identity()).context("deriving this node's identity")?;
    let node = catalog
        .ensure_local_node(&id, name)
        .context("recording this node in the catalog")?;
    Ok(node.id)
}

fn machine_identity() -> String {
    let raw = std::fs::read_to_string("/var/lib/dbus/machine-id")
        .or_else(|_| std::fs::read_to_string("/etc/machine-id"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "hoard".to_owned());
    fold(&raw)
}

fn fold(name: &str) -> String {
    let folded: String = name
        .chars()
        .map(|letter| {
            if letter.is_ascii_alphanumeric() {
                letter
            } else {
                '-'
            }
        })
        .collect();
    if folded.is_empty() {
        "hoard".to_owned()
    } else {
        folded
    }
}

pub async fn list(context: Context) -> Result<()> {
    let Context { catalog, .. } = context;
    let nodes = tokio::task::spawn_blocking(move || catalog.list_nodes())
        .await
        .context("reading nodes")?
        .context("reading nodes")?;
    let mut out = String::new();
    out.push_str(&format!(
        "{:<16} {:<8} {:<8} {:>12}  {}\n",
        "NAME", "SELF", "ONLINE", "INDEXED", "URL"
    ));
    for node in &nodes {
        out.push_str(&format!(
            "{:<16} {:<8} {:<8} {:>12}  {}\n",
            node.name,
            if node.is_self { "yes" } else { "-" },
            if node.online { "yes" } else { "-" },
            hoard_core::format_size(node.indexed_bytes),
            node.url.as_deref().unwrap_or("-"),
        ));
    }
    tracing::info!("\n{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_folded_to_the_accepted_alphabet() {
        assert_eq!(fold("rig-02.lan"), "rig-02-lan");
        assert_eq!(fold(""), "hoard");
        assert!(NodeId::new(fold("rig 02")).is_ok());
        assert!(NodeId::new(machine_identity()).is_ok());
    }

    #[test]
    fn renaming_a_node_does_not_create_a_second_one() {
        let directory = tempfile::tempdir().expect("temp dir");
        let config = hoard_index::config::IndexConfig {
            data_dir: directory.path().join("data"),
            ..hoard_index::config::IndexConfig::default()
        };
        let catalog =
            Catalog::open_or_scan(&config, "workstation", |_, _| {}).expect("catalog opens");
        ensure_local_node(&catalog, "workstation").expect("first run records the node");
        ensure_local_node(&catalog, "rig-02").expect("a rename updates the same row");
        let nodes = catalog.list_nodes().expect("nodes read");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "rig-02");
        assert!(nodes[0].is_self);
    }
}
