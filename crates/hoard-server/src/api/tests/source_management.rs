use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::*;

struct SourceWorld {
    state: AppState,
    config: HoardConfig,
    first: SourceConfig,
    second: SourceConfig,
    _directory: TempDir,
}

fn source_world() -> SourceWorld {
    let directory = tempfile::tempdir().expect("temporary source tree");
    let source = |name: &str| {
        let path = directory.path().join(name);
        std::fs::create_dir(&path).expect("source directory");
        std::fs::write(path.join("shared.safetensors"), weights("shared")).expect("source file");
        SourceConfig {
            name: name.to_owned(),
            path,
            kind: SourceKind::Loose,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
            repo_per_folder: false,
        }
    };
    let first = source("first");
    let second = source("second");
    let mut config = HoardConfig::default();
    config.server.data_dir = directory.path().join("data");
    config.index.data_dir = config.server.data_dir.clone();
    config.index.sources = vec![first.clone(), second.clone()];
    config.index.hash_policy = HashPolicy::Now;
    config.index.watch = false;
    config.mirror.offline = true;
    let catalog =
        Arc::new(Catalog::open_or_scan(&config.index, "test", |_, _| {}).expect("catalog scans"));
    let state = AppState::new(config.clone(), catalog).expect("state builds");
    SourceWorld {
        state,
        config,
        first,
        second,
        _directory: directory,
    }
}

async fn remove(state: &AppState, name: &str) -> Value {
    let (status, payload) = call(
        state,
        Method::DELETE,
        &format!("/api/v1/sources/{name}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{payload}");
    payload
}

#[tokio::test]
async fn source_removal_preserves_files_shared_artifacts_and_other_source_scan_state() {
    let world = source_world();
    let before = get(&world.state, "/api/v1/sources").await;
    assert_eq!(before[0]["can_remove"], true);
    assert_eq!(before[0]["browse_query"], "source:first");
    let second_scan = before[1]["last_scan"].clone();
    let totals = world.state.catalog().totals().expect("totals");
    assert_eq!(totals.artifacts, 1);
    let deleted = remove(&world.state, "first").await;
    assert_eq!(deleted, json!({"name":"first","removed":true}));
    let sources = get(&world.state, "/api/v1/sources").await;
    assert_eq!(sources.as_array().expect("sources").len(), 1);
    assert_eq!(sources[0]["name"], "second");
    assert_eq!(sources[0]["last_scan"], second_scan);
    assert!(world.first.path.join("shared.safetensors").is_file());
    assert!(world.second.path.join("shared.safetensors").is_file());
    assert_eq!(world.state.catalog().totals().expect("totals").artifacts, 1);
    assert_eq!(
        get(&world.state, "/api/v1/search?q=dupe:true").await["total"],
        0
    );
    assert_eq!(
        world.state.catalog().totals().expect("totals"),
        world.state.catalog().count_summary().expect("recount")
    );
    assert_eq!(
        get(&world.state, "/api/v1/search?q=source:first").await["total"],
        0
    );
    remove(&world.state, "second").await;
    assert_eq!(world.state.catalog().totals().expect("totals").artifacts, 0);
    assert_eq!(
        world.state.catalog().totals().expect("totals"),
        world.state.catalog().count_summary().expect("recount")
    );
}

#[tokio::test]
async fn configured_and_ui_sources_remain_removed_after_reload_and_can_be_readded() {
    let world = source_world();
    remove(&world.state, "first").await;
    remove(&world.state, "first").await;
    let mut loaded = world.config.clone();
    loaded
        .load_managed_sources()
        .expect("configuration reloads exclusions");
    assert!(loaded.index.source("first").is_none());
    assert!(loaded.index.source("second").is_some());
    world
        .state
        .add_source(world.first.clone())
        .await
        .expect("source restores");
    let mut restored = world.config.clone();
    restored
        .load_managed_sources()
        .expect("restored configuration loads");
    assert_eq!(restored.index.source("first"), Some(&world.first));
    assert_eq!(restored.index.sources.len(), 2);
    remove(&world.state, "first").await;
    let mut removed_again = world.config.clone();
    removed_again
        .load_managed_sources()
        .expect("managed source is excluded too");
    assert!(removed_again.index.source("first").is_none());
    assert!(world.first.path.is_dir());
}

#[tokio::test]
async fn startup_recovery_finishes_persisted_source_removal_without_scanning_other_folders() {
    let world = source_world();
    world
        .config
        .disable_source("first")
        .expect("interrupted removal persisted");
    assert!(
        world
            .state
            .catalog()
            .sources()
            .expect("sources")
            .iter()
            .any(|source| source.name == "first")
    );
    world
        .state
        .recover_source_removals()
        .await
        .expect("recovery finishes");
    assert!(world.state.coordinator().config().source("first").is_none());
    assert_eq!(world.state.catalog().sources().expect("sources").len(), 1);
    assert!(world.first.path.join("shared.safetensors").is_file());
    assert_eq!(world.state.catalog().totals().expect("totals").artifacts, 1);
}

#[tokio::test]
async fn source_removal_and_scan_history_cleanup_require_authorization_and_protect_managed_storage()
{
    let world = source_world();
    let guarded = world.state.clone().with_node_token(NODE_TOKEN);
    let scan = world.state.catalog().create_job("scan", None).expect("job");
    world
        .state
        .catalog()
        .finish_job(scan)
        .expect("job finishes");
    for uri in [
        "/api/v1/sources/first".to_owned(),
        format!("/api/v1/jobs/{scan}"),
        "/api/v1/jobs?kind=scan".to_owned(),
    ] {
        let (status, _) = call(&guarded, Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    for name in [
        MIRROR_SOURCE,
        hoard_index::catalog::PULL_SOURCE,
        "peer:example",
    ] {
        let (status, _) = call(
            &world.state,
            Method::DELETE,
            &format!("/api/v1/sources/{name}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}");
    }
    let (unknown, _) = call(
        &world.state,
        Method::DELETE,
        "/api/v1/sources/missing",
        None,
    )
    .await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);
    let (authorized, _) = call_as(
        &guarded,
        Method::DELETE,
        "/api/v1/sources/first",
        None,
        Some(&format!("Bearer {NODE_TOKEN}")),
    )
    .await;
    assert_eq!(authorized, StatusCode::OK);
}

#[tokio::test]
async fn completed_scan_cleanup_preserves_active_scans_other_jobs_and_monotonic_ids() {
    let world = source_world();
    let active = world
        .state
        .catalog()
        .create_job("scan", None)
        .expect("active scan");
    let pull = world
        .state
        .catalog()
        .create_job("pull", None)
        .expect("pull");
    world
        .state
        .catalog()
        .finish_job(pull)
        .expect("pull finishes");
    let done = world
        .state
        .catalog()
        .create_job("scan", None)
        .expect("done scan");
    world
        .state
        .catalog()
        .finish_job(done)
        .expect("scan finishes");
    let cancelled = world
        .state
        .catalog()
        .create_job("scan", None)
        .expect("cancelled scan");
    world
        .state
        .catalog()
        .cancel_job(cancelled)
        .expect("scan cancels");
    for id in [active, pull] {
        let (status, _) = call(
            &world.state,
            Method::DELETE,
            &format!("/api/v1/jobs/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    let (deleted, _) = call(
        &world.state,
        Method::DELETE,
        &format!("/api/v1/jobs/{done}"),
        None,
    )
    .await;
    assert_eq!(deleted, StatusCode::OK);
    let (cleared, payload) =
        call(&world.state, Method::DELETE, "/api/v1/jobs?kind=scan", None).await;
    assert_eq!(cleared, StatusCode::OK);
    assert_eq!(payload["deleted"], 1);
    assert!(
        world
            .state
            .catalog()
            .job(active)
            .expect("job reads")
            .is_some()
    );
    assert!(
        world
            .state
            .catalog()
            .job(pull)
            .expect("job reads")
            .is_some()
    );
    assert!(
        world
            .state
            .catalog()
            .job(cancelled)
            .expect("job reads")
            .is_none()
    );
    let new = world
        .state
        .catalog()
        .create_job("scan", None)
        .expect("new scan");
    assert!(new > cancelled);
    let (bad_filter, _) = call(&world.state, Method::DELETE, "/api/v1/jobs", None).await;
    assert_eq!(bad_filter, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_aborted_removal_request_still_finishes_after_the_current_scan() {
    let world = source_world();
    let catalog = Arc::clone(world.state.catalog());
    let (ready, held) = tokio::sync::oneshot::channel();
    let (release, release_wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let mut ready = Some(ready);
        catalog.rebuild_search(|_| {
            if let Some(ready) = ready.take() {
                ready.send(()).ok();
                release_wait.recv_timeout(Duration::from_secs(5)).ok();
            }
        })
    });
    held.await.expect("search lock is held");
    let scan = world
        .state
        .coordinator()
        .request(hoard_index::coordinate::ScanRequest {
            sources: Some(BTreeSet::from(["first".to_owned()])),
            hash: HashPolicy::Never,
            origin: hoard_index::coordinate::Origin::Manual,
        })
        .await
        .expect("scan queues");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = get(&world.state, &format!("/api/v1/jobs/{scan}")).await;
            if row["phase"] == "refreshing search" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scan reaches held search lane");
    let state = world.state.clone();
    let request =
        tokio::spawn(
            async move { call(&state, Method::DELETE, "/api/v1/sources/first", None).await },
        );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !world
            .config
            .disabled_sources()
            .expect("disabled sources")
            .contains("first")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("removal intent persists");
    request.abort();
    release.send(()).expect("search lock releases");
    holder
        .join()
        .expect("search holder joins")
        .expect("search rebuilds");
    tokio::time::timeout(Duration::from_secs(5), async {
        while world
            .state
            .catalog()
            .sources()
            .expect("sources")
            .iter()
            .any(|source| source.name == "first")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned removal finishes");
    world
        .state
        .add_source(world.first.clone())
        .await
        .expect("re-add waits for removal and succeeds");
    assert!(world.first.path.join("shared.safetensors").is_file());
    assert!(
        !world
            .config
            .disabled_sources()
            .expect("disabled sources")
            .contains("first")
    );
    world
        .state
        .coordinator()
        .shutdown()
        .await
        .expect("scan lane stops");
}
