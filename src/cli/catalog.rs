use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use indicatif::ProgressBar;

use hoard_index::catalog::{Catalog, SearchRequest};

#[derive(Debug, Subcommand)]
pub enum CatalogCommand {
    /// Scan configured sources into a fresh native_db catalog and Tantivy index.
    Scan {
        /// A new output directory outside every scanned source.
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        source: Option<String>,
    },
    /// Search a native catalog using Tantivy syntax, not the legacy Hoard grammar.
    Search {
        #[arg(long)]
        directory: PathBuf,
        /// Bare words are ANDed. Example: role:lora AND size:[0 TO 400000000].
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Rebuild Tantivy entirely from the native catalog, without reading model files.
    Rebuild {
        #[arg(long)]
        directory: PathBuf,
    },
    /// Report native catalog counts and check redb integrity.
    Status {
        #[arg(long)]
        directory: PathBuf,
    },
}

pub async fn run(config_path: Option<&Path>, command: CatalogCommand) -> Result<()> {
    let config_path = config_path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || run_blocking(config_path.as_deref(), command))
        .await
        .context("running native catalog operation")?
}

fn run_blocking(config_path: Option<&Path>, command: CatalogCommand) -> Result<()> {
    match command {
        CatalogCommand::Scan { directory, source } => {
            let mut config = super::node::load_config(config_path)?;
            if let Some(name) = source {
                let selected = config
                    .index
                    .source(&name)
                    .cloned()
                    .with_context(|| format!("no source named {name:?}"))?;
                config.index.sources = vec![selected];
            }
            let progress = progress_bar("Scanning native catalog");
            let catalog = Catalog::scan_sources(
                &directory,
                &config.node.name,
                &config.index,
                |phase, count| {
                    progress.set_message(format!("{phase}: {count} entries"));
                },
            )
            .context("scanning into native catalog")?;
            catalog.rebuild_search(|count| {
                progress.set_message(format!("Building Tantivy index: {count} entries"));
            })?;
            progress.finish_and_clear();
            tracing::info!("{}", serde_json::to_string_pretty(&catalog.status()?)?);
        }
        CatalogCommand::Search {
            directory,
            query,
            limit,
            offset,
        } => {
            let catalog = Catalog::open(&directory)?;
            let result = catalog.search(SearchRequest {
                query: &query,
                limit,
                offset,
            })?;
            tracing::info!("{}", serde_json::to_string_pretty(&result)?);
        }
        CatalogCommand::Rebuild { directory } => {
            let catalog = Catalog::open(&directory)?;
            let progress = progress_bar("Rebuilding Tantivy index");
            catalog.rebuild_search(|count| {
                progress.set_message(format!("Rebuilding Tantivy index: {count} entries"));
            })?;
            progress.finish_and_clear();
            tracing::info!("{}", serde_json::to_string_pretty(&catalog.status()?)?);
        }
        CatalogCommand::Status { directory } => {
            let mut catalog = Catalog::open(&directory)?;
            anyhow::ensure!(
                catalog.check_integrity()?,
                "native database integrity check failed"
            );
            tracing::info!("{}", serde_json::to_string_pretty(&catalog.status()?)?);
        }
    }
    Ok(())
}

fn progress_bar(message: &'static str) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.enable_steady_tick(Duration::from_millis(100));
    progress.set_message(message);
    progress
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};

    #[test]
    fn native_search_command_keeps_the_query_and_pagination() -> Result<()> {
        let command = Cli::try_parse_from([
            "hoard",
            "catalog",
            "search",
            "--directory",
            "/tmp/catalog",
            "role:lora",
            "--limit",
            "12",
            "--offset",
            "3",
        ])?;
        assert!(matches!(command.command, Command::Catalog {
            command: CatalogCommand::Search { query, limit: 12, offset: 3, .. }
        } if query == "role:lora"));
        Ok(())
    }

    #[test]
    fn progress_bar_starts_empty() {
        let progress = progress_bar("test");
        assert_eq!(progress.position(), 0);
        progress.finish_and_clear();
    }
}
