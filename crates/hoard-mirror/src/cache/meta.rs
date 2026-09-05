//! The metadata half of the cache: the `{status_code, headers}` envelope and the `.body`
//! sidecar every cached Hub API document is stored as.
//!
//! The format is olah's, so a `repos/` tree written by the reference implementation is read
//! back unchanged, including the legacy documents that hex-encoded the body inline.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::cache::layout::body_path;
use crate::error::Result;

/// The status a cached document must carry to be served again.
pub const CACHEABLE_STATUS: u16 = 200;

const JSON_CONTENT_TYPE: &str = "application/json";
const CONTENT_TYPE: &str = "content-type";

/// The JSON document beside every cached API body: what the upstream answered, minus the
/// bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaEnvelope {
    pub status_code: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The body of a document written before the sidecar split, hex encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl MetaEnvelope {
    /// The envelope of an answer hoard renders itself, which is always JSON.
    pub fn json(status_code: u16) -> Self {
        Self {
            status_code,
            headers: BTreeMap::from([(CONTENT_TYPE.to_owned(), JSON_CONTENT_TYPE.to_owned())]),
            content: None,
        }
    }

    /// The envelope of an upstream answer, with its headers lower-cased into a map.
    pub fn upstream(status_code: u16, headers: &[(String, String)]) -> Self {
        Self {
            status_code,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
                .collect(),
            content: None,
        }
    }

    /// The headers as a proxy answer carries them.
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}

/// A cached API response read back off disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMeta {
    pub envelope: MetaEnvelope,
    pub body: Bytes,
    /// How long ago the document was written, so a caller can require it to be fresh.
    pub age: Duration,
}

/// Writes one document and its body sidecar, both atomically.
pub async fn write_meta_document(path: &Path, envelope: &MetaEnvelope, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    write_atomic(path, &serde_json::to_vec(envelope)?).await?;
    write_atomic(&body_path(path), body).await
}

/// Reads one document, or `None` when it is absent or unreadable.
pub async fn read_meta_document(path: &Path) -> Result<Option<CachedMeta>> {
    let Some(raw) = read_optional(path).await? else {
        return Ok(None);
    };
    let Ok(envelope) = serde_json::from_slice::<MetaEnvelope>(&raw) else {
        return Ok(None);
    };
    let body = match read_optional(&body_path(path)).await? {
        Some(body) => Bytes::from(body),
        None => match envelope.content.as_deref().map(hex::decode) {
            Some(Ok(decoded)) => Bytes::from(decoded),
            Some(Err(_)) | None => return Ok(None),
        },
    };
    Ok(Some(CachedMeta {
        envelope,
        body,
        age: age_of(path).await,
    }))
}

/// Reads several documents as the one array answer they were split from, for the routes
/// whose cache layout stores one document per path.
///
/// Any document missing, stale or not holding a JSON array makes the whole answer a miss:
/// a partial answer would tell a client a path does not exist.
pub async fn read_meta_array(
    paths: &[PathBuf],
    max_age: Option<Duration>,
) -> Result<Option<CachedMeta>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut entries = Vec::with_capacity(paths.len());
    let mut age = Duration::ZERO;
    for path in paths {
        let Some(cached) = fresh(read_meta_document(path).await?, max_age) else {
            return Ok(None);
        };
        let Ok(Value::Array(found)) = serde_json::from_slice::<Value>(&cached.body) else {
            return Ok(None);
        };
        entries.extend(found);
        age = age.max(cached.age);
    }
    Ok(Some(CachedMeta {
        envelope: MetaEnvelope::json(CACHEABLE_STATUS),
        body: Bytes::from(serde_json::to_vec(&Value::Array(entries))?),
        age,
    }))
}

/// The document if it answered 200 and is no older than `max_age`, otherwise nothing.
pub fn fresh(cached: Option<CachedMeta>, max_age: Option<Duration>) -> Option<CachedMeta> {
    cached
        .filter(|cached| cached.envelope.status_code == CACHEABLE_STATUS)
        .filter(|cached| max_age.is_none_or(|limit| cached.age < limit))
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = temp_path(path);
    let mut file = fs::File::create(&temp).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    fs::rename(&temp, path).await?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

async fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn age_of(path: &Path) -> Duration {
    let Ok(metadata) = fs::metadata(path).await else {
        return Duration::ZERO;
    };
    metadata
        .modified()
        .ok()
        .and_then(|written| SystemTime::now().duration_since(written).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn headers() -> Vec<(String, String)> {
        vec![
            ("Content-Type".to_owned(), JSON_CONTENT_TYPE.to_owned()),
            ("x-repo-commit".to_owned(), "abc".to_owned()),
        ]
    }

    #[tokio::test]
    async fn a_document_round_trips_through_its_envelope_and_sidecar() {
        let root = TempDir::new().expect("temp dir");
        let path = root.path().join("api/models/gpt2/revision/x/meta_get.json");
        write_meta_document(
            &path,
            &MetaEnvelope::upstream(200, &headers()),
            b"{\"sha\":\"a\"}",
        )
        .await
        .expect("document writes");

        let raw: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("json reads")).expect("json");
        assert_eq!(
            raw,
            serde_json::json!({
                "status_code": 200,
                "headers": { "content-type": JSON_CONTENT_TYPE, "x-repo-commit": "abc" },
            })
        );
        assert_eq!(
            std::fs::read(body_path(&path)).expect("body reads"),
            b"{\"sha\":\"a\"}"
        );

        let cached = read_meta_document(&path)
            .await
            .expect("document reads")
            .expect("document exists");
        assert_eq!(cached.envelope.status_code, 200);
        assert_eq!(cached.body, Bytes::from_static(b"{\"sha\":\"a\"}"));
        assert_eq!(
            cached.envelope.header_pairs(),
            vec![
                ("content-type".to_owned(), JSON_CONTENT_TYPE.to_owned()),
                ("x-repo-commit".to_owned(), "abc".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_legacy_olah_document_with_an_inline_body_is_still_read() {
        let root = TempDir::new().expect("temp dir");
        let path = root.path().join("meta_get.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "status_code": 200,
                "headers": { "content-type": JSON_CONTENT_TYPE },
                "content": hex::encode(b"[]"),
            }))
            .expect("json"),
        )
        .expect("legacy document writes");

        let cached = read_meta_document(&path)
            .await
            .expect("document reads")
            .expect("document exists");
        assert_eq!(cached.body, Bytes::from_static(b"[]"));
    }

    #[tokio::test]
    async fn a_missing_or_broken_document_is_a_miss() {
        let root = TempDir::new().expect("temp dir");
        let missing = root.path().join("absent.json");
        assert_eq!(read_meta_document(&missing).await.expect("reads"), None);

        let broken = root.path().join("broken.json");
        std::fs::write(&broken, b"not json").expect("writes");
        assert_eq!(read_meta_document(&broken).await.expect("reads"), None);

        let bodiless = root.path().join("bodiless.json");
        std::fs::write(&bodiless, b"{\"status_code\":200,\"headers\":{}}").expect("writes");
        assert_eq!(read_meta_document(&bodiless).await.expect("reads"), None);
    }

    #[tokio::test]
    async fn an_array_answer_is_merged_from_one_document_per_path() {
        let root = TempDir::new().expect("temp dir");
        let first = root.path().join("a/paths-info_post.json");
        let second = root.path().join("b/paths-info_post.json");
        write_meta_document(&first, &MetaEnvelope::json(200), b"[{\"path\":\"a\"}]")
            .await
            .expect("writes");
        write_meta_document(&second, &MetaEnvelope::json(200), b"[{\"path\":\"b\"}]")
            .await
            .expect("writes");

        let merged = read_meta_array(&[first.clone(), second], None)
            .await
            .expect("reads")
            .expect("both documents exist");
        assert_eq!(
            serde_json::from_slice::<Value>(&merged.body).expect("json"),
            serde_json::json!([{ "path": "a" }, { "path": "b" }])
        );

        let absent = root.path().join("c/paths-info_post.json");
        assert_eq!(
            read_meta_array(&[first, absent], None)
                .await
                .expect("reads"),
            None
        );
        assert_eq!(read_meta_array(&[], None).await.expect("reads"), None);
    }

    #[tokio::test]
    async fn freshness_rejects_an_aged_or_failed_document() {
        let root = TempDir::new().expect("temp dir");
        let path = root.path().join("meta_get.json");
        write_meta_document(&path, &MetaEnvelope::json(404), b"{}")
            .await
            .expect("writes");
        let failed = read_meta_document(&path).await.expect("reads");
        assert_eq!(fresh(failed, None), None);

        write_meta_document(&path, &MetaEnvelope::json(200), b"{}")
            .await
            .expect("writes");
        let cached = read_meta_document(&path).await.expect("reads");
        assert!(fresh(cached.clone(), Some(Duration::from_secs(60))).is_some());
        assert_eq!(fresh(cached, Some(Duration::ZERO)).map(|_| ()), None);
    }
}
