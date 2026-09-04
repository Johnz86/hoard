//! `hoard path`: print where a repo or artifact actually is, for shell interpolation.

use anyhow::{Context as _, Result};

use hoard_core::{RepoId, Sha256};
use hoard_index::store::Store;
use hoard_index::store::blob::blob_by_sha256;
use hoard_index::store::entry::{entries_for_artifact, entries_for_blob};
use hoard_index::store::node::local_node;
use hoard_index::store::repo::repo_by_qualified;

use super::node::Context;

/// Resolves `org/name`, `type/org/name`, an artifact id or a sha256 to a path on this disk.
/// This is what makes `llama-server -m $(hoard path org/model)` work.
pub async fn run(context: Context, target: String) -> Result<()> {
    let Context { store, .. } = context;
    let path = resolve(&store, &target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{target} is not held on this node"))?;
    tracing::info!("{path}");
    Ok(())
}

async fn resolve(store: &Store, target: &str) -> Result<Option<String>> {
    if let Ok(sha) = target.parse::<Sha256>() {
        return by_sha256(store, sha).await;
    }
    if let Ok(id) = target.parse::<hoard_core::ArtifactId>()
        && target.starts_with("a_")
    {
        let entries = store
            .call(move |conn| entries_for_artifact(conn, id))
            .await
            .context("reading entries")?;
        return Ok(entries.into_iter().find_map(|entry| entry.abs_path));
    }
    by_repo(store, target).await
}

async fn by_sha256(store: &Store, sha: Sha256) -> Result<Option<String>> {
    let blob = store
        .call(move |conn| blob_by_sha256(conn, &sha))
        .await
        .context("reading the blob")?;
    let Some(blob) = blob else {
        return Ok(None);
    };
    let entries = store
        .call(move |conn| entries_for_blob(conn, blob.id))
        .await
        .context("reading entries")?;
    Ok(entries.into_iter().find_map(|entry| entry.abs_path))
}

async fn by_repo(store: &Store, target: &str) -> Result<Option<String>> {
    let repo = RepoId::parse_qualified(target).context("reading the repo reference")?;
    let qualified = repo.qualified();
    let found = store
        .call(move |conn| {
            let node = local_node(conn)?;
            repo_by_qualified(conn, &qualified, node.id)
        })
        .await
        .context("reading the repo")?;
    Ok(found
        .and_then(|row| row.root_path)
        .map(|path| path.display().to_string()))
}
