//! The index side of node gossip: reading this node's digest and filing a peer's.
//!
//! A digest is one row per artifact a node holds. Paging is a keyset bound on the artifact
//! row id rather than an offset, so rows inserted between two polls are delivered exactly
//! once and rows already delivered are never repeated. The wire format and the cursor
//! spelling belong to `hoard-net`; everything here is SQL.

use jiff::Timestamp;
use rusqlite::Connection;

use hoard_core::{Base, CommitSha, RepoId, Role, Sha256, State};

use crate::config::SourceKind;
use crate::error::Result;
use crate::scan::SNIFF_VERSION;
use crate::store::artifact::{NewArtifact, artifact_by_blob, upsert_artifact};
use crate::store::blob::{blob_by_sha256, set_sha256, upsert_blob};
use crate::store::convert::{optional_row, timestamp_text};
use crate::store::entry::{NewEntry, upsert_entry};
use crate::store::repo::{NewRepo, repo_by_qualified, upsert_repo};
use crate::store::revision::{NewRevision, revision_by_sha, upsert_revision};
use crate::store::row::SqlValue;
use crate::store::search_exec;

/// Only this node's own, hashed artifacts are offered: a peer wants what this machine
/// holds, and a row without a content hash can neither be deduplicated nor verified.
const DIGEST_WHERE: &str = "node_is_self = 1 AND blob_sha256 IS NOT NULL";
const DIGEST_BOUND: &str = "node_is_self = 1 AND blob_sha256 IS NOT NULL AND artifact_id > ?";
const DIGEST_ORDER: &str = "artifact_id ASC";

/// One artifact of this node, flattened to what a peer needs to find and fetch it.
#[derive(Debug, Clone, PartialEq)]
pub struct DigestEntry {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: Base,
    pub repo: String,
    pub revision_sha: Option<CommitSha>,
    pub path: String,
    pub state: State,
}

/// One page of this node's digest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DigestPage {
    pub rows: Vec<DigestEntry>,
    /// The highest artifact row this page reached, which is where the next one starts.
    /// A page that returned nothing has nowhere new to point, so a reader that asks again
    /// from the same place is asking for what has been added since.
    pub next: Option<i64>,
    pub total: u64,
}

/// A peer's row, parsed and validated, ready to be filed against that peer's node row.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEntry {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: Base,
    pub repo: RepoId,
    pub revision_sha: CommitSha,
    pub path: String,
    pub name: String,
}

/// What filing one digest page changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigestApplied {
    pub entries: u64,
    pub artifacts: u64,
}

/// Reads one page of this node's digest, starting after `since`.
pub fn collect(conn: &Connection, since: Option<i64>, limit: u32) -> Result<DigestPage> {
    let limit = limit.max(1);
    let (where_sql, params) = match since {
        Some(bound) => (DIGEST_BOUND, vec![SqlValue::Int(bound)]),
        None => (DIGEST_WHERE, Vec::new()),
    };
    let rows = search_exec::query_rows(conn, where_sql, &params, DIGEST_ORDER, limit, 0)?;
    let highest = rows.last().map(|row| row.artifact_id.get());
    Ok(DigestPage {
        rows: rows
            .iter()
            .filter_map(|row| {
                Some(DigestEntry {
                    sha256: row.blob_sha256?,
                    size: row.size,
                    role: row.role,
                    base: row.base.clone(),
                    repo: row.repo.clone(),
                    revision_sha: row.revision_sha,
                    path: row.path.clone(),
                    state: row.state,
                })
            })
            .collect(),
        next: highest,
        total: search_exec::count_rows(conn, DIGEST_WHERE, &[])?,
    })
}

/// Files a peer's rows as REMOTE entries owned by that peer, in one transaction.
///
/// Bytes this node already knows keep their own interpretation: only the entry saying the
/// peer also holds them is added, which is what makes one blob show every node that has it.
pub fn apply(conn: &mut Connection, peer_row: i64, rows: &[RemoteEntry]) -> Result<DigestApplied> {
    let now = Timestamp::now();
    let mut applied = DigestApplied::default();
    let transaction = conn.transaction()?;
    for row in rows {
        applied = apply_row(&transaction, peer_row, row, now, applied)?;
    }
    transaction.commit()?;
    Ok(applied)
}

fn apply_row(
    conn: &Connection,
    peer_row: i64,
    row: &RemoteEntry,
    now: Timestamp,
    mut applied: DigestApplied,
) -> Result<DigestApplied> {
    let qualified = row.repo.qualified();
    let repo_row = match repo_by_qualified(conn, &qualified, peer_row)? {
        Some(existing) => existing.id,
        None => upsert_repo(
            conn,
            &NewRepo::new(row.repo.clone(), peer_row, SourceKind::Loose),
        )?,
    };
    let revision = match revision_by_sha(conn, repo_row, &row.revision_sha)? {
        Some(existing) => existing.id,
        None => upsert_revision(conn, &NewRevision::synthetic(repo_row, row.revision_sha))?,
    };
    let blob = match blob_by_sha256(conn, &row.sha256)? {
        Some(existing) => existing.id,
        None => {
            let blob = upsert_blob(conn, &row.sha256.to_hex(), row.size, now)?;
            set_sha256(conn, blob, row.sha256, None, now)?;
            blob
        }
    };
    let artifact = match artifact_by_blob(conn, blob, SNIFF_VERSION)? {
        Some(existing) => existing.artifact.id,
        None => {
            applied.artifacts += 1;
            let mut artifact = NewArtifact::new(blob, row.name.clone());
            artifact.sniff_version = SNIFF_VERSION;
            artifact.role = row.role;
            artifact.base = row.base.clone();
            artifact.org = row.repo.org().map(str::to_owned);
            upsert_artifact(conn, &artifact, now)?
        }
    };
    let mut entry = NewEntry::new(revision, row.path.clone(), peer_row);
    entry.blob = Some(blob);
    entry.artifact = Some(artifact);
    entry.size = row.size;
    entry.state = State::Remote;
    upsert_entry(conn, &entry, now)?;
    applied.entries += 1;
    Ok(applied)
}

/// How far this node has read a peer's digest, as the peer's own opaque cursor.
pub fn read_cursor(conn: &Connection, peer_row: i64) -> Result<Option<String>> {
    let found = optional_row(conn.query_row(
        "SELECT cursor FROM peer_cursor WHERE node_id = ?1",
        [peer_row],
        |row| Ok(Ok(row.get::<_, Option<String>>("cursor")?)),
    ))?;
    Ok(found.flatten())
}

/// Records how far this node has read a peer's digest. A page that ends the digest carries
/// no cursor and leaves the last one standing, so the next poll resumes where this stopped.
pub fn write_cursor(
    conn: &Connection,
    peer_row: i64,
    cursor: Option<&str>,
    at: Timestamp,
) -> Result<()> {
    conn.execute(
        "INSERT INTO peer_cursor(node_id, cursor, synced) VALUES(?1, ?2, ?3) \
         ON CONFLICT(node_id) DO UPDATE SET \
             cursor = COALESCE(excluded.cursor, peer_cursor.cursor), \
             synced = excluded.synced",
        (peer_row, cursor, timestamp_text(at)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use hoard_core::{ArtifactFormat, NodeId, RepoType};

    use super::*;
    use crate::store::Store;
    use crate::store::node::{NewNode, upsert_node};

    fn sha256(seed: u8) -> Sha256 {
        Sha256::from_bytes([seed; 32])
    }

    fn commit(seed: u8) -> CommitSha {
        CommitSha::from_bytes([seed; 20])
    }

    fn remote(seed: u8, path: &str) -> RemoteEntry {
        RemoteEntry {
            sha256: sha256(seed),
            size: 1024 * u64::from(seed),
            role: Role::Lora,
            base: Base::new("sdxl"),
            repo: RepoId::new(RepoType::Models, Some("kohya".to_owned()), "film-grain"),
            revision_sha: commit(seed),
            path: path.to_owned(),
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        }
    }

    /// A store holding `count` hashed local artifacts, plus an empty peer node row.
    fn seeded(count: u8) -> (Store, i64) {
        let store = Store::open_in_memory().expect("store opens");
        let peer = {
            let conn = store.write().expect("writer");
            let local = upsert_node(
                &conn,
                &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
            )
            .expect("local node");
            let peer = upsert_node(
                &conn,
                &NewNode::peer(
                    NodeId::new("laptop-1").expect("id"),
                    "laptop",
                    "http://laptop:7099",
                ),
            )
            .expect("peer node");
            let now = Timestamp::now();
            let repo = RepoId::new(RepoType::Models, Some("kohya".to_owned()), "local");
            let repo_row =
                upsert_repo(&conn, &NewRepo::new(repo, local, SourceKind::Loose)).expect("repo");
            let revision =
                upsert_revision(&conn, &NewRevision::new(repo_row, commit(9))).expect("revision");
            for seed in 1..=count {
                let name = format!("weight-{seed}.safetensors");
                let blob =
                    upsert_blob(&conn, &format!("fp-{seed}"), u64::from(seed), now).expect("blob");
                set_sha256(&conn, blob, sha256(seed), None, now).expect("hash");
                let mut artifact = NewArtifact::new(blob, name.clone());
                artifact.sniff_version = SNIFF_VERSION;
                artifact.format = ArtifactFormat::Safetensors;
                artifact.role = Role::Lora;
                artifact.base = Base::new("sdxl");
                let artifact = upsert_artifact(&conn, &artifact, now).expect("artifact");
                let mut entry = NewEntry::new(revision, name, local);
                entry.blob = Some(blob);
                entry.artifact = Some(artifact);
                entry.size = u64::from(seed);
                entry.state = State::Served;
                upsert_entry(&conn, &entry, now).expect("entry");
            }
            peer
        };
        (store, peer)
    }

    #[test]
    fn paging_a_digest_skips_no_row_and_repeats_none() {
        let (store, _) = seeded(7);
        let conn = store.read().expect("reader");
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = collect(&conn, cursor, 3).expect("page reads");
            assert_eq!(page.total, 7);
            if page.rows.is_empty() {
                break;
            }
            seen.extend(page.rows.iter().map(|row| row.sha256));
            cursor = page.next;
        }
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(seen.len(), 7);
        assert_eq!(unique.len(), 7);
    }

    #[test]
    fn a_cursor_delivers_only_what_is_new() {
        let (store, _) = seeded(2);
        let conn = store.read().expect("reader");
        let first = collect(&conn, None, 100).expect("first page");
        assert_eq!(first.rows.len(), 2);

        let highest = search_exec::query_rows(&conn, DIGEST_WHERE, &[], "artifact_id DESC", 1, 0)
            .expect("rows")
            .first()
            .map(|row| row.artifact_id.get())
            .expect("at least one artifact");
        assert_eq!(first.next, Some(highest));
        let after = collect(&conn, Some(highest), 100).expect("second page");
        assert!(after.rows.is_empty());
        assert_eq!(after.next, None);
        assert_eq!(after.total, 2);
    }

    #[test]
    fn an_applied_digest_lands_as_remote_entries_owned_by_the_peer() {
        let (store, peer) = seeded(1);
        let mut conn = store.write().expect("writer");
        let rows = [remote(20, "loras/film-grain.safetensors")];

        let applied = apply(&mut conn, peer, &rows).expect("digest applies");
        assert_eq!(applied.entries, 1);
        assert_eq!(applied.artifacts, 1);

        let found = search_exec::query_rows(&conn, "node = ?", &["laptop".into()], "", 10, 0)
            .expect("rows read");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state, State::Remote);
        assert_eq!(found[0].path, "loras/film-grain.safetensors");
        assert_eq!(found[0].blob_sha256, Some(sha256(20)));
        assert_eq!(found[0].role, Role::Lora);
        assert_eq!(found[0].org.as_deref(), Some("kohya"));

        let again = apply(&mut conn, peer, &rows).expect("digest reapplies");
        assert_eq!(again.entries, 1);
        assert_eq!(again.artifacts, 0);
        assert_eq!(
            search_exec::count_rows(&conn, "node = ?", &["laptop".into()]).expect("count"),
            1
        );
    }

    #[test]
    fn bytes_this_node_already_holds_keep_their_own_interpretation() {
        let (store, peer) = seeded(1);
        let mut conn = store.write().expect("writer");
        let mut shared = remote(1, "weight-1.safetensors");
        shared.role = Role::Unk;

        let applied = apply(&mut conn, peer, &[shared]).expect("digest applies");
        assert_eq!(applied.artifacts, 0);

        let found = search_exec::query_rows(
            &conn,
            "blob_sha256 = ?",
            &[sha256(1).to_hex().into()],
            "",
            10,
            0,
        )
        .expect("rows read");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].role, Role::Lora);
    }

    #[test]
    fn a_peer_cursor_is_remembered_and_a_page_without_one_keeps_the_last() {
        let (store, peer) = seeded(1);
        let conn = store.write().expect("writer");
        let now = Timestamp::now();
        assert_eq!(read_cursor(&conn, peer).expect("no cursor yet"), None);

        write_cursor(&conn, peer, Some("d1-c"), now).expect("cursor writes");
        assert_eq!(
            read_cursor(&conn, peer).expect("cursor reads").as_deref(),
            Some("d1-c")
        );

        write_cursor(&conn, peer, None, now).expect("cursor holds");
        assert_eq!(
            read_cursor(&conn, peer).expect("cursor reads").as_deref(),
            Some("d1-c")
        );
    }
}
