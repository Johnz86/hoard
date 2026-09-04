//! The compact index digest nodes exchange.
//!
//! A digest is one row per artifact a node holds, small enough that a peer can read a whole
//! index in a few pages. This module owns the wire shape and the cursor spelling; the SQL
//! behind both lives in `hoard_index::store::digest`, reached only through the store's
//! blocking calls.

use hoard_index::store::Store;
use hoard_index::store::digest as store_digest;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use hoard_core::{Base, CommitSha, RepoId, Role, Sha256, State};

use crate::error::{NetError, Result};

/// Rows a digest request returns when it names no limit.
pub const DEFAULT_LIMIT: u32 = 100;
/// The most rows one digest page ever carries, whatever a peer asks for.
pub const MAX_LIMIT: u32 = 500;

const CURSOR_PREFIX: &str = "d1";

pub use hoard_index::store::digest::{DigestApplied, RemoteEntry};

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
pub async fn collect(store: &Store, since: Option<DigestCursor>, limit: u32) -> Result<DigestPage> {
    let bound = since.map(DigestCursor::get);
    let limit = limit.clamp(1, MAX_LIMIT);
    let page = store
        .call(move |conn| store_digest::collect(conn, bound, limit))
        .await?;
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
    store: &Store,
    peer: &str,
    peer_row: i64,
    page: &DigestPage,
) -> Result<DigestApplied> {
    let rows = page
        .rows
        .iter()
        .map(|row| validate(peer, row))
        .collect::<Result<Vec<_>>>()?;
    let applied = store
        .call_mut(move |conn| store_digest::apply(conn, peer_row, &rows))
        .await?;
    Ok(applied)
}

/// The cursor this node last consumed from a peer, restarting the digest if it is corrupt.
pub async fn read_cursor(store: &Store, peer_row: i64) -> Result<Option<DigestCursor>> {
    let raw = store
        .call(move |conn| store_digest::read_cursor(conn, peer_row))
        .await?;
    Ok(raw
        .as_deref()
        .map(DigestCursor::parse)
        .transpose()
        .unwrap_or_default())
}

/// Records how far this node has read a peer's digest.
pub async fn write_cursor(
    store: &Store,
    peer_row: i64,
    cursor: Option<DigestCursor>,
) -> Result<()> {
    let encoded = cursor.map(DigestCursor::encode);
    let at = Timestamp::now();
    store
        .call_mut(move |conn| store_digest::write_cursor(conn, peer_row, encoded.as_deref(), at))
        .await?;
    Ok(())
}

fn row_of(entry: store_digest::DigestEntry) -> DigestRow {
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
    if path.is_empty() || path.split('/').any(|segment| segment == "..") {
        return Err(bad(format!("path {:?} is not a repository path", row.path)));
    }
    if row.repo.trim().is_empty() {
        return Err(bad("a row names no repository".to_owned()));
    }
    let repo = RepoId::parse_qualified(&row.repo)
        .map_err(|error| bad(format!("repo {:?}: {error}", row.repo)))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    Ok(RemoteEntry {
        sha256: row.sha256,
        size: row.size,
        role: row.role,
        base: row.base.clone(),
        revision_sha: row
            .revision_sha
            .unwrap_or_else(|| synthetic_sha(&row.sha256)),
        name: name.to_owned(),
        path: path.to_owned(),
        repo,
    })
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
    use hoard_core::{ArtifactFormat, NodeId, RepoType};
    use hoard_index::config::SourceKind;
    use hoard_index::scan::SNIFF_VERSION;
    use hoard_index::store::artifact::{NewArtifact, upsert_artifact};
    use hoard_index::store::blob::{set_sha256, upsert_blob};
    use hoard_index::store::entry::{NewEntry, upsert_entry};
    use hoard_index::store::node::{NewNode, upsert_node};
    use hoard_index::store::repo::{NewRepo, upsert_repo};
    use hoard_index::store::revision::{NewRevision, upsert_revision};
    use hoard_index::store::search_exec;

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

    /// A store holding `count` hashed local artifacts, plus an empty peer node row.
    async fn seeded(count: u8) -> (Store, i64) {
        let store = Store::open_in_memory().expect("store opens");
        let peer = store
            .call_mut(move |conn| {
                let local = upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                let peer = upsert_node(
                    conn,
                    &NewNode::peer(
                        NodeId::new("laptop-1").expect("id"),
                        "laptop",
                        "http://laptop:7099",
                    ),
                )?;
                let now = Timestamp::now();
                let repo = RepoId::new(RepoType::Models, Some("kohya".to_owned()), "local");
                let repo_row = upsert_repo(conn, &NewRepo::new(repo, local, SourceKind::Loose))?;
                let revision = upsert_revision(conn, &NewRevision::new(repo_row, commit(9)))?;
                for seed in 1..=count {
                    let name = format!("weight-{seed}.safetensors");
                    let blob = upsert_blob(conn, &format!("fp-{seed}"), u64::from(seed), now)?;
                    set_sha256(conn, blob, sha256(seed), None, now)?;
                    let mut artifact = NewArtifact::new(blob, name.clone());
                    artifact.sniff_version = SNIFF_VERSION;
                    artifact.format = ArtifactFormat::Safetensors;
                    artifact.role = Role::Lora;
                    artifact.base = Base::new("sdxl");
                    let artifact = upsert_artifact(conn, &artifact, now)?;
                    let mut entry = NewEntry::new(revision, name, local);
                    entry.blob = Some(blob);
                    entry.artifact = Some(artifact);
                    entry.size = u64::from(seed);
                    entry.state = State::Served;
                    upsert_entry(conn, &entry, now)?;
                }
                Ok(peer)
            })
            .await
            .expect("index seeds");
        (store, peer)
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
        let (store, _) = seeded(7).await;
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = collect(&store, cursor, 3).await.expect("page reads");
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
        let (store, peer) = seeded(1).await;
        let page = DigestPage {
            rows: vec![row(20, "loras/film-grain.safetensors")],
            next_cursor: None,
            total: 1,
        };
        let applied = apply(&store, "laptop", peer, &page)
            .await
            .expect("digest applies");
        assert_eq!(applied.entries, 1);
        assert_eq!(applied.artifacts, 1);

        let found = store
            .call(|conn| search_exec::query_rows(conn, "node = ?", &["laptop".into()], "", 10, 0))
            .await
            .expect("rows read");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, State::Remote);
        assert_eq!(found[0].path, "loras/film-grain.safetensors");
        assert_eq!(found[0].blob_sha256, Some(sha256(20)));
    }

    #[tokio::test]
    async fn a_malformed_digest_names_the_peer_and_leaves_the_index_untouched() {
        let (store, peer) = seeded(1).await;
        let page = DigestPage {
            rows: vec![row(20, "loras/ok.safetensors"), row(21, "   ")],
            next_cursor: None,
            total: 2,
        };
        let error = apply(&store, "laptop", peer, &page)
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

        let counted = store
            .call(|conn| search_exec::count_rows(conn, "node = ?", &["laptop".into()]))
            .await
            .expect("count reads");
        assert_eq!(counted, 0);
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
    }

    #[tokio::test]
    async fn a_peer_cursor_is_remembered_and_a_corrupt_one_restarts_the_digest() {
        let (store, peer) = seeded(1).await;
        assert_eq!(read_cursor(&store, peer).await.expect("no cursor"), None);

        write_cursor(&store, peer, Some(DigestCursor::new(12)))
            .await
            .expect("cursor writes");
        assert_eq!(
            read_cursor(&store, peer).await.expect("cursor reads"),
            Some(DigestCursor::new(12))
        );

        store
            .call_mut(move |conn| {
                conn.execute(
                    "UPDATE peer_cursor SET cursor = 'rubbish' WHERE node_id = ?1",
                    [peer],
                )?;
                Ok(())
            })
            .await
            .expect("cursor corrupts");
        assert_eq!(read_cursor(&store, peer).await.expect("cursor reads"), None);
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
