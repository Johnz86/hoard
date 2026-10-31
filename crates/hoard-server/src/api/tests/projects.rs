use std::process::Command;
use std::sync::Arc;

use super::*;

struct ProjectWorld {
    state: AppState,
    id: String,
    checkout: std::path::PathBuf,
    _directory: TempDir,
}

#[test]
fn project_readme_names_are_case_insensitive_and_include_plain_text() {
    for path in [
        "README.md",
        "readme.MARKDOWN",
        "README",
        "docs/Readme.txt",
        "README.rst",
    ] {
        assert!(api::projects::is_readme(path), "{path}");
    }
    assert!(!api::projects::is_readme("readme.rs"));
    assert!(!api::projects::is_readme("my-readme.md"));
}

fn git_project(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Git output is UTF-8")
}

fn project_world(bare: bool, readme: bool) -> ProjectWorld {
    let directory = tempfile::tempdir().expect("temporary tree");
    let checkout = directory.path().join("pose-project");
    std::fs::create_dir_all(checkout.join("src")).expect("source directory");
    git_project(&checkout, &["init", "--quiet", "--initial-branch=main"]);
    git_project(&checkout, &["config", "user.name", "Hoard Test"]);
    git_project(
        &checkout,
        &["config", "user.email", "hoard@example.invalid"],
    );
    git_project(&checkout, &["config", "commit.gpgsign", "false"]);
    if readme {
        std::fs::write(
            checkout.join("README.md"),
            "# Pose project\n\nCommitted documentation.\n",
        )
        .expect("README writes");
    }
    std::fs::write(checkout.join("src/main.rs"), "fn main() {}\n").expect("source writes");
    std::fs::write(checkout.join("binary.dat"), [0, 255, 1]).expect("binary writes");
    std::fs::write(checkout.join("large.txt"), vec![b'x'; 256 * 1024 + 1])
        .expect("large file writes");
    git_project(&checkout, &["add", "."]);
    git_project(&checkout, &["commit", "--quiet", "-m", "initial project"]);
    git_project(&checkout, &["checkout", "--quiet", "-b", "alternate"]);
    if readme {
        std::fs::write(checkout.join("README.md"), "# Alternate documentation\n")
            .expect("alternate README writes");
    }
    std::fs::write(checkout.join("src/main.rs"), "fn alternate() {}\n")
        .expect("alternate source writes");
    git_project(&checkout, &["add", "."]);
    git_project(&checkout, &["commit", "--quiet", "-m", "alternate project"]);
    git_project(&checkout, &["checkout", "--quiet", "main"]);
    let source = if bare {
        let destination = directory.path().join("bare.git");
        git_project(
            directory.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                checkout.to_str().expect("checkout path"),
                destination.to_str().expect("bare path"),
            ],
        );
        destination
    } else {
        checkout.clone()
    };
    let data = directory.path().join("data");
    let mut config = HoardConfig::default();
    config.server.data_dir = data.clone();
    config.server.public_url = Some("http://hoarder.lan:7099".to_owned());
    config.index.data_dir = data;
    config.index.hash_policy = HashPolicy::Never;
    config.index.sources.push(SourceConfig {
        name: "projects".to_owned(),
        path: source,
        kind: SourceKind::Git,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: true,
    });
    config.mirror.offline = true;
    let catalog = Arc::new(
        Catalog::open_or_scan(&config.index, "workstation", |_, _| {}).expect("project scans"),
    );
    let repos = catalog
        .repositories(&hoard_index::catalog::RepositoryFilter::default(), None, 10)
        .expect("repositories list");
    let id = repos.rows[0].id.to_string();
    let state = AppState::new(config, catalog).expect("state builds");
    ProjectWorld {
        state,
        id,
        checkout,
        _directory: directory,
    }
}

#[tokio::test]
async fn git_project_detail_has_neutral_identity_files_refs_and_lan_clone_command() {
    let world = project_world(false, true);
    let legacy = get(&world.state, "/api/v1/repos/models/-/pose-project").await;
    assert_eq!(legacy["project_id"], world.id);
    let detail = get(&world.state, &format!("/api/v1/projects/{}", world.id)).await;
    let files = get(
        &world.state,
        &format!("/api/v1/projects/{}/files", world.id),
    )
    .await;

    assert_eq!(detail["name"], "pose-project");
    assert_eq!(detail["source"], "projects");
    assert_eq!(detail["node"], "workstation");
    assert_eq!(
        detail["project_url"],
        format!("http://hoarder.lan:7099/#/project/{}", world.id)
    );
    assert_eq!(
        detail["clone_command"],
        format!(
            "git clone -- http://hoarder.lan:7099/git/projects/{}.git pose-project",
            world.id
        )
    );
    assert_eq!(detail["files"], 4);
    assert!(detail.get("role").is_none());
    assert!(detail.get("type").is_none());
    assert_eq!(detail["refs"].as_array().expect("refs").len(), 2);
    assert!(
        files["rows"]
            .as_array()
            .expect("files")
            .iter()
            .any(|file| file["path"] == "src/main.rs")
    );
}

#[tokio::test]
async fn project_readme_and_source_preview_read_selected_commits_not_dirty_worktree() {
    let world = project_world(false, true);
    std::fs::write(
        world.checkout.join("README.md"),
        "dirty private documentation\n",
    )
    .expect("worktree changes");
    std::fs::write(world.checkout.join("src/main.rs"), "dirty private source\n")
        .expect("worktree changes");

    let readme = get(
        &world.state,
        &format!("/api/v1/projects/{}/readme", world.id),
    )
    .await;
    let alternate = get(
        &world.state,
        &format!("/api/v1/projects/{}/readme?revision=alternate", world.id),
    )
    .await;
    let source = get(
        &world.state,
        &format!("/api/v1/projects/{}/file?path=src/main.rs", world.id),
    )
    .await;
    let other_source = get(
        &world.state,
        &format!(
            "/api/v1/projects/{}/file?path=src/main.rs&revision=alternate",
            world.id
        ),
    )
    .await;

    assert!(
        readme["html"]
            .as_str()
            .expect("HTML")
            .contains("Pose project")
    );
    assert!(!readme["html"].as_str().expect("HTML").contains("dirty"));
    assert_eq!(readme["generated"], false);
    assert_eq!(readme["missing"], false);
    assert_eq!(readme["path"], "README.md");
    assert!(
        alternate["html"]
            .as_str()
            .expect("HTML")
            .contains("Alternate documentation")
    );
    assert_eq!(source["text"], "fn main() {}\n");
    assert_eq!(other_source["text"], "fn alternate() {}\n");
}

#[tokio::test]
async fn bare_projects_show_committed_readme_without_worktree_paths() {
    let world = project_world(true, true);
    let readme = get(
        &world.state,
        &format!("/api/v1/projects/{}/readme", world.id),
    )
    .await;
    let files = get(
        &world.state,
        &format!("/api/v1/projects/{}/files", world.id),
    )
    .await;

    assert!(
        readme["html"]
            .as_str()
            .expect("README")
            .contains("Pose project")
    );
    assert_eq!(files["rows"].as_array().expect("files").len(), 4);
}

#[tokio::test]
async fn a_project_without_readme_is_explicitly_missing_without_generated_model_card() {
    let world = project_world(false, false);
    let readme = get(
        &world.state,
        &format!("/api/v1/projects/{}/readme", world.id),
    )
    .await;

    assert_eq!(readme["missing"], true);
    assert_eq!(readme["generated"], false);
    assert_eq!(readme["html"], "");
}

#[tokio::test]
async fn project_previews_reject_traversal_unknown_files_and_unknown_revisions() {
    let world = project_world(false, true);
    for path in [
        "../README.md",
        "/README.md",
        "src%2F..%2FREADME.md",
        "src%5Cmain.rs",
        "",
    ] {
        let (status, _) = call(
            &world.state,
            Method::GET,
            &format!("/api/v1/projects/{}/file?path={path}", world.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    for suffix in [
        "/file?path=absent.txt",
        "/readme?revision=absent",
        "/files?revision=absent",
    ] {
        let (status, _) = call(
            &world.state,
            Method::GET,
            &format!("/api/v1/projects/{}{suffix}", world.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{suffix}");
    }
    let (unknown, _) = call(&world.state, Method::GET, "/api/v1/projects/r_999999", None).await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn binary_and_large_previews_have_explicit_metadata_and_raw_downloads_are_attachments() {
    let world = project_world(false, true);
    let binary = get(
        &world.state,
        &format!("/api/v1/projects/{}/file?path=binary.dat", world.id),
    )
    .await;
    let large = get(
        &world.state,
        &format!("/api/v1/projects/{}/file?path=large.txt", world.id),
    )
    .await;
    let response = router(&world.state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/projects/{}/raw?path=src/main.rs",
                    world.id
                ))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(binary["kind"], "binary");
    assert_eq!(binary["text"], Value::Null);
    assert_eq!(large["kind"], "too_large");
    assert_eq!(large["text"], Value::Null);
    assert_eq!(large["truncated"], true);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .expect("attachment header")
            .starts_with("attachment;")
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(bytes.as_ref(), b"fn main() {}\n");
}

#[tokio::test]
async fn main_search_finds_git_projects_by_name_committed_readme_and_file_paths() {
    let world = project_world(false, true);
    std::fs::write(
        world.checkout.join("README.md"),
        "privateonly worktree changes\n",
    )
    .expect("dirty README writes");

    for query in ["pose", "documentation", "src/main.rs"] {
        let search = get(&world.state, &format!("/api/v1/search?q={query}")).await;
        assert_eq!(search["total"], 0, "the project has no model artifacts");
        assert_eq!(search["projects"]["total"], 1, "{query}");
        assert_eq!(search["projects"]["rows"][0]["id"], world.id);
        assert_eq!(search["projects"]["rows"][0]["name"], "pose-project");
    }
    let dirty = get(&world.state, "/api/v1/search?q=privateonly").await;
    assert_eq!(dirty["projects"]["total"], 0);
    let models = get(&world.state, "/api/v1/search?q=role:llm").await;
    assert_eq!(models["projects"]["total"], 0);
}

#[tokio::test]
async fn project_listing_filters_source_and_binds_pagination_cursor_to_query() {
    let world = project_world(false, true);
    let original = world
        .state
        .catalog()
        .repository_by_id(world.id.parse().expect("id"))
        .expect("repository reads")
        .expect("project exists");
    world
        .state
        .catalog()
        .seed(&[SeedRepository {
            repo: RepoId::new(RepoType::Models, None, "other-project"),
            source: "projects".to_owned(),
            kind: SourceKind::Git,
            root_path: std::path::PathBuf::from(original.root_path),
            git_dir: original.git_dir.map(std::path::PathBuf::from),
            tool_layout: None,
            revisions: Vec::new(),
        }])
        .expect("second project seeds");

    let filtered = get(&world.state, "/api/v1/projects?source=projects").await;
    let missing = get(&world.state, "/api/v1/projects?source=missing").await;
    let first = get(&world.state, "/api/v1/projects?limit=1").await;
    let cursor = first["next_cursor"].as_str().expect("next page exists");
    let second = get(
        &world.state,
        &format!("/api/v1/projects?limit=1&cursor={cursor}"),
    )
    .await;
    let (reused, _) = call(
        &world.state,
        Method::GET,
        &format!("/api/v1/projects?q=pose&limit=1&cursor={cursor}"),
        None,
    )
    .await;

    assert_eq!(filtered["total"], 2);
    assert_eq!(missing["total"], 0);
    assert_eq!(first["rows"].as_array().expect("page").len(), 1);
    assert_eq!(second["rows"].as_array().expect("page").len(), 1);
    assert_ne!(first["rows"][0]["id"], second["rows"][0]["id"]);
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(reused, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn project_search_includes_bare_repositories_without_present_worktree_files() {
    let world = project_world(true, true);
    let projects = get(&world.state, "/api/v1/projects?q=documentation").await;
    let search = get(&world.state, "/api/v1/search?q=documentation").await;

    assert_eq!(projects["total"], 1);
    assert_eq!(projects["rows"][0]["id"], world.id);
    assert_eq!(projects["rows"][0]["available"], true);
    assert_eq!(search["projects"]["total"], 1);
}

struct ProjectHttpServer(tokio::task::JoinHandle<std::io::Result<()>>);

impl Drop for ProjectHttpServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn canonical_project_url_clones_committed_content_branches_and_tags_over_http() {
    let world = project_world(false, true);
    let expected_head = git_project(&world.checkout, &["rev-parse", "HEAD"]);
    let expected_alternate = git_project(&world.checkout, &["rev-parse", "alternate"]);
    git_project(&world.checkout, &["tag", "v1"]);
    std::fs::write(
        world.checkout.join("README.md"),
        "uncommitted private changes\n",
    )
    .expect("dirty worktree writes");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("isolated listener binds");
    let address = listener.local_addr().expect("listener address");
    let state = world.state.clone();
    let server = ProjectHttpServer(tokio::spawn(async move {
        axum::serve(listener, crate::app(state)).await
    }));
    let destination = world._directory.path().join("downloaded-project");
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "clone",
            "--quiet",
            "--",
            &format!("http://{address}/git/projects/{}.git", world.id),
        ])
        .arg(&destination)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .expect("clone completes within timeout")
        .expect("Git clone runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git_project(&destination, &["rev-parse", "HEAD"]),
        expected_head
    );
    assert_eq!(
        git_project(&destination, &["rev-parse", "origin/alternate"]),
        expected_alternate
    );
    assert_eq!(
        git_project(&destination, &["rev-parse", "refs/tags/v1"]),
        expected_head
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("README.md")).expect("cloned README"),
        "# Pose project\n\nCommitted documentation.\n"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("src/main.rs")).expect("cloned source"),
        "fn main() {}\n"
    );
    server.0.abort();
}

#[tokio::test]
async fn a_completed_git_scan_invalidates_cached_project_search_content() {
    let world = project_world(false, true);
    let original = get(&world.state, "/api/v1/projects?q=documentation").await;
    let absent = get(&world.state, "/api/v1/projects?q=refreshedsearchtoken").await;
    assert_eq!(original["total"], 1);
    assert_eq!(absent["total"], 0);
    std::fs::write(
        world.checkout.join("README.md"),
        "# Pose project\n\nrefreshedsearchtoken\n",
    )
    .expect("updated README writes");
    git_project(&world.checkout, &["add", "README.md"]);
    git_project(
        &world.checkout,
        &["commit", "--quiet", "-m", "update project documentation"],
    );
    let finished = world.state.coordinator().finished();
    world
        .state
        .coordinator()
        .request(hoard_index::coordinate::ScanRequest {
            sources: Some(std::collections::BTreeSet::from(["projects".to_owned()])),
            hash: HashPolicy::Never,
            origin: hoard_index::coordinate::Origin::Manual,
        })
        .await
        .expect("scan queues");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while world.state.coordinator().finished() == finished {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scan finishes");
    let updated = get(&world.state, "/api/v1/projects?q=refreshedsearchtoken").await;
    let removed = get(&world.state, "/api/v1/projects?q=documentation").await;
    world
        .state
        .coordinator()
        .shutdown()
        .await
        .expect("scan lane shuts down");

    assert_eq!(updated["total"], 1);
    assert_eq!(updated["rows"][0]["id"], world.id);
    assert_eq!(removed["total"], 0);
}
