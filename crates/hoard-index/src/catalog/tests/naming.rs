use super::*;

#[test]
fn two_sets_sharing_a_directory_and_stem_are_told_apart_by_member_count() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let index = br#"{"metadata":{},"weight_map":{"a.weight":"model-00001-of-00006.safetensors","b.weight":"model-00002-of-00006.safetensors"}}"#.to_vec();
    let mut files: Vec<(String, Vec<u8>)> =
        vec![("model.safetensors.index.json".to_owned(), index)];
    for number in 1..=6 {
        files.push((
            format!("model-{number:05}-of-00006.safetensors"),
            shard_header(&[&format!("model.layers.{number}.mlp.up_proj.weight")], 64),
        ));
    }
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.clone()))
        .collect();
    hf_cache_repo(&hub, "acme", "twins", &borrowed)?;
    let catalog = scan_hub(directory.path(), &hub)?;

    let rows = search(&catalog, "")?;
    let mut names: Vec<&str> = rows.rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["model.safetensors (2 files)", "model.safetensors (4 files)"],
        "two sets at one path are told apart by how many files each of them holds"
    );
    Ok(())
}

#[test]
fn an_incomplete_sets_flag_survives_into_the_stored_record() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Partial",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
        ],
    )?;
    let partial = scan_hub(directory.path(), &hub)?;
    assert!(
        only_record(&partial)?.incomplete,
        "a set that found two of three files says so on its own record"
    );

    let second = tempfile::tempdir()?;
    let whole_hub = second.path().join("hub");
    fs::create_dir(&whole_hub)?;
    hf_cache_repo(
        &whole_hub,
        "Qwen",
        "Qwen3-Whole",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[1].clone()),
        ],
    )?;
    let whole = scan_hub(second.path(), &whole_hub)?;
    assert!(
        !only_record(&whole)?.incomplete,
        "a set that found every file it declares is complete"
    );
    Ok(())
}

fn only_record(catalog: &Catalog) -> Result<ArtifactRecord> {
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

#[test]
fn a_quantization_sidecar_names_the_scheme_without_costing_the_model_its_role() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    hf_cache_repo(
        &hub,
        "nvidia",
        "Nemotron-FP8",
        &[
            (
                "config.json",
                br#"{"model_type":"nemotron","architectures":["NemotronForCausalLM"]}"#.to_vec(),
            ),
            (
                "hf_quant_config.json",
                br#"{"quantization":{"quant_algo":"FP8"}}"#.to_vec(),
            ),
            ("model.safetensors", plain_header()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;

    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1);
    assert_eq!(
        rows.rows[0].precision.as_ref().map(Precision::as_str),
        Some("fp8"),
        "the sidecar is read and its scheme reaches the row"
    );
    assert_eq!(
        rows.rows[0].role,
        Role::Llm,
        "and the model config beside it still named the architecture"
    );
    Ok(())
}
