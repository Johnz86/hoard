use super::*;

/// A loose tree of three weights where only the one inside `gemma/` has a card beside it.
fn encoders_tree(directory: &Path) -> Result<Catalog> {
    let input = directory.join("files");
    fs::create_dir_all(input.join("t5"))?;
    fs::create_dir_all(input.join("gemma"))?;
    fs::write(
        input.join("ministral.safetensors"),
        lora_header_named("ministral"),
    )?;
    fs::write(input.join("t5/umt5.safetensors"), lora_header_named("umt5"))?;
    fs::write(
        input.join("gemma/model.safetensors"),
        lora_header_named("gemma"),
    )?;
    fs::write(
        input.join("gemma/README.md"),
        "---\ntags:\n- gemma3\n- gemma\n- google\n---\nTrained on chinchilla photographs.\n",
    )?;
    let config = IndexConfig {
        sources: vec![source(&input, "comfy")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

fn row_named(page: &QueryPage, name: &str) -> Result<SearchRow> {
    page.rows
        .iter()
        .find(|row| row.name == name)
        .cloned()
        .ok_or_else(|| IndexError::Catalog(format!("no row named {name}")))
}

#[test]
fn a_card_is_searchable_only_through_the_files_it_covers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = encoders_tree(directory.path())?;

    let found = search(&catalog, "chinchilla")?;
    assert_eq!(
        found.total, 1,
        "the card's words reach only the weight below it"
    );
    assert_eq!(found.rows[0].name, "model.safetensors");
    assert_eq!(
        search(&catalog, "")?.total,
        3,
        "every weight is still an artifact of its own"
    );
    let repository = only_repository(&catalog)?;
    let revision = repository
        .revisions
        .first()
        .ok_or_else(|| IndexError::Catalog("the repository holds no revision".into()))?;
    assert_eq!(
        stored_card(&catalog, repository.id.get(), revision.ordinal)?.as_deref(),
        Some("Trained on chinchilla photographs.\n"),
        "the revision still stores its one shallowest card"
    );
    Ok(())
}

#[test]
fn a_root_card_is_searchable_through_every_file_of_its_revision() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("unet"))?;
    fs::write(input.join("README.md"), card_body("vicuna"))?;
    fs::write(input.join("a.safetensors"), lora_header_named("first"))?;
    fs::write(
        input.join("unet/b.safetensors"),
        lora_header_named("second"),
    )?;
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

    assert_eq!(
        search(&catalog, "vicuna")?.total,
        2,
        "a card at the revision root covers every file under it"
    );
    Ok(())
}

#[test]
fn a_card_that_moves_to_another_folder_is_stored_again() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let card = |directory| models::CardText {
        directory,
        body: "words",
    };
    let transaction = catalog.database.rw_transaction()?;

    assert!(
        models::store_revision_card(&transaction, 7, 0, card("a"))?,
        "a card the revision did not hold is written"
    );
    assert!(
        !models::store_revision_card(&transaction, 7, 0, card("a"))?,
        "the same card in the same folder writes nothing"
    );
    assert!(
        models::store_revision_card(&transaction, 7, 0, card("b"))?,
        "the same words in another folder describe other files and are written again"
    );
    let stored: models::RevisionText = transaction
        .get()
        .primary(LocationKey::revision(7, 0))?
        .ok_or_else(|| IndexError::Catalog("the card was not stored".into()))?;
    assert_eq!(stored.directory, "b");
    assert_eq!(stored.text_for("b/x.safetensors"), "words");
    assert_eq!(stored.text_for("b/deeper/x.safetensors"), "words");
    assert_eq!(stored.text_for("a/x.safetensors"), "");
    assert_eq!(stored.text_for("x.safetensors"), "");
    assert_eq!(stored.text_for("bb/x.safetensors"), "");
    Ok(())
}

#[test]
fn a_pulled_card_below_the_root_reaches_only_the_files_under_it() -> Result<()> {
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
    let under = pull_into(
        &catalog,
        slot,
        &root,
        "loras/film-grain.safetensors",
        &lora_header_named("grain"),
        0x42,
    )?;
    let beside = pull_into(
        &catalog,
        slot,
        &root,
        "vae/ae.safetensors",
        &lora_header_named("autoencoder"),
        0x43,
    )?;
    catalog.rebuild_search(|_| {})?;

    pull_into(
        &catalog,
        slot,
        &root,
        "loras/README.md",
        &card_body("capybara"),
        0x44,
    )?;
    catalog.refresh_search(|_| {})?;
    let found = search(&catalog, "capybara")?;
    assert!(
        found.rows.iter().any(|row| row.artifact_id == under),
        "the weight under the pulled card is indexed with it"
    );
    assert!(
        !found.rows.iter().any(|row| row.artifact_id == beside),
        "the weight in a sibling folder is not"
    );
    Ok(())
}

#[test]
fn a_readme_in_a_subfolder_gives_the_files_beside_that_folder_no_trigger_words() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = encoders_tree(directory.path())?;

    let page = search(&catalog, "")?;
    assert_eq!(page.total, 3, "each weight is an artifact of its own");
    assert_eq!(
        row_named(&page, "model.safetensors")?.trigger.as_deref(),
        Some("gemma3, gemma, google"),
        "the weight under the card reads the card's tags"
    );
    for beside in ["ministral.safetensors", "umt5.safetensors"] {
        let trigger = row_named(&page, beside)?.trigger;
        assert!(
            trigger
                .as_deref()
                .is_none_or(|trigger| !trigger.contains("gemma")),
            "{beside} sits outside the card's directory and reads nothing from it, got {trigger:?}"
        );
    }
    Ok(())
}
