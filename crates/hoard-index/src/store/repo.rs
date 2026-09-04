use std::path::PathBuf;

use jiff::Timestamp;
use rusqlite::{Connection, Row, params, params_from_iter};
use serde::Serialize;

use hoard_core::{RepoId, RepoRowId, RepoType, SourceId};

use crate::config::SourceKind;
use crate::error::Result;
use crate::store::SqlValue;
use crate::store::convert::{
    collect_rows, optional_row, parse_enum, parse_opt_timestamp, required_row, timestamp_text,
    to_u64,
};
use crate::store::source::source_kind;

const REPO_COLUMNS: &str = "id, qualified, repo_type, org, name, node_id, source_id, kind, \
     root_path, git_dir, default_ref, description, card_data, license, downloads, likes, \
     private, gated, last_modified, created_at";

/// A repository as one node sees it: the same upstream repo on two nodes is two rows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoRow {
    pub id: RepoRowId,
    pub repo: RepoId,
    pub node_row: i64,
    pub source: Option<SourceId>,
    pub kind: SourceKind,
    pub root_path: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub default_ref: String,
    pub description: Option<String>,
    pub card_data: Option<String>,
    pub license: Option<String>,
    pub downloads: u64,
    pub likes: u64,
    pub private: bool,
    pub gated: bool,
    pub last_modified: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
}

impl RepoRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        let repo_type: RepoType = parse_enum(&row.get::<_, String>("repo_type")?)?;
        Ok(Self {
            id: RepoRowId::new(row.get("id")?),
            repo: RepoId::new(repo_type, row.get("org")?, row.get::<_, String>("name")?),
            node_row: row.get("node_id")?,
            source: row.get::<_, Option<i64>>("source_id")?.map(SourceId::new),
            kind: source_kind(&row.get::<_, String>("kind")?)?,
            root_path: row
                .get::<_, Option<String>>("root_path")?
                .map(PathBuf::from),
            git_dir: row.get::<_, Option<String>>("git_dir")?.map(PathBuf::from),
            default_ref: row.get("default_ref")?,
            description: row.get("description")?,
            card_data: row.get("card_data")?,
            license: row.get("license")?,
            downloads: to_u64(row.get::<_, i64>("downloads")?),
            likes: to_u64(row.get::<_, i64>("likes")?),
            private: row.get::<_, i64>("private")? != 0,
            gated: row.get::<_, i64>("gated")? != 0,
            last_modified: parse_opt_timestamp(row.get::<_, Option<String>>("last_modified")?)?,
            created_at: parse_opt_timestamp(row.get::<_, Option<String>>("created_at")?)?,
        })
    }
}

/// A repository as discovery found it. Metadata left empty is kept from the previous scan.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRepo {
    pub repo: RepoId,
    pub node_row: i64,
    pub source: Option<SourceId>,
    pub kind: SourceKind,
    pub root_path: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub default_ref: String,
    pub description: Option<String>,
    pub card_data: Option<String>,
    pub license: Option<String>,
    pub downloads: u64,
    pub likes: u64,
    pub private: bool,
    pub gated: bool,
    pub last_modified: Option<Timestamp>,
    pub created_at: Option<Timestamp>,
}

impl NewRepo {
    pub fn new(repo: RepoId, node_row: i64, kind: SourceKind) -> Self {
        Self {
            repo,
            node_row,
            source: None,
            kind,
            root_path: None,
            git_dir: None,
            default_ref: "main".to_owned(),
            description: None,
            card_data: None,
            license: None,
            downloads: 0,
            likes: 0,
            private: false,
            gated: false,
            last_modified: None,
            created_at: None,
        }
    }
}

/// Which repositories a listing wants; every field left empty widens the list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoFilter {
    pub text: Option<String>,
    pub repo_type: Option<RepoType>,
    pub node_row: Option<i64>,
    /// Keep only repositories that still hold at least one file.
    ///
    /// A vanished file is marked absent rather than deleted, so its repository survives a
    /// rescan and its history with it. That is right for the record and wrong for a list
    /// someone picks from: a repository with nothing left in it would still be offered,
    /// and pulling it would fetch nothing. The scan leaves this off, because it needs
    /// every repository to rebuild its stamp cache.
    pub present_only: bool,
}

/// Inserts a repository or refreshes it, keeping metadata a metadata-less rescan omits.
pub fn upsert_repo(conn: &Connection, input: &NewRepo) -> Result<RepoRowId> {
    let id: i64 = conn.query_row(
        "INSERT INTO repo(qualified, repo_type, org, name, node_id, source_id, kind, root_path, \
             git_dir, default_ref, description, card_data, license, downloads, likes, private, \
             gated, last_modified, created_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19) \
         ON CONFLICT(qualified, node_id) DO UPDATE SET \
             source_id = COALESCE(excluded.source_id, repo.source_id), \
             kind = excluded.kind, \
             root_path = COALESCE(excluded.root_path, repo.root_path), \
             git_dir = COALESCE(excluded.git_dir, repo.git_dir), \
             default_ref = excluded.default_ref, \
             description = COALESCE(excluded.description, repo.description), \
             card_data = COALESCE(excluded.card_data, repo.card_data), \
             license = COALESCE(excluded.license, repo.license), \
             downloads = MAX(excluded.downloads, repo.downloads), \
             likes = MAX(excluded.likes, repo.likes), \
             private = excluded.private, gated = excluded.gated, \
             last_modified = COALESCE(excluded.last_modified, repo.last_modified), \
             created_at = COALESCE(repo.created_at, excluded.created_at) \
         RETURNING id",
        params![
            input.repo.qualified(),
            input.repo.repo_type().as_str(),
            input.repo.org(),
            input.repo.name(),
            input.node_row,
            input.source.map(SourceId::get),
            input.kind.as_str(),
            input
                .root_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            input
                .git_dir
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            input.default_ref,
            input.description,
            input.card_data,
            input.license,
            input.downloads as i64,
            input.likes as i64,
            i64::from(input.private),
            i64::from(input.gated),
            input.last_modified.map(timestamp_text),
            input.created_at.map(timestamp_text),
        ],
        |row| row.get(0),
    )?;
    Ok(RepoRowId::new(id))
}

pub fn repo(conn: &Connection, id: RepoRowId) -> Result<RepoRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {REPO_COLUMNS} FROM repo WHERE id = ?1"),
            [id.get()],
            |row| Ok(RepoRow::from_row(row)),
        ),
        "repo",
        id,
    )
}

/// The repository named `type/org/name` as held by one node.
pub fn repo_by_qualified(
    conn: &Connection,
    qualified: &str,
    node_row: i64,
) -> Result<Option<RepoRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {REPO_COLUMNS} FROM repo WHERE qualified = ?1 AND node_id = ?2"),
        (qualified, node_row),
        |row| Ok(RepoRow::from_row(row)),
    ))
}

/// Every node holding the repository named `type/org/name`.
pub fn repos_by_qualified(conn: &Connection, qualified: &str) -> Result<Vec<RepoRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {REPO_COLUMNS} FROM repo WHERE qualified = ?1 ORDER BY node_id"
    ))?;
    let rows = statement.query_map([qualified], |row| Ok(RepoRow::from_row(row)))?;
    collect_rows(rows)
}

/// Lists repositories in id order, resuming after `cursor`.
pub fn list_repos(
    conn: &Connection,
    filter: &RepoFilter,
    cursor: Option<RepoRowId>,
    limit: u32,
) -> Result<Vec<RepoRow>> {
    let mut clauses = vec!["id > ?1".to_owned()];
    let mut values = vec![SqlValue::Int(cursor.map_or(0, RepoRowId::get))];
    if let Some(text) = &filter.text {
        values.push(SqlValue::text(format!("%{}%", text.to_lowercase())));
        clauses.push(format!("LOWER(qualified) LIKE ?{}", values.len()));
    }
    if let Some(repo_type) = filter.repo_type {
        values.push(SqlValue::text(repo_type.as_str()));
        clauses.push(format!("repo_type = ?{}", values.len()));
    }
    if let Some(node_row) = filter.node_row {
        values.push(SqlValue::Int(node_row));
        clauses.push(format!("node_id = ?{}", values.len()));
    }
    if filter.present_only {
        clauses.push(
            "EXISTS (SELECT 1 FROM revision v JOIN entry e ON e.revision_id = v.id \
             WHERE v.repo_id = repo.id AND e.present = 1)"
                .to_owned(),
        );
    }
    values.push(SqlValue::Int(i64::from(limit)));
    let sql = format!(
        "SELECT {REPO_COLUMNS} FROM repo WHERE {} ORDER BY id LIMIT ?{}",
        clauses.join(" AND "),
        values.len()
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| {
        Ok(RepoRow::from_row(row))
    })?;
    collect_rows(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::node::{NewNode, upsert_node};
    use hoard_core::NodeId;

    fn store_with_node() -> (Store, i64) {
        let store = Store::open_in_memory().expect("store opens");
        let node_row = {
            let conn = store.write().expect("writer");
            upsert_node(
                &conn,
                &NewNode::local(
                    NodeId::new("workstation-1").expect("node id"),
                    "workstation",
                ),
            )
            .expect("node inserts")
        };
        (store, node_row)
    }

    fn repo_id(reference: &str) -> RepoId {
        RepoId::parse_ref(RepoType::Models, reference).expect("repo reference parses")
    }

    #[test]
    fn a_repository_upserts_and_keeps_metadata_a_bare_rescan_omits() {
        let (store, node_row) = store_with_node();
        let conn = store.write().expect("writer");
        let described = NewRepo {
            description: Some("a checkpoint".to_owned()),
            license: Some("openrail".to_owned()),
            root_path: Some(PathBuf::from("/data/hub/models--org--name")),
            ..NewRepo::new(repo_id("org/name"), node_row, SourceKind::HfCache)
        };
        let id = upsert_repo(&conn, &described).expect("repo inserts");
        let bare = NewRepo::new(repo_id("org/name"), node_row, SourceKind::HfCache);
        assert_eq!(upsert_repo(&conn, &bare).expect("repo upserts"), id);

        let row = repo(&conn, id).expect("repo reads");
        assert_eq!(row.repo.qualified(), "models/org/name");
        assert_eq!(row.description.as_deref(), Some("a checkpoint"));
        assert_eq!(row.license.as_deref(), Some("openrail"));
        assert_eq!(
            row.root_path,
            Some(PathBuf::from("/data/hub/models--org--name"))
        );
        assert_eq!(row.default_ref, "main");
        assert_eq!(
            repo_by_qualified(&conn, "models/org/name", node_row).expect("lookup"),
            Some(row)
        );
        assert!(
            repo_by_qualified(&conn, "models/org/name", node_row + 1)
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn the_same_repository_on_two_nodes_is_two_rows() {
        let (store, node_row) = store_with_node();
        let conn = store.write().expect("writer");
        let peer = upsert_node(
            &conn,
            &NewNode::peer(
                NodeId::new("laptop-1").expect("node id"),
                "laptop",
                "http://laptop",
            ),
        )
        .expect("peer inserts");
        let here = upsert_repo(
            &conn,
            &NewRepo::new(repo_id("org/name"), node_row, SourceKind::HfCache),
        )
        .expect("repo inserts");
        let there = upsert_repo(
            &conn,
            &NewRepo::new(repo_id("org/name"), peer, SourceKind::Git),
        )
        .expect("repo inserts");
        assert_ne!(here, there);
        assert_eq!(
            repos_by_qualified(&conn, "models/org/name")
                .expect("lookup")
                .len(),
            2
        );
    }

    #[test]
    fn listing_filters_by_text_type_and_node_then_pages() {
        let (store, node_row) = store_with_node();
        let conn = store.write().expect("writer");
        for reference in ["org/alpha", "org/beta", "other/gamma"] {
            upsert_repo(
                &conn,
                &NewRepo::new(repo_id(reference), node_row, SourceKind::Loose),
            )
            .expect("repo inserts");
        }
        upsert_repo(
            &conn,
            &NewRepo::new(
                RepoId::parse_ref(RepoType::Datasets, "org/set").expect("reference parses"),
                node_row,
                SourceKind::Loose,
            ),
        )
        .expect("dataset inserts");

        let all = list_repos(&conn, &RepoFilter::default(), None, 10).expect("list");
        assert_eq!(all.len(), 4);

        let page = list_repos(&conn, &RepoFilter::default(), None, 2).expect("page");
        assert_eq!(page.len(), 2);
        let next = list_repos(&conn, &RepoFilter::default(), Some(page[1].id), 10).expect("page");
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].repo.name(), "gamma");

        let filter = RepoFilter {
            text: Some("org/".to_owned()),
            ..RepoFilter::default()
        };
        assert_eq!(list_repos(&conn, &filter, None, 10).expect("list").len(), 3);

        let datasets = RepoFilter {
            repo_type: Some(RepoType::Datasets),
            ..RepoFilter::default()
        };
        assert_eq!(
            list_repos(&conn, &datasets, None, 10).expect("list").len(),
            1
        );

        let elsewhere = RepoFilter {
            node_row: Some(node_row + 1),
            ..RepoFilter::default()
        };
        assert!(
            list_repos(&conn, &elsewhere, None, 10)
                .expect("list")
                .is_empty()
        );
    }
}
