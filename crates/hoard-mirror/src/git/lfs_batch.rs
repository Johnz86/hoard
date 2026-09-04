//! The git LFS v1 batch API. Download hrefs point back at the Hub `resolve` route so a
//! `git lfs` fetch and an `hf download` of the same file take the same path through hoard.

use std::collections::BTreeMap;

use hoard_core::{CommitSha, RepoId, RepoType, Sha256};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};

/// `content-type` of both the batch request and the batch response.
pub const CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
/// Message for an object this node does not hold.
pub const OBJECT_MISSING: &str = "Object does not exist";
/// Message for a write attempt against a read-only mirror.
pub const UPLOAD_REFUSED: &str = "this mirror is read-only: upload is not supported";

const MISSING_CODE: u16 = 404;
const REFUSED_CODE: u16 = 403;
const PATH_ESCAPES: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// The batch operation a client asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchOperation {
    Download,
    Upload,
}

/// The only transfer adapter hoard offers: plain HTTP GETs of the href.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferAdapter {
    #[default]
    Basic,
}

/// The ref a client is fetching for, carried through untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRef {
    pub name: String,
}

/// One object of a batch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchObject {
    pub oid: Sha256,
    pub size: u64,
}

/// `POST /{org}/{repo}.git/info/lfs/objects/batch` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    pub operation: BatchOperation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transfers: Vec<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<BatchRef>,
    pub objects: Vec<BatchObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_algo: Option<String>,
}

/// One href a client should GET.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchAction {
    pub href: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
}

/// The actions offered for an object; hoard only ever offers `download`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchActions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<BatchAction>,
}

/// The per-object failure form of the batch response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchObjectError {
    pub code: u16,
    pub message: String,
}

/// One object of a batch response, carrying either actions or an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponseObject {
    pub oid: Sha256,
    pub size: u64,
    pub authenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<BatchActions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchObjectError>,
}

/// The batch response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    pub transfer: TransferAdapter,
    pub objects: Vec<BatchResponseObject>,
}

/// Where an LFS object lives in Hub terms: the repo, the revision and the path whose
/// `resolve` route serves those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsLocation {
    pub repo: RepoId,
    pub revision: CommitSha,
    pub path: String,
}

impl LfsLocation {
    pub fn new(repo: RepoId, revision: CommitSha, path: impl Into<String>) -> Self {
        Self {
            repo,
            revision,
            path: path.into(),
        }
    }

    /// `{base}/[{repo_type}/]{org}/{name}/resolve/{revision}/{path}`, the Hub route.
    pub fn resolve_url(&self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        let namespace = match self.repo.repo_type() {
            RepoType::Models => String::new(),
            repo_type => format!("{repo_type}/"),
        };
        let path = utf8_percent_encode(self.path.trim_start_matches('/'), PATH_ESCAPES);
        format!(
            "{base}/{namespace}{}/resolve/{}/{path}",
            self.repo.reference(),
            self.revision.to_hex()
        )
    }
}

/// Answers a batch request: every held object gets a `download` action pointing at the Hub
/// `resolve` route, every unheld object gets the 404 error form, and an `upload` batch is
/// refused object by object.
pub fn build_batch_response(
    request: &BatchRequest,
    base_url: &str,
    resolver: &dyn Fn(&Sha256) -> Option<LfsLocation>,
) -> BatchResponse {
    let objects = request
        .objects
        .iter()
        .map(|object| match request.operation {
            BatchOperation::Upload => failed(object, REFUSED_CODE, UPLOAD_REFUSED),
            BatchOperation::Download => match resolver(&object.oid) {
                Some(location) => downloadable(object, location.resolve_url(base_url)),
                None => failed(object, MISSING_CODE, OBJECT_MISSING),
            },
        })
        .collect();
    BatchResponse {
        transfer: TransferAdapter::Basic,
        objects,
    }
}

fn downloadable(object: &BatchObject, href: String) -> BatchResponseObject {
    BatchResponseObject {
        oid: object.oid,
        size: object.size,
        authenticated: true,
        actions: Some(BatchActions {
            download: Some(BatchAction {
                href,
                header: BTreeMap::new(),
                expires_in: None,
            }),
        }),
        error: None,
    }
}

fn failed(object: &BatchObject, code: u16, message: &str) -> BatchResponseObject {
    BatchResponseObject {
        oid: object.oid,
        size: object.size,
        authenticated: true,
        actions: None,
        error: Some(BatchObjectError {
            code,
            message: message.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HELD: &str = "1c2d3e4f50617283940a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60";
    const ABSENT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const REVISION: &str = "462c3cb0f1a2b3c4d5e6f708192a3b4c5d6e7f80";

    fn sha256(hex: &str) -> Sha256 {
        hex.parse().expect("sha256 parses")
    }

    fn location(repo_type: RepoType, path: &str) -> LfsLocation {
        LfsLocation::new(
            RepoId::parse_ref(repo_type, "org/name").expect("repo parses"),
            REVISION.parse().expect("revision parses"),
            path,
        )
    }

    fn request(operation: BatchOperation, oids: &[&str]) -> BatchRequest {
        BatchRequest {
            operation,
            transfers: vec!["basic".to_owned()],
            reference: Some(BatchRef {
                name: "refs/heads/main".to_owned(),
            }),
            objects: oids
                .iter()
                .map(|oid| BatchObject {
                    oid: sha256(oid),
                    size: 4096,
                })
                .collect(),
            hash_algo: Some("sha256".to_owned()),
        }
    }

    fn resolver(oid: &Sha256) -> Option<LfsLocation> {
        (oid.to_hex() == HELD).then(|| location(RepoType::Models, "unet/model.safetensors"))
    }

    #[test]
    fn batch_request_parses_the_spec_body() {
        let body = json!({
            "operation": "download",
            "transfers": ["basic"],
            "ref": {"name": "refs/heads/main"},
            "objects": [{"oid": HELD, "size": 4096}],
            "hash_algo": "sha256"
        });
        let parsed: BatchRequest = serde_json::from_value(body).expect("request parses");
        assert_eq!(parsed, request(BatchOperation::Download, &[HELD]));
    }

    #[test]
    fn download_hrefs_point_at_the_resolve_route() {
        let response = build_batch_response(
            &request(BatchOperation::Download, &[HELD]),
            "http://hoard.lan:7099/",
            &resolver,
        );
        assert_eq!(
            serde_json::to_value(&response).expect("response serializes"),
            json!({
                "transfer": "basic",
                "objects": [{
                    "oid": HELD,
                    "size": 4096,
                    "authenticated": true,
                    "actions": {
                        "download": {
                            "href": format!(
                                "http://hoard.lan:7099/org/name/resolve/{REVISION}/unet/model.safetensors"
                            )
                        }
                    }
                }]
            })
        );
    }

    #[test]
    fn missing_objects_use_the_error_form() {
        let response = build_batch_response(
            &request(BatchOperation::Download, &[ABSENT]),
            "http://hoard.lan:7099",
            &resolver,
        );
        assert_eq!(
            serde_json::to_value(&response).expect("response serializes"),
            json!({
                "transfer": "basic",
                "objects": [{
                    "oid": ABSENT,
                    "size": 4096,
                    "authenticated": true,
                    "error": {"code": 404, "message": OBJECT_MISSING}
                }]
            })
        );
    }

    #[test]
    fn upload_is_refused_for_every_object() {
        let response = build_batch_response(
            &request(BatchOperation::Upload, &[HELD, ABSENT]),
            "http://hoard.lan:7099",
            &resolver,
        );
        assert_eq!(response.objects.len(), 2);
        for object in &response.objects {
            assert!(object.actions.is_none());
            assert_eq!(
                object.error,
                Some(BatchObjectError {
                    code: 403,
                    message: UPLOAD_REFUSED.to_owned()
                })
            );
        }
    }

    #[test]
    fn dataset_and_space_hrefs_carry_their_namespace() {
        let base = "http://hoard.lan:7099";
        assert_eq!(
            location(RepoType::Datasets, "train/shard 0.parquet").resolve_url(base),
            format!("{base}/datasets/org/name/resolve/{REVISION}/train/shard%200.parquet")
        );
        assert_eq!(
            location(RepoType::Spaces, "app.py").resolve_url(base),
            format!("{base}/spaces/org/name/resolve/{REVISION}/app.py")
        );
    }
}
