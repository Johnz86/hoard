use super::*;

/// What an interrupted stage panics with, so a test that catches another panic can say
/// so rather than pass.
const KILLED: &str = "the catalog stage was killed";

/// Everything two runs of a stage must agree on: the counts the catalog keeps, the
/// counts a fresh count of every record gives, what each source, repository and node
/// was left holding, the artifacts and entries on record, and the first page of the
/// default query. Timestamps and job rows are not here: they differ between two runs
/// that agree on everything else.
#[derive(Debug, PartialEq)]
struct Fingerprint {
    totals: Totals,
    counted: Totals,
    sources: Vec<(String, u64, u64)>,
    repositories: Vec<(String, u32, u64)>,
    nodes: Vec<(i64, u64)>,
    artifacts: Vec<String>,
    entries: Vec<String>,
    page: Vec<String>,
    matched: u64,
}

fn fingerprint(catalog: &Catalog) -> Result<Fingerprint> {
    let read = catalog.database.r_transaction()?;
    let mut sources = Vec::new();
    for source in read.scan().primary::<Source>()?.all()? {
        let source: Source = source?;
        sources.push((source.name, source.file_count, source.pending_hashes));
    }
    sources.sort();
    let mut repositories = Vec::new();
    for repository in read.scan().primary::<Repository>()?.all()? {
        let repository: Repository = repository?;
        let key = repository.key;
        repositories.push((key, repository.present_files, repository.present_bytes));
    }
    repositories.sort();
    let mut nodes = Vec::new();
    for node in read.scan().primary::<Node>()?.all()? {
        let node: Node = node?;
        nodes.push((node.id, node.filed));
    }
    nodes.sort();
    let mut artifacts = Vec::new();
    for artifact in read.scan().primary::<ArtifactRecord>()?.all()? {
        let artifact: ArtifactRecord = artifact?;
        artifacts.push(format!(
            "{} {} paths {}",
            artifact.id, artifact.name, artifact.path_count
        ));
    }
    artifacts.sort();
    let mut entries = Vec::new();
    for entry in read.scan().primary::<Entry>()?.all()? {
        let entry: Entry = entry?;
        entries.push(format!(
            "{} repo {} revision {} {} {} bytes {:?} {}",
            entry.id,
            entry.repo_id,
            entry.revision,
            entry.rel_path,
            entry.size,
            entry.state,
            entry
                .sha256
                .map_or_else(|| "unhashed".to_owned(), |sha| sha.to_hex())
        ));
    }
    entries.sort();
    drop(read);
    let page = search(catalog, "")?;
    Ok(Fingerprint {
        totals: catalog.totals()?,
        counted: catalog.count_summary()?,
        sources,
        repositories,
        nodes,
        artifacts,
        entries,
        page: page.rows.iter().map(|row| row.path.clone()).collect(),
        matched: page.total,
    })
}

/// Runs `stage` with a progress callback that, on its `at`-th call, copies the catalog
/// directory to `snapshot` and then panics; returns the panic payload's text and leaves
/// the catalog as the stage left it.
fn interrupt<T>(
    catalog_dir: &Path,
    snapshot: &Path,
    at: usize,
    stage: impl FnOnce(&mut (dyn FnMut() + Send)) -> Result<T>,
) -> String {
    let mut calls = 0_usize;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut kill = || {
            calls += 1;
            if calls == at {
                copy_tree(catalog_dir, snapshot).expect("the catalog directory copies");
                panic!("{KILLED}");
            }
        };
        stage(&mut kill)
    }));
    match outcome {
        Ok(_) => panic!("the stage finished without reaching progress call {at}"),
        Err(payload) => payload_text(payload),
    }
}

fn payload_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic carrying no text".to_owned())
}

/// A plain recursive copy, which is what a power loss leaves: every file as its last
/// write left it, and a Tantivy directory exactly as the build had written it so far.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        match entry.file_type()?.is_dir() {
            true => copy_tree(&entry.path(), &target)?,
            false => {
                fs::copy(entry.path(), &target)?;
            }
        }
    }
    Ok(())
}

/// Three repositories of three revisions of three files. A revision is one unit, so the
/// walk commits nine times and an interruption can land between two of them; thirty
/// files in one revision would be one chunk, one commit and nothing to land between.
fn hub_fixture(root: &Path) -> Result<IndexConfig> {
    let hub = root.join("hub");
    fs::create_dir_all(&hub)?;
    for repo in 0..3_u8 {
        for revision in 0..3_u8 {
            for file in 0..3_u8 {
                file_in_revision(&hub, repo, revision, &format!("part{file}.safetensors"));
            }
        }
    }
    Ok(IndexConfig {
        data_dir: root.to_path_buf(),
        ..hub_config(&hub)
    })
}

/// Files one more weight into one revision of one repository, leaving what is already
/// there alone.
fn file_in_revision(hub: &Path, repo: u8, revision: u8, name: &str) {
    let body = lora_header_named(&format!("layer.{repo}.{revision}.{name}.weight"));
    crate::discover::hf_cache::fixture::write_repo(
        hub,
        &repo_directory(repo),
        &revision_sha(repo, revision).to_hex(),
        &[(name, &body)],
    );
}

fn repo_directory(repo: u8) -> String {
    format!("models--acme--m{repo}")
}

fn revision_sha(repo: u8, revision: u8) -> CommitSha {
    CommitSha::from_bytes([repo * 16 + revision; 20])
}

/// The hub fixture with a loose folder walked before it, and every file hashed as it is
/// scanned. A loose file rewritten in place keeps the path the walk knows it by and takes
/// the identity of its new bytes, which is how one unit transaction takes the last path
/// from an artifact; the hub's nine revisions are what a kill lands between.
fn mixed_fixture(root: &Path) -> Result<IndexConfig> {
    let config = hub_fixture(root)?;
    let loose = root.join("files");
    fs::create_dir_all(&loose)?;
    for file in 0..3_u8 {
        fs::write(
            loose.join(format!("weights{file}.safetensors")),
            lora_header_named(&format!("loose.{file}.weight")),
        )?;
    }
    let mut sources = vec![source(&loose, "files")];
    sources.extend(config.sources.iter().cloned());
    Ok(IndexConfig {
        sources,
        hash_policy: HashPolicy::Now,
        ..config
    })
}

/// Rewrites one loose file with longer bytes, so its stamp moves whatever the clock did
/// and its content names another artifact.
fn rewrite_loose(root: &Path, name: &str) -> Result<()> {
    fs::write(
        root.join("files").join(name),
        lora_header_named("rewritten.with.a.longer.tensor.name.weight"),
    )?;
    Ok(())
}

/// The nine revisions of the fixture, each with one more file than the scan recorded.
fn file_into_every_revision(hub: &Path, name: &str) {
    for repo in 0..3_u8 {
        for revision in 0..3_u8 {
            file_in_revision(hub, repo, revision, name);
        }
    }
}

fn scanned(config: &IndexConfig) -> Result<Catalog> {
    fs::create_dir_all(&config.data_dir)?;
    let catalog = Catalog::scan_sources(
        &config.native_path(),
        "n",
        config,
        config.hash_policy,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

/// The allocator is the high-water mark every pass hands ids out from. An id above it
/// is an id the next pass hands out again, onto the row already holding it.
fn ids_stay_within_the_allocator(catalog: &Catalog) -> Result<()> {
    let read = catalog.database.r_transaction()?;
    let allocator: Allocator = read
        .get()
        .primary(0_u8)?
        .ok_or_else(|| IndexError::Catalog("the catalog has no allocator".into()))?;
    for entry in read.scan().primary::<Entry>()?.all()? {
        let entry: Entry = entry?;
        assert!(
            u64::try_from(entry.id).unwrap_or(u64::MAX) <= allocator.entries,
            "entry {} is above the allocator at {}",
            entry.id,
            allocator.entries
        );
    }
    for artifact in read.scan().primary::<ArtifactRecord>()?.all()? {
        let artifact: ArtifactRecord = artifact?;
        assert!(
            u64::try_from(artifact.id).unwrap_or(u64::MAX) <= allocator.artifacts,
            "artifact {} is above the allocator at {}",
            artifact.id,
            allocator.artifacts
        );
    }
    for repository in read.scan().primary::<Repository>()?.all()? {
        let repository: Repository = repository?;
        assert!(
            u64::try_from(repository.id).unwrap_or(u64::MAX) <= allocator.repos,
            "repository {} is above the allocator at {}",
            repository.id,
            allocator.repos
        );
    }
    Ok(())
}

/// What every source owes a hasher, counted from the entries themselves, against what
/// the writers left on the source rows.
fn owed_hashes_match_the_entries(catalog: &Catalog) -> Result<()> {
    let read = catalog.database.r_transaction()?;
    let mut roots: BTreeMap<i64, String> = BTreeMap::new();
    for repository in read.scan().primary::<Repository>()?.all()? {
        let repository: Repository = repository?;
        roots.insert(repository.id, repository.source);
    }
    let mut counted: BTreeMap<String, u64> = BTreeMap::new();
    let mut present: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    for entry in read.scan().primary::<Entry>()?.all()? {
        let entry: Entry = entry?;
        let Some(source) = roots.get(&entry.repo_id) else {
            continue;
        };
        if crate::catalog::scan::owes_a_hash(&entry) {
            *counted.entry(source.clone()).or_default() += 1;
        }
        if entry.state != State::Remote {
            let slot = present.entry(source.clone()).or_default();
            slot.0 += 1;
            slot.1 += entry.size;
        }
    }
    for source in read.scan().primary::<Source>()?.all()? {
        let source: Source = source?;
        let owed = counted.get(&source.name).copied().unwrap_or_default();
        assert_eq!(
            source.pending_hashes, owed,
            "source {} owes {} hashes and records {}",
            source.name, owed, source.pending_hashes
        );
        let held = present.get(&source.name).copied().unwrap_or_default();
        assert_eq!(
            (source.file_count, source.byte_count),
            (u64::from(held.0), held.1),
            "source {} holds {:?} and records {:?}",
            source.name,
            held,
            (source.file_count, source.byte_count)
        );
    }
    Ok(())
}

/// Every artifact on record is reached by a path. An artifact whose last path a pass took
/// away is swept in the transaction that took it, so a kill cannot leave one counting a
/// path nothing backs: the next run skips the rows the killed one wrote and would never
/// look at it again.
fn artifacts_are_reached_by_a_path(catalog: &Catalog) -> Result<()> {
    let read = catalog.database.r_transaction()?;
    let mut reached: BTreeSet<i64> = BTreeSet::new();
    for entry in read.scan().primary::<Entry>()?.all()? {
        let entry: Entry = entry?;
        if let Some(id) = entry.artifact_id {
            reached.insert(id);
        }
    }
    for artifact in read.scan().primary::<ArtifactRecord>()?.all()? {
        let artifact: ArtifactRecord = artifact?;
        assert!(
            reached.contains(&artifact.id),
            "artifact {} named {} is reached by no path",
            artifact.id,
            artifact.name
        );
    }
    Ok(())
}

/// Everything a resumed run must agree with a clean one about, beyond the fingerprint:
/// the derived counts no reader recomputes and the ids the next pass depends on.
fn is_consistent(catalog: &Catalog) -> Result<()> {
    ids_stay_within_the_allocator(catalog)?;
    owed_hashes_match_the_entries(catalog)?;
    artifacts_are_reached_by_a_path(catalog)?;
    assert_eq!(
        catalog.totals()?,
        catalog.count_summary()?,
        "the stored totals are the counted ones"
    );
    Ok(())
}

fn rescanned(catalog: &Catalog, config: &IndexConfig) -> Result<()> {
    catalog.rescan(
        config,
        config.hash_policy,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;
    Ok(())
}

/// The nine unit commits of the fixture's walk, plus the call discovery makes: the
/// second, the fifth and the last but one of them.
const KILL_AT: [usize; 3] = [2, 5, 9];

#[test]
fn a_scan_killed_mid_walk_completes_on_the_next_run() -> Result<()> {
    for at in KILL_AT {
        let directory = tempfile::tempdir()?;
        let config = hub_fixture(directory.path())?;
        let native = config.native_path();
        let clean_config = IndexConfig {
            data_dir: directory.path().join("clean"),
            ..config.clone()
        };
        let expected = {
            let clean = scanned(&clean_config)?;
            is_consistent(&clean)?;
            fingerprint(&clean)?
        };

        let snapshot = directory.path().join("copy");
        let text = interrupt(&native, &snapshot, at, |progress| {
            Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| progress())
        });
        assert!(text.contains(KILLED), "the harness killed the walk at {at}");
        assert!(
            Catalog::open(&native).is_err(),
            "a walk that never wrote a manifest is not a catalog"
        );

        let resumed = Catalog::open_or_scan(&config, "n", |_, _| {})?;
        is_consistent(&resumed)?;
        assert_eq!(
            fingerprint(&resumed)?,
            expected,
            "the run after a kill at {at} completes to what a clean run holds"
        );
    }
    Ok(())
}

#[test]
fn a_rescan_killed_mid_walk_leaves_the_previous_generation_published_and_completes() -> Result<()> {
    for at in KILL_AT {
        let directory = tempfile::tempdir()?;
        let config = mixed_fixture(directory.path())?;
        let hub = directory.path().join("hub");
        let native = config.native_path();
        let clean_config = IndexConfig {
            data_dir: directory.path().join("clean"),
            ..config.clone()
        };
        let clean = scanned(&clean_config)?;
        let catalog = scanned(&config)?;
        let published = catalog.manifest()?.search_directory;

        file_into_every_revision(&hub, "extra.safetensors");
        rewrite_loose(directory.path(), "weights0.safetensors")?;
        fs::remove_file(directory.path().join("files/weights1.safetensors"))?;
        rescanned(&clean, &clean_config)?;
        let expected = fingerprint(&clean)?;
        is_consistent(&clean)?;

        let snapshot = directory.path().join("copy");
        let text = interrupt(&native, &snapshot, at, |progress| {
            catalog.rescan(
                &config,
                config.hash_policy,
                RescanScope::Configured,
                |_, _| progress(),
            )
        });
        assert!(text.contains(KILLED), "the harness killed the walk at {at}");
        assert_eq!(
            catalog.status()?.generation,
            1,
            "the killed rescan published no generation of its own"
        );
        assert_eq!(
            catalog.manifest()?.search_directory,
            published,
            "and left the index of the generation before it published"
        );
        drop(catalog);

        let resumed = Catalog::open(&native)?;
        rescanned(&resumed, &config)?;
        is_consistent(&resumed)?;
        assert_eq!(
            fingerprint(&resumed)?,
            expected,
            "the rescan after a kill at {at} completes to what a clean one holds"
        );
        assert_eq!(
            resumed.status()?.generation,
            2,
            "one generation was published"
        );
        drop(resumed);

        let copied = Catalog::open(&snapshot)?;
        rescanned(&copied, &config)?;
        is_consistent(&copied)?;
        assert_eq!(
            fingerprint(&copied)?,
            expected,
            "a catalog copied mid-walk at {at} completes to the same"
        );
    }
    Ok(())
}

/// One repository of one revision of many files. A hash pass reads sixty-four files
/// between two progress calls, so interrupting between its batches needs more than that.
fn hashable_fixture(root: &Path, files: usize) -> Result<IndexConfig> {
    let hub = root.join("hub");
    fs::create_dir_all(&hub)?;
    for file in 0..files {
        file_in_revision(&hub, 0, 0, &format!("part{file:03}.safetensors"));
    }
    Ok(IndexConfig {
        data_dir: root.to_path_buf(),
        ..hub_config(&hub)
    })
}

fn changes(catalog: &Catalog) -> Result<Vec<SearchChange>> {
    let read = catalog.database.r_transaction()?;
    Ok(read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?)
}

/// Puts back the log rows a refresh pruned and the cursor it advanced, which is the
/// state a kill between a Tantivy commit and the manifest leaves: the documents are in
/// the index, and nothing durable says so.
fn rewind_cursor(catalog: &Catalog, to: u64, rows: &[SearchChange]) -> Result<()> {
    let transaction = catalog.database.rw_transaction()?;
    let manifest = catalog.manifest()?;
    transaction.upsert(Manifest {
        search_seq: to,
        ..manifest
    })?;
    for row in rows {
        transaction.upsert(row.clone())?;
    }
    transaction.commit()?;
    Ok(())
}

#[test]
fn a_hash_pass_killed_between_batches_resumes_without_rehashing_what_it_stored() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = hashable_fixture(directory.path(), 150)?;
    let native = config.native_path();
    let clean_config = IndexConfig {
        data_dir: directory.path().join("clean"),
        ..config.clone()
    };
    let expected = {
        let clean = scanned(&clean_config)?;
        clean.hash_pending(&clean_config, |_| {})?;
        clean.refresh_search(|_| {})?;
        is_consistent(&clean)?;
        fingerprint(&clean)?
    };

    let catalog = scanned(&config)?;
    let owed = catalog.pending_hashes()?;
    assert_eq!(owed, 150, "no file was hashed by the scan");
    let snapshot = directory.path().join("copy");
    let text = interrupt(&native, &snapshot, 2, |progress| {
        catalog.hash_pending(&config, |_| progress())
    });
    assert!(text.contains(KILLED), "the harness killed the hash pass");
    assert_eq!(
        catalog.totals()?,
        catalog.count_summary()?,
        "committed hash merges have matching summary deltas before recovery"
    );
    drop(catalog);

    let resumed = Catalog::open(&native)?;
    owed_hashes_match_the_entries(&resumed)?;
    let left = resumed.pending_hashes()?;
    assert!(
        left < owed,
        "the digests the reader sent are committed by the writer draining behind it"
    );
    let report = resumed.hash_pending(&config, |_| {})?;
    assert_eq!(
        report.hashed, left,
        "the next pass reads what is left and not what was stored"
    );
    assert_eq!(resumed.pending_hashes()?, 0, "nothing is left owing");
    resumed.refresh_search(|_| {})?;
    is_consistent(&resumed)?;
    assert_eq!(
        fingerprint(&resumed)?,
        expected,
        "the hash pass after a kill completes to what a clean one holds"
    );
    Ok(())
}

#[test]
fn a_refresh_resumes_at_its_callback_and_replays_a_batch_rewound_by_hand() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = hub_fixture(directory.path())?;
    let hub = directory.path().join("hub");
    let native = config.native_path();
    let catalog = scanned(&config)?;
    file_into_every_revision(&hub, "extra.safetensors");
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    let logged = changes(&catalog)?;
    assert!(!logged.is_empty(), "the rescan logged what it changed");
    let cursor = catalog.manifest()?.search_seq;

    let snapshot = directory.path().join("copy");
    let text = interrupt(&native, &snapshot, 1, |progress| {
        catalog.refresh_search(|_| progress())
    });
    assert!(text.contains(KILLED), "the harness killed the refresh");
    assert_eq!(
        catalog.pending_changes()?,
        0,
        "the batch it applied is pruned and its cursor advanced"
    );
    drop(catalog);

    let resumed = Catalog::open(&native)?;
    assert_eq!(
        resumed.refresh_search(|_| {})?,
        0,
        "nothing is left to apply"
    );
    let expected = fingerprint(&resumed)?;
    assert_eq!(expected.matched, 36, "every artifact answers");

    rewind_cursor(&resumed, cursor, &logged)?;
    assert_eq!(
        resumed.refresh_search(|_| {})?,
        logged.len() as u64,
        "a cursor the kill left behind replays the batch the index already holds"
    );
    assert_eq!(resumed.pending_changes()?, 0, "and prunes it again");
    assert_eq!(
        fingerprint(&resumed)?,
        expected,
        "replaying a batch changes no document"
    );

    let applied = resumed.manifest()?.search_seq;
    let stale = SearchChange {
        seq: applied,
        artifact_id: 1,
    };
    let transaction = resumed.database.rw_transaction()?;
    transaction.upsert(stale)?;
    transaction.commit()?;
    assert_eq!(
        resumed.pending_changes()?,
        1,
        "a kill between the cursor and the prune leaves a row the cursor has passed"
    );
    assert_eq!(
        resumed.refresh_search(|_| {})?,
        0,
        "it is not applied again"
    );
    assert_eq!(
        resumed.pending_changes()?,
        0,
        "the next refresh drops what the cursor has passed"
    );
    assert!(
        resumed.status()?.search_ready,
        "and the catalog stops reporting a staleness no refresh would clear"
    );
    Ok(())
}

#[test]
fn a_cursor_left_behind_a_mid_batch_budget_commit_replays_the_whole_batch() -> Result<()> {
    let (_directory, catalog) = paging_fixture(0)?;
    let ids = super::wide_card_fixture(&catalog)?;
    catalog.rebuild_search(|_| {})?;
    let cursor = catalog.manifest()?.search_seq;

    let now = jiff::Timestamp::now();
    for id in &ids {
        catalog.bump_artifact_use(*id, now)?;
    }
    let logged = changes(&catalog)?;
    assert_eq!(logged.len(), 40, "each bump logged its artifact");

    let report = catalog.refresh_with_budget(1024 * 1024, |_| {})?;
    assert_eq!(report.applied, 40);
    assert!(
        report.commits >= 2,
        "a 1 MiB budget against 40 artifacts of 16 KiB cards commits several times mid \
             batch: {}",
        report.commits
    );
    assert_eq!(catalog.pending_changes()?, 0);
    let expected = fingerprint(&catalog)?;

    rewind_cursor(&catalog, cursor, &logged)?;
    assert_eq!(
        catalog.pending_changes()?,
        40,
        "the rewound cursor and the restored rows are the state a kill leaves between a \
             budget commit inside the batch and the cursor advancing past it"
    );
    assert_eq!(
        catalog.refresh_with_budget(1024 * 1024, |_| {})?.applied,
        40,
        "the whole batch replays, not just what a mid-batch commit left uncommitted"
    );
    assert_eq!(catalog.pending_changes()?, 0);
    assert_eq!(
        fingerprint(&catalog)?,
        expected,
        "the committed prefix is deleted by term and re-added exactly as the rest of the \
             batch is, so replaying it changes nothing"
    );
    Ok(())
}

/// Every `search-v1-*` directory under the catalog, in name order.
fn index_directories(directory: &Path) -> Result<Vec<String>> {
    let mut found: Vec<String> = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("search-v1-"))
        .collect();
    found.sort();
    Ok(found)
}

#[test]
fn a_rebuild_killed_before_publish_leaves_no_orphan_directory_after_reopen() -> Result<()> {
    for at in [1, 2] {
        let directory = tempfile::tempdir()?;
        let config = hub_fixture(directory.path())?;
        let hub = directory.path().join("hub");
        let native = config.native_path();
        let catalog = scanned(&config)?;
        let published = catalog.manifest()?.search_directory;
        file_into_every_revision(&hub, "extra.safetensors");
        catalog.rescan(
            &config,
            config.hash_policy,
            RescanScope::Configured,
            |_, _| {},
        )?;
        let served = search(&catalog, "")?.total;

        let snapshot = directory.path().join("copy").join("native");
        let text = interrupt(&native, &snapshot, at, |progress| {
            catalog.rebuild_search(|_| progress())
        });
        assert!(
            text.contains(KILLED),
            "the harness killed the build at {at}"
        );
        assert_eq!(
            catalog.manifest()?.search_directory,
            published,
            "an unpublished build leaves the manifest on the index it had"
        );
        assert_eq!(
            search(&catalog, "")?.total,
            served,
            "and that index still answers"
        );
        assert_eq!(
            index_directories(&native)?.len(),
            2,
            "the unpublished build is still on disk"
        );
        drop(catalog);

        let reopened = Catalog::open(&native)?;
        assert_eq!(
            index_directories(&native)?,
            vec![published.clone().unwrap_or_default()],
            "opening the catalog removes what the killed build left"
        );
        assert_eq!(
            search(&reopened, "")?.total,
            served,
            "the published index answers after the removal"
        );
        drop(reopened);

        let reopened = Catalog::open_or_scan(&config, "n", |_, _| {})?;
        assert_eq!(
            fingerprint(&reopened)?.matched,
            36,
            "and the refresh the open runs catches the index up"
        );
        is_consistent(&reopened)?;
    }
    Ok(())
}

#[test]
fn a_catalog_copied_mid_rebuild_opens_and_rebuilds() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = hub_fixture(directory.path())?;
    let hub = directory.path().join("hub");
    let native = config.native_path();
    let clean_config = IndexConfig {
        data_dir: directory.path().join("clean"),
        ..config.clone()
    };
    let clean = scanned(&clean_config)?;
    let catalog = scanned(&config)?;
    file_into_every_revision(&hub, "extra.safetensors");
    rescanned(&clean, &clean_config)?;
    let expected = fingerprint(&clean)?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    let copy_config = IndexConfig {
        data_dir: directory.path().join("copy"),
        ..config.clone()
    };
    let snapshot = copy_config.native_path();
    let text = interrupt(&native, &snapshot, 2, |progress| {
        catalog.rebuild_search(|_| progress())
    });
    assert!(text.contains(KILLED), "the harness killed the build");
    drop(catalog);

    let copied = Catalog::open_or_scan(&copy_config, "n", |_, _| {})?;
    assert_eq!(
        index_directories(&snapshot)?.len(),
        1,
        "the copy serves one index and keeps no half-built one"
    );
    is_consistent(&copied)?;
    assert_eq!(
        fingerprint(&copied)?,
        expected,
        "a catalog copied mid-build answers what a clean run answers"
    );
    Ok(())
}

#[test]
fn the_harness_interrupts_at_the_named_call_and_leaves_a_reopenable_catalog() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = hub_fixture(directory.path())?;
    let hub = directory.path().join("hub");
    let native = config.native_path();
    let catalog = scanned(&config)?;
    let before = fingerprint(&catalog)?;
    assert_eq!(
        before.entries.len(),
        27,
        "three files in each of nine revisions"
    );

    file_into_every_revision(&hub, "extra.safetensors");
    let snapshot = directory.path().join("copy");
    let text = interrupt(&native, &snapshot, 3, |progress| {
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| progress(),
        )
    });
    assert!(text.contains(KILLED), "the harness panicked, not the stage");
    assert_eq!(
        catalog.status()?.generation,
        1,
        "the killed rescan published no generation of its own"
    );
    let killed = fingerprint(&catalog)?;
    assert!(
        killed.entries.len() > before.entries.len(),
        "the commits the walk made before the kill are on record"
    );
    assert_eq!(
        killed.totals, killed.counted,
        "the totals are committed with the records the kill left"
    );
    drop(catalog);

    let reopened = Catalog::open(&native)?;
    let settled = fingerprint(&reopened)?;
    assert_eq!(
        settled.entries, killed.entries,
        "a reopened catalog holds what the killed stage committed"
    );
    assert_eq!(
        settled.totals, settled.counted,
        "and the totals the open counted again"
    );
    owed_hashes_match_the_entries(&reopened)?;
    drop(reopened);

    let copied = Catalog::open(&snapshot)?;
    assert_eq!(
        fingerprint(&copied)?.entries.len(),
        killed.entries.len(),
        "the copy holds the same commits as the catalog it was taken from"
    );
    Ok(())
}
