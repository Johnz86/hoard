use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde::Serialize;
use serde_json::json;

use hoard_index::catalog::{Catalog, QueryRequest, RescanScope, StorageReport};
use hoard_index::config::HashPolicy;
use hoard_index::search::{SortKey, SortSpec};

use super::HashWhen;
use super::scan::{hashing_message, progress_bar};

#[derive(Debug, Subcommand)]
pub enum CatalogCommand {
    /// Scan configured sources into a native catalog; an existing catalog directory is
    /// rescanned in place.
    Scan {
        /// A new output directory outside every scanned source, or one already scanned.
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        source: Option<String>,
        /// When to compute sha256.
        #[arg(long, value_enum, default_value_t = HashWhen::Later)]
        hash: HashWhen,
    },
    /// Search a native catalog with Hoard's query grammar, the same one hoard ls and the
    /// search bar use.
    Search {
        #[arg(long)]
        directory: PathBuf,
        /// Tokens are ANDed, comma values ORed. Example: role:lora base:sdxl size:<400mb.
        #[arg(default_value = "")]
        query: String,
        /// One of role, name, base, format, precision, size, revision, node, state, used.
        #[arg(long, default_value = "name")]
        sort: String,
        #[arg(long)]
        desc: bool,
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
    /// Rewrite the catalog file without its free pages and report allocated bytes before
    /// and after. Needs the catalog to itself; a catalog another process holds is refused,
    /// not waited for.
    Compact {
        #[arg(long)]
        directory: PathBuf,
    },
}

/// What one compaction returned: the bytes `catalog.redb` was allocated before and after
/// being rewritten without its free pages, and the storage report of the rewritten file.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Compaction {
    before: u64,
    after: u64,
    reclaimed: u64,
    storage: StorageReport,
}

fn compact(directory: &Path) -> Result<Compaction> {
    let before = Catalog::storage(directory)
        .context("measuring the catalog before compaction")?
        .allocated_bytes;
    let mut catalog = Catalog::open(directory).context("opening native catalog")?;
    catalog.compact().context("compacting native catalog")?;
    drop(catalog);
    let storage = Catalog::storage(directory).context("measuring the catalog after compaction")?;
    Ok(Compaction {
        before,
        after: storage.allocated_bytes,
        reclaimed: before.saturating_sub(storage.allocated_bytes),
        storage,
    })
}

pub async fn run(config_path: Option<&Path>, command: CatalogCommand) -> Result<()> {
    let config_path = config_path.map(Path::to_path_buf);
    tokio::task::spawn_blocking(move || run_blocking(config_path.as_deref(), command))
        .await
        .context("running native catalog operation")?
}

fn run_blocking(config_path: Option<&Path>, command: CatalogCommand) -> Result<()> {
    match command {
        CatalogCommand::Scan {
            directory,
            source,
            hash,
        } => {
            let mut config = super::node::load_config(config_path)?;
            if let Some(name) = source.as_deref()
                && config.index.source(name).is_none()
            {
                anyhow::bail!("no source named {name} in hoard.toml");
            }
            if let Some(name) = source.as_deref() {
                config
                    .index
                    .sources
                    .retain(|candidate| candidate.name == name);
            }
            if config.index.sources.is_empty() {
                anyhow::bail!("no sources are configured; add [[index.sources]] to hoard.toml");
            }
            let scope = match source {
                Some(_) => RescanScope::Narrowed,
                None => RescanScope::Configured,
            };
            let policy = HashPolicy::from(hash);
            let progress = progress_bar();
            progress.set_message("Scanning native catalog");
            let rescanned = directory.join("catalog.redb").exists();
            let mut catalog = match rescanned {
                true => Catalog::open(&directory).context("opening native catalog")?,
                false => Catalog::scan_sources(
                    &directory,
                    &config.node.name,
                    &config.index,
                    policy,
                    |phase, count| {
                        progress.set_message(format!("{phase}: {count} entries"));
                    },
                )
                .context("scanning into native catalog")?,
            };
            let rescan = match rescanned {
                true => Some(
                    catalog
                        .rescan(&config.index, policy, scope, |phase, count| {
                            progress.set_message(format!("{phase}: {count} entries"));
                        })
                        .context("rescanning native catalog")?,
                ),
                false => None,
            };
            let hashing = match policy {
                HashPolicy::Now | HashPolicy::Background => Some(
                    catalog
                        .hash_pending(&config.index, |update| {
                            progress.set_message(hashing_message(update));
                        })
                        .context("hashing the native catalog")?,
                ),
                HashPolicy::Never => None,
            };
            match (rescan.is_some(), catalog.status()?.search_built) {
                (true, true) => {
                    catalog.refresh_search(|count| {
                        progress.set_message(format!("Refreshing Tantivy index: {count} changes"));
                    })?;
                }
                (true, false) | (false, true) | (false, false) => {
                    catalog.rebuild_search(|count| {
                        progress.set_message(format!("Building Tantivy index: {count} entries"));
                    })?;
                }
            }
            catalog.compact()?;
            progress.finish_and_clear();
            let status = catalog.status()?;
            drop(catalog);
            report(&json!({
                "rescan": rescan,
                "hashing": hashing,
                "status": status,
                "storage": Catalog::storage(&directory)?,
            }))?;
        }
        CatalogCommand::Search {
            directory,
            query,
            sort,
            desc,
            limit,
            offset,
        } => {
            let catalog = Catalog::open(&directory)?;
            let context = catalog
                .compile_context(&query, jiff::Timestamp::now(), Vec::new())
                .context("resolving the query context")?;
            let result = catalog.query(QueryRequest {
                raw: &query,
                sort: SortSpec::new(sort.parse::<SortKey>()?, desc),
                limit,
                offset,
                context: &context,
            })?;
            report(&result)?;
        }
        CatalogCommand::Rebuild { directory } => {
            let catalog = Catalog::open(&directory)?;
            let progress = progress_bar();
            progress.set_message("Rebuilding Tantivy index");
            catalog.rebuild_search(|count| {
                progress.set_message(format!("Rebuilding Tantivy index: {count} entries"));
            })?;
            progress.finish_and_clear();
            report(&catalog.status()?)?;
        }
        CatalogCommand::Compact { directory } => {
            report(&compact(&directory)?)?;
        }
        CatalogCommand::Status { directory } => {
            let mut catalog = Catalog::open(&directory)?;
            anyhow::ensure!(
                catalog.check_integrity()?,
                "native database integrity check failed"
            );
            let status = catalog.status()?;
            drop(catalog);
            report(&json!({ "status": status, "storage": Catalog::storage(&directory)? }))?;
        }
    }
    Ok(())
}

fn report<T: serde::Serialize>(value: &T) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string_pretty(value)?).context("writing results")?;
    out.flush().context("flushing results")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};

    #[test]
    fn compact_parses_its_directory() -> Result<()> {
        let parsed =
            Cli::try_parse_from(["hoard", "catalog", "compact", "--directory", "/tmp/catalog"])?;
        assert!(matches!(parsed.command, Command::Catalog {
            command: CatalogCommand::Compact { directory }
        } if directory == Path::new("/tmp/catalog")));
        Ok(())
    }

    #[test]
    fn compacting_reports_before_and_after_and_keeps_the_data() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = root.path().join("files");
        std::fs::create_dir_all(&input)?;
        for index in 0..128 {
            std::fs::write(
                input.join(format!("d{index}.jsonl")),
                format!("{{\"text\":\"row {index}\"}}\n"),
            )?;
        }
        let config = hoard_index::config::IndexConfig {
            sources: vec![hoard_index::config::SourceConfig {
                name: "files".to_owned(),
                path: input.clone(),
                kind: hoard_index::config::SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            }],
            ..hoard_index::config::IndexConfig::default()
        };
        let directory = root.path().join("native");
        let catalog =
            Catalog::scan_sources(&directory, "n", &config, HashPolicy::Never, |_, _| {})?;
        for index in 0..96 {
            std::fs::remove_file(input.join(format!("d{index}.jsonl")))?;
        }
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        let expected = catalog.count_summary()?;
        drop(catalog);

        let compaction = compact(&directory)?;
        assert!(
            compaction.after <= compaction.before,
            "a compaction never grows the file: {compaction:?}"
        );
        assert_eq!(
            compaction.reclaimed,
            compaction.before - compaction.after,
            "what it reclaimed is the difference it reported"
        );
        assert_eq!(
            compaction.storage.allocated_bytes, compaction.after,
            "the report describes the file the compaction left"
        );

        let reopened = Catalog::inspect(&directory)?;
        assert_eq!(
            reopened.count_summary()?,
            expected,
            "a compaction moves pages, not records"
        );
        Ok(())
    }

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
            command: CatalogCommand::Search { query, sort, desc: false, limit: 12, offset: 3, .. }
        } if query == "role:lora" && sort == "name"));
        Ok(())
    }
}
