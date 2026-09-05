use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};

use hoard_core::{CommitSha, RepoRowId, RevisionId};

use crate::error::{IndexError, Result};
use crate::store::convert::{
    collect_rows, optional_row, parse_enum, parse_opt_timestamp, required_row, timestamp_text,
};

const REVISION_COLUMNS: &str = "id, repo_id, sha, title, author, committed, synthetic";

const REF_COLUMNS: &str = "g.id AS id, g.repo_id AS repo_id, g.name AS name, g.kind AS kind, \
     g.revision_id AS revision_id, v.sha AS sha";

/// A commit in a repository. Loose folders get a synthetic revision so every file in the
/// index is reachable by the same `repo/revision/path` triple.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RevisionRow {
    pub id: RevisionId,
    pub repo: RepoRowId,
    pub sha: CommitSha,
    pub title: Option<String>,
    pub author: Option<String>,
    pub committed: Option<Timestamp>,
    pub synthetic: bool,
}

impl RevisionRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: RevisionId::new(row.get("id")?),
            repo: RepoRowId::new(row.get("repo_id")?),
            sha: parse_enum(&row.get::<_, String>("sha")?)?,
            title: row.get("title")?,
            author: row.get("author")?,
            committed: parse_opt_timestamp(row.get::<_, Option<String>>("committed")?)?,
            synthetic: row.get::<_, i64>("synthetic")? != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRevision {
    pub repo: RepoRowId,
    pub sha: CommitSha,
    pub title: Option<String>,
    pub author: Option<String>,
    pub committed: Option<Timestamp>,
    pub synthetic: bool,
}

impl NewRevision {
    pub fn new(repo: RepoRowId, sha: CommitSha) -> Self {
        Self {
            repo,
            sha,
            title: None,
            author: None,
            committed: None,
            synthetic: false,
        }
    }

    /// The single revision a loose folder presents, named by a content hash.
    pub fn synthetic(repo: RepoRowId, sha: CommitSha) -> Self {
        Self {
            synthetic: true,
            ..Self::new(repo, sha)
        }
    }
}

/// Whether a name points at a moving branch or a fixed tag. Ordered so branches sort ahead
/// of tags, which is the order the REFS tab and the `search_row` view both present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Branch,
    Tag,
}

impl RefKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RefKind::Branch => "branch",
            RefKind::Tag => "tag",
        }
    }
}

impl fmt::Display for RefKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RefKind {
    type Err = IndexError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "branch" => Ok(RefKind::Branch),
            "tag" => Ok(RefKind::Tag),
            other => Err(IndexError::Config(format!("unknown ref kind {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RefRow {
    pub id: i64,
    pub repo: RepoRowId,
    pub name: String,
    pub kind: RefKind,
    pub revision: RevisionId,
    pub sha: CommitSha,
}

impl RefRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            repo: RepoRowId::new(row.get("repo_id")?),
            name: row.get("name")?,
            kind: parse_enum(&row.get::<_, String>("kind")?)?,
            revision: RevisionId::new(row.get("revision_id")?),
            sha: parse_enum(&row.get::<_, String>("sha")?)?,
        })
    }
}

pub fn upsert_revision(conn: &Connection, input: &NewRevision) -> Result<RevisionId> {
    let id: i64 = conn.query_row(
        "INSERT INTO revision(repo_id, sha, title, author, committed, synthetic) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(repo_id, sha) DO UPDATE SET \
             title = COALESCE(excluded.title, revision.title), \
             author = COALESCE(excluded.author, revision.author), \
             committed = COALESCE(excluded.committed, revision.committed), \
             synthetic = excluded.synthetic \
         RETURNING id",
        params![
            input.repo.get(),
            input.sha.to_hex(),
            input.title,
            input.author,
            input.committed.map(timestamp_text),
            i64::from(input.synthetic),
        ],
        |row| row.get(0),
    )?;
    Ok(RevisionId::new(id))
}

pub fn upsert_ref(
    conn: &Connection,
    repo: RepoRowId,
    name: &str,
    kind: RefKind,
    revision: RevisionId,
) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO git_ref(repo_id, name, kind, revision_id) VALUES(?1, ?2, ?3, ?4) \
         ON CONFLICT(repo_id, name) DO UPDATE SET \
             kind = excluded.kind, revision_id = excluded.revision_id \
         RETURNING id",
        (repo.get(), name, kind.as_str(), revision.get()),
        |row| row.get(0),
    )?;
    Ok(id)
}

pub fn refs_for_repo(conn: &Connection, repo: RepoRowId) -> Result<Vec<RefRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {REF_COLUMNS} FROM git_ref g JOIN revision v ON v.id = g.revision_id \
         WHERE g.repo_id = ?1 ORDER BY g.kind, g.name"
    ))?;
    let rows = statement.query_map([repo.get()], |row| Ok(RefRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn revision_by_sha(
    conn: &Connection,
    repo: RepoRowId,
    sha: &CommitSha,
) -> Result<Option<RevisionRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {REVISION_COLUMNS} FROM revision WHERE repo_id = ?1 AND sha = ?2"),
        (repo.get(), sha.to_hex()),
        |row| Ok(RevisionRow::from_row(row)),
    ))
}

pub fn revisions_for_repo(conn: &Connection, repo: RepoRowId) -> Result<Vec<RevisionRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {REVISION_COLUMNS} FROM revision WHERE repo_id = ?1 ORDER BY id"
    ))?;
    let rows = statement.query_map([repo.get()], |row| Ok(RevisionRow::from_row(row)))?;
    collect_rows(rows)
}

pub fn revision(conn: &Connection, id: RevisionId) -> Result<RevisionRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {REVISION_COLUMNS} FROM revision WHERE id = ?1"),
            [id.get()],
            |row| Ok(RevisionRow::from_row(row)),
        ),
        "revision",
        id,
    )
}

/// Resolves what a client asked for: a branch, a tag, a full commit sha, or `main`, which
/// falls back to the repository's default ref and then to its only revision.
pub fn resolve_revision(conn: &Connection, repo: RepoRowId, rev: &str) -> Result<RevisionRow> {
    let wanted = rev.trim();
    if let Some(found) = revision_by_ref(conn, repo, wanted)? {
        return Ok(found);
    }
    if let Ok(sha) = wanted.parse::<CommitSha>()
        && let Some(found) = revision_by_sha(conn, repo, &sha)?
    {
        return Ok(found);
    }
    let default_ref: String = conn.query_row(
        "SELECT default_ref FROM repo WHERE id = ?1",
        [repo.get()],
        |row| row.get(0),
    )?;
    if wanted.is_empty() || wanted == "main" || wanted == default_ref {
        if let Some(found) = revision_by_ref(conn, repo, &default_ref)? {
            return Ok(found);
        }
        let mut revisions = revisions_for_repo(conn, repo)?;
        if revisions.len() == 1 {
            return Ok(revisions.remove(0));
        }
    }
    Err(IndexError::not_found(
        "revision",
        format!("{repo}@{wanted}"),
    ))
}

fn revision_by_ref(conn: &Connection, repo: RepoRowId, name: &str) -> Result<Option<RevisionRow>> {
    optional_row(conn.query_row(
        &format!(
            "SELECT {REVISION_COLUMNS} FROM revision \
             WHERE id = (SELECT revision_id FROM git_ref WHERE repo_id = ?1 AND name = ?2)"
        ),
        (repo.get(), name),
        |row| Ok(RevisionRow::from_row(row)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceKind;
    use crate::store::Store;
    use crate::store::node::{NewNode, upsert_node};
    use crate::store::repo::{NewRepo, upsert_repo};
    use hoard_core::{NodeId, RepoId, RepoType};

    fn sha(byte: u8) -> CommitSha {
        CommitSha::from_bytes([byte; 20])
    }

    fn store_with_repo() -> (Store, RepoRowId) {
        let store = Store::open_in_memory().expect("store opens");
        let repo = {
            let conn = store.write().expect("writer");
            let node_row = upsert_node(
                &conn,
                &NewNode::local(
                    NodeId::new("workstation-1").expect("node id"),
                    "workstation",
                ),
            )
            .expect("node inserts");
            let repo_id =
                RepoId::parse_ref(RepoType::Models, "org/name").expect("reference parses");
            upsert_repo(&conn, &NewRepo::new(repo_id, node_row, SourceKind::Git))
                .expect("repo inserts")
        };
        (store, repo)
    }

    #[test]
    fn revisions_and_refs_upsert_by_identity() {
        let (store, repo) = store_with_repo();
        let conn = store.write().expect("writer");
        let first =
            upsert_revision(&conn, &NewRevision::new(repo, sha(0x46))).expect("revision inserts");
        let titled = NewRevision {
            title: Some("initial commit".to_owned()),
            ..NewRevision::new(repo, sha(0x46))
        };
        assert_eq!(
            upsert_revision(&conn, &titled).expect("revision upserts"),
            first
        );
        assert_eq!(
            upsert_revision(&conn, &NewRevision::new(repo, sha(0x46)))
                .expect("bare rescan keeps the title"),
            first
        );
        assert_eq!(
            revision(&conn, first)
                .expect("revision reads")
                .title
                .as_deref(),
            Some("initial commit")
        );

        let id = upsert_ref(&conn, repo, "main", RefKind::Branch, first).expect("ref inserts");
        assert_eq!(
            upsert_ref(&conn, repo, "main", RefKind::Branch, first).expect("ref upserts"),
            id
        );
        let refs = refs_for_repo(&conn, repo).expect("refs list");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, RefKind::Branch);
        assert_eq!(refs[0].sha, sha(0x46));
    }

    #[test]
    fn resolution_accepts_branches_tags_shas_and_the_default() {
        let (store, repo) = store_with_repo();
        let conn = store.write().expect("writer");
        let head =
            upsert_revision(&conn, &NewRevision::new(repo, sha(0x46))).expect("revision inserts");
        let tagged =
            upsert_revision(&conn, &NewRevision::new(repo, sha(0x91))).expect("revision inserts");
        upsert_ref(&conn, repo, "main", RefKind::Branch, head).expect("branch inserts");
        upsert_ref(&conn, repo, "v1.0", RefKind::Tag, tagged).expect("tag inserts");

        assert_eq!(
            resolve_revision(&conn, repo, "main").expect("branch").id,
            head
        );
        assert_eq!(
            resolve_revision(&conn, repo, "v1.0").expect("tag").id,
            tagged
        );
        assert_eq!(
            resolve_revision(&conn, repo, &sha(0x91).to_hex())
                .expect("sha")
                .id,
            tagged
        );
        assert_eq!(resolve_revision(&conn, repo, "").expect("default").id, head);
        assert!(matches!(
            resolve_revision(&conn, repo, "nowhere"),
            Err(IndexError::NotFound {
                kind: "revision",
                ..
            })
        ));
    }

    #[test]
    fn a_repository_with_one_revision_and_no_refs_resolves_main() {
        let (store, repo) = store_with_repo();
        let conn = store.write().expect("writer");
        let only = upsert_revision(&conn, &NewRevision::synthetic(repo, sha(0x0a)))
            .expect("revision inserts");
        assert_eq!(
            resolve_revision(&conn, repo, "main").expect("fallback").id,
            only
        );
        assert!(revision(&conn, only).expect("revision reads").synthetic);

        upsert_revision(&conn, &NewRevision::new(repo, sha(0x0b))).expect("revision inserts");
        assert!(resolve_revision(&conn, repo, "main").is_err());
    }

    #[test]
    fn ref_kinds_round_trip_their_spelling() {
        for kind in [RefKind::Branch, RefKind::Tag] {
            assert_eq!(kind.as_str().parse::<RefKind>().expect("kind parses"), kind);
        }
        assert!("note".parse::<RefKind>().is_err());
    }
}
