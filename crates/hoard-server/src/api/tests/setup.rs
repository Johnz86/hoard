use std::sync::Arc;

use super::*;

fn empty_setup_world() -> (TempDir, HoardConfig, AppState) {
    let directory = tempfile::tempdir().expect("temporary setup tree");
    let mut config = HoardConfig::default();
    config.server.data_dir = directory.path().join("data");
    config.index.data_dir = config.server.data_dir.clone();
    config.index.watch = false;
    config.mirror.offline = true;
    let catalog = crate::testing::empty_catalog(&config.index.data_dir, "test");
    let state = AppState::new(config.clone(), catalog).expect("state builds");
    (directory, config, state)
}

#[tokio::test]
async fn a_fresh_node_has_an_actionable_setup_state_without_an_unusable_public_address() {
    let (_directory, _config, state) = empty_setup_world();
    let response = get(&state, "/api/v1/setup").await;

    assert_eq!(response["first_run"], true);
    assert_eq!(response["configured_sources"], 0);
    assert_eq!(response["resource_count"], 0);
    assert_eq!(response["scanning"], false);
    assert_eq!(response["public_url"], "http://127.0.0.1:7099");
    assert_eq!(response["address_state"], "local_only");
    assert_eq!(response["pending"], Value::Null);
    assert_eq!(response["restart_required"], false);
    assert!(
        response["access_message"]
            .as_str()
            .expect("access message")
            .contains("anyone")
    );
}

#[tokio::test]
async fn an_empty_configured_folder_is_not_mistaken_for_an_unconfigured_installation() {
    let (directory, mut config, state) = empty_setup_world();
    let path = directory.path().join("empty");
    std::fs::create_dir(&path).expect("empty folder");
    config.index.sources.push(SourceConfig {
        name: "empty".to_owned(),
        path,
        kind: SourceKind::Auto,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: true,
    });
    let configured = AppState::new(config, Arc::clone(state.catalog())).expect("configured node");
    let response = get(&configured, "/api/v1/setup").await;

    assert_eq!(response["first_run"], false);
    assert_eq!(response["configured_sources"], 1);
    assert_eq!(response["resource_count"], 0);
}

#[tokio::test]
async fn setup_counts_all_resources_including_projects_and_peers_independent_of_search() {
    let (directory, _config, state) = empty_setup_world();
    let mut project = seed_repo(
        "pose",
        RepoType::Models,
        1,
        vec![seed_entry("README.md", "/mock/README.md", 10, None)],
    );
    project.source = "projects".to_owned();
    project.kind = SourceKind::Git;
    project.git_dir = Some(directory.path().join("pose.git"));
    let artifact = seed_repo(
        "film",
        RepoType::Models,
        2,
        vec![seed_entry(
            "film.gguf",
            "/mock/film.gguf",
            20,
            Some(seed_artifact(
                1,
                "film",
                facts(20, ArtifactFormat::Gguf, Role::Llm, 1.0),
            )),
        )],
    );
    state
        .catalog()
        .seed(&[project, artifact])
        .expect("resources seed");
    state
        .catalog()
        .rebuild_search(|_| {})
        .expect("search builds");
    let before = get(&state, "/api/v1/setup").await;
    assert_eq!(before["first_run"], false);
    assert_eq!(before["configured_sources"], 0);
    assert_eq!(before["resource_count"], 2);
    assert_eq!(
        get(&state, "/api/v1/search?q=does-not-exist").await["resource_total"],
        0
    );
    assert_eq!(get(&state, "/api/v1/setup").await, before);

    let (_directory, _config, state) = empty_setup_world();
    seed_catalog_peer(state.catalog());
    let response = get(&state, "/api/v1/setup").await;
    assert_eq!(response["first_run"], false);
    assert_eq!(response["configured_sources"], 0);
    assert_eq!(response["resource_count"], 1);
}

#[tokio::test]
async fn sharing_settings_are_pending_until_restart_and_survive_reloading_configuration() {
    let (_directory, mut config, state) = empty_setup_world();
    let body = json!({"node_name":" Studio ","public_url":" http://studio.lan:7099/ "});
    let (status, response) = call(&state, Method::PUT, "/api/v1/setup", Some(body)).await;

    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["restart_required"], true);
    assert_eq!(
        response["pending"],
        json!({"node_name":"Studio","public_url":"http://studio.lan:7099"})
    );
    assert_eq!(response["node_name"], state.node_name());
    assert_eq!(response["public_url"], "http://127.0.0.1:7099");
    assert_ne!(state.node_name(), "Studio");
    assert!(config.server.data_dir.join("node.json").is_file());

    config.load_managed_sources().expect("managed config loads");
    assert_eq!(config.node.name, "Studio");
    assert_eq!(
        config.server.public_url.as_deref(),
        Some("http://studio.lan:7099")
    );
    let restarted = AppState::new(config, Arc::clone(state.catalog())).expect("restarted node");
    let response = get(&restarted, "/api/v1/setup").await;
    assert_eq!(response["node_name"], "Studio");
    assert_eq!(response["public_url"], "http://studio.lan:7099");
    assert_eq!(response["address_state"], "configured");
    assert_eq!(response["pending"], Value::Null);
    assert_eq!(response["restart_required"], false);
}

#[tokio::test]
async fn saving_the_current_configuration_cancels_pending_changes_without_restarting() {
    let (_directory, _config, state) = empty_setup_world();
    let changed = json!({"node_name":"Other","public_url":"http://other.lan:7099"});
    let (status, _) = call(&state, Method::PUT, "/api/v1/setup", Some(changed)).await;
    assert_eq!(status, StatusCode::OK);
    let current = json!({"node_name":state.node_name(),"public_url":state.public_url()});
    let (status, response) = call(&state, Method::PUT, "/api/v1/setup", Some(current)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["pending"], Value::Null);
    assert_eq!(response["restart_required"], false);
}

#[tokio::test]
async fn setup_is_readable_without_the_owner_token_but_changes_require_it_and_never_return_it() {
    let (_directory, config, state) = empty_setup_world();
    let state = state.with_node_token(NODE_TOKEN);
    let response = get(&state, "/api/v1/setup").await;
    assert_eq!(response["write_token_required"], true);
    assert!(!response.to_string().contains(NODE_TOKEN));
    let body = json!({"node_name":"Studio","public_url":"http://studio.lan:7099"});
    for authorization in [None, Some("Bearer wrong")] {
        let (status, _) = call_as(
            &state,
            Method::PUT,
            "/api/v1/setup",
            Some(body.clone()),
            authorization,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(!config.server.data_dir.join("node.json").exists());
    let (status, response) = call_as(
        &state,
        Method::PUT,
        "/api/v1/setup",
        Some(body),
        Some(&format!("Bearer {NODE_TOKEN}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(!response.to_string().contains(NODE_TOKEN));
    assert!(
        !std::fs::read_to_string(config.server.data_dir.join("node.json"))
            .expect("saved settings")
            .contains(NODE_TOKEN)
    );
}

#[tokio::test]
async fn invalid_sharing_configuration_is_rejected_without_writing_a_pending_file() {
    let (_directory, config, state) = empty_setup_world();
    for body in [
        json!({"node_name":" ","public_url":"http://studio.lan"}),
        json!({"node_name":"Studio","public_url":"http://0.0.0.0:7099"}),
        json!({"node_name":"Studio","public_url":"http://[::]:7099"}),
        json!({"node_name":"Studio","public_url":"http://user:secret@studio.lan"}),
        json!({"node_name":"Studio","public_url":"http://studio.lan?token=secret"}),
        json!({"node_name":"Studio","public_url":"http://studio.lan/hoard"}),
    ] {
        let (status, _) = call(&state, Method::PUT, "/api/v1/setup", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert!(!config.server.data_dir.join("node.json").exists());
}

#[tokio::test]
async fn sharing_status_classifies_configuration_without_claiming_network_reachability() {
    let (_directory, config, state) = empty_setup_world();
    for (bind, public_url, expected) in [
        ("0.0.0.0:7099", "http://studio.lan:7099", "configured"),
        ("127.0.0.1:7099", "http://studio.lan:7099", "local_only"),
        ("0.0.0.0:7099", "http://localhost:7099", "local_only"),
        ("[::]:7099", "http://[::1]:7099", "local_only"),
        ("0.0.0.0:7099", "http://0.0.0.0:7099", "unusable"),
        ("[::]:7099", "http://[::]:7099", "unusable"),
        ("0.0.0.0:7099", "not a URL", "unusable"),
    ] {
        let mut config = config.clone();
        config.server.bind = bind.parse().expect("listener address");
        config.server.public_url = Some(public_url.to_owned());
        let configured = AppState::new(config, Arc::clone(state.catalog())).expect("state builds");
        let response = get(&configured, "/api/v1/setup").await;
        assert_eq!(response["address_state"], expected, "{public_url}");
        if expected == "configured" {
            assert!(
                response["address_message"]
                    .as_str()
                    .expect("address message")
                    .contains("check network access")
            );
        }
    }
}
