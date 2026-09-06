use std::path::Path;

use anyhow::Result;

use std::sync::Arc;

use hoard_core::format_size;

use super::node::Context;

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

pub async fn run(context: Context) -> Result<()> {
    let Context {
        config, catalog, ..
    } = context;
    let mut checks = Vec::new();

    checks.push(git_binary());
    checks.push(data_dir(&config.server.data_dir));

    let listing = Arc::clone(&catalog);
    let nodes = tokio::task::spawn_blocking(move || listing.list_nodes()).await??;
    let this = nodes.iter().find(|node| node.is_self);
    checks.push(match this {
        Some(node) => Check::pass("node", format!("{} ({})", node.name, node.node_id)),
        None => Check::fail("node", "this machine is not recorded; run `hoard scan`"),
    });
    checks.push(Check::pass(
        "peers",
        format!("{} known", nodes.len().saturating_sub(1)),
    ));

    let sources = tokio::task::spawn_blocking(move || catalog.sources()).await??;
    if config.index.sources.is_empty() {
        checks.push(Check::fail(
            "sources",
            "none configured; add [[index.sources]] to hoard.toml",
        ));
    }
    for source in &config.index.sources {
        let indexed = sources.iter().find(|record| record.name == source.name);
        checks.push(if !source.path.exists() {
            Check::fail(
                "source",
                format!(
                    "{} at {} does not exist",
                    source.name,
                    source.path.display()
                ),
            )
        } else {
            match indexed {
                Some(record) if record.last_scan.is_some() => Check::pass(
                    "source",
                    format!(
                        "{} {} files, {}",
                        source.name,
                        record.file_count,
                        format_size(record.byte_count)
                    ),
                ),
                Some(_) | None => Check::pass("source", format!("{} not scanned yet", source.name)),
            }
        });
    }

    checks.push(cache_dir(&config.cache_root()));
    checks.push(if config.mirror.offline {
        Check::pass("upstream", "offline mode; the network is never touched")
    } else {
        Check::pass("upstream", config.mirror.upstream.clone())
    });

    let mut out = String::new();
    for check in &checks {
        let mark = if check.ok { "ok  " } else { "FAIL" };
        out.push_str(&format!("{mark} {:<9} {}\n", check.name, check.detail));
    }
    tracing::info!("\n{out}");

    let failures = checks.iter().filter(|check| !check.ok).count();
    if failures > 0 {
        anyhow::bail!("{failures} checks failed");
    }
    Ok(())
}

fn git_binary() -> Check {
    match std::process::Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => Check::pass(
            "git",
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        _ => Check::fail("git", "not found; the git face cannot serve clones"),
    }
}

fn data_dir(path: &Path) -> Check {
    match std::fs::create_dir_all(path) {
        Ok(()) => Check::pass("data", path.display().to_string()),
        Err(error) => Check::fail("data", format!("{}: {error}", path.display())),
    }
}

fn cache_dir(path: &Path) -> Check {
    match std::fs::create_dir_all(path) {
        Ok(()) => Check::pass("cache", path.display().to_string()),
        Err(error) => Check::fail("cache", format!("{}: {error}", path.display())),
    }
}
