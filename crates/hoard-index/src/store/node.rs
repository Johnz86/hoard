use jiff::Timestamp;
use rusqlite::{Connection, Row};
use serde::Serialize;

use hoard_core::NodeId;

use crate::error::Result;
use crate::store::convert::{
    collect_rows, optional_row, parse_opt_timestamp, required_row, timestamp_text, to_u64,
};

const NODE_COLUMNS: &str = "id, node_id, name, url, is_self, online, indexed_bytes, last_seen";

/// A machine in the index: this one, plus every peer discovered or configured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeRow {
    pub id: i64,
    pub node_id: NodeId,
    pub name: String,
    pub url: Option<String>,
    pub is_self: bool,
    pub online: bool,
    pub indexed_bytes: u64,
    pub last_seen: Option<Timestamp>,
}

impl NodeRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            node_id: NodeId::new(row.get::<_, String>("node_id")?)?,
            name: row.get("name")?,
            url: row.get("url")?,
            is_self: row.get::<_, i64>("is_self")? != 0,
            online: row.get::<_, i64>("online")? != 0,
            indexed_bytes: to_u64(row.get::<_, i64>("indexed_bytes")?),
            last_seen: parse_opt_timestamp(row.get::<_, Option<String>>("last_seen")?)?,
        })
    }
}

/// The identity of a node as first learned, before any index statistics exist.
#[derive(Debug, Clone, PartialEq)]
pub struct NewNode {
    pub node_id: NodeId,
    pub name: String,
    pub url: Option<String>,
    pub is_self: bool,
}

impl NewNode {
    pub fn local(node_id: NodeId, name: impl Into<String>) -> Self {
        Self {
            node_id,
            name: name.into(),
            url: None,
            is_self: true,
        }
    }

    pub fn peer(node_id: NodeId, name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            node_id,
            name: name.into(),
            url: Some(url.into()),
            is_self: false,
        }
    }
}

/// Inserts a node or refreshes its name and url, returning its row id.
pub fn upsert_node(conn: &Connection, node: &NewNode) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO node(node_id, name, url, is_self) VALUES(?1, ?2, ?3, ?4) \
         ON CONFLICT(node_id) DO UPDATE SET \
             name = excluded.name, \
             url = COALESCE(excluded.url, node.url), \
             is_self = excluded.is_self \
         RETURNING id",
        (
            node.node_id.as_str(),
            &node.name,
            &node.url,
            i64::from(node.is_self),
        ),
        |row| row.get(0),
    )?;
    Ok(id)
}

/// The row marked `is_self`, which every scan and every served request attributes to.
pub fn local_node(conn: &Connection) -> Result<NodeRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {NODE_COLUMNS} FROM node WHERE is_self = 1 ORDER BY id LIMIT 1"),
            [],
            |row| Ok(NodeRow::from_row(row)),
        ),
        "node",
        "self",
    )
}

pub fn node_by_name(conn: &Connection, name: &str) -> Result<Option<NodeRow>> {
    optional_row(conn.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM node WHERE name = ?1"),
        [name],
        |row| Ok(NodeRow::from_row(row)),
    ))
}

pub fn node(conn: &Connection, id: i64) -> Result<NodeRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {NODE_COLUMNS} FROM node WHERE id = ?1"),
            [id],
            |row| Ok(NodeRow::from_row(row)),
        ),
        "node",
        id,
    )
}

pub fn list_nodes(conn: &Connection) -> Result<Vec<NodeRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {NODE_COLUMNS} FROM node ORDER BY is_self DESC, name"
    ))?;
    let rows = statement.query_map([], |row| Ok(NodeRow::from_row(row)))?;
    collect_rows(rows)
}

/// Records that a node was seen, with how many bytes its index now covers.
pub fn touch_node(conn: &Connection, id: i64, at: Timestamp, indexed_bytes: u64) -> Result<()> {
    conn.execute(
        "UPDATE node SET last_seen = ?2, indexed_bytes = ?3, online = 1 WHERE id = ?1",
        (id, timestamp_text(at), indexed_bytes as i64),
    )?;
    Ok(())
}

pub fn set_node_online(conn: &Connection, id: i64, online: bool) -> Result<()> {
    conn.execute(
        "UPDATE node SET online = ?2 WHERE id = ?1",
        (id, i64::from(online)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IndexError;
    use crate::store::Store;

    fn node_id(raw: &str) -> NodeId {
        NodeId::new(raw).expect("node id is valid")
    }

    #[test]
    fn the_local_node_round_trips_and_upserts_by_identity() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = upsert_node(
            &conn,
            &NewNode::local(node_id("workstation-1"), "workstation"),
        )
        .expect("node inserts");

        let again = upsert_node(
            &conn,
            &NewNode::local(node_id("workstation-1"), "workstation"),
        )
        .expect("node upserts");
        assert_eq!(id, again);

        let local = local_node(&conn).expect("self node exists");
        assert_eq!(local.id, id);
        assert!(local.is_self);
        assert_eq!(local.indexed_bytes, 0);

        let at: Timestamp = "2026-09-04T09:10:00Z".parse().expect("timestamp");
        touch_node(&conn, id, at, 4096).expect("touch");
        set_node_online(&conn, id, false).expect("offline");
        let refreshed = node(&conn, id).expect("node by id");
        assert_eq!(refreshed.indexed_bytes, 4096);
        assert_eq!(refreshed.last_seen, Some(at));
        assert!(!refreshed.online);
    }

    #[test]
    fn peers_list_after_the_local_node() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        upsert_node(
            &conn,
            &NewNode::peer(node_id("laptop-1"), "laptop", "http://laptop:7099"),
        )
        .expect("peer inserts");
        upsert_node(
            &conn,
            &NewNode::local(node_id("workstation-1"), "workstation"),
        )
        .expect("self inserts");

        let nodes = list_nodes(&conn).expect("nodes list");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "workstation");
        assert_eq!(nodes[1].url.as_deref(), Some("http://laptop:7099"));
        assert!(node_by_name(&conn, "laptop").expect("lookup").is_some());
        assert!(node_by_name(&conn, "absent").expect("lookup").is_none());
    }

    #[test]
    fn a_database_without_a_local_node_reports_not_found() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.read().expect("reader");
        assert!(matches!(
            local_node(&conn),
            Err(IndexError::NotFound { kind: "node", .. })
        ));
    }
}
