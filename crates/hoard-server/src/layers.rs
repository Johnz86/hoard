use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
use tower_http::LatencyUnit;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
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

/// Compression for text responses only. Weights are already incompressible and running
/// them through gzip would burn CPU on every LAN pull for nothing.
pub fn compression() -> CompressionLayer<impl Predicate + Send + Sync + 'static> {
    let predicate = DefaultPredicate::new()
        .and(NotForContentType::new("application/octet-stream"))
        .and(NotForContentType::new("binary/octet-stream"))
        .and(NotForContentType::new("multipart/byteranges"))
        .and(NotForContentType::new("application/x-safetensors"));
    CompressionLayer::new()
        .gzip(true)
        .zstd(true)
        .compress_when(predicate)
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

/// Read routes are open on the LAN, so the browser UI of one node may talk to another.
pub fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
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
