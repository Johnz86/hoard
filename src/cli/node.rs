use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};

use hoard_core::NodeId;
use hoard_index::catalog::Catalog;
use hoard_index::store::Store;
use hoard_index::store::node::{NewNode, local_node, upsert_node};
use hoard_server::HoardConfig;

const DEFAULT_CONFIG: &str = "hoard.toml";

/// The configuration, the open index and the native catalog beside it, which every
/// subcommand needs.
pub struct Context {
    pub config: HoardConfig,
    pub store: Store,
    pub catalog: Arc<Catalog>,
}

/// Loads `hoard.toml`, opens the index and the native catalog beside it, and makes sure this
/// node exists in the index under the name the configuration chose. A data directory without
/// a native catalog gets one scanned into it, which is why this reports its progress.
pub async fn open(config_path: Option<&Path>) -> Result<Context> {
    let config = load_config(config_path)?;
    let database = config.index.database_path();
    if let Some(parent) = database.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating the data directory {}", parent.display()))?;
    }
    let store = Store::open(&database)
        .with_context(|| format!("opening the index at {}", database.display()))?;
    ensure_local_node(&store, &config.node.name).await?;
    let catalog = open_catalog(&config).await?;
    Ok(Context {
        config,
        store,
        catalog,
    })
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
pub async fn ensure_local_node(store: &Store, name: &str) -> Result<i64> {
    let id = NodeId::new(machine_identity()).context("deriving this node's identity")?;
    let name = name.to_owned();
    let row = store
        .call_mut(move |conn| upsert_node(conn, &NewNode::local(id, name)))
        .await
        .context("recording this node in the index")?;
    Ok(row)
}

pub async fn local_node_name(store: &Store) -> Result<String> {
    let row = store.call(local_node).await.context("reading this node")?;
    Ok(row.name)
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
    let Context { store, .. } = context;
    let nodes = store
        .call(hoard_index::store::node::list_nodes)
        .await
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

    #[tokio::test]
    async fn renaming_a_node_does_not_create_a_second_one() {
        let store = Store::open_in_memory().expect("store opens");
        ensure_local_node(&store, "workstation")
            .await
            .expect("first run records the node");
        ensure_local_node(&store, "rig-02")
            .await
            .expect("a rename updates the same row");
        let nodes = store
            .call(hoard_index::store::node::list_nodes)
            .await
            .expect("nodes read");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "rig-02");
        assert!(nodes[0].is_self);
    }
}
