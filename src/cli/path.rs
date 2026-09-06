use std::sync::Arc;

use anyhow::{Context as _, Result};

use hoard_core::{ArtifactId, RepoId, Sha256};
use hoard_index::catalog::Catalog;

use super::node::Context;

/// Prints only the path, so the command composes as `llama-server -m $(hoard path org/model)`.
pub async fn run(context: Context, target: String) -> Result<()> {
    let Context { catalog, .. } = context;
    let path = tokio::task::spawn_blocking(move || resolve(&catalog, &target))
        .await
        .context("resolving the target")??;
    tracing::info!("{path}");
    Ok(())
}

fn resolve(catalog: &Arc<Catalog>, target: &str) -> Result<String> {
    let found = if let Ok(sha) = target.parse::<Sha256>() {
        by_sha256(catalog, sha)?
    } else if target.starts_with("a_")
        && let Ok(id) = target.parse::<ArtifactId>()
    {
        by_artifact(catalog, id)?
    } else {
        by_repo(catalog, target)?
    };
    found.ok_or_else(|| anyhow::anyhow!("{target} is not held on this node"))
}

fn by_sha256(catalog: &Arc<Catalog>, sha: Sha256) -> Result<Option<String>> {
    let located = catalog
        .entries_by_sha256(&[sha])
        .context("reading the entries holding the hash")?;
    Ok(located
        .into_iter()
        .find_map(|(_, entry)| entry.abs_path.filter(|_| entry.present)))
}

fn by_artifact(catalog: &Arc<Catalog>, id: ArtifactId) -> Result<Option<String>> {
    let entries = catalog.entries_of(id).context("reading entries")?;
    Ok(entries
        .into_iter()
        .find_map(|entry| entry.abs_path.filter(|_| entry.present)))
}

fn by_repo(catalog: &Arc<Catalog>, target: &str) -> Result<Option<String>> {
    let repo = RepoId::parse_qualified(target).context("reading the repo reference")?;
    let found = catalog.repository(&repo).context("reading the repo")?;
    Ok(found.map(|detail| detail.root_path))
}

#[cfg(test)]
mod tests {
    use hoard_index::catalog::{QueryPage, QueryRequest};
    use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
    use hoard_index::search::{SortKey, SortSpec};

    use super::*;

    fn scanned() -> (tempfile::TempDir, Arc<Catalog>, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("temp dir");
        let files = directory.path().join("files");
        std::fs::create_dir(&files).expect("source directory");
        std::fs::write(files.join("training.csv"), "prompt,response\nhello,world\n")
            .expect("fixture file");
        let config = IndexConfig {
            data_dir: directory.path().join("data"),
            hash_policy: HashPolicy::Now,
            sources: vec![SourceConfig {
                name: "datasets".to_owned(),
                path: files.clone(),
                kind: SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            }],
            ..IndexConfig::default()
        };
        let catalog =
            Catalog::open_or_scan(&config, "test-node", |_, _| {}).expect("catalog scans");
        (directory, Arc::new(catalog), files)
    }

    fn only_row(catalog: &Arc<Catalog>) -> QueryPage {
        let context = catalog
            .compile_context("", jiff::Timestamp::now(), Vec::new())
            .expect("an empty query compiles");
        catalog
            .query(QueryRequest {
                raw: "",
                sort: SortSpec::new(SortKey::Size, true),
                limit: 10,
                offset: 0,
                context: &context,
            })
            .expect("the catalog answers")
    }

    #[test]
    fn a_hash_an_artifact_id_and_a_repository_all_name_the_same_file() {
        let (_directory, catalog, files) = scanned();
        let page = only_row(&catalog);
        let row = page.rows.first().expect("the scan found the file");
        let on_disk = row.abs_path.clone().expect("the entry has a path");
        let sha = row.blob_sha256.expect("the file was hashed");

        assert_eq!(
            resolve(&catalog, &row.artifact_id.to_string()).expect("the id resolves"),
            on_disk
        );
        assert_eq!(
            resolve(&catalog, &sha.to_hex()).expect("the hash resolves"),
            on_disk
        );
        assert_eq!(
            resolve(&catalog, &row.repo).expect("the repository resolves"),
            files.display().to_string()
        );
    }

    #[test]
    fn a_target_this_node_does_not_hold_is_an_error() {
        let (_directory, catalog, _files) = scanned();
        assert!(resolve(&catalog, "models/absent/model").is_err());
        assert!(resolve(&catalog, "a_9999").is_err());
        assert!(
            resolve(&catalog, &"0".repeat(64)).is_err(),
            "an unheld hash is not a path"
        );
    }
}
