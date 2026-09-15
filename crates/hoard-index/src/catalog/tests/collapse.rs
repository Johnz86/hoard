use super::*;

/// Where the node rail sits in the five the facets carry, which is the one dimension a
/// query in these cases names its own key in.
const NODE_RAIL: usize = 4;

#[test]
fn the_primary_path_answers_exactly_what_the_collapse_path_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Sharded",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[("unet/adapter.safetensors", lora_header())],
    )?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler-copy",
        &[("unet/adapter.safetensors", lora_header())],
    )?;
    hf_cache_repo(
        &hub,
        "data",
        "rows",
        &[("train.csv", b"x,y\n1,2\n".to_vec())],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let queries = [
        "",
        "role:lora",
        "fmt:csv,safetensors",
        "base:qwen",
        "dupe:true",
        "size:>100b",
        "-role:ds",
        "adapter",
    ];
    let keys = [
        SortKey::Name,
        SortKey::Size,
        SortKey::Role,
        SortKey::Base,
        SortKey::Format,
        SortKey::Precision,
        SortKey::Used,
    ];
    for raw in queries {
        for key in keys {
            for descending in [false, true] {
                for (limit, offset) in [(10, 0), (2, 0), (2, 2), (1, 3)] {
                    let request = QueryRequest {
                        raw,
                        sort: SortSpec::new(key, descending),
                        limit,
                        offset,
                        context: &context,
                    };
                    let fast = catalog.query_via(request.clone(), QueryPath::Primary)?;
                    let slow = catalog.query_via(request, QueryPath::Collapse)?;
                    assert_eq!(fast.total, slow.total, "{raw} {key:?} desc={descending}");
                    assert_eq!(fast.nodes, slow.nodes, "{raw}");
                    assert_eq!(fast.facets, slow.facets, "{raw} {key:?}");
                    let fast_ids: Vec<_> = fast
                        .rows
                        .iter()
                        .map(|row| (row.artifact_id, row.entry_id))
                        .collect();
                    let slow_ids: Vec<_> = slow
                        .rows
                        .iter()
                        .map(|row| (row.artifact_id, row.entry_id))
                        .collect();
                    assert_eq!(
                        fast_ids, slow_ids,
                        "{raw} {key:?} desc={descending} limit={limit} offset={offset}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn the_collapse_path_keeps_one_row_per_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    for name in ["styler", "styler-copy", "styler-third"] {
        hf_cache_repo(
            &hub,
            "acme",
            name,
            &[("unet/adapter.safetensors", lora_header())],
        )?;
    }
    hf_cache_repo(
        &hub,
        "data",
        "rows",
        &[("train.csv", b"x,y\n1,2\n".to_vec())],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    for raw in ["node:self", "state:local"] {
        let request = QueryRequest {
            raw,
            sort: SortSpec::default(),
            limit: 50,
            offset: 0,
            context: &context,
        };
        let page = catalog.query_via(request, QueryPath::Collapse)?;
        let mut seen: Vec<i64> = page.rows.iter().map(|row| row.artifact_id.get()).collect();
        let listed = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), listed, "{raw} listed an artifact twice");
        assert_eq!(page.total, listed as u64, "{raw} counted what it listed");
        assert!(page.total > 0, "{raw} matched nothing");
    }
    let both = QueryRequest {
        raw: "role:lora",
        sort: SortSpec::default(),
        limit: 50,
        offset: 0,
        context: &context,
    };
    let primary = catalog.query_via(both.clone(), QueryPath::Primary)?;
    let collapsed = catalog.query_via(both, QueryPath::Collapse)?;
    assert_eq!(primary.facets, collapsed.facets);
    assert_eq!(primary.total, collapsed.total);
    assert_eq!(primary.nodes, collapsed.nodes);
    Ok(())
}

/// A seeded catalog whose artifacts own runs of entry documents: the base repository holds
/// two revisions and every fifth artifact carries a second entry on the peer node, and three
/// later repositories are refreshed in rather than rebuilt, so the runs are spread over
/// several segments.
fn collapse_fixture() -> Result<(TempDir, Catalog)> {
    let (directory, catalog) = paging_fixture(24)?;
    for round in 0..3_u8 {
        let size = 2_048 + u64::from(round);
        let artifact = seeded_artifact(
            0x9000 + u128::from(round),
            &format!("late-{round}"),
            Some("acme"),
            Artifact {
                size,
                role: Role::Lora,
                base: Base::new("flux"),
                format: ArtifactFormat::Safetensors,
                precision: Some(Precision::new("fp16")),
                ..seeded_facts()
            },
        );
        catalog.seed(&[seeded_revision(
            seeded_repo(&format!("acme/late-{round}"), RepoType::Models),
            0x7a + round,
            vec![
                seeded_entry(
                    &format!("late-{round}/a.safetensors"),
                    size,
                    Some(artifact.clone()),
                ),
                seeded_entry(&format!("late-{round}/b.safetensors"), size, Some(artifact)),
            ],
        )])?;
        catalog.refresh_search(|_| {})?;
    }
    Ok((directory, catalog))
}

/// The entry of each artifact a collapse would keep among the entries a query admits,
/// computed from the records the index was built from rather than from the index.
fn preferred_by(
    catalog: &Catalog,
    admits: &dyn Fn(&EntryDetail) -> bool,
) -> Result<Vec<(ArtifactId, EntryDetail)>> {
    let ranked = |detail: &EntryDetail| {
        (
            preference(detail.state, detail.present),
            Reverse(detail.entry_id.get()),
        )
    };
    let repositories = catalog.repositories(&RepositoryFilter::default(), None, 1_000)?;
    let mut seen: Vec<ArtifactId> = Vec::new();
    let mut preferred: Vec<(ArtifactId, EntryDetail)> = Vec::new();
    for repository in &repositories.rows {
        for row in catalog.repository_rows(repository.id, 1_000)? {
            if seen.contains(&row.artifact_id) {
                continue;
            }
            seen.push(row.artifact_id);
            let mut best: Option<EntryDetail> = None;
            for entry in catalog.entries_of(row.artifact_id)? {
                if !admits(&entry) {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|held| ranked(&entry) > ranked(held))
                {
                    best = Some(entry);
                }
            }
            if let Some(best) = best {
                preferred.push((row.artifact_id, best));
            }
        }
    }
    Ok(preferred)
}

/// One query the collapse path takes: the entries it admits, and the rail it names its own
/// key in, which the facets count over the set that key does not narrow.
struct CollapseCase {
    raw: String,
    admits: Box<dyn Fn(&EntryDetail) -> bool>,
    own_rail: Option<usize>,
}

/// The five facet rails the preferred entries spell, as the counts the rail would show.
fn expected_facets(
    catalog: &Catalog,
    preferred: &[(ArtifactId, EntryDetail)],
) -> Result<[BTreeMap<String, u64>; 5]> {
    let mut counted: [BTreeMap<String, u64>; 5] = std::array::from_fn(|_| BTreeMap::new());
    for (artifact_id, entry) in preferred {
        let row = catalog.artifact_row(*artifact_id)?.ok_or_else(|| {
            IndexError::Catalog(format!("the fixture lost artifact {artifact_id:?}"))
        })?;
        let values = [
            row.role.as_str().to_owned(),
            row.base.as_str().to_owned(),
            row.format.as_str().to_owned(),
            row.precision
                .as_ref()
                .map_or(String::new(), |precision| precision.as_str().to_owned()),
            entry.node.clone(),
        ];
        for (slot, value) in counted.iter_mut().zip(values) {
            if value.is_empty() {
                continue;
            }
            *slot.entry(value).or_default() += 1;
        }
    }
    Ok(counted)
}

fn counted_rail(buckets: &[crate::row::FacetBucket]) -> BTreeMap<String, u64> {
    buckets
        .iter()
        .map(|bucket| (bucket.value.clone(), bucket.count))
        .collect()
}

#[test]
fn the_collapse_path_answers_the_same_page_facets_and_totals_as_before() -> Result<()> {
    let (_directory, catalog) = collapse_fixture()?;
    assert!(
        catalog.search_segments()? > 1,
        "the refreshes leave the entry documents spread over several segments"
    );
    let base = catalog
        .repositories(&RepositoryFilter::default(), None, 1_000)?
        .rows
        .iter()
        .map(|repository| repository.repo.qualified())
        .find(|qualified| qualified.ends_with("/paging"))
        .ok_or_else(|| IndexError::Catalog("the fixture lost acme/paging".into()))?;
    let by_repo = base.clone();
    let cases = [
        CollapseCase {
            raw: "node:self".to_owned(),
            admits: Box::new(|entry: &EntryDetail| entry.node == "workstation"),
            own_rail: Some(NODE_RAIL),
        },
        CollapseCase {
            raw: "state:local".to_owned(),
            admits: Box::new(|entry: &EntryDetail| entry.state == State::Local),
            own_rail: None,
        },
        CollapseCase {
            raw: format!("repo:{base}"),
            admits: Box::new(move |entry: &EntryDetail| entry.repo == by_repo),
            own_rail: None,
        },
    ];
    let unnarrowed = expected_facets(&catalog, &preferred_by(&catalog, &|_| true)?)?;
    for CollapseCase {
        raw,
        admits,
        own_rail,
    } in &cases
    {
        let preferred = preferred_by(&catalog, admits.as_ref())?;
        assert!(!preferred.is_empty(), "{raw} admits no entry at all");
        let mut nodes: Vec<&str> = preferred
            .iter()
            .map(|(_, entry)| entry.node.as_str())
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        let mut rails = expected_facets(&catalog, &preferred)?;
        if let Some(dimension) = own_rail {
            rails[*dimension] = unnarrowed[*dimension].clone();
        }
        let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
        for descending in [false, true] {
            for offset in (0..12).step_by(3) {
                let request = QueryRequest {
                    raw,
                    sort: SortSpec::new(SortKey::Revision, descending),
                    limit: 4,
                    offset,
                    context: &context,
                };
                let page = catalog.query(request.clone())?;
                let (sampled, _) = catalog.query_paged(request, Paging::new(0, 8, 1_000))?;
                let where_ = format!("{raw} desc={descending} offset={offset}");
                assert_eq!(identified(&page), identified(&sampled), "{where_}");
                assert_eq!(sampled.total, page.total, "{where_}");
                assert_eq!(sampled.facets, page.facets, "{where_}");
                assert_eq!(page.total, preferred.len() as u64, "{where_}");
                assert_eq!(page.nodes, nodes.len() as u64, "{where_}");
                let shown = [
                    counted_rail(&page.facets.role),
                    counted_rail(&page.facets.base),
                    counted_rail(&page.facets.format),
                    counted_rail(&page.facets.precision),
                    counted_rail(&page.facets.node),
                ];
                assert_eq!(shown, rails, "{where_}");
                let listed: Vec<ArtifactId> = page.rows.iter().map(|row| row.artifact_id).collect();
                let mut deduped = listed.clone();
                deduped.sort_unstable();
                deduped.dedup();
                assert_eq!(deduped.len(), listed.len(), "{where_} listed one twice");
                for row in &page.rows {
                    let expected = preferred
                        .iter()
                        .find(|(id, _)| *id == row.artifact_id)
                        .map(|(_, entry)| entry.entry_id);
                    assert_eq!(Some(row.entry_id), expected, "{where_}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn the_collapsed_count_agrees_with_the_collection_that_tallies() -> Result<()> {
    let (_directory, catalog) = collapse_fixture()?;
    assert!(
        catalog.search_segments()? > 1,
        "the refreshes leave the runs spread over several segments"
    );
    for raw in [
        "node:self",
        "state:local",
        "rev:main",
        "state:local role:lora",
        "state:local size:>1kb",
        "state:local base:flux",
    ] {
        let context = catalog.compile_context(raw, jiff::Timestamp::now(), Vec::new())?;
        let (folded, tallied) = catalog.collapsed_counts(raw, &context)?;
        assert_eq!(folded, tallied, "{raw}");
        assert!(folded > 0, "{raw} matched nothing, so it proves nothing");
    }
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let (folded, tallied) = catalog.collapsed_counts("state:pulling", &context)?;
    assert_eq!(
        (folded, tallied),
        (0, 0),
        "a query matching nothing counts 0"
    );
    Ok(())
}

#[test]
fn a_sampled_collapse_page_matches_the_heap_page_everywhere_in_the_order() -> Result<()> {
    let (_directory, catalog) = paging_fixture(60)?;
    let context = catalog.compile_context("state:local", jiff::Timestamp::now(), Vec::new())?;
    let paging = Paging::new(0, 8, 1000);
    for key in PAGED_KEYS {
        for descending in [false, true] {
            for offset in (0..=60).step_by(5) {
                let request = QueryRequest {
                    raw: "state:local",
                    sort: SortSpec::new(key, descending),
                    limit: 7,
                    offset,
                    context: &context,
                };
                let (sampled, _) = catalog.query_paged(request.clone(), paging)?;
                let heaped = catalog.query(request)?;
                let where_ = format!("{key:?} desc={descending} offset={offset}");
                assert_eq!(heaped.total, 60, "{where_}");
                assert_eq!(identified(&sampled), identified(&heaped), "{where_}");
                assert_eq!(sampled.total, heaped.total, "{where_}");
                assert_eq!(sampled.nodes, heaped.nodes, "{where_}");
                assert_eq!(sampled.facets, heaped.facets, "{where_}");
            }
        }
    }
    Ok(())
}

#[test]
fn a_run_straddling_a_block_boundary_collapses_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let wide = seeded_artifact(
        0x7000,
        "wide",
        Some("acme"),
        Artifact {
            size: 4_096,
            role: Role::Lora,
            base: Base::new("sdxl"),
            format: ArtifactFormat::Safetensors,
            ..seeded_facts()
        },
    );
    let narrow = seeded_artifact(
        0x7001,
        "narrow",
        Some("acme"),
        Artifact {
            size: 2_048,
            role: Role::Vae,
            base: Base::new("flux"),
            format: ArtifactFormat::Safetensors,
            ..seeded_facts()
        },
    );
    let mut entries: Vec<SeedEntry> = (0..70)
        .map(|index| {
            seeded_entry(
                &format!("wide/copy-{index:03}.safetensors"),
                4_096,
                Some(wide.clone()),
            )
        })
        .collect();
    entries.push(seeded_entry("narrow/one.safetensors", 2_048, Some(narrow)));
    catalog.seed(&[seeded_revision(
        seeded_repo("acme/wide", RepoType::Models),
        0x31,
        entries,
    )])?;
    catalog.rebuild_search(|_| {})?;

    let page = search(&catalog, "state:local")?;
    assert_eq!(
        page.total, 2,
        "a run of 70 entry documents crossing the block is one artifact"
    );
    assert_eq!(page.rows.len(), 2);
    let roles: u64 = page.facets.role.iter().map(|bucket| bucket.count).sum();
    assert_eq!(roles, 2, "the run is tallied once: {:?}", page.facets.role);
    let shown = page
        .rows
        .iter()
        .find(|row| row.name == "wide")
        .ok_or_else(|| IndexError::Catalog("the wide artifact is not on the page".into()))?;
    let lowest = catalog
        .entries_of(shown.artifact_id)?
        .into_iter()
        .map(|entry| entry.entry_id)
        .min()
        .ok_or_else(|| IndexError::Catalog("the wide artifact lost its entries".into()))?;
    assert_eq!(
        shown.entry_id, lowest,
        "the run collapses to the entry the page would show"
    );
    Ok(())
}

#[test]
fn entry_level_tokens_and_sorts_take_the_collapse_path() -> Result<()> {
    let context = CompileContext::new(jiff::Timestamp::now()).with_shelves(vec![
        ("here".into(), "node:self".into()),
        ("loras".into(), "role:lora".into()),
    ]);
    let collapse = [
        "node:self",
        "state:local",
        "source:hub",
        "repo:acme/styler",
        "rev:main",
        "hash:abcd",
        "shelf:here",
        "role:lora node:self",
    ];
    let primary = [
        "",
        "role:lora",
        "base:sdxl",
        "fmt:gguf",
        "prec:fp16",
        "dupe:true",
        "size:<1gb",
        "used:>30d",
        "trigger:grain",
        "org:acme",
        "film grain",
        "shelf:loras",
        "-role:ds",
    ];
    for raw in collapse {
        assert!(
            query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?,
            "{raw}"
        );
    }
    for raw in primary {
        assert!(
            !query::needs_collapse(&parse(raw).tokens, &context, SortKey::Name)?,
            "{raw}"
        );
    }
    for key in [SortKey::Revision, SortKey::Node, SortKey::State] {
        assert!(
            query::needs_collapse(&parse("").tokens, &context, key)?,
            "{key:?}"
        );
    }
    Ok(())
}
