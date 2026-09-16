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

fn family<'a>(page: &'a QueryPage, key: &str) -> Option<&'a crate::row::FacetBucket> {
    page.facets
        .base_families
        .iter()
        .find(|bucket| bucket.value == key)
}

fn base_bucket<'a>(page: &'a QueryPage, value: &str) -> &'a crate::row::FacetBucket {
    page.facets
        .base
        .iter()
        .find(|bucket| bucket.value == value)
        .expect("the rail holds the seeded base")
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
    let qwen = family(&page, "qwen").expect("the qwen family is grouped");
    assert_eq!(qwen.label.as_deref(), Some("QWEN"));
    assert_eq!(
        qwen.count, 5,
        "the family counts its two members and never the unrelated qwen-image family"
    );
    assert_eq!(base_bucket(&page, "qwenimage2512").group, None);
    let members: Vec<&str> = page
        .facets
        .base
        .iter()
        .filter(|bucket| bucket.group.as_deref() == Some("qwen"))
        .map(|bucket| bucket.value.as_str())
        .collect();
    assert_eq!(
        members,
        vec!["qwen3", "qwen35moe"],
        "the grouped members are the rail's own, in the order the rail lists them"
    );
    assert_eq!(
        search(&catalog, "base:qwen3,qwen35moe")?.total,
        qwen.count,
        "the query a family row opens counts what the family row says"
    );
    Ok(())
}

#[test]
fn a_family_counts_each_artifact_once() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("qwen3", 1), ("qwen35", 1)])?;
    let page = search(&catalog, "")?;
    let qwen = family(&page, "qwen").expect("the qwen family is grouped");
    let members = base_bucket(&page, "qwen3").count + base_bucket(&page, "qwen35").count;
    assert_eq!(
        members, 4,
        "the prefix rule counts each artifact in both buckets, so the two counts overlap"
    );
    assert_eq!(
        qwen.count, 2,
        "the union counts an artifact its members share once, not once per member"
    );
    Ok(())
}

#[test]
fn a_brand_with_one_surviving_member_is_not_grouped() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("gemma3", 2), ("lfm2", 1)])?;
    let page = search(&catalog, "")?;
    assert!(
        page.facets.base_families.is_empty(),
        "a brand holding one bucket is the flat row it always was"
    );
    assert_eq!(base_bucket(&page, "gemma3").group, None);
    assert_eq!(base_bucket(&page, "lfm2").group, None);
    Ok(())
}

#[test]
fn a_family_row_is_active_when_the_query_selects_it() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[("gemma3", 2), ("gemma4", 2)])?;
    let page = search(&catalog, "base:gemma3,gemma4")?;
    let gemma = family(&page, "gemma").expect("the gemma family is grouped");
    assert!(gemma.active, "the family row the query selects is marked");
    assert_eq!(
        gemma.count, 4,
        "the family is counted over the set the base key does not narrow"
    );
    Ok(())
}

#[test]
fn a_query_holding_one_member_leaves_the_family_row_unmarked() -> Result<()> {
    let (_directory, catalog) = brand_fixture(&[
        ("qwen35moe", 2),
        ("qwen3vl", 2),
        ("qwenimage2512", 3),
        ("gemma3", 2),
        ("gemma4", 2),
    ])?;
    let one = search(&catalog, "base:qwen35moe")?;
    assert!(
        !family(&one, "qwen")
            .expect("the qwen family is grouped")
            .active,
        "the header a click on one member reaches is not already selected"
    );
    assert!(base_bucket(&one, "qwen35moe").active);
    assert!(!base_bucket(&one, "qwen3vl").active);
    assert!(
        !family(&one, "gemma")
            .expect("the gemma family is grouped")
            .active,
        "a family the query never names is untouched"
    );
    let whole = search(&catalog, "base:qwen35moe,qwen3vl")?;
    assert!(
        family(&whole, "qwen")
            .expect("the qwen family is grouped")
            .active,
        "the query the family row itself opens marks it"
    );
    Ok(())
}

#[test]
fn a_query_for_an_unrelated_product_never_marks_the_brand_it_shares_a_prefix_with() -> Result<()> {
    let (_directory, catalog) =
        brand_fixture(&[("qwen35moe", 2), ("qwen3vl", 2), ("qwenimage2512", 3)])?;
    let page = search(&catalog, "base:qwenimage2512")?;
    let qwen = family(&page, "qwen").expect("the qwen family is grouped");
    assert!(
        !qwen.active,
        "qwen-image is neither counted by the qwen family nor marks it"
    );
    assert!(base_bucket(&page, "qwenimage2512").active);
    assert_eq!(
        qwen.count, 4,
        "the family still counts its own two members, over the set the base key does not narrow"
    );
    Ok(())
}
