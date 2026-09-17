use std::sync::Arc;

use hoard_index::catalog::{Catalog, RepositoryFilter};
use hoard_index::config::{IndexConfig, SourceConfig, SourceKind};
use hoard_mirror::Result;
use hoard_mirror::access::{AccessPolicy, AccessRule};
use hoard_mirror::cache::BlockCache;
use hoard_mirror::config::{Geometry, MirrorConfig};
use hoard_mirror::resolve::{Mirror, RevisionSource};
use hoard_mirror::upstream::client::{RetryPolicy, UpstreamClient};
use hoard_mirror::upstream::hub::Hub;
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn mirror_over(root: &std::path::Path, upstream: &str, catalog: Arc<Catalog>) -> Result<Mirror> {
    let config = MirrorConfig {
        offline: false,
        geometry: Geometry::new(64, 16)?,
        proxy: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
        cache: vec![AccessRule::glob("*", true), AccessRule::glob("*/*", true)],
        ..MirrorConfig::default()
    };
    let access = AccessPolicy::compile(&config)?;
    let cache = BlockCache::at_root(Arc::new(config), root);
    let client = UpstreamClient::new(false)?.with_retry(RetryPolicy::immediate(2));
    let hub = Hub::new(client, upstream)?;
    Ok(Mirror::new(catalog, Arc::new(cache), Arc::new(access), hub))
}

#[tokio::test]
async fn the_index_answers_a_loose_revisions_sha_before_falling_to_upstream() -> Result<()> {
    let server = MockServer::start().await;
    for verb in ["HEAD", "GET"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
    }
    let root = TempDir::new().expect("temp dir");
    let models = root.path().join("models");
    std::fs::create_dir_all(models.join("loras")).expect("fixture directory");
    std::fs::write(
        models.join("loras/keep.safetensors"),
        br#"{"lora_up.weight":{"dtype":"F16","shape":[4,4],"data_offsets":[0,32]}}"#,
    )
    .expect("fixture writes");
    let catalog = Arc::new(
        Catalog::open_or_scan(
            &IndexConfig {
                data_dir: root.path().join("native"),
                sources: vec![SourceConfig {
                    name: "comfy".to_owned(),
                    path: models,
                    kind: SourceKind::Loose,
                    include: Vec::new(),
                    exclude: Vec::new(),
                    follow_symlinks: false,
                    repo_per_folder: false,
                }],
                ..IndexConfig::default()
            },
            "node-a",
            |_, _| {},
        )
        .expect("catalog opens"),
    );
    let repo = catalog
        .repositories(&RepositoryFilter::default(), None, 50)
        .expect("repositories list")
        .rows
        .into_iter()
        .next()
        .expect("the loose folder is a repository")
        .repo;

    let mirror = mirror_over(root.path(), &server.uri(), catalog)?;
    let resolution = mirror.resolve_revision(&repo, "main", None).await?;

    assert_eq!(
        resolution.source,
        RevisionSource::Index,
        "a loose folder's own content sha answers without reaching upstream"
    );
    Ok(())
}
