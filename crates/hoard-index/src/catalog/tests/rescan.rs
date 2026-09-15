use super::*;

#[test]
fn an_unchanged_rescan_reads_no_headers_and_a_deleted_file_is_marked_absent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    let first = catalog.status()?;
    assert_eq!(first.generation, 1);

    let unchanged = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        unchanged,
        RescanReport {
            unchanged: 2,
            changed: 0,
            added: 0,
            absent: 0,
            interrupted: false,
        }
    );
    assert_eq!(catalog.status()?.generation, 2);
    assert!(!catalog.status()?.search_ready, "a rescan needs a rebuild");

    fs::remove_file(input.join("loras/gone.safetensors"))?;
    let retracted = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        retracted,
        RescanReport {
            unchanged: 1,
            changed: 0,
            added: 0,
            absent: 1,
            interrupted: false,
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
    assert!(!gone.placed.is_absent(), "the path it had is kept");
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

    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report,
        RescanReport {
            unchanged: 1,
            changed: 1,
            added: 1,
            absent: 0,
            interrupted: false,
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

#[test]
fn a_location_key_separates_repositories_and_ordinals() {
    let path = "unet/adapter.safetensors";
    assert_eq!(
        location_hash(7, 0, path),
        location_hash(7, 0, path),
        "a location is the repository, the ordinal and the path, and nothing about where \
         the bytes of the row filed there sit"
    );
    assert_ne!(
        location_hash(7, 0, path),
        location_hash(7, 1, path),
        "one path under two revisions of a repository is two locations"
    );
    assert_ne!(
        location_hash(7, 0, path),
        location_hash(8, 0, path),
        "and one path under two repositories is two more"
    );
    assert_ne!(
        location_hash(7, 0, path),
        location_hash(7, 0, "unet/other.safetensors"),
        "two paths of one revision are two locations"
    );
}

#[test]
fn an_hf_cache_file_rewritten_in_place_is_replaced_on_the_next_rescan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let card = b"A style LoRA.\n".to_vec();
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[
            ("README.md", card.clone()),
            ("unet/adapter.safetensors", lora_header()),
        ],
    )?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let before = only_repository(&catalog)?;
    let first_artifact = artifact_ids(&catalog)?;
    assert_eq!(first_artifact.len(), 1);

    let rewritten = lora_header_named("up");
    let repo = hub.join(STYLER);
    let blob = repo.join("blobs").join("f".repeat(64));
    fs::write(&blob, &rewritten)?;
    let link = repo
        .join("snapshots")
        .join("b".repeat(40))
        .join("unet/adapter.safetensors");
    fs::remove_file(&link)?;
    std::os::unix::fs::symlink(&blob, &link)?;

    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report,
        RescanReport {
            unchanged: 1,
            changed: 1,
            added: 0,
            absent: 0,
            interrupted: false,
        },
        "the rewritten file is recognised where it is filed, not at the blob it moved to"
    );

    let after = only_repository(&catalog)?;
    let entries = catalog.revision_entries(after.id, 0, Dedupe::Ignore)?;
    assert_eq!(entries.len(), 2, "the location holds one row, not two");
    let adapter = entries
        .iter()
        .find(|entry| entry.rel_path == "unet/adapter.safetensors")
        .ok_or_else(|| IndexError::Catalog("the rewritten path keeps its location".into()))?;
    assert_eq!(
        adapter.size,
        rewritten.len() as u64,
        "the row carries the bytes that are there now"
    );
    assert_eq!(
        adapter.sha256,
        Some("f".repeat(64).parse::<Sha256>()?),
        "and the identity of the blob it now points at"
    );
    let second_artifact = artifact_ids(&catalog)?;
    assert_eq!(second_artifact.len(), 1);
    assert_ne!(
        second_artifact, first_artifact,
        "the artifact of the old bytes lost its last path and was swept"
    );
    assert_eq!(
        after.present_bytes,
        (card.len() + rewritten.len()) as u64,
        "the derived counts moved with the bytes"
    );
    assert_eq!(after.present_files, 2);
    assert_eq!(
        before.present_bytes,
        (card.len() + lora_header().len()) as u64
    );
    Ok(())
}

#[test]
fn a_moved_file_under_a_loose_root_is_replaced_not_duplicated() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    fs::rename(
        input.join("loras/keep.safetensors"),
        input.join("loras/moved.safetensors"),
    )?;

    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report,
        RescanReport {
            unchanged: 1,
            changed: 0,
            added: 1,
            absent: 1,
            interrupted: false,
        },
        "the location it left is retracted and the one it took is a new row"
    );
    let repository = only_repository(&catalog)?;
    let entries = catalog.revision_entries(repository.id, 0, Dedupe::Ignore)?;
    assert_eq!(entries.len(), 3, "the path it left keeps its row");
    let moved = entries
        .iter()
        .find(|entry| entry.rel_path.ends_with("moved.safetensors"))
        .ok_or_else(|| IndexError::Catalog("the new location is stored".into()))?;
    assert_eq!(moved.state, State::Local);
    let left = entries
        .iter()
        .find(|entry| entry.rel_path.ends_with("keep.safetensors"))
        .ok_or_else(|| IndexError::Catalog("the old location is kept".into()))?;
    assert_eq!(left.state, State::Remote);
    Ok(())
}

#[test]
fn a_raised_stop_ends_a_rescan_between_units_and_retires_nothing() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let input = directory.path().join("files");
    let config = IndexConfig {
        sources: vec![source(&input, "datasets")],
        ..IndexConfig::default()
    };
    fs::remove_file(input.join("tiny.jsonl"))?;

    let stop = Stop::default();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let report = catalog.rescan_until(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        &stop,
        |_, _| {},
    )?;

    assert!(report.interrupted, "the raised stop ended the walk");
    assert_eq!(report.absent, 0, "an interrupted walk retracts nothing");
    assert_eq!(
        catalog.totals()?.present_artifacts,
        2,
        "a path the walk never reached is not a path this node stopped holding"
    );
    assert!(
        pass_is_open(&catalog)?,
        "the mark is left for the next walk"
    );

    let finished = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert!(!finished.interrupted);
    assert_eq!(finished.absent, 1, "the next walk retracts the gone file");
    assert_eq!(catalog.totals()?.present_artifacts, 1);
    assert!(!pass_is_open(&catalog)?);
    Ok(())
}

/// Scans one hf-cache repository holding `held` as a snapshot of one blob, then links
/// `added` to the same blob under the same path and rescans, answering what each revision
/// holds afterwards.
fn snapshots_sharing_a_blob(held: &str, added: &str) -> Result<Vec<Vec<String>>> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let repo = hub.join(STYLER);
    fs::create_dir_all(repo.join("blobs"))?;
    fs::create_dir_all(repo.join("refs"))?;
    let blob = repo.join("blobs").join("a".repeat(64));
    fs::write(&blob, lora_header())?;
    let link = |commit: &str| -> Result<()> {
        let snapshot = repo.join("snapshots").join(commit);
        fs::create_dir_all(&snapshot)?;
        std::os::unix::fs::symlink(&blob, snapshot.join("adapter.safetensors"))?;
        Ok(())
    };
    link(held)?;
    fs::write(repo.join("refs/main"), held)?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    link(added)?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    let after = only_repository(&catalog)?;
    (0..u32::try_from(after.revisions.len()).unwrap_or_default())
        .map(|ordinal| paths_of_revision(&catalog, &after, ordinal))
        .collect()
}

#[test]
fn a_snapshot_that_appears_beside_one_sharing_its_blob_takes_no_row_from_it() -> Result<()> {
    for (held, added) in [
        ("b".repeat(40), "a".repeat(40)),
        ("b".repeat(40), "c".repeat(40)),
    ] {
        let revisions = snapshots_sharing_a_blob(&held, &added)?;
        assert_eq!(
            revisions.len(),
            2,
            "the snapshot added beside {held} is a revision of its own"
        );
        for (ordinal, paths) in revisions.iter().enumerate() {
            assert_eq!(
                paths,
                &["adapter.safetensors"],
                "revision {ordinal} keeps its row when {added} was added beside {held}"
            );
        }
    }
    Ok(())
}

#[test]
fn a_rescan_retracts_more_paths_than_one_transaction_holds() -> Result<()> {
    let total = crate::catalog::scan::CHUNK + 7;
    let (directory, catalog, config) = csv_catalog_with_entries(total)?;
    let input = directory.path().join("files");
    assert_eq!(catalog.totals()?.present_artifacts, total as u64);

    for index in 0..total {
        fs::remove_file(input.join(format!("row{index:05}.csv")))?;
    }
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    assert_eq!(
        report.absent, total as u64,
        "every path the walk did not reach is retracted, across the chunk boundary"
    );
    assert_eq!(catalog.totals()?.present_artifacts, 0);
    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "the derived counts of every chunk reached the summary"
    );
    Ok(())
}

#[test]
fn a_branch_that_advances_twice_leaves_one_card_and_slots_that_answer_to_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = git_fixture(directory.path(), None)?;
    let clone = input.join("org").join("clone");
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let repo = RepoId::parse_ref(RepoType::Models, "org/clone")?;
    let first = repository_of(&catalog, &repo)?;
    let abandoned: Vec<String> = first
        .revisions
        .iter()
        .filter_map(|revision| revision.sha.map(|sha| sha.to_string()))
        .collect();
    assert_eq!(abandoned.len(), 1);

    for step in 0..2 {
        fs::write(clone.join(format!("note{step}.md")), "# notes\n")?;
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "another import", "--quiet"]);
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
    }

    let detail = repository_of(&catalog, &repo)?;
    assert_eq!(
        detail.revisions.len(),
        1,
        "only the head a ref reaches is a revision: {:?}",
        detail.revisions
    );
    assert_eq!(
        detail.revisions[0].ordinal, 2,
        "the two heads it left keep their ordinals"
    );
    for sha in &abandoned {
        assert!(
            catalog.resolve_revision(&detail, sha).is_none(),
            "a retired revision answers to no name of its own"
        );
    }
    assert!(
        catalog
            .revision_entries(detail.id, 0, Dedupe::Ignore)?
            .is_empty(),
        "and no entry is filed under it"
    );

    let read = catalog.database.r_transaction()?;
    let cards: Vec<models::RevisionText> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(
        cards.len(),
        1,
        "one card, on the head that holds it, rather than one per abandoned slot"
    );
    assert_eq!(cards[0].key, LocationKey::revision(detail.id.get(), 2));
    Ok(())
}
