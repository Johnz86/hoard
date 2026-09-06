//! The compact index digest nodes exchange.
//!
//! A digest is one row per artifact a node holds, small enough that a peer can read a whole
//! index in a few pages. This module owns the wire shape and the cursor spelling; the records
//! behind both live in the catalog, reached only through a blocking task.

use std::sync::Arc;

use hoard_index::catalog::{Catalog, DigestEntry};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{Base, CommitSha, RepoId, Role, Sha256, State};

use crate::error::{NetError, Result};
use crate::peer::on_catalog;

/// Rows a digest request returns when it names no limit.
pub const DEFAULT_LIMIT: u32 = 100;
/// The most rows one digest page ever carries, whatever a peer asks for.
pub const MAX_LIMIT: u32 = 500;

/// The longest repository path a peer may name, matching what a filesystem accepts.
pub const MAX_PATH: usize = 1024;
/// The longest qualified repository name a peer may name.
pub const MAX_REPO: usize = 256;
/// The longest base family key a peer may name.
pub const MAX_BASE: usize = 64;

const CURSOR_PREFIX: &str = "d1";

pub use hoard_index::catalog::{DigestApplied, RemoteEntry};

/// One artifact as a peer learns of it, exactly the shape `GET /api/v1/peers/digest` names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestRow {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: Base,
    pub repo: String,
    pub revision_sha: Option<CommitSha>,
    pub path: String,
    pub state: State,
}

/// One page of a digest, with the cursor that continues it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DigestPage {
    pub rows: Vec<DigestRow>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

/// A monotonic position in a digest: every artifact at or below it has been sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DigestCursor(i64);

impl DigestCursor {
    pub const fn new(artifact_row: i64) -> Self {
        Self(artifact_row)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub fn encode(self) -> String {
        format!("{CURSOR_PREFIX}-{:x}", self.0)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let bound = raw
            .trim()
            .strip_prefix(CURSOR_PREFIX)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|hex| i64::from_str_radix(hex, 16).ok())
            .filter(|bound| *bound >= 0)
            .ok_or_else(|| NetError::BadCursor(raw.to_owned()))?;
        Ok(Self(bound))
    }

    /// The cursor a request carries, absent or empty meaning the start of the digest.
    pub fn from_param(raw: Option<&str>) -> Result<Option<Self>> {
        match raw.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => Self::parse(value).map(Some),
            None => Ok(None),
        }
    }
}

/// Reads one page of this node's digest.
///
/// `next_cursor` is where the next page starts, and it is present whenever a page carried
/// rows: a reader pages until it is handed an empty one, and keeps the last cursor so the
/// following poll receives only what has been added since.
pub async fn collect(
    catalog: &Arc<Catalog>,
    since: Option<DigestCursor>,
    limit: u32,
) -> Result<DigestPage> {
    let bound = since.map(DigestCursor::get);
    let limit = limit.clamp(1, MAX_LIMIT);
    let page = on_catalog(catalog, move |catalog| catalog.digest(bound, limit)).await?;
    Ok(DigestPage {
        rows: page.rows.into_iter().map(row_of).collect(),
        next_cursor: page.next.map(|bound| DigestCursor::new(bound).encode()),
        total: page.total,
    })
}

/// Files a peer's digest page in the local index as REMOTE entries owned by that peer.
///
/// Every row is validated first, so a page carrying one unreadable row is refused whole and
/// leaves the index exactly as it was.
pub async fn apply(
    catalog: &Arc<Catalog>,
    peer: &str,
    peer_row: i64,
    page: &DigestPage,
    pass: u64,
) -> Result<DigestApplied> {
    let rows = page
        .rows
        .iter()
        .map(|row| validate(peer, row))
        .collect::<Result<Vec<_>>>()?;
    on_catalog(catalog, move |catalog| {
        catalog.apply_digest(peer_row, &rows, pass)
    })
    .await
}

/// Drops the peer's rows the pass numbered `pass` did not re-confirm.
pub async fn retract(catalog: &Arc<Catalog>, peer_row: i64, pass: u64) -> Result<u64> {
    on_catalog(catalog, move |catalog| {
        catalog.retract_digest(peer_row, pass)
    })
    .await
}

/// The pass number a full re-read of a peer's digest stamps every row it confirms with.
pub async fn begin_pass(catalog: &Arc<Catalog>, peer_row: i64) -> Result<u64> {
    on_catalog(catalog, move |catalog| catalog.begin_pass(peer_row)).await
}

/// How many entries this node files for a peer.
pub async fn filed_for(catalog: &Arc<Catalog>, peer_row: i64) -> Result<u64> {
    on_catalog(catalog, move |catalog| catalog.filed_for(peer_row)).await
}

/// The cursor this node last consumed from a peer, restarting the digest if it is corrupt.
pub async fn read_cursor(catalog: &Arc<Catalog>, peer_row: i64) -> Result<Option<DigestCursor>> {
    let raw = on_catalog(catalog, move |catalog| catalog.peer_cursor(peer_row)).await?;
    Ok(raw
        .as_deref()
        .map(DigestCursor::parse)
        .transpose()
        .unwrap_or_default())
}

/// Records how far this node has read a peer's digest.
pub async fn write_cursor(
    catalog: &Arc<Catalog>,
    peer_row: i64,
    cursor: Option<DigestCursor>,
) -> Result<()> {
    let encoded = cursor.map(DigestCursor::encode);
    let at = Timestamp::now();
    on_catalog(catalog, move |catalog| {
        catalog.write_peer_cursor(peer_row, encoded.as_deref(), at)
    })
    .await
}

fn row_of(entry: DigestEntry) -> DigestRow {
    DigestRow {
        sha256: entry.sha256,
        size: entry.size,
        role: entry.role,
        base: entry.base,
        repo: entry.repo,
        revision_sha: entry.revision_sha,
        path: entry.path,
        state: entry.state,
    }
}

/// Reads one wire row, or says which peer sent something hoard cannot file.
pub fn validate(peer: &str, row: &DigestRow) -> Result<RemoteEntry> {
    let bad = |reason: String| NetError::BadDigest {
        peer: peer.to_owned(),
        reason,
    };
    let path = row.path.trim().trim_start_matches('/');
    if path.is_empty() || path.len() > MAX_PATH || traverses(path) {
        return Err(bad(format!("path {:?} is not a repository path", row.path)));
    }
    let named = row.repo.trim();
    if named.is_empty() || named.len() > MAX_REPO || traverses(named) {
        return Err(bad(format!("repo {:?} is not a repository", row.repo)));
    }
    if row.base.as_str().len() > MAX_BASE {
        return Err(bad(format!("base {:?} is not a family", row.base.as_str())));
    }
    let repo = RepoId::parse_qualified(named)
        .map_err(|error| bad(format!("repo {:?}: {error}", row.repo)))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    Ok(RemoteEntry {
        sha256: row.sha256,
        size: row.size,
        role: row.role,
        base: Base::new(row.base.as_str()),
        revision_sha: row
            .revision_sha
            .unwrap_or_else(|| synthetic_sha(&row.sha256)),
        name: name.to_owned(),
        path: path.to_owned(),
        repo,
    })
}

/// Whether a peer's path or repository name walks out of the directory it names, which is
/// how bytes a pull materializes would land outside the pull root.
fn traverses(value: &str) -> bool {
    value
        .split('/')
        .any(|segment| segment == ".." || segment == ".")
}

/// The revision a row without one is filed under: the leading bytes of its content hash, so
/// two nodes reporting the same bytes agree on the same synthetic revision.
fn synthetic_sha(sha256: &Sha256) -> CommitSha {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&sha256.as_bytes()[..20]);
    CommitSha::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use hoard_core::{NodeId, RepoType};
    use hoard_index::config::{HashPolicy, IndexConfig, SourceConfig, SourceKind};
    use hoard_index::row::SearchRow;
    use hoard_index::search::{SortKey, SortSpec};
    use tempfile::TempDir;

    use super::*;

    fn sha256(seed: u8) -> Sha256 {
        Sha256::from_bytes([seed; 32])
    }

    fn commit(seed: u8) -> CommitSha {
        CommitSha::from_bytes([seed; 20])
    }

    fn row(seed: u8, path: &str) -> DigestRow {
        DigestRow {
            sha256: sha256(seed),
            size: 1024 * u64::from(seed),
            role: Role::Lora,
            base: Base::new("sdxl"),
            repo: "models/kohya/film-grain".to_owned(),
            revision_sha: Some(commit(seed)),
            path: path.to_owned(),
            state: State::Served,
        }
    }

    /// A synthetic safetensors header whose one pair of tensors is named after `tensor`, so
    /// every seeded file hashes to something of its own.
    fn safetensors(tensor: &str) -> Vec<u8> {
        let header = format!(
            "{{\"lora_unet_{tensor}.lora_down.weight\":\
             {{\"dtype\":\"F16\",\"shape\":[16,768],\"data_offsets\":[0,24576]}},\
             \"lora_unet_{tensor}.lora_up.weight\":\
             {{\"dtype\":\"F16\",\"shape\":[768,16],\"data_offsets\":[24576,49152]}}}}"
        );
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        bytes
    }

    /// A catalog holding `count` hashed local artifacts, plus an empty peer node row.
    fn seeded(root: &Path, count: u8) -> (Arc<Catalog>, i64) {
        let input = root.join("files");
        fs::create_dir_all(&input).expect("input directory");
        for seed in 1..=count {
            fs::write(
                input.join(format!("weight-{seed}.safetensors")),
                safetensors(&format!("t{seed}")),
            )
            .expect("fixture writes");
        }
        let config = IndexConfig {
            data_dir: root.join("native-data"),
            hash_policy: HashPolicy::Now,
            sources: vec![SourceConfig {
                name: "files".to_owned(),
                path: input,
                kind: SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            }],
            ..IndexConfig::default()
        };
        let catalog =
            Catalog::open_or_scan(&config, "workstation", |_, _| {}).expect("catalog opens");
        let peer = catalog
            .upsert_peer(
                &NodeId::new("laptop-1").expect("id"),
                "laptop",
                Some("http://laptop:7099"),
            )
            .expect("peer records")
            .id;
        (Arc::new(catalog), peer)
    }

    /// The rows one query answers, with the search index caught up to what was just filed.
    fn rows_of(catalog: &Catalog, raw: &str) -> Vec<SearchRow> {
        catalog.refresh_search(|_| {}).expect("index refreshes");
        let context = catalog
            .compile_context(raw, Timestamp::now(), Vec::new())
            .expect("context compiles");
        catalog
            .query(hoard_index::catalog::QueryRequest {
                raw,
                sort: SortSpec::new(SortKey::default(), false),
                limit: 50,
                offset: 0,
                context: &context,
            })
            .expect("query answers")
            .rows
    }

    #[test]
    fn a_cursor_round_trips_and_refuses_anything_else() {
        let cursor = DigestCursor::new(4_294_967_296);
        assert_eq!(
            DigestCursor::parse(&cursor.encode()).expect("parses"),
            cursor
        );
        assert_eq!(cursor.encode(), "d1-100000000");
        assert!(DigestCursor::parse("").is_err());
        assert!(DigestCursor::parse("100").is_err());
        assert!(DigestCursor::parse("d1-").is_err());
        assert!(DigestCursor::parse("d1-zz").is_err());
        assert!(DigestCursor::parse("d2-10").is_err());
        assert!(matches!(
            DigestCursor::parse("nonsense"),
            Err(NetError::BadCursor(_))
        ));
        assert_eq!(DigestCursor::from_param(None).expect("absent"), None);
        assert_eq!(DigestCursor::from_param(Some("  ")).expect("blank"), None);
        assert_eq!(
            DigestCursor::from_param(Some("d1-a")).expect("present"),
            Some(DigestCursor::new(10))
        );
    }

    #[tokio::test]
    async fn paging_a_digest_skips_no_row_and_repeats_none() {
        let root = TempDir::new().expect("temp dir");
        let (catalog, _) = seeded(root.path(), 7);
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = collect(&catalog, cursor, 3).await.expect("page reads");
            assert_eq!(page.total, 7);
            if page.rows.is_empty() {
                assert_eq!(page.next_cursor, None);
                break;
            }
            seen.extend(page.rows.iter().map(|row| row.sha256));
            cursor = page
                .next_cursor
                .as_deref()
                .map(|raw| DigestCursor::parse(raw).expect("cursor parses"));
        }
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(seen.len(), 7);
        assert_eq!(unique.len(), 7);
    }

    #[tokio::test]
    async fn an_applied_digest_lands_as_remote_entries_owned_by_the_peer() {
        let root = TempDir::new().expect("temp dir");
        let (catalog, peer) = seeded(root.path(), 1);
        let page = DigestPage {
            rows: vec![row(20, "loras/film-grain.safetensors")],
            next_cursor: None,
            total: 1,
        };
        let pass = begin_pass(&catalog, peer).await.expect("pass starts");
        let applied = apply(&catalog, "laptop", peer, &page, pass)
            .await
            .expect("digest applies");
        assert_eq!(applied.entries, 1);
        assert_eq!(applied.artifacts, 1);

        let found = rows_of(&catalog, "node:laptop");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, State::Remote);
        assert_eq!(found[0].path, "loras/film-grain.safetensors");
        assert_eq!(found[0].blob_sha256, Some(sha256(20)));
    }

    #[tokio::test]
    async fn a_malformed_digest_names_the_peer_and_leaves_the_index_untouched() {
        let root = TempDir::new().expect("temp dir");
        let (catalog, peer) = seeded(root.path(), 1);
        let page = DigestPage {
            rows: vec![row(20, "loras/ok.safetensors"), row(21, "   ")],
            next_cursor: None,
            total: 2,
        };
        let pass = begin_pass(&catalog, peer).await.expect("pass starts");
        let error = apply(&catalog, "laptop", peer, &page, pass)
            .await
            .expect_err("a pathless row is refused");
        assert!(matches!(error, NetError::BadDigest { ref peer, .. } if peer == "laptop"));

        let mut escaping = row(22, "../../etc/passwd");
        escaping.repo = "models/kohya/film-grain".to_owned();
        assert!(matches!(
            validate("laptop", &escaping),
            Err(NetError::BadDigest { .. })
        ));

        let mut nameless = row(23, "a.safetensors");
        nameless.repo = "  ".to_owned();
        assert!(matches!(
            validate("laptop", &nameless),
            Err(NetError::BadDigest { .. })
        ));

        assert_eq!(filed_for(&catalog, peer).await.expect("count reads"), 0);
    }

    #[tokio::test]
    async fn a_repository_that_walks_out_of_the_pull_root_is_refused() {
        for named in [
            "models/../..",
            "models/kohya/..",
            "models/./film-grain",
            "../../etc",
        ] {
            let mut escaping = row(24, "a.safetensors");
            escaping.repo = named.to_owned();
            assert!(
                matches!(
                    validate("laptop", &escaping),
                    Err(NetError::BadDigest { .. })
                ),
                "repo {named:?} is refused"
            );
        }

        let root = TempDir::new().expect("temp dir");
        let (catalog, peer) = seeded(root.path(), 1);
        let mut escaping = row(25, "a.safetensors");
        escaping.repo = "models/../..".to_owned();
        let page = DigestPage {
            rows: vec![escaping],
            next_cursor: None,
            total: 1,
        };
        let pass = begin_pass(&catalog, peer).await.expect("pass starts");
        assert!(apply(&catalog, "laptop", peer, &page, pass).await.is_err());
        assert_eq!(filed_for(&catalog, peer).await.expect("count reads"), 0);
    }

    #[test]
    fn a_row_carrying_runaway_strings_is_refused_and_its_base_is_renormalized() {
        let mut long = row(26, &format!("loras/{}.safetensors", "a".repeat(MAX_PATH)));
        assert!(matches!(
            validate("laptop", &long),
            Err(NetError::BadDigest { .. })
        ));

        long = row(27, "loras/a.safetensors");
        long.repo = format!("models/kohya/{}", "a".repeat(MAX_REPO));
        assert!(matches!(
            validate("laptop", &long),
            Err(NetError::BadDigest { .. })
        ));

        let mut based = row(28, "loras/a.safetensors");
        based.base = serde_json::from_str(&format!("\"{}\"", "b".repeat(MAX_BASE + 1)))
            .expect("a wire base is any string");
        assert!(matches!(
            validate("laptop", &based),
            Err(NetError::BadDigest { .. })
        ));

        based.base = serde_json::from_str("\"SDXL 1.0\"").expect("a wire base is any string");
        assert_eq!(
            validate("laptop", &based).expect("row validates").base,
            Base::new("sdxl10")
        );
    }

    #[test]
    fn a_row_without_a_revision_is_filed_under_a_hash_derived_one() {
        let mut loose = row(40, "/vae/sdxl-vae.safetensors");
        loose.revision_sha = None;
        let entry = validate("laptop", &loose).expect("row validates");
        assert_eq!(entry.revision_sha, synthetic_sha(&sha256(40)));
        assert_eq!(entry.path, "vae/sdxl-vae.safetensors");
        assert_eq!(entry.name, "sdxl-vae.safetensors");
        assert_eq!(entry.repo.qualified(), "models/kohya/film-grain");
        assert_eq!(RepoType::Models, entry.repo.repo_type());
    }

    #[tokio::test]
    async fn a_peer_cursor_is_remembered_and_a_corrupt_one_restarts_the_digest() {
        let root = TempDir::new().expect("temp dir");
        let (catalog, peer) = seeded(root.path(), 1);
        assert_eq!(read_cursor(&catalog, peer).await.expect("no cursor"), None);

        write_cursor(&catalog, peer, Some(DigestCursor::new(12)))
            .await
            .expect("cursor writes");
        assert_eq!(
            read_cursor(&catalog, peer).await.expect("cursor reads"),
            Some(DigestCursor::new(12))
        );

        catalog
            .write_peer_cursor(peer, Some("rubbish"), Timestamp::now())
            .expect("cursor corrupts");
        assert_eq!(
            read_cursor(&catalog, peer).await.expect("cursor reads"),
            None
        );
    }

    #[test]
    fn a_digest_page_round_trips_as_json() {
        let page = DigestPage {
            rows: vec![row(3, "loras/a.safetensors")],
            next_cursor: Some(DigestCursor::new(3).encode()),
            total: 9,
        };
        let text = serde_json::to_string(&page).expect("page serializes");
        assert_eq!(
            serde_json::from_str::<DigestPage>(&text).expect("page parses"),
            page
        );
        assert!(serde_json::from_str::<DigestPage>("{\"rows\":[{}]}").is_err());
    }
}
