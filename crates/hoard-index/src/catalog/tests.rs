use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::config::{IndexConfig, SourceConfig, SourceKind};

use crate::error::{IndexError, Result};

use super::models::{ArtifactRecord, ArtifactRecordKey, Manifest, SearchEntry, Source};
use super::{Catalog, SearchRequest, models};

fn source(path: &Path, name: &str) -> SourceConfig {
    SourceConfig {
        name: name.to_owned(),
        path: path.to_path_buf(),
        kind: SourceKind::Loose,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: false,
    }
}

fn fixture() -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("training.csv"), "prompt,response\nhello,world\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "datasets")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    Ok((directory, catalog))
}

fn search(catalog: &Catalog, query: &str) -> Result<super::SearchPage> {
    catalog.search(SearchRequest {
        query,
        limit: 50,
        offset: 0,
    })
}

#[test]
fn sources_scan_into_typed_native_records_and_reopen_without_sqlite() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let status = catalog.status()?;
    assert_eq!(status.node, "test-node");
    assert!(status.counts.contains(&("entries".into(), 2)));
    assert!(!status.search_ready);
    assert!(search(&catalog, "").is_err());
    {
        let read = catalog.database.r_transaction()?;
        let sources: Vec<Source> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].resolved_kind, SourceKind::Loose);
        assert_eq!(read.len().primary::<ArtifactRecord>()?, 2);
        assert_eq!(read.len().primary::<SearchEntry>()?, 2);
    }
    catalog.rebuild_search(|_| {})?;
    let found = search(&catalog, "format:csv")?;
    assert_eq!(found.total, 1);
    assert_eq!(found.rows[0].name, "training.csv");
    assert_eq!(found.rows[0].node, "test-node");
    drop(catalog);
    let mut reopened = Catalog::open(&directory.path().join("native"))?;
    assert!(reopened.check_integrity()?);
    assert!(reopened.status()?.search_ready);
    assert_eq!(search(&reopened, "")?.total, 2);
    assert!(!directory.path().join("native/hoard.db").exists());
    Ok(())
}

#[test]
fn rebuilding_search_needs_only_native_records() -> Result<()> {
    let (directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let original = search(&catalog, "")?;
    fs::remove_dir_all(directory.path().join("files"))?;
    let old_directory = catalog.manifest()?.search_directory;
    catalog.rebuild_search(|_| {})?;
    assert_ne!(catalog.manifest()?.search_directory, old_directory);
    assert_eq!(search(&catalog, "")?, original);
    Ok(())
}

#[test]
fn filtering_ranges_boolean_queries_and_pagination_are_deterministic() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    assert!(all.rows[0].size > all.rows[1].size);
    assert_eq!(search(&catalog, "format:csv AND node:test-node")?.total, 1);
    assert_eq!(search(&catalog, "format:csv OR format:jsonl")?.total, 2);
    assert_eq!(
        search(&catalog, "size:[0 TO 20]")?.rows[0].name,
        "tiny.jsonl"
    );
    assert_eq!(search(&catalog, "training")?.total, 1);
    let first = catalog.search(SearchRequest {
        query: "",
        limit: 1,
        offset: 0,
    })?;
    let second = catalog.search(SearchRequest {
        query: "",
        limit: 1,
        offset: 1,
    })?;
    assert_eq!(first.total, 2);
    assert_eq!(first.rows, vec![all.rows[0].clone()]);
    assert_eq!(second.rows, vec![all.rows[1].clone()]);
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 0,
                offset: 0
            })?
            .rows
            .is_empty()
    );
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 50,
                offset: u32::MAX
            })?
            .rows
            .is_empty()
    );
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 1001,
                offset: 0
            })
            .is_err()
    );
    assert!(search(&catalog, "unknown_field:csv").is_err());
    Ok(())
}

#[test]
fn duplicate_paths_collapse_after_correlated_entry_filtering() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut entry = transaction
        .scan()
        .primary::<SearchEntry>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture entry".into()))??;
    let original_node = entry.row.node.clone();
    let artifact_id = entry.artifact_id;
    let mut artifact: ArtifactRecord = transaction
        .get()
        .primary(artifact_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))?;
    artifact.path_count = 2;
    transaction.upsert(artifact)?;
    entry.entry_id = 900;
    entry.row.entry_id = hoard_core::EntryId::new(900);
    entry.row.node = "peer".into();
    entry.row.state = hoard_core::State::Remote;
    entry.preference = 1;
    transaction.insert(entry)?;
    transaction.commit()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    assert_eq!(all.total, 2);
    let shared = all
        .rows
        .iter()
        .find(|row| row.artifact_id.get() == artifact_id)
        .ok_or_else(|| IndexError::Catalog("missing shared result".into()))?;
    assert_eq!(shared.node, original_node);
    assert!(shared.dupe);
    assert_eq!(shared.path_count, 2);
    assert_eq!(search(&catalog, "node:peer")?.rows[0].node, "peer");
    assert_eq!(search(&catalog, "node:peer AND state:local")?.total, 0);
    Ok(())
}

#[test]
fn native_secondary_indexes_update_and_abort_atomically() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut artifact = transaction
        .scan()
        .primary::<ArtifactRecord>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))??;
    let original = artifact.identity.clone();
    artifact.identity = "replacement".into();
    transaction.upsert(artifact.clone())?;
    let missing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, original.as_str())?;
    assert!(missing.is_none());
    transaction.abort()?;
    let read = catalog.database.r_transaction()?;
    let restored: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, original.as_str())?;
    assert!(restored.is_some());
    let aborted: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, "replacement")?;
    assert!(aborted.is_none());
    Ok(())
}

#[test]
fn incomplete_or_unknown_catalogs_are_refused() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database =
        native_db::Builder::new().create(models()?, directory.path().join("catalog.redb"))?;
    drop(database);
    assert!(Catalog::open(directory.path()).is_err());
    let (_directory, catalog) = fixture()?;
    let mut manifest = catalog.manifest()?;
    manifest.format_version = 999;
    let transaction = catalog.database.rw_transaction()?;
    transaction.upsert(manifest)?;
    transaction.commit()?;
    let path = catalog.directory.clone();
    drop(catalog);
    assert!(Catalog::open(&path).is_err());
    Ok(())
}

#[test]
fn invalid_output_and_sources_do_not_overwrite_existing_data() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let config = IndexConfig {
        sources: vec![source(&directory.path().join("files"), "datasets")],
        ..IndexConfig::default()
    };
    assert!(Catalog::scan_sources(&catalog.directory, "node", &config, |_, _| {}).is_err());
    assert!(
        Catalog::scan_sources(
            &directory.path().join("files/nested"),
            "node",
            &config,
            |_, _| {}
        )
        .is_err()
    );
    assert!(!directory.path().join("files/nested").exists());
    assert_eq!(
        catalog.status()?.counts.last(),
        Some(&("entries".into(), 2))
    );
    assert!(
        Catalog::scan_sources(
            &directory.path().join("empty"),
            "node",
            &IndexConfig::default(),
            |_, _| {}
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn unpublished_rebuilds_do_not_change_the_previous_search_manifest() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let original = catalog.manifest()?;
    fs::create_dir(catalog.directory.join("search-v1-interrupted"))?;
    let reopened_manifest: Manifest = catalog
        .database
        .r_transaction()?
        .get()
        .primary(0_u8)?
        .ok_or_else(|| IndexError::Catalog("missing fixture manifest".into()))?;
    assert_eq!(reopened_manifest, original);
    assert_eq!(search(&catalog, "")?.total, 2);
    Ok(())
}

#[test]
fn a_catalog_open_elsewhere_reports_the_lock_plainly() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let second = Catalog::open(&directory.path().join("native"));
    let message = match second {
        Err(IndexError::Catalog(message)) => message,
        other => panic!("expected a catalog error, got {other:?}"),
    };
    assert!(message.contains("another process"), "{message}");
    drop(catalog);
    Ok(())
}
