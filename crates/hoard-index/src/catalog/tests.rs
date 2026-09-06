use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, EntryId, Evidence,
    EvidenceSource, JobId, Loadout, LoadoutPin, NodeId, Precision, RepoId, RepoType, Role, Sha256,
    State, SubRole, TensorSample, TensorSummary,
};

use crate::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};

use crate::discover::RefKind;
use crate::error::{IndexError, Result};
use crate::row::SearchRow;
use crate::search::{CompileContext, SortKey, SortSpec, parse};

use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, ArtifactRecordV1, Entry, EntryKey, EntryV1,
    LOCAL_NODE, Manifest, ManifestV1, Node, Override, Ref, Repository, RepositoryKey, RepositoryV1,
    RepositoryV2, RepositoryV3, Revision, RevisionV1, RevisionV2, SearchChange, SearchText, Source,
    SourceV1, SourceV2, Summary, SummaryV1, SummaryV2,
};
use super::search::QueryPath;
use super::seed::{SeedArtifact, SeedEntry, SeedRepository, SeedRevision};
use super::{
    Catalog, DigestApplied, JobProgress, JobState, MIRROR_SOURCE, NewProjection, PullMark,
    PulledFile, QueryPage, QueryRequest, RemoteEntry, RepositoryDetail, RepositoryFilter,
    RescanReport, ServedFile, UsageEvent, builder, models, query,
};

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
}

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
        HashPolicy::Never,
        |_, _| {},
    )?;
    Ok((directory, catalog))
}

fn search(catalog: &Catalog, raw: &str) -> Result<QueryPage> {
    let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
    catalog.query(QueryRequest {
        raw,
        sort: SortSpec::default(),
        limit: 50,
        offset: 0,
        context: &context,
    })
}

fn paged(catalog: &Catalog, limit: u32, offset: u32) -> Result<QueryPage> {
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::default(),
        limit,
        offset,
        context: &context,
    })
}

#[test]
fn sources_scan_into_typed_records_that_survive_a_reopen() -> Result<()> {
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
        let entries: Vec<Entry> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.artifact_id.is_some())
                .count(),
            2
        );
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
    let names: Vec<&str> = all.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        ["tiny.jsonl", "training.csv"],
        "the default order is name"
    );
    assert_eq!(search(&catalog, "format:csv node:test-node")?.total, 1);
    assert_eq!(search(&catalog, "format:csv,jsonl")?.total, 2);
    assert_eq!(
        search(&catalog, "size:<=20b")?.rows[0].name,
        "tiny.jsonl",
        "a bare size is megabytes, so the byte suffix is what selects the small file"
    );
    assert_eq!(search(&catalog, "size:<=20b")?.total, 1);
    assert_eq!(search(&catalog, "training")?.total, 1);
    let first = paged(&catalog, 1, 0)?;
    let second = paged(&catalog, 1, 1)?;
    assert_eq!(first.total, 2);
    assert_eq!(first.rows, vec![all.rows[0].clone()]);
    assert_eq!(second.rows, vec![all.rows[1].clone()]);
    assert!(paged(&catalog, 0, 0)?.rows.is_empty());
    assert!(paged(&catalog, 50, u32::MAX)?.rows.is_empty());
    assert!(paged(&catalog, 1001, 0).is_err());
    assert_eq!(
        search(&catalog, "unknown_field:csv")?.total,
        0,
        "a key the grammar does not know is free text, not an error"
    );
    Ok(())
}

#[test]
fn duplicate_paths_collapse_after_correlated_entry_filtering() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut entry = transaction
        .scan()
        .primary::<Entry>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture entry".into()))??;
    let artifact_id = entry
        .artifact_id
        .ok_or_else(|| IndexError::Catalog("fixture entry has no artifact".into()))?;
    let mut artifact: ArtifactRecord = transaction
        .get()
        .primary(artifact_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))?;
    artifact.path_count = 2;
    transaction.upsert(artifact)?;
    let mut repository: Repository = transaction
        .get()
        .primary(entry.repo_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture repository".into()))?;
    repository.id = 900;
    repository.key = models::repository_key("peer", &repository.repo);
    repository.source = "peer".to_owned();
    transaction.insert(repository)?;
    entry.id = 900;
    entry.repo_id = 900;
    entry.state = hoard_core::State::Remote;
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
    assert_eq!(shared.source.as_deref(), Some("datasets"));
    assert!(shared.dupe);
    assert_eq!(shared.path_count, 2);
    assert_eq!(
        search(&catalog, "source:peer")?.rows[0].source.as_deref(),
        Some("peer")
    );
    assert_eq!(search(&catalog, "source:peer state:local")?.total, 0);
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
    let original = artifact.identity;
    let replacement = 7_u128;
    artifact.identity = replacement;
    transaction.upsert(artifact.clone())?;
    let missing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, original)?;
    assert!(missing.is_none());
    transaction.abort()?;
    let read = catalog.database.r_transaction()?;
    let restored: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, original)?;
    assert!(restored.is_some());
    let aborted: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, replacement)?;
    assert!(aborted.is_none());
    Ok(())
}

#[test]
fn an_identity_is_a_hash_of_its_text_and_two_copies_share_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let read = catalog.database.r_transaction()?;
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    let canonical = input.join("a/one.csv").canonicalize()?;
    let expected = xxhash_rust::xxh3::xxh3_128(format!("path:{}", canonical.display()).as_bytes());
    let found: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, expected)?;
    assert!(
        found.is_some(),
        "the identity is the xxh3-128 of the canonical path text"
    );
    Ok(())
}

#[test]
fn exactly_one_document_per_artifact_is_primary_and_it_is_the_preferred_entry() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    fs::write(input.join("solo.csv"), "x,y\n3,4\n")?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let primaries = catalog.count_documents("primary", 1)?;
    let all = catalog.count_documents("primary", 0)? + primaries;
    assert_eq!(primaries, 2, "two artifacts, two primary documents");
    assert_eq!(all, 3, "three entries, three documents");
    let page = search(&catalog, "dupe:true")?;
    assert_eq!(page.total, 1);
    assert!(
        page.rows[0].path.ends_with("a/one.csv"),
        "the lowest entry id wins a preference tie: {}",
        page.rows[0].path
    );
    let solo = input.join("solo.csv");
    fs::write(&solo, "x,y\n5,6\n")?;
    fs::File::options()
        .write(true)
        .open(&solo)?
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert!(catalog.pending_changes()? > 0, "the rewrite is logged");
    catalog.refresh_search(|_| {})?;
    assert_eq!(
        catalog.count_documents("primary", 1)?,
        2,
        "a refresh keeps one primary per artifact"
    );
    Ok(())
}

#[test]
fn version_one_records_are_migrated_on_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("catalog.redb");
    {
        let database = native_db::Builder::new().create(models()?, &path)?;
        let transaction = database.rw_transaction()?;
        transaction.insert(ManifestV1 {
            id: 0,
            format_version: 5,
            generation: 3,
            scanned_at: "then".to_owned(),
            node: "old".to_owned(),
            counts: vec![("entries".to_owned(), 1)],
            search_directory: None,
            search_generation: 0,
        })?;
        transaction.insert(SummaryV1 {
            id: 0,
            reclaimable_bytes: 4096,
            duplicate_artifacts: 3,
        })?;
        transaction.commit()?;
        let read = database.r_transaction()?;
        let invisible: Option<Manifest> = read.get().primary(0_u8)?;
        assert!(
            invisible.is_none(),
            "a v1 row is not visible to the v2 type until migrated"
        );
    }
    let catalog = Catalog::open(directory.path())?;
    let status = catalog.status()?;
    assert_eq!(status.generation, 3);
    assert_eq!(status.node, "old");
    let read = catalog.database.r_transaction()?;
    let migrated: Option<Manifest> = read.get().primary(0_u8)?;
    assert_eq!(migrated.map(|manifest| manifest.search_seq), Some(0));
    let old: Option<ManifestV1> = read.get().primary(0_u8)?;
    assert!(old.is_none(), "the v1 table is drained");
    let summary: Option<Summary> = read.get().primary(0_u8)?;
    assert_eq!(
        summary.map(|summary| (summary.reclaimable_bytes, summary.artifacts)),
        Some((4096, 0)),
        "the totals a v1 summary never held start at zero"
    );
    let old: Option<SummaryV1> = read.get().primary(0_u8)?;
    assert!(old.is_none(), "the v1 summary table is drained");
    Ok(())
}

#[test]
fn an_override_rewrites_the_artifact_reaches_search_and_survives_a_rescan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let before = search(&catalog, "role:lora")?;
    let id = before.rows[0].artifact_id;
    catalog.put_override(id, Some(Role::Ctrl), None, Some(Base::new("sdxl 1.0")))?;
    assert_eq!(
        search(&catalog, "role:ctrl")?.total,
        1,
        "the index sees the verdict without a rebuild"
    );
    let verdict = catalog
        .artifact_override(id)?
        .ok_or_else(|| IndexError::Catalog("no override".into()))?;
    assert_eq!(verdict.role, Some(Role::Ctrl));
    let touched = input.join("loras/keep.safetensors");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&touched)?
        .set_modified(later)?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    catalog.refresh_search(|_| {})?;
    let after = search(&catalog, "role:ctrl")?;
    assert!(
        after.rows.iter().any(|row| row.artifact_id == id),
        "a re-inferred artifact keeps its verdict"
    );
    assert!(catalog.clear_override(id)?);
    assert!(!catalog.clear_override(id)?);
    Ok(())
}

#[test]
fn refs_keep_their_kind_and_old_records_migrate_on_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[("unet/adapter.safetensors", lora_header())],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let read = catalog.database.r_transaction()?;
    let repositories: Vec<Repository> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let refs = &repositories[0].revisions[0].refs;
    assert!(
        refs.iter()
            .any(|reference| reference.name == "main" && reference.kind == RefKind::Branch),
        "{refs:?}"
    );
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(!records[0].first_seen.is_empty());
    assert!(!records[0].triage_cleared);
    drop(read);

    let legacy = tempfile::tempdir()?;
    {
        let database = builder().create(models()?, legacy.path().join("catalog.redb"))?;
        let transaction = database.rw_transaction()?;
        transaction.insert(ManifestV1 {
            id: 0,
            format_version: 5,
            generation: 1,
            scanned_at: "then".to_owned(),
            node: "old".to_owned(),
            counts: Vec::new(),
            search_directory: None,
            search_generation: 0,
        })?;
        transaction.insert(ArtifactRecordV1 {
            id: 9_000,
            identity: 0x9000,
            artifact: frozen_artifact(),
            name: "old".into(),
            org: None,
            path_count: 1,
            group_bytes: None,
            shard_count: None,
        })?;
        let repo = RepoId::new(RepoType::Models, Some("acme".to_owned()), "styler");
        transaction.insert(RepositoryV1 {
            id: 11,
            key: models::repository_key("hub", &repo),
            source: "hub".to_owned(),
            repo,
            root_path: "/hub".to_owned(),
            default_ref: "main".to_owned(),
            revisions: vec![RevisionV1 {
                name: "main".to_owned(),
                sha: None,
                refs: vec!["main".to_owned()],
            }],
        })?;
        transaction.commit()?;
    }
    let reopened = Catalog::open(legacy.path())?;
    let read = reopened.database.r_transaction()?;
    let migrated: Option<ArtifactRecord> = read.get().primary(9_000_i64)?;
    assert!(
        migrated.is_some_and(|record| !record.triage_cleared && record.first_seen.is_empty()),
        "a v1 record is migrated on open"
    );
    let repository: Option<Repository> = read.get().primary(11_i64)?;
    assert_eq!(
        repository
            .and_then(|repository| repository.revisions[0].refs.first().map(|ref_| ref_.kind)),
        Some(RefKind::Branch),
        "a v1 repository's ref names a branch after migration"
    );
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
    assert!(
        Catalog::scan_sources(
            &catalog.directory,
            "node",
            &config,
            HashPolicy::Never,
            |_, _| {}
        )
        .is_err()
    );
    assert!(
        Catalog::scan_sources(
            &directory.path().join("files/nested"),
            "node",
            &config,
            HashPolicy::Never,
            |_, _| {}
        )
        .is_err()
    );
    assert!(!directory.path().join("files/nested").exists());
    assert_eq!(
        catalog.status()?.counts.last(),
        Some(&("entries".into(), 2))
    );
    let empty = Catalog::scan_sources(
        &directory.path().join("empty"),
        "node",
        &IndexConfig::default(),
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(
        empty.status()?.counts.last(),
        Some(&("entries".into(), 0)),
        "a configuration naming no source scans to an empty catalog of its own"
    );
    Ok(())
}

#[test]
fn an_unpublished_build_leaves_the_previous_index_serving() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let before = catalog.manifest()?;
    let staged = catalog.directory.join("search-v1-staged");
    fs::create_dir(&staged)?;
    catalog.build_index(&staged, |_| {})?;
    assert_eq!(catalog.manifest()?, before, "building publishes nothing");
    assert_eq!(
        search(&catalog, "")?.total,
        2,
        "the previous index still answers"
    );
    catalog.publish("search-v1-staged".to_owned())?;
    assert_eq!(
        catalog.manifest()?.search_directory.as_deref(),
        Some("search-v1-staged")
    );
    assert_eq!(
        search(&catalog, "")?.total,
        2,
        "the new index answers after publish"
    );
    Ok(())
}

#[test]
fn a_rebuild_prunes_the_index_directories_it_supersedes() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let first = catalog
        .manifest()?
        .search_directory
        .ok_or_else(|| IndexError::Catalog("no index".into()))?;
    catalog.rebuild_search(|_| {})?;
    let second = catalog
        .manifest()?
        .search_directory
        .ok_or_else(|| IndexError::Catalog("no index".into()))?;
    assert_ne!(first, second);
    assert!(
        !catalog.directory.join(&first).exists(),
        "the superseded directory is gone"
    );
    assert!(catalog.directory.join(&second).exists());
    assert_eq!(search(&catalog, "")?.total, 2);
    Ok(())
}

#[test]
fn a_rebuild_keeps_manifest_fields_it_does_not_own() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    {
        let transaction = catalog.database.rw_transaction()?;
        let mut manifest = catalog.manifest()?;
        manifest.node = "renamed".to_owned();
        transaction.upsert(manifest)?;
        transaction.commit()?;
    }
    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.manifest()?.node, "renamed");
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

#[test]
fn a_missing_source_root_is_skipped_not_fatal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![
            source(&input, "present"),
            source(&directory.path().join("unmounted"), "absent"),
        ],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let status = catalog.status()?;
    assert!(status.counts.contains(&("sources".into(), 1)));
    assert!(status.counts.contains(&("entries".into(), 1)));
    Ok(())
}

#[test]
fn a_failed_scan_leaves_no_directory_behind() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let not_a_directory = directory.path().join("file-as-root");
    fs::write(&not_a_directory, "x")?;
    let config = IndexConfig {
        sources: vec![source(&not_a_directory, "broken")],
        ..IndexConfig::default()
    };
    let output = directory.path().join("native");
    let result = Catalog::scan_sources(&output, "test-node", &config, HashPolicy::Never, |_, _| {});
    assert!(result.is_err());
    assert!(!output.exists());
    Ok(())
}

#[test]
fn an_unreadable_file_is_recorded_as_absent_instead_of_aborting() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("readable.csv"), "a,b\n1,2\n")?;
    let locked = input.join("locked.csv");
    fs::write(&locked, "a,b\n1,2\n")?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the unreadable file proves no artifact");
    assert_eq!(rows.rows[0].name, "readable.csv");
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let absent = entries
        .iter()
        .find(|entry| entry.rel_path == "locked.csv")
        .ok_or_else(|| IndexError::Catalog("missing unreadable entry".into()))?;
    assert_eq!(entries.len(), 2, "the unreadable file is still an entry");
    assert_eq!(absent.state, hoard_core::State::Remote);
    assert!(
        absent.abs_path.is_some(),
        "the file is on this disk even though it could not be read"
    );
    assert!(absent.artifact_id.is_none());
    drop(read);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[test]
fn an_lfs_pointer_is_an_absent_entry_without_an_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    let oid = "a".repeat(64);
    fs::write(
        input.join("model.safetensors"),
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 123456\n"),
    )?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        search(&catalog, "")?.total,
        0,
        "a pointer is not a model that can be served"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert!(entry.lfs_pointer);
    assert_eq!(entry.size, 123_456);
    assert_eq!(entry.state, hoard_core::State::Remote);
    assert!(entry.abs_path.is_none());
    assert!(
        entry.artifact_id.is_none(),
        "a pointer stub is a path, not an artifact"
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 0);
    Ok(())
}

fn discovered(
    rel_path: &str,
    abs_path: Option<&Path>,
    lfs_pointer: bool,
) -> crate::discover::DiscoveredEntry {
    crate::discover::DiscoveredEntry {
        rel_path: rel_path.to_owned(),
        abs_path: abs_path.map(Path::to_path_buf),
        size: 0,
        stamp: None,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        lfs_pointer,
    }
}

#[test]
fn a_fetched_lfs_weight_is_an_artifact_and_an_unfetched_pointer_is_not() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let weights = directory.path().join("model.safetensors");
    fs::write(&weights, lora_header())?;
    let fetched = discovered("model.safetensors", Some(&weights), true);
    let sniffed = crate::sniff::sniff_path(&weights)?;
    let presence = super::scan::Presence::of(&fetched, Some(&sniffed));
    assert!(presence.readable);
    assert!(
        presence.lfs_pointer,
        "discovery's flag is kept for the entry row"
    );
    assert!(
        !presence.pointer_bytes,
        "the bytes on disk are weights, not a pointer"
    );
    assert!(super::scan::creates_artifact(
        sniffed.format,
        &presence,
        &super::scan::EntryRole::Standalone
    ));

    let pointer = directory.path().join("pointer.safetensors");
    let oid = "a".repeat(64);
    fs::write(
        &pointer,
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 5\n"),
    )?;
    let unfetched = discovered("pointer.safetensors", Some(&pointer), false);
    let sniffed = crate::sniff::sniff_path(&pointer)?;
    let presence = super::scan::Presence::of(&unfetched, Some(&sniffed));
    assert!(presence.pointer_bytes);
    assert!(!presence.readable);
    assert!(!super::scan::creates_artifact(
        sniffed.format,
        &presence,
        &super::scan::EntryRole::Standalone
    ));
    Ok(())
}

fn plain_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"model.layers.0.mlp.down_proj.weight":"#,
        r#"{"dtype":"F16","shape":[16,64],"data_offsets":[0,2048]}}"#
    ))
}

fn lora_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"__metadata__":{"ss_network_dim":"16","ss_network_alpha":"8"},"#,
        r#""lora_unet_down_blocks_0_attn.lora_down.weight":"#,
        r#"{"dtype":"F16","shape":[16,768],"data_offsets":[0,24576]},"#,
        r#""lora_unet_down_blocks_0_attn.lora_up.weight":"#,
        r#"{"dtype":"F16","shape":[768,16],"data_offsets":[24576,49152]}}"#
    ))
}

fn lora_header_named(tensor: &str) -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(&format!(
        "{{\"__metadata__\":{{\"ss_network_dim\":\"16\",\"ss_network_alpha\":\"8\"}},\
         \"lora_unet_{tensor}_attn.lora_down.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[16,768],\"data_offsets\":[0,24576]}},\
         \"lora_unet_{tensor}_attn.lora_up.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[768,16],\"data_offsets\":[24576,49152]}}}}"
    ))
}

fn remote_entry(sha256: Sha256, path: &str) -> RemoteEntry {
    RemoteEntry {
        sha256,
        size: 12,
        role: Role::Lora,
        base: Base::new("sdxl"),
        repo: RepoId::new(RepoType::Models, Some("kohya".to_owned()), "film-grain"),
        revision_sha: CommitSha::from_bytes([0x22; 20]),
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        path: path.to_owned(),
    }
}

fn hf_cache_repo(root: &Path, org: &str, name: &str, files: &[(&str, Vec<u8>)]) -> Result<()> {
    let repo = root.join(format!("models--{org}--{name}"));
    let commit = "b".repeat(40);
    let snapshot = repo.join("snapshots").join(&commit);
    fs::create_dir_all(&snapshot)?;
    fs::create_dir_all(repo.join("blobs"))?;
    fs::create_dir_all(repo.join("refs"))?;
    fs::write(repo.join("refs/main"), &commit)?;
    for (index, (rel_path, content)) in files.iter().enumerate() {
        let blob = repo.join("blobs").join(format!("{index:064x}"));
        fs::write(&blob, content)?;
        let link = snapshot.join(rel_path);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&blob, &link)?;
    }
    Ok(())
}

#[test]
fn hf_cache_rows_match_the_legacy_shape() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[
            (
                "README.md",
                b"---\ninstance_prompt: acme style\n---\nA style LoRA.\n".to_vec(),
            ),
            (
                "config.json",
                b"{\"architectures\":[\"UNet2DConditionModel\"]}".to_vec(),
            ),
            ("unet/adapter.safetensors", lora_header()),
        ],
    )?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(&hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(
        rows.total, 1,
        "only the weights become an artifact, not README or config"
    );
    let row = &rows.rows[0];
    assert_eq!(row.org.as_deref(), Some("acme"));
    assert_eq!(
        row.name, "styler",
        "a single-artifact repo is named by its repo"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<Entry>()?,
        3,
        "every file is still an entry"
    );
    Ok(())
}

#[test]
fn a_revisions_own_readme_and_config_reach_inference() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("unet"))?;
    fs::write(
        input.join("README.md"),
        "---\ninstance_prompt: acme style\n---\nA style LoRA.\n",
    )?;
    fs::write(
        input.join("config.json"),
        "{\"architectures\":[\"LlamaForCausalLM\"]}",
    )?;
    fs::write(input.join("unet/adapter.safetensors"), plain_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(
        rows.total, 1,
        "only the weights become an artifact, not README or config"
    );
    let row = &rows.rows[0];
    assert_eq!(
        row.name, "adapter.safetensors",
        "a loose file is named by its own file name"
    );
    assert_eq!(
        row.trigger.as_deref(),
        Some("acme style"),
        "the README frontmatter reached inference"
    );
    assert_ne!(
        row.trigger.as_deref(),
        Some("unet/adapter"),
        "the rel path never leaks into the trigger"
    );
    assert_eq!(
        search(&catalog, "style lora")?.total,
        1,
        "the README body is searchable text"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<Entry>()?,
        3,
        "every file is still an entry"
    );
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(records.len(), 1);
    let detail: ArtifactDetail = read
        .get()
        .primary(records[0].id)?
        .ok_or_else(|| IndexError::Catalog("detail missing".into()))?;
    assert!(
        detail.evidence.iter().any(|evidence| {
            evidence.field == "role" && evidence.source == hoard_core::EvidenceSource::Config
        }),
        "the neighbouring config.json was consulted: {:?}",
        detail.evidence
    );
    Ok(())
}

#[test]
fn path_counts_are_final_on_the_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/same.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/same.csv"), input.join("b/same.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    {
        let read = catalog.database.r_transaction()?;
        let entries: Vec<Entry> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        assert_eq!(entries.len(), 2);
        let first: Entry = read
            .get()
            .primary(1_i64)?
            .ok_or_else(|| IndexError::Catalog("first entry missing".into()))?;
        let record: ArtifactRecord = read
            .get()
            .primary(
                first
                    .artifact_id
                    .ok_or_else(|| IndexError::Catalog("first entry has no artifact".into()))?,
            )?
            .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
        assert_eq!(
            record.path_count, 2,
            "only the final sweep can have set the count"
        );
    }
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert!(
        rows.rows.iter().all(|row| row.path_count == 2 && row.dupe),
        "every assembled row reads the final count"
    );
    Ok(())
}

#[test]
fn a_row_is_assembled_from_its_entry_and_artifact() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "format:csv")?;
    assert_eq!(rows.total, 1);
    let row = &rows.rows[0];
    let read = catalog.database.r_transaction()?;
    let entry: Entry = read
        .get()
        .primary(row.entry_id.get())?
        .ok_or_else(|| IndexError::Catalog("entry missing".into()))?;
    let artifact: ArtifactRecord = read
        .get()
        .primary(row.artifact_id.get())?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(row.name, artifact.name);
    assert_eq!(row.path, entry.rel_path);
    assert_eq!(row.size, artifact.artifact.size);
    assert_eq!(row.state, entry.state);
    assert!(
        read.get().primary::<ArtifactDetail>(artifact.id)?.is_some(),
        "the detail record exists"
    );
    Ok(())
}

#[test]
fn an_entry_is_the_only_per_file_record() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "format:csv")?;
    assert_eq!(rows.total, 1);
    let row = &rows.rows[0];
    let read = catalog.database.r_transaction()?;
    let entry: Entry = read
        .get()
        .primary(row.entry_id.get())?
        .ok_or_else(|| IndexError::Catalog("entry missing".into()))?;
    let repository: Repository = read
        .get()
        .primary(entry.repo_id)?
        .ok_or_else(|| IndexError::Catalog("repository missing".into()))?;
    assert_eq!(
        row.revision,
        repository.revisions[entry.revision as usize].name
    );
    assert_eq!(row.repo, repository.repo.qualified());
    assert_eq!(row.source.as_deref(), Some(repository.source.as_str()));
    assert_eq!(row.node, "test-node");
    assert_eq!(row.path, entry.rel_path);
    assert_eq!(entry.artifact_id, Some(row.artifact_id.get()));
    let by_artifact: Vec<Entry> = read
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(row.artifact_id.get()))?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(
        by_artifact.len(),
        1,
        "the optional secondary key finds the entry"
    );
    let found: Option<Repository> = read.get().secondary(
        RepositoryKey::key,
        super::models::repository_key("datasets", &repository.repo).as_str(),
    )?;
    assert!(
        found.is_some(),
        "a repository is found by source and qualified name"
    );
    Ok(())
}

#[test]
fn a_second_path_does_not_rename_the_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/first.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/first.csv"), input.join("b/second.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    assert_eq!(
        rows.rows[0].name, "first.csv",
        "the first path names the artifact"
    );
    assert_eq!(rows.rows[0].path_count, 2);
    assert!(rows.rows[0].dupe);
    Ok(())
}

#[test]
fn storage_reports_every_table_and_compaction_keeps_the_data() -> Result<()> {
    let (directory, mut catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    catalog.compact()?;
    assert_eq!(search(&catalog, "")?.total, 2, "compaction loses nothing");
    let native = directory.path().join("native");
    drop(catalog);
    let report = Catalog::storage(&native)?;
    assert!(report.file_bytes > 0);
    assert!(report.allocated_bytes > 0);
    let names: Vec<&str> = report
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    assert!(
        names.iter().any(|name| name.starts_with("7_2_")),
        "the Entry table is listed: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.starts_with("3_1_")),
        "the ArtifactRecord table is listed: {names:?}"
    );
    let entries = report
        .tables
        .iter()
        .find(|table| table.name.starts_with("7_2_id"))
        .map(|table| table.entries);
    assert_eq!(entries, Some(2));
    let reopened = Catalog::open(&native)?;
    assert_eq!(search(&reopened, "")?.total, 2);
    Ok(())
}

#[test]
fn the_grammar_sorts_facets_and_counts_like_the_index_screen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/b-style.safetensors"), lora_header())?;
    fs::write(input.join("loras/a-style.safetensors"), lora_header())?;
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let by_name = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Name, false),
        limit: 10,
        offset: 0,
        context: &catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?,
    })?;
    assert_eq!(by_name.total, 3);
    assert_eq!(by_name.nodes, 1);
    let names: Vec<&str> = by_name.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        ["a-style.safetensors", "b-style.safetensors", "data.csv"]
    );
    let roles: Vec<(&str, u64)> = by_name
        .facets
        .role
        .iter()
        .map(|bucket| (bucket.value.as_str(), bucket.count))
        .collect();
    assert!(roles.contains(&("lora", 2)), "{roles:?}");
    assert!(
        by_name
            .facets
            .format
            .iter()
            .any(|bucket| bucket.value == "csv" && bucket.count == 1)
    );

    let filtered = search(&catalog, "role:lora")?;
    assert_eq!(filtered.total, 2);
    assert!(
        filtered
            .facets
            .role
            .iter()
            .any(|bucket| bucket.value == "lora" && bucket.active)
    );
    assert!(
        filtered
            .facets
            .format
            .iter()
            .all(|bucket| bucket.value != "csv"),
        "format buckets are counted within the role filter"
    );
    assert!(
        filtered
            .facets
            .role
            .iter()
            .any(|bucket| bucket.value != "lora"),
        "role buckets are counted with the role filter excluded: {:?}",
        filtered.facets.role
    );
    assert_eq!(search(&catalog, "-role:lora")?.total, 1);
    assert_eq!(search(&catalog, "role:lora,ds")?.total, 3);
    assert_eq!(search(&catalog, "size:>1000000")?.total, 0);
    assert_eq!(search(&catalog, "used:>30d")?.total, 0);
    assert_eq!(search(&catalog, "node:self")?.total, 3);
    assert_eq!(search(&catalog, "dupe:true")?.total, 0);
    assert_eq!(search(&catalog, "style")?.total, 2);
    assert_eq!(
        search(&catalog, "~sty")?.total,
        2,
        "fuzzy text is a prefix match"
    );
    Ok(())
}

#[test]
fn a_shelf_and_a_compat_target_resolve_against_the_catalog() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/style.safetensors"), lora_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let shelves = vec![("adapters".to_owned(), "role:lora".to_owned())];
    let context = catalog.compile_context("shelf:adapters", jiff::Timestamp::now(), shelves)?;
    let page = catalog.query(QueryRequest {
        raw: "shelf:adapters",
        sort: SortSpec::default(),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    assert_eq!(page.total, 1);
    let context = catalog.compile_context("compat:style", jiff::Timestamp::now(), Vec::new())?;
    assert!(
        context.compat.is_some(),
        "the target resolves through free text"
    );
    Ok(())
}

#[test]
fn disk_pressure_is_summarised_after_every_sweep() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/one.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/one.csv"), input.join("b/two.csv"))?;
    fs::write(input.join("a/solo.csv"), "x,y\n3,4\n")?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let pressure = catalog.disk_pressure()?;
    assert_eq!(pressure.duplicate_blobs, 1);
    assert_eq!(
        pressure.reclaimable_bytes,
        fs::metadata(input.join("a/one.csv"))?.len()
    );
    fs::remove_file(input.join("b/two.csv"))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let after = catalog.disk_pressure()?;
    assert_eq!(after.reclaimable_bytes, 0);
    assert_eq!(after.duplicate_blobs, 0);
    Ok(())
}

#[test]
fn a_seeded_catalog_answers_like_a_scanned_one() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    let lora = SeedArtifact {
        identity: 0x9f9f,
        artifact: Artifact {
            id: ArtifactId::new(0),
            blob: BlobId::new(0),
            sha256: Some(Sha256::from_bytes([0x9f; 32])),
            size: 239_075_328,
            format: ArtifactFormat::Safetensors,
            role: Role::Lora,
            sub_role: None,
            role_confidence: 0.98,
            base: Base::new("sdxl 1.0"),
            base_label: Some("sdxl 1.0".into()),
            base_confidence: 0.9,
            precision: Some(Precision::new("fp16")),
            quant: None,
            rank: Some(32),
            alpha: Some(16.0),
            params: None,
            context: None,
            trigger: Some("film grain, 35mm, halation".into()),
            sniff_version: 1,
        },
        name: "film-grain-35mm".into(),
        org: Some("kohya".into()),
        tensors: None,
        evidence: Vec::new(),
        card: Some("A grain adapter for SDXL.".into()),
        group_bytes: None,
        shard_count: None,
    };
    let entry = |rel_path: &str, abs_path: &str| SeedEntry {
        node: LOCAL_NODE,
        rel_path: rel_path.into(),
        abs_path: Some(abs_path.into()),
        size: 239_075_328,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: Some(lora.clone()),
    };
    catalog.seed(&[SeedRepository {
        repo: RepoId::parse_ref(RepoType::Models, "kohya/film-grain-35mm")?,
        source: "cache".into(),
        kind: SourceKind::HfCache,
        root_path: "/data/hub".into(),
        git_dir: None,
        revisions: vec![SeedRevision {
            name: "main".into(),
            sha: Some(CommitSha::from_bytes([0x22; 20])),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries: vec![
                entry(
                    "film-grain-35mm.safetensors",
                    "/mnt/vault/loras/film-grain-35mm.safetensors",
                ),
                entry(
                    "loras/film-grain-35mm.safetensors",
                    "/mnt/comfy/models/loras/film-grain-35mm.safetensors",
                ),
            ],
        }],
    }])?;
    catalog.rebuild_search(|_| {})?;
    let page = search(&catalog, "role:lora")?;
    assert_eq!(page.total, 1);
    let row = &page.rows[0];
    assert_eq!(row.name, "film-grain-35mm");
    assert_eq!(row.org.as_deref(), Some("kohya"));
    assert_eq!(row.path_count, 2);
    assert!(row.dupe);
    assert_eq!(row.trigger.as_deref(), Some("film grain, 35mm, halation"));
    assert_eq!(row.revision, "main");
    assert_eq!(
        row.abs_path.as_deref(),
        Some("/mnt/vault/loras/film-grain-35mm.safetensors"),
        "the lowest entry id is preferred on a tie"
    );
    assert_eq!(catalog.disk_pressure()?.duplicate_blobs, 1);
    assert_eq!(
        search(&catalog, "grain")?.total,
        1,
        "the card is searchable text"
    );
    assert_eq!(catalog.status()?.generation, 2);
    Ok(())
}

/// The six artifacts the server world seeds, as the ids the seed handed them.
struct SeededIds {
    ckpt: ArtifactId,
    lora: ArtifactId,
    unknown: ArtifactId,
}

/// The world the detail screens read: a checkpoint repository with a card and a config, a
/// LoRA on two paths, three more single-artifact repositories, and one queued unknown.
fn seeded_catalog() -> Result<(TempDir, Catalog, SeededIds)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let ckpt = seeded_artifact(
        0x11,
        "sdxl-base-1.0",
        Some("stability"),
        Artifact {
            sha256: Some(Sha256::from_bytes([0x11; 32])),
            size: 6_938_040_000,
            role: Role::Ckpt,
            role_confidence: 0.99,
            base: Base::new("sdxl"),
            base_label: Some("sdxl".to_owned()),
            precision: Some(Precision::new("fp16")),
            ..seeded_facts()
        },
    );
    let lora = seeded_artifact(
        0x9f9f,
        "film-grain-35mm",
        Some("kohya"),
        Artifact {
            sha256: Some(Sha256::from_bytes([0x9f; 32])),
            size: 239_075_328,
            role: Role::Lora,
            role_confidence: 0.98,
            base: Base::new("sdxl 1.0"),
            base_label: Some("sdxl 1.0".to_owned()),
            ..seeded_facts()
        },
    );
    let llm = seeded_artifact(
        0x33,
        "llama-3-8b",
        Some("meta"),
        Artifact {
            size: 4_920_000_000,
            format: ArtifactFormat::Gguf,
            role: Role::Llm,
            role_confidence: 0.97,
            base: Base::new("llama3"),
            base_label: Some("llama 3".to_owned()),
            context: Some(8192),
            ..seeded_facts()
        },
    );
    let vae = seeded_artifact(
        0x66,
        "sdxl-vae",
        Some("madebyollin"),
        Artifact {
            size: 334_641_000,
            role: Role::Vae,
            role_confidence: 0.93,
            ..seeded_facts()
        },
    );
    let dataset = seeded_artifact(
        0x44,
        "squad",
        None,
        Artifact {
            size: 88_000_000,
            format: ArtifactFormat::Parquet,
            role: Role::Ds,
            role_confidence: 0.99,
            ..seeded_facts()
        },
    );
    let unknown = SeedArtifact {
        tensors: Some(queued_tensors()),
        evidence: vec![
            Evidence::new("role", "lora", EvidenceSource::Tensor, 0.6)
                .with_detail("lora_unet prefix on 2 tensors"),
            Evidence::new("base", "sdxl 1.0", EvidenceSource::Tensor, 0.5),
        ],
        ..seeded_artifact(
            0x55,
            "mystery",
            None,
            Artifact {
                size: 1_200_000,
                format: ArtifactFormat::Bin,
                role: Role::Unk,
                role_confidence: 0.2,
                ..seeded_facts()
            },
        )
    };
    let written = catalog.seed(&[
        SeedRepository {
            revisions: vec![SeedRevision {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes([0x11; 20])),
                refs: vec![
                    ("main".to_owned(), RefKind::Branch),
                    ("v1.0".to_owned(), RefKind::Tag),
                ],
                title: None,
                author: None,
                committed: None,
                entries: vec![
                    seeded_entry("model.safetensors", 6_938_040_000, Some(ckpt)),
                    seeded_entry("README.md", 84, None),
                    seeded_entry("config.json", 900, None),
                ],
            }],
            ..seeded_repo("stability/sdxl-base-1.0", RepoType::Models)
        },
        seeded_revision(
            seeded_repo("kohya/film-grain-35mm", RepoType::Models),
            0x22,
            vec![
                seeded_entry(
                    "film-grain-35mm.safetensors",
                    239_075_328,
                    Some(lora.clone()),
                ),
                seeded_entry("loras/film-grain-35mm.safetensors", 239_075_328, Some(lora)),
            ],
        ),
        seeded_revision(
            seeded_repo("meta/llama-3-8b", RepoType::Models),
            0x33,
            vec![seeded_entry("model.gguf", 4_920_000_000, Some(llm))],
        ),
        seeded_revision(
            seeded_repo("madebyollin/sdxl-vae", RepoType::Models),
            0x66,
            vec![seeded_entry("vae.safetensors", 334_641_000, Some(vae))],
        ),
        seeded_revision(
            seeded_repo("squad", RepoType::Datasets),
            0x44,
            vec![seeded_entry("train.parquet", 88_000_000, Some(dataset))],
        ),
        seeded_revision(
            seeded_repo("misc/mystery", RepoType::Models),
            0x55,
            vec![SeedEntry {
                node: LOCAL_NODE,
                state: State::Triage,
                ..seeded_entry("mystery.bin", 1_200_000, Some(unknown))
            }],
        ),
    ])?;
    catalog.rebuild_search(|_| {})?;
    let id = |identity: u128| {
        written
            .iter()
            .find(|(seeded, _)| *seeded == identity)
            .map(|(_, id)| *id)
            .ok_or_else(|| IndexError::Catalog("every seeded identity was written".into()))
    };
    let ids = SeededIds {
        ckpt: id(0x11)?,
        lora: id(0x9f9f)?,
        unknown: id(0x55)?,
    };
    Ok((directory, catalog, ids))
}

fn seeded_repo(reference: &str, repo_type: RepoType) -> SeedRepository {
    SeedRepository {
        repo: RepoId::parse_ref(repo_type, reference).expect("reference"),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: PathBuf::from("/data/hub"),
        git_dir: None,
        revisions: Vec::new(),
    }
}

fn seeded_revision(repo: SeedRepository, byte: u8, entries: Vec<SeedEntry>) -> SeedRepository {
    SeedRepository {
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([byte; 20])),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries,
        }],
        ..repo
    }
}

fn seeded_artifact(
    identity: u128,
    name: &str,
    org: Option<&str>,
    artifact: Artifact,
) -> SeedArtifact {
    SeedArtifact {
        identity,
        artifact,
        name: name.to_owned(),
        org: org.map(str::to_owned),
        tensors: None,
        evidence: Vec::new(),
        card: None,
        group_bytes: None,
        shard_count: None,
    }
}

fn seeded_entry(rel_path: &str, size: u64, artifact: Option<SeedArtifact>) -> SeedEntry {
    let root = match rel_path {
        "film-grain-35mm.safetensors" => "/mnt/vault/loras",
        "loras/film-grain-35mm.safetensors" => "/mnt/comfy/models",
        _ => "/data/hub",
    };
    SeedEntry {
        node: LOCAL_NODE,
        abs_path: Some(format!("{root}/{rel_path}")),
        rel_path: rel_path.to_owned(),
        size,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact,
    }
}

fn queued_tensors() -> TensorSummary {
    TensorSummary {
        tensor_count: 2,
        params: 1_024,
        dtypes: BTreeMap::from([("F16".to_owned(), 2_u32)]),
        key_prefixes: vec![("lora_unet".to_owned(), 2)],
        samples: vec![TensorSample {
            name: "lora_unet_down.weight".to_owned(),
            dtype: "F16".to_owned(),
            shape: vec![32, 768],
        }],
        metadata: BTreeMap::new(),
    }
}

#[test]
fn the_read_api_answers_the_inspector_the_repo_screens_and_the_queue() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let lora = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    assert_eq!(lora.path_count, 2);
    assert_eq!(
        lora.abs_path.as_deref(),
        Some("/mnt/vault/loras/film-grain-35mm.safetensors")
    );
    let entries = catalog.entries_of(ids.lora)?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].repo, "models/kohya/film-grain-35mm");
    assert_eq!(entries[0].revision, "main");
    assert!(entries.iter().all(|entry| entry.deduped && entry.present));
    let by_sha = catalog.artifact_by_sha256(&Sha256::from_bytes([0x9f; 32]))?;
    assert_eq!(by_sha.map(|row| row.artifact_id), Some(ids.lora));
    let detail = catalog
        .artifact_detail(ids.unknown)?
        .ok_or_else(|| IndexError::Catalog("unknown".into()))?;
    assert_eq!(detail.evidence[0].value, "lora");
    assert_eq!(
        detail.tensors.as_ref().map(|tensors| tensors.tensor_count),
        Some(2)
    );

    let repo = catalog
        .repository(&RepoId::parse_ref(
            RepoType::Models,
            "stability/sdxl-base-1.0",
        )?)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(repo.refs.len(), 2);
    assert_eq!(repo.refs[0].kind, RefKind::Branch);
    let main = catalog
        .resolve_revision(&repo, "main")
        .ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert!(catalog.resolve_revision(&repo, "nope").is_none());
    let files = catalog.revision_entries(repo.id, main.ordinal)?;
    assert_eq!(files.len(), 3);
    assert_eq!(catalog.repository_rows(repo.id, 10)?.len(), 1);
    assert_eq!(
        catalog.artifact_row(ids.ckpt)?.map(|row| row.repo),
        Some("models/stability/sdxl-base-1.0".to_owned())
    );
    let models = || RepositoryFilter {
        repo_type: Some(RepoType::Models),
        present_only: true,
        ..RepositoryFilter::default()
    };
    let page = catalog.repositories(&models(), None, 2)?;
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.total, 5);
    let next = catalog.repositories(&models(), Some(page.rows[1].id), 10)?;
    assert_eq!(next.rows.len(), 3, "paging continues after the cursor");

    let queue = catalog.triage_queue(10)?;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].row.artifact_id, ids.unknown);
    assert_eq!(queue[0].guess_role, Role::Lora);
    assert_eq!(queue[0].guess_base.as_deref(), Some("sdxl 1.0"));
    assert_eq!(queue[0].header.hidden_dim, Some(768));
    catalog.clear_triage(ids.unknown)?;
    assert!(catalog.triage_queue(10)?.is_empty());
    Ok(())
}

#[test]
fn loadouts_are_stored_by_name_and_pins_resolve_through_the_catalog() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let lora = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let mut document = Loadout::new("night");
    let mut pin = LoadoutPin::new(RepoId::parse_qualified(&lora.repo)?, lora.path.clone());
    pin.sha256 = lora.blob_sha256;
    document.adapters.push(pin.clone());

    let id = catalog.upsert_loadout(&document)?;
    assert_eq!(
        catalog.upsert_loadout(&document)?,
        id,
        "the same name keeps its id"
    );
    let stored = catalog
        .loadout_by_name("night")?
        .ok_or_else(|| IndexError::Catalog("night".into()))?;
    assert_eq!(stored.parse()?.len(), 1);
    catalog.update_loadout(id, &Loadout::new("night").to_toml()?)?;
    assert_eq!(
        catalog
            .loadout(id)?
            .map(|record| record.parse().map(|document| document.len()))
            .transpose()?,
        Some(0)
    );
    assert_eq!(catalog.list_loadouts()?.len(), 1);

    let by_sha = catalog.resolve_pin(&pin)?;
    assert_eq!(
        by_sha.len(),
        2,
        "both paths of the LoRA answer for its sha256"
    );
    pin.sha256 = None;
    let by_path = catalog.resolve_pin(&pin)?;
    assert_eq!(by_path.len(), 1);
    assert_eq!(by_path[0].artifact_id, ids.lora);
    assert!(catalog.delete_loadout(id)?);
    assert!(catalog.loadout(id)?.is_none());
    Ok(())
}

#[test]
fn jobs_run_settle_cancel_and_are_swept() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let scan = catalog.create_job("scan", Some("cache"))?;
    catalog.update_job(
        scan,
        &JobProgress {
            phase: Some("reading".into()),
            progress: 0.5,
            bytes: 10,
            total: Some(20),
            rate_bps: None,
        },
    )?;
    let running = catalog.list_jobs(Some(JobState::Running))?;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].phase.as_deref(), Some("reading"));

    catalog.finish_job(scan)?;
    let done = catalog
        .job(scan)?
        .ok_or_else(|| IndexError::Catalog("job".into()))?;
    assert_eq!(done.state, JobState::Done);
    assert_eq!(done.progress, 1.0);
    assert!(done.finished.is_some());
    catalog.update_job(
        scan,
        &JobProgress {
            phase: None,
            progress: 0.1,
            bytes: 0,
            total: None,
            rate_bps: None,
        },
    )?;
    assert_eq!(
        catalog.job(scan)?.map(|job| job.progress),
        Some(1.0),
        "a finished job ignores progress"
    );

    let pull = catalog.create_job("pull", Some("film-grain"))?;
    catalog.cancel_job(pull)?;
    assert_eq!(
        catalog.job(pull)?.map(|job| job.state),
        Some(JobState::Cancelled)
    );

    let stuck = catalog.create_job("apply", None)?;
    assert_eq!(catalog.sweep_running_jobs("killed")?, 1);
    assert_eq!(
        catalog.job(stuck)?.and_then(|job| job.error),
        Some("killed".into())
    );
    assert!(catalog.job(JobId::new(9_999))?.is_none());

    let newest_first: Vec<_> = catalog
        .list_jobs(None)?
        .into_iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(newest_first, vec![stuck, pull, scan]);
    Ok(())
}

#[test]
fn an_any_base_answers_only_its_own_bucket() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    let seeded = |identity: u128, name: &str, base: Base| SeedRepository {
        repo: RepoId::new(RepoType::Models, None, name),
        source: "cache".into(),
        kind: SourceKind::HfCache,
        root_path: "/data/hub".into(),
        git_dir: None,
        revisions: vec![SeedRevision {
            name: "main".into(),
            sha: None,
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries: vec![SeedEntry {
                node: LOCAL_NODE,
                rel_path: format!("{name}.safetensors"),
                abs_path: Some(format!("/data/hub/{name}.safetensors")),
                size: 1_024,
                state: State::Local,
                is_symlink: false,
                sha256: None,
                git_sha1: None,
                artifact: Some(SeedArtifact {
                    identity,
                    artifact: Artifact {
                        base_label: Some(base.as_str().to_owned()),
                        base,
                        ..seeded_facts()
                    },
                    name: name.to_owned(),
                    org: None,
                    tensors: None,
                    evidence: Vec::new(),
                    card: None,
                    group_bytes: None,
                    shard_count: None,
                }),
            }],
        }],
    };
    catalog.seed(&[
        seeded(1, "sdxl-vae", Base::any()),
        seeded(2, "sdxl-lora", Base::new("sdxl 1.0")),
    ])?;
    catalog.rebuild_search(|_| {})?;
    let family = search(&catalog, "base:sdxl")?;
    assert_eq!(family.total, 1);
    assert_eq!(family.rows[0].name, "sdxl-lora", "an any base is no family");
    let any = search(&catalog, "base:any")?;
    assert_eq!(any.total, 1);
    assert_eq!(any.rows[0].name, "sdxl-vae");
    let bucket = search(&catalog, "")?
        .facets
        .base
        .into_iter()
        .find(|bucket| bucket.value == "any")
        .ok_or_else(|| IndexError::Catalog("the rail offers an any bucket".into()))?;
    assert_eq!(bucket.count, 1, "the bucket counts the query it opens");
    Ok(())
}

fn seeded_facts() -> Artifact {
    Artifact {
        id: ArtifactId::new(0),
        blob: BlobId::new(0),
        sha256: None,
        size: 1_024,
        format: ArtifactFormat::Safetensors,
        role: Role::Lora,
        sub_role: None,
        role_confidence: 0.99,
        base: Base::any(),
        base_label: None,
        base_confidence: 0.9,
        precision: None,
        quant: None,
        rank: None,
        alpha: None,
        params: None,
        context: None,
        trigger: None,
        sniff_version: 1,
    }
}

#[test]
fn a_served_file_is_filed_under_the_mirror_source_and_found_by_hash() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(
        &IndexConfig {
            data_dir: directory.path().join("native"),
            sources: Vec::new(),
            ..IndexConfig::default()
        },
        "n",
        |_, _| {},
    )?;
    let repo = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    let sha = Sha256::from_bytes([0x42; 32]);
    let file = ServedFile {
        repo: repo.clone(),
        commit: CommitSha::from_bytes([0x11; 20]),
        rel_path: "unet/model.safetensors".to_owned(),
        size: 4096,
        sha256: sha,
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    };
    let first = catalog.ingest_served(&file)?;
    assert!(first.artifact.is_some());
    let again = catalog.ingest_served(&file)?;
    assert_eq!(again, first, "the same file lands on the same rows");
    let detail = catalog
        .repository(&repo)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(detail.source, MIRROR_SOURCE);
    assert_eq!(detail.revisions.len(), 1);
    let entries = catalog.revision_entries(detail.id, 0)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Served);
    assert_eq!(entries[0].sha256, Some(sha));
    assert!(!entries[0].deduped, "one fetch is one path");
    let located = catalog.entries_by_sha256(&[sha])?;
    assert_eq!(located.len(), 1);
    assert_eq!(located[0].1.rel_path, "unet/model.safetensors");
    catalog.refresh_search(|_| {})?;
    let row = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("row".into()))?;
    assert_eq!(
        row.artifact_id,
        first
            .artifact
            .ok_or_else(|| IndexError::Catalog("artifact".into()))?
    );
    assert_eq!(row.state, State::Served);
    let config = ServedFile {
        rel_path: "config.json".to_owned(),
        sha256: Sha256::from_bytes([0x43; 32]),
        git_sha1: Some(CommitSha::from_bytes([0x44; 20])),
        ..file
    };
    assert!(
        catalog.ingest_served(&config)?.artifact.is_none(),
        "a config is not an artifact"
    );
    assert_eq!(catalog.totals()?.artifacts, 1);
    assert_eq!(catalog.revision_entries(detail.id, 0)?.len(), 2);
    Ok(())
}

#[test]
fn a_rescan_leaves_the_mirror_source_alone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let repo = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    catalog.ingest_served(&ServedFile {
        repo: repo.clone(),
        commit: CommitSha::from_bytes([0x11; 20]),
        rel_path: "model.safetensors".to_owned(),
        size: 10,
        sha256: Sha256::from_bytes([0x42; 32]),
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    })?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let detail = catalog
        .repository(&repo)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(
        catalog.revision_entries(detail.id, 0)?[0].state,
        State::Served
    );
    Ok(())
}

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "hoard")
        .env("GIT_AUTHOR_EMAIL", "hoard@example.invalid")
        .env("GIT_COMMITTER_NAME", "hoard")
        .env("GIT_COMMITTER_EMAIL", "hoard@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One clone at `<input>/org/clone` holding a README committed as `first import`.
fn git_fixture(directory: &Path) -> Result<(PathBuf, IndexConfig)> {
    let input = directory.join("clones");
    let clone = input.join("org").join("clone");
    fs::create_dir_all(&clone)?;
    git(&clone, &["init", "--initial-branch=main", "--quiet"]);
    fs::write(clone.join("README.md"), "# clone\n")?;
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "first import", "--quiet"]);
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::Git,
            ..source(&input, "clones")
        }],
        ..IndexConfig::default()
    };
    Ok((input, config))
}

#[test]
fn a_git_clone_keeps_its_git_dir_and_commit_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = git_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let repo = RepoId::parse_ref(RepoType::Models, "org/clone")?;
    let detail = catalog
        .repository(&repo)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(
        detail.git_dir.as_deref(),
        Some(input.join("org/clone/.git").to_string_lossy().as_ref())
    );
    let main = catalog
        .resolve_revision(&detail, "main")
        .ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert_eq!(main.title.as_deref(), Some("first import"));
    assert_eq!(main.author.as_deref(), Some("hoard"));
    assert!(main.committed.is_some());
    let entry = catalog
        .revision_entry(detail.id, main.ordinal, "README.md")?
        .ok_or_else(|| IndexError::Catalog("readme".into()))?;
    assert_eq!(entry.rel_path, "README.md");
    assert!(
        catalog
            .revision_entry(detail.id, main.ordinal, "missing.bin")?
            .is_none()
    );
    Ok(())
}

#[test]
fn a_version_2_repository_is_migrated_on_open() -> Result<()> {
    let legacy = tempfile::tempdir()?;
    let repo = RepoId::new(RepoType::Models, Some("acme".to_owned()), "styler");
    {
        let database = builder().create(models()?, legacy.path().join("catalog.redb"))?;
        let transaction = database.rw_transaction()?;
        transaction.insert(ManifestV1 {
            id: 0,
            format_version: 5,
            generation: 1,
            scanned_at: "then".to_owned(),
            node: "old".to_owned(),
            counts: Vec::new(),
            search_directory: None,
            search_generation: 0,
        })?;
        transaction.insert(Source {
            name: "hub".to_owned(),
            config: SourceConfig {
                name: "hub".to_owned(),
                ..frozen_source_config()
            },
            resolved_kind: SourceKind::Git,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        })?;
        transaction.insert(RepositoryV2 {
            id: 11,
            key: models::repository_key("hub", &repo),
            source: "hub".to_owned(),
            repo: repo.clone(),
            root_path: "/hub".to_owned(),
            default_ref: "main".to_owned(),
            revisions: vec![RevisionV2 {
                name: "main".to_owned(),
                sha: Some(CommitSha::from_bytes([7; 20])),
                refs: vec![Ref {
                    name: "main".to_owned(),
                    kind: RefKind::Branch,
                }],
            }],
        })?;
        transaction.commit()?;
    }
    let reopened = Catalog::open(legacy.path())?;
    let detail = reopened
        .repository(&repo)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(detail.git_dir, None, "a v2 repository named no clone");
    assert_eq!(detail.revisions.len(), 1);
    assert_eq!(detail.revisions[0].title, None);
    assert_eq!(detail.revisions[0].author, None);
    assert_eq!(detail.revisions[0].committed, None);
    assert_eq!(
        detail.revisions[0].sha,
        Some(CommitSha::from_bytes([7; 20]))
    );
    let read = reopened.database.r_transaction()?;
    let old: Option<RepositoryV2> = read.get().primary(11_i64)?;
    assert!(old.is_none(), "the v2 table is drained");
    Ok(())
}

#[test]
fn repository_counts_follow_every_writer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let scanned_bytes = fs::metadata(input.join("loras/keep.safetensors"))?.len()
        + fs::metadata(input.join("loras/gone.safetensors"))?.len();
    let walked = only_repository(&catalog)?;
    assert_eq!(walked.present_files, 2);
    assert_eq!(walked.present_bytes, scanned_bytes);
    let kept_bytes = fs::metadata(input.join("loras/keep.safetensors"))?.len();

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let rescanned = only_repository(&catalog)?;
    assert_eq!(rescanned.present_files, 1, "an absent path holds no bytes");
    assert_eq!(rescanned.present_bytes, kept_bytes);

    let mirrored = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    let record = catalog.ingest_served(&ServedFile {
        repo: mirrored.clone(),
        commit: CommitSha::from_bytes([0x11; 20]),
        rel_path: "model.safetensors".to_owned(),
        size: 4096,
        sha256: Sha256::from_bytes([0x42; 32]),
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    })?;
    let served = repository_of(&catalog, &mirrored)?;
    assert_eq!(served.present_files, 1);
    assert_eq!(served.present_bytes, 4096);

    assert_eq!(catalog.retire_entries(&[record.entry])?, 1);
    let retired = repository_of(&catalog, &mirrored)?;
    assert_eq!(retired.present_files, 0);
    assert_eq!(retired.present_bytes, 0);

    let laptop = catalog.upsert_peer(&NodeId::new("laptop-1")?, "laptop", None)?;
    let pass = catalog.begin_pass(laptop.id)?;
    catalog.apply_digest(
        laptop.id,
        &[remote_entry(
            Sha256::from_bytes([0x77; 32]),
            "loras/only-there.safetensors",
        )],
        pass,
    )?;
    let advertised = catalog
        .repositories(&RepositoryFilter::default(), None, 50)?
        .rows
        .into_iter()
        .filter(|row| row.node == "laptop")
        .collect::<Vec<_>>();
    assert_eq!(advertised.len(), 1);
    assert_eq!(
        (advertised[0].present_files, advertised[0].present_bytes),
        (0, 0),
        "a peer's rows are bytes this node does not hold"
    );
    assert_eq!(
        rescanned.present_files, 1,
        "the walked repository is intact"
    );
    Ok(())
}

#[test]
fn a_migrated_repository_recounts_its_presence() -> Result<()> {
    let legacy = tempfile::tempdir()?;
    let repo = RepoId::new(RepoType::Models, Some("acme".to_owned()), "styler");
    {
        let database = builder().create(models()?, legacy.path().join("catalog.redb"))?;
        let transaction = database.rw_transaction()?;
        transaction.insert(Manifest {
            id: 0,
            format_version: 5,
            generation: 1,
            scanned_at: "then".to_owned(),
            node: "old".to_owned(),
            counts: Vec::new(),
            search_directory: None,
            search_generation: 0,
            search_seq: 0,
        })?;
        transaction.insert(Source {
            name: "hub".to_owned(),
            config: SourceConfig {
                name: "hub".to_owned(),
                ..frozen_source_config()
            },
            resolved_kind: SourceKind::Git,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        })?;
        transaction.insert(RepositoryV3 {
            id: 11,
            key: models::repository_key("hub", &repo),
            source: "hub".to_owned(),
            repo: repo.clone(),
            root_path: "/hub".to_owned(),
            git_dir: None,
            default_ref: "main".to_owned(),
            revisions: frozen_revisions(),
        })?;
        for (id, size, state) in [(1_i64, 100_u64, State::Local), (2, 500, State::Remote)] {
            transaction.insert(legacy_entry(id, 11, size, state))?;
        }
        transaction.commit()?;
    }
    let reopened = Catalog::open(legacy.path())?;
    let detail = repository_of(&reopened, &repo)?;
    assert_eq!(detail.present_files, 1, "a remote path holds no bytes here");
    assert_eq!(detail.present_bytes, 100);
    Ok(())
}

fn legacy_entry(id: i64, repo_id: i64, size: u64, state: State) -> Entry {
    Entry {
        id,
        repo_id,
        artifact_id: None,
        revision: 0,
        rel_path: format!("w{id}.safetensors"),
        abs_path: Some(format!("/hub/w{id}.safetensors")),
        size,
        state,
        triage: false,
        preference: 0,
        lfs_pointer: false,
        is_symlink: false,
        sha256: None,
        content: None,
        git_sha1: None,
        mtime: None,
        inode: None,
        node: LOCAL_NODE,
        pass: 0,
        pull_progress: None,
    }
}

/// How many artifacts the stale check must never read to answer.
const LOGGED_CHANGES: usize = 500;

#[test]
fn a_stale_check_reads_one_change() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..LOGGED_CHANGES {
        fs::write(
            input.join(format!("w{index}.safetensors")),
            lora_header_named(&format!("t{index}")),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(!search(&catalog, "")?.stale);

    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    let ids: Vec<EntryId> = entries.iter().map(|entry| EntryId::new(entry.id)).collect();
    assert_eq!(ids.len(), LOGGED_CHANGES);
    assert_eq!(catalog.retire_entries(&ids)?, LOGGED_CHANGES);
    let logged = catalog.pending_changes()?;
    assert!(
        logged >= LOGGED_CHANGES as u64,
        "a retired path is a logged change, got {logged}"
    );
    assert!(search(&catalog, "")?.stale, "the index is behind the log");

    catalog.refresh_search(|_| {})?;
    assert!(
        !search(&catalog, "")?.stale,
        "the refresh caught the index up"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<SearchChange>()?,
        0,
        "a refresh prunes everything at or below its cursor"
    );
    drop(read);
    assert_eq!(catalog.pending_changes()?, 0);
    Ok(())
}

#[test]
fn pending_hashes_are_counted_without_a_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..3 {
        fs::write(
            input.join(format!("w{index}.safetensors")),
            lora_header_named(&format!("t{index}")),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(catalog.sources()?[0].pending_hashes, 3);
    assert_eq!(catalog.pending_hashes()?, 3);

    catalog.hash_pending(&config, |_| {})?;
    assert_eq!(catalog.sources()?[0].pending_hashes, 0);
    assert_eq!(catalog.pending_hashes()?, 0);

    let touched = input.join("w1.safetensors");
    fs::write(&touched, lora_header_named("t1-again"))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(catalog.sources()?[0].pending_hashes, 1);
    assert_eq!(catalog.pending_hashes()?, 1);
    Ok(())
}

fn only_repository(catalog: &Catalog) -> Result<RepositoryDetail> {
    let page = catalog.repositories(&RepositoryFilter::default(), None, 50)?;
    match page.rows.len() {
        1 => page
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::Catalog("row".into())),
        found => Err(IndexError::Catalog(format!("{found} repositories"))),
    }
}

fn repository_of(catalog: &Catalog, repo: &RepoId) -> Result<RepositoryDetail> {
    catalog
        .repository(repo)?
        .ok_or_else(|| IndexError::Catalog(format!("no repository {}", repo.qualified())))
}

fn loose_fixture(directory: &Path) -> Result<(PathBuf, IndexConfig)> {
    let input = directory.join("models");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/keep.safetensors"), lora_header())?;
    fs::write(input.join("loras/gone.safetensors"), plain_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    Ok((input, config))
}

#[test]
fn totals_follow_the_sweep() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let totals = catalog.totals()?;
    assert_eq!(totals.artifacts, 2);
    assert_eq!(totals.present_artifacts, 2);
    assert_eq!(totals.repositories, 1);
    assert_eq!(
        totals.indexed_bytes,
        fs::metadata(input.join("loras/keep.safetensors"))?.len()
            + fs::metadata(input.join("loras/gone.safetensors"))?.len()
    );
    fs::remove_file(input.join("loras/gone.safetensors"))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let after = catalog.totals()?;
    assert_eq!(after.artifacts, 1, "the orphan was removed");
    assert_eq!(after.present_artifacts, 1);
    Ok(())
}

#[test]
fn a_source_remembers_what_its_last_scan_found() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let sources = catalog.sources()?;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].file_count, 2);
    assert_eq!(
        sources[0].byte_count,
        fs::metadata(input.join("loras/keep.safetensors"))?.len()
            + fs::metadata(input.join("loras/gone.safetensors"))?.len()
    );
    assert!(sources[0].last_scan.is_some());
    fs::remove_file(input.join("loras/gone.safetensors"))?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        catalog.sources()?[0].file_count,
        1,
        "absent rows are not counted"
    );
    Ok(())
}

#[test]
fn an_unchanged_rescan_reads_no_headers_and_a_deleted_file_is_marked_absent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    let first = catalog.status()?;
    assert_eq!(first.generation, 1);

    let unchanged = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        unchanged,
        RescanReport {
            unchanged: 2,
            changed: 0,
            added: 0,
            absent: 0
        }
    );
    assert_eq!(catalog.status()?.generation, 2);
    assert!(!catalog.status()?.search_ready, "a rescan needs a rebuild");

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    let retracted = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        retracted,
        RescanReport {
            unchanged: 1,
            changed: 0,
            added: 0,
            absent: 1
        }
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let gone = entries
        .iter()
        .find(|entry| entry.rel_path.ends_with("gone.safetensors"))
        .ok_or_else(|| IndexError::Catalog("the deleted path keeps its row".into()))?;
    assert_eq!(gone.state, hoard_core::State::Remote);
    assert!(gone.abs_path.is_some(), "the path it had is kept");
    assert!(
        gone.artifact_id.is_none(),
        "its artifact had no other path and is gone"
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    drop(read);
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    assert!(rows.rows[0].path.ends_with("keep.safetensors"));
    Ok(())
}

#[test]
fn a_changed_file_is_reread_and_a_new_file_continues_the_ids() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    let before: Vec<Entry> = {
        let read = catalog.database.r_transaction()?;
        read.scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?
    };
    let max_id = before.iter().map(|entry| entry.id).max().unwrap_or(0);

    let changed_path = input.join("loras/gone.safetensors");
    fs::write(&changed_path, lora_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&changed_path)?
        .set_modified(later)?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;

    let report = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        report,
        RescanReport {
            unchanged: 1,
            changed: 1,
            added: 1,
            absent: 0
        }
    );
    let read = catalog.database.r_transaction()?;
    let after: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(after.len(), 3);
    let new = after
        .iter()
        .find(|entry| entry.rel_path.ends_with("new.safetensors"))
        .ok_or_else(|| IndexError::Catalog("new file missing".into()))?;
    assert!(new.id > max_id, "ids continue from the allocator");
    let changed = after
        .iter()
        .find(|entry| entry.rel_path.ends_with("gone.safetensors"))
        .ok_or_else(|| IndexError::Catalog("changed file missing".into()))?;
    let artifact: ArtifactRecord = read
        .get()
        .primary(
            changed
                .artifact_id
                .ok_or_else(|| IndexError::Catalog("no artifact".into()))?,
        )?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(
        artifact.artifact.role,
        hoard_core::Role::Lora,
        "the header was read again"
    );
    assert_eq!(
        read.len().primary::<ArtifactRecord>()?,
        3,
        "the changed file's artifact was rewritten in place, not duplicated"
    );
    let repositories: Vec<Repository> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(repositories.len(), 1, "the repository keeps its id");
    Ok(())
}

fn frozen_encoding<T: native_model::Model>(value: &T) -> Result<String> {
    native_model::encode(value)
        .map(hex::encode)
        .map_err(|error| IndexError::Catalog(error.to_string()))
}

fn frozen_source_config() -> SourceConfig {
    SourceConfig {
        name: "frozen".to_owned(),
        path: PathBuf::from("/frozen/source"),
        kind: SourceKind::HfCache,
        include: vec!["**/*.safetensors".to_owned()],
        exclude: vec!["**/*.tmp".to_owned()],
        follow_symlinks: true,
        repo_per_folder: false,
    }
}

fn frozen_artifact() -> Artifact {
    Artifact {
        id: ArtifactId::new(7),
        blob: BlobId::new(9),
        sha256: Some(Sha256::from_bytes([3; 32])),
        size: 4096,
        format: ArtifactFormat::Gguf,
        role: Role::Lora,
        sub_role: Some(SubRole::Ipa),
        role_confidence: 0.875,
        base: Base::new("sdxl 1.0"),
        base_label: Some("SDXL 1.0".to_owned()),
        base_confidence: 0.625,
        precision: Some(Precision::new("fp16")),
        quant: Some("q4_k_m".to_owned()),
        rank: Some(16),
        alpha: Some(8.0),
        params: Some(1_234_567),
        context: Some(8192),
        trigger: Some("frozen style".to_owned()),
        sniff_version: 3,
    }
}

#[test]
fn source_encoding_is_frozen() -> Result<()> {
    let value = Source {
        name: "frozen".to_owned(),
        config: frozen_source_config(),
        resolved_kind: SourceKind::Git,
        file_count: 12,
        byte_count: 4096,
        last_scan: Some("2026-09-04T09:10:00Z".to_owned()),
        pending_hashes: 5,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn source_v2_encoding_is_frozen() -> Result<()> {
    let value = SourceV2 {
        name: "frozen".to_owned(),
        config: frozen_source_config(),
        resolved_kind: SourceKind::Git,
        file_count: 12,
        byte_count: 4096,
        last_scan: Some("2026-09-04T09:10:00Z".to_owned()),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn source_v1_encoding_is_frozen() -> Result<()> {
    let value = SourceV1 {
        name: "frozen".to_owned(),
        config: frozen_source_config(),
        resolved_kind: SourceKind::Git,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

fn frozen_revisions() -> Vec<Revision> {
    vec![Revision {
        name: "main".to_owned(),
        sha: Some(CommitSha::from_bytes([5; 20])),
        refs: vec![Ref {
            name: "refs/heads/main".to_owned(),
            kind: RefKind::Tag,
        }],
        title: Some("frozen import".to_owned()),
        author: Some("frozen-org".to_owned()),
        committed: Some("2026-01-17T10:22:31Z".to_owned()),
    }]
}

#[test]
fn repository_encoding_is_frozen() -> Result<()> {
    let repo = RepoId::new(RepoType::Datasets, Some("frozen-org".to_owned()), "frozen");
    let value = Repository {
        id: 11,
        key: models::repository_key("frozen", &repo),
        source: "frozen".to_owned(),
        repo,
        root_path: "/frozen/source/datasets--frozen-org--frozen".to_owned(),
        git_dir: Some("/frozen/source/datasets--frozen-org--frozen/.git".to_owned()),
        default_ref: "main".to_owned(),
        revisions: frozen_revisions(),
        present_files: 3,
        present_bytes: 12_288,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn repository_v3_encoding_is_frozen() -> Result<()> {
    let repo = RepoId::new(RepoType::Datasets, Some("frozen-org".to_owned()), "frozen");
    let value = RepositoryV3 {
        id: 11,
        key: models::repository_key("frozen", &repo),
        source: "frozen".to_owned(),
        repo,
        root_path: "/frozen/source/datasets--frozen-org--frozen".to_owned(),
        git_dir: Some("/frozen/source/datasets--frozen-org--frozen/.git".to_owned()),
        default_ref: "main".to_owned(),
        revisions: frozen_revisions(),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn repository_v2_encoding_is_frozen() -> Result<()> {
    let repo = RepoId::new(RepoType::Datasets, Some("frozen-org".to_owned()), "frozen");
    let value = RepositoryV2 {
        id: 11,
        key: models::repository_key("frozen", &repo),
        source: "frozen".to_owned(),
        repo,
        root_path: "/frozen/source/datasets--frozen-org--frozen".to_owned(),
        default_ref: "main".to_owned(),
        revisions: vec![RevisionV2 {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([5; 20])),
            refs: vec![Ref {
                name: "refs/heads/main".to_owned(),
                kind: RefKind::Tag,
            }],
        }],
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn repository_v1_encoding_is_frozen() -> Result<()> {
    let repo = RepoId::new(RepoType::Datasets, Some("frozen-org".to_owned()), "frozen");
    let value = RepositoryV1 {
        id: 11,
        key: models::repository_key("frozen", &repo),
        source: "frozen".to_owned(),
        repo,
        root_path: "/frozen/source/datasets--frozen-org--frozen".to_owned(),
        default_ref: "main".to_owned(),
        revisions: vec![RevisionV1 {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([5; 20])),
            refs: vec!["refs/heads/main".to_owned()],
        }],
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn artifact_record_encoding_is_frozen() -> Result<()> {
    let value = ArtifactRecord {
        id: 7,
        identity: 0x0303_0303_0303_0303_u128,
        artifact: frozen_artifact(),
        name: "frozen.safetensors".to_owned(),
        org: Some("frozen-org".to_owned()),
        path_count: 2,
        group_bytes: Some(8192),
        shard_count: Some(3),
        triage_cleared: true,
        first_seen: "2026-09-05T00:00:00Z".to_owned(),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn artifact_record_v1_encoding_is_frozen() -> Result<()> {
    let value = ArtifactRecordV1 {
        id: 7,
        identity: 0x0303_0303_0303_0303_u128,
        artifact: frozen_artifact(),
        name: "frozen.safetensors".to_owned(),
        org: Some("frozen-org".to_owned()),
        path_count: 2,
        group_bytes: Some(8192),
        shard_count: Some(3),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn override_encoding_is_frozen() -> Result<()> {
    let value = Override {
        artifact_id: 7,
        role: Some(Role::Ctrl),
        sub_role: Some(SubRole::Mot),
        base: Some(Base::new("sdxl 1.0")),
        decided_at: "2026-09-05T00:00:00Z".to_owned(),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn artifact_detail_encoding_is_frozen() -> Result<()> {
    let value = ArtifactDetail {
        artifact_id: 7,
        tensors: Some(TensorSummary {
            tensor_count: 2,
            params: 1_234_567,
            dtypes: BTreeMap::from([("F16".to_owned(), 2_u32)]),
            key_prefixes: vec![("lora_unet".to_owned(), 2_u32)],
            samples: vec![TensorSample {
                name: "lora_unet_down.lora_down.weight".to_owned(),
                dtype: "F16".to_owned(),
                shape: vec![16, 768],
            }],
            metadata: BTreeMap::from([("ss_network_dim".to_owned(), "16".to_owned())]),
        }),
        evidence: vec![
            Evidence::new("role", "lora", EvidenceSource::Tensor, 0.95)
                .with_detail("lora_unet_ prefix on 264 tensors"),
        ],
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn entry_encoding_is_frozen() -> Result<()> {
    let value = Entry {
        id: 13,
        repo_id: 11,
        artifact_id: Some(7),
        revision: 1,
        rel_path: "unet/frozen.safetensors".to_owned(),
        abs_path: Some("/frozen/source/unet/frozen.safetensors".to_owned()),
        size: 4096,
        state: State::Triage,
        triage: true,
        preference: 19,
        lfs_pointer: true,
        is_symlink: true,
        sha256: Some(Sha256::from_bytes([3; 32])),
        content: Some(0x0303_0303_0303_0303_u128),
        git_sha1: Some(CommitSha::from_bytes([7; 20])),
        mtime: Some(1_767_225_600),
        inode: Some(424_242),
        node: 1,
        pass: 4,
        pull_progress: Some(0.5),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn entry_v1_encoding_is_frozen() -> Result<()> {
    let value = EntryV1 {
        id: 13,
        repo_id: 11,
        artifact_id: Some(7),
        revision: 1,
        rel_path: "unet/frozen.safetensors".to_owned(),
        abs_path: Some("/frozen/source/unet/frozen.safetensors".to_owned()),
        size: 4096,
        state: State::Triage,
        triage: true,
        preference: 19,
        lfs_pointer: true,
        is_symlink: true,
        git_sha1: Some(CommitSha::from_bytes([7; 20])),
        mtime: Some(1_767_225_600),
        inode: Some(424_242),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn search_text_encoding_is_frozen() -> Result<()> {
    let value = SearchText {
        artifact_id: 7,
        keys: "lora_unet lora_te".to_owned(),
        card: "# Frozen\n\nA synthetic card.\n".to_owned(),
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn manifest_encoding_is_frozen() -> Result<()> {
    let value = Manifest {
        id: 0,
        format_version: 2,
        generation: 5,
        scanned_at: "2026-09-05T00:00:00Z".to_owned(),
        node: "frozen-node".to_owned(),
        counts: vec![("entries".to_owned(), 13)],
        search_directory: Some("search-v1-1767225600000000000".to_owned()),
        search_generation: 5,
        search_seq: 9,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn node_encoding_is_frozen() -> Result<()> {
    let value = Node {
        id: 2,
        node_id: "laptop-1".to_owned(),
        name: "laptop".to_owned(),
        url: Some("http://laptop:7099".to_owned()),
        is_self: false,
        online: true,
        indexed_bytes: 65_536,
        last_seen: Some("2026-09-06T10:00:00Z".to_owned()),
        cursor: Some("d1-2a".to_owned()),
        synced: Some("2026-09-06T10:00:00Z".to_owned()),
        pass: 3,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn a_pull_is_marked_while_moving_and_filed_when_verified() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(
        &IndexConfig {
            data_dir: directory.path().join("native"),
            ..IndexConfig::default()
        },
        "workstation",
        |_, _| {},
    )?;
    let repo = RepoId::parse_ref(RepoType::Models, "kohya/film-grain")?;
    let root = directory.path().join("pulled");
    let slot = catalog.pull_slot(&repo, CommitSha::from_bytes([0x22; 20]), &root)?;
    assert_eq!(
        catalog.pull_slot(&repo, CommitSha::from_bytes([0x22; 20]), &root)?,
        slot
    );
    let destination = root.join("loras/film-grain.safetensors");
    let marked = catalog.mark_pull(&PullMark {
        slot,
        rel_path: "loras/film-grain.safetensors",
        abs_path: &destination,
        size: 96,
        state: State::Pulling,
        progress: Some(0.5),
    })?;
    let entries = catalog.revision_entries(slot.repo, slot.revision)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Pulling);
    assert_eq!(entries[0].pull_progress, Some(0.5));
    assert_eq!(catalog.reset_stale_pulls()?, 1);
    assert_eq!(
        catalog.revision_entries(slot.repo, slot.revision)?[0].state,
        State::Remote
    );
    fs::create_dir_all(
        destination
            .parent()
            .ok_or_else(|| IndexError::Catalog("parent".into()))?,
    )?;
    fs::write(&destination, lora_header())?;
    let sniff = crate::sniff::sniff_path(&destination)?;
    let mut input = crate::infer::InferInput::new(
        sniff.format,
        &destination,
        "film-grain.safetensors",
        sniff.size,
    );
    if let Some(tensors) = &sniff.tensors {
        input = input.with_tensors(tensors);
    }
    let inference = crate::infer::infer(&input);
    let filed = catalog.file_pulled(&PulledFile {
        slot,
        rel_path: "loras/film-grain.safetensors",
        abs_path: &destination,
        sha256: Sha256::from_bytes([0x42; 32]),
        git_sha1: None,
        sniff: &sniff,
        inference: &inference,
        served: true,
        triage: false,
    })?;
    assert_eq!(filed.entry, marked, "the marked row is the filed row");
    let entries = catalog.revision_entries(slot.repo, slot.revision)?;
    assert_eq!(entries[0].state, State::Served);
    assert_eq!(entries[0].sha256, Some(Sha256::from_bytes([0x42; 32])));
    assert_eq!(entries[0].pull_progress, None);
    catalog.rebuild_search(|_| {})?;
    let row = catalog
        .artifact_by_sha256(&Sha256::from_bytes([0x42; 32]))?
        .ok_or_else(|| IndexError::Catalog("row".into()))?;
    assert_eq!(row.role, Role::Lora);
    assert_eq!(row.state, State::Served);
    assert_eq!(
        catalog.digest(None, 10)?.total,
        1,
        "a pulled artifact is offered onward"
    );
    Ok(())
}

#[test]
fn a_digest_pages_hashed_local_artifacts_and_a_peers_rows_land_as_remote() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(&input)?;
    for index in 0..3 {
        fs::write(
            input.join(format!("w{index}.safetensors")),
            lora_header_named(&format!("t{index}")),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "workstation",
        &config,
        HashPolicy::Now,
        |_, _| {},
    )?;
    let first = catalog.digest(None, 2)?;
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.total, 3);
    let second = catalog.digest(first.next, 2)?;
    assert_eq!(second.rows.len(), 1);
    assert!(
        second
            .rows
            .iter()
            .all(|row| !first.rows.iter().any(|seen| seen.sha256 == row.sha256))
    );
    assert!(catalog.digest(second.next, 2)?.rows.is_empty());
    let laptop = catalog.upsert_peer(
        &NodeId::new("laptop-1")?,
        "laptop",
        Some("http://laptop:7099"),
    )?;
    let pass = catalog.begin_pass(laptop.id)?;
    let known = first.rows[0].sha256;
    let rows = vec![
        remote_entry(known, "loras/shared.safetensors"),
        remote_entry(
            Sha256::from_bytes([0x77; 32]),
            "loras/only-there.safetensors",
        ),
    ];
    let applied = catalog.apply_digest(laptop.id, &rows, pass)?;
    assert_eq!(
        applied,
        DigestApplied {
            entries: 2,
            artifacts: 1
        },
        "the shared hash reuses this node's record"
    );
    assert_eq!(catalog.filed_for(laptop.id)?, 2);
    catalog.rebuild_search(|_| {})?;
    let remote = search(&catalog, "node:laptop")?;
    assert_eq!(remote.total, 2);
    assert!(
        remote
            .rows
            .iter()
            .all(|row| row.state == State::Remote && row.node == "laptop")
    );
    let shared = search(&catalog, &format!("hash:{}", known.to_hex()))?;
    assert_eq!(shared.total, 1);
    assert_eq!(
        shared.rows[0].node, "workstation",
        "the local copy stays the preferred row"
    );
    assert_eq!(catalog.entries_of(shared.rows[0].artifact_id)?.len(), 2);
    let next_pass = catalog.begin_pass(laptop.id)?;
    catalog.apply_digest(laptop.id, &rows[..1], next_pass)?;
    assert_eq!(catalog.retract_digest(laptop.id, next_pass)?, 1);
    assert_eq!(catalog.filed_for(laptop.id)?, 1);
    catalog.refresh_search(|_| {})?;
    assert_eq!(search(&catalog, "node:laptop")?.total, 1);
    assert!(
        catalog
            .artifact_by_sha256(&Sha256::from_bytes([0x77; 32]))?
            .is_none(),
        "an artifact only the peer held is gone with its row"
    );
    assert_eq!(
        catalog.digest(None, 10)?.total,
        3,
        "remote rows are never offered"
    );
    catalog.write_peer_cursor(laptop.id, Some("d1-2a"), jiff::Timestamp::now())?;
    catalog.write_peer_cursor(laptop.id, None, jiff::Timestamp::now())?;
    assert_eq!(catalog.peer_cursor(laptop.id)?.as_deref(), Some("d1-2a"));
    Ok(())
}

#[test]
fn nodes_are_recorded_and_entries_name_their_node() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(
        &IndexConfig {
            data_dir: directory.path().join("native"),
            ..IndexConfig::default()
        },
        "workstation",
        |_, _| {},
    )?;
    let listed = catalog.list_nodes()?;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_self);
    assert_eq!(listed[0].id, LOCAL_NODE);
    let this = catalog.ensure_local_node(&NodeId::new("workstation-1")?, "rig")?;
    assert_eq!(this.node_id.as_str(), "workstation-1");
    assert_eq!(catalog.status()?.node, "rig");
    let laptop = catalog.upsert_peer(
        &NodeId::new("laptop-1")?,
        "laptop",
        Some("http://laptop:7099"),
    )?;
    assert_eq!(laptop.id, 2);
    let renamed = catalog.upsert_peer(&NodeId::new("laptop-1")?, "laptop-2", None)?;
    assert_eq!(renamed.id, 2);
    assert_eq!(
        renamed.url.as_deref(),
        Some("http://laptop:7099"),
        "an absent address keeps the old one"
    );
    let at: jiff::Timestamp = "2026-09-06T10:00:00Z"
        .parse()
        .map_err(|_| IndexError::Catalog("timestamp".into()))?;
    catalog.touch_node(2, at, 512)?;
    let peer = catalog
        .node(2)?
        .ok_or_else(|| IndexError::Catalog("peer".into()))?;
    assert!(peer.online);
    assert_eq!(peer.indexed_bytes, 512);
    catalog.set_node_online(2, false)?;
    assert!(
        !catalog
            .node_by_name("laptop-2")?
            .ok_or_else(|| IndexError::Catalog("peer".into()))?
            .online
    );
    assert_eq!(
        catalog
            .node_by_id(&NodeId::new("laptop-1")?)?
            .ok_or_else(|| IndexError::Catalog("peer".into()))?
            .id,
        2
    );
    assert_eq!(
        catalog
            .list_nodes()?
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        ["rig", "laptop-2"]
    );
    Ok(())
}

#[test]
fn summary_encoding_is_frozen() -> Result<()> {
    let value = Summary {
        id: 0,
        reclaimable_bytes: 4096,
        duplicate_artifacts: 3,
        artifacts: 11,
        present_artifacts: 9,
        triage_artifacts: 2,
        indexed_bytes: 65_536,
        repositories: 4,
        advertised_artifacts: 7,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn summary_v2_encoding_is_frozen() -> Result<()> {
    let value = SummaryV2 {
        id: 0,
        reclaimable_bytes: 4096,
        duplicate_artifacts: 3,
        artifacts: 11,
        present_artifacts: 9,
        triage_artifacts: 2,
        indexed_bytes: 65_536,
        repositories: 4,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn summary_v1_encoding_is_frozen() -> Result<()> {
    let value = SummaryV1 {
        id: 0,
        reclaimable_bytes: 4096,
        duplicate_artifacts: 3,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

#[test]
fn manifest_v1_encoding_is_frozen() -> Result<()> {
    let value = ManifestV1 {
        id: 0,
        format_version: 2,
        generation: 5,
        scanned_at: "2026-09-05T00:00:00Z".to_owned(),
        node: "frozen-node".to_owned(),
        counts: vec![("entries".to_owned(), 13)],
        search_directory: Some("search-v1-1767225600000000000".to_owned()),
        search_generation: 5,
    };
    insta::assert_snapshot!(frozen_encoding(&value)?);
    Ok(())
}

fn shard_header(keys: &[&str], width: u64) -> Vec<u8> {
    let bytes = width * width * 2;
    let entries: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let start = index as u64 * bytes;
            format!(
                r#""{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[{start},{}]}}"#,
                start + bytes
            )
        })
        .collect();
    crate::sniff::tests::safetensors_fixture(&format!("{{{}}}", entries.join(",")))
}

fn llm_shards() -> Vec<Vec<u8>> {
    vec![
        shard_header(&["model.layers.0.self_attn.q_proj.weight"], 64),
        shard_header(&["model.layers.1.self_attn.q_proj.weight"], 48),
        shard_header(&["lm_head.weight"], 32),
    ]
}

fn scan_hub(directory: &Path, hub: &Path) -> Result<Catalog> {
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

#[test]
fn a_rescan_refreshes_the_index_without_a_rebuild() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(catalog.status()?.search_ready);

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    let report = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(report.added, 1);
    assert_eq!(report.absent, 1);
    assert!(
        catalog.pending_changes()? >= 2,
        "the new artifact and the retracted one are logged"
    );
    assert!(!catalog.status()?.search_ready);
    assert!(
        search(&catalog, "").is_err(),
        "a stale index is refused, not served"
    );

    let applied = catalog.refresh_search(|_| {})?;
    assert!(applied >= 2);
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(catalog.status()?.search_ready);
    let rows = search(&catalog, "")?;
    let paths: Vec<&str> = rows.rows.iter().map(|row| row.path.as_str()).collect();
    assert_eq!(
        rows.total, 2,
        "keep and new; gone had no other path: {paths:?}"
    );
    assert!(paths.iter().any(|path| path.ends_with("new.safetensors")));
    assert!(!paths.iter().any(|path| path.ends_with("gone.safetensors")));
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<SearchChange>()?,
        0,
        "applied changes are pruned"
    );
    Ok(())
}

#[test]
fn a_refresh_is_idempotent_and_resumes_from_the_cursor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let before = search_total_after_refresh(&catalog)?;
    let again = catalog.refresh_search(|_| {})?;
    assert_eq!(again, 0, "nothing after the cursor");
    assert_eq!(search(&catalog, "")?.total, before);
    {
        let transaction = catalog.database.rw_transaction()?;
        let mut manifest = catalog.manifest()?;
        manifest.search_seq = 0;
        transaction.upsert(manifest)?;
        transaction.commit()?;
    }
    assert_eq!(
        catalog.refresh_search(|_| {})?,
        0,
        "pruned rows cannot be replayed twice"
    );
    assert_eq!(
        search(&catalog, "")?.total,
        before,
        "a rewound cursor changes no document"
    );
    Ok(())
}

fn search_total_after_refresh(catalog: &Catalog) -> Result<u64> {
    catalog.refresh_search(|_| {})?;
    Ok(search(catalog, "")?.total)
}

#[test]
fn a_refresh_beside_a_writer_returns() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;

    fs::write(input.join("loras/first.safetensors"), lora_header())?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let bounded = catalog.pending_changes()?;
    assert!(bounded >= 1, "the first artifact is logged");

    let mut filed = false;
    let applied = catalog.refresh_search(|_| {
        if filed {
            return;
        }
        filed = true;
        fs::write(input.join("loras/second.safetensors"), lora_header())
            .expect("a writer files another artifact");
        catalog
            .rescan(&config, HashPolicy::Never, |_, _| {})
            .expect("the writer's rescan logs a change");
    })?;
    assert!(filed, "the writer ran while the refresh was applying");
    assert_eq!(
        applied, bounded,
        "the refresh drained only up to the bound it read"
    );

    let rest = catalog.pending_changes()?;
    assert!(rest >= 1, "the writer's change is still pending");
    assert_eq!(
        catalog.refresh_search(|_| {})?,
        rest,
        "the next refresh takes the rest"
    );
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(
        search(&catalog, "")?
            .rows
            .iter()
            .any(|row| row.path.ends_with("second.safetensors")),
        "the writer's artifact is searchable after the second refresh"
    );
    Ok(())
}

#[test]
fn a_duplicated_shard_set_counts_two_paths_and_a_missing_member_still_one() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Twice",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let one = search(&catalog, "")?;
    assert_eq!(one.total, 1);
    assert_eq!(
        one.rows[0].path_count, 1,
        "two shards of one set are one path"
    );
    assert!(!one.rows[0].dupe);
    drop(catalog);

    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Copy",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    let copied = directory.path().join("copied");
    fs::create_dir(&copied)?;
    let catalog = scan_hub(&copied, &hub)?;
    let two = search(&catalog, "")?;
    assert_eq!(
        two.total, 1,
        "the same blobs under two repositories are one model"
    );
    assert_eq!(
        two.rows[0].path_count, 2,
        "four present shards of a two shard set are two paths"
    );
    assert!(two.rows[0].dupe);
    Ok(())
}

#[test]
fn a_sharded_model_is_one_native_artifact_over_every_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let total: u64 = shards.iter().map(|bytes| bytes.len() as u64).sum();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Sharded",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
            ("model-00003-of-00003.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the shards are one model");
    let row = &rows.rows[0];
    assert_eq!(row.role, hoard_core::Role::Llm);
    assert_eq!(row.name, "Qwen3-Sharded");
    assert_eq!(row.org.as_deref(), Some("Qwen"));
    assert_eq!(row.size, total, "the row weighs the whole model");
    assert_eq!(row.shard_count, 3);
    assert!(!row.triage);
    assert_eq!(
        row.path, "model-00001-of-00003.safetensors",
        "the anchor is the lowest shard"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 3, "every shard is still an entry");
    assert!(
        entries
            .iter()
            .all(|entry| entry.artifact_id == Some(row.artifact_id.get()))
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    assert!(catalog.status()?.counts.contains(&("artifacts".into(), 1)));
    Ok(())
}

#[test]
fn an_incomplete_shard_set_is_triage_and_counts_what_it_has() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Partial",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "state:triage")?;
    assert_eq!(rows.total, 1);
    assert_eq!(rows.rows[0].shard_count, 2);
    assert!(rows.rows[0].triage);
    Ok(())
}

#[test]
fn a_weight_map_groups_shards_the_naming_convention_would_miss() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let index = br#"{"metadata":{},"weight_map":{"model.layers.0.self_attn.q_proj.weight":"part-a.safetensors","model.layers.1.self_attn.q_proj.weight":"part-b.safetensors","lm_head.weight":"part-c.safetensors"}}"#.to_vec();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Mapped",
        &[
            ("model.safetensors.index.json", index),
            ("part-a.safetensors", shards[0].clone()),
            ("part-b.safetensors", shards[1].clone()),
            ("part-c.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the index document names the pieces");
    assert_eq!(rows.rows[0].shard_count, 3);
    assert_eq!(rows.rows[0].name, "Qwen3-Mapped");
    Ok(())
}

#[test]
fn two_shard_sets_in_one_directory_are_told_apart_by_stem() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Twins",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
            ("draft-00001-of-00002.safetensors", shards[1].clone()),
            ("draft-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 2);
    let mut names: Vec<&str> = rows.rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["draft", "model"],
        "two sets at the root are named by stem"
    );
    Ok(())
}

#[test]
fn shards_group_across_chunk_boundaries() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(302);
    files.push((
        "model-00001-of-00002.safetensors".to_owned(),
        shards[0].clone(),
    ));
    for index in 0..300 {
        files.push((
            format!("model-00001-z{index:03}.safetensors"),
            lora_header(),
        ));
    }
    files.push((
        "model-00002-of-00002.safetensors".to_owned(),
        shards[2].clone(),
    ));
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();
    hf_cache_repo(&hub, "acme", "wide", &borrowed)?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = paged(&catalog, 1000, 0)?;
    assert_eq!(rows.total, 301, "300 standalone files plus one shard set");
    let set: Vec<&SearchRow> = rows
        .rows
        .iter()
        .filter(|row| row.shard_count == 2)
        .collect();
    assert_eq!(
        set.len(),
        1,
        "the set is one artifact even with 300 files between its shards"
    );
    assert_eq!(set[0].path, "model-00001-of-00002.safetensors");
    Ok(())
}

#[test]
fn every_sort_key_orders_the_page_and_paging_is_stable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    for name in ["c-style", "a-style", "b-style"] {
        fs::write(
            input.join(format!("loras/{name}.safetensors")),
            lora_header(),
        )?;
    }
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let keys = [
        SortKey::Role,
        SortKey::Name,
        SortKey::Base,
        SortKey::Format,
        SortKey::Precision,
        SortKey::Size,
        SortKey::Revision,
        SortKey::Node,
        SortKey::State,
        SortKey::Used,
    ];
    for key in keys {
        for descending in [false, true] {
            let sort = SortSpec::new(key, descending);
            let whole = catalog.query(QueryRequest {
                raw: "",
                sort,
                limit: 10,
                offset: 0,
                context: &context,
            })?;
            assert_eq!(whole.total, 5, "{key:?}");
            let mut walked = Vec::new();
            for offset in [0, 2, 4] {
                let page = catalog.query(QueryRequest {
                    raw: "",
                    sort,
                    limit: 2,
                    offset,
                    context: &context,
                })?;
                walked.extend(page.rows.iter().map(|row| row.artifact_id));
            }
            let expected: Vec<_> = whole.rows.iter().map(|row| row.artifact_id).collect();
            assert_eq!(
                walked, expected,
                "paging by 2 walks the same order as one page for {key:?} desc={descending}"
            );
        }
    }
    let by_name = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Name, false),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    let names: Vec<&str> = by_name.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "a-style.safetensors",
            "b-style.safetensors",
            "c-style.safetensors",
            "data.csv",
            "tiny.jsonl"
        ]
    );
    let by_size = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Size, true),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    let sizes: Vec<u64> = by_size.rows.iter().map(|row| row.size).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "{sizes:?}");
    Ok(())
}

#[test]
fn facets_and_nodes_survive_a_multi_segment_index() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header(),
        )?;
        catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
        catalog.refresh_search(|_| {})?;
    }
    let page = search(&catalog, "")?;
    assert_eq!(page.total, 5);
    assert_eq!(page.nodes, 1);
    let names: Vec<&str> = page.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "extra-0.safetensors",
            "extra-1.safetensors",
            "extra-2.safetensors",
            "gone.safetensors",
            "keep.safetensors"
        ],
        "ordinals of different segments are merged by the term they spell"
    );
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let mut walked = Vec::new();
    for offset in [0, 2, 4] {
        let page = catalog.query(QueryRequest {
            raw: "",
            sort: SortSpec::default(),
            limit: 2,
            offset,
            context: &context,
        })?;
        walked.extend(page.rows.iter().map(|row| row.name.clone()));
    }
    assert_eq!(walked, names, "paging is stable across the segments");
    let lora = page
        .facets
        .role
        .iter()
        .find(|bucket| bucket.value == "lora")
        .map(|bucket| bucket.count);
    assert_eq!(
        lora,
        Some(4),
        "counts merge across the segments the refreshes created: {:?}",
        page.facets.role
    );
    Ok(())
}

#[test]
fn the_primary_path_answers_exactly_what_the_collapse_path_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Sharded",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[("unet/adapter.safetensors", lora_header())],
    )?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler-copy",
        &[("unet/adapter.safetensors", lora_header())],
    )?;
    hf_cache_repo(
        &hub,
        "data",
        "rows",
        &[("train.csv", b"x,y\n1,2\n".to_vec())],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let queries = [
        "",
        "role:lora",
        "fmt:csv,safetensors",
        "base:qwen",
        "dupe:true",
        "size:>100b",
        "-role:ds",
        "adapter",
    ];
    let keys = [
        SortKey::Name,
        SortKey::Size,
        SortKey::Role,
        SortKey::Base,
        SortKey::Format,
        SortKey::Precision,
        SortKey::Used,
    ];
    for raw in queries {
        for key in keys {
            for descending in [false, true] {
                for (limit, offset) in [(10, 0), (2, 0), (2, 2), (1, 3)] {
                    let request = QueryRequest {
                        raw,
                        sort: SortSpec::new(key, descending),
                        limit,
                        offset,
                        context: &context,
                    };
                    let fast = catalog.query_via(request.clone(), QueryPath::Primary)?;
                    let slow = catalog.query_via(request, QueryPath::Collapse)?;
                    assert_eq!(fast.total, slow.total, "{raw} {key:?} desc={descending}");
                    assert_eq!(fast.nodes, slow.nodes, "{raw}");
                    assert_eq!(fast.facets, slow.facets, "{raw} {key:?}");
                    let fast_ids: Vec<_> = fast
                        .rows
                        .iter()
                        .map(|row| (row.artifact_id, row.entry_id))
                        .collect();
                    let slow_ids: Vec<_> = slow
                        .rows
                        .iter()
                        .map(|row| (row.artifact_id, row.entry_id))
                        .collect();
                    assert_eq!(
                        fast_ids, slow_ids,
                        "{raw} {key:?} desc={descending} limit={limit} offset={offset}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn entry_level_tokens_and_sorts_take_the_collapse_path() -> Result<()> {
    let context = CompileContext::new(jiff::Timestamp::now()).with_shelves(vec![
        ("here".into(), "node:self".into()),
        ("loras".into(), "role:lora".into()),
    ]);
    let collapse = [
        "node:self",
        "state:local",
        "source:hub",
        "repo:acme/styler",
        "rev:main",
        "hash:abcd",
        "shelf:here",
        "role:lora node:self",
    ];
    let primary = [
        "",
        "role:lora",
        "base:sdxl",
        "fmt:gguf",
        "prec:fp16",
        "dupe:true",
        "size:<1gb",
        "used:>30d",
        "trigger:grain",
        "org:acme",
        "film grain",
        "shelf:loras",
        "-role:ds",
    ];
    for raw in collapse {
        assert!(
            query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?,
            "{raw}"
        );
    }
    for raw in primary {
        assert!(
            !query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?,
            "{raw}"
        );
    }
    for key in [SortKey::Revision, SortKey::Node, SortKey::State] {
        assert!(
            query::needs_collapse(&parse("").tokens, &context, key)?,
            "{key:?}"
        );
    }
    Ok(())
}

#[test]
fn a_catalog_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Catalog>();
}

#[test]
fn a_query_serves_the_published_index_while_changes_are_pending() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    fs::write(input.join("loras/new.safetensors"), lora_header())?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    let page = search(&catalog, "")?;
    assert!(page.stale, "the index has not seen the rescan yet");
    assert_eq!(page.total, 2, "the published index still answers");
    catalog.refresh_search(|_| {})?;
    let fresh = search(&catalog, "")?;
    assert!(!fresh.stale);
    assert_eq!(fresh.total, 3);
    Ok(())
}

#[test]
fn open_or_scan_creates_then_reopens_and_tolerates_no_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        ..config
    };
    let first = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert!(first.status()?.search_ready);
    assert_eq!(search(&first, "")?.total, 2);
    drop(first);
    let second = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert_eq!(second.status()?.generation, 1, "reopening does not rescan");
    drop(second);
    let empty = IndexConfig {
        data_dir: directory.path().join("empty"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let none = Catalog::open_or_scan(&empty, "n", |_, _| {})?;
    assert_eq!(search(&none, "")?.total, 0);
    Ok(())
}

#[test]
fn shelves_settings_and_noise_live_in_the_catalog_and_noise_reaches_the_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let shelves = catalog.list_shelves()?;
    assert_eq!(shelves.len(), 6, "the builtin shelves are seeded");
    assert!(shelves.iter().all(|shelf| shelf.builtin));
    assert_eq!(shelves[0].position, 0);
    let created = catalog.create_shelf("grainy", "role:lora film", 9)?;
    assert!(!created.builtin);
    assert!(
        catalog.create_shelf("grainy", "x", 1).is_err(),
        "names are unique"
    );
    let updated = catalog.update_shelf(created.id, "grainy", "role:lora base:sdxl", 9)?;
    assert_eq!(updated.query, "role:lora base:sdxl");
    assert!(catalog.delete_shelf(created.id)?);
    assert_eq!(catalog.seed_builtin_shelves()?, 6);
    assert_eq!(catalog.list_shelves()?.len(), 6, "re-seeding is idempotent");

    catalog.put_setting("theme", "dark")?;
    assert_eq!(catalog.get_setting("theme")?.as_deref(), Some("dark"));
    catalog.put_setting("theme", "light")?;
    assert_eq!(
        catalog.all_settings()?.get("theme").map(String::as_str),
        Some("light")
    );
    assert!(catalog.remove_setting("theme")?);
    assert!(!catalog.remove_setting("theme")?);

    let gone = input
        .join("loras/gone.safetensors")
        .to_string_lossy()
        .into_owned();
    let id = catalog.add_noise(&gone, Some("rejected from triage"))?;
    assert_eq!(
        catalog.add_noise(&gone, None)?,
        id,
        "adding the same path again keeps its id"
    );
    assert!(catalog.is_noise(&gone)?);
    assert_eq!(catalog.list_noise()?.len(), 1);
    let report = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        report.absent, 1,
        "a noisy path is walked past, so its row goes absent"
    );
    assert!(catalog.remove_noise(id)?);
    assert!(!catalog.is_noise(&gone)?);
    Ok(())
}

#[test]
fn projections_are_recorded_by_link_path_and_cleared_by_profile() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let first = catalog.record_projections(&[NewProjection {
        link_path: "/p/comfyui/loras/a".into(),
        target_path: "/data/a".into(),
        profile: "comfyui".into(),
        artifact_id: Some(ArtifactId::new(1)),
    }])?;
    let again = catalog.record_projections(&[NewProjection {
        link_path: "/p/comfyui/loras/a".into(),
        target_path: "/data/a2".into(),
        profile: "comfyui".into(),
        artifact_id: Some(ArtifactId::new(1)),
    }])?;
    assert_eq!(first, again, "the same link keeps its id");
    catalog.record_projections(&[NewProjection {
        link_path: "/p/a1111/lora/b".into(),
        target_path: "/data/b".into(),
        profile: "a1111".into(),
        artifact_id: None,
    }])?;
    assert_eq!(catalog.list_projections(None)?.len(), 2);
    assert_eq!(
        catalog.list_projections(Some("comfyui"))?[0].target_path,
        "/data/a2"
    );
    assert_eq!(
        catalog.projections_for_artifact(ArtifactId::new(1))?.len(),
        1
    );
    assert_eq!(catalog.clear_profile("comfyui")?, 1);
    assert!(catalog.remove_projection(catalog.list_projections(None)?[0].id)?);
    assert!(catalog.list_projections(None)?.is_empty());
    Ok(())
}

#[test]
fn usage_is_logged_summarised_and_reaches_the_used_token() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let row = search(&catalog, "role:lora")?.rows[0].clone();
    let repo = row.repo_id;
    let now = jiff::Timestamp::now();
    for client in ["a", "b", "b"] {
        catalog.log_usage(&UsageEvent {
            client: Some(client.to_owned()),
            repo: Some(repo),
            artifact: Some(row.artifact_id),
            bytes: 10,
            rate_bps: Some(5),
            ..UsageEvent::new("keep.safetensors", now)
        })?;
    }
    catalog.bump_artifact_use(row.artifact_id, now)?;
    assert_eq!(catalog.distinct_clients(repo)?, 2);
    assert_eq!(catalog.recent_pulls(repo, 10)?.len(), 3);
    let days = catalog.usage_days(repo, 7)?;
    assert_eq!(days.len(), 7);
    assert_eq!(days[6].pulls, 3, "today is the last bucket: {days:?}");
    let usage = catalog.artifact_usage(row.artifact_id)?;
    assert_eq!(usage.pulls, 1);
    assert!(usage.last_used.is_some());
    assert!(
        catalog.pending_changes()? > 0,
        "a bump logs the artifact for the index"
    );
    catalog.refresh_search(|_| {})?;
    assert_eq!(search(&catalog, "used:<1d")?.total, 1);
    assert_eq!(search(&catalog, "used:>30d")?.total, 0);
    assert_eq!(
        search(&catalog, "role:lora")?.rows[0].last_used,
        usage.last_used
    );
    let context = catalog.compile_context("", now, Vec::new())?;
    let by_use = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Used, true),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    assert_eq!(
        by_use.rows[0].artifact_id, row.artifact_id,
        "the used artifact sorts first descending"
    );
    Ok(())
}

#[test]
fn hashing_gives_every_file_a_sha256_and_merges_two_copies_into_one_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    fs::write(input.join("a/other.csv"), "x,y\n1,2\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(catalog.pending_hashes()?, 3);
    assert_eq!(
        catalog.totals()?.artifacts,
        3,
        "two copies are two artifacts until hashed"
    );
    let report = catalog.hash_pending(&config, |_| {})?;
    assert_eq!(report.hashed, 3);
    assert_eq!(report.merged, 1);
    assert_eq!(report.unreadable, 0);
    assert_eq!(catalog.pending_hashes()?, 0);
    assert_eq!(
        catalog.totals()?.artifacts,
        2,
        "the copies are one artifact now"
    );
    catalog.rebuild_search(|_| {})?;
    let page = search(&catalog, "role:lora")?;
    assert_eq!(page.total, 1);
    assert_eq!(page.rows[0].path_count, 2);
    assert!(page.rows[0].blob_sha256.is_some());
    assert!(page.rows[0].dupe);
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(entries.iter().all(|entry| entry.sha256.is_some()));
    assert!(
        entries.iter().all(|entry| entry.git_sha1.is_some()),
        "small files also get a git sha1"
    );
    drop(read);
    let sha = page.rows[0]
        .blob_sha256
        .ok_or_else(|| IndexError::Catalog("sha".into()))?;
    assert_eq!(
        catalog.artifact_by_sha256(&sha)?.map(|row| row.artifact_id),
        Some(page.rows[0].artifact_id)
    );
    let holders = catalog.entries_by_sha256(&[sha])?;
    assert_eq!(holders.len(), 1, "two copies collapse to one holder");
    assert_eq!(holders[0].1.sha256, Some(sha));
    assert!(holders[0].1.deduped);
    assert_eq!(
        catalog.hash_pending(&config, |_| {})?.hashed,
        0,
        "nothing is hashed twice"
    );
    Ok(())
}

#[test]
fn a_rescan_after_hashing_keeps_the_merged_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.hash_pending(&config, |_| {})?;
    let report = catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(report.unchanged, 2, "hashed files are unchanged files");
    assert_eq!(catalog.totals()?.artifacts, 1);
    fs::write(input.join("a/style.safetensors"), plain_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(input.join("a/style.safetensors"))?
        .set_modified(later)?;
    catalog.rescan(&config, HashPolicy::Never, |_, _| {})?;
    assert_eq!(
        catalog.pending_hashes()?,
        1,
        "a changed file loses its hash until rehashed"
    );
    catalog.hash_pending(&config, |_| {})?;
    assert_eq!(
        catalog.totals()?.artifacts,
        2,
        "different content, two artifacts again"
    );
    Ok(())
}

#[test]
fn hashing_now_gives_content_identities_from_the_first_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/style.safetensors"), lora_header())?;
    fs::write(input.join("b/style-copy.safetensors"), lora_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Now,
        |_, _| {},
    )?;
    assert_eq!(catalog.pending_hashes()?, 0);
    assert_eq!(
        catalog.totals()?.artifacts,
        1,
        "one artifact from the start"
    );
    let never = Catalog::scan_sources(
        &directory.path().join("never"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(never.pending_hashes()?, 2);
    assert_eq!(never.totals()?.artifacts, 2);
    Ok(())
}
