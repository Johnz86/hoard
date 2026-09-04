//! The HTTP client every upstream shares: one connection pool, one retry classification and
//! one place that forwards a caller's credentials without ever logging them.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, HeaderMap, RANGE};
use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, Method, Response, StatusCode};
use url::{Host, Url};

use crate::config::MirrorConfig;
use crate::error::{MirrorError, Result};

const USER_AGENT: &str = concat!("hoard/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const POOL_MAX_IDLE_PER_HOST: usize = 16;
const MAX_REDIRECTS: usize = 10;

/// The encoding hoard asks for whenever the bytes are going into the block cache: a cached
/// block is the file itself, so a transfer encoding would make the cache unreadable.
pub const IDENTITY_ENCODING: &str = "identity";

/// The header family `hf_xet` keys off to fetch content straight from Xet. Hoard strips it
/// from anything it serves itself so a client cannot be told to walk around the mirror.
pub const XET_HEADER_PREFIX: &str = "x-xet-";

/// The media type of every document the Hub API exchanges.
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// The most a pass-through route may hold in memory. The routes that use it exchange tokens
/// and reconstruction manifests, so an answer past this is an upstream that lost the plot
/// rather than a body worth buffering.
pub const MAX_PASS_THROUGH_BYTES: u64 = 64 * 1024 * 1024;

/// A caller's `Authorization` header, forwarded upstream and never rendered.
#[derive(Clone, PartialEq, Eq)]
pub struct Authorization(String);

impl Authorization {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The header as a request carried it, if it carried a non-empty one.
    pub fn from_header(value: Option<&str>) -> Option<Self> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(Self::new)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Authorization(<redacted>)")
    }
}

/// Whether a failed upstream attempt is worth repeating.
///
/// 408, 425, 429 and every 5xx are the upstream saying "not now", which the contract turns
/// into 504 `ProxyTimeout`. Everything else, 401, 403 and 404 included, is an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    Terminal,
    Retryable,
}

pub fn classify(status: StatusCode) -> Retryability {
    match status.as_u16() {
        408 | 425 | 429 => Retryability::Retryable,
        code if code >= 500 => Retryability::Retryable,
        _ => Retryability::Terminal,
    }
}

/// How often a retryable upstream answer is repeated before it becomes `ProxyTimeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            backoff: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    /// A policy that gives up after `attempts` tries without waiting between them.
    pub const fn immediate(attempts: u32) -> Self {
        Self {
            attempts,
            backoff: Duration::ZERO,
        }
    }

    fn delay(self, attempt: u32) -> Duration {
        self.backoff.saturating_mul(1u32 << attempt.min(4))
    }
}

/// A request body and the media type that describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestBody {
    pub content_type: &'static str,
    pub bytes: Bytes,
}

impl RequestBody {
    /// The JSON body the metadata routes post, so `paths-info` reaches an upstream as the
    /// document a Hub expects rather than as whatever spelling the client used.
    pub fn json(bytes: impl Into<Bytes>) -> Self {
        Self {
            content_type: JSON_CONTENT_TYPE,
            bytes: bytes.into(),
        }
    }
}

/// One upstream call: what to ask for and what to carry with it.
#[derive(Debug, Clone)]
pub struct UpstreamRequest<'a> {
    pub method: Method,
    pub url: &'a Url,
    pub auth: Option<&'a Authorization>,
    pub range: Option<String>,
    pub accept_encoding: Option<&'a str>,
    /// The query string as an upstream expects it, replacing whatever `url` carries.
    pub query: Option<String>,
    pub body: Option<RequestBody>,
}

impl<'a> UpstreamRequest<'a> {
    pub fn new(method: Method, url: &'a Url) -> Self {
        Self {
            method,
            url,
            auth: None,
            range: None,
            accept_encoding: None,
            query: None,
            body: None,
        }
    }

    pub fn get(url: &'a Url) -> Self {
        Self::new(Method::GET, url)
    }

    pub fn head(url: &'a Url) -> Self {
        Self::new(Method::HEAD, url)
    }

    pub fn post(url: &'a Url) -> Self {
        Self::new(Method::POST, url)
    }

    pub fn with_auth(mut self, auth: Option<&'a Authorization>) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_range(mut self, range: impl Into<String>) -> Self {
        self.range = Some(range.into());
        self
    }

    pub fn with_accept_encoding(mut self, encoding: &'a str) -> Self {
        self.accept_encoding = Some(encoding);
        self
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    pub fn with_body(mut self, body: RequestBody) -> Self {
        self.body = Some(body);
        self
    }

    /// The URL the request is actually sent to, which is `url` with its query replaced.
    pub fn target(&self) -> Url {
        let mut url = self.url.clone();
        if let Some(query) = &self.query {
            url.set_query(Some(query));
        }
        url
    }

    /// The byte range spelled the way an upstream expects it, inclusive at both ends.
    pub fn with_byte_range(self, start: u64, end_exclusive: u64) -> Self {
        self.with_range(format!("bytes={start}-{}", end_exclusive.saturating_sub(1)))
    }
}

/// An upstream response held whole, for the routes hoard passes through unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl ProxyResponse {
    /// The same response with the Xet header family removed, for anything hoard serves as
    /// its own rather than forwards on the client's behalf.
    pub fn without_xet_headers(mut self) -> Self {
        self.headers
            .retain(|(name, _)| !name.starts_with(XET_HEADER_PREFIX));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A pooled HTTP client that knows whether it is allowed on the network at all.
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    http: Client,
    offline: bool,
    retry: RetryPolicy,
}

impl UpstreamClient {
    /// Builds the client. `offline` makes every request fail before a socket is opened.
    pub fn new(offline: bool) -> Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .redirect(redirect_policy())
            .no_gzip()
            .build()?;
        Ok(Self {
            http,
            offline,
            retry: RetryPolicy::default(),
        })
    }

    pub fn from_config(config: &MirrorConfig) -> Result<Self> {
        Self::new(config.offline)
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub const fn is_offline(&self) -> bool {
        self.offline
    }

    pub const fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// Sends one request, repeating it while the upstream answers retryably.
    ///
    /// A terminal status comes back as `Ok`: 401, 403 and 404 are answers the caller has to
    /// map onto the error contract, not transport failures.
    pub async fn send(&self, request: UpstreamRequest<'_>) -> Result<Response> {
        if self.offline {
            return Err(MirrorError::Offline(request.url.to_string()));
        }
        let mut last: Option<StatusCode> = None;
        for attempt in 0..self.retry.attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(self.retry.delay(attempt - 1)).await;
            }
            match self.attempt(&request).await {
                Ok(response) if classify(response.status()) == Retryability::Terminal => {
                    return Ok(response);
                }
                Ok(response) => last = Some(response.status()),
                Err(error) if error.is_builder() || error.is_redirect() => {
                    return Err(error.into());
                }
                Err(_) => last = None,
            }
        }
        tracing::warn!(
            url = %request.url,
            status = last.map(|status| status.as_u16()),
            "upstream gave up after retries"
        );
        Err(MirrorError::ProxyTimeout)
    }

    async fn attempt(
        &self,
        request: &UpstreamRequest<'_>,
    ) -> std::result::Result<Response, reqwest::Error> {
        let mut builder = self.http.request(request.method.clone(), request.target());
        if let Some(body) = &request.body {
            builder = builder
                .header(CONTENT_TYPE, body.content_type)
                .body(body.bytes.clone());
        }
        if let Some(auth) = request.auth {
            builder = builder.header(AUTHORIZATION, auth.expose());
        }
        if let Some(range) = &request.range {
            builder = builder.header(RANGE, range);
        }
        if let Some(encoding) = request.accept_encoding {
            builder = builder.header(ACCEPT_ENCODING, encoding);
        }
        builder.send().await
    }

    /// Sends a request and holds the answer, for the pass-through routes.
    ///
    /// The body is capped at [`MAX_PASS_THROUGH_BYTES`]: these routes forward documents, so
    /// an upstream that answers with a file instead is refused rather than buffered.
    pub async fn pass_through(&self, request: UpstreamRequest<'_>) -> Result<ProxyResponse> {
        let url = request.target();
        let response = self.send(request).await?;
        let status = response.status().as_u16();
        let headers = header_pairs(response.headers());
        let body = read_capped(response, MAX_PASS_THROUGH_BYTES, &url).await?;
        Ok(ProxyResponse {
            status,
            headers,
            body,
        })
    }
}

/// Reads a response body, failing as soon as more than `limit` bytes have arrived.
///
/// An upstream that ignores `Range` answers with the whole file, so a body is never held
/// whole before its length is known to be the length that was asked for.
pub async fn read_capped(response: Response, limit: u64, url: &Url) -> Result<Bytes> {
    let mut response = response;
    let hint = response.content_length().unwrap_or(0).min(limit);
    let mut body = BytesMut::with_capacity(usize::try_from(hint).unwrap_or_default());
    while let Some(chunk) = response.chunk().await? {
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err(MirrorError::ProxyInvalidData(format!(
                "{url} sent more than the {limit} bytes it was asked for"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// Follows a redirect only back to the host the request started at or on to a public
/// address, so an upstream on the LAN cannot point the mirror at loopback, a private range
/// or a link-local metadata service and have the answer land in the block cache.
fn redirect_policy() -> Policy {
    Policy::custom(|attempt: Attempt<'_>| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
        }
        if redirect_allowed(attempt.url(), attempt.previous()) {
            return attempt.follow();
        }
        let refusal = format!(
            "redirect to {} left the upstream for a non-public address",
            attempt.url()
        );
        attempt.error(refusal)
    })
}

fn redirect_allowed(next: &Url, previous: &[Url]) -> bool {
    let Some(host) = next.host() else {
        return false;
    };
    let started_here = previous
        .first()
        .and_then(Url::host_str)
        .zip(next.host_str())
        .is_some_and(|(origin, target)| origin == target);
    if started_here {
        return true;
    }
    match host {
        Host::Domain(_) => true,
        Host::Ipv4(address) => is_public_address(IpAddr::V4(address)),
        Host::Ipv6(address) => is_public_address(IpAddr::V6(address)),
    }
}

/// Whether an address is routable on the public internet, which is the only place a
/// redirect may take an upstream that did not start on the LAN.
fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
                || address.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && octets[1] & 0xc0 == 64))
        }
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => is_public_address(IpAddr::V4(mapped)),
            None => {
                let segments = address.segments();
                !(address.is_loopback()
                    || address.is_unspecified()
                    || segments[0] & 0xfe00 == 0xfc00
                    || segments[0] & 0xffc0 == 0xfe80)
            }
        },
    }
}

/// The response headers as lower-case name and value pairs, dropping any value that is not
/// text a client could have sent.
pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn url(base: &str, suffix: &str) -> Url {
        Url::parse(&format!("{base}{suffix}")).expect("url parses")
    }

    fn client() -> UpstreamClient {
        UpstreamClient::new(false)
            .expect("client builds")
            .with_retry(RetryPolicy::immediate(3))
    }

    #[test]
    fn credentials_never_render() {
        let auth = Authorization::new("Bearer hf_secret");
        assert_eq!(format!("{auth:?}"), "Authorization(<redacted>)");
        assert_eq!(auth.expose(), "Bearer hf_secret");
        assert_eq!(
            Authorization::from_header(Some("Bearer hf_secret")),
            Some(auth)
        );
        assert_eq!(Authorization::from_header(Some("   ")), None);
        assert_eq!(Authorization::from_header(None), None);
    }

    #[test]
    fn only_the_upstream_saying_not_now_is_retryable() {
        for code in [408, 425, 429, 500, 502, 503, 504] {
            assert_eq!(
                classify(StatusCode::from_u16(code).expect("status")),
                Retryability::Retryable,
                "{code}"
            );
        }
        for code in [200, 206, 301, 401, 403, 404, 416] {
            assert_eq!(
                classify(StatusCode::from_u16(code).expect("status")),
                Retryability::Terminal,
                "{code}"
            );
        }
    }

    #[test]
    fn the_backoff_grows_and_stops_growing() {
        let policy = RetryPolicy {
            attempts: 5,
            backoff: Duration::from_millis(10),
        };
        assert_eq!(policy.delay(0), Duration::from_millis(10));
        assert_eq!(policy.delay(1), Duration::from_millis(20));
        assert_eq!(policy.delay(9), Duration::from_millis(160));
        assert_eq!(RetryPolicy::immediate(2).delay(3), Duration::ZERO);
    }

    #[tokio::test]
    async fn offline_never_opens_a_socket() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let client = UpstreamClient::new(true).expect("client builds");
        let target = url(&server.uri(), "/api/models/gpt2");
        let error = client
            .send(UpstreamRequest::get(&target))
            .await
            .expect_err("offline refuses");
        assert!(matches!(error, MirrorError::Offline(_)), "{error}");
    }

    #[tokio::test]
    async fn a_rate_limited_upstream_becomes_a_proxy_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(ResponseTemplate::new(429))
            .expect(3)
            .mount(&server)
            .await;

        let target = url(&server.uri(), "/limited");
        let error = client()
            .send(UpstreamRequest::get(&target))
            .await
            .expect_err("429 is not an answer");
        assert!(matches!(error, MirrorError::ProxyTimeout), "{error}");
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_a_not_found_is_not() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = client();
        let down = url(&server.uri(), "/down");
        assert!(matches!(
            client
                .send(UpstreamRequest::head(&down))
                .await
                .expect_err("503 is retried"),
            MirrorError::ProxyTimeout
        ));

        let gone = url(&server.uri(), "/gone");
        let response = client
            .send(UpstreamRequest::head(&gone))
            .await
            .expect("404 is an answer");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn credentials_range_and_encoding_reach_the_upstream() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .and(header("authorization", "Bearer hf_secret"))
            .and(header("range", "bytes=0-63"))
            .and(header("accept-encoding", IDENTITY_ENCODING))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(vec![7u8; 64]))
            .expect(1)
            .mount(&server)
            .await;

        let auth = Authorization::new("Bearer hf_secret");
        let target = url(&server.uri(), "/file");
        let response = client()
            .send(
                UpstreamRequest::get(&target)
                    .with_auth(Some(&auth))
                    .with_byte_range(0, 64)
                    .with_accept_encoding(IDENTITY_ENCODING),
            )
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.expect("body").len(), 64);
    }

    #[test]
    fn a_redirect_may_come_home_or_go_public_and_nowhere_else() {
        let origin = Url::parse("http://peer.lan:7099/file").expect("url parses");
        let previous = [origin.clone()];
        let allowed = [
            "http://peer.lan:7099/elsewhere",
            "https://cdn-lfs.huggingface.co/object",
            "https://[2606:4700::1111]/object",
            "https://93.184.216.34/object",
        ];
        for target in allowed {
            let target = Url::parse(target).expect("url parses");
            assert!(redirect_allowed(&target, &previous), "{target}");
        }
        let refused = [
            "http://127.0.0.1:9000/object",
            "http://169.254.169.254/latest/meta-data",
            "http://192.168.1.4:7099/object",
            "http://10.0.0.1/object",
            "http://[::1]:9000/object",
            "http://[fd00::1]/object",
        ];
        for target in refused {
            let target = Url::parse(target).expect("url parses");
            assert!(!redirect_allowed(&target, &previous), "{target}");
        }
        assert!(
            !redirect_allowed(&Url::parse("http://127.0.0.1/x").expect("url"), &[]),
            "a redirect with no origin to compare against is refused"
        );
    }

    #[tokio::test]
    async fn a_body_longer_than_its_cap_is_refused_rather_than_held() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4096]))
            .mount(&server)
            .await;

        let target = url(&server.uri(), "/big");
        let client = client();
        let response = client
            .send(UpstreamRequest::get(&target))
            .await
            .expect("request succeeds");
        let error = read_capped(response, 64, &target)
            .await
            .expect_err("a body past the cap is refused");
        assert!(matches!(error, MirrorError::ProxyInvalidData(_)), "{error}");

        let response = client
            .send(UpstreamRequest::get(&target))
            .await
            .expect("request succeeds");
        assert_eq!(
            read_capped(response, 4096, &target)
                .await
                .expect("a body within the cap is kept")
                .len(),
            4096
        );
    }

    #[tokio::test]
    async fn a_pass_through_keeps_the_status_headers_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/whoami-v2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-xet-hash", "deadbeef")
                    .insert_header("x-repo-commit", "abc")
                    .set_body_string("{\"name\":\"anon\"}"),
            )
            .mount(&server)
            .await;

        let target = url(&server.uri(), "/api/whoami-v2");
        let proxied = client()
            .pass_through(UpstreamRequest::get(&target))
            .await
            .expect("pass through succeeds");
        assert_eq!(proxied.status, 200);
        assert_eq!(proxied.header("x-xet-hash"), Some("deadbeef"));
        assert_eq!(proxied.body, Bytes::from_static(b"{\"name\":\"anon\"}"));

        let served = proxied.without_xet_headers();
        assert_eq!(served.header("x-xet-hash"), None);
        assert_eq!(served.header("x-repo-commit"), Some("abc"));
    }
}
