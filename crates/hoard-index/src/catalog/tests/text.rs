use super::*;

const OVERLONG_WORD: &str = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrs";

fn named_fixture() -> Result<(TempDir, Catalog)> {
    scanned_names(&[
        "gpt-oss-style_v2.safetensors",
        "Qwen3-VL-8B-Instruct-Q4_K_M.safetensors",
        "loxsSoulsMixPerfectDoll_v30.safetensors",
    ])
}

fn named_and_cased_fixture() -> Result<(TempDir, Catalog)> {
    scanned_names(&[
        "gpt-oss-style_v2.safetensors",
        "Qwen3-VL-8B-Instruct-Q4_K_M.safetensors",
        "loxsSoulsMixPerfectDoll_v30.safetensors",
        "cyberrealisticPony_v160.safetensors",
        "oneObsession_v24.safetensors",
    ])
}

fn scanned_config(root: &Path, names: &[&str]) -> Result<IndexConfig> {
    let input = root.join("files");
    fs::create_dir_all(input.join("loras"))?;
    for name in names {
        fs::write(input.join("loras").join(name), lora_header())?;
    }
    Ok(IndexConfig {
        data_dir: root.to_path_buf(),
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    })
}

fn scanned_into(config: &IndexConfig) -> Result<Catalog> {
    let catalog = Catalog::scan_sources(
        &config.native_path(),
        "n",
        config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

fn scanned_names(names: &[&str]) -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let config = scanned_config(directory.path(), names)?;
    let catalog = scanned_into(&config)?;
    Ok((directory, catalog))
}

fn totals(catalog: &Catalog, expected: &[(&str, u64)]) -> Result<()> {
    for (raw, total) in expected {
        assert_eq!(search(catalog, raw)?.total, *total, "{raw}");
    }
    Ok(())
}

#[test]
fn a_pasted_name_finds_its_file_across_hyphens_underscores_and_dots() -> Result<()> {
    let (_directory, catalog) = named_fixture()?;
    totals(
        &catalog,
        &[
            ("gpt-oss", 1),
            ("Q4_K_M", 1),
            ("Qwen3-VL-8B", 1),
            ("loxsSoulsMixPerfectDoll_v30", 1),
            ("gpt-oss-style_v2.safetensors", 1),
        ],
    )
}

#[test]
fn the_start_of_a_word_finds_it_without_a_tilde() -> Result<()> {
    let (_directory, catalog) = named_fixture()?;
    totals(&catalog, &[("loxsSoulsMix", 1), ("qwen", 1), ("gpt-o", 1)])
}

#[test]
fn a_phrase_keeps_its_order_and_separate_words_do_not() -> Result<()> {
    let (_directory, catalog) = named_fixture()?;
    totals(
        &catalog,
        &[("oss-gpt", 0), ("oss gpt", 1), ("style gpt", 1)],
    )
}

#[test]
fn a_tilde_matches_every_word_as_a_prefix_in_any_order() -> Result<()> {
    let (_directory, catalog) = named_fixture()?;
    totals(
        &catalog,
        &[("~oss-gp", 1), ("~instr-qwen", 1), ("instr-qwen", 0)],
    )
}

#[test]
fn a_word_too_long_for_the_index_leaves_a_gap_the_phrase_steps_over() -> Result<()> {
    assert_eq!(OVERLONG_WORD.len(), 45);
    let (_directory, catalog) = scanned_names(&[&format!("gpt-{OVERLONG_WORD}-oss.safetensors")])?;
    totals(&catalog, &[(&format!("gpt-{OVERLONG_WORD}-oss"), 1)])
}

#[test]
fn punctuation_alone_matches_nothing_and_a_missing_word_still_excludes() -> Result<()> {
    let (_directory, catalog) = named_fixture()?;
    totals(&catalog, &[("...", 0), ("gpt-oss nothere", 0)])
}

#[test]
fn a_word_inside_a_camel_cased_name_is_found() -> Result<()> {
    let (_directory, catalog) = named_and_cased_fixture()?;
    totals(
        &catalog,
        &[
            ("pony", 1),
            ("Pony", 1),
            ("souls", 1),
            ("obsession", 1),
            ("doll", 1),
            ("ponytail", 0),
            ("8B", 1),
            ("120", 0),
            ("Qwen3-VL-8B", 1),
            ("Q4_K_M", 1),
            ("loxsSoulsMix", 1),
        ],
    )
}

#[test]
fn the_query_is_cut_by_the_index_it_runs_against() -> Result<()> {
    let (_directory, catalog) = named_and_cased_fixture()?;
    totals(&catalog, &[("vl-8b", 1), ("vl8b", 1)])
}

#[test]
fn an_index_opened_from_disk_registers_the_word_tokenizer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = scanned_config(directory.path(), &["cyberrealisticPony_v160.safetensors"])?;
    let catalog = scanned_into(&config)?;
    drop(catalog);

    let reopened = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    totals(&reopened, &[("pony", 1), ("cyberrealistic", 1)])
}

#[test]
fn a_name_is_still_matched_from_its_start() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = scanned_config(
        directory.path(),
        &[
            "gpt-oss-style_v2.safetensors",
            "Qwen3-VL-8B-Instruct-Q4_K_M.safetensors",
            "loxsSoulsMixPerfectDoll_v30.safetensors",
        ],
    )?;
    let notes = directory.path().join("files/notes");
    fs::create_dir_all(&notes)?;
    fs::write(notes.join("noted.safetensors"), lora_header())?;
    fs::write(
        notes.join("README.md"),
        "If you want to run this, see the docs.\n",
    )?;
    let catalog = scanned_into(&config)?;

    totals(
        &catalog,
        &[
            ("gpt-oss", 1),
            ("souls", 1),
            ("Qwen3-VL-8B", 1),
            ("q4", 1),
            ("wan", 0),
            ("want", 1),
            ("docs", 1),
            ("doc", 0),
        ],
    )
}
