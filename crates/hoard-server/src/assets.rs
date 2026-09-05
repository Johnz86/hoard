use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

const INDEX: &str = "index.html";

/// The web UI. Embedded in the binary for release builds and read from disk in debug, so a
/// developer editing `web/` sees the change on reload without a rebuild.
#[derive(RustEmbed)]
#[folder = "../../web"]
#[exclude = "*.map"]
#[exclude = ".*"]
struct WebAssets;

/// Serves an embedded file, falling back to `index.html` so deep links into the UI work.
///
/// A request that presents the file's current entity tag is answered 304, which is what
/// keeps `index.html` from being sent again on every load: it is served `no-cache`, so the
/// browser revalidates it every time and the whole shell would otherwise be resent.
pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { INDEX } else { path };
    let found = WebAssets::get(path)
        .map(|file| (path, file))
        .or_else(|| WebAssets::get(INDEX).map(|file| (INDEX, file)));
    let Some((path, file)) = found else {
        return (
            StatusCode::NOT_FOUND,
            "the web UI was not embedded in this build",
        )
            .into_response();
    };
    embedded_response(path, file, &headers)
}

fn embedded_response(path: &str, file: rust_embed::EmbeddedFile, headers: &HeaderMap) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let etag = entity_tag(&file.metadata.sha256_hash());
    let cache = cache_control(path);
    if presented_etag_matches(headers, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache.to_owned()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

/// Whether `if-none-match` names the tag this file already has, honouring the list form
/// and the `*` and weak spellings the header allows.
fn presented_etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| {
            presented.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*" || candidate.trim_start_matches("W/") == etag
            })
        })
}

const fn cache_control(path: &str) -> &'static str {
    if matches!(path.as_bytes(), b"index.html") {
        "no-cache"
    } else {
        "public, max-age=3600"
    }
}

/// The quoted hex spelling of a file's content hash.
fn entity_tag(bytes: &[u8; 32]) -> String {
    const DIGITS: [u8; 16] = *b"0123456789abcdef";
    let mut tag = String::with_capacity(bytes.len() * 2 + 2);
    tag.push('"');
    for byte in bytes {
        tag.push(char::from(DIGITS[usize::from(byte >> 4)]));
        tag.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    tag.push('"');
    tag
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    #[test]
    fn index_html_is_available_to_the_router() {
        assert!(
            WebAssets::get("index.html").is_some(),
            "web/index.html must exist"
        );
    }

    #[test]
    fn every_shipped_file_under_web_reads_back_out_of_the_binary() {
        let embedded: BTreeSet<String> = WebAssets::iter().map(Cow::into_owned).collect();
        let missing: Vec<String> = shipped_sources()
            .into_iter()
            .filter(|path| !embedded.contains(path))
            .collect();
        assert!(
            missing.is_empty(),
            "a release build embeds web/ at compile time, so a file left out of the binary \
             is a blank page that serving from disk never shows: {missing:?}"
        );
        for path in &embedded {
            let file = WebAssets::get(path).expect("an embedded file reads back");
            assert!(!file.data.is_empty(), "{path} carries no bytes");
        }
    }

    /// The files under `web/` that the binary is expected to carry, which is everything the
    /// `exclude` attributes on `WebAssets` do not drop.
    fn shipped_sources() -> BTreeSet<String> {
        fn walk(dir: &Path, root: &Path, found: &mut BTreeSet<String>) {
            let read = std::fs::read_dir(dir).expect("web/ is readable from the crate root");
            for entry in read {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    walk(&path, root, found);
                    continue;
                }
                let relative = path.strip_prefix(root).expect("a path under web/");
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name.ends_with(".map") {
                    continue;
                }
                found.insert(relative.to_string_lossy().into_owned());
            }
        }

        let root = Path::new("../../web");
        let mut found = BTreeSet::new();
        walk(root, root, &mut found);
        assert!(!found.is_empty(), "web/ has files to ship");
        found
    }

    #[tokio::test]
    async fn unknown_paths_fall_back_to_the_shell() {
        let response = serve(Uri::from_static("/repo/models/org/name"), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn an_entity_tag_is_the_quoted_hex_of_the_content_hash() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        bytes[31] = 0x0f;
        let tag = entity_tag(&bytes);
        assert!(tag.starts_with("\"ab00"), "{tag}");
        assert!(tag.ends_with("0f\""), "{tag}");
        assert_eq!(tag.len(), 66);
    }

    #[tokio::test]
    async fn a_shell_that_is_already_held_is_not_sent_again() {
        let first = serve(Uri::from_static("/"), HeaderMap::new()).await;
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .expect("the shell carries an entity tag")
            .to_owned();

        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            etag.parse().expect("the tag is a header value"),
        );
        let again = serve(Uri::from_static("/"), headers.clone()).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        let body = axum::body::to_bytes(again.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(body.is_empty());

        let mut weak = HeaderMap::new();
        weak.insert(
            header::IF_NONE_MATCH,
            format!("W/{etag}").parse().expect("header value"),
        );
        assert_eq!(
            serve(Uri::from_static("/"), weak).await.status(),
            StatusCode::NOT_MODIFIED
        );

        let mut stale = HeaderMap::new();
        stale.insert(
            header::IF_NONE_MATCH,
            "\"nothing\"".parse().expect("header value"),
        );
        assert_eq!(
            serve(Uri::from_static("/"), stale).await.status(),
            StatusCode::OK
        );
    }
}
