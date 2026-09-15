use super::*;

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
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.unchanged, 2, "hashed files are unchanged files");
    assert_eq!(catalog.totals()?.artifacts, 1);
    fs::write(input.join("a/style.safetensors"), plain_header())?;
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(input.join("a/style.safetensors"))?
        .set_modified(later)?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
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

#[test]
fn a_file_that_blocks_holds_no_other_file_back() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let empty = directory.path().join("empty");
    fs::create_dir_all(&empty)?;
    let config = IndexConfig {
        sources: vec![source(&empty, "files")],
        hash_threads: 2,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;

    let storage = directory.path().join("files");
    fs::create_dir_all(&storage)?;
    let fifo = storage.join("blocked.bin");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map_err(|error| IndexError::Catalog(format!("mkfifo is unavailable: {error}")))?;
    assert!(status.success(), "mkfifo did not create the fifo");

    let mut entries = vec![SeedEntry {
        node: LOCAL_NODE,
        abs_path: Some(fifo.to_string_lossy().into_owned()),
        rel_path: "blocked.bin".to_owned(),
        size: 20 * 1024 * 1024,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: None,
    }];
    for index in 0..300_u32 {
        let name = format!("f{index:03}.bin");
        let path = storage.join(&name);
        fs::write(&path, format!("payload-{index}\n"))?;
        entries.push(SeedEntry {
            node: LOCAL_NODE,
            abs_path: Some(path.to_string_lossy().into_owned()),
            rel_path: name,
            size: fs::metadata(&path)?.len(),
            state: State::Local,
            is_symlink: false,
            sha256: None,
            git_sha1: None,
            artifact: None,
        });
    }
    catalog.seed(&[SeedRepository {
        repo: RepoId::parse_ref(RepoType::Models, "acme/blocked").expect("a valid reference"),
        source: "files".to_owned(),
        kind: SourceKind::Loose,
        root_path: storage.clone(),
        git_dir: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: None,
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
            entries,
        }],
    }])?;
    assert_eq!(catalog.pending_hashes()?, 301);

    let mut fifo_writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)?;

    let report = std::thread::scope(|scope| -> Result<HashReport> {
        let handle = scope.spawn(|| catalog.hash_pending(&config, |_| {}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if catalog.pending_hashes()? == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the blocked file held the rest back"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::io::Write::write_all(&mut fifo_writer, b"abc")?;
        drop(fifo_writer);
        handle
            .join()
            .map_err(|_| IndexError::Catalog("the hashing pass panicked".into()))?
    })?;

    assert_eq!(report.hashed, 301);
    assert_eq!(report.unreadable, 0);
    assert!(!report.interrupted);
    assert_eq!(catalog.pending_hashes()?, 0);
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    let fifo_entry = entries
        .iter()
        .find(|entry| entry.rel_path == "blocked.bin")
        .ok_or_else(|| IndexError::Catalog("the fifo entry vanished".into()))?;
    assert_eq!(
        fifo_entry.sha256,
        Some(crate::hash::sha256_bytes(b"abc")),
        "the fifo's entry carries the hash of what was written to it"
    );
    Ok(())
}

#[test]
fn hash_threads_of_one_hashes_in_primary_key_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(&input)?;
    for index in 0..130_u32 {
        fs::write(
            input.join(format!("f{index:03}.bin")),
            format!("payload-{index}\n"),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        hash_threads: 1,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(catalog.pending_hashes()?, 130);

    let mut counts = Vec::new();
    let report = catalog.hash_pending(&config, |hashed| counts.push(hashed))?;

    assert_eq!(report.hashed, 130);
    assert!(
        counts.windows(2).all(|pair| pair[0] <= pair[1]),
        "the progress counts never go backwards: {counts:?}"
    );
    assert_eq!(counts.last().copied(), Some(130));
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(
        entries.iter().all(|entry| entry.sha256.is_some()),
        "every entry has a sha256 after the pass"
    );
    Ok(())
}

#[test]
fn a_raised_stop_ends_a_hashing_pass_within_a_queue_of_files_and_the_next_pass_finishes_it()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    let batch = crate::catalog::hashing::HASH_BATCH as u64;
    let total = 3 * batch;
    for index in 0..total {
        fs::write(
            input.join(format!("shard{index:04}.csv")),
            format!("prompt,response\nhello,{index}\n"),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        hash_threads: 2,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    assert_eq!(catalog.pending_hashes()?, total);

    let stop = Stop::default();
    let report = catalog.hash_pending_until(&config, &stop, |hashed| {
        if hashed >= batch {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })?;

    assert!(report.interrupted, "the raised stop ended the pass early");
    assert!(
        report.hashed >= batch,
        "the pass ran at least until the stop was observed, got {}",
        report.hashed
    );
    assert!(
        report.hashed < total,
        "the pass did not reach every file, got {}",
        report.hashed
    );
    let left = catalog.pending_hashes()?;
    assert_eq!(
        left,
        total - report.hashed,
        "what it did not reach still owes a hash"
    );
    assert!(
        pass_is_open(&catalog)?,
        "an interrupted pass leaves its mark set for the next one"
    );

    let finished = catalog.hash_pending(&config, |_| {})?;
    assert!(!finished.interrupted);
    assert_eq!(finished.hashed, left, "the next pass hashed the rest");
    assert_eq!(catalog.pending_hashes()?, 0);
    assert!(
        !pass_is_open(&catalog)?,
        "a pass that ran to its end clears the mark"
    );
    Ok(())
}

#[test]
fn a_stop_raised_while_a_file_is_being_read_leaves_that_file_owing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let empty = directory.path().join("empty");
    fs::create_dir_all(&empty)?;
    let config = IndexConfig {
        sources: vec![source(&empty, "files")],
        hash_threads: 1,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;

    let storage = directory.path().join("files");
    fs::create_dir_all(&storage)?;
    let large_path = storage.join("large.bin");
    let large_file = fs::File::create(&large_path)?;
    large_file.set_len(64 * 1024 * 1024)?;
    drop(large_file);

    let mut entries = vec![SeedEntry {
        node: LOCAL_NODE,
        abs_path: Some(large_path.to_string_lossy().into_owned()),
        rel_path: "large.bin".to_owned(),
        size: 64 * 1024 * 1024,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: None,
    }];
    for index in 0..8_u32 {
        let name = format!("f{index}.bin");
        let path = storage.join(&name);
        fs::write(&path, format!("payload-{index}\n"))?;
        entries.push(SeedEntry {
            node: LOCAL_NODE,
            abs_path: Some(path.to_string_lossy().into_owned()),
            rel_path: name,
            size: fs::metadata(&path)?.len(),
            state: State::Local,
            is_symlink: false,
            sha256: None,
            git_sha1: None,
            artifact: None,
        });
    }
    catalog.seed(&[SeedRepository {
        repo: RepoId::parse_ref(RepoType::Models, "acme/large").expect("a valid reference"),
        source: "files".to_owned(),
        kind: SourceKind::Loose,
        root_path: storage.clone(),
        git_dir: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: None,
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
            entries,
        }],
    }])?;
    assert_eq!(catalog.pending_hashes()?, 9);

    let stop = Stop::default();
    let report = catalog.hash_pending_until(&config, &stop, |_| {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    })?;
    assert!(report.interrupted, "the raised stop ended the pass early");

    let read = catalog.database.r_transaction()?;
    let seeded: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    let large_entry = seeded
        .iter()
        .find(|entry| entry.rel_path == "large.bin")
        .ok_or_else(|| IndexError::Catalog("the large file's entry vanished".into()))?;
    assert!(
        large_entry.sha256.is_none(),
        "the stop was raised while the large file was still being read"
    );
    assert!(
        catalog.pending_hashes()? >= 1,
        "the file the stop caught mid-read still owes a hash"
    );

    let finished = catalog.hash_pending(&config, |_| {})?;
    assert!(!finished.interrupted);
    assert_eq!(
        catalog.pending_hashes()?,
        0,
        "a fresh pass finishes what is left"
    );
    Ok(())
}

/// Writes a total the catalog cannot have, so a pass that settles the counts is visible in
/// what `totals()` reads and a pass that does not is visible too.
fn miscount_the_summary(catalog: &Catalog) -> Result<u64> {
    let transaction = catalog.database.rw_transaction()?;
    let stored: models::Summary = transaction
        .get()
        .primary(0_u8)?
        .ok_or_else(|| IndexError::Catalog("summary missing".into()))?;
    let wrong = stored.artifacts + 1_000;
    transaction.upsert(models::Summary {
        artifacts: wrong,
        ..stored
    })?;
    transaction.commit()?;
    Ok(wrong)
}

#[test]
fn a_hashing_pass_asked_to_stop_before_it_starts_opens_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    for index in 0..4 {
        fs::write(
            input.join(format!("row{index}.csv")),
            format!("prompt,response\nhello,{index}\n"),
        )?;
    }
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

    let stop = Stop::default();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let interrupted = catalog.rescan_until(
        &config,
        HashPolicy::Background,
        RescanScope::Configured,
        &stop,
        |_, _| {},
    )?;
    assert!(interrupted.interrupted);
    assert!(pass_is_open(&catalog)?, "the walk left its mark held");
    let wrong = miscount_the_summary(&catalog)?;

    let report = catalog.hash_pending_until(&config, &stop, |_| {})?;

    assert!(report.interrupted, "a raised stop ends the pass at once");
    assert_eq!(report.hashed, 0);
    assert_eq!(
        catalog.totals()?.artifacts,
        wrong,
        "the pass opened nothing, so it counted nothing"
    );
    assert!(
        pass_is_open(&catalog)?,
        "and left the mark for the next one"
    );
    assert_eq!(catalog.pending_hashes()?, 4);

    let finished = catalog.hash_pending(&config, |_| {})?;

    assert!(!finished.interrupted);
    assert_eq!(finished.hashed, 4);
    assert_eq!(
        catalog.totals()?.artifacts,
        wrong - 1_000,
        "the pass that did open settled the counts the interrupted one left"
    );
    assert!(!pass_is_open(&catalog)?);
    Ok(())
}

#[test]
fn a_hashing_pass_counts_triage_against_the_threshold_the_catalog_stored() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/style.safetensors"), lora_header())?;
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    let scanned = IndexConfig {
        sources: vec![source(&input, "files")],
        triage_confidence: 0.5,
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &scanned,
        HashPolicy::Never,
        |_, _| {},
    )?;

    let hashing = IndexConfig {
        triage_confidence: 0.99,
        ..scanned.clone()
    };
    let report = catalog.hash_pending(&hashing, |_| {})?;

    assert_eq!(report.hashed, 2);
    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "the pass counted against the threshold the scan stored, not the one it was handed"
    );
    Ok(())
}
