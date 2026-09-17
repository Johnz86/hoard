use super::*;

#[test]
fn filtering_ranges_boolean_queries_and_pagination_are_deterministic() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    let names: Vec<&str> = all.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        ["tiny.jsonl", "training.csv"],
        "the default order is name"
    );
    assert_eq!(search(&catalog, "format:csv node:test-node")?.total, 1);
    assert_eq!(search(&catalog, "format:csv,jsonl")?.total, 2);
    assert_eq!(
        search(&catalog, "size:<=20b")?.rows[0].name,
        "tiny.jsonl",
        "a bare size is megabytes, so the byte suffix is what selects the small file"
    );
    assert_eq!(search(&catalog, "size:<=20b")?.total, 1);
    assert_eq!(search(&catalog, "training")?.total, 1);
    let first = paged(&catalog, 1, 0)?;
    let second = paged(&catalog, 1, 1)?;
    assert_eq!(first.total, 2);
    assert_eq!(first.rows, vec![all.rows[0].clone()]);
    assert_eq!(second.rows, vec![all.rows[1].clone()]);
    assert!(paged(&catalog, 0, 0)?.rows.is_empty());
    assert!(paged(&catalog, 50, u32::MAX)?.rows.is_empty());
    assert!(paged(&catalog, 1001, 0).is_err());
    assert_eq!(
        search(&catalog, "unknown_field:csv")?.total,
        0,
        "a key the grammar does not know is free text, not an error"
    );
    Ok(())
}

#[test]
fn duplicate_paths_collapse_after_correlated_entry_filtering() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut entry = transaction
        .scan()
        .primary::<Entry>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture entry".into()))??;
    let artifact_id = entry
        .artifact_id
        .ok_or_else(|| IndexError::Catalog("fixture entry has no artifact".into()))?;
    let mut artifact: ArtifactRecord = transaction
        .get()
        .primary(artifact_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))?;
    artifact.path_count = 2;
    artifact.copies = 2;
    transaction.upsert(artifact)?;
    let mut repository: Repository = transaction
        .get()
        .primary(entry.repo_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture repository".into()))?;
    repository.id = 900;
    repository.key = models::repository_key("peer", &repository.repo);
    repository.source = "peer".to_owned();
    transaction.insert(repository)?;
    entry.id = 900;
    entry.repo_id = 900;
    entry.state = hoard_core::State::Remote;
    entry.preference = 1;
    transaction.insert(entry)?;
    transaction.commit()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    assert_eq!(all.total, 2);
    let shared = all
        .rows
        .iter()
        .find(|row| row.artifact_id.get() == artifact_id)
        .ok_or_else(|| IndexError::Catalog("missing shared result".into()))?;
    assert_eq!(shared.source.as_deref(), Some("datasets"));
    assert!(shared.dupe);
    assert_eq!(shared.path_count, 2);
    assert_eq!(
        search(&catalog, "source:peer")?.rows[0].source.as_deref(),
        Some("peer")
    );
    assert_eq!(search(&catalog, "source:peer state:local")?.total, 0);
    Ok(())
}

#[test]
fn a_page_is_assembled_from_retained_rows_only() -> Result<()> {
    let (_directory, catalog) = paging_fixture(30)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    for sort in [
        SortSpec::new(SortKey::Name, false),
        SortSpec::new(SortKey::Size, true),
    ] {
        for offset in [0, 7, 13, 25] {
            let request = QueryRequest {
                raw: "",
                sort,
                limit: 6,
                offset,
                context: &context,
            };
            let primary = catalog.query_via(request.clone(), QueryPath::Primary)?;
            let collapsed = catalog.query_via(request, QueryPath::Collapse)?;
            assert_eq!(primary.total, 30, "{sort:?} offset={offset}");
            assert_eq!(primary.total, collapsed.total, "{sort:?} offset={offset}");
            assert_eq!(primary.nodes, 2, "{sort:?} offset={offset}");
            assert_eq!(primary.nodes, collapsed.nodes, "{sort:?} offset={offset}");
            assert_eq!(primary.facets, collapsed.facets, "{sort:?} offset={offset}");
            let paged: Vec<(ArtifactId, EntryId)> = primary
                .rows
                .iter()
                .map(|row| (row.artifact_id, row.entry_id))
                .collect();
            let expected: Vec<(ArtifactId, EntryId)> = collapsed
                .rows
                .iter()
                .map(|row| (row.artifact_id, row.entry_id))
                .collect();
            assert_eq!(paged, expected, "{sort:?} offset={offset}");
            assert_eq!(paged.len(), 6.min(30 - offset as usize));
        }
    }
    Ok(())
}

#[test]
fn a_sampled_page_matches_the_heap_page_everywhere_in_the_order() -> Result<()> {
    let (_directory, catalog) = paging_fixture(60)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let paging = Paging::new(0, 8, 1000);
    for key in PAGED_KEYS {
        for descending in [false, true] {
            for offset in (0..=60).step_by(5) {
                let request = QueryRequest {
                    raw: "",
                    sort: SortSpec::new(key, descending),
                    limit: 7,
                    offset,
                    context: &context,
                };
                let (sampled, _) = catalog.query_paged(request.clone(), paging)?;
                let heaped = catalog.query(request)?;
                let where_ = format!("{key:?} desc={descending} offset={offset}");
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
fn a_bucket_over_the_cap_falls_back_to_the_heap() -> Result<()> {
    let (_directory, catalog) = paging_fixture(60)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let request = QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Size, false),
        limit: 7,
        offset: 25,
        context: &context,
    };
    let (capped, _) = catalog.query_paged(request.clone(), Paging::new(0, 8, 0))?;
    let heaped = catalog.query(request)?;
    assert_eq!(identified(&capped), identified(&heaped));
    assert_eq!(capped.total, heaped.total);
    assert_eq!(capped.facets, heaped.facets);
    Ok(())
}

#[test]
fn a_sampled_page_reads_no_more_than_the_bucket_and_a_page() -> Result<()> {
    let (_directory, catalog) = paging_fixture(60)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let request = QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Size, false),
        limit: 7,
        offset: 30,
        context: &context,
    };
    let (sampled, cut) = catalog.query_paged(request.clone(), Paging::new(0, 16, 1000))?;
    let (heaped, held) = catalog.query_paged(request, Paging::new(usize::MAX, 8, 1000))?;
    assert_eq!(identified(&sampled), identified(&heaped));
    assert_eq!(
        held.kept, 37,
        "the pass that counts is the pass that keeps, so the heap holds the whole front"
    );
    assert_eq!(
        cut.passes, 4,
        "the deep window is counted, sampled, bucketed and cut"
    );
    assert!(
        cut.kept <= 12,
        "a sampled window holds its bucket of five and one page of seven, not {}",
        cut.kept
    );
    Ok(())
}

#[test]
fn a_shallow_window_is_answered_in_one_pass_and_a_deep_one_in_four() -> Result<()> {
    let (_directory, catalog) = paging_fixture(60)?;
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let paging = Paging::new(4, 8, 1000);
    let answered = |offset: u32| -> Result<(QueryPage, Cost)> {
        catalog.query_paged(
            QueryRequest {
                raw: "",
                sort: SortSpec::new(SortKey::Name, false),
                limit: 4,
                offset,
                context: &context,
            },
            paging,
        )
    };
    let passes = |offset: u32| -> Result<usize> { Ok(answered(offset)?.1.passes) };
    assert_eq!(
        passes(0)?,
        1,
        "a window inside the shallow bound is one heap"
    );
    assert_eq!(
        passes(56)?,
        2,
        "the last page is the count and a heap of the far end"
    );
    assert_eq!(
        passes(28)?,
        4,
        "the middle is the count, the sampling pass, the buckets and the window"
    );
    for offset in [60, 200] {
        let (page, cost) = answered(offset)?;
        assert_eq!(
            cost.passes, 2,
            "an offset past the total is the count and the pass that tallies the facets"
        );
        assert_eq!(cost.kept, 0, "and nothing is kept");
        assert!(page.rows.is_empty(), "offset {offset} is past the total");
        assert_eq!(page.total, 60, "the count still counted the match");
    }
    Ok(())
}

#[test]
fn every_sort_key_orders_the_page_and_paging_is_stable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    for name in ["c-style", "a-style", "b-style"] {
        fs::write(
            input.join(format!("loras/{name}.safetensors")),
            lora_header(),
        )?;
    }
    fs::write(input.join("data.csv"), "x,y\n1,2\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
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
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let keys = [
        SortKey::Role,
        SortKey::Name,
        SortKey::Base,
        SortKey::Format,
        SortKey::Precision,
        SortKey::Size,
        SortKey::Revision,
        SortKey::Node,
        SortKey::State,
        SortKey::Used,
    ];
    for key in keys {
        for descending in [false, true] {
            let sort = SortSpec::new(key, descending);
            let whole = catalog.query(QueryRequest {
                raw: "",
                sort,
                limit: 10,
                offset: 0,
                context: &context,
            })?;
            assert_eq!(whole.total, 5, "{key:?}");
            let mut walked = Vec::new();
            for offset in [0, 2, 4] {
                let page = catalog.query(QueryRequest {
                    raw: "",
                    sort,
                    limit: 2,
                    offset,
                    context: &context,
                })?;
                walked.extend(page.rows.iter().map(|row| row.artifact_id));
            }
            let expected: Vec<_> = whole.rows.iter().map(|row| row.artifact_id).collect();
            assert_eq!(
                walked, expected,
                "paging by 2 walks the same order as one page for {key:?} desc={descending}"
            );
        }
    }
    let by_name = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Name, false),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    let names: Vec<&str> = by_name.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "a-style.safetensors",
            "b-style.safetensors",
            "c-style.safetensors",
            "data.csv",
            "tiny.jsonl"
        ]
    );
    let by_size = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Size, true),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    let sizes: Vec<u64> = by_size.rows.iter().map(|row| row.size).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "{sizes:?}");
    Ok(())
}

#[test]
fn a_sampled_text_page_crosses_segments_whose_terms_interleave() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header(),
        )?;
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        catalog.refresh_search(|_| {})?;
    }
    fs::write(input.join("loras/aaa-before.safetensors"), lora_header())?;
    fs::write(input.join("loras/hhh-between.safetensors"), lora_header())?;
    fs::write(input.join("loras/zzz-after.safetensors"), plain_header())?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;
    assert!(
        catalog.search_segments()? >= 3,
        "the refreshes leave terms spread over segments"
    );
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let paging = Paging::new(0, 4, 1000);
    for key in [
        SortKey::Name,
        SortKey::Base,
        SortKey::Role,
        SortKey::Precision,
    ] {
        for descending in [false, true] {
            for offset in (0..=8).step_by(2) {
                let request = QueryRequest {
                    raw: "",
                    sort: SortSpec::new(key, descending),
                    limit: 3,
                    offset,
                    context: &context,
                };
                let (sampled, _) = catalog.query_paged(request.clone(), paging)?;
                let heaped = catalog.query(request)?;
                let where_ = format!("{key:?} desc={descending} offset={offset}");
                assert_eq!(sampled.total, 8, "{where_}");
                assert_eq!(identified(&sampled), identified(&heaped), "{where_}");
                assert_eq!(sampled.nodes, heaped.nodes, "{where_}");
                assert_eq!(sampled.facets, heaped.facets, "{where_}");
            }
        }
    }
    Ok(())
}

#[test]
fn a_boundary_absent_from_a_segment_maps_to_the_next_term() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    fs::write(input.join("loras/middle.safetensors"), lora_header())?;
    catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    catalog.refresh_search(|_| {})?;
    assert_eq!(
        catalog.search_segments()?,
        2,
        "the refresh commits the new artifact into a segment of its own"
    );
    let mut held = catalog.sort_bounds(SortKey::Name, "middle.safetensors")?;
    held.sort_unstable();
    assert_eq!(
        held,
        vec![(0, true, 1), (2, false, 2)],
        "the segment holding the term answers exactly, the other answers past its last term"
    );
    let mut above = catalog.sort_bounds(SortKey::Name, "zzz.safetensors")?;
    above.sort_unstable();
    assert_eq!(
        above,
        vec![(1, false, 1), (2, false, 2)],
        "a term above every term maps to the term count of each segment"
    );
    let mut between = catalog.sort_bounds(SortKey::Name, "keep.safetensors")?;
    between.sort_unstable();
    assert_eq!(
        between,
        vec![(0, false, 1), (1, true, 2)],
        "a term the other segment sorts after maps to that segment's lowest ordinal"
    );
    for (ordinal, exact, terms) in catalog.sort_bounds(SortKey::Role, "lora")? {
        assert!(
            exact && ordinal < terms,
            "a role sort reads the facet column both segments already opened"
        );
    }
    for (ordinal, exact, terms) in catalog.sort_bounds(SortKey::Role, "zzzz")? {
        assert_eq!(
            (ordinal, exact),
            (terms, false),
            "a role above every role maps past the last term of each segment"
        );
    }
    Ok(())
}

#[test]
fn a_published_index_reports_how_many_segments_it_answers_from() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    assert!(
        catalog.search_segments().is_err(),
        "a catalog with no published index has no segments to report"
    );
    catalog.rebuild_search(|_| {})?;
    let built = catalog.search_segments()?;
    assert_eq!(
        built, 1,
        "one commit of one writer thread builds one segment"
    );
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header(),
        )?;
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        catalog.refresh_search(|_| {})?;
    }
    assert!(
        catalog.search_segments()? > built,
        "each refresh commits its own segment"
    );
    Ok(())
}

#[test]
fn facets_and_nodes_survive_a_multi_segment_index() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header(),
        )?;
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        catalog.refresh_search(|_| {})?;
    }
    let page = search(&catalog, "")?;
    assert_eq!(page.total, 5);
    assert_eq!(page.nodes, 1);
    let names: Vec<&str> = page.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "extra-0.safetensors",
            "extra-1.safetensors",
            "extra-2.safetensors",
            "gone.safetensors",
            "keep.safetensors"
        ],
        "ordinals of different segments are merged by the term they spell"
    );
    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    let mut walked = Vec::new();
    for offset in [0, 2, 4] {
        let page = catalog.query(QueryRequest {
            raw: "",
            sort: SortSpec::default(),
            limit: 2,
            offset,
            context: &context,
        })?;
        walked.extend(page.rows.iter().map(|row| row.name.clone()));
    }
    assert_eq!(walked, names, "paging is stable across the segments");
    let lora = page
        .facets
        .role
        .iter()
        .find(|bucket| bucket.value == "lora")
        .map(|bucket| bucket.count);
    assert_eq!(
        lora,
        Some(4),
        "counts merge across the segments the refreshes created: {:?}",
        page.facets.role
    );
    Ok(())
}

#[test]
fn tallies_keyed_by_ordinal_merge_across_segments_by_the_term_they_spell() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/round-{round}.safetensors")),
            lora_header_named(&format!("block_{round}")),
        )?;
        catalog.rescan(
            &config,
            HashPolicy::Never,
            RescanScope::Configured,
            |_, _| {},
        )?;
        catalog.refresh_search(|_| {})?;
    }
    assert!(
        catalog.search_segments()? > 1,
        "the refreshes leave the ordinals spread over several segments"
    );
    let page = search(&catalog, "")?;
    assert_eq!(page.total, 5);
    assert_eq!(page.nodes, 1, "every segment sits on the one node");
    let lora = page
        .facets
        .role
        .iter()
        .find(|bucket| bucket.value == "lora")
        .map(|bucket| bucket.count);
    assert_eq!(
        lora,
        Some(4),
        "one ordinal per segment sums into one bucket: {:?}",
        page.facets.role
    );
    let roles: u64 = page.facets.role.iter().map(|bucket| bucket.count).sum();
    assert_eq!(roles, page.total, "every artifact is counted exactly once");
    let formats: Vec<(&str, u64)> = page
        .facets
        .format
        .iter()
        .map(|bucket| (bucket.value.as_str(), bucket.count))
        .collect();
    assert_eq!(
        formats,
        [("safetensors", 5)],
        "a term every segment spells is one bucket, not one per segment"
    );
    assert_eq!(search(&catalog, "role:lora")?.total, 4);
    Ok(())
}

#[test]
fn a_segment_with_deletes_answers_what_a_fresh_index_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (input, config) = loose_fixture(directory.path())?;
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header_named(&format!("short_{round}")),
        )?;
    }
    let native = directory.path().join("native");
    let catalog = Catalog::scan_sources(&native, "n", &config, HashPolicy::Never, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        catalog.search_deleted_documents()?,
        0,
        "a fresh index deletes nothing"
    );
    for round in 0..3 {
        fs::write(
            input.join(format!("loras/extra-{round}.safetensors")),
            lora_header_named(&format!("a_much_longer_block_name_{round}")),
        )?;
    }
    let rescanned = catalog.rescan(
        &config,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;
    assert_eq!(rescanned.changed, 3);
    catalog.refresh_search(|_| {})?;
    assert!(
        catalog.search_deleted_documents()? > 0,
        "a refresh over changed artifacts leaves the old segment carrying deletes"
    );
    let deleted = search(&catalog, "")?;
    let names: Vec<String> = deleted.rows.iter().map(|row| row.name.clone()).collect();
    let lora = search(&catalog, "role:lora")?;

    catalog.rebuild_search(|_| {})?;
    assert_eq!(catalog.search_deleted_documents()?, 0);
    let fresh = search(&catalog, "")?;
    assert_eq!(
        deleted.total, fresh.total,
        "a deleted document is counted by neither"
    );
    assert_eq!(deleted.nodes, fresh.nodes);
    assert_eq!(
        names,
        fresh
            .rows
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<String>>()
    );
    assert_eq!(
        deleted.facets, fresh.facets,
        "the rail counts the living documents alone"
    );
    assert_eq!(lora.total, search(&catalog, "role:lora")?.total);
    Ok(())
}

#[test]
fn a_base_bucket_keeps_its_label_only_when_every_segment_agrees() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let labelled = |name: &str, label: &str, identity: u128| SeedRepository {
        repo: RepoId::new(RepoType::Models, Some("acme".to_owned()), name.to_owned()),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        tool_layout: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: None,
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
            entries: vec![seeded_entry(
                &format!("{name}.safetensors"),
                1_024,
                Some(seeded_artifact(
                    identity,
                    name,
                    Some("acme"),
                    Artifact {
                        base: Base::new("sdxl 1.0"),
                        base_label: Some(label.to_owned()),
                        ..seeded_facts()
                    },
                )),
            )],
        }],
    };
    let bucket_label = |catalog: &Catalog| -> Result<Option<String>> {
        Ok(search(catalog, "")?
            .facets
            .base
            .into_iter()
            .find(|bucket| bucket.value == "sdxl10")
            .and_then(|bucket| bucket.label))
    };
    catalog.seed(&[
        labelled("first", "SDXL 1.0", 0x1),
        labelled("second", "SDXL 1.0", 0x2),
    ])?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        bucket_label(&catalog)?,
        Some("SDXL 1.0".to_owned()),
        "one segment whose artifacts agree carries the label"
    );
    catalog.seed(&[labelled("third", "SDXL Turbo", 0x3)])?;
    catalog.refresh_search(|_| {})?;
    assert!(
        catalog.search_segments()? > 1,
        "the refresh puts the differing label in a segment of its own"
    );
    assert_eq!(
        bucket_label(&catalog)?,
        None,
        "a segment that spells another label takes the label off the bucket"
    );
    catalog.seed(&[labelled("fourth", "SDXL 1.0", 0x4)])?;
    catalog.refresh_search(|_| {})?;
    assert_eq!(
        bucket_label(&catalog)?,
        None,
        "an agreeing segment afterwards does not restore it"
    );
    assert_eq!(search(&catalog, "")?.total, 4);
    Ok(())
}

#[test]
fn an_absent_facet_column_tallies_nothing_and_breaks_nothing() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let page = search(&catalog, "")?;
    assert_eq!(page.total, 2);
    assert_eq!(page.nodes, 1);
    assert!(
        page.facets.precision.is_empty(),
        "a segment with no precision term has no precision bucket: {:?}",
        page.facets.precision
    );
    assert!(
        page.facets.base.is_empty(),
        "a segment with no base term has no base bucket: {:?}",
        page.facets.base
    );
    let mut formats: Vec<(&str, u64)> = page
        .facets
        .format
        .iter()
        .map(|bucket| (bucket.value.as_str(), bucket.count))
        .collect();
    formats.sort_unstable();
    assert_eq!(formats, [("csv", 1), ("jsonl", 1)]);
    let roles: u64 = page.facets.role.iter().map(|bucket| bucket.count).sum();
    assert_eq!(roles, page.total);
    Ok(())
}
