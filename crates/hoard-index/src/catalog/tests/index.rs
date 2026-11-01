use super::*;

#[test]
fn an_old_search_schema_is_rebuilt_without_rescanning_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    catalog.seed(&[seeded_revision(
        seeded_repo("comfy/models", RepoType::Models),
        0x77,
        vec![seeded_entry(
            "moss/alias.safetensors",
            1024,
            Some(seeded_artifact(0x77, "canonical", None, seeded_facts())),
        )],
    )])?;
    catalog.rebuild_search(|_| {})?;
    let before = search(&catalog, "alias")?.rows[0].clone();
    let old_directory = catalog
        .manifest()?
        .search_directory
        .expect("published index");
    let metadata = catalog.directory.join(&old_directory).join("meta.json");
    drop(catalog);
    let text = fs::read_to_string(&metadata)?;
    assert!(text.contains("path_text_v1"));
    fs::write(metadata, text.replace("path_text_v1", "text"))?;
    let mut phases = Vec::new();
    let catalog = Catalog::open_or_scan(&config, "workstation", |phase, _| {
        phases.push(phase.to_owned())
    })?;
    assert!(
        phases
            .iter()
            .any(|phase| phase == "building the search index")
    );
    assert!(phases.iter().all(|phase| !phase.contains("scanning")));
    assert_ne!(
        catalog.manifest()?.search_directory.as_deref(),
        Some(old_directory.as_str())
    );
    let after = search(&catalog, "alias")?.rows[0].clone();
    assert_eq!(before.artifact_id, after.artifact_id);
    assert_eq!(before.entry_id, after.entry_id);
    Ok(())
}

#[test]
fn an_unpublished_build_leaves_the_previous_index_serving() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let before = catalog.manifest()?;
    let staged = catalog.directory.join("search-v1-staged");
    fs::create_dir(&staged)?;
    catalog.build_index(&staged, INDEX_FLUSH_BYTES, |_| {})?;
    assert_eq!(catalog.manifest()?, before, "building publishes nothing");
    assert_eq!(
        search(&catalog, "")?.total,
        2,
        "the previous index still answers"
    );
    catalog.publish("search-v1-staged".to_owned(), before.search_seq)?;
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

fn seed_one_late_artifact(catalog: &Catalog) -> Result<()> {
    catalog.seed(&[seeded_revision(
        seeded_repo("acme/late-arrival", RepoType::Models),
        0x7e,
        vec![seeded_entry(
            "late.safetensors",
            4_096,
            Some(seeded_artifact(
                0x7e7e,
                "late-arrival",
                Some("acme"),
                seeded_facts(),
            )),
        )],
    )])?;
    Ok(())
}

#[test]
fn a_change_logged_during_a_rebuild_survives_to_the_next_refresh() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        catalog.pending_changes()?,
        0,
        "a fresh build drains the log"
    );

    let mut seeded = false;
    catalog.rebuild_search(|_| {
        if !seeded {
            seeded = true;
            seed_one_late_artifact(&catalog).expect("the late artifact is written");
        }
    })?;

    assert_eq!(
        search(&catalog, "")?.total,
        2,
        "the rebuild indexed the snapshot it opened, which predates the change"
    );
    assert!(
        catalog.pending_changes()? >= 1,
        "a change logged while the rebuild ran is left for the next refresh"
    );
    let applied = catalog.refresh_search(|_| {})?;
    assert!(applied >= 1, "the refresh applies what the rebuild left");
    assert_eq!(
        search(&catalog, "")?.total,
        3,
        "the late artifact reaches the index"
    );
    Ok(())
}

#[test]
fn the_search_lane_serialises_a_refresh_and_a_rebuild() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let pause = std::time::Duration::from_millis(50);
    let reached_the_lane = std::sync::Barrier::new(2);
    std::thread::scope(|scope| -> Result<()> {
        let rebuilding = scope.spawn(|| -> Result<std::time::Instant> {
            let mut first = true;
            catalog.rebuild_search(|_| {
                if first {
                    first = false;
                    reached_the_lane.wait();
                }
                std::thread::sleep(pause);
            })?;
            Ok(std::time::Instant::now())
        });
        let refreshing = scope.spawn(|| -> Result<(std::time::Instant, std::time::Instant)> {
            reached_the_lane.wait();
            let asked = std::time::Instant::now();
            catalog.refresh_search(|_| {})?;
            Ok((asked, std::time::Instant::now()))
        });
        let published = rebuilding.join().expect("the rebuild thread joins")?;
        let (asked, refreshed) = refreshing.join().expect("the refresh thread joins")?;
        assert!(
            refreshed >= published,
            "the refresh returned only after the rebuild published"
        );
        assert!(
            refreshed.duration_since(asked) >= pause,
            "the refresh waited on the lane rather than running beside the rebuild"
        );
        Ok(())
    })
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
        Err(error @ IndexError::CatalogBusy) => error.to_string(),
        other => panic!("expected the catalog to report its lock, got {other:?}"),
    };
    assert!(message.contains("another process"), "{message}");
    let inspected = Catalog::inspect(&directory.path().join("native"));
    assert!(
        matches!(inspected, Err(IndexError::CatalogBusy)),
        "inspecting a locked catalog reports the lock rather than waiting: {inspected:?}"
    );
    drop(catalog);
    Ok(())
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
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.added, 1);
    assert_eq!(report.absent, 1);
    assert!(
        catalog.pending_changes()? >= 2,
        "the new artifact and the retracted one are logged"
    );
    assert!(!catalog.status()?.search_ready);
    let behind = search(&catalog, "")?;
    assert!(
        behind.stale,
        "the index still answers between the rescan and the refresh, marked stale"
    );
    assert!(
        !behind
            .rows
            .iter()
            .any(|row| row.path.ends_with("gone.safetensors")),
        "the row the rescan retracted is skipped, not refused"
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
fn a_query_between_a_rescan_and_its_refresh_answers_stale_rather_than_failing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let fresh = search(&catalog, "")?;
    assert_eq!(fresh.total, 2);
    assert!(!fresh.stale);

    let changed = input.join("loras/gone.safetensors");
    fs::write(&changed, lora_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&changed)?
        .set_modified(later)?;
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.changed, 1);

    let between = search(&catalog, "")?;
    assert!(
        between.stale,
        "the answer predates the rows the rescan replaced and says so"
    );
    assert_eq!(between.total, 2, "total stays what the index counted");
    assert_eq!(
        between.rows.len(),
        1,
        "the replaced row is skipped, the rest of the page stands"
    );
    assert!(between.rows[0].path.ends_with("keep.safetensors"));

    let applied = catalog.refresh_search(|_| {})?;
    assert!(applied >= 1);
    let after = search(&catalog, "")?;
    assert!(!after.stale, "the refresh settles it");
    assert_eq!(after.total, 2);
    assert!(
        after
            .rows
            .iter()
            .any(|row| row.path.ends_with("gone.safetensors")),
        "and the page holds the entry the rescan wrote in its place"
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
            .rescan(
                &config,
                HashPolicy::Never,
                RescanScope::Configured,
                |_, _| {},
            )
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
fn a_rebuild_over_the_budget_lands_in_more_than_one_segment_and_answers_the_same() -> Result<()> {
    let (_directory, catalog) = paging_fixture(0)?;
    let ids = wide_card_fixture(&catalog)?;
    assert_eq!(ids.len(), 40, "every artifact was written");

    catalog.rebuild_search_with_budget(1024 * 1024, |_| {})?;
    assert!(
        catalog.search_segments()? >= 2,
        "a 1 MiB budget against 40 artifacts of 16 KiB cards forces more than one segment"
    );
    assert_eq!(search(&catalog, "")?.total, 40);
    let budgeted_page = search(&catalog, "node:self")?;
    assert_eq!(budgeted_page.total, 40);
    assert_eq!(budgeted_page.rows.len(), 40);
    for raw in ["node:self", "state:local"] {
        let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
        let (folded, tallied) = catalog.collapsed_counts(raw, &context)?;
        assert_eq!(folded, tallied, "{raw}");
        assert_eq!(folded, 40, "{raw}");
    }
    let budgeted_order: Vec<ArtifactId> = budgeted_page
        .rows
        .iter()
        .map(|row| row.artifact_id)
        .collect();

    catalog.rebuild_search(|_| {})?;
    let default_order: Vec<ArtifactId> = search(&catalog, "node:self")?
        .rows
        .iter()
        .map(|row| row.artifact_id)
        .collect();
    assert_eq!(
        default_order, budgeted_order,
        "the default budget answers the same order as the budgeted rebuild"
    );
    Ok(())
}

#[test]
fn a_rebuild_reports_the_commits_it_made() -> Result<()> {
    let (_directory, catalog) = paging_fixture(0)?;
    let ids = wide_card_fixture(&catalog)?;
    assert_eq!(ids.len(), 40, "every artifact was written");

    let commits = catalog.rebuild_search_commits(|_| {})?;
    assert!(
        commits >= 1,
        "a rebuild always commits at least once: {commits}"
    );
    assert_eq!(
        search(&catalog, "")?.total,
        40,
        "the reporting rebuild indexes exactly what the plain one does"
    );
    Ok(())
}

#[test]
fn a_refresh_over_the_budget_commits_more_than_once_and_keeps_one_run_per_artifact() -> Result<()> {
    let (_directory, catalog) = paging_fixture(0)?;
    let ids = wide_card_fixture(&catalog)?;
    catalog.rebuild_search(|_| {})?;
    let segments_before = catalog.search_segments()?;

    let now = jiff::Timestamp::now();
    for id in &ids {
        catalog.bump_artifact_use(*id, now)?;
    }
    assert_eq!(
        catalog.pending_changes()?,
        40,
        "each bump logged its artifact"
    );

    let report = catalog.refresh_with_budget(1024 * 1024, |_| {})?;
    assert_eq!(report.applied, 40);
    assert!(
        report.commits >= 2,
        "a 1 MiB budget against 40 artifacts of 16 KiB cards commits more than once: {}",
        report.commits
    );
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(
        catalog.search_segments()? > segments_before,
        "the extra commits leave more segments than the rebuild published"
    );

    let refreshed_page = search(&catalog, "node:self")?;
    assert_eq!(refreshed_page.total, 40);
    assert_eq!(refreshed_page.rows.len(), 40);
    for raw in ["node:self", "state:local"] {
        let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
        let (folded, tallied) = catalog.collapsed_counts(raw, &context)?;
        assert_eq!(folded, tallied, "{raw}");
        assert_eq!(folded, 40, "{raw}");
    }
    Ok(())
}

#[test]
fn a_refresh_reports_the_documents_and_bytes_it_wrote() -> Result<()> {
    let (_directory, catalog) = paging_fixture(0)?;
    let ids = wide_card_fixture(&catalog)?;
    catalog.rebuild_search(|_| {})?;

    let now = jiff::Timestamp::now();
    for id in ids.iter().take(5) {
        catalog.bump_artifact_use(*id, now)?;
    }

    let report = catalog.refresh_search_reporting(|_| {})?;
    assert_eq!(report.applied, 5);
    assert_eq!(report.documents, 40, "5 artifacts of 8 entries each");
    assert!(
        report.bytes >= 5 * 8 * 16 * 1024,
        "each of the 40 documents carries the 16 KiB card: {}",
        report.bytes
    );
    assert_eq!(report.commits, 1);

    let again = catalog.refresh_search_reporting(|_| {})?;
    assert_eq!(again, RefreshReport::default(), "nothing left to apply");
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
fn a_refresh_batch_holds_the_change_at_its_bound() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let now = jiff::Timestamp::now();
    catalog.bump_artifact_use(ids.lora, now)?;
    catalog.bump_artifact_use(ids.ckpt, now)?;

    let read = catalog.database.r_transaction()?;
    let logged: Vec<SearchChange> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    let seqs: Vec<u64> = logged.iter().map(|change| change.seq).collect();
    let bound = seqs
        .last()
        .copied()
        .ok_or_else(|| IndexError::Catalog("a bump logs a change".into()))?;
    assert!(seqs.len() >= 2, "each bump logged its artifact: {seqs:?}");

    assert_eq!(
        catalog.change_seqs_up_to(0, bound)?,
        seqs,
        "a batch bounded by the highest sequence replays it"
    );
    assert_eq!(
        catalog.change_seqs_up_to(bound - 1, bound)?,
        vec![bound],
        "and a batch of one is that one"
    );
    Ok(())
}
