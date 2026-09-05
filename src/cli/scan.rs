use std::sync::Arc;

use anyhow::{Context as _, Result};
use indicatif::{ProgressBar, ProgressStyle};

use hoard_core::format_size;
use hoard_index::config::HashPolicy;
use hoard_index::scan::{self, ScanOptions, ScanProgress};

use super::node::Context;

pub async fn run(context: Context, source: Option<String>, hash: HashPolicy) -> Result<()> {
    let Context { config, store } = context;
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
    Ok(())
}

fn progress_bar() -> Arc<ProgressBar> {
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
