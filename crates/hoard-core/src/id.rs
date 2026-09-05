use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::HoardError;

/// The three repository namespaces the HuggingFace Hub API exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoType {
    Models,
    Datasets,
    Spaces,
}

impl RepoType {
    pub const ALL: [RepoType; 3] = [RepoType::Models, RepoType::Datasets, RepoType::Spaces];

    /// The plural spelling used in Hub route segments (`/api/models/...`).
    pub const fn as_str(self) -> &'static str {
        match self {
            RepoType::Models => "models",
            RepoType::Datasets => "datasets",
            RepoType::Spaces => "spaces",
        }
    }

    /// The singular spelling used in HF cache directory names (`models--org--name`).
    pub const fn singular(self) -> &'static str {
        match self {
            RepoType::Models => "model",
            RepoType::Datasets => "dataset",
            RepoType::Spaces => "space",
        }
    }
}

impl fmt::Display for RepoType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RepoType {
    type Err = HoardError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "models" | "model" => Ok(RepoType::Models),
            "datasets" | "dataset" => Ok(RepoType::Datasets),
            "spaces" | "space" => Ok(RepoType::Spaces),
            other => Err(HoardError::UnknownRepoType(other.to_owned())),
        }
    }
}

/// `type/org/name`, the identity of a repository in the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId {
    repo_type: RepoType,
    org: Option<String>,
    name: String,
}

impl RepoId {
    pub fn new(repo_type: RepoType, org: Option<String>, name: impl Into<String>) -> Self {
        Self {
            repo_type,
            org: org.filter(|o| !o.is_empty()),
            name: name.into(),
        }
    }

    /// Parses `org/name` or a bare `name` in the given namespace.
    ///
    /// A segment of `.` or `..` is refused. A repo identity is joined onto a directory in
    /// several places -- a projection root, a peer's pull root, the cache layout -- so a
    /// relative segment here becomes a path escape there.
    pub fn parse_ref(repo_type: RepoType, reference: &str) -> Result<Self, HoardError> {
        let trimmed = reference.trim_matches('/');
        if trimmed.is_empty() {
            return Err(HoardError::InvalidRepoRef(reference.to_owned()));
        }
        let mut parts = trimmed.splitn(2, '/');
        let first = parts.next().unwrap_or_default();
        let segments = match parts.next() {
            Some(name) if !name.is_empty() && !name.contains('/') => (first, Some(name)),
            Some(_) => return Err(HoardError::InvalidRepoRef(reference.to_owned())),
            None => (first, None),
        };
        if is_relative_segment(segments.0) || segments.1.is_some_and(is_relative_segment) {
            return Err(HoardError::InvalidRepoRef(reference.to_owned()));
        }
        match segments.1 {
            Some(name) => Ok(Self::new(repo_type, Some(segments.0.to_owned()), name)),
            None => Ok(Self::new(repo_type, None, segments.0)),
        }
    }

    /// Parses a fully qualified `type/org/name`, falling back to `models` when the first
    /// segment is not a repo type.
    pub fn parse_qualified(value: &str) -> Result<Self, HoardError> {
        let trimmed = value.trim_matches('/');
        match trimmed.split_once('/') {
            Some((head, rest)) => match RepoType::from_str(head) {
                Ok(repo_type) => Self::parse_ref(repo_type, rest),
                Err(_) => Self::parse_ref(RepoType::Models, trimmed),
            },
            None => Self::parse_ref(RepoType::Models, trimmed),
        }
    }

    pub fn repo_type(&self) -> RepoType {
        self.repo_type
    }

    pub fn org(&self) -> Option<&str> {
        self.org.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `org/name`, or just `name` when the repo has no organization.
    pub fn reference(&self) -> String {
        match &self.org {
            Some(org) => format!("{org}/{}", self.name),
            None => self.name.clone(),
        }
    }

    /// `type/org/name`, the canonical index key.
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.repo_type, self.reference())
    }

    /// The HF cache directory spelling, `models--org--name`.
    pub fn cache_dir_name(&self) -> String {
        let reference = self.reference().replace('/', "--");
        format!("{}s--{reference}", self.repo_type.singular())
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.qualified())
    }
}

impl Serialize for RepoId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.qualified())
    }
}

impl<'de> Deserialize<'de> for RepoId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        RepoId::parse_qualified(&raw).map_err(serde::de::Error::custom)
    }
}

fn is_relative_segment(segment: &str) -> bool {
    segment == "." || segment == ".."
}

/// A 40 hex character git object name. Synthetic revisions of loose folders use the
/// first 20 bytes of a content hash so every revision has the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitSha([u8; 20]);

impl CommitSha {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        crate::hex_encode(&self.0)
    }

    /// The seven character form the UI shows next to a ref.
    pub fn short(self) -> String {
        self.to_hex()[..7].to_owned()
    }
}

/// A 32 byte content hash; the LFS oid and the `etag` of large files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256([u8; 32]);

impl Sha256 {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        crate::hex_encode(&self.0)
    }

    pub fn short(self) -> String {
        let hex = self.to_hex();
        format!("{}...{}", &hex[..4], &hex[hex.len() - 4..])
    }
}

macro_rules! hex_newtype {
    ($ty:ty, $len:expr, $label:expr) => {
        impl FromStr for $ty {
            type Err = HoardError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let mut bytes = [0u8; $len];
                crate::hex_decode(value, &mut bytes).map_err(|_| HoardError::InvalidHash {
                    kind: $label,
                    value: value.to_owned(),
                })?;
                Ok(Self(bytes))
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&crate::hex_encode(&self.0))
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&crate::hex_encode(&self.0))
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

hex_newtype!(CommitSha, 20, "commit sha");
hex_newtype!(Sha256, 32, "sha256");

/// A git blob name, `sha1("blob <len>\0" + bytes)`, used as the Hub `oid` of small files.
pub type GitSha1 = CommitSha;

macro_rules! row_id {
    ($ty:ident, $prefix:literal) => {
        #[doc = concat!("Surrogate index identity rendered as `", $prefix, "<rowid>` in JSON.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $ty(i64);

        impl $ty {
            pub const fn new(raw: i64) -> Self {
                Self(raw)
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }

        impl FromStr for $ty {
            type Err = HoardError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value
                    .strip_prefix($prefix)
                    .unwrap_or(value)
                    .parse()
                    .map(Self)
                    .map_err(|_| HoardError::InvalidId(value.to_owned()))
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;

                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $ty;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("an index id as a string or integer")
                    }

                    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<$ty, E> {
                        value.parse().map_err(E::custom)
                    }

                    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<$ty, E> {
                        Ok($ty(value))
                    }

                    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<$ty, E> {
                        Ok($ty(value as i64))
                    }
                }

                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

row_id!(BlobId, "b_");
row_id!(ArtifactId, "a_");
row_id!(RepoRowId, "r_");
row_id!(RevisionId, "v_");
row_id!(EntryId, "e_");
row_id!(SourceId, "s_");
row_id!(ShelfId, "sh_");
row_id!(LoadoutId, "ld_");
row_id!(JobId, "job_");

/// A stable per-machine identity, 32 hex characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// Accepts a non-empty identity of ASCII alphanumeric characters and hyphens only.
    pub fn new(raw: impl Into<String>) -> Result<Self, HoardError> {
        let raw = raw.into();
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(HoardError::InvalidId(raw));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_forms_parse() {
        let with_org = RepoId::parse_ref(RepoType::Models, "org/name").expect("org/name parses");
        assert_eq!(with_org.org(), Some("org"));
        assert_eq!(with_org.qualified(), "models/org/name");
        assert_eq!(with_org.cache_dir_name(), "models--org--name");

        let bare = RepoId::parse_ref(RepoType::Models, "gpt2").expect("bare name parses");
        assert_eq!(bare.org(), None);
        assert_eq!(bare.reference(), "gpt2");
        assert_eq!(bare.cache_dir_name(), "models--gpt2");

        assert!(RepoId::parse_ref(RepoType::Models, "a/b/c").is_err());
        for escape in ["..", "../..", "models/..", "../name", "org/..", "."] {
            assert!(
                RepoId::parse_ref(RepoType::Models, escape).is_err(),
                "{escape} would join onto a directory as a step out of it"
            );
        }
        assert!(RepoId::parse_ref(RepoType::Models, "").is_err());
    }

    #[test]
    fn qualified_falls_back_to_models() {
        let dataset = RepoId::parse_qualified("datasets/org/set").expect("qualified parses");
        assert_eq!(dataset.repo_type(), RepoType::Datasets);
        let model = RepoId::parse_qualified("org/name").expect("two segments parse");
        assert_eq!(model.repo_type(), RepoType::Models);
        assert_eq!(model.org(), Some("org"));
    }

    #[test]
    fn hashes_round_trip_hex() {
        let hex = "462c3cb0f1a2b3c4d5e6f708192a3b4c5d6e7f80";
        let sha: CommitSha = hex.parse().expect("commit sha parses");
        assert_eq!(sha.to_hex(), hex);
        assert_eq!(sha.short(), "462c3cb");
        assert!("nothex".parse::<CommitSha>().is_err());

        let digest = "9f2c000000000000000000000000000000000000000000000000000000ee41ab";
        let sha256: Sha256 = digest.parse().expect("sha256 parses");
        assert_eq!(sha256.to_hex(), digest);
        assert_eq!(sha256.short(), "9f2c...41ab");
    }

    #[test]
    fn row_ids_render_with_prefix() {
        let id = ArtifactId::new(42);
        assert_eq!(id.to_string(), "a_42");
        assert_eq!(
            "a_42".parse::<ArtifactId>().expect("prefixed id parses"),
            id
        );
        assert_eq!("42".parse::<ArtifactId>().expect("bare id parses"), id);
        assert_eq!(
            serde_json::to_string(&id).expect("id serializes"),
            "\"a_42\""
        );
    }
}
