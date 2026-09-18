use super::*;

use crate::catalog::{Listed, LookupAnswer};

fn sdxl_checkpoint_header(width: u64) -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(&format!(
        "{{\"model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[{width},2048],\"data_offsets\":[0,2048]}},\
         \"model.diffusion_model.label_emb.0.0.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[1280,2816],\"data_offsets\":[2048,4096]}}}}"
    ))
}

fn language_model_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(
        "{\"model.layers.0.mlp.down_proj.weight\":\
         {\"dtype\":\"F16\",\"shape\":[4096,11008],\"data_offsets\":[0,2048]},\
         \"model.embed_tokens.weight\":\
         {\"dtype\":\"F16\",\"shape\":[32000,4096],\"data_offsets\":[2048,4096]}}",
    )
}

fn listing(base_model: &str) -> Listed {
    Listed {
        base_model: base_model.to_owned(),
        model: "One obsession".to_owned(),
        version: "v24".to_owned(),
        kind: "Checkpoint".to_owned(),
        trained_words: Vec::new(),
    }
}

fn answered(base_model: &str) -> LookupAnswer {
    LookupAnswer {
        asked_at: crate::time::timestamp_text(crate::time::now()),
        found: Some(listing(base_model)),
    }
}

fn unknown_at(asked_at: jiff::Timestamp) -> LookupAnswer {
    LookupAnswer {
        asked_at: crate::time::timestamp_text(asked_at),
        found: None,
    }
}

/// Two hashed sdxl checkpoints and one language model, scanned and hashed in one pass.
fn hashed_fixture(directory: &Path) -> Result<(IndexConfig, Catalog)> {
    let input = directory.join("files");
    fs::create_dir_all(input.join("checkpoints"))?;
    fs::create_dir_all(input.join("llm"))?;
    fs::write(
        input.join("checkpoints/one-obsession.safetensors"),
        sdxl_checkpoint_header(640),
    )?;
    fs::write(
        input.join("checkpoints/other-mix.safetensors"),
        sdxl_checkpoint_header(641),
    )?;
    fs::write(input.join("llm/qwen.safetensors"), language_model_header())?;
    let config = IndexConfig {
        data_dir: directory.to_path_buf(),
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &config.native_path(),
        "n",
        &config,
        HashPolicy::Now,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok((config, catalog))
}

fn candidate_of(catalog: &Catalog, name: &str) -> Result<Sha256> {
    let page = search(catalog, name)?;
    let row = page
        .rows
        .first()
        .ok_or_else(|| IndexError::Catalog(format!("{name} is not indexed")))?;
    row.blob_sha256
        .ok_or_else(|| IndexError::Catalog(format!("{name} was never hashed")))
}

fn evidence_of(catalog: &Catalog, sha: &Sha256) -> Result<Vec<Evidence>> {
    let row = catalog
        .artifact_by_sha256(sha)?
        .ok_or_else(|| IndexError::Catalog("the artifact is indexed".into()))?;
    let view = catalog
        .artifact_detail(row.artifact_id)?
        .ok_or_else(|| IndexError::Catalog("the artifact has a detail row".into()))?;
    Ok(view.evidence)
}

fn lookup_rows(evidence: &[Evidence]) -> Vec<&Evidence> {
    evidence
        .iter()
        .filter(|row| row.source == EvidenceSource::Lookup)
        .collect()
}

#[test]
fn a_listing_refines_the_stored_base_and_the_search_follows() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    assert_eq!(search(&catalog, "base:sdxl")?.total, 2);
    assert_eq!(search(&catalog, "base:illustrious")?.total, 0);

    let answers = vec![(sha, answered("Illustrious"))];
    catalog.remember_lookups(&answers)?;
    assert_eq!(catalog.apply_lookups(&answers)?, 1);

    let refined = search(&catalog, "base:illustrious")?;
    assert_eq!(refined.total, 1, "the refined lineage answers for itself");
    assert_eq!(
        refined.rows[0].base_label.as_deref(),
        Some("Illustrious"),
        "the lineage is labelled by its own name"
    );
    assert_eq!(
        search(&catalog, "base:Illustrious")?.total,
        1,
        "a base value is normalized before it compiles"
    );
    assert_eq!(
        search(&catalog, "base:sdxl")?.total,
        2,
        "the architecture still answers for the file it refined"
    );
    let evidence = evidence_of(&catalog, &sha)?;
    let rows = lookup_rows(&evidence);
    assert_eq!(rows.len(), 1, "one lookup row, not one per pass");
    assert_eq!(rows[0].field, "base");
    assert_eq!(rows[0].value, "illustrious");
    assert_eq!(
        rows[0].detail.as_deref(),
        Some("Civitai lists these bytes as One obsession v24, base Illustrious")
    );
    Ok(())
}

#[test]
fn a_users_verdict_is_never_overwritten() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    let row = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("the artifact is indexed".into()))?;
    catalog.put_override(row.artifact_id, None, None, Some(Base::new("pony")))?;

    let answers = vec![(sha, answered("Illustrious"))];
    assert_eq!(catalog.apply_lookups(&answers)?, 0);

    assert_eq!(search(&catalog, "base:pony")?.total, 1);
    assert_eq!(search(&catalog, "base:illustrious")?.total, 0);
    assert!(
        lookup_rows(&evidence_of(&catalog, &sha)?).is_empty(),
        "an artifact a user decided is left alone entirely"
    );
    Ok(())
}

#[test]
fn an_artifact_a_user_decided_is_not_a_candidate() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    let row = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("the artifact is indexed".into()))?;
    assert_eq!(catalog.lookup_candidates(16)?.len(), 2);

    catalog.put_override(row.artifact_id, None, None, Some(Base::new("pony")))?;

    let candidates = catalog.lookup_candidates(16)?;
    assert_eq!(candidates.len(), 1, "a decided artifact leaves the window");
    assert!(
        candidates.iter().all(|candidate| candidate.sha256 != sha),
        "an answer could do nothing for it, so it never takes a place in a window again"
    );

    catalog.clear_override(row.artifact_id)?;
    assert_eq!(
        catalog.lookup_candidates(16)?.len(),
        2,
        "an artifact whose verdict is withdrawn can be asked about again"
    );
    Ok(())
}

#[test]
fn a_verdict_that_names_no_base_leaves_the_artifact_askable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    let row = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("the artifact is indexed".into()))?;

    catalog.put_override(row.artifact_id, Some(Role::Ckpt), None, None)?;

    assert!(
        catalog
            .lookup_candidates(16)?
            .iter()
            .any(|candidate| candidate.sha256 == sha),
        "a verdict on the role says nothing about the base"
    );
    Ok(())
}

#[test]
fn an_applied_artifact_is_no_longer_a_candidate() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    let before = catalog.lookup_candidates(16)?;
    assert_eq!(before.len(), 2, "both checkpoints are candidates");
    assert!(before.iter().any(|candidate| candidate.sha256 == sha));

    let answers = vec![(sha, answered("Illustrious"))];
    catalog.remember_lookups(&answers)?;
    catalog.apply_lookups(&answers)?;

    let after = catalog.lookup_candidates(16)?;
    assert_eq!(after.len(), 1, "what was applied is not asked about again");
    assert!(after.iter().all(|candidate| candidate.sha256 != sha));
    assert_eq!(
        catalog.lookup_candidates(1)?.len(),
        1,
        "a pass holds a window, not the catalog"
    );
    Ok(())
}

#[test]
fn an_unhashed_artifact_is_not_a_candidate() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("checkpoints"))?;
    fs::write(
        input.join("checkpoints/one-obsession.safetensors"),
        sdxl_checkpoint_header(640),
    )?;
    let config = IndexConfig {
        data_dir: directory.path().to_path_buf(),
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &config.native_path(),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;

    assert!(
        catalog.lookup_candidates(16)?.is_empty(),
        "a file with no sha256 has nothing to ask about"
    );

    catalog.hash_pending(&config, |_| {})?;
    assert_eq!(catalog.lookup_candidates(16)?.len(), 1);
    Ok(())
}

#[test]
fn an_llm_is_never_asked_about() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let language_model = candidate_of(&catalog, "qwen")?;
    assert_eq!(
        catalog
            .artifact_by_sha256(&language_model)?
            .map(|row| row.role),
        Some(Role::Llm),
        "the fixture is a language model, which is what makes this test say anything"
    );

    let candidates = catalog.lookup_candidates(16)?;

    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.sha256 != language_model),
        "a language model is not a role a listing service carries"
    );
    Ok(())
}

#[test]
fn an_unknown_answer_is_a_candidate_again_only_after_thirty_days() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;

    catalog.remember_lookups(&[(sha, unknown_at(crate::time::now()))])?;
    let asked = catalog.lookup_candidates(16)?;
    assert!(
        asked.iter().all(|candidate| candidate.sha256 != sha),
        "a hash the service has just denied knowing is not asked about again"
    );

    let long_ago = crate::time::now() - jiff::SignedDuration::from_hours(31 * 24);
    catalog.remember_lookups(&[(sha, unknown_at(long_ago))])?;

    let stale = catalog.lookup_candidates(16)?;
    assert!(
        stale.iter().any(|candidate| candidate.sha256 == sha),
        "an uploader can list a file after hoard asked, so an unknown does not stand forever"
    );
    Ok(())
}

#[test]
fn a_contradicted_listing_leaves_the_base_and_records_why() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;

    let answers = vec![(sha, answered("Flux.2 D"))];
    catalog.remember_lookups(&answers)?;
    assert_eq!(catalog.apply_lookups(&answers)?, 0);

    assert_eq!(search(&catalog, "base:sdxl")?.total, 2, "the header stands");
    assert_eq!(search(&catalog, "base:flux")?.total, 0);
    let evidence = evidence_of(&catalog, &sha)?;
    let rows = lookup_rows(&evidence);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, "flux2dev");
    assert_eq!(
        rows[0].detail.as_deref(),
        Some(
            "Civitai lists these bytes as One obsession v24, base Flux.2 D - hoard has sdxl, \
             which stands"
        )
    );
    assert!(
        catalog
            .lookup_candidates(16)?
            .iter()
            .all(|candidate| candidate.sha256 != sha),
        "a contradiction is an answer, and the same hash is not asked about twice"
    );
    Ok(())
}

#[test]
fn an_answer_no_base_table_knows_records_that_and_changes_nothing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;

    let answers = vec![(sha, answered("Stable Cascade"))];
    catalog.remember_lookups(&answers)?;
    assert_eq!(catalog.apply_lookups(&answers)?, 0);

    let evidence = evidence_of(&catalog, &sha)?;
    let rows = lookup_rows(&evidence);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, "Stable Cascade");
    assert_eq!(
        rows[0].detail.as_deref(),
        Some(
            "Civitai lists these bytes as One obsession v24, base Stable Cascade - not a base \
             hoard knows"
        )
    );
    assert_eq!(search(&catalog, "base:sdxl")?.total, 2);
    Ok(())
}

#[test]
fn the_summary_still_agrees_with_a_fresh_count_after_a_batch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let refined = candidate_of(&catalog, "one-obsession")?;
    let contradicted = candidate_of(&catalog, "other-mix")?;
    let answers = vec![
        (refined, answered("Illustrious")),
        (contradicted, answered("Flux.2 D")),
    ];
    catalog.remember_lookups(&answers)?;

    assert_eq!(catalog.apply_lookups(&answers)?, 1);

    assert_eq!(
        catalog.count_summary()?,
        catalog.totals()?,
        "a batch of answers counts what a recount of the catalog counts"
    );
    assert_eq!(
        catalog.status()?.pending_changes,
        0,
        "the batch refreshed the index it logged"
    );
    Ok(())
}

fn tagged_lora_header(tags: &str) -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(&format!(
        "{{\"__metadata__\":{{\"ss_network_dim\":\"16\",\"ss_network_alpha\":\"8\",\
         \"ss_tag_frequency\":{tags}}},\
         \"lora_unet_down_blocks_0_attn.lora_down.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[16,768],\"data_offsets\":[0,24576]}},\
         \"lora_unet_down_blocks_0_attn.lora_up.weight\":\
         {{\"dtype\":\"F16\",\"shape\":[768,16],\"data_offsets\":[24576,49152]}}}}"
    ))
}

/// One hashed LoRA of the given name and header, alone under its own source.
fn hashed_lora(directory: &Path, name: &str, header: Vec<u8>) -> Result<Catalog> {
    let input = directory.join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras").join(name), header)?;
    let config = IndexConfig {
        data_dir: directory.to_path_buf(),
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &config.native_path(),
        "n",
        &config,
        HashPolicy::Now,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

fn only_row(catalog: &Catalog) -> Result<SearchRow> {
    search(catalog, "")?
        .rows
        .first()
        .cloned()
        .ok_or_else(|| IndexError::Catalog("the catalog holds no row".into()))
}

fn listing_with(base_model: &str, trained_words: &[&str]) -> Listed {
    Listed {
        trained_words: trained_words
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
        ..listing(base_model)
    }
}

fn answer_of(listed: Listed) -> LookupAnswer {
    LookupAnswer {
        asked_at: crate::time::timestamp_text(crate::time::now()),
        found: Some(listed),
    }
}

#[test]
fn a_listing_that_names_a_key_the_rules_spell_stores_no_label() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = hashed_lora(directory.path(), "mystery.safetensors", lora_header())?;
    let row = only_row(&catalog)?;
    assert!(row.base.is_empty(), "the header proves no architecture");
    let sha = row
        .blob_sha256
        .ok_or_else(|| IndexError::Catalog("the lora was never hashed".into()))?;

    let answers = vec![(sha, answer_of(listing("SDXL 1.0")))];
    assert_eq!(catalog.apply_lookups(&answers)?, 1);

    let named = only_row(&catalog)?;
    assert_eq!(named.base, Base::new("sdxl"));
    assert_eq!(
        named.base_label, None,
        "a key hoard's own rules spell is printed the way they spell it"
    );

    let other = tempfile::tempdir()?;
    let (_config, checkpoints) = hashed_fixture(other.path())?;
    let refined = candidate_of(&checkpoints, "one-obsession")?;
    let answers = vec![(refined, answer_of(listing("Illustrious")))];
    assert_eq!(checkpoints.apply_lookups(&answers)?, 1);
    assert_eq!(
        checkpoints
            .artifact_by_sha256(&refined)?
            .and_then(|row| row.base_label),
        Some("Illustrious".to_owned()),
        "a lineage no rule of hoard's spells keeps the spelling the listing writes"
    );
    Ok(())
}

#[test]
fn a_listing_is_kept_on_the_detail() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_config, catalog) = hashed_fixture(directory.path())?;
    let sha = candidate_of(&catalog, "one-obsession")?;
    let id = catalog
        .artifact_by_sha256(&sha)?
        .ok_or_else(|| IndexError::Catalog("the artifact is indexed".into()))?
        .artifact_id;

    let first = listing_with("Illustrious", &["one obsession"]);
    catalog.apply_lookups(&[(sha, answer_of(first.clone()))])?;

    assert_eq!(
        catalog
            .artifact_detail(id)?
            .and_then(|detail| detail.listing),
        Some(first),
        "the detail row keeps what the listing said, trigger words and all"
    );

    let second = listing_with("NoobAI", &["v24"]);
    catalog.apply_lookups(&[(sha, answer_of(second.clone()))])?;

    assert_eq!(
        catalog
            .artifact_detail(id)?
            .and_then(|detail| detail.listing),
        Some(second),
        "a later answer for the same bytes replaces the one before it"
    );
    let row = catalog
        .artifact_row(id)?
        .ok_or_else(|| IndexError::Catalog("the row is gone".into()))?;
    assert_eq!(
        row.trigger.as_deref(),
        Some("v24"),
        "the trigger words follow the listing the detail keeps"
    );
    let lookup_rows = catalog.artifact_detail(id)?.map_or(0, |detail| {
        detail
            .evidence
            .iter()
            .filter(|evidence| {
                evidence.field == "trigger" && evidence.source == EvidenceSource::Lookup
            })
            .count()
    });
    assert_eq!(lookup_rows, 1, "one lookup row on the trigger, the latest");
    Ok(())
}

#[test]
fn trained_words_outrank_a_file_name_guess_and_not_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let catalog = hashed_lora(directory.path(), "film-grain-v2.safetensors", lora_header())?;
    let guessed = only_row(&catalog)?;
    let sha = guessed
        .blob_sha256
        .ok_or_else(|| IndexError::Catalog("the lora was never hashed".into()))?;
    assert_eq!(
        strongest_trigger(&catalog, &sha)?,
        Some(EvidenceSource::Filename),
        "the file name is all this lora says about its trigger"
    );

    let listed = listing_with("SDXL 1.0", &["score_9", "rating_safe"]);
    catalog.apply_lookups(&[(sha, answer_of(listed))])?;

    assert_eq!(
        only_row(&catalog)?.trigger.as_deref(),
        Some("score_9, rating_safe"),
        "the words the uploader lists beat a guess read off the file name"
    );
    let evidence = evidence_of(&catalog, &sha)?;
    let triggers: Vec<&Evidence> = lookup_rows(&evidence)
        .into_iter()
        .filter(|row| row.field == "trigger")
        .collect();
    assert_eq!(
        triggers.len(),
        1,
        "one lookup row on the trigger, not one per pass"
    );
    assert_eq!(
        triggers[0].detail.as_deref(),
        Some("Civitai lists the trigger words of One obsession v24")
    );
    assert_eq!(
        search(&catalog, "trigger:rating")?.total,
        1,
        "the refresh reindexes the trigger the listing wrote"
    );

    let metadata = tempfile::tempdir()?;
    let known = hashed_lora(
        metadata.path(),
        "tagged.safetensors",
        tagged_lora_header("{\"acme\":{\"woodcut\":12,\"linocut\":3}}"),
    )?;
    let tagged = only_row(&known)?;
    let tagged_sha = tagged
        .blob_sha256
        .ok_or_else(|| IndexError::Catalog("the lora was never hashed".into()))?;
    assert_eq!(
        strongest_trigger(&known, &tagged_sha)?,
        Some(EvidenceSource::Metadata)
    );
    let held = tagged.trigger.clone();

    known.apply_lookups(&[(
        tagged_sha,
        answer_of(listing_with("SDXL 1.0", &["score_9"])),
    )])?;

    assert_eq!(
        only_row(&known)?.trigger,
        held,
        "the uploader's own training metadata outranks the page the file was listed on"
    );
    assert!(
        lookup_rows(&evidence_of(&known, &tagged_sha)?)
            .into_iter()
            .all(|row| row.field != "trigger"),
        "and no lookup row is filed on the trigger at all"
    );

    let empty = tempfile::tempdir()?;
    let silent = hashed_lora(empty.path(), "silent.safetensors", lora_header())?;
    let silent_sha = only_row(&silent)?
        .blob_sha256
        .ok_or_else(|| IndexError::Catalog("the lora was never hashed".into()))?;
    let before = only_row(&silent)?.trigger;

    silent.apply_lookups(&[(silent_sha, answer_of(listing("SDXL 1.0")))])?;

    assert_eq!(
        only_row(&silent)?.trigger,
        before,
        "an answer listing no trigger words writes none"
    );
    Ok(())
}

fn strongest_trigger(catalog: &Catalog, sha: &Sha256) -> Result<Option<EvidenceSource>> {
    let evidence = evidence_of(catalog, sha)?;
    Ok(evidence
        .iter()
        .filter(|row| row.field == "trigger")
        .max_by(|left, right| left.confidence.total_cmp(&right.confidence))
        .map(|row| row.source))
}
