use std::sync::Arc;

use anyhow::{Context as _, Result};
use indicatif::{ProgressBar, ProgressStyle};

use hoard_index::catalog::{Catalog, HashReport, RescanReport};
use hoard_index::config::{HashPolicy, IndexConfig};
use hoard_index::{IndexError, catalog};

use super::node::Context;

pub async fn run(context: Context, source: Option<String>, hash: HashPolicy) -> Result<()> {
    let Context {
        config, catalog, ..
    } = context;
    if config.index.sources.is_empty() {
        anyhow::bail!("no sources are configured; add [[index.sources]] to hoard.toml");
    }
    let index = match source.as_deref() {
        Some(name) => {
            let named = config
                .index
                .source(name)
                .ok_or_else(|| anyhow::anyhow!("no source named {name} in hoard.toml"))?;
            config.index.over(vec![named.clone()])
        }
        None => config.index.clone(),
    };

    let bar = progress_bar();
    let (report, hashed) = rescan(catalog, index, hash, &bar).await?;
    bar.finish_and_clear();

    match hashed {
        Some(hashed) => tracing::info!(
            unchanged = report.unchanged,
            changed = report.changed,
            added = report.added,
            absent = report.absent,
            hashed = hashed.hashed,
            merged = hashed.merged,
            "scan complete"
        ),
        None => tracing::info!(
            unchanged = report.unchanged,
            changed = report.changed,
            added = report.added,
            absent = report.absent,
            "scan complete"
        ),
    }
    Ok(())
}

/// Walks the sources into the native catalog, hashes what the walk left unhashed unless the
/// run was asked not to, and applies what changed to the search index. All three are
/// blocking work, so they run off the runtime's threads.
async fn rescan(
    catalog: Arc<Catalog>,
    config: IndexConfig,
    hash: HashPolicy,
    bar: &Arc<ProgressBar>,
) -> Result<(RescanReport, Option<HashReport>)> {
    let walking = Arc::clone(bar);
    let hashing = Arc::clone(bar);
    let indexing = Arc::clone(bar);
    tokio::task::spawn_blocking(
        move || -> Result<(RescanReport, Option<catalog::HashReport>), IndexError> {
            let report = catalog.rescan(&config, hash, |phase, count| {
                walking.set_message(format!("{phase}: {count} entries"));
                walking.tick();
            })?;
            let hashed = match hash {
                HashPolicy::Now | HashPolicy::Background => {
                    Some(catalog.hash_pending(&config, |count| {
                        hashing.set_message(format!("Hashing: {count} files"));
                        hashing.tick();
                    })?)
                }
                HashPolicy::Never => None,
            };
            catalog.refresh_search(|count| {
                indexing.set_message(format!("Refreshing the search index: {count} changes"));
                indexing.tick();
            })?;
            Ok((report, hashed))
        },
    )
    .await
    .context("rescanning the catalog")?
    .context("rescanning the catalog")
}

pub(super) fn progress_bar() -> Arc<ProgressBar> {
    let bar = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner} {msg}") {
        bar.set_style(style);
    }
    Arc::new(bar)
}
