use super::*;

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
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
fn an_identity_is_a_hash_of_the_paths_bytes_and_two_copies_share_it() -> Result<()> {
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
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(b"path:");
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    let found: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, hasher.digest128())?;
    assert!(
        found.is_some(),
        "the identity is the xxh3-128 of the canonical path's bytes"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn two_paths_that_differ_only_in_invalid_utf8_have_distinct_identities() -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let directory = tempfile::tempdir()?;
    let paths: Vec<PathBuf> = [
        b"weights_\xfe.safetensors".as_slice(),
        b"weights_\xff.safetensors".as_slice(),
    ]
    .iter()
    .map(|name| directory.path().join(OsStr::from_bytes(name)))
    .collect();
    assert_eq!(
        paths[0].to_string_lossy(),
        paths[1].to_string_lossy(),
        "the two names must be the pair a lossy rendering folds together"
    );
    for path in &paths {
        fs::write(path, lora_header())?;
    }

    let identities: Vec<u128> = paths
        .iter()
        .map(|path| {
            let entry = DiscoveredEntry {
                rel_path: "weights.safetensors".to_owned(),
                abs_path: Some(path.clone()),
                size: 0,
                stamp: None,
                is_symlink: false,
                sha256: None,
                git_sha1: None,
                lfs_pointer: false,
            };
            let presence = Presence::of(&entry, None);
            identity(&entry, &presence)
        })
        .collect::<Result<_>>()?;

    assert_ne!(
        identities[0], identities[1],
        "two paths no text can tell apart are still two files"
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
fn refs_keep_their_kind() -> Result<()> {
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
        !absent.placed.is_absent(),
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
    assert!(entry.placed.is_absent());
    assert!(
        entry.artifact_id.is_none(),
        "a pointer stub is a path, not an artifact"
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 0);
    Ok(())
}

#[test]
fn a_fetched_lfs_weight_is_an_artifact_and_an_unfetched_pointer_is_not() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let weights = directory.path().join("model.safetensors");
    fs::write(&weights, lora_header())?;
    let fetched = discovered("model.safetensors", Some(&weights), true);
    let sniffed = crate::sniff::sniff_path(&weights)?;
    let presence = crate::catalog::scan::Presence::of(&fetched, Some(&sniffed));
    assert!(presence.readable);
    assert!(
        presence.lfs_pointer,
        "discovery's flag is kept for the entry row"
    );
    assert!(
        !presence.pointer_bytes,
        "the bytes on disk are weights, not a pointer"
    );
    assert!(crate::catalog::scan::creates_artifact(
        sniffed.format,
        &presence,
        &crate::catalog::scan::EntryRole::Standalone
    ));

    let pointer = directory.path().join("pointer.safetensors");
    let oid = "a".repeat(64);
    fs::write(
        &pointer,
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 5\n"),
    )?;
    let unfetched = discovered("pointer.safetensors", Some(&pointer), false);
    let sniffed = crate::sniff::sniff_path(&pointer)?;
    let presence = crate::catalog::scan::Presence::of(&unfetched, Some(&sniffed));
    assert!(presence.pointer_bytes);
    assert!(!presence.readable);
    assert!(!crate::catalog::scan::creates_artifact(
        sniffed.format,
        &presence,
        &crate::catalog::scan::EntryRole::Standalone
    ));
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
fn a_source_dropped_from_the_configuration_is_retired_by_the_next_full_rescan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let kept = directory.path().join("kept");
    let dropped = directory.path().join("dropped");
    fs::create_dir(&kept)?;
    fs::create_dir(&dropped)?;
    fs::write(kept.join("kept.safetensors"), lora_header_named("kept"))?;
    fs::write(kept.join("README.md"), card_body("chinchilla"))?;
    fs::write(
        dropped.join("dropped.safetensors"),
        lora_header_named("gone"),
    )?;
    fs::write(dropped.join("README.md"), card_body("vicuna"))?;
    let both = IndexConfig {
        sources: vec![source(&kept, "kept"), source(&dropped, "dropped")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &both,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let before = catalog.totals()?;
    assert_eq!(catalog.sources()?.len(), 2);
    assert_eq!(before.present_artifacts, 2);

    let now = jiff::Timestamp::now();
    let served = |path: &str| -> Result<RepoRowId> {
        let row = search(&catalog, path)?
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::Catalog(format!("no row for {path}")))?;
        catalog.log_usage(&UsageEvent {
            client: Some("10.0.0.2".to_owned()),
            repo: Some(row.repo_id),
            artifact: Some(row.artifact_id),
            bytes: 10,
            ..UsageEvent::new(row.path.clone(), now)
        })?;
        Ok(row.repo_id)
    };
    let kept_repo = served("kept")?;
    let dropped_repo = served("gone")?;
    assert_eq!(catalog.recent_pulls(dropped_repo, 10)?.len(), 1);

    let only_kept = IndexConfig {
        sources: vec![source(&kept, "kept")],
        ..IndexConfig::default()
    };
    catalog.rescan(
        &only_kept,
        HashPolicy::Never,
        RescanScope::Narrowed,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.sources()?.len(),
        2,
        "a narrowed rescan retires nothing it was not asked to walk"
    );

    let report = catalog.rescan(
        &only_kept,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report.absent, 2,
        "both paths of the dropped source are retired"
    );
    let names: Vec<String> = catalog
        .sources()?
        .into_iter()
        .map(|record| record.name)
        .collect();
    assert_eq!(names, ["kept"], "the dropped source row is gone");

    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let mut paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry.rel_path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        ["README.md", "kept.safetensors"],
        "the retired source's rows go with it"
    );
    let repositories: Vec<Repository> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(repositories.len(), 1, "a repository outlives no source");
    let cards: Vec<models::RevisionText> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(
        cards.len(),
        1,
        "the retired repository's card is removed with its rows"
    );
    assert_eq!(
        cards[0].key,
        LocationKey::revision(repositories[0].id, 0),
        "and the one that is left is the kept repository's"
    );
    assert_eq!(
        read.len().primary::<ArtifactRecord>()?,
        1,
        "the artifact that lost its last path is swept"
    );
    drop(read);

    let after = catalog.totals()?;
    assert_eq!(
        after.present_artifacts, 1,
        "the totals follow the paths that are still present"
    );
    assert_eq!(after.artifacts, 1);
    assert!(
        catalog.recent_pulls(dropped_repo, 10)?.is_empty(),
        "the retired repository's served history goes with it"
    );
    assert_eq!(catalog.distinct_clients(dropped_repo)?, 0);
    assert_eq!(
        catalog.recent_pulls(kept_repo, 10)?.len(),
        1,
        "the kept repository keeps its own"
    );
    assert_eq!(catalog.distinct_clients(kept_repo)?, 1);

    catalog.refresh_search(|_| {})?;
    let rows = search(&catalog, "state:local")?;
    let paths: Vec<&str> = rows.rows.iter().map(|row| row.path.as_str()).collect();
    assert_eq!(paths, ["kept.safetensors"]);
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
        crate::catalog::models::repository_key("datasets", &repository.repo).as_str(),
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
fn two_copies_in_one_unit_share_one_record_without_a_cache() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(&input)?;
    fs::write(input.join("a-copy.safetensors"), lora_header())?;
    fs::write(input.join("b-copy.safetensors"), lora_header())?;
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
    catalog.rebuild_search(|_| {})?;

    let read = catalog.database.r_transaction()?;
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(
        records.len(),
        1,
        "the identity key sees the row this transaction wrote a moment ago"
    );
    assert_eq!(records[0].path_count, 2, "both copies are its paths");
    assert_eq!(
        records[0].name, "a-copy.safetensors",
        "the first path names it"
    );
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.artifact_id == Some(records[0].id))
    );
    Ok(())
}

#[test]
fn prepared_scan_units_validate_files_before_storing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("changing.safetensors");
    fs::write(&path, lora_header())?;
    let taken = crate::hash::stamp(&path)?;
    assert!(
        !crate::catalog::scan::stamp_moved(Some(&path), Some(taken)),
        "a file nobody touched did not change"
    );
    assert!(
        !crate::catalog::scan::stamp_moved(Some(&path), None),
        "a path the walk never stamped is not a change"
    );
    assert!(
        !crate::catalog::scan::stamp_moved(None, Some(taken)),
        "a row with no path is not a change"
    );
    let mut grown = lora_header();
    grown.extend_from_slice(&[0; 64]);
    fs::write(&path, &grown)?;
    assert!(
        crate::catalog::scan::stamp_moved(Some(&path), Some(taken)),
        "a file rewritten under the read is a change"
    );
    let after = crate::hash::stamp(&path)?;
    assert_ne!(after.size, taken.size);
    fs::remove_file(&path)?;
    assert!(
        !crate::catalog::scan::stamp_moved(Some(&path), Some(taken)),
        "a file that is gone takes the absent path, not the retry"
    );

    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    let weight = input.join("w.safetensors");
    fs::write(&weight, lora_header_named("first"))?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    let repo = only_repository(&catalog)?;
    let first = catalog
        .revision_entry(repo.id, 0, "w.safetensors")?
        .ok_or_else(|| IndexError::Catalog("the scanned entry".into()))?;
    let mut longer = lora_header_named("second");
    longer.extend_from_slice(&[0; 128]);
    fs::write(&weight, &longer)?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    let second = catalog
        .revision_entry(repo.id, 0, "w.safetensors")?
        .ok_or_else(|| IndexError::Catalog("the rescanned entry".into()))?;
    assert_ne!(
        second.size, first.size,
        "the replacement row carries the bytes that are there, not the ones that were"
    );
    assert_eq!(
        second.size,
        longer.len() as u64,
        "and it carries all of them"
    );

    let transaction = catalog.database.rw_transaction()?;
    let stored: Entry = transaction
        .get()
        .primary(second.entry_id.get())?
        .ok_or_else(|| IndexError::Catalog("the stored entry".into()))?;
    transaction.update(
        stored.clone(),
        Entry {
            size: 1,
            ..stored.clone()
        },
    )?;
    transaction.abort()?;
    let kept = catalog
        .revision_entry(repo.id, 0, "w.safetensors")?
        .ok_or_else(|| IndexError::Catalog("the kept entry".into()))?;
    assert_eq!(kept.size, second.size, "an aborted store keeps the old row");
    Ok(())
}

#[test]
fn an_entry_under_its_root_stores_no_absolute_path() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert_eq!(
            entry.placed,
            Placement::UnderRoot,
            "a loose file sits where its repository says it does: {}",
            entry.rel_path
        );
    }
    drop(read);

    let repo = catalog
        .repositories(&RepositoryFilter::default(), None, 10)?
        .rows
        .into_iter()
        .next()
        .ok_or_else(|| IndexError::Catalog("the scanned repository".into()))?;
    let detail = catalog
        .revision_entry(repo.id, 0, "loras/keep.safetensors")?
        .ok_or_else(|| IndexError::Catalog("the scanned entry".into()))?;
    assert_eq!(
        detail.abs_path.as_deref(),
        input.join("loras/keep.safetensors").to_str(),
        "a reader still sees the whole path"
    );

    let elsewhere = "/hub/blobs/deadbeef".to_owned();
    catalog.seed(&[SeedRepository {
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([0x71; 20])),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries: vec![SeedEntry {
                abs_path: Some(elsewhere.clone()),
                ..seeded_entry("model.safetensors", 64, None)
            }],
        }],
        ..seeded_repo("hub/snapshotted", RepoType::Models)
    }])?;
    let read = catalog.database.r_transaction()?;
    let blob: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let blob = blob
        .iter()
        .find(|entry| entry.rel_path == "model.safetensors")
        .ok_or_else(|| IndexError::Catalog("the seeded blob".into()))?;
    assert_eq!(
        blob.placed,
        Placement::At(elsewhere),
        "a snapshot pointing at a blob outside its root names the blob"
    );
    Ok(())
}

fn artifact_names(catalog: &Catalog) -> Result<Vec<String>> {
    let read = catalog.database.r_transaction()?;
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let mut names: Vec<String> = records.into_iter().map(|record| record.name).collect();
    names.sort();
    Ok(names)
}

/// One repository of two snapshots sharing a file: `alone` holds `consolidated.safetensors`
/// by itself, `beside` holds the same blob next to a four shard set.
fn shared_snapshot_names(alone: CommitSha, beside: CommitSha) -> Result<Vec<String>> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let consolidated = shard_header(&["model.embed_tokens.weight"], 96);
    let shards: Vec<Vec<u8>> = (0..4)
        .map(|index| shard_header(&[&format!("model.layers.{index}.mlp.up_proj.weight")], 64))
        .collect();
    let mut crowded: Vec<(&str, &[u8])> =
        vec![("consolidated.safetensors", consolidated.as_slice())];
    let names: Vec<String> = (0..4)
        .map(|index| format!("model-0000{}-of-00004.safetensors", index + 1))
        .collect();
    crowded.extend(
        names
            .iter()
            .zip(&shards)
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
    );
    crate::discover::hf_cache::fixture::write_repo(
        &hub,
        STYLER,
        &alone.to_hex(),
        &[("consolidated.safetensors", consolidated.as_slice())],
    );
    crate::discover::hf_cache::fixture::write_repo(&hub, STYLER, &beside.to_hex(), &crowded);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &hub_config(&hub),
        HashPolicy::Never,
        |_, _| {},
    )?;
    artifact_names(&catalog)
}

#[test]
fn two_snapshots_sharing_content_are_named_the_same_way_round_in_either_order() -> Result<()> {
    let first = CommitSha::from_bytes([0xaa; 20]);
    let second = CommitSha::from_bytes([0xdd; 20]);
    let alone_first = shared_snapshot_names(first, second)?;
    let beside_first = shared_snapshot_names(second, first)?;
    assert_eq!(
        alone_first, beside_first,
        "the walk order of the two snapshots must not decide either name"
    );
    assert_eq!(
        alone_first,
        ["consolidated.safetensors", "model"],
        "the shared file is named as the file it is and the set is named as a set"
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
