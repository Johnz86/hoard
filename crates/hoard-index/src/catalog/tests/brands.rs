use super::*;

fn brand_fixture(populations: &[(&str, usize)]) -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    let mut entries = Vec::new();
    let mut identity = 0x9000_u128;
    for (base, held) in populations {
        for copy in 0..*held {
            identity += 1;
            let name = format!("{base}-{copy}");
            entries.push(seeded_entry(
                &format!("brands/{name}.safetensors"),
                1_024,
                Some(seeded_artifact(
                    identity,
                    &name,
                    Some("acme"),
                    Artifact {
                        base: Base::new(base),
                        base_label: Some((*base).to_owned()),
                        ..seeded_facts()
                    },
                )),
            ));
        }
    }
    catalog.seed(&[seeded_revision(
        seeded_repo("acme/brands", RepoType::Models),
        0x51,
        entries,
    )])?;
    catalog.rebuild_search(|_| {})?;
    Ok((directory, catalog))
}

fn base_row<'a>(page: &'a QueryPage, value: &str) -> &'a crate::row::FacetBucket {
    page.facets
        .base
        .iter()
        .find(|bucket| bucket.value == value)
        .expect("the rail holds the row")
}

fn base_values(page: &QueryPage) -> Vec<&str> {
    page.facets
        .base
        .iter()
        .map(|bucket| bucket.value.as_str())
        .collect()
}

fn members(page: &QueryPage, value: &str) -> Vec<String> {
    base_row(page, value).members.clone()
}

#[test]
fn a_brand_row_stands_alone_and_its_members_are_not_rows() -> Result<()> {
    let (_directory, catalog) =
        brand_fixture(&[("qwen3", 3), ("qwen35moe", 2), ("qwenimage2512", 4)])?;
    let page = search(&catalog, "")?;
    assert_eq!(
        base_values(&page),
        vec!["qwen", "qwenimage2512"],
        "a brand replaces the rows it claims rather than heading them"
    );
    assert_eq!(members(&page, "qwen"), vec!["qwen3", "qwen35moe"]);
    assert!(
        base_row(&page, "qwenimage2512").members.is_empty(),
        "a row no brand claims stands for itself alone"
    );
    assert_eq!(
        page.facets.base_shown, 3,
        "base_shown counts the exact base terms the rows stand for, not the row count, so \
         the cap note never claims a consolidated variant is hidden"
    );
    Ok(())
}

#[test]
fn a_brand_row_unions_its_members_and_never_prefixes_them() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[
        ("qwen3", 3),
        ("qwen35moe", 2),
        ("qwenimage2512", 4),
        ("gemma3", 1),
    ])?;
    let page = search(&catalog, "")?;
    let qwen = base_row(&page, "qwen");
    assert_eq!(
        qwen.label.as_deref(),
        Some("qwen"),
        "a brand row labels itself with the key its members spell, not a second vocabulary"
    );
    assert_eq!(
        qwen.count, 5,
        "the row counts its two members and never the unrelated qwen-image family"
    );
    assert_eq!(
        members(&page, "qwen"),
        vec!["qwen3", "qwen35moe"],
        "the members are the rail's own, in the order the rail listed them"
    );
    assert_eq!(
        search(&catalog, "base:qwen3,qwen35moe")?.total,
        qwen.count,
        "the query a brand row opens counts what the brand row says"
    );
    assert_eq!(base_row(&page, "qwenimage2512").count, 4);
    Ok(())
}

#[test]
fn a_lineage_query_answers_only_that_lineage() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[
        ("sdxl", 3),
        ("sdxlpony", 1),
        ("sdxlillustrious", 1),
        ("flux", 1),
    ])?;
    assert_eq!(
        search(&catalog, "base:sdxl")?.total,
        5,
        "the architecture answers for every lineage stored under it"
    );
    assert_eq!(
        search(&catalog, "base:sdxlpony")?.total,
        1,
        "a lineage answers for itself and never for the bare architecture"
    );
    assert_eq!(search(&catalog, "base:sdxlillustrious")?.total, 1);
    assert_eq!(search(&catalog, "base:flux")?.total, 1);
    Ok(())
}

#[test]
fn a_brand_row_counts_each_artifact_once() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("qwen3", 1), ("qwen35", 1)])?;
    let page = search(&catalog, "")?;
    let separate = search(&catalog, "base:qwen3")?.total + search(&catalog, "base:qwen35")?.total;
    assert_eq!(
        separate, 3,
        "the wider query answers the narrower family too, so the two counts overlap on it"
    );
    assert_eq!(
        base_row(&page, "qwen").count,
        2,
        "the union counts an artifact its members share once, not once per member"
    );
    Ok(())
}

#[test]
fn a_brand_with_one_member_is_still_a_brand_row() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("gemma3", 2)])?;
    let page = search(&catalog, "")?;
    assert_eq!(
        base_values(&page),
        vec!["gemma"],
        "the rail's vocabulary is the brand's, however many variants happen to be held"
    );
    let gemma = base_row(&page, "gemma");
    assert_eq!(gemma.label.as_deref(), Some("gemma"));
    assert_eq!(gemma.count, 2);
    assert_eq!(members(&page, "gemma"), vec!["gemma3"]);
    Ok(())
}

#[test]
fn a_brand_row_is_active_only_when_its_whole_union_is_selected() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[
        ("qwen35moe", 2),
        ("qwen3vl", 2),
        ("qwenimage2512", 3),
        ("gemma3", 2),
        ("gemma4", 2),
    ])?;
    let one = search(&catalog, "base:qwen35moe")?;
    assert!(
        !base_row(&one, "qwen").active,
        "one member is not the whole union a click on the row issues"
    );
    assert!(
        !base_row(&one, "gemma").active,
        "a brand the query never names is untouched"
    );
    let whole = search(&catalog, "base:qwen35moe,qwen3vl")?;
    assert!(
        base_row(&whole, "qwen").active,
        "the query the brand row itself opens marks it"
    );
    let unrelated = search(&catalog, "base:qwenimage2512")?;
    let qwen = base_row(&unrelated, "qwen");
    assert!(
        !qwen.active,
        "qwen-image is neither counted by the brand nor marks it"
    );
    assert!(base_row(&unrelated, "qwenimage2512").active);
    assert_eq!(
        qwen.count, 4,
        "the brand still counts its own two members, over the set the base key does not narrow"
    );
    assert_eq!(
        base_values(&search(&catalog, "")?),
        vec!["gemma", "qwen", "qwenimage2512"],
        "a brand row is re-sorted by its own union count, not left at its first member's rank"
    );
    Ok(())
}

#[test]
fn a_base_no_brand_claims_is_its_own_row() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("sdxl", 2), ("qwenimage2512", 1)])?;
    let page = search(&catalog, "")?;
    for value in ["sdxl", "qwenimage2512"] {
        let row = base_row(&page, value);
        assert!(
            row.members.is_empty(),
            "{value} names no brand anchor and stands for itself"
        );
        assert_eq!(row.label.as_deref(), Some(value));
    }
    Ok(())
}
