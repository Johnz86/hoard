use std::sync::Arc;

use anyhow::{Context as _, Result};
use indicatif::{ProgressBar, ProgressStyle};

use hoard_core::format_size;
use hoard_index::catalog::{Catalog, HashProgress, HashReport, RescanReport, RescanScope};
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
    let scope = match source {
        Some(_) => RescanScope::Narrowed,
        None => RescanScope::Configured,
    };
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
    let (report, hashed) = rescan(catalog, index, hash, scope, &bar).await?;
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
    scope: RescanScope,
    bar: &Arc<ProgressBar>,
) -> Result<(RescanReport, Option<HashReport>)> {
    let walking = Arc::clone(bar);
    let hashing = Arc::clone(bar);
    let indexing = Arc::clone(bar);
    tokio::task::spawn_blocking(
        move || -> Result<(RescanReport, Option<catalog::HashReport>), IndexError> {
            let report = catalog.rescan(&config, hash, scope, |phase, count| {
                walking.set_message(format!("{phase}: {count} entries"));
                walking.tick();
            })?;
            let hashed = match hash {
                HashPolicy::Now | HashPolicy::Background => {
                    Some(catalog.hash_pending(&config, |update| {
                        hashing.set_message(hashing_message(update));
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

/// What a hashing pass says while it runs: the files it has finished of the files it owes,
/// the bytes it has read through them, and what it is reading at once a rate is known.
pub(super) fn hashing_message(update: HashProgress) -> String {
    let rate = match update.rate_bps {
        Some(rate) => format!("{}/s", format_size(rate)),
        None => "measuring".to_owned(),
    };
    format!(
        "Hashing: {}/{} files, {} read at {rate}",
        update.files_done,
        update.files_total,
        format_size(update.bytes_done)
    )
}

pub(super) fn progress_bar() -> Arc<ProgressBar> {
    let bar = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner} {msg}") {
        bar.set_style(style);
    }
    Arc::new(bar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashing_message_names_the_files_the_bytes_and_the_rate() {
        assert_eq!(
            hashing_message(HashProgress {
                files_done: 12,
                files_total: 130,
                bytes_done: 26_400_000_000,
                bytes_total: 314_000_000_000,
                rate_bps: Some(239_075_328),
            }),
            "Hashing: 12/130 files, 26.4 GB read at 239.1 MB/s"
        );
    }

    #[test]
    fn a_hashing_message_before_the_first_timed_tick_says_it_is_measuring() {
        assert_eq!(
            hashing_message(HashProgress::default()),
            "Hashing: 0/0 files, 0 B read at measuring"
        );
    }
}
