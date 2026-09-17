use super::*;

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
    fs::write(input.join("b/two.csv"), "x,y\n1,2\n")?;
    fs::write(input.join("a/solo.csv"), "x,y\n3,4\n")?;
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
    let pressure = catalog.disk_pressure()?;
    assert_eq!(pressure.duplicate_blobs, 1);
    assert_eq!(
        pressure.reclaimable_bytes,
        fs::metadata(input.join("a/one.csv"))?.len()
    );
    fs::remove_file(input.join("b/two.csv"))?;
    catalog.rescan(&config, HashPolicy::Now, RescanScope::Configured, |_, _| {})?;
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
    let files = catalog.revision_entries(repo.id, main.ordinal, Dedupe::Ignore)?;
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
fn the_triage_queue_pages_in_artifact_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let queued = |identity: u128| SeedArtifact {
        tensors: Some(queued_tensors()),
        ..seeded_artifact(
            identity,
            &format!("queued-{identity:x}"),
            None,
            Artifact {
                role: Role::Unk,
                role_confidence: 0.2,
                ..seeded_facts()
            },
        )
    };
    let settled = |identity: u128| {
        seeded_artifact(
            identity,
            &format!("settled-{identity:x}"),
            None,
            seeded_facts(),
        )
    };
    let mut repositories = Vec::new();
    for (index, identity) in [0xa1_u128, 0xa2, 0xb1, 0xa3, 0xb2, 0xa4].iter().enumerate() {
        let triaged = *identity < 0xb0;
        let artifact = match triaged {
            true => queued(*identity),
            false => settled(*identity),
        };
        let entry = SeedEntry {
            state: match triaged {
                true => State::Triage,
                false => State::Local,
            },
            ..seeded_entry(&format!("f{index}.safetensors"), 1_024, Some(artifact))
        };
        repositories.push(seeded_revision(
            seeded_repo(&format!("queue/r{index}"), RepoType::Models),
            index as u8,
            vec![entry],
        ));
    }
    let written = catalog.seed(&repositories)?;
    catalog.rebuild_search(|_| {})?;
    let id = |identity: u128| {
        written
            .iter()
            .find(|(seeded, _)| *seeded == identity)
            .map(|(_, id)| id.get())
            .ok_or_else(|| IndexError::Catalog("every seeded identity was written".into()))
    };

    let page = catalog.triage_queue(2)?;
    assert_eq!(page.len(), 2, "a limit bounds the page");
    let queued_ids = [id(0xa1)?, id(0xa2)?, id(0xa3)?, id(0xa4)?];
    assert_eq!(
        page.iter()
            .map(|item| item.row.artifact_id.get())
            .collect::<Vec<_>>(),
        queued_ids[..2].to_vec(),
        "the queue ascends by artifact id"
    );
    let whole = catalog.triage_queue(10)?;
    assert_eq!(
        whole
            .iter()
            .map(|item| item.row.artifact_id.get())
            .collect::<Vec<_>>(),
        queued_ids.to_vec(),
        "only the artifacts needing a verdict appear, in artifact id order"
    );
    assert!(
        whole.iter().all(|item| item.header.tensor_count == 2),
        "each row carries the header its detail record holds"
    );

    catalog.clear_triage(ArtifactId::new(queued_ids[0]))?;
    assert_eq!(
        catalog
            .triage_queue(10)?
            .iter()
            .map(|item| item.row.artifact_id.get())
            .collect::<Vec<_>>(),
        queued_ids[1..].to_vec(),
        "a cleared record leaves the queue even though its entry keeps its state"
    );
    Ok(())
}

#[test]
fn the_triage_queue_answers_from_the_catalog_when_no_index_exists() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    for index in 0..3 {
        fs::write(
            input.join(format!("unknown{index}.safetensors")),
            plain_header_named(&format!("block.{index}.mlp")),
        )?;
    }
    for index in 0..2 {
        fs::write(
            input.join(format!("lora{index}.safetensors")),
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
    assert!(
        !catalog.status()?.search_built,
        "a scan alone publishes no search directory"
    );

    let queued = |items: &[TriageItem]| -> Vec<i64> {
        items
            .iter()
            .map(|item| item.row.artifact_id.get())
            .collect()
    };
    let unindexed = catalog.triage_queue(10)?;
    assert_eq!(unindexed.len(), 3, "every unclassified file queues");
    assert!(
        unindexed.iter().all(|item| item.guess_role == Role::Unk),
        "the queue carries the role the record holds"
    );
    assert_eq!(
        queued(&catalog.triage_queue(2)?),
        queued(&unindexed)[..2].to_vec(),
        "a limit bounds the record scan"
    );

    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        queued(&catalog.triage_queue(10)?),
        queued(&unindexed),
        "the index answers the same artifacts in the same order"
    );
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

/// A catalog of 46 distinct bases: two families whose roots the rail has to add, and forty
/// siblings no root joins, which together are more bases than the rail can hold.
fn base_rail_fixture() -> Result<(TempDir, Catalog, usize)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    let mut populations: Vec<(String, usize)> = vec![
        ("fam".to_owned(), 3),
        ("fam 2.0".to_owned(), 3),
        ("sd 1.5".to_owned(), 2),
        ("sd 2.1".to_owned(), 2),
        ("sdxl".to_owned(), 2),
        ("sd".to_owned(), 1),
    ];
    populations.extend((0..27).map(|index| (format!("base-{index:02}"), 2)));
    populations.extend((27..40).map(|index| (format!("base-{index:02}"), 1)));
    let mut entries = Vec::new();
    let mut identity = 0x4000_u128;
    for (base, held) in &populations {
        for copy in 0..*held {
            identity += 1;
            let name = format!("{}-{copy}", Base::new(base).as_str());
            entries.push(seeded_entry(
                &format!("rail/{name}.safetensors"),
                1_024,
                Some(seeded_artifact(
                    identity,
                    &name,
                    Some("acme"),
                    Artifact {
                        base: Base::new(base),
                        base_label: Some(base.clone()),
                        ..seeded_facts()
                    },
                )),
            ));
        }
    }
    let seeded = entries.len();
    catalog.seed(&[seeded_revision(
        seeded_repo("acme/rail", RepoType::Models),
        0x44,
        entries,
    )])?;
    catalog.rebuild_search(|_| {})?;
    Ok((directory, catalog, seeded))
}

#[test]
fn the_base_rail_holds_at_most_the_capped_buckets_and_counts_them_by_prefix() -> Result<()> {
    let (_directory, catalog, seeded) = base_rail_fixture()?;
    let page = search(&catalog, "")?;
    assert_eq!(
        page.total, seeded as u64,
        "every seeded artifact is indexed"
    );
    assert_eq!(
        page.facets.base.len(),
        32,
        "the rail is capped: {:?}",
        page.facets.base
    );
    let mut shown: Vec<&str> = page
        .facets
        .base
        .iter()
        .map(|bucket| bucket.value.as_str())
        .collect();
    shown.sort_unstable();
    let mut expected: Vec<String> = ["fam", "fam20", "sd", "sd15", "sd21", "sdxl"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    expected.extend((0..26).map(|index| Base::new(format!("base-{index:02}")).as_str().to_owned()));
    expected.sort_unstable();
    assert_eq!(
        shown, expected,
        "the rail holds the bases the prefix rule makes largest"
    );

    let root = page
        .facets
        .base
        .first()
        .ok_or_else(|| IndexError::Catalog("the rail is empty".into()))?;
    assert_eq!(
        (root.value.as_str(), root.count),
        ("sd", 7),
        "a root one artifact carries exactly, and six more extend, leads the rail"
    );
    assert_eq!(
        search(&catalog, "base:sd")?.total,
        7,
        "the bucket counts the query it opens"
    );

    let family = page
        .facets
        .base
        .iter()
        .find(|bucket| bucket.value == "fam")
        .ok_or_else(|| IndexError::Catalog("the rail lost the family bucket".into()))?;
    assert_eq!(
        family.count, 6,
        "a family bucket counts by the prefix rule, not by its own term"
    );
    assert_eq!(
        search(&catalog, "base:fam")?.total,
        6,
        "the bucket counts the query it opens"
    );
    assert_eq!(
        search(&catalog, "base:base35")?.total,
        1,
        "a base the rail does not show still answers its own query"
    );
    Ok(())
}

/// One query counted both ways: the full path, which tallies the rail and cuts a page, and
/// the total on its own.
fn totals_agree(catalog: &Catalog, raw: &str) -> Result<u64> {
    let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
    let page = catalog.query(QueryRequest {
        raw,
        sort: SortSpec::default(),
        limit: 50,
        offset: 0,
        context: &context,
    })?;
    let counted = catalog.total(QueryRequest {
        raw,
        sort: SortSpec::default(),
        limit: 0,
        offset: 0,
        context: &context,
    })?;
    assert_eq!(
        counted, page.total,
        "{raw:?} counts the same with the facet rail and without it"
    );
    Ok(counted)
}

#[test]
fn a_total_counts_what_the_page_path_counts_without_answering_a_page() -> Result<()> {
    let (_directory, catalog, seeded) = base_rail_fixture()?;
    assert_eq!(
        totals_agree(&catalog, "")?,
        seeded as u64,
        "the empty query counts every artifact, over more bases than the rail can show"
    );
    for raw in ["base:sd", "base:base35", "role:lora", "role:ds", "size:>1"] {
        totals_agree(&catalog, raw)?;
    }
    let (_mirrored, collapsing) = paging_fixture(40)?;
    assert_eq!(
        totals_agree(&collapsing, "")?,
        40,
        "the collapsed path counts artifacts, not the entries mirrored onto a second node"
    );
    for raw in ["node:self", "node:attic", "state:local"] {
        totals_agree(&collapsing, raw)?;
    }
    Ok(())
}

#[test]
fn the_base_rail_reports_how_many_bases_it_could_not_show() -> Result<()> {
    let (_directory, catalog, _seeded) = base_rail_fixture()?;
    let page = search(&catalog, "")?;
    assert_eq!(page.facets.base.len(), 32, "the rail is capped");
    assert_eq!(
        page.facets.base_total, 46,
        "every distinct base is counted before the cap"
    );
    assert!(
        page.facets.base_total > page.facets.base.len() as u64,
        "the rail knows it is hiding the rest"
    );
    assert_eq!(
        search(&catalog, "base:sd")?.facets.base_total,
        46,
        "the total is counted over the set the base key does not narrow, like the buckets"
    );
    assert_eq!(
        search(&catalog, "role:ds")?.facets.base_total,
        0,
        "a result set holding no base hides none"
    );
    Ok(())
}

/// The layout facet over a real ComfyUI-shaped tree, discovered and scanned rather than
/// seeded, because a seeded repository row can carry a shape discovery never writes: a
/// loose source gives every synthetic repository the source's own root and names the
/// category folder in the repository's identity instead.
#[test]
fn the_layout_facet_counts_the_folder_discovery_filed_an_entry_under() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let models = directory.path().join("models");
    for (path, header) in [
        ("checkpoints/anima.safetensors", plain_header_named("anima")),
        ("loras/film-grain.safetensors", lora_header_named("grain")),
        ("loras/halation.safetensors", lora_header_named("halation")),
        ("vae/sdxl-vae.safetensors", plain_header_named("decoder")),
        ("unet/flux-dev.safetensors", plain_header_named("flux")),
    ] {
        let file = models.join(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(file, header)?;
    }
    let config = IndexConfig {
        sources: vec![SourceConfig {
            repo_per_folder: true,
            ..source(&models, "comfy")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "workstation",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;

    let snapshot = catalog.database.r_transaction()?;
    let repositories: Vec<Repository> = snapshot
        .scan()
        .primary::<Repository>()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(snapshot);
    let mut filed: Vec<(&str, &str)> = repositories
        .iter()
        .map(|repository| (repository.repo.name(), repository.root_path.as_str()))
        .collect();
    filed.sort_unstable();
    let root = models.to_str().unwrap_or_default();
    assert_eq!(
        filed,
        vec![
            ("checkpoints", root),
            ("loras", root),
            ("unet", root),
            ("vae", root),
        ],
        "discovery names the category folder in the repository and leaves every root at the \
         source's own, which is why a layout read from the root path reads nothing"
    );

    let page = search(&catalog, "")?;
    assert_eq!(page.total, 5, "every file is indexed");
    let mut counted: Vec<(&str, u64)> = page
        .facets
        .layout
        .iter()
        .map(|bucket| (bucket.value.as_str(), bucket.count))
        .collect();
    counted.sort_unstable();
    assert_eq!(
        counted,
        vec![
            ("checkpoints", 1),
            ("diffusion_models", 1),
            ("loras", 2),
            ("vae", 1),
        ],
        "each entry is counted under the folder it was filed in, with unet folded into \
         diffusion_models"
    );
    let mut placed: Vec<(&str, &str)> = page
        .rows
        .iter()
        .map(|row| (row.path.as_str(), row.layout.as_str()))
        .collect();
    placed.sort_unstable();
    assert_eq!(
        placed,
        vec![
            ("anima.safetensors", "checkpoints"),
            ("film-grain.safetensors", "loras"),
            ("flux-dev.safetensors", "diffusion_models"),
            ("halation.safetensors", "loras"),
            ("sdxl-vae.safetensors", "vae"),
        ],
        "every row reads the folder its own file sits in"
    );
    assert_eq!(
        search(&catalog, "layout:loras")?.total,
        2,
        "the bucket counts the query it opens"
    );
    assert_eq!(
        search(&catalog, "layout:LORAS")?.total,
        2,
        "a hand-typed layout reaches the same term the rail's own click does"
    );
    assert_eq!(
        search(&catalog, "layout:unet")?.total,
        1,
        "a folder spelling reaches the layout it was folded into"
    );
    Ok(())
}

/// [`source_kind_of`] answers with no kind for a source name the catalog does not hold.
///
/// Nothing reaches this through the catalog's own writers: each creates a source row before
/// the repositories that name it, and `retire_source` removes it after them. This covers the
/// defensive branch itself, not a state a run can be in.
#[test]
fn a_source_name_the_catalog_does_not_hold_resolves_to_no_kind() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let snapshot = catalog.database.r_transaction()?;
    assert!(
        source_kind_of(&snapshot, "datasets")?.is_some(),
        "the source the fixture scanned resolves to a kind"
    );
    assert_eq!(source_kind_of(&snapshot, "no-such-source")?, None);
    assert_eq!(
        crate::layout::classify(None, "loras"),
        "(unplaced)",
        "and a kind that could not be resolved reads unplaced rather than the folder"
    );
    Ok(())
}

#[test]
fn inspect_opens_without_seeding_shelves_or_a_self_row() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("models");
    fs::create_dir_all(&input)?;
    fs::write(input.join("w.safetensors"), lora_header_named("t"))?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    assert!(
        !catalog.list_shelves()?.is_empty(),
        "a scan seeds the shelves"
    );
    assert_eq!(catalog.list_nodes()?.len(), 1, "and records this machine");

    let read = catalog.database.r_transaction()?;
    let shelves: Vec<Shelf> = read
        .scan()
        .primary::<Shelf>()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    let transaction = catalog.database.rw_transaction()?;
    for shelf in shelves {
        transaction.remove(shelf)?;
    }
    let this: Option<Node> = transaction.get().primary(LOCAL_NODE)?;
    transaction.remove(this.ok_or_else(|| IndexError::Catalog("the self row".into()))?)?;
    transaction.commit()?;
    drop(catalog);

    let inspected = Catalog::inspect(&native)?;
    assert!(
        inspected.list_shelves()?.is_empty(),
        "inspecting seeds no shelf"
    );
    assert!(
        inspected.list_nodes()?.is_empty(),
        "and records no node either"
    );
    drop(inspected);

    let opened = Catalog::open(&native)?;
    assert!(
        !opened.list_shelves()?.is_empty(),
        "opening restores the shelves"
    );
    assert_eq!(
        opened.list_nodes()?.len(),
        1,
        "and records this machine again"
    );
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
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report.absent, 1,
        "a noisy path is walked past, so its row goes absent"
    );
    assert!(catalog.remove_noise(id)?);
    assert!(!catalog.is_noise(&gone)?);
    Ok(())
}

#[test]
fn the_triage_queue_leads_the_seeded_shelves() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let shelves = catalog.list_shelves()?;
    let triage = shelves
        .iter()
        .find(|shelf| shelf.name == "triage queue")
        .ok_or_else(|| IndexError::Catalog("the triage queue is seeded".into()))?;
    assert_eq!(triage.position, 0);
    assert_eq!(
        shelves.first().map(|shelf| shelf.name.as_str()),
        Some("triage queue"),
        "the rail and the SHELVES tab both draw it first"
    );
    catalog.seed_builtin_shelves()?;
    assert_eq!(
        catalog
            .list_shelves()?
            .first()
            .map(|shelf| shelf.name.as_str()),
        Some("triage queue"),
        "re-seeding keeps it there"
    );
    Ok(())
}

/// A catalog holding three artifacts no rule classified and two flux LoRAs, which is what
/// two of the seeded shelves answer with and every other one answers nothing with.
fn shelf_count_fixture() -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let mut repositories = Vec::with_capacity(5);
    for index in 0..3_u128 {
        let artifact = seeded_artifact(
            0x7100 + index,
            &format!("mystery-{index}"),
            None,
            Artifact {
                role: Role::Unk,
                role_confidence: 0.2,
                ..seeded_facts()
            },
        );
        repositories.push(seeded_revision(
            seeded_repo(&format!("misc/mystery-{index}"), RepoType::Models),
            0x71 + index as u8,
            vec![seeded_entry(
                &format!("mystery-{index}.bin"),
                1_024,
                Some(artifact),
            )],
        ));
    }
    for index in 0..2_u128 {
        let artifact = seeded_artifact(
            0x7200 + index,
            &format!("flux-lora-{index}"),
            Some("kohya"),
            Artifact {
                role: Role::Lora,
                base: Base::new("flux"),
                base_label: Some("flux".to_owned()),
                ..seeded_facts()
            },
        );
        repositories.push(seeded_revision(
            seeded_repo(&format!("kohya/flux-lora-{index}"), RepoType::Models),
            0x81 + index as u8,
            vec![seeded_entry(
                &format!("flux-lora-{index}.safetensors"),
                2_048,
                Some(artifact),
            )],
        ));
    }
    catalog.seed(&repositories)?;
    catalog.rebuild_search(|_| {})?;
    Ok((directory, catalog))
}

#[test]
fn a_shelf_reports_how_many_artifacts_its_query_answers() -> Result<()> {
    let (_directory, catalog) = shelf_count_fixture()?;
    let counted = catalog.shelves_with_counts(jiff::Timestamp::now())?;
    let count_of = |counted: &[(ShelfRecord, Option<u64>)], name: &str| {
        counted
            .iter()
            .find(|(shelf, _)| shelf.name == name)
            .map(|(_, count)| *count)
    };
    assert_eq!(counted.len(), 6, "every shelf is counted");
    assert_eq!(
        count_of(&counted, "triage queue"),
        Some(Some(3)),
        "the triage queue answers the three unclassified artifacts"
    );
    assert_eq!(count_of(&counted, "flux workhorse"), Some(Some(2)));
    assert_eq!(
        count_of(&counted, "everything on laptop"),
        Some(Some(0)),
        "a shelf naming a node this catalog does not hold counts nothing"
    );

    let flux = counted
        .iter()
        .find(|(shelf, _)| shelf.name == "flux workhorse")
        .map(|(shelf, _)| shelf.id)
        .ok_or_else(|| IndexError::Catalog("the flux shelf is seeded".into()))?;
    catalog.update_shelf(flux, "flux workhorse", "shelf:nowhere", 1)?;
    let recounted = catalog.shelves_with_counts(jiff::Timestamp::now())?;
    assert_eq!(
        count_of(&recounted, "flux workhorse"),
        Some(None),
        "a shelf whose query does not compile reports no count"
    );
    assert_eq!(
        count_of(&recounted, "triage queue"),
        Some(Some(3)),
        "and its siblings still report numbers"
    );
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
fn a_revision_listing_resolves_the_dedupe_mark_only_when_it_is_asked_to() -> Result<()> {
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
    let repository = only_repository(&catalog)?;

    let resolved = catalog.revision_entries(repository.id, 0, Dedupe::Resolve)?;
    assert_eq!(resolved.len(), 2);
    assert!(
        resolved.iter().all(|entry| entry.deduped),
        "two paths hold the one artifact"
    );

    let ignored = catalog.revision_entries(repository.id, 0, Dedupe::Ignore)?;
    assert_eq!(
        ignored
            .iter()
            .map(|entry| entry.rel_path.clone())
            .collect::<Vec<_>>(),
        resolved
            .iter()
            .map(|entry| entry.rel_path.clone())
            .collect::<Vec<_>>(),
        "the same files, in the same order"
    );
    assert!(
        ignored.iter().all(|entry| !entry.deduped),
        "a listing that was not asked for the mark does not read the records behind it"
    );
    Ok(())
}
