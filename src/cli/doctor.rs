use std::path::Path;

use anyhow::Result;

use hoard_core::format_size;
use hoard_index::catalog::Catalog;
use hoard_index::config::SourceConfig;

use super::node::{Finding, Inspection};

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

/// Reports what this installation can do, and corrects nothing unless `repair` is set, in
/// which case the derived totals are recounted, stored and verified.
pub async fn run(inspection: Inspection, repair: bool) -> Result<()> {
    let Inspection { config, catalog } = inspection;
    let mut checks = vec![git_binary(), data_dir(&config.server.data_dir)];
    let cache = cache_dir(&config.cache_root());
    let upstream = match config.mirror.offline {
        true => Check::pass("upstream", "offline mode; the network is never touched"),
        false => Check::pass("upstream", config.mirror.upstream),
    };
    let sources = config.index.sources;

    checks.extend(
        tokio::task::spawn_blocking(move || catalog_checks(catalog, &sources, repair)).await??,
    );
    checks.push(cache);
    checks.push(upstream);

    tracing::info!("\n{}", render(&checks));

    let failures = checks.iter().filter(|check| !check.ok).count();
    if failures > 0 {
        anyhow::bail!("{failures} checks failed");
    }
    Ok(())
}

fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let mark = if check.ok { "ok  " } else { "FAIL" };
        out.push_str(&format!("{mark} {:<9} {}\n", check.name, check.detail));
    }
    out
}

fn catalog_checks(
    catalog: Result<Catalog, Finding>,
    sources: &[SourceConfig],
    repair: bool,
) -> Result<Vec<Check>> {
    match catalog {
        Ok(catalog) => opened(catalog, sources, repair),
        Err(finding) => Ok(unopened(&finding, sources)),
    }
}

fn opened(mut catalog: Catalog, sources: &[SourceConfig], repair: bool) -> Result<Vec<Check>> {
    let status = catalog.status()?;
    let mut checks = vec![Check::pass(
        "catalog",
        format!(
            "format {}, generation {}",
            status.format_version, status.generation
        ),
    )];

    let nodes = catalog.list_nodes()?;
    checks.push(match nodes.iter().find(|node| node.is_self) {
        Some(node) => Check::pass("node", format!("{} ({})", node.name, node.node_id)),
        None => Check::fail("node", "this machine is not recorded; run `hoard scan`"),
    });
    checks.push(Check::pass(
        "peers",
        format!("{} known", nodes.len().saturating_sub(1)),
    ));
    checks.push(summary(&mut catalog, repair)?);
    checks.push(match status.search_built {
        true => Check::pass(
            "search",
            format!("built, {} pending changes", status.pending_changes),
        ),
        false => Check::fail("search", "not built; run `hoard scan`"),
    });

    let indexed = catalog.sources()?;
    checks.extend(source_checks(sources, |name| {
        match indexed
            .iter()
            .find(|record| record.name == name && record.last_scan.is_some())
        {
            Some(record) => Check::pass(
                "source",
                format!(
                    "{name} {} files, {}",
                    record.file_count,
                    format_size(record.byte_count)
                ),
            ),
            None => Check::pass("source", format!("{name} not scanned yet")),
        }
    }));
    Ok(checks)
}

fn unopened(finding: &Finding, sources: &[SourceConfig]) -> Vec<Check> {
    let mut checks = vec![
        Check::fail("catalog", finding.to_string()),
        Check::fail("node", CLOSED),
        Check::fail("peers", CLOSED),
        Check::fail("summary", CLOSED),
        Check::fail("search", CLOSED),
    ];
    checks.extend(source_checks(sources, |name| {
        Check::fail("source", format!("{name} {CLOSED}"))
    }));
    checks
}

const CLOSED: &str = "catalog not open";

/// One line per configured source: whether its root is on disk, and what the catalog holds
/// for it when the catalog answered.
fn source_checks(
    sources: &[SourceConfig],
    indexed: impl Fn(&str) -> Check,
) -> impl Iterator<Item = Check> {
    let empty = sources.is_empty().then(|| {
        Check::fail(
            "sources",
            "none configured; add [[index.sources]] to hoard.toml",
        )
    });
    empty.into_iter().chain(
        sources
            .iter()
            .map(move |source| match source.path.exists() {
                false => Check::fail(
                    "source",
                    format!(
                        "{} at {} does not exist",
                        source.name,
                        source.path.display()
                    ),
                ),
                true => indexed(&source.name),
            }),
    )
}

/// The stored totals against a fresh count of every artifact record. They differ only when
/// a writer moved the records without moving the totals. Without `repair` the drift is
/// reported and the stored totals are left alone; with it they are recounted, stored and
/// counted again to prove the repair converged.
fn summary(catalog: &mut Catalog, repair: bool) -> Result<Check> {
    if !repair {
        let stored = catalog.totals()?;
        let counted = catalog.count_summary()?;
        return Ok(match stored == counted {
            true => Check::pass(
                "summary",
                format!(
                    "{} artifacts, {} present, {} repositories",
                    counted.artifacts, counted.present_artifacts, counted.repositories
                ),
            ),
            false => Check::fail(
                "summary",
                format!(
                    "totals have drifted: stored {stored:?}, counted {counted:?}; \
                     run `hoard doctor --repair`"
                ),
            ),
        });
    }
    let (before, after) = catalog.repair_summary()?;
    let verified = catalog.count_summary()?;
    Ok(if verified != after {
        Check::fail(
            "summary",
            format!("repair did not converge: stored {after:?}, counted {verified:?}"),
        )
    } else if before == after {
        Check::pass(
            "summary",
            format!("no drift; the stored totals were already exact: {after:?}"),
        )
    } else {
        Check::pass(
            "summary",
            format!("repaired: {before:?} became {after:?}, verified"),
        )
    })
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

enum DirectoryState {
    Present,
    NotADirectory,
    Absent,
    Unreadable(String),
}

fn directory_state(path: &Path) -> DirectoryState {
    match path.metadata() {
        Ok(metadata) if metadata.is_dir() => DirectoryState::Present,
        Ok(_) => DirectoryState::NotADirectory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirectoryState::Absent,
        Err(error) => DirectoryState::Unreadable(error.to_string()),
    }
}

fn data_dir(path: &Path) -> Check {
    match directory_state(path) {
        DirectoryState::Present => Check::pass("data", path.display().to_string()),
        DirectoryState::NotADirectory => {
            Check::fail("data", format!("{} is not a directory", path.display()))
        }
        DirectoryState::Absent => Check::fail(
            "data",
            format!("{} does not exist; run `hoard scan`", path.display()),
        ),
        DirectoryState::Unreadable(error) => {
            Check::fail("data", format!("{}: {error}", path.display()))
        }
    }
}

fn cache_dir(path: &Path) -> Check {
    match directory_state(path) {
        DirectoryState::Present => Check::pass("cache", path.display().to_string()),
        DirectoryState::Absent => Check::pass(
            "cache",
            format!(
                "{}: not created yet; the first download creates it",
                path.display()
            ),
        ),
        DirectoryState::NotADirectory => {
            Check::fail("cache", format!("{} is not a directory", path.display()))
        }
        DirectoryState::Unreadable(error) => {
            Check::fail("cache", format!("{}: {error}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hoard_index::catalog::{Catalog, Totals};
    use hoard_server::HoardConfig;

    use super::super::node::inspect;
    use super::*;

    fn configured(root: &Path) -> PathBuf {
        let models = root.join("models");
        std::fs::create_dir_all(&models).expect("the source root");
        let path = root.join("hoard.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\ndata_dir = {data:?}\n\n[[index.sources]]\nname = \"comfy\"\npath = {models:?}\nkind = \"loose\"\n",
                data = root.join("data"),
                models = models,
            ),
        )
        .expect("the config file");
        path
    }

    fn scan(config: &HoardConfig) -> Catalog {
        Catalog::open_or_scan(&config.index, &config.node.name, |_, _| {}).expect("a fresh catalog")
    }

    fn diagnose(inspection: super::super::node::Inspection, repair: bool) -> String {
        let sources = inspection.config.index.sources.clone();
        render(&catalog_checks(inspection.catalog, &sources, repair).expect("the catalog answers"))
    }

    #[tokio::test]
    async fn doctor_reports_a_missing_catalog_without_creating_one() {
        let root = tempfile::tempdir().expect("temp dir");
        let config_path = configured(root.path());
        let data = root.path().join("data");

        let inspection = inspect(Some(&config_path))
            .await
            .expect("the configuration loads");
        assert!(
            matches!(inspection.catalog, Err(Finding::Missing(_))),
            "a data directory with no catalog is a finding, not an error"
        );
        let report = diagnose(inspection, false);
        assert!(report.contains("FAIL catalog"), "{report}");
        assert!(report.contains("no catalog at"), "{report}");
        assert!(
            report.contains("FAIL summary   catalog not open"),
            "{report}"
        );
        assert!(
            report.contains("FAIL source    comfy catalog not open"),
            "a configured source keeps its line when the catalog is closed: {report}"
        );

        let second = inspect(Some(&config_path)).await.expect("the second load");
        let cache_root = second.config.cache_root();
        let failed = run(second, false).await;
        assert!(failed.is_err(), "a diagnosis with failures exits non-zero");
        assert!(!data.exists(), "a diagnosis creates no data directory");
        assert!(
            !data.join("native").exists(),
            "and no catalog directory either"
        );
        assert!(
            !cache_root.exists(),
            "and no cache root; the first download creates that"
        );
    }

    #[tokio::test]
    async fn doctor_reports_drift_and_repairs_only_when_asked() {
        let root = tempfile::tempdir().expect("temp dir");
        let config_path = configured(root.path());
        let config = HoardConfig::load(&config_path).expect("the configuration loads");
        let catalog = scan(&config);
        let truth = catalog.count_summary().expect("a count");
        let wrong = Totals {
            artifacts: truth.artifacts + 9,
            indexed_bytes: truth.indexed_bytes + 8192,
            ..truth.clone()
        };
        catalog.drift_summary(&wrong).expect("drifted totals");
        drop(catalog);

        let report = diagnose(inspect(Some(&config_path)).await.expect("inspect"), false);
        assert!(report.contains("FAIL summary"), "{report}");
        assert!(
            report.contains("run `hoard doctor --repair`"),
            "the diagnosis names the command that repairs: {report}"
        );
        let left = Catalog::inspect(&config.index.native_path()).expect("reopen");
        assert_eq!(
            left.totals().expect("stored totals"),
            wrong,
            "a diagnosis leaves the drift where it found it"
        );
        drop(left);

        let repaired = diagnose(inspect(Some(&config_path)).await.expect("inspect"), true);
        assert!(repaired.contains("ok   summary"), "{repaired}");
        assert!(repaired.contains("verified"), "{repaired}");
        let fixed = Catalog::inspect(&config.index.native_path()).expect("reopen");
        assert_eq!(
            fixed.totals().expect("stored totals"),
            truth,
            "a repair stores what the records count to"
        );
        assert_eq!(
            fixed.totals().expect("stored"),
            fixed.count_summary().expect("counted")
        );
        drop(fixed);

        let final_pass = inspect(Some(&config_path)).await.expect("inspect");
        let cache_root = final_pass.config.cache_root();
        run(final_pass, false)
            .await
            .expect("a repaired installation passes every check without a cache root");
        assert!(
            !cache_root.exists(),
            "and passing did not create one either"
        );
    }

    #[test]
    fn an_absent_cache_root_passes_and_an_absent_data_directory_does_not() {
        let root = tempfile::tempdir().expect("temp dir");
        let absent = root.path().join("nothing-here");

        let cache = cache_dir(&absent);
        assert!(cache.ok, "the mirror creates its cache on the first write");
        assert!(
            cache
                .detail
                .contains("not created yet; the first download creates it"),
            "{}",
            cache.detail
        );
        assert!(!data_dir(&absent).ok, "a data directory has to be there");

        let file = root.path().join("a-file");
        std::fs::write(&file, b"not a directory").expect("the file writes");
        assert!(
            !cache_dir(&file).ok,
            "a cache root that is a file is a fault"
        );
        assert!(
            !data_dir(&file).ok,
            "and so is a data directory that is one"
        );

        assert!(cache_dir(root.path()).ok, "a real directory passes");
        assert!(data_dir(root.path()).ok, "for both checks");
    }
}
