use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The web UI. Embedded in the binary for release builds and read from disk in debug, so a
/// developer editing `web/` sees the change on reload without a rebuild.
#[derive(RustEmbed)]
#[folder = "../../web"]
#[exclude = "*.map"]
#[exclude = ".*"]
struct WebAssets;

/// Serves an embedded file, falling back to `index.html` so deep links into the UI work.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(path) {
        Some(file) => embedded_response(path, file),
        None => match WebAssets::get("index.html") {
            Some(index) => embedded_response("index.html", index),
            None => (
                StatusCode::NOT_FOUND,
                "the web UI was not embedded in this build",
            )
                .into_response(),
        },
    }
}

fn embedded_response(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let etag = format!("\"{}\"", hex_digest(&file.metadata.sha256_hash()));
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache_control(path).to_owned()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

const fn cache_control(path: &str) -> &'static str {
    if matches!(path.as_bytes(), b"index.html") {
        "no-cache"
    } else {
        "public, max-age=3600"
    }
}

fn hex_digest(bytes: &[u8; 32]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_html_is_available_to_the_router() {
        assert!(
            WebAssets::get("index.html").is_some(),
            "web/index.html must exist"
        );
    }

    #[tokio::test]
    async fn unknown_paths_fall_back_to_the_shell() {
        let response = serve(Uri::from_static("/repo/models/org/name")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
