use super::*;

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
    let entries = catalog.revision_entries(detail.id, 0, Dedupe::Resolve)?;
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
    assert_eq!(
        catalog
            .revision_entries(detail.id, 0, Dedupe::Ignore)?
            .len(),
        2
    );
    Ok(())
}

#[test]
fn a_file_landing_on_an_incomplete_set_is_filed_for_triage() -> Result<()> {
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
    let sha = Sha256::from_bytes([0x51; 32]);
    let file = ServedFile {
        repo: RepoId::parse_ref(RepoType::Models, "qwen/partial")?,
        commit: CommitSha::from_bytes([0x52; 20]),
        rel_path: "model-00001-of-00003.safetensors".to_owned(),
        size: 4096,
        sha256: sha,
        git_sha1: None,
        cache_root: directory.path().join("repos"),
    };
    let landed = catalog.ingest_served(&file)?;
    let id = landed
        .artifact
        .ok_or_else(|| IndexError::Catalog("the file is an artifact".into()))?;

    let transaction = catalog.database.rw_transaction()?;
    let stored: ArtifactRecord = transaction
        .get()
        .primary(id.get())?
        .ok_or_else(|| IndexError::Catalog("the record exists".into()))?;
    transaction.upsert(ArtifactRecord {
        incomplete: true,
        artifact: Artifact {
            role: Role::Ckpt,
            role_confidence: 0.99,
            ..stored.artifact.clone()
        },
        ..stored
    })?;
    transaction.commit()?;

    catalog.ingest_served(&file)?;
    catalog.refresh_search(|_| {})?;
    let row = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("the row reads back".into()))?;
    assert!(
        row.triage,
        "a file filed onto a set still missing pieces needs a person, whatever its role \
         inference says"
    );
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
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    let detail = catalog
        .repository(&repo)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(
        catalog.revision_entries(detail.id, 0, Dedupe::Ignore)?[0].state,
        State::Served
    );
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
    let entries = catalog.revision_entries(slot.repo, slot.revision, Dedupe::Ignore)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Pulling);
    assert_eq!(entries[0].pull_progress, Some(0.5));
    assert_eq!(catalog.reset_stale_pulls()?, 1);
    assert_eq!(
        catalog.revision_entries(slot.repo, slot.revision, Dedupe::Ignore)?[0].state,
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
    let entries = catalog.revision_entries(slot.repo, slot.revision, Dedupe::Ignore)?;
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

/// Files one pulled file into a slot, sniffed and inferred the way the pull queue does, and
/// answers the artifact it was filed under.
fn pull_into(
    catalog: &Catalog,
    slot: PullSlot,
    root: &Path,
    rel_path: &str,
    bytes: &[u8],
    sha: u8,
) -> Result<ArtifactId> {
    let destination = root.join(rel_path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&destination, bytes)?;
    let sniff = crate::sniff::sniff_path(&destination)?;
    let name = crate::shard::file_name(rel_path);
    let mut input = crate::infer::InferInput::new(sniff.format, &destination, name, sniff.size);
    if let Some(tensors) = &sniff.tensors {
        input = input.with_tensors(tensors);
    }
    let inference = crate::infer::infer(&input);
    catalog
        .file_pulled(&PulledFile {
            slot,
            rel_path,
            abs_path: &destination,
            sha256: Sha256::from_bytes([sha; 32]),
            git_sha1: None,
            sniff: &sniff,
            inference: &inference,
            served: true,
            triage: false,
        })?
        .artifact
        .ok_or_else(|| IndexError::Catalog(format!("{rel_path} was filed under no artifact")))
}

#[test]
fn a_pulled_readme_becomes_the_revisions_card_and_reindexes_what_it_holds() -> Result<()> {
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
    let weight = pull_into(
        &catalog,
        slot,
        &root,
        "loras/film-grain.safetensors",
        &lora_header(),
        0x42,
    )?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        stored_card(&catalog, slot.repo.get(), slot.revision)?,
        None,
        "a revision holds no card until one is pulled"
    );

    pull_into(
        &catalog,
        slot,
        &root,
        "README.md",
        &card_body("chinchilla"),
        1,
    )?;
    assert_eq!(
        stored_card(&catalog, slot.repo.get(), slot.revision)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "the pulled README is the revision's card"
    );
    catalog.refresh_search(|_| {})?;
    assert!(
        search(&catalog, "chinchilla")?
            .rows
            .iter()
            .any(|row| row.artifact_id == weight),
        "the weight pulled before the card is indexed again with it"
    );

    pull_into(&catalog, slot, &root, "NOTES.md", &card_body("vicuna"), 2)?;
    assert_eq!(
        stored_card(&catalog, slot.repo.get(), slot.revision)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "markdown that is not a README is not a card"
    );

    pull_into(
        &catalog,
        slot,
        &root,
        "loras/README.md",
        &card_body("capybara"),
        3,
    )?;
    assert_eq!(
        stored_card(&catalog, slot.repo.get(), slot.revision)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "and a README below the shallowest one is not either"
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
    assert_eq!(
        catalog.filed_for(laptop.id)?,
        2,
        "the peer's count follows what its digest filed"
    );
    assert_eq!(
        catalog.filed_for(LOCAL_NODE)?,
        3,
        "this node files what its own scan found"
    );
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
    assert_eq!(
        catalog.filed_for(laptop.id)?,
        1,
        "a retracted row leaves the peer's count"
    );
    assert_eq!(catalog.filed_for(LOCAL_NODE)?, 3);
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
