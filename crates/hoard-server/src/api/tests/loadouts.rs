use super::*;

async fn started(world: &World, name: &str) -> String {
    let created = post(&world.state, "/api/v1/loadouts", json!({ "name": name })).await;
    created["id"].as_str().expect("id").to_owned()
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
