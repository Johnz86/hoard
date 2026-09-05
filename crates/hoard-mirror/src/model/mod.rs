//! The JSON bodies of the Hub API, spelled exactly as `huggingface_hub` expects them.
//!
//! Field names are the Hub's, camel case included; a renamed field makes a real client fail
//! to parse the response, so every deviation here is a bug rather than a style choice.

use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{MirrorError, Result};

const HUB_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S.%6fZ";

/// A Hub timestamp: UTC, `Z` suffixed, always six fractional digits, as in
/// `2026-01-17T10:22:31.000000Z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HubTimestamp(Timestamp);

impl HubTimestamp {
    pub const fn new(timestamp: Timestamp) -> Self {
        Self(timestamp)
    }

    pub fn now() -> Self {
        Self(Timestamp::now())
    }

    /// The instant a git commit or a file modification time names.
    pub fn from_unix_seconds(seconds: i64) -> Result<Self> {
        Timestamp::from_second(seconds)
            .map(Self)
            .map_err(|error| MirrorError::ProxyInvalidData(error.to_string()))
    }

    pub const fn timestamp(self) -> Timestamp {
        self.0
    }

    pub fn as_second(self) -> i64 {
        self.0.as_second()
    }
}

impl fmt::Display for HubTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.strftime(HUB_TIMESTAMP_FORMAT).to_string())
    }
}

impl FromStr for HubTimestamp {
    type Err = MirrorError;

    fn from_str(value: &str) -> Result<Self> {
        Timestamp::from_str(value)
            .map(Self)
            .map_err(|error| MirrorError::ProxyInvalidData(format!("{value}: {error}")))
    }
}

impl Serialize for HubTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HubTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_str(&raw).map_err(D::Error::custom)
    }
}

/// One file of a repo as `/api/{repo_type}/{ref}` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sibling {
    pub rfilename: String,
}

impl Sibling {
    pub fn new(rfilename: impl Into<String>) -> Self {
        Self {
            rfilename: rfilename.into(),
        }
    }
}

/// The body of `/api/{repo_type}/{ref}` and `/api/{repo_type}/{ref}/revision/{revision}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    #[serde(rename = "_id")]
    pub internal_id: String,
    pub id: String,
    pub author: Option<String>,
    pub sha: String,
    #[serde(rename = "lastModified")]
    pub last_modified: HubTimestamp,
    pub private: bool,
    pub gated: bool,
    pub disabled: bool,
    pub tags: Vec<String>,
    pub description: String,
    pub paperswithcode_id: Option<String>,
    pub downloads: u64,
    pub likes: u64,
    #[serde(rename = "cardData")]
    pub card_data: Option<serde_json::Value>,
    pub siblings: Vec<Sibling>,
    #[serde(rename = "createdAt")]
    pub created_at: HubTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeEntryKind {
    File,
    Directory,
}

/// The LFS pointer of a tree entry: the sha256 of the blob, its size and the size of the
/// pointer file that stands in for it in git.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfsInfo {
    pub oid: String,
    pub size: u64,
    #[serde(rename = "pointerSize")]
    pub pointer_size: u64,
}

/// The commit `expand=true` attaches to a tree entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCommit {
    pub id: String,
    pub title: String,
    pub date: HubTimestamp,
}

/// The scan report `expand=true` attaches to a tree entry. Hoard reports the constant
/// "nothing found" shape, since it scans nothing on behalf of upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityInfo {
    #[serde(rename = "blobId")]
    pub blob_id: String,
    pub safe: bool,
    #[serde(rename = "avScan")]
    pub av_scan: AvScan,
    #[serde(rename = "pickleImportScan")]
    pub pickle_import_scan: PickleImportScan,
}

impl SecurityInfo {
    pub fn clean(blob_id: impl Into<String>) -> Self {
        Self {
            blob_id: blob_id.into(),
            safe: true,
            av_scan: AvScan {
                virus_found: false,
                virus_names: None,
            },
            pickle_import_scan: PickleImportScan {
                highest_safety_level: "innocuous".to_owned(),
                imports: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvScan {
    #[serde(rename = "virusFound")]
    pub virus_found: bool,
    #[serde(rename = "virusNames")]
    pub virus_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickleImportScan {
    #[serde(rename = "highestSafetyLevel")]
    pub highest_safety_level: String,
    pub imports: Vec<serde_json::Value>,
}

/// One element of the `/api/{repo_type}/{ref}/tree/{revision}/{path}` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeEntry {
    #[serde(rename = "type")]
    pub kind: TreeEntryKind,
    pub oid: String,
    pub size: u64,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs: Option<LfsInfo>,
    #[serde(
        default,
        rename = "lastCommit",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_commit: Option<LastCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityInfo>,
}

impl TreeEntry {
    pub fn directory(oid: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            kind: TreeEntryKind::Directory,
            oid: oid.into(),
            size: 0,
            path: path.into(),
            lfs: None,
            last_commit: None,
            security: None,
        }
    }

    pub fn file(oid: impl Into<String>, size: u64, path: impl Into<String>) -> Self {
        Self {
            kind: TreeEntryKind::File,
            oid: oid.into(),
            size,
            path: path.into(),
            lfs: None,
            last_commit: None,
            security: None,
        }
    }

    pub fn with_lfs(mut self, lfs: LfsInfo) -> Self {
        self.lfs = Some(lfs);
        self
    }
}

/// One element of the `/api/{repo_type}/{ref}/paths-info/{revision}` array: a tree entry
/// plus the Xet content hash when the file is stored in Xet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathsInfoEntry {
    #[serde(flatten)]
    pub entry: TreeEntry,
    #[serde(default, rename = "xetHash", skip_serializing_if = "Option::is_none")]
    pub xet_hash: Option<String>,
}

impl PathsInfoEntry {
    pub fn new(entry: TreeEntry) -> Self {
        Self {
            entry,
            xet_hash: None,
        }
    }
}

/// The author of a commit. The Hub always carries an `avatar` key, null when it has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitAuthor {
    pub name: String,
    pub avatar: Option<String>,
}

impl CommitAuthor {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            avatar: None,
        }
    }
}

/// One element of the `/api/{repo_type}/{ref}/commits/{revision}` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEntry {
    pub id: String,
    pub title: String,
    pub message: String,
    pub authors: Vec<CommitAuthor>,
    pub date: HubTimestamp,
}

/// The `x-error-code` values of the error contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubErrorCode {
    RepoNotFound,
    EntryNotFound,
    RevisionNotFound,
    ProxyTimeout,
    ProxyInvalidData,
}

impl HubErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            HubErrorCode::RepoNotFound => "RepoNotFound",
            HubErrorCode::EntryNotFound => "EntryNotFound",
            HubErrorCode::RevisionNotFound => "RevisionNotFound",
            HubErrorCode::ProxyTimeout => "ProxyTimeout",
            HubErrorCode::ProxyInvalidData => "ProxyInvalidData",
        }
    }
}

impl fmt::Display for HubErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The JSON body of an error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

impl ErrorBody {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

/// One row of the error contract: the status, the `x-error-code` header and the body, which
/// some rows leave empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubError {
    pub status: u16,
    pub code: HubErrorCode,
    pub body: Option<ErrorBody>,
}

impl HubError {
    /// A repo that is unknown or that the rules deny. 401 so `huggingface_hub` raises
    /// `RepositoryNotFoundError`.
    pub fn repo_not_found() -> Self {
        Self {
            status: 401,
            code: HubErrorCode::RepoNotFound,
            body: Some(ErrorBody::new("Repository not found")),
        }
    }

    /// A bad `repo_type` or an unknown route.
    pub fn page_not_found() -> Self {
        Self {
            status: 404,
            code: HubErrorCode::RepoNotFound,
            body: Some(ErrorBody::new(
                "Sorry, we can't find the page you are looking for.",
            )),
        }
    }

    /// A path that is not in the revision.
    pub fn entry_not_found() -> Self {
        Self {
            status: 404,
            code: HubErrorCode::EntryNotFound,
            body: None,
        }
    }

    pub fn revision_not_found(revision: &str) -> Self {
        Self {
            status: 404,
            code: HubErrorCode::RevisionNotFound,
            body: Some(ErrorBody::new(format!("Invalid rev id: {revision}"))),
        }
    }

    /// An upstream that timed out, refused or rate limited the request.
    pub fn proxy_timeout() -> Self {
        Self {
            status: 504,
            code: HubErrorCode::ProxyTimeout,
            body: None,
        }
    }

    /// An upstream response that does not match the protocol.
    pub fn proxy_invalid_data() -> Self {
        Self {
            status: 504,
            code: HubErrorCode::ProxyInvalidData,
            body: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp() -> HubTimestamp {
        "2026-01-17T10:22:31Z".parse().expect("timestamp parses")
    }

    #[test]
    fn a_timestamp_renders_with_six_fractional_digits_and_a_zulu_suffix() {
        assert_eq!(timestamp().to_string(), "2026-01-17T10:22:31.000000Z");
        assert_eq!(
            serde_json::to_string(&timestamp()).expect("serializes"),
            "\"2026-01-17T10:22:31.000000Z\""
        );

        let fractional: HubTimestamp = "2026-01-17T10:22:31.5Z".parse().expect("parses");
        assert_eq!(fractional.to_string(), "2026-01-17T10:22:31.500000Z");

        let offset: HubTimestamp = "2026-01-17T12:22:31+02:00".parse().expect("parses");
        assert_eq!(offset.to_string(), "2026-01-17T10:22:31.000000Z");

        let nanos: HubTimestamp = "2026-01-17T10:22:31.123456789Z".parse().expect("parses");
        assert_eq!(nanos.to_string(), "2026-01-17T10:22:31.123456Z");

        assert_eq!(
            HubTimestamp::from_unix_seconds(1_768_645_351).expect("epoch seconds"),
            timestamp()
        );
        assert!("not a time".parse::<HubTimestamp>().is_err());
    }

    #[test]
    fn a_timestamp_round_trips_through_json() {
        let json = serde_json::to_string(&timestamp()).expect("serializes");
        let parsed: HubTimestamp = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(parsed, timestamp());
    }

    #[test]
    fn repo_metadata_serializes_with_the_hub_field_names() {
        let info = RepoInfo {
            internal_id: "621ffdc036468d709f17434d".to_owned(),
            id: "openai/whisper-large-v3".to_owned(),
            author: Some("openai".to_owned()),
            sha: "1a2b3c".to_owned(),
            last_modified: timestamp(),
            private: false,
            gated: false,
            disabled: false,
            tags: vec!["automatic-speech-recognition".to_owned()],
            description: "A speech model.".to_owned(),
            paperswithcode_id: None,
            downloads: 0,
            likes: 0,
            card_data: Some(serde_json::json!({"license": "apache-2.0"})),
            siblings: vec![Sibling::new("model.safetensors")],
            created_at: timestamp(),
        };

        let expected = r#"{"_id":"621ffdc036468d709f17434d","id":"openai/whisper-large-v3","author":"openai","sha":"1a2b3c","lastModified":"2026-01-17T10:22:31.000000Z","private":false,"gated":false,"disabled":false,"tags":["automatic-speech-recognition"],"description":"A speech model.","paperswithcode_id":null,"downloads":0,"likes":0,"cardData":{"license":"apache-2.0"},"siblings":[{"rfilename":"model.safetensors"}],"createdAt":"2026-01-17T10:22:31.000000Z"}"#;
        assert_eq!(serde_json::to_string(&info).expect("serializes"), expected);
        assert_eq!(
            serde_json::from_str::<RepoInfo>(expected).expect("deserializes"),
            info
        );
    }

    #[test]
    fn tree_entries_serialize_as_the_hub_spells_them() {
        let directory = TreeEntry::directory("aaa", "unet");
        assert_eq!(
            serde_json::to_string(&directory).expect("serializes"),
            r#"{"type":"directory","oid":"aaa","size":0,"path":"unet"}"#
        );

        let file = TreeEntry::file(
            "bbb",
            4_265_146_304,
            "unet/diffusion_pytorch_model.safetensors",
        )
        .with_lfs(LfsInfo {
            oid: "ccc".to_owned(),
            size: 4_265_146_304,
            pointer_size: 135,
        });
        assert_eq!(
            serde_json::to_string(&file).expect("serializes"),
            r#"{"type":"file","oid":"bbb","size":4265146304,"path":"unet/diffusion_pytorch_model.safetensors","lfs":{"oid":"ccc","size":4265146304,"pointerSize":135}}"#
        );
    }

    #[test]
    fn an_expanded_tree_entry_carries_the_last_commit_and_the_scan_report() {
        let mut entry = TreeEntry::file("bbb", 12, "config.json");
        entry.last_commit = Some(LastCommit {
            id: "1a2b3c".to_owned(),
            title: "Upload config".to_owned(),
            date: timestamp(),
        });
        entry.security = Some(SecurityInfo::clean("bbb"));

        assert_eq!(
            serde_json::to_string(&entry).expect("serializes"),
            r#"{"type":"file","oid":"bbb","size":12,"path":"config.json","lastCommit":{"id":"1a2b3c","title":"Upload config","date":"2026-01-17T10:22:31.000000Z"},"security":{"blobId":"bbb","safe":true,"avScan":{"virusFound":false,"virusNames":null},"pickleImportScan":{"highestSafetyLevel":"innocuous","imports":[]}}}"#
        );
    }

    #[test]
    fn a_paths_info_entry_flattens_the_tree_entry_and_adds_the_xet_hash() {
        let mut entry = PathsInfoEntry::new(TreeEntry::file("bbb", 12, "config.json"));
        assert_eq!(
            serde_json::to_string(&entry).expect("serializes"),
            r#"{"type":"file","oid":"bbb","size":12,"path":"config.json"}"#
        );

        entry.xet_hash = Some("ddd".to_owned());
        assert_eq!(
            serde_json::to_string(&entry).expect("serializes"),
            r#"{"type":"file","oid":"bbb","size":12,"path":"config.json","xetHash":"ddd"}"#
        );
    }

    #[test]
    fn a_commit_entry_matches_the_documented_shape() {
        let commit = CommitEntry {
            id: "1a2b3c".to_owned(),
            title: "Initial commit".to_owned(),
            message: String::new(),
            authors: vec![CommitAuthor::new("openai")],
            date: timestamp(),
        };
        assert_eq!(
            serde_json::to_string(&commit).expect("serializes"),
            r#"{"id":"1a2b3c","title":"Initial commit","message":"","authors":[{"name":"openai","avatar":null}],"date":"2026-01-17T10:22:31.000000Z"}"#
        );
    }

    #[test]
    fn the_error_contract_rows_carry_their_status_code_and_body() {
        let repo = HubError::repo_not_found();
        assert_eq!((repo.status, repo.code.as_str()), (401, "RepoNotFound"));
        assert_eq!(
            serde_json::to_string(&repo.body).expect("serializes"),
            r#"{"error":"Repository not found"}"#
        );

        let page = HubError::page_not_found();
        assert_eq!((page.status, page.code.as_str()), (404, "RepoNotFound"));
        assert_eq!(
            serde_json::to_string(&page.body).expect("serializes"),
            r#"{"error":"Sorry, we can't find the page you are looking for."}"#
        );

        let entry = HubError::entry_not_found();
        assert_eq!((entry.status, entry.code.as_str()), (404, "EntryNotFound"));
        assert!(entry.body.is_none());

        let revision = HubError::revision_not_found("nope");
        assert_eq!(
            (revision.status, revision.code.as_str()),
            (404, "RevisionNotFound")
        );
        assert_eq!(
            serde_json::to_string(&revision.body).expect("serializes"),
            r#"{"error":"Invalid rev id: nope"}"#
        );

        assert_eq!(
            (
                HubError::proxy_timeout().status,
                HubError::proxy_timeout().code.as_str()
            ),
            (504, "ProxyTimeout")
        );
        assert_eq!(
            (
                HubError::proxy_invalid_data().status,
                HubError::proxy_invalid_data().code.as_str()
            ),
            (504, "ProxyInvalidData")
        );
    }
}
