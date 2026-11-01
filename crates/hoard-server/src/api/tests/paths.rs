use super::*;

fn path_world() -> (TempDir, AppState) {
    let directory = tempfile::tempdir().expect("temporary catalog");
    let catalog = crate::testing::empty_catalog(directory.path(), "workstation");
    let alias = seed_artifact(
        0x70,
        "text_encoder/model.fp16.safetensors",
        Artifact {
            sha256: Some(Sha256::from_bytes([0x70; 32])),
            ..facts(1024, ArtifactFormat::Safetensors, Role::Emb, 1.0)
        },
    );
    let original = seed_repo(
        "publisher/original",
        RepoType::Models,
        0x70,
        vec![seed_entry(
            "text_encoder/model.fp16.safetensors",
            "/mock/cache/model.fp16.safetensors",
            1024,
            Some(alias.clone()),
        )],
    );
    let mut encoders = seed_repo(
        "comfy/text_encoders",
        RepoType::Models,
        0x71,
        vec![seed_entry(
            "clip_l.safetensors",
            "/mock/models/text_encoders/clip_l.safetensors",
            1024,
            Some(alias),
        )],
    );
    encoders.source = "comfy".to_owned();
    encoders.kind = SourceKind::Loose;
    encoders.tool_layout = Some("comfyui".to_owned());
    let entries = ["portrait", "landscape"]
        .into_iter()
        .enumerate()
        .map(|(index, folder)| {
            let byte = index as u8 + 0x72;
            seed_entry(
                &format!("{folder}/model.safetensors"),
                &format!("/mock/models/diffusion_models/{folder}/model.safetensors"),
                1024,
                Some(seed_artifact(
                    u128::from(byte),
                    "model.safetensors",
                    Artifact {
                        sha256: Some(Sha256::from_bytes([byte; 32])),
                        ..facts(1024, ArtifactFormat::Safetensors, Role::Ckpt, 1.0)
                    },
                )),
            )
        })
        .collect();
    let mut models = seed_repo("comfy/diffusion_models", RepoType::Models, 0x72, entries);
    models.source = "comfy".to_owned();
    models.kind = SourceKind::Loose;
    models.tool_layout = Some("comfyui".to_owned());
    catalog
        .seed(&[original, encoders, models])
        .expect("seed locations");
    catalog.rebuild_search(|_| {}).expect("build search");
    let mut config = HoardConfig::default();
    config.node.name = "workstation".to_owned();
    let state = AppState::new(config, catalog).expect("state");
    (directory, state)
}

#[tokio::test]
async fn alias_search_inspection_and_commands_keep_the_selected_entry() {
    let (_directory, state) = path_world();
    let found = get(&state, "/api/v1/search?q=source:comfy%20clip_l").await;
    assert_eq!(found["resource_total"], 1);
    let row = &found["resources"][0];
    assert_eq!(row["display_name"], "clip_l.safetensors");
    assert_eq!(row["relative_path"], "clip_l.safetensors");
    assert_eq!(row["name"], "text_encoder/model.fp16.safetensors");
    let id = row["artifact_id"].as_str().expect("artifact id");
    let entry = row["entry_id"].as_str().expect("entry id");
    let inspection = get(&state, &format!("/api/v1/artifacts/{id}?entry={entry}")).await;
    assert_eq!(inspection["repo"], "models/comfy/text_encoders");
    assert_eq!(inspection["entry_id"], row["entry_id"]);
    assert_eq!(inspection["entries"].as_array().expect("aliases").len(), 2);
    let command = get(
        &state,
        &format!("/api/v1/artifacts/{id}/pull-command?entry={entry}"),
    )
    .await;
    assert!(
        command["hf"]
            .as_str()
            .expect("command")
            .contains("comfy/text_encoders 'clip_l.safetensors'")
    );
    let repo = get(
        &state,
        &format!("/api/v1/repos/models/comfy/text_encoders?artifact={id}&entry={entry}"),
    )
    .await;
    assert!(
        repo["pull"]["hf"]
            .as_str()
            .expect("command")
            .contains("clip_l.safetensors")
    );
    let (status, _) = call(
        &state,
        Method::GET,
        &format!("/api/v1/repos/models/publisher/original?artifact={id}&entry={entry}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let other = get(&state, "/api/v1/search?q=portrait").await;
    let wrong = other["resources"][0]["entry_id"]
        .as_str()
        .expect("other entry");
    let (status, _) = call(
        &state,
        Method::GET,
        &format!("/api/v1/artifacts/{id}?entry={wrong}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let created = post(&state, "/api/v1/loadouts", json!({"name":"Selected alias"})).await;
    let loadout = created["id"].as_str().expect("loadout id");
    let added = post(
        &state,
        &format!("/api/v1/loadouts/{loadout}/members"),
        json!({"artifact_ids":[id],"entry_ids":{id:entry}}),
    )
    .await;
    assert_eq!(added["items"][0]["repo"], "models/comfy/text_encoders");
    assert_eq!(added["items"][0]["path"], "clip_l.safetensors");
    assert_eq!(
        added["items"][0]["destination"],
        "text_encoders/clip_l.safetensors"
    );
    let (status, _) = call(
        &state,
        Method::POST,
        &format!("/api/v1/loadouts/{loadout}/members"),
        Some(json!({"artifact_ids":[id],"entry_ids":{id:wrong}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&state, &format!("/api/v1/loadouts/{loadout}")).await["items"]
            .as_array()
            .expect("members")
            .len(),
        1
    );
}

#[tokio::test]
async fn nested_models_remain_distinguishable_and_keep_their_client_folders() {
    let (_directory, state) = path_world();
    let found = get(
        &state,
        "/api/v1/search?q=source:comfy%20repo:diffusion_models",
    )
    .await;
    let rows = found["resources"].as_array().expect("models");
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0]["display_name"], rows[1]["display_name"]);
    let ids: Vec<_> = rows.iter().map(|row| row["artifact_id"].clone()).collect();
    let created = post(
        &state,
        "/api/v1/loadouts",
        json!({"name":"Nested collection"}),
    )
    .await;
    let id = created["id"].as_str().expect("loadout id");
    let added = post(
        &state,
        &format!("/api/v1/loadouts/{id}/members"),
        json!({"artifact_ids":ids}),
    )
    .await;
    let members = added["items"].as_array().expect("members");
    assert_ne!(members[0]["destination"], members[1]["destination"]);
    for member in members {
        assert_eq!(
            member["destination"],
            format!(
                "diffusion_models/{}",
                member["path"].as_str().expect("path")
            )
        );
    }
    let script = get(
        &state,
        &format!("/api/v1/loadouts/{id}/download?format=shell"),
    )
    .await;
    let text = script["text"].as_str().expect("script");
    assert!(text.contains("diffusion_models/portrait/model.safetensors"));
    assert!(text.contains("diffusion_models/landscape/model.safetensors"));
    let discoverable = get(&state, "/api/v1/search?q=role:loadout%20portrait").await;
    assert_eq!(discoverable["resource_total"], 1);
}
