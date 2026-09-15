use super::*;

/// How many artifacts the stale check must never read to answer.
const LOGGED_CHANGES: usize = 500;

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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
fn counting_the_summary_writes_nothing() -> Result<()> {
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
    let mut catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;

    let truth = catalog.count_summary()?;
    assert_eq!(truth.artifacts, 3, "the scan filed one artifact per file");
    let wrong = crate::catalog::Totals {
        artifacts: truth.artifacts + 7,
        indexed_bytes: truth.indexed_bytes + 4096,
        ..truth.clone()
    };
    catalog.drift_summary(&wrong)?;

    assert_eq!(
        catalog.count_summary()?,
        truth,
        "a count reads the records, not the stored totals"
    );
    assert_eq!(
        catalog.totals()?,
        wrong,
        "and it leaves the drifted totals where they were"
    );

    assert_eq!(
        catalog.repair_summary()?,
        (wrong, truth.clone()),
        "a repair reports what was stored and what it stored"
    );
    assert_eq!(
        catalog.totals()?,
        truth,
        "the stored totals are the counted"
    );
    assert_eq!(
        catalog.repair_summary()?,
        (truth.clone(), truth),
        "a second repair finds nothing to move"
    );
    Ok(())
}

#[test]
fn the_summary_is_exact_after_incremental_passes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..4 {
        fs::write(
            input.join(format!("w{index}.safetensors")),
            lora_header_named(&format!("t{index}")),
        )?;
    }
    fs::write(input.join("unclassified.safetensors"), plain_header())?;
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
    let exact = |stage: &str| -> Result<()> {
        let stored = catalog.totals()?;
        let counted = catalog.count_summary()?;
        assert_eq!(stored, counted, "the summary drifted after {stage}");
        Ok(())
    };
    exact("a scan")?;

    catalog.hash_pending(&config, |_| {})?;
    exact("hashing")?;

    let mirrored = RepoId::parse_ref(RepoType::Models, "openai/whisper")?;
    let served = catalog.ingest_served(&ServedFile {
        repo: mirrored,
        commit: CommitSha::from_bytes([0x11; 20]),
        rel_path: "model.safetensors".to_owned(),
        size: 4096,
        sha256: Sha256::from_bytes([0x42; 32]),
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    })?;
    exact("an ingest")?;

    let before_retirement = catalog.totals()?;
    assert_eq!(catalog.retire_entries(&[served.entry])?, 1);
    exact("a retirement")?;
    assert_eq!(
        catalog.totals()?.present_artifacts,
        before_retirement.present_artifacts - 1,
        "a retired path leaves the artifact holding none"
    );

    catalog.rebuild_search(|_| {})?;
    let queued = catalog
        .triage_queue(10)?
        .first()
        .map(|item| item.row.artifact_id)
        .ok_or_else(|| IndexError::Catalog("the unclassified file queues".into()))?;
    let before_verdict = catalog.totals()?;
    assert!(before_verdict.triage_artifacts > 0);
    catalog.put_override(queued, Some(Role::Lora), None, None)?;
    exact("an override")?;
    assert_eq!(
        catalog.totals()?.triage_artifacts,
        before_verdict.triage_artifacts - 1,
        "a verdict takes the artifact out of the triage total"
    );
    assert!(catalog.clear_override(queued)?);
    exact("a cleared override")?;

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
    exact("a digest")?;

    let next = catalog.begin_pass(laptop.id)?;
    assert_eq!(catalog.retract_digest(laptop.id, next)?, 1);
    exact("a retraction")?;

    fs::remove_file(input.join("w0.safetensors"))?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    exact("a rescan that dropped a path")?;

    let before = catalog.totals()?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.totals()?,
        before,
        "a rescan that changed nothing changes nothing"
    );
    exact("a rescan that touched nothing")?;
    Ok(())
}

#[test]
fn new_and_existing_artifacts_keep_counts_and_search_changes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    fs::write(input.join("a.safetensors"), lora_header_named("a"))?;
    fs::write(input.join("b.safetensors"), lora_header_named("b"))?;
    fs::write(input.join("b-copy.safetensors"), lora_header_named("b"))?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let fresh = search(&catalog, "")?.total;
    assert_eq!(fresh, 3, "three fresh artifacts, none shares an identity");

    let unchanged = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(unchanged.changed, 0, "nothing changed: {unchanged:?}");
    assert_eq!(unchanged.added, 0);
    assert_eq!(unchanged.absent, 0);
    assert_eq!(
        catalog.pending_changes()?,
        0,
        "a pass that wrote nothing logs nothing"
    );
    assert_eq!(search(&catalog, "")?.total, fresh);

    fs::write(input.join("a.safetensors"), lora_header_named("changed"))?;
    fs::write(input.join("c.safetensors"), lora_header_named("c"))?;
    fs::remove_file(input.join("b-copy.safetensors"))?;
    let moved = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(moved.changed, 1, "a rewritten file: {moved:?}");
    assert_eq!(moved.added, 1, "a new file: {moved:?}");
    assert_eq!(moved.absent, 1, "a removed file: {moved:?}");
    assert!(
        catalog.pending_changes()? >= 3,
        "every touched artifact is logged"
    );
    catalog.refresh_search(|_| {})?;
    assert_eq!(
        search(&catalog, "")?.total,
        3,
        "a, c and b survive; the copy of b was removed"
    );
    let totals = catalog.count_summary()?;
    assert_eq!(
        totals.artifacts,
        search(&catalog, "")?.total,
        "the summary counts what the index holds"
    );
    Ok(())
}

#[test]
fn a_scan_before_the_first_publish_logs_nothing_and_logs_again_after_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..8 {
        fs::write(
            input.join(format!("w{index}.safetensors")),
            lora_header_named(&format!("t{index}")),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<SearchChange>()?,
        0,
        "a scan with no index to be a difference against logs nothing"
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 8);
    drop(read);
    assert_eq!(catalog.pending_changes()?, 0);
    assert!(!catalog.status()?.search_built);

    let ingested = catalog.ingest_served(&ServedFile {
        repo: RepoId::parse_ref(RepoType::Models, "acme/served")?,
        commit: CommitSha::from_bytes([0x21; 20]),
        rel_path: "unet/model.safetensors".to_owned(),
        size: 4096,
        sha256: Sha256::from_bytes([0x51; 32]),
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    })?;
    assert!(
        ingested.artifact.is_some(),
        "an ingest before the first publish still writes its rows"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<SearchChange>()?,
        0,
        "so does every other writer that runs before the first publish"
    );
    drop(read);

    catalog.rebuild_search(|_| {})?;
    assert!(catalog.status()?.search_built);
    let built = search(&catalog, "")?.total;
    assert_eq!(
        built, 9,
        "the rebuild read the records, not the log: eight scanned and one served"
    );

    fs::write(input.join("w0.safetensors"), lora_header_named("changed"))?;
    fs::write(input.join("w8.safetensors"), lora_header_named("t8"))?;
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.changed, 1);
    assert_eq!(report.added, 1);
    let logged = catalog.pending_changes()?;
    assert!(
        logged >= 2,
        "a pass after a publish logs every artifact it touched: {logged}"
    );
    let applied = catalog.refresh_search(|_| {})?;
    assert_eq!(applied, logged, "and the refresh applies exactly those");
    assert_eq!(catalog.pending_changes()?, 0);
    assert_eq!(search(&catalog, "")?.total, 10);
    Ok(())
}

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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(catalog.sources()?[0].pending_hashes, 1);
    assert_eq!(catalog.pending_hashes()?, 1);
    Ok(())
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.sources()?[0].file_count,
        1,
        "absent rows are not counted"
    );
    Ok(())
}

#[test]
fn a_catalog_counts_triage_against_the_threshold_it_was_scanned_with() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/style.safetensors"), lora_header())?;
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let lenient = IndexConfig {
        sources: vec![source(&input, "files")],
        triage_confidence: 0.5,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &lenient,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "a fresh scan at 0.5 counts what a recount of it counts"
    );
    let lenient_triage = catalog.totals()?.triage_artifacts;

    catalog.rescan(
        &lenient,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "a rescan at the stored threshold leaves the count where it was"
    );

    let strict = IndexConfig {
        triage_confidence: 0.99,
        ..lenient.clone()
    };
    catalog.rescan(
        &strict,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "a rescan that moves the threshold counts the summary again"
    );
    assert!(
        catalog.totals()?.triage_artifacts > lenient_triage,
        "a threshold of 0.99 puts more artifacts in triage than 0.5 did"
    );
    Ok(())
}

fn rewrite_hf_cache_file(
    root: &Path,
    repository: (&str, &str),
    rel_path: &str,
    blob_prefix: &str,
    content: &[u8],
) -> Result<()> {
    let (org, name) = repository;
    let repo = root.join(format!("models--{org}--{name}"));
    let blob = repo.join("blobs").join(blob_prefix.repeat(64));
    fs::write(&blob, content)?;
    let link = repo.join("snapshots").join("b".repeat(40)).join(rel_path);
    fs::remove_file(&link)?;
    std::os::unix::fs::symlink(&blob, &link)?;
    Ok(())
}

#[test]

fn the_summary_is_exact_when_a_pass_touches_an_artifact_more_than_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shared = lora_header_named("shared");
    hf_cache_repo(
        &hub,
        "acme",
        "one",
        &[("adapter.safetensors", shared.clone())],
    )?;
    hf_cache_repo(
        &hub,
        "acme",
        "two",
        &[("adapter.safetensors", shared.clone())],
    )?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(
        catalog.totals()?,
        catalog.count_summary()?,
        "two copies discovered by one pass, in two transactions, telescope to the same total"
    );
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "identical content is one artifact");
    assert_eq!(rows.rows[0].path_count, 2);

    let moved = lora_header_named("moved");
    rewrite_hf_cache_file(&hub, ("acme", "one"), "adapter.safetensors", "e", &moved)?;
    rewrite_hf_cache_file(&hub, ("acme", "two"), "adapter.safetensors", "e", &moved)?;
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.changed, 2, "both copies were rewritten");

    assert_eq!(
        catalog.totals()?,
        catalog.count_summary()?,
        "the old artifact lost its second and then its last path in two transactions of one rescan"
    );
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the rewritten copies are one artifact again");
    assert_eq!(rows.rows[0].path_count, 2);
    Ok(())
}

#[test]

fn a_pass_holds_no_summary_snapshot_once_its_transaction_commits() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..4 {
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
    let ids = artifact_ids(&catalog)?;
    assert!(ids.len() >= 2, "the fixture needs more than one artifact");
    let triage = catalog.stored_triage()?;
    let mut counters = Counters::load(&catalog.database, triage)?;
    for id in &ids {
        let transaction = catalog.database.rw_transaction()?;
        counters.touch(&transaction, *id)?;
        counters.settle_summary(&transaction)?;
        transaction.commit()?;
        assert!(
            counters.holds_no_summary_snapshot(),
            "the snapshot for artifact {id} outlived the transaction that settled it"
        );
    }
    Ok(())
}

#[test]

fn a_pull_that_creates_a_repository_settles_the_count_in_the_same_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = Catalog::open_or_scan(
        &IndexConfig {
            data_dir: directory.path().join("native"),
            ..IndexConfig::default()
        },
        "workstation",
        |_, _| {},
    )?;
    let before = catalog.totals()?.repositories;
    let repo = RepoId::parse_ref(RepoType::Models, "kohya/film-grain")?;
    let root = directory.path().join("pulled");
    catalog.pull_slot(&repo, CommitSha::from_bytes([0x22; 20]), &root)?;
    assert_eq!(
        catalog.totals()?.repositories,
        before + 1,
        "the repository the pull created moved the stored count in its own transaction"
    );
    assert_eq!(
        catalog.totals()?,
        catalog.count_summary()?,
        "no pass ran between the pull and this read to settle it another way"
    );
    Ok(())
}

#[test]
fn summary_changes_roll_back_with_the_artifact_transaction() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let before = catalog.totals()?;
    let mut counters = Counters::load(&catalog.database, catalog.stored_triage()?)?;
    let transaction = catalog.database.rw_transaction()?;
    let mut record: ArtifactRecord = transaction
        .get()
        .primary(ids.lora.get())?
        .ok_or_else(|| IndexError::Catalog("the seeded artifact is missing".into()))?;
    counters.touch(&transaction, record.id)?;
    record.artifact.size += 1024;
    transaction.upsert(record)?;
    counters.save(&transaction)?;
    let staged: models::Summary = transaction
        .get()
        .primary(0_u8)?
        .ok_or_else(|| IndexError::Catalog("the staged summary is missing".into()))?;
    assert_eq!(staged.indexed_bytes, before.indexed_bytes + 1024);
    drop(transaction);
    assert_eq!(catalog.totals()?, before);
    assert_eq!(catalog.count_summary()?, before);
    Ok(())
}

#[test]
fn taking_touched_artifacts_deduplicates_and_summary_capture_does_not_requeue() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let before = catalog.totals()?;
    let mut counters = Counters::load(&catalog.database, catalog.stored_triage()?)?;
    assert!(counters.take_touched().is_empty());
    let transaction = catalog.database.rw_transaction()?;
    for id in [ids.lora, ids.lora, ids.ckpt] {
        counters.touch(&transaction, id.get())?;
    }
    counters.save(&transaction)?;
    transaction.commit()?;
    let touched = counters.take_touched();
    assert_eq!(touched.len(), 2);
    assert!(touched.contains(&ids.lora.get()));
    assert!(touched.contains(&ids.ckpt.get()));
    assert!(counters.take_touched().is_empty());

    let transaction = catalog.database.rw_transaction()?;
    counters.capture_summary(&transaction, ids.lora.get())?;
    let mut record: ArtifactRecord = transaction
        .get()
        .primary(ids.lora.get())?
        .ok_or_else(|| IndexError::Catalog("the seeded artifact is missing".into()))?;
    record.artifact.size += 1024;
    transaction.upsert(record)?;
    counters.capture_summary(&transaction, ids.lora.get())?;
    counters.save(&transaction)?;
    transaction.commit()?;
    assert_eq!(catalog.totals()?.indexed_bytes, before.indexed_bytes + 1024);
    assert_eq!(catalog.totals()?, catalog.count_summary()?);
    assert!(counters.take_touched().is_empty());
    assert!(counters.holds_no_summary_snapshot());

    let transaction = catalog.database.rw_transaction()?;
    counters.touch(&transaction, ids.lora.get())?;
    counters.save(&transaction)?;
    transaction.commit()?;
    assert_eq!(counters.take_touched(), [ids.lora.get()].into());
    Ok(())
}

#[test]
fn sweeping_consumes_touched_work_across_transaction_boundaries() -> Result<()> {
    let total = crate::catalog::scan::CHUNK + 7;
    let (_directory, catalog, _config) = csv_catalog_with_entries(total)?;
    let before = catalog.totals()?;
    let mut counters = Counters::load(&catalog.database, catalog.stored_triage()?)?;
    let transaction = catalog.database.rw_transaction()?;
    for id in artifact_ids(&catalog)? {
        counters.touch(&transaction, id)?;
    }
    counters.save(&transaction)?;
    transaction.commit()?;
    catalog.sweep_artifacts(&mut counters)?;
    assert!(counters.take_touched().is_empty());
    assert!(counters.holds_no_summary_snapshot());
    assert_eq!(catalog.totals()?, before);
    assert_eq!(catalog.count_summary()?, before);
    catalog.sweep_artifacts(&mut counters)?;
    assert_eq!(catalog.totals()?, before);
    Ok(())
}

#[test]
fn retirement_recounts_every_artifact_across_transaction_boundaries() -> Result<()> {
    let total = crate::catalog::scan::CHUNK + 7;
    let (_directory, catalog, _config) = csv_catalog_with_entries(total)?;
    let read = catalog.database.r_transaction()?;
    let ids = read
        .scan()
        .primary::<Entry>()?
        .all()?
        .map(|entry| Ok(EntryId::new(entry?.id)))
        .collect::<Result<Vec<_>>>()?;
    drop(read);
    assert_eq!(ids.len(), total);
    assert_eq!(catalog.retire_entries(&ids)?, total);
    assert_eq!(catalog.totals()?.artifacts, total as u64);
    assert_eq!(catalog.totals()?.present_artifacts, 0);
    assert_eq!(catalog.totals()?, catalog.count_summary()?);
    let read = catalog.database.r_transaction()?;
    for entry in read.scan().primary::<Entry>()?.all()? {
        assert_eq!(entry?.state, State::Remote);
    }
    for artifact in read.scan().primary::<ArtifactRecord>()?.all()? {
        assert_eq!(artifact?.path_count, 0);
    }
    drop(read);
    assert_eq!(catalog.retire_entries(&ids)?, 0);
    Ok(())
}

#[test]
fn source_count_recovery_preserves_timestamps_and_non_scan_sources() -> Result<()> {
    let (_directory, catalog, config) = csv_catalog_with_entries(2)?;
    let transaction = catalog.database.rw_transaction()?;
    let original: Source = transaction
        .get()
        .primary(config.sources[0].name.clone())?
        .ok_or_else(|| IndexError::Catalog("the scanned source is missing".into()))?;
    transaction.upsert(Source {
        file_count: 99,
        byte_count: 999,
        ..original.clone()
    })?;
    let mut empty = original.clone();
    empty.name = "empty-source".to_owned();
    empty.config.name = empty.name.clone();
    empty.file_count = 13;
    empty.byte_count = 17;
    empty.pending_hashes = 0;
    transaction.upsert(empty.clone())?;
    let mut protected = Vec::with_capacity(3);
    for name in [
        MIRROR_SOURCE.to_owned(),
        crate::catalog::PULL_SOURCE.to_owned(),
        format!("{}test-peer", crate::catalog::PEER_SOURCE),
    ] {
        let mut source = empty.clone();
        source.name = name;
        source.config.name = source.name.clone();
        transaction.upsert(source.clone())?;
        protected.push(source);
    }
    transaction.commit()?;
    catalog.open_pass()?;
    catalog.open_pass()?;
    catalog.close_pass()?;

    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.get().primary::<Source>(original.name.clone())?,
        Some(original)
    );
    empty.file_count = 0;
    empty.byte_count = 0;
    assert_eq!(
        read.get().primary::<Source>(empty.name.clone())?,
        Some(empty)
    );
    for source in protected {
        assert_eq!(
            read.get().primary::<Source>(source.name.clone())?,
            Some(source)
        );
    }
    Ok(())
}
