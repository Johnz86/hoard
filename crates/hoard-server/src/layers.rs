use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::{
    Extensions, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Version, header,
};
use tower_http::LatencyUnit;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{Predicate, SizeAbove};
use tower_http::cors::{Any, CorsLayer};

use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// A monotonic per-process request id. A counter rather than a uuid because the id only has
/// to correlate a log line with a response inside one node's lifetime.
#[derive(Debug, Clone, Default)]
pub struct MakeCountingRequestId {
    next: std::sync::Arc<AtomicU64>,
}

impl MakeRequestId for MakeCountingRequestId {
    fn make_request_id<B>(&mut self, _: &Request<B>) -> Option<RequestId> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        HeaderValue::from_str(&format!("{id:016x}"))
            .ok()
            .map(RequestId::new)
    }
}

/// Body limit for the mutating `/api/v1` routes; file bytes are never uploaded to hoard.
pub const REQUEST_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// The smallest response worth compressing.
const MIN_COMPRESS_BYTES: u64 = 256;

/// A predicate over the response headers, which the closure impl of [`Predicate`] accepts.
type MediaPredicate = fn(StatusCode, Version, &HeaderMap, &Extensions) -> bool;

/// Compression for text responses only, chosen by an allowlist rather than a list of
/// exclusions.
///
/// Weights, packfiles and cached blocks are already incompressible, so compressing them
/// spends CPU on every LAN pull for nothing. It is not only wasteful: gzip applied to the
/// streamed `git upload-pack` result truncated the body, which broke `git clone` outright.
/// An allowlist cannot regress that way when a new binary content type appears.
pub fn compression() -> CompressionLayer<impl Predicate + Send + Sync + 'static> {
    let predicate = SizeAbove::new(MIN_COMPRESS_BYTES).and(is_text_response as MediaPredicate);
    CompressionLayer::new()
        .gzip(true)
        .zstd(true)
        .compress_when(predicate)
}

fn is_text_response(
    _status: StatusCode,
    _version: Version,
    headers: &HeaderMap,
    _extensions: &Extensions,
) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
        .is_some_and(|media| is_text_media(&media))
}

fn is_text_media(media: &str) -> bool {
    media.starts_with("text/")
        || media.ends_with("+json")
        || media.ends_with("+xml")
        || matches!(
            media,
            "application/json"
                | "application/javascript"
                | "application/xml"
                | "application/x-ndjson"
                | "application/toml"
        )
}

pub fn tracing_layer()
-> TraceLayer<tower_http::classify::SharedClassifier<tower_http::classify::ServerErrorsAsFailures>>
{
    TraceLayer::new_for_http().on_response(
        DefaultOnResponse::new()
            .level(Level::DEBUG)
            .latency_unit(LatencyUnit::Millis),
    )
}

pub fn request_id() -> SetRequestIdLayer<MakeCountingRequestId> {
    SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeCountingRequestId::default())
}

pub fn propagate_request_id() -> PropagateRequestIdLayer {
    PropagateRequestIdLayer::new(REQUEST_ID_HEADER)
}

/// The methods a cross-origin caller is approved for.
///
/// Reads are open on the LAN, so the browser UI of one node may talk to another. Writes are
/// not: a node with no token configured would otherwise let any page the user happens to
/// visit scan, apply a loadout, project symlinks or delete a shelf against this host. The
/// UI is served from this same origin and so is never subject to this list.
const CROSS_ORIGIN_METHODS: [Method; 3] = [Method::GET, Method::HEAD, Method::OPTIONS];

pub fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(CROSS_ORIGIN_METHODS)
        .allow_headers(Any)
}

pub fn catch_panic() -> CatchPanicLayer<tower_http::catch_panic::DefaultResponseForPanic> {
    CatchPanicLayer::new()
}

pub fn body_limit() -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(REQUEST_BODY_LIMIT)
}

/// Timeout for metadata and API routes. File streaming routes must not carry this: a 40 GB
/// pull over a slow link is not a hung request.
pub fn metadata_timeout(seconds: u64) -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn only_text_media_types_are_compressed() {
        for media in [
            "text/html",
            "text/plain; charset=utf-8",
            "application/json",
            "application/vnd.api+json",
            "image/svg+xml",
        ] {
            assert!(is_text_media(media), "{media} should compress");
        }
        for media in [
            "application/octet-stream",
            "application/x-git-upload-pack-result",
            "application/x-git-upload-pack-advertisement",
            "multipart/byteranges",
            "image/png",
            "font/woff2",
        ] {
            assert!(!is_text_media(media), "{media} must not compress");
        }
    }

    #[test]
    fn a_response_without_a_content_type_is_not_compressed() {
        assert!(!is_text_response(
            StatusCode::OK,
            Version::HTTP_11,
            &HeaderMap::new(),
            &Extensions::new()
        ));
    }

    #[test]
    fn a_cross_origin_write_is_never_approved() {
        assert!(
            !CROSS_ORIGIN_METHODS.contains(&Method::POST),
            "a page on another origin must not be able to write to this node"
        );
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
            assert!(
                !CROSS_ORIGIN_METHODS.contains(&method),
                "{method} is a write"
            );
        }
    }

    #[test]
    fn request_ids_are_unique_and_hex() {
        let mut maker = MakeCountingRequestId::default();
        let request = Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("request builds");
        let first = maker.make_request_id(&request).expect("id is produced");
        let second = maker.make_request_id(&request).expect("id is produced");
        assert_ne!(first.header_value(), second.header_value());
        let rendered = first.header_value().to_str().expect("id is ascii");
        assert_eq!(rendered.len(), 16);
        assert!(rendered.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
