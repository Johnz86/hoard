#![allow(clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use hoard_index::catalog::Catalog;
use hoard_server::{AppState, HoardConfig};
use serde_json::Value;
use tokio::sync::{Barrier, oneshot};

#[test]
#[ignore]
fn simultaneous_midpoint_http_requests() {
    let runtime = tokio::runtime::Runtime::new().expect("the HTTP runtime starts");
    runtime.block_on(async {
        let root = PathBuf::from(
            std::env::var_os("HOARD_SCALE_DIR")
                .expect("HOARD_SCALE_DIR must name an independently seeded synthetic catalog"),
        );
        let count: u64 = std::env::var("HOARD_SCALE_ENTRIES")
            .expect("HOARD_SCALE_ENTRIES is required")
            .parse()
            .expect("the count is numeric");
        let catalog = Catalog::open(&root.join(format!("native-{count}")))
            .expect("the synthetic catalog opens");
        let mut config = HoardConfig::default();
        config.server.data_dir = root.join("http");
        config.index.data_dir = root.join("http");
        config.index.watch = false;
        config.node.name = "scale".to_owned();
        config.mirror.offline = true;
        let catalog = Arc::new(catalog);
        let state = AppState::new(config, Arc::clone(&catalog)).expect("the HTTP state starts");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the synthetic server binds");
        let address = listener.local_addr().expect("the server has an address");
        let (shutdown, stopping) = oneshot::channel();
        let server = tokio::spawn(hoard_server::serve(listener, state, async move {
            stopping.await.expect("the measurement requests shutdown");
        }));
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("the client starts");
        let base = format!("http://{address}/api/v1/search?limit=100");
        let first: Value = client
            .get(&base)
            .send()
            .await
            .expect("the first page responds")
            .error_for_status()
            .expect("the first page succeeds")
            .json()
            .await
            .expect("the first page is JSON");
        assert_eq!(first["total"], count);
        let cursor = first["next_cursor"]
            .as_str()
            .expect("the first page has a cursor");
        let (_, digest) = cursor
            .split_once('-')
            .expect("the cursor includes its query digest");
        let offset = count.saturating_sub(100) / 2;
        let url = format!("{base}&cursor={offset:x}-{digest}");
        let expected: Value = client
            .get(&url)
            .send()
            .await
            .expect("the midpoint responds")
            .error_for_status()
            .expect("the midpoint succeeds")
            .json()
            .await
            .expect("the midpoint is JSON");
        assert_eq!(
            expected["rows"]
                .as_array()
                .expect("the rows are an array")
                .len(),
            100
        );
        let background = std::env::var_os("HOARD_SCALE_BACKGROUND").is_some();
        let barrier = Arc::new(Barrier::new(9 + usize::from(background)));
        let rebuilding = background.then(|| {
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                tokio::task::spawn_blocking(move || catalog.rebuild_search(|_| {}))
                    .await.expect("the background rebuild joins")
                    .expect("the background rebuild succeeds");
            })
        });
        let mut queries = Vec::with_capacity(8);
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let client = client.clone();
            let url = url.clone();
            queries.push(tokio::spawn(async move {
                barrier.wait().await;
                let started = Instant::now();
                let bytes = client
                    .get(url)
                    .send()
                    .await
                    .expect("a simultaneous request responds")
                    .error_for_status()
                    .expect("a simultaneous request succeeds")
                    .bytes()
                    .await
                    .expect("the whole response body arrives");
                (bytes, started.elapsed())
            }));
        }
        let started = Instant::now();
        barrier.wait().await;
        let mut walls = Vec::with_capacity(8);
        for query in queries {
            let (body, wall) = query.await.expect("the request task joins");
            let page: Value = serde_json::from_slice(&body).expect("the response is JSON");
            assert_eq!(page, expected);
            walls.push(wall);
        }
        eprintln!(
            "HTTP midpoint offset={offset}, background={background}, individual={walls:?}, wall={:?}, slowest={:?}",
            started.elapsed(),
            walls.iter().max().expect("eight requests finished")
        );
        if let Some(rebuilding) = rebuilding {
            rebuilding.await.expect("the background task joins");
        }
        shutdown.send(()).expect("the server receives shutdown");
        server
            .await
            .expect("the server task joins")
            .expect("the server shuts down");
    });
}
