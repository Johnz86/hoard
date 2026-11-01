use super::*;

async fn started(world: &World, name: &str) -> String {
    let created = post(&world.state, "/api/v1/loadouts", json!({ "name": name })).await;
    created["id"].as_str().expect("id").to_owned()
}

#[cfg(unix)]
#[tokio::test]
async fn a_successful_apply_creates_and_records_its_link_and_can_be_repeated() {
    let world = seeded();
    let id = started(&world, "apply-success").await;
    post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;

    for (profile, folder) in [
        ("comfyui", "checkpoints"),
        ("a1111", "Stable-diffusion"),
        ("llama", "models"),
    ] {
        for _ in 0..2 {
            let job = post(
                &world.state,
                &format!("/api/v1/loadouts/{id}/apply"),
                json!({ "profile": profile }),
            )
            .await;
            assert_eq!(job["profile"], profile);
            assert_eq!(
                job["destination"],
                world
                    .state
                    .projection_root()
                    .join(profile)
                    .to_string_lossy()
                    .as_ref()
            );
            let finished = settle(&world.state, job["job_id"].as_str().expect("job id")).await;

            assert_eq!(finished["state"], "done");
            assert_eq!(finished["error"], Value::Null);
            assert!(
                finished["phase"]
                    .as_str()
                    .expect("phase")
                    .starts_with("1 links written")
            );
            let projections = world
                .state
                .catalog()
                .projections_for_artifact(world.ckpt)
                .expect("projections");
            let projection = projections
                .iter()
                .find(|row| {
                    Path::new(&row.link_path).parent()
                        == Some(
                            world
                                .state
                                .projection_root()
                                .join(profile)
                                .join(folder)
                                .as_path(),
                        )
                })
                .expect("correct layout");
            assert_eq!(
                std::fs::read_link(&projection.link_path).expect("link exists"),
                Path::new(&projection.target_path)
            );
        }
    }
}

#[tokio::test]
async fn an_apply_with_a_blocked_destination_fails_and_preserves_the_file() {
    let world = seeded();
    let id = started(&world, "apply-blocked").await;
    post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;
    let root = world.state.projection_root();
    std::fs::create_dir_all(&root).expect("projection root");
    for profile in ["comfyui", "a1111", "llama"] {
        let obstruction = root.join(profile);
        std::fs::write(&obstruction, b"existing user file").expect("obstruction");

        let job = post(
            &world.state,
            &format!("/api/v1/loadouts/{id}/apply"),
            json!({ "profile": profile }),
        )
        .await;
        let finished = settle(&world.state, job["job_id"].as_str().expect("job id")).await;

        assert_eq!(finished["state"], "failed");
        assert!(finished["error"].as_str().expect("error").contains(profile));
        assert_eq!(
            std::fs::read(obstruction).expect("file preserved"),
            b"existing user file"
        );
        assert!(
            world
                .state
                .catalog()
                .list_projections(None)
                .expect("projections")
                .is_empty()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_partial_apply_fails_but_records_successful_links_and_preserves_conflicts() {
    let world = seeded();
    let id = started(&world, "apply-partial").await;
    for artifact in [world.ckpt, world.lora] {
        post(
            &world.state,
            &format!("/api/v1/loadouts/{id}/items"),
            json!({ "artifact_id": artifact.to_string() }),
        )
        .await;
    }
    let loras = world.state.projection_root().join("comfyui/loras");
    std::fs::create_dir_all(&loras).expect("lora destination");
    let obstruction = loras.join("film-grain-35mm.safetensors");
    std::fs::write(&obstruction, b"existing model").expect("obstruction");

    let job = post(
        &world.state,
        &format!("/api/v1/loadouts/{id}/apply"),
        json!({ "profile": "comfyui" }),
    )
    .await;
    let finished = settle(&world.state, job["job_id"].as_str().expect("job id")).await;

    assert_eq!(finished["state"], "failed");
    assert!(
        finished["error"]
            .as_str()
            .expect("error")
            .contains("film-grain-35mm")
    );
    assert_eq!(
        std::fs::read(obstruction).expect("model preserved"),
        b"existing model"
    );
    let projections = world
        .state
        .catalog()
        .list_projections(None)
        .expect("projections");
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0].artifact_id, Some(world.ckpt));
    assert!(std::fs::read_link(&projections[0].link_path).is_ok());
}

#[tokio::test]
async fn a_second_host_replaces_the_first_and_the_answer_names_it() {
    let world = seeded();
    let night = started(&world, "night").await;

    let first = post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;
    assert_eq!(first["replaced"], json!(null));
    assert!(
        first["notice"]
            .as_str()
            .expect("notice")
            .starts_with("+ sdxl-base-1.0 pinned at "),
        "{}",
        first["notice"]
    );

    let second = post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.llm.to_string() }),
    )
    .await;
    assert_eq!(second["id"], json!(night));
    assert_eq!(second["items"].as_array().expect("items").len(), 1);
    assert_eq!(second["items"][0]["name"], "llama-3-8b");
    assert_eq!(
        second["replaced"],
        json!({ "slot": "host", "name": "sdxl-base-1.0" })
    );
    assert_eq!(
        second["notice"],
        "replaced checkpoint sdxl-base-1.0 with llama-3-8b"
    );

    let again = post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.llm.to_string() }),
    )
    .await;
    assert_eq!(again["notice"], "llama-3-8b is already in night");
    assert_eq!(again["items"].as_array().expect("items").len(), 1);
    assert_eq!(again["replaced"], json!(null));

    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(listing.as_array().expect("rows").len(), 1);
}

#[tokio::test]
async fn a_second_host_branches_into_a_loadout_named_after_it() {
    let world = seeded();
    let night = started(&world, "night").await;
    post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.ckpt.to_string() }),
    )
    .await;

    let branched = post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.llm.to_string(), "on_second_host": "branch" }),
    )
    .await;
    let branch_id = branched["id"].as_str().expect("id").to_owned();
    assert_ne!(branch_id, night);
    assert_eq!(branched["name"], "llama-3-8b");
    assert_eq!(branched["items"].as_array().expect("items").len(), 1);
    assert_eq!(branched["items"][0]["slot"], "host");
    assert_eq!(branched["replaced"], json!(null));
    assert_eq!(
        branched["notice"],
        "new loadout \"llama-3-8b\": checkpoint pinned"
    );

    let kept = get(&world.state, &format!("/api/v1/loadouts/{night}")).await;
    assert_eq!(kept["items"].as_array().expect("items").len(), 1);
    assert_eq!(kept["items"][0]["name"], "sdxl-base-1.0");

    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(listing.as_array().expect("rows").len(), 2);
    assert_eq!(listing[0]["name"], "llama-3-8b");
    assert_eq!(listing[1]["name"], "night");

    let twice = post(
        &world.state,
        &format!("/api/v1/loadouts/{night}/items"),
        json!({ "artifact_id": world.llm.to_string(), "on_second_host": "branch" }),
    )
    .await;
    assert_eq!(twice["name"], "llama-3-8b-2");
    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(listing.as_array().expect("rows").len(), 3);

    let adapter = post(
        &world.state,
        &format!("/api/v1/loadouts/{branch_id}/items"),
        json!({ "artifact_id": world.lora.to_string(), "on_second_host": "branch" }),
    )
    .await;
    assert_eq!(adapter["id"], json!(branch_id));
    assert_eq!(adapter["items"].as_array().expect("items").len(), 2);
    assert_eq!(adapter["replaced"], json!(null));
}

#[tokio::test]
async fn an_unnamed_loadout_takes_the_next_free_number() {
    let world = seeded();
    let first = post(&world.state, "/api/v1/loadouts", json!({})).await;
    assert_eq!(first["name"], "loadout-1");
    let blank = post(&world.state, "/api/v1/loadouts", json!({ "name": "  " })).await;
    assert_eq!(blank["name"], "loadout-2");

    let (status, taken) = call(
        &world.state,
        Method::POST,
        "/api/v1/loadouts",
        Some(json!({ "name": "loadout-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(taken["error"]["code"], "Conflict");
    assert_eq!(
        taken["error"]["message"],
        "invalid loadout: a loadout named \"loadout-1\" already exists"
    );

    let (status, from_text) = call(
        &world.state,
        Method::POST,
        "/api/v1/loadouts",
        Some(json!({ "text": "name = \"loadout-2\"\n" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(
        listing.as_array().expect("rows").len(),
        2,
        "{from_text} refused, so nothing was stored"
    );
}

async fn create_status(world: &World, body: &Value) -> StatusCode {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/loadouts")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    router(&world.state)
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn a_malformed_create_body_is_refused() {
    let world = seeded();
    for body in [
        json!({ "name": 5 }),
        json!({ "nmae": "night" }),
        json!({ "text": 5 }),
    ] {
        let status = create_status(&world, &body).await;
        assert!(
            status.is_client_error(),
            "{body} answered {status} rather than a client error"
        );
    }

    let numbered = post(&world.state, "/api/v1/loadouts", json!({})).await;
    assert_eq!(numbered["name"], "loadout-1");
    let listing = get(&world.state, "/api/v1/loadouts").await;
    assert_eq!(listing.as_array().expect("rows").len(), 1);
}

#[tokio::test]
async fn a_loadout_is_renamed_and_deleted_and_a_taken_name_is_refused() {
    let world = seeded();
    let first = started(&world, "a").await;
    started(&world, "b").await;

    let (status, refused) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/loadouts/{first}"),
        Some(json!({ "name": "b" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused["error"]["code"], "Conflict");

    let (status, blank) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/loadouts/{first}"),
        Some(json!({ "name": " " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(blank["error"]["code"], "BadRequest");

    let (status, renamed) = call(
        &world.state,
        Method::PUT,
        &format!("/api/v1/loadouts/{first}"),
        Some(json!({ "name": "c" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["name"], "c");

    let exported = get(
        &world.state,
        &format!("/api/v1/loadouts/{first}/export?format=toml"),
    )
    .await;
    assert_eq!(exported["filename"], "c.toml");
    assert!(
        exported["text"]
            .as_str()
            .expect("text")
            .contains("name = \"c\"")
    );

    let (status, deleted) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/loadouts/{first}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted, json!({ "id": first, "deleted": true }));

    let (status, _) = call(
        &world.state,
        Method::GET,
        &format!("/api/v1/loadouts/{first}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/loadouts/{first}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &world.state,
        Method::PUT,
        "/api/v1/loadouts/999999",
        Some(json!({ "name": "d" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &world.state,
        Method::DELETE,
        "/api/v1/loadouts/999999",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn profile_preview_is_absolute_and_does_not_create_destinations() {
    let world = seeded();
    let info = get(&world.state, "/api/v1/loadouts/profiles").await;
    assert_eq!(info["default_profile"], "comfyui");
    assert_eq!(info["node"], world.state.node_name());
    let rows = info["profiles"].as_array().expect("profiles");
    assert_eq!(rows.len(), 3);
    for (row, (id, label)) in rows.iter().zip([
        ("comfyui", "ComfyUI"),
        ("a1111", "A1111"),
        ("llama", "llama"),
    ]) {
        assert_eq!(row["id"], id);
        assert_eq!(row["label"], label);
        let destination = Path::new(row["destination"].as_str().expect("destination"));
        assert!(destination.is_absolute());
        assert_eq!(
            destination,
            std::path::absolute(world.state.projection_root().join(id)).expect("absolute path")
        );
        assert!(!destination.exists());
    }
}
