//! The Hub API spoken outwards: visibility probes, revision lookups, file metadata and the
//! ranged block fetches that fill the cache.
//!
//! One [`Hub`] is one base URL. `huggingface.co` and a peer hoard node are the same type,
//! which is what makes a peer pull an ordinary block-cached resolve rather than a second
//! implementation.

use std::ops::Range;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use hoard_core::{CommitSha, RepoId, RepoType};
use reqwest::StatusCode;
use reqwest::header::{
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue,
};
use serde::Deserialize;
use url::Url;

use crate::cache::{BlockLease, CacheEntry};
use crate::error::{MirrorError, Result};
use crate::model::HubTimestamp;
use crate::upstream::client::{
    Authorization, IDENTITY_ENCODING, ProxyResponse, UpstreamClient, UpstreamRequest,
};
use crate::upstream::meta::MetaRequest;

/// The header carrying the commit a resolve answered from.
pub const HEADER_REPO_COMMIT: &str = "x-repo-commit";

/// The header carrying the sha256 of the media an LFS pointer stands for.
pub const HEADER_LINKED_ETAG: &str = "x-linked-etag";

/// The header carrying the size of the media an LFS pointer stands for.
pub const HEADER_LINKED_SIZE: &str = "x-linked-size";

/// The route `hf auth whoami` calls.
pub const WHOAMI_PATH: &str = "api/whoami-v2";

/// The Xet content route `hf_xet` follows once it has a read token.
pub const XET_BRIDGE_PATH: &str = "xet-bridge-us";

/// How often a running block download refreshes its lock so a slow but healthy transfer is
/// never mistaken for a dead one.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// The revision document of `/api/{repo_type}/{ref}/revision/{revision}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRevision {
    pub sha: CommitSha,
    pub last_modified: Option<HubTimestamp>,
    /// The body exactly as upstream sent it, for the metadata cache on disk.
    pub body: Bytes,
}

#[derive(Debug, Deserialize)]
struct RevisionDocument {
    sha: String,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<HubTimestamp>,
}

/// What a resolve HEAD says about a file, read in one upstream request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub size: u64,
    pub etag: Option<String>,
    pub linked_etag: Option<String>,
    pub linked_size: Option<u64>,
    pub commit: Option<CommitSha>,
    pub content_type: Option<String>,
}

impl RemoteFile {
    /// The identity the cache entry is keyed on: the content hash when upstream reported
    /// one, otherwise whatever it called the file.
    pub fn cache_etag(&self) -> Option<&str> {
        self.linked_etag.as_deref().or(self.etag.as_deref())
    }
}

/// Which block of which file to fetch.
#[derive(Debug, Clone, Copy)]
pub struct BlockFetch<'a> {
    pub url: &'a Url,
    pub auth: Option<&'a Authorization>,
    pub index: u64,
}

/// An upstream that speaks the Hub API.
#[derive(Debug, Clone)]
pub struct Hub {
    client: UpstreamClient,
    base: Url,
}

impl Hub {
    pub fn new(client: UpstreamClient, base: &str) -> Result<Self> {
        let base = Url::parse(base).map_err(|error| MirrorError::Config(error.to_string()))?;
        if base.cannot_be_a_base() {
            return Err(MirrorError::Config(format!("upstream {base} has no path")));
        }
        Ok(Self { client, base })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn client(&self) -> &UpstreamClient {
        &self.client
    }

    pub const fn is_offline(&self) -> bool {
        self.client.is_offline()
    }

    /// `{base}/api/{repo_type}/{ref}{suffix}`.
    pub fn api_url(&self, repo: &RepoId, suffix: &str) -> Result<Url> {
        self.url(&format!(
            "api/{}/{}{suffix}",
            repo.repo_type().as_str(),
            repo.reference()
        ))
    }

    /// `{base}/{repo_type}/{ref}/resolve/{revision}/{path}`, with the repo type omitted for
    /// models exactly as the Hub spells it.
    pub fn resolve_url(&self, repo: &RepoId, revision: &str, path: &str) -> Result<Url> {
        let prefix = match repo.repo_type() {
            RepoType::Models => String::new(),
            other => format!("{}/", other.as_str()),
        };
        self.url(&format!(
            "{prefix}{}/resolve/{revision}/{path}",
            repo.reference()
        ))
    }

    fn url(&self, suffix: &str) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| MirrorError::Config(format!("upstream {} has no path", self.base)))?;
            segments.pop_if_empty();
            segments.extend(suffix.split('/').filter(|part| !part.is_empty()));
        }
        Ok(url)
    }

    /// Whether the upstream admits this repository exists and the caller may see it.
    ///
    /// A retryable answer never reaches here as `false`: it surfaces as `ProxyTimeout` so a
    /// rate limit is not reported to a client as a missing repository.
    pub async fn probe_repo(&self, repo: &RepoId, auth: Option<&Authorization>) -> Result<bool> {
        let url = self.api_url(repo, "")?;
        let response = self
            .client
            .send(UpstreamRequest::head(&url).with_auth(auth))
            .await?;
        Ok(response.status().is_success())
    }

    /// The commit a revision names, or `None` when the upstream says there is no such
    /// revision.
    pub async fn revision(
        &self,
        repo: &RepoId,
        revision: &str,
        auth: Option<&Authorization>,
    ) -> Result<Option<UpstreamRevision>> {
        let url = self.api_url(repo, &format!("/revision/{revision}"))?;
        let response = self
            .client
            .send(
                UpstreamRequest::get(&url)
                    .with_auth(auth)
                    .with_accept_encoding(IDENTITY_ENCODING),
            )
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let body = response.bytes().await?;
        let document: RevisionDocument = serde_json::from_slice(&body)
            .map_err(|error| MirrorError::ProxyInvalidData(error.to_string()))?;
        let sha = document
            .sha
            .parse::<CommitSha>()
            .map_err(|error| MirrorError::ProxyInvalidData(error.to_string()))?;
        Ok(Some(UpstreamRevision {
            sha,
            last_modified: document.last_modified,
            body,
        }))
    }

    /// Everything a resolve answer needs about a file, in one HEAD.
    pub async fn file_metadata(
        &self,
        repo: &RepoId,
        revision: &str,
        path: &str,
        auth: Option<&Authorization>,
    ) -> Result<RemoteFile> {
        let url = self.resolve_url(repo, revision, path)?;
        let response = self
            .client
            .send(UpstreamRequest::head(&url).with_auth(auth))
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(metadata_error(status, repo, path));
        }
        let headers = response.headers();
        let linked_size = header_u64(headers, HEADER_LINKED_SIZE);
        let size = header_u64(headers, CONTENT_LENGTH.as_str())
            .or(linked_size)
            .ok_or_else(|| {
                MirrorError::ProxyInvalidData(format!("{url} answered without a size"))
            })?;
        Ok(RemoteFile {
            size,
            etag: header_text(headers, ETAG.as_str()).map(quoted),
            linked_etag: header_text(headers, HEADER_LINKED_ETAG).map(quoted),
            linked_size,
            commit: header_text(headers, HEADER_REPO_COMMIT)
                .and_then(|value| value.parse::<CommitSha>().ok()),
            content_type: header_text(headers, CONTENT_TYPE.as_str()),
        })
    }

    /// Downloads `range` without touching the cache, for a repository the rules let hoard
    /// proxy but not persist.
    pub async fn download_range(
        &self,
        url: &Url,
        auth: Option<&Authorization>,
        range: Range<u64>,
    ) -> Result<Bytes> {
        let expected = range.end.saturating_sub(range.start);
        let response = self
            .client
            .send(
                UpstreamRequest::get(url)
                    .with_auth(auth)
                    .with_byte_range(range.start, range.end)
                    .with_accept_encoding(IDENTITY_ENCODING),
            )
            .await?;
        if !response.status().is_success() {
            return Err(MirrorError::ProxyInvalidData(format!(
                "{url} answered {} for a range request",
                response.status()
            )));
        }
        reject_transfer_encoding(response.headers(), url)?;
        let body = response.bytes().await?;
        if body.len() as u64 != expected {
            return Err(MirrorError::ProxyInvalidData(format!(
                "{url} returned {} bytes for a {expected} byte range",
                body.len()
            )));
        }
        Ok(body)
    }

    /// Makes one block of a cache entry present, downloading it exactly once across every
    /// task and process that wants it.
    ///
    /// The caller that wins the lease streams the block from upstream, refreshing the lock
    /// as bytes arrive; every other caller waits and then reads what the winner published.
    pub async fn fill_block(&self, entry: &mut CacheEntry, fetch: BlockFetch<'_>) -> Result<()> {
        let range = entry.meta().block_range(fetch.index);
        if range.end <= range.start {
            return Ok(());
        }
        let guard = match entry.acquire_block(fetch.index).await? {
            BlockLease::Present => return Ok(()),
            BlockLease::Download(guard) => guard,
        };
        let expected = range.end - range.start;
        let response = self
            .client
            .send(
                UpstreamRequest::get(fetch.url)
                    .with_auth(fetch.auth)
                    .with_byte_range(range.start, range.end)
                    .with_accept_encoding(IDENTITY_ENCODING),
            )
            .await?;
        if !response.status().is_success() {
            return Err(MirrorError::ProxyInvalidData(format!(
                "{} answered {} for block {}",
                fetch.url,
                response.status(),
                fetch.index
            )));
        }
        reject_transfer_encoding(response.headers(), fetch.url)?;

        let mut response = response;
        let mut block = BytesMut::with_capacity(usize::try_from(expected).unwrap_or_default());
        let mut beat = Instant::now();
        while let Some(chunk) = response.chunk().await? {
            block.extend_from_slice(&chunk);
            if beat.elapsed() >= HEARTBEAT_INTERVAL {
                guard.heartbeat();
                beat = Instant::now();
            }
        }
        if block.len() as u64 != expected {
            return Err(MirrorError::ProxyInvalidData(format!(
                "block {} of {} is {} bytes, expected {expected}",
                fetch.index,
                fetch.url,
                block.len()
            )));
        }
        entry.write_block(fetch.index, &block).await?;
        Ok(())
    }

    /// Forwards a request and its answer unchanged, for `whoami-v2`, `xet-read-token` and
    /// the Xet bridge.
    pub async fn pass_through(
        &self,
        suffix: &str,
        auth: Option<&Authorization>,
    ) -> Result<ProxyResponse> {
        let url = self.url(suffix)?;
        self.client
            .pass_through(UpstreamRequest::get(&url).with_auth(auth))
            .await
    }

    /// Forwards one metadata request and returns the answer verbatim.
    ///
    /// Exactly one upstream call: the route the client asked for, carrying its query and
    /// its body, without the visibility probe olah sent first.
    pub async fn metadata(&self, request: &MetaRequest<'_>) -> Result<ProxyResponse> {
        let url = self.api_url(request.repo, &request.kind.suffix())?;
        let mut upstream =
            UpstreamRequest::new(request.kind.method(), &url).with_auth(request.auth);
        if let Some(query) = request.kind.query() {
            upstream = upstream.with_query(query);
        }
        if let Some(body) = request.kind.body()? {
            upstream = upstream.with_body(body);
        }
        self.client.pass_through(upstream).await
    }

    /// `GET /api/whoami-v2`, so `hf auth whoami` answers through the mirror.
    pub async fn whoami(&self, auth: Option<&Authorization>) -> Result<ProxyResponse> {
        self.pass_through(WHOAMI_PATH, auth).await
    }

    /// `GET /api/{repo_type}/{ref}/xet-read-token/{revision}`, which is how `hf_xet`
    /// refreshes its content access token.
    pub async fn xet_read_token(
        &self,
        repo: &RepoId,
        revision: &str,
        auth: Option<&Authorization>,
    ) -> Result<ProxyResponse> {
        self.pass_through(
            &format!(
                "api/{}/{}/xet-read-token/{revision}",
                repo.repo_type().as_str(),
                repo.reference()
            ),
            auth,
        )
        .await
    }

    /// `GET /xet-bridge-us/{repo_hash}/{hash}`, the content route `hf_xet` follows.
    pub async fn xet_bridge(
        &self,
        repo_hash: &str,
        hash: &str,
        auth: Option<&Authorization>,
    ) -> Result<ProxyResponse> {
        self.pass_through(&format!("{XET_BRIDGE_PATH}/{repo_hash}/{hash}"), auth)
            .await
    }
}

/// The identity an offline mirror reports, so `hf auth whoami` answers instead of hanging.
pub fn anonymous_whoami() -> ProxyResponse {
    let body = serde_json::json!({
        "type": "user",
        "name": "anonymous",
        "fullname": "anonymous",
        "email": null,
        "emailVerified": false,
        "canPay": false,
        "isPro": false,
        "avatarUrl": null,
        "orgs": [],
        "auth": { "type": "none", "accessToken": null },
    });
    ProxyResponse {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: Bytes::from(body.to_string()),
    }
}

fn metadata_error(status: StatusCode, repo: &RepoId, path: &str) -> MirrorError {
    match status.as_u16() {
        401 | 403 => MirrorError::RepoNotFound(repo.reference()),
        404 => MirrorError::EntryNotFound(path.to_owned()),
        code => MirrorError::ProxyInvalidData(format!("resolve answered {code}")),
    }
}

fn reject_transfer_encoding(headers: &HeaderMap, url: &Url) -> Result<()> {
    let encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or(IDENTITY_ENCODING);
    if encoding.is_empty() || encoding.eq_ignore_ascii_case(IDENTITY_ENCODING) {
        return Ok(());
    }
    Err(MirrorError::ProxyInvalidData(format!(
        "{url} answered with content-encoding {encoding}"
    )))
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| HeaderValue::to_str(value).ok())
        .and_then(|value| value.trim().parse().ok())
}

/// An etag as the contract requires it: quoted, and quoted only once.
pub fn quoted(value: impl AsRef<str>) -> String {
    let value = value.as_ref().trim();
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    format!("\"{inner}\"")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::cache::BlockCache;
    use crate::config::{Geometry, MirrorConfig};
    use crate::upstream::client::RetryPolicy;

    const SHA: &str = "9f1a2b3c4d5e6f708192a3b4c5d6e7f809a1b2c3";

    fn hub(base: &str) -> Hub {
        let client = UpstreamClient::new(false)
            .expect("client builds")
            .with_retry(RetryPolicy::immediate(2));
        Hub::new(client, base).expect("hub builds")
    }

    fn repo() -> RepoId {
        RepoId::new(RepoType::Models, Some("openai".to_owned()), "whisper")
    }

    fn cache(root: &TempDir) -> BlockCache {
        let config = MirrorConfig {
            geometry: Geometry::new(64, 16).expect("valid geometry"),
            ..MirrorConfig::default()
        };
        BlockCache::at_root(Arc::new(config), root.path())
    }

    #[test]
    fn urls_follow_the_hub_spelling_for_every_repo_type() {
        let hub = hub("https://huggingface.co");
        assert_eq!(
            hub.api_url(&repo(), "").expect("url").as_str(),
            "https://huggingface.co/api/models/openai/whisper"
        );
        assert_eq!(
            hub.api_url(&repo(), "/revision/main")
                .expect("url")
                .as_str(),
            "https://huggingface.co/api/models/openai/whisper/revision/main"
        );
        assert_eq!(
            hub.resolve_url(&repo(), "main", "unet/model.safetensors")
                .expect("url")
                .as_str(),
            "https://huggingface.co/openai/whisper/resolve/main/unet/model.safetensors"
        );

        let dataset = RepoId::new(RepoType::Datasets, None, "squad");
        assert_eq!(
            hub.resolve_url(&dataset, "main", "train.json")
                .expect("url")
                .as_str(),
            "https://huggingface.co/datasets/squad/resolve/main/train.json"
        );
    }

    #[test]
    fn a_base_url_with_a_path_prefix_keeps_it() {
        let hub = hub("http://peer.lan:7099/mirror");
        assert_eq!(
            hub.api_url(&repo(), "").expect("url").as_str(),
            "http://peer.lan:7099/mirror/api/models/openai/whisper"
        );
    }

    #[test]
    fn an_etag_is_quoted_exactly_once() {
        assert_eq!(quoted("abc"), "\"abc\"");
        assert_eq!(quoted("\"abc\""), "\"abc\"");
        assert_eq!(quoted(" \"abc\" "), "\"abc\"");
    }

    #[tokio::test]
    async fn the_visibility_probe_separates_denial_from_rate_limiting() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path_matcher("/api/models/openai/whisper"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        assert!(
            hub(&server.uri())
                .probe_repo(&repo(), None)
                .await
                .expect("probe answers")
        );

        let denied = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&denied)
            .await;
        assert!(
            !hub(&denied.uri())
                .probe_repo(&repo(), None)
                .await
                .expect("probe answers")
        );

        let limited = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&limited)
            .await;
        assert!(matches!(
            hub(&limited.uri())
                .probe_repo(&repo(), None)
                .await
                .expect_err("a rate limit is not an answer"),
            MirrorError::ProxyTimeout
        ));
    }

    #[tokio::test]
    async fn a_revision_lookup_reads_the_sha_and_keeps_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/models/openai/whisper/revision/main"))
            .and(header("authorization", "Bearer hf_secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": SHA,
                "lastModified": "2026-01-17T10:22:31.000Z",
            })))
            .mount(&server)
            .await;

        let auth = Authorization::new("Bearer hf_secret");
        let found = hub(&server.uri())
            .revision(&repo(), "main", Some(&auth))
            .await
            .expect("lookup succeeds")
            .expect("revision exists");
        assert_eq!(found.sha.to_hex(), SHA);
        assert_eq!(
            found.last_modified.map(|value| value.to_string()),
            Some("2026-01-17T10:22:31.000000Z".to_owned())
        );
        assert!(found.body.starts_with(b"{"));
    }

    #[tokio::test]
    async fn an_unknown_revision_is_reported_as_absent_and_garbage_as_invalid() {
        let missing = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&missing)
            .await;
        assert_eq!(
            hub(&missing.uri())
                .revision(&repo(), "nope", None)
                .await
                .expect("lookup answers"),
            None
        );

        let garbage = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&garbage)
            .await;
        assert!(matches!(
            hub(&garbage.uri())
                .revision(&repo(), "main", None)
                .await
                .expect_err("garbage is rejected"),
            MirrorError::ProxyInvalidData(_)
        ));
    }

    #[tokio::test]
    async fn file_metadata_reads_the_lfs_headers_in_one_request() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path_matcher(
                "/openai/whisper/resolve/main/model.safetensors",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-length", "4096")
                    .insert_header("etag", "\"abc123\"")
                    .insert_header(HEADER_LINKED_ETAG, "def456")
                    .insert_header(HEADER_LINKED_SIZE, "4096")
                    .insert_header(HEADER_REPO_COMMIT, SHA)
                    .insert_header("content-type", "application/octet-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let found = hub(&server.uri())
            .file_metadata(&repo(), "main", "model.safetensors", None)
            .await
            .expect("metadata reads");
        assert_eq!(found.size, 4096);
        assert_eq!(found.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(found.linked_etag.as_deref(), Some("\"def456\""));
        assert_eq!(found.linked_size, Some(4096));
        assert_eq!(found.commit.map(CommitSha::to_hex), Some(SHA.to_owned()));
        assert_eq!(found.cache_etag(), Some("\"def456\""));
    }

    #[tokio::test]
    async fn a_missing_file_and_a_denied_repo_keep_their_shapes() {
        let missing = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&missing)
            .await;
        assert!(matches!(
            hub(&missing.uri())
                .file_metadata(&repo(), "main", "gone.bin", None)
                .await
                .expect_err("missing file"),
            MirrorError::EntryNotFound(_)
        ));

        let denied = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&denied)
            .await;
        assert!(matches!(
            hub(&denied.uri())
                .file_metadata(&repo(), "main", "model.bin", None)
                .await
                .expect_err("denied repo"),
            MirrorError::RepoNotFound(_)
        ));
    }

    #[tokio::test]
    async fn a_block_is_fetched_by_range_and_published_once() {
        let server = MockServer::start().await;
        let body: Vec<u8> = (0u8..64).collect();
        Mock::given(method("GET"))
            .and(path_matcher("/file"))
            .and(header("range", "bytes=64-127"))
            .and(header("accept-encoding", IDENTITY_ENCODING))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(body.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let sha = SHA.parse::<CommitSha>().expect("sha parses");
        let mut entry = cache
            .file_entry(&repo(), &sha, "model.bin", 129, Some(b"\"etag\""))
            .await
            .expect("entry opens");

        let hub = hub(&server.uri());
        let url = hub.url("file").expect("url");
        let fetch = BlockFetch {
            url: &url,
            auth: None,
            index: 1,
        };
        hub.fill_block(&mut entry, fetch)
            .await
            .expect("block fetches");
        assert!(entry.has_block(1));
        assert_eq!(entry.read_range(64..128).await.expect("reads"), body);

        hub.fill_block(&mut entry, fetch)
            .await
            .expect("a present block is not fetched again");
    }

    #[tokio::test]
    async fn a_short_or_compressed_block_is_rejected() {
        let short = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(vec![0u8; 8]))
            .mount(&short)
            .await;

        let root = TempDir::new().expect("temp dir");
        let cache = cache(&root);
        let sha = SHA.parse::<CommitSha>().expect("sha parses");
        let mut entry = cache
            .file_entry(&repo(), &sha, "model.bin", 64, None)
            .await
            .expect("entry opens");
        let short_hub = hub(&short.uri());
        let url = short_hub.url("file").expect("url");
        let error = short_hub
            .fill_block(
                &mut entry,
                BlockFetch {
                    url: &url,
                    auth: None,
                    index: 0,
                },
            )
            .await
            .expect_err("a short block is not written");
        assert!(matches!(error, MirrorError::ProxyInvalidData(_)), "{error}");
        assert!(!entry.has_block(0));

        let compressed = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(vec![0u8; 64]),
            )
            .mount(&compressed)
            .await;
        let compressed_hub = hub(&compressed.uri());
        let url = compressed_hub.url("file").expect("url");
        assert!(matches!(
            compressed_hub
                .fill_block(
                    &mut entry,
                    BlockFetch {
                        url: &url,
                        auth: None,
                        index: 0,
                    },
                )
                .await
                .expect_err("a compressed block is not a block"),
            MirrorError::ProxyInvalidData(_)
        ));
    }

    #[tokio::test]
    async fn a_range_can_be_downloaded_without_a_cache_entry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("range", "bytes=10-19"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(vec![3u8; 10]))
            .mount(&server)
            .await;
        let hub = hub(&server.uri());
        let url = hub.url("file").expect("url");
        assert_eq!(
            hub.download_range(&url, None, 10..20)
                .await
                .expect("range downloads"),
            Bytes::from(vec![3u8; 10])
        );
    }

    #[tokio::test]
    async fn offline_never_asks_the_network_anything() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let hub = Hub::new(
            UpstreamClient::new(true).expect("client builds"),
            &server.uri(),
        )
        .expect("hub builds");
        assert!(hub.is_offline());
        assert!(hub.probe_repo(&repo(), None).await.is_err());
        assert!(hub.revision(&repo(), "main", None).await.is_err());
        assert!(
            hub.file_metadata(&repo(), "main", "model.bin", None)
                .await
                .is_err()
        );
    }
}
