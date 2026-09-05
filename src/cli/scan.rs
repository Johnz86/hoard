use std::sync::Arc;

use anyhow::{Context as _, Result};
use indicatif::{ProgressBar, ProgressStyle};

use hoard_core::format_size;
use hoard_index::config::HashPolicy;
use hoard_index::scan::{self, ScanOptions, ScanProgress};
use hoard_index::{IndexError, catalog::RescanReport};

use super::node::Context;

pub async fn run(context: Context, source: Option<String>, hash: HashPolicy) -> Result<()> {
    let Context {
        config,
        store,
        catalog,
    } = context;
    if config.index.sources.is_empty() {
        anyhow::bail!("no sources are configured; add [[index.sources]] to hoard.toml");
    }
    if let Some(name) = source.as_deref()
        && config.index.source(name).is_none()
    {
        anyhow::bail!("no source named {name} in hoard.toml");
    }

    let bar = progress_bar();
    let ticker = Arc::clone(&bar);
    let report = scan::scan(
        &store,
        &config.index,
        ScanOptions { source, hash },
        &move |progress: ScanProgress| tick(&ticker, &progress),
    )
    .await
    .context("scanning")?;
    bar.finish_and_clear();

    tracing::info!(
        files = report.files,
        blobs = report.blobs,
        artifacts = report.artifacts,
        triage = report.triage,
        bytes = %format_size(report.bytes),
        ms = report.elapsed_ms,
        "scan complete"
    );

    if hash == HashPolicy::Background {
        let bar = progress_bar();
        let ticker = Arc::clone(&bar);
        let hashed = scan::hash_pending(&store, &config.index, &move |progress| {
            tick(&ticker, &progress)
        })
        .await
        .context("hashing")?;
        bar.finish_and_clear();
        tracing::info!(hashed, "background hashing complete");
    }

    let bar = progress_bar();
    let report = rescan_catalog(catalog, config.index.clone(), &bar).await?;
    bar.finish_and_clear();
    tracing::info!(
        unchanged = report.unchanged,
        changed = report.changed,
        added = report.added,
        absent = report.absent,
        "native catalog rescanned"
    );
    Ok(())
}

/// Walks the sources again into the native catalog and applies what changed to its search
/// index. Both are blocking work, so they run off the runtime's threads.
async fn rescan_catalog(
    catalog: Arc<hoard_index::catalog::Catalog>,
    config: hoard_index::config::IndexConfig,
    bar: &Arc<ProgressBar>,
) -> Result<RescanReport> {
    let walking = Arc::clone(bar);
    let indexing = Arc::clone(bar);
    tokio::task::spawn_blocking(move || -> Result<RescanReport, IndexError> {
        let report = catalog.rescan(&config, |phase, count| {
            walking.set_message(format!("{phase}: {count} entries"));
            walking.tick();
        })?;
        catalog.refresh_search(|count| {
            indexing.set_message(format!("Refreshing the native index: {count} changes"));
            indexing.tick();
        })?;
        Ok(report)
    })
    .await
    .context("rescanning the native catalog")?
    .context("rescanning the native catalog")
}

pub(super) fn progress_bar() -> Arc<ProgressBar> {
    let bar = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner} {msg}") {
        bar.set_style(style);
    }
    Arc::new(bar)
}

fn tick(bar: &ProgressBar, progress: &ScanProgress) {
    let current = progress.current.as_deref().unwrap_or_default();
    let message = match progress.total {
        Some(total) => format!("{} {}/{total} {current}", progress.phase, progress.done),
        None => format!("{} {} {current}", progress.phase, progress.done),
    };
    bar.set_message(message);
    bar.tick();
}
