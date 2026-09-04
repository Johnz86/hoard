//! The embedded web UI and its deep-link fallback.

use axum::Router;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::assets;
use crate::error::HubError;
use crate::state::AppState;

/// The prefix both API faces share. A path under it is answered as an API route even when
/// no route matched, because a client there parses JSON and cannot read the shell.
const API_PREFIX: &str = "/api/";

/// Serves `web/` at the root. Every unmatched read falls back to the shell so a deep link
/// such as `/repo/models/org/name` opens the app rather than 404ing.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(assets::serve))
        .fallback(fallback)
}

/// The one answer for a path no router claimed.
///
/// An unknown API route and any method other than a read answer the documented 404 with
/// `x-error-code`, rather than 200 and a page of HTML or a bare 405.
async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let readable = method == Method::GET || method == Method::HEAD;
    if !readable || uri.path().starts_with(API_PREFIX) {
        return HubError::page_not_found().into_response();
    }
    assets::serve(uri, headers).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn an_unknown_api_route_is_the_documented_404_rather_than_the_shell() {
        let response = fallback(
            Method::GET,
            Uri::from_static("/api/v1/nothing"),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("x-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("RepoNotFound")
        );
    }

    #[tokio::test]
    async fn an_unmatched_write_is_a_404_rather_than_a_405_without_an_error_code() {
        let response = fallback(Method::POST, Uri::from_static("/nothing"), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().contains_key("x-error-code"));
    }

    #[tokio::test]
    async fn a_deep_link_still_opens_the_app() {
        let response = fallback(
            Method::GET,
            Uri::from_static("/repo/models/org/name"),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
