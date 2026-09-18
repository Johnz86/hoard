use super::reads::{lone_shard_catalog, lone_shard_tree};
use super::*;

fn three_counts(catalog: &Catalog) -> Result<(u64, u64, u64)> {
    Ok((
        catalog.triage_queue(100)?.len() as u64,
        catalog.totals()?.triage_artifacts,
        search(catalog, "state:triage")?.total,
    ))
}

fn lone_shard_world(directory: &Path) -> Result<(IndexConfig, Catalog)> {
    let hub = lone_shard_tree(directory)?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok((config, catalog))
}

fn stored_record(catalog: &Catalog, id: ArtifactId) -> Result<ArtifactRecord> {
    catalog
        .database
        .r_transaction()?
        .get()
        .primary::<ArtifactRecord>(id.get())?
        .ok_or_else(|| IndexError::Catalog("the artifact record is gone".into()))
}

fn filed_by_node(catalog: &Catalog) -> Result<BTreeMap<i64, u64>> {
    let snapshot = catalog.database.r_transaction()?;
    let mut filed = BTreeMap::new();
    for node in snapshot.scan().primary::<Node>()?.all()? {
        let node: Node = node?;
        filed.insert(node.id, node.filed);
    }
    Ok(filed)
}

fn mystery_catalog(directory: &Path) -> Result<Catalog> {
    let config = IndexConfig {
        data_dir: directory.join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    Catalog::open_or_scan(&config, "workstation", |_, _| {})
}

fn queued_seed(identity: u128, name: &str, rel_path: &str) -> SeedEntry {
    SeedEntry {
        state: State::Triage,
        ..seeded_entry(
            rel_path,
            2_048,
            Some(seeded_artifact(
                identity,
                name,
                None,
                Artifact {
                    role: Role::Unk,
                    role_confidence: 0.2,
                    ..seeded_facts()
                },
            )),
        )
    }
}

fn seeded_id(written: &[(u128, ArtifactId)], identity: u128) -> Result<ArtifactId> {
    written
        .iter()
        .find(|(seeded, _)| *seeded == identity)
        .map(|(_, id)| *id)
        .ok_or_else(|| IndexError::Catalog("the seeded artifact is missing".into()))
}

fn only_artifact(catalog: &Catalog) -> Result<ArtifactId> {
    let queue = catalog.triage_queue(10)?;
    queue
        .first()
        .map(|item| item.row.artifact_id)
        .ok_or_else(|| IndexError::Catalog("the queue is empty".into()))
}

#[test]
fn a_lone_shards_row_says_it_is_incomplete() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = lone_shard_catalog(directory.path())?;

    let page = search(&catalog, "state:triage")?;

    assert_eq!(page.rows.len(), 1, "the lone shard is the only queued row");
    let row = &page.rows[0];
    assert!(row.incomplete, "the row says its set is short");
    assert_eq!(row.shard_count, 1, "and counts the one file present");
    assert!(row.is_set(), "one shard of a declared set is still a set");
    let read = catalog
        .artifact_row(row.artifact_id)?
        .ok_or_else(|| IndexError::Catalog("the artifact row is missing".into()))?;
    assert!(
        read.incomplete,
        "the reader assembles the same fact as the search page"
    );
    Ok(())
}

#[test]
fn a_whole_sets_row_is_a_set_and_not_incomplete() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Whole",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
            ("model-00003-of-00003.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;

    let page = search(&catalog, "")?;

    assert_eq!(page.rows.len(), 1, "three shards collapse to one row");
    let row = &page.rows[0];
    assert!(!row.incomplete, "every file it declares is present");
    assert_eq!(row.shard_count, 3);
    assert!(row.is_set(), "a whole set is still pulled as a set");

    let (_loose, single) = fixture()?;
    single.rebuild_search(|_| {})?;
    let loose = search(&single, "format:csv")?;
    assert_eq!(loose.rows.len(), 1);
    assert!(
        !loose.rows[0].is_set(),
        "a single loose file is pulled by its path"
    );
    Ok(())
}

#[test]
fn a_rejection_takes_an_incomplete_set_out_of_the_queue_the_shelf_and_the_total() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = lone_shard_catalog(directory.path())?;
    let id = only_artifact(&catalog)?;
    assert_eq!(three_counts(&catalog)?, (1, 1, 1));

    catalog.clear_triage(id)?;

    assert_eq!(
        three_counts(&catalog)?,
        (0, 0, 0),
        "the queue, the stats total and the shelf drop in one transaction"
    );
    let row = catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::Catalog("the rejected row is gone".into()))?;
    assert!(row.incomplete, "the set is still short of its files");
    assert_eq!(row.state, State::Local, "its path is settled, not queued");
    assert!(!row.triage);
    Ok(())
}

#[test]
fn a_rejection_takes_an_ordinary_artifact_out_of_all_three() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = mystery_catalog(directory.path())?;
    catalog.seed(&[seeded_revision(
        seeded_repo("misc/mystery", RepoType::Models),
        0x77,
        vec![queued_seed(0x77, "mystery", "mystery.safetensors")],
    )])?;
    catalog.rebuild_search(|_| {})?;
    let id = only_artifact(&catalog)?;
    assert_eq!(three_counts(&catalog)?, (1, 1, 1));

    catalog.clear_triage(id)?;

    assert_eq!(three_counts(&catalog)?, (0, 0, 0));
    Ok(())
}

#[test]
fn a_confirm_takes_an_ordinary_artifact_out_of_all_three() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = mystery_catalog(directory.path())?;
    catalog.seed(&[seeded_revision(
        seeded_repo("misc/mystery", RepoType::Models),
        0x77,
        vec![queued_seed(0x77, "mystery", "mystery.safetensors")],
    )])?;
    catalog.rebuild_search(|_| {})?;
    let id = only_artifact(&catalog)?;
    assert_eq!(three_counts(&catalog)?, (1, 1, 1));

    catalog.put_override(id, Some(Role::Lora), None, None)?;

    assert_eq!(
        three_counts(&catalog)?,
        (0, 0, 0),
        "a confirm settles the queue, the stats total and the shelf together"
    );
    let row = catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::Catalog("the confirmed row is gone".into()))?;
    assert_eq!(row.state, State::Local, "its path is settled, not queued");
    assert!(!row.triage);
    assert_eq!(row.role, Role::Lora);
    let record = stored_record(&catalog, id)?;
    assert!(record.triage_cleared);
    assert!(!record.rejected, "a confirm is not a rejection");
    assert!(catalog.artifact_override(id)?.is_some());
    Ok(())
}

#[test]
fn a_verdict_that_leaves_the_role_unsettled_keeps_its_path_in_triage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = mystery_catalog(directory.path())?;
    catalog.seed(&[seeded_revision(
        seeded_repo("misc/mystery", RepoType::Models),
        0x77,
        vec![queued_seed(0x77, "mystery", "mystery.safetensors")],
    )])?;
    catalog.rebuild_search(|_| {})?;
    let id = only_artifact(&catalog)?;
    assert_eq!(three_counts(&catalog)?, (1, 1, 1));

    catalog.put_override(id, None, None, Some(Base::new("sdxl")))?;

    assert_eq!(
        three_counts(&catalog)?,
        (0, 1, 1),
        "the queue drops a cleared record; the summary and the shelf still count it"
    );
    let row = catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::Catalog("the base-only row is gone".into()))?;
    assert_eq!(
        row.state,
        State::Triage,
        "the next scan of the file writes the same state, so the verdict leaves it"
    );
    assert!(row.triage);
    assert_eq!(row.base, Base::new("sdxl"));
    let record = stored_record(&catalog, id)?;
    assert!(record.triage_cleared);
    assert!(!record.rejected);
    assert_eq!(catalog.totals()?, catalog.count_summary()?);
    Ok(())
}

#[test]
fn a_confirm_on_an_incomplete_set_moves_no_path() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = lone_shard_catalog(directory.path())?;
    let id = only_artifact(&catalog)?;
    assert_eq!(three_counts(&catalog)?, (1, 1, 1));

    catalog.put_override(id, Some(Role::Llm), None, None)?;

    assert_eq!(
        three_counts(&catalog)?,
        (1, 1, 1),
        "a verdict cannot make a set complete, so it stays queued"
    );
    let row = catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::Catalog("the confirmed row is gone".into()))?;
    assert_eq!(row.state, State::Triage, "its path waits with the set");
    assert!(row.triage);
    assert!(row.incomplete);
    let record = stored_record(&catalog, id)?;
    assert!(record.triage_cleared);
    assert!(!record.rejected);
    Ok(())
}

#[test]
fn a_confirm_then_a_rejection_and_the_reverse_end_at_the_same_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = mystery_catalog(directory.path())?;
    let written = catalog.seed(&[seeded_revision(
        seeded_repo("misc/mystery", RepoType::Models),
        0x77,
        vec![
            queued_seed(0x77, "mystery-a", "mystery-a.safetensors"),
            queued_seed(0x78, "mystery-b", "mystery-b.safetensors"),
        ],
    )])?;
    catalog.rebuild_search(|_| {})?;
    let first = seeded_id(&written, 0x77)?;
    let second = seeded_id(&written, 0x78)?;
    assert_eq!(three_counts(&catalog)?, (2, 2, 2));

    catalog.put_override(first, Some(Role::Lora), None, None)?;
    assert_eq!(
        three_counts(&catalog)?,
        (1, 1, 1),
        "the confirmed artifact leaves all three and the other waits"
    );
    catalog.clear_triage(first)?;
    assert_eq!(
        three_counts(&catalog)?,
        (1, 1, 1),
        "rejecting what was already settled moves nothing"
    );
    catalog.clear_triage(second)?;
    assert_eq!(three_counts(&catalog)?, (0, 0, 0));
    catalog.put_override(second, Some(Role::Lora), None, None)?;
    assert_eq!(
        three_counts(&catalog)?,
        (0, 0, 0),
        "confirming what was already rejected moves nothing"
    );

    for id in [first, second] {
        let row = catalog
            .artifact_row(id)?
            .ok_or_else(|| IndexError::Catalog("the settled row is gone".into()))?;
        assert_eq!(row.state, State::Local);
        assert!(!row.triage);
        assert_eq!(row.role, Role::Lora);
        let record = stored_record(&catalog, id)?;
        assert!(record.rejected);
        assert!(record.triage_cleared);
    }
    assert_eq!(catalog.totals()?, catalog.count_summary()?);
    Ok(())
}

#[test]
fn a_verdict_settles_no_path_another_node_holds() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = mystery_catalog(directory.path())?;
    let attic = catalog.upsert_peer(&NodeId::new("attic-node")?, "attic", None)?;
    let held = queued_seed(0x79, "mystery", "mystery.safetensors");
    let artifact = held.artifact.clone();
    catalog.seed(&[
        seeded_revision(
            seeded_repo("misc/mystery", RepoType::Models),
            0x79,
            vec![held],
        ),
        seeded_revision(
            seeded_repo("attic/mystery", RepoType::Models),
            0x79,
            vec![SeedEntry {
                node: attic.id,
                state: State::Remote,
                abs_path: None,
                ..seeded_entry("mystery.safetensors", 2_048, artifact)
            }],
        ),
    ])?;
    catalog.rebuild_search(|_| {})?;
    let id = only_artifact(&catalog)?;
    let before = filed_by_node(&catalog)?;

    catalog.put_override(id, Some(Role::Lora), None, None)?;

    assert_eq!(three_counts(&catalog)?, (0, 0, 0));
    let entries = catalog.entries_of(id)?;
    assert_eq!(entries.len(), 2, "both paths still point at the artifact");
    let here = entries
        .iter()
        .find(|entry| entry.node == "workstation")
        .ok_or_else(|| IndexError::Catalog("this node holds no path".into()))?;
    assert_eq!(here.state, State::Local, "the path we hold is settled");
    let there = entries
        .iter()
        .find(|entry| entry.node == "attic")
        .ok_or_else(|| IndexError::Catalog("the peer holds no path".into()))?;
    assert_eq!(
        there.state,
        State::Remote,
        "a peer's row keeps the state the peer advertised"
    );
    assert_eq!(
        filed_by_node(&catalog)?,
        before,
        "settling a path files it on the same node"
    );
    assert_eq!(catalog.totals()?, catalog.count_summary()?);
    Ok(())
}

#[test]
fn a_rejected_set_whose_noise_did_not_take_stays_out_after_a_rescan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, catalog) = lone_shard_world(directory.path())?;
    let id = only_artifact(&catalog)?;
    catalog.clear_triage(id)?;
    let entries = catalog.entries_of(id)?;
    let shard = entries
        .first()
        .and_then(|entry| entry.abs_path.clone())
        .ok_or_else(|| IndexError::Catalog("the shard has no path on disk".into()))?;
    fs::File::options()
        .write(true)
        .open(&shard)?
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))?;

    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;

    assert_eq!(
        three_counts(&catalog)?,
        (0, 0, 0),
        "a re-stored record carries the rejection it was given"
    );
    assert!(stored_record(&catalog, id)?.rejected);
    Ok(())
}

#[test]
fn a_rejected_set_is_walked_past_by_the_next_scan() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, catalog) = lone_shard_world(directory.path())?;
    let id = only_artifact(&catalog)?;
    let walked = catalog
        .entries_of(id)?
        .first()
        .and_then(|entry| entry.walked_path.clone())
        .ok_or_else(|| IndexError::Catalog("the cached entry has no walked path".into()))?;
    assert!(
        walked.contains("/snapshots/"),
        "the walker visits the snapshot link, not the blob: {walked}"
    );
    catalog.add_noise(&walked, Some("rejected from triage"))?;
    catalog.clear_triage(id)?;

    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;

    assert!(
        catalog.artifact_row(id)?.is_none(),
        "the noise path is the one the walker tests, so the row is gone"
    );
    assert_eq!(three_counts(&catalog)?, (0, 0, 0));
    Ok(())
}
