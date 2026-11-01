use super::*;

fn resource_world() -> (TempDir, AppState) {
    let directory = tempfile::tempdir().expect("temporary catalog");
    let catalog = crate::testing::empty_catalog(directory.path(), "workstation");
    let artifacts = [("alpha", 60), ("charlie", 20), ("echo", 40)];
    let projects = [("Bravo", 50), ("Delta", 10), ("Foxtrot", 30)];
    let mut repositories = Vec::new();
    for (position, (name, size)) in artifacts.into_iter().enumerate() {
        repositories.push(seed_repo(
            name,
            RepoType::Models,
            position as u8 + 1,
            vec![seed_entry(
                "weights.gguf",
                "/mock/weights.gguf",
                size,
                Some(seed_artifact(
                    position as u128 + 1,
                    name,
                    facts(size, ArtifactFormat::Gguf, Role::Llm, 1.0),
                )),
            )],
        ));
    }
    for (position, (name, size)) in projects.into_iter().enumerate() {
        let mut project = seed_repo(
            name,
            RepoType::Models,
            position as u8 + 10,
            vec![seed_entry("main.rs", "/mock/main.rs", size, None)],
        );
        project.source = "projects".to_owned();
        project.kind = SourceKind::Git;
        project.git_dir = Some(directory.path().join(format!("missing-{name}.git")));
        repositories.push(project);
    }
    catalog.seed(&repositories).expect("resources seed");
    catalog
        .rebuild_search(|_| {})
        .expect("artifact index builds");
    let mut config = HoardConfig::default();
    config.server.data_dir = directory.path().join("server");
    config.index.data_dir = directory.path().join("index");
    config.mirror.cache_root = Some(directory.path().join("mirror"));
    config.mirror.offline = true;
    let state = AppState::new(config, catalog).expect("application builds");
    (directory, state)
}

fn resource_names(response: &Value) -> Vec<&str> {
    response["resources"]
        .as_array()
        .expect("resource rows")
        .iter()
        .map(|row| row["name"].as_str().expect("resource name"))
        .collect()
}

#[tokio::test]
async fn saved_shelf_counts_match_the_resources_its_query_displays() {
    let (_directory, state) = resource_world();
    state
        .catalog()
        .create_shelf("all resources", "", 100)
        .expect("shelf writes");
    state
        .catalog()
        .create_shelf("Git projects", "role:git", 101)
        .expect("shelf writes");
    let shelves = get(&state, "/api/v1/shelves").await;
    let rows = shelves.as_array().expect("shelf list");
    assert_eq!(
        rows.iter()
            .find(|row| row["name"] == "all resources")
            .expect("all shelf")["count"],
        6
    );
    assert_eq!(
        rows.iter()
            .find(|row| row["name"] == "Git projects")
            .expect("Git shelf")["count"],
        3
    );
}

#[tokio::test]
async fn mixed_resources_share_name_order_without_changing_artifact_results() {
    let (_directory, state) = resource_world();
    let response = get(&state, "/api/v1/search?limit=100").await;
    assert_eq!(
        resource_names(&response),
        ["alpha", "Bravo", "charlie", "Delta", "echo", "Foxtrot"]
    );
    assert_eq!(response["total"], 3);
    assert_eq!(response["resource_total"], 6);
    assert_eq!(response["rows"].as_array().expect("artifact rows").len(), 3);
    assert_eq!(response["resources"][1]["resource_kind"], "git");
    assert_eq!(response["resources"][1]["artifact_id"], Value::Null);
    assert_eq!(response["resources"][1]["role"], "git");
    assert_eq!(response["resources"][1]["size"], 50);
    assert_eq!(response["resources"][1]["base"], Value::Null);
}

#[tokio::test]
async fn mixed_resources_share_numeric_size_order_in_both_directions() {
    let (_directory, state) = resource_world();
    let ascending = get(&state, "/api/v1/search?sort=size&dir=asc").await;
    assert_eq!(
        resource_names(&ascending),
        ["Delta", "charlie", "Foxtrot", "echo", "Bravo", "alpha"]
    );
    let descending = get(&state, "/api/v1/search?sort=size&dir=desc").await;
    assert_eq!(
        resource_names(&descending),
        ["alpha", "Bravo", "echo", "Foxtrot", "charlie", "Delta"]
    );
}

#[tokio::test]
async fn resource_pagination_neither_repeats_nor_skips_either_kind() {
    let (_directory, state) = resource_world();
    let mut uri = "/api/v1/search?limit=2".to_owned();
    let mut names = Vec::new();
    loop {
        let response = get(&state, &uri).await;
        names.extend(resource_names(&response).into_iter().map(str::to_owned));
        match response["resource_next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/search?limit=2&resource_cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(
        names,
        ["alpha", "Bravo", "charlie", "Delta", "echo", "Foxtrot"]
    );
}

#[tokio::test]
async fn project_and_artifact_filters_keep_one_resource_list_and_consistent_facets() {
    let (_directory, state) = resource_world();
    for filter in [
        "role:git",
        "format:git",
        "-role:llm",
        "source:projects",
        "state:stale",
    ] {
        let response = get(&state, &format!("/api/v1/search?q={filter}")).await;
        assert_eq!(resource_names(&response), ["Bravo", "Delta", "Foxtrot"]);
        assert_eq!(response["resource_total"], 3);
        assert!(
            response["query"]["warnings"]
                .as_array()
                .expect("warnings")
                .is_empty()
        );
    }
    for filter in ["role:llm", "-role:git", "format:gguf", "state:local"] {
        let response = get(&state, &format!("/api/v1/search?q={filter}")).await;
        assert_eq!(resource_names(&response), ["alpha", "charlie", "echo"]);
    }
    let response = get(&state, "/api/v1/search?q=role:git").await;
    let roles = response["facets"]["role"].as_array().expect("roles");
    let git = roles
        .iter()
        .find(|bucket| bucket["value"] == "git")
        .expect("Git facet");
    let llm = roles
        .iter()
        .find(|bucket| bucket["value"] == "llm")
        .expect("LLM facet");
    assert_eq!(git["count"], 3);
    assert_eq!(git["active"], true);
    assert_eq!(llm["count"], 3);
    assert_eq!(response["facets"]["node"][0]["count"], 3);
    let all = get(&state, "/api/v1/search").await;
    assert_eq!(all["facets"]["node"][0]["count"], 6);
}

#[tokio::test]
async fn a_project_only_query_keeps_a_compact_resource_row() {
    let (_directory, state) = resource_world();
    let response = get(&state, "/api/v1/search?q=brav").await;
    assert_eq!(resource_names(&response), ["Bravo"]);
    assert_eq!(response["total"], 0);
    assert_eq!(response["resource_total"], 1);
    assert!(response["resource_next_cursor"].is_null());
}

#[tokio::test]
async fn size_filters_compare_the_bytes_shown_for_both_resource_kinds() {
    let (_directory, state) = resource_world();
    let response = get(&state, "/api/v1/search?q=size:%3C35b&sort=size").await;
    assert_eq!(resource_names(&response), ["Delta", "charlie", "Foxtrot"]);
    let negated = get(&state, "/api/v1/search?q=-size:%3C35b&sort=size").await;
    assert_eq!(resource_names(&negated), ["echo", "Bravo", "alpha"]);
}

#[tokio::test]
async fn loadouts_share_resource_pagination_facets_and_shelf_counts() {
    let (_directory, state) = resource_world();
    for name in ["Bravo", "Collection", "Zulu"] {
        state
            .catalog()
            .create_loadout(&hoard_core::Loadout::new(name))
            .expect("create collection");
    }
    let mut uri = "/api/v1/search?limit=2".to_owned();
    let mut found = Vec::new();
    loop {
        let response = get(&state, &uri).await;
        assert_eq!(response["resource_total"], 9);
        found.extend(
            response["resources"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| {
                    (
                        row["name"].as_str().expect("name").to_owned(),
                        row["resource_kind"].as_str().expect("kind").to_owned(),
                    )
                }),
        );
        match response["resource_next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/search?limit=2&resource_cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(
        found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "alpha",
            "Bravo",
            "Bravo",
            "charlie",
            "Collection",
            "Delta",
            "echo",
            "Foxtrot",
            "Zulu"
        ]
    );
    assert_eq!(found[1].1, "git");
    assert_eq!(found[2].1, "loadout");
    let response = get(&state, "/api/v1/search?q=role:loadout").await;
    assert_eq!(response["resource_total"], 3);
    assert_eq!(response["facets"]["node"][0]["count"], 3);
    let response = get(&state, "/api/v1/search?q=role:llm").await;
    assert_eq!(response["resource_total"], 3);
    state
        .catalog()
        .create_shelf("collections", "role:loadout", 99)
        .expect("shelf");
    let shelves = get(&state, "/api/v1/shelves").await;
    assert_eq!(
        shelves
            .as_array()
            .expect("shelves")
            .iter()
            .find(|shelf| shelf["name"] == "collections")
            .expect("collection shelf")["count"],
        3
    );
}
