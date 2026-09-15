use super::*;

#[test]
fn an_hf_cache_revision_gets_its_card_and_its_config() -> Result<()> {
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
                b"---\ninstance_prompt: acme style\n---\nTrained on chinchilla photographs.\n"
                    .to_vec(),
            ),
            (
                "config.json",
                b"{\"architectures\":[\"LlamaForCausalLM\"]}".to_vec(),
            ),
            ("unet/adapter.safetensors", plain_header()),
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

    let (repo_id, revision) = only_slot(&catalog)?;
    assert_eq!(
        stored_card(&catalog, repo_id, revision)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "the snapshot link names a README the blob it points at does not"
    );
    let rows = search(&catalog, "chinchilla")?;
    assert_eq!(rows.total, 1, "the card of a cached revision is searchable");
    assert_eq!(rows.rows[0].path, "unet/adapter.safetensors");
    assert_eq!(
        rows.rows[0].trigger.as_deref(),
        Some("acme style"),
        "the README frontmatter reached inference"
    );

    let read = catalog.database.r_transaction()?;
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(records.len(), 1, "only the weights become an artifact");
    let detail: ArtifactDetail = read
        .get()
        .primary(records[0].id)?
        .ok_or_else(|| IndexError::Catalog("detail missing".into()))?;
    assert!(
        detail.evidence.iter().any(|evidence| {
            evidence.field == "role" && evidence.source == hoard_core::EvidenceSource::Config
        }),
        "the cached config.json was consulted: {:?}",
        detail.evidence
    );
    Ok(())
}

/// The repository and revision the fixture's one weight sits at.
fn only_slot(catalog: &Catalog) -> Result<(i64, u32)> {
    let read = catalog.database.r_transaction()?;
    let entry: Entry = read
        .scan()
        .primary()?
        .all()?
        .next()
        .transpose()?
        .ok_or_else(|| IndexError::Catalog("the fixture holds no entry".into()))?;
    Ok((entry.repo_id, entry.revision))
}

#[test]
fn a_revision_card_is_stored_once_and_indexed_for_every_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("acme");
    fs::create_dir(&input)?;
    fs::write(input.join("README.md"), card_body("chinchilla"))?;
    for index in 0..5 {
        fs::write(
            input.join(format!("adapter{index}.safetensors")),
            lora_header_named(&format!("block{index}")),
        )?;
    }
    let config = IndexConfig {
        sources: vec![source(&input, "loose")],
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

    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<models::RevisionText>()?,
        1,
        "five artifacts of one revision share one card"
    );
    drop(read);
    assert_eq!(
        search(&catalog, "")?.total,
        5,
        "every weight is an artifact of its own"
    );
    assert_eq!(
        search(&catalog, "chinchilla")?.total,
        5,
        "the card the revision holds is indexed into every one of them"
    );
    Ok(())
}

#[test]
fn a_changed_card_reindexes_the_revision_and_an_unchanged_one_writes_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("acme");
    fs::create_dir(&input)?;
    fs::write(input.join("README.md"), card_body("chinchilla"))?;
    fs::write(input.join("adapter.safetensors"), lora_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "loose")],
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
    let (repo_id, revision) = only_slot(&catalog)?;
    assert_eq!(
        stored_card(&catalog, repo_id, revision)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "the revision holds the body of its README"
    );
    assert_eq!(search(&catalog, "chinchilla")?.total, 1);

    let sentinel = "a card no walk of this tree would produce";
    let transaction = catalog.database.rw_transaction()?;
    transaction.upsert(models::RevisionText {
        key: LocationKey::revision(repo_id, revision),
        card: sentinel.to_owned(),
    })?;
    transaction.commit()?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        stored_card(&catalog, repo_id, revision)?.as_deref(),
        Some(sentinel),
        "a revision no file of which changed does not have its card written again"
    );

    fs::write(input.join("README.md"), card_body("vicuna"))?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;
    assert_eq!(
        stored_card(&catalog, repo_id, revision)?.as_deref(),
        Some("Trained on vicuna photographs.\n"),
        "a card that changed is written once for the revision"
    );
    assert_eq!(
        search(&catalog, "vicuna")?.total,
        1,
        "the artifact the card belongs to is indexed again"
    );
    assert_eq!(
        search(&catalog, "chinchilla")?.total,
        0,
        "the card it replaced is gone from the index"
    );
    Ok(())
}

#[test]
fn a_revision_refiled_onto_a_new_slot_keeps_the_card_the_new_commit_wrote() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("clones");
    let clone = input.join("org").join("clone");
    fs::create_dir_all(&clone)?;
    git(&clone, &["init", "--initial-branch=main", "--quiet"]);
    fs::write(clone.join("README.md"), card_body("chinchilla"))?;
    fs::write(clone.join("adapter.safetensors"), lora_header())?;
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "first import", "--quiet"]);
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::Git,
            ..source(&input, "clones")
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
    let repo = RepoId::parse_ref(RepoType::Models, "org/clone")?;
    let held = only_revision(&catalog, &repo)?;
    assert_eq!(
        stored_card(&catalog, held.0, held.1)?.as_deref(),
        Some("Trained on chinchilla photographs.\n")
    );

    fs::write(clone.join("README.md"), card_body("vicuna"))?;
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "reword the card", "--quiet"]);
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    let advanced = only_revision(&catalog, &repo)?;
    assert_ne!(
        advanced.1, held.1,
        "the branch moved onto a commit the catalog had not seen"
    );
    assert_eq!(
        stored_card(&catalog, advanced.0, advanced.1)?.as_deref(),
        Some("Trained on vicuna photographs.\n"),
        "the weights refiled onto the new slot do not carry the old card back with them"
    );
    assert_eq!(
        stored_card(&catalog, held.0, held.1)?,
        None,
        "and the slot they left keeps none"
    );
    Ok(())
}

/// The repository row id and the ordinal of the revision `main` names.
fn only_revision(catalog: &Catalog, repo: &RepoId) -> Result<(i64, u32)> {
    let detail = catalog
        .repository(repo)?
        .ok_or_else(|| IndexError::Catalog("the fixture holds no repository".into()))?;
    let main = catalog
        .resolve_revision(&detail, "main")
        .ok_or_else(|| IndexError::Catalog("the fixture holds no main".into()))?;
    Ok((detail.id.get(), main.ordinal))
}

#[test]
fn a_reordered_source_keeps_every_entry_on_its_revision() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let held = CommitSha::from_bytes([0xcc; 20]);
    let ahead = CommitSha::from_bytes([0xbb; 20]);
    snapshot(&hub, held, "adapter.safetensors", "down");
    fs::write(hub.join(STYLER).join("refs/dev"), ahead.to_hex())?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let before = only_repository(&catalog)?;
    assert_eq!(
        before.revisions.len(),
        2,
        "the ref names a revision of its own"
    );
    assert_eq!(before.revisions[0].sha, Some(held));
    assert_eq!(
        paths_of_revision(&catalog, &before, 0)?,
        ["adapter.safetensors"]
    );

    snapshot(&hub, ahead, "other.safetensors", "up");
    fs::write(hub.join(STYLER).join("refs/main"), held.to_hex())?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    let after = only_repository(&catalog)?;
    assert_eq!(
        after.revisions.len(),
        2,
        "a revision rediscovered in another position is not a second revision"
    );
    assert_eq!(
        after.revisions[0].sha,
        Some(held),
        "the stored revision keeps its ordinal though discovery now lists it second"
    );
    assert_eq!(after.revisions[1].sha, Some(ahead));
    assert_eq!(
        paths_of_revision(&catalog, &after, 0)?,
        ["adapter.safetensors"]
    );
    assert_eq!(
        paths_of_revision(&catalog, &after, 1)?,
        ["other.safetensors"]
    );
    let main = catalog
        .resolve_revision(&after, "main")
        .ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert_eq!(main.ordinal, 0);
    Ok(())
}

#[test]
fn a_new_revision_is_appended_and_a_vanished_one_keeps_its_slot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let kept = CommitSha::from_bytes([0xbb; 20]);
    let gone = CommitSha::from_bytes([0xcc; 20]);
    let fresh = CommitSha::from_bytes([0xaa; 20]);
    snapshot(&hub, kept, "kept.safetensors", "one");
    snapshot(&hub, gone, "gone.safetensors", "two");
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let before = only_repository(&catalog)?;
    assert_eq!(before.revisions.len(), 2);
    assert_eq!(before.revisions[1].sha, Some(gone));

    fs::remove_dir_all(hub.join(STYLER).join("snapshots").join(gone.to_hex()))?;
    snapshot(&hub, fresh, "fresh.safetensors", "three");
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    let after = only_repository(&catalog)?;
    assert_eq!(after.revisions.len(), 3);
    assert_eq!(
        after
            .revisions
            .iter()
            .map(|revision| revision.sha)
            .collect::<Vec<_>>(),
        [Some(kept), Some(gone), Some(fresh)],
        "a rediscovered revision keeps its slot, a new one is appended"
    );
    assert_eq!(
        paths_of_revision(&catalog, &after, 0)?,
        ["kept.safetensors"]
    );
    assert_eq!(
        paths_of_revision(&catalog, &after, 2)?,
        ["fresh.safetensors"]
    );
    let vanished = catalog.revision_entries(after.id, 1, Dedupe::Ignore)?;
    assert_eq!(
        vanished.len(),
        1,
        "the slot still holds the entries filed under it"
    );
    assert_eq!(vanished[0].state, State::Remote);
    let main = catalog
        .resolve_revision(&after, "main")
        .ok_or_else(|| IndexError::Catalog("main".into()))?;
    assert_eq!(
        main.ordinal, 2,
        "a revision no ref reaches gives up the name that moved"
    );
    Ok(())
}

/// A revision holding a weight and the two documents that describe it.
fn documented_revision(root: &Path) -> Result<IndexConfig> {
    let input = root.join("files");
    fs::create_dir(&input)?;
    fs::write(
        input.join("README.md"),
        "---\ninstance_prompt: acme style\n---\nA style LoRA.\n",
    )?;
    fs::write(
        input.join("config.json"),
        "{\"architectures\":[\"UNet2DConditionModel\"]}",
    )?;
    fs::write(input.join("adapter.safetensors"), lora_header())?;
    Ok(IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    })
}

#[test]
#[cfg(unix)]
fn an_unchanged_revision_is_rescanned_without_opening_its_documents() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir()?;
    let config = documented_revision(directory.path())?;
    let input = directory.path().join("files");
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;

    for document in ["README.md", "config.json"] {
        fs::set_permissions(input.join(document), fs::Permissions::from_mode(0o000))?;
    }
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    for document in ["README.md", "config.json"] {
        fs::set_permissions(input.join(document), fs::Permissions::from_mode(0o600))?;
    }

    assert_eq!(
        report,
        RescanReport {
            unchanged: 3,
            changed: 0,
            added: 0,
            absent: 0,
            interrupted: false,
        },
        "a revision no file changed in is settled before anything in it is opened"
    );
    Ok(())
}

#[test]
fn a_changed_weight_still_reads_the_revision_documents() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = documented_revision(directory.path())?;
    let input = directory.path().join("files");
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    fs::write(
        input.join("adapter.safetensors"),
        lora_header_named("mid_block"),
    )?;
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(report.changed, 1, "the rewritten weight is prepared again");

    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    let adapter = rows
        .rows
        .iter()
        .find(|row| row.path == "adapter.safetensors")
        .ok_or_else(|| IndexError::Catalog("adapter".into()))?;
    assert_eq!(
        adapter.trigger.as_deref(),
        Some("acme style"),
        "the revision's README described the weight the walk read again"
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
fn a_git_clone_keeps_its_git_dir_and_commit_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = git_fixture(directory.path(), None)?;
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
    assert_eq!(detail.remote, None, "a clone with no origin records none");
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
fn a_scanned_clone_records_the_remote_it_came_from() -> Result<()> {
    const REMOTE: &str = "https://huggingface.co/org/clone.git";
    let directory = tempfile::tempdir()?;
    let (_input, config) = git_fixture(directory.path(), Some(REMOTE))?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let detail = catalog
        .repository(&RepoId::parse_ref(RepoType::Models, "org/clone")?)?
        .ok_or_else(|| IndexError::Catalog("repo".into()))?;
    assert_eq!(
        detail.remote.as_deref(),
        Some(REMOTE),
        "the url the clone came from reaches the repository record"
    );
    Ok(())
}

#[test]
fn a_clone_whose_branch_advanced_keeps_its_unchanged_files_on_the_branch() -> Result<()> {
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
    assert_eq!(
        paths_of_ref(&catalog, &repo, "main")?,
        ["README.md"],
        "the first commit holds the file"
    );

    fs::write(clone.join("NOTES.md"), "# notes\n")?;
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-m", "second import", "--quiet"]);
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
            absent: 0,
            interrupted: false,
        },
        "the file the commit did not touch is refiled onto the new ordinal, not read again"
    );

    let detail = repository_of(&catalog, &repo)?;
    assert_eq!(
        detail.revisions.len(),
        1,
        "the branch's old head, which no ref reaches and no entry names, is retired"
    );
    assert_eq!(
        detail.revisions[0].ordinal, 1,
        "the retired head keeps its ordinal, so the new head takes the next one"
    );
    let mut listed = paths_of_ref(&catalog, &repo, "main")?;
    listed.sort();
    assert_eq!(
        listed,
        ["NOTES.md", "README.md"],
        "a file the second commit did not touch is still filed under the branch"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let cards: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.rel_path == "README.md")
        .collect();
    assert_eq!(
        cards.len(),
        1,
        "the row moved to the branch's new ordinal; it was not copied and retracted"
    );
    assert_eq!(cards[0].state, State::Local);
    Ok(())
}

#[test]
fn a_path_two_revisions_share_is_left_alone_when_a_third_revision_appears() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let repo = hub.join(STYLER);
    fs::create_dir_all(repo.join("blobs"))?;
    fs::create_dir_all(repo.join("refs"))?;
    let blob = repo.join("blobs").join("a".repeat(64));
    fs::write(&blob, lora_header())?;
    let snapshot_of = |commit: &str| -> Result<()> {
        let snapshot = repo.join("snapshots").join(commit);
        fs::create_dir_all(&snapshot)?;
        std::os::unix::fs::symlink(&blob, snapshot.join("adapter.safetensors"))?;
        Ok(())
    };
    snapshot_of(&"b".repeat(40))?;
    snapshot_of(&"c".repeat(40))?;
    fs::write(repo.join("refs/main"), "b".repeat(40))?;
    let config = hub_config(&hub);
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    let before = only_repository(&catalog)?;
    assert_eq!(
        before.revisions.len(),
        2,
        "two snapshots of one blob are two revisions of one path"
    );

    snapshot_of(&"d".repeat(40))?;
    let report = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(
        report,
        RescanReport {
            unchanged: 2,
            changed: 0,
            added: 1,
            absent: 0,
            interrupted: false,
        },
        "the fresh revision stores its own row; neither stored row is refiled onto it"
    );
    let after = only_repository(&catalog)?;
    assert_eq!(after.revisions.len(), 3);
    for ordinal in 0..3 {
        assert_eq!(
            paths_of_revision(&catalog, &after, ordinal)?,
            ["adapter.safetensors"],
            "revision {ordinal} keeps the path it holds"
        );
    }
    Ok(())
}

fn paths_of_ref(catalog: &Catalog, repo: &RepoId, wanted: &str) -> Result<Vec<String>> {
    let detail = repository_of(catalog, repo)?;
    let revision = catalog
        .resolve_revision(&detail, wanted)
        .ok_or_else(|| IndexError::Catalog(format!("no revision {wanted}")))?;
    Ok(catalog
        .revision_entries(detail.id, revision.ordinal, Dedupe::Ignore)?
        .into_iter()
        .map(|entry| entry.rel_path)
        .collect())
}

#[test]
fn a_revision_that_lost_a_shard_is_prepared_again_though_its_survivors_are_unchanged() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Thirds",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
            ("model-00003-of-00003.safetensors", shards[2].clone()),
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
    let whole = only_artifact(&catalog)?;
    assert_eq!(whole.shard_count, Some(3));

    let snapshot = hub
        .join("models--Qwen--Qwen3-Thirds")
        .join("snapshots")
        .join("b".repeat(40));
    fs::remove_file(snapshot.join("model-00003-of-00003.safetensors"))?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    let short = only_artifact(&catalog)?;
    assert_eq!(
        short.shard_count,
        Some(2),
        "a set that lost a member is a different model from the one on record"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(
        entries
            .iter()
            .filter(|entry| entry.state != State::Remote)
            .all(|entry| entry.triage),
        "an incomplete set is triage"
    );
    Ok(())
}

fn only_artifact(catalog: &Catalog) -> Result<ArtifactRecord> {
    let read = catalog.database.r_transaction()?;
    let mut records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    match records.len() {
        1 => records
            .pop()
            .ok_or_else(|| IndexError::Catalog("record".into())),
        found => Err(IndexError::Catalog(format!("{found} artifacts"))),
    }
}
