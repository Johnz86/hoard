use std::collections::HashMap;

use native_db::transaction::RTransaction;
use serde::Serialize;

use hoard_core::NodeId;

use crate::error::{IndexError, Result};
use crate::time::parse_opt_timestamp;

use super::Catalog;
use super::models::{LOCAL_NODE, Manifest, Node, NodeKey};
use super::userdata::next_id;

/// The prefix a source of a peer's rows carries, so the repositories filed under it read
/// back as that peer's rather than this node's.
pub const PEER_SOURCE: &str = "peer:";

/// The identity a node with no configured one falls back to.
const FALLBACK_ID: &str = "hoard";

/// One machine the catalog knows about, as the NODES tab and the peers routes show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeRecord {
    pub id: i64,
    pub node_id: NodeId,
    pub name: String,
    pub url: Option<String>,
    pub is_self: bool,
    pub online: bool,
    pub indexed_bytes: u64,
    pub last_seen: Option<jiff::Timestamp>,
}

impl NodeRecord {
    fn of(stored: Node) -> Result<Self> {
        Ok(Self {
            id: stored.id,
            node_id: NodeId::new(stored.node_id)?,
            name: stored.name,
            url: stored.url,
            is_self: stored.is_self,
            online: stored.online,
            indexed_bytes: stored.indexed_bytes,
            last_seen: parse_opt_timestamp(stored.last_seen)?,
        })
    }
}

impl Catalog {
    /// Records this machine as node 1 under its identity and display name, and keeps the
    /// manifest's node name in step with it.
    pub fn ensure_local_node(&self, node_id: &NodeId, name: &str) -> Result<NodeRecord> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Node> = transaction.get().primary(LOCAL_NODE)?;
        let node = Node {
            id: LOCAL_NODE,
            node_id: node_id.as_str().to_owned(),
            name: name.to_owned(),
            is_self: true,
            online: true,
            ..stored.unwrap_or_else(|| blank(LOCAL_NODE))
        };
        transaction.upsert(node.clone())?;
        let manifest: Option<Manifest> = transaction.get().primary(0_u8)?;
        if let Some(manifest) = manifest.filter(|manifest| manifest.node != name) {
            transaction.upsert(Manifest {
                node: name.to_owned(),
                ..manifest
            })?;
        }
        transaction.commit()?;
        NodeRecord::of(node)
    }

    /// This node first, then every peer in the order they were learned.
    pub fn list_nodes(&self) -> Result<Vec<NodeRecord>> {
        let snapshot = self.database.r_transaction()?;
        let mut stored: Vec<Node> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        stored.sort_unstable_by_key(|node| (!node.is_self, node.id));
        stored.into_iter().map(NodeRecord::of).collect()
    }

    pub fn node(&self, id: i64) -> Result<Option<NodeRecord>> {
        let stored: Option<Node> = self.database.r_transaction()?.get().primary(id)?;
        stored.map(NodeRecord::of).transpose()
    }

    pub fn node_by_name(&self, name: &str) -> Result<Option<NodeRecord>> {
        self.list_nodes()
            .map(|nodes| nodes.into_iter().find(|node| node.name == name))
    }

    pub fn node_by_id(&self, node_id: &NodeId) -> Result<Option<NodeRecord>> {
        let stored: Option<Node> = self
            .database
            .r_transaction()?
            .get()
            .secondary(NodeKey::node_id, node_id.as_str())?;
        stored.map(NodeRecord::of).transpose()
    }

    /// Adds a peer or refreshes its name and address. An absent address keeps the old one,
    /// because a peer learned over mDNS names one and a peer named in the config may not.
    pub fn upsert_peer(
        &self,
        node_id: &NodeId,
        name: &str,
        url: Option<&str>,
    ) -> Result<NodeRecord> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Node> = transaction
            .get()
            .secondary(NodeKey::node_id, node_id.as_str())?;
        let node = match stored {
            Some(stored) => Node {
                name: name.to_owned(),
                url: url.map(str::to_owned).or(stored.url),
                ..stored
            },
            None => Node {
                name: name.to_owned(),
                url: url.map(str::to_owned),
                node_id: node_id.as_str().to_owned(),
                ..blank(next_id::<Node>(&transaction)?)
            },
        };
        transaction.upsert(node.clone())?;
        transaction.commit()?;
        NodeRecord::of(node)
    }

    /// Records that a node answered, with how many bytes its index now covers.
    pub fn touch_node(&self, id: i64, at: jiff::Timestamp, indexed_bytes: u64) -> Result<()> {
        self.amend(id, |node| Node {
            online: true,
            indexed_bytes,
            last_seen: Some(at.to_string()),
            ..node
        })
    }

    pub fn set_node_online(&self, id: i64, online: bool) -> Result<()> {
        self.amend(id, |node| Node { online, ..node })
    }

    /// Records this machine under the name a scan ran with, so a catalog always lists
    /// itself even before an identity is known.
    pub(super) fn ensure_self_row(&self, name: &str) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Node> = transaction.get().primary(LOCAL_NODE)?;
        if stored.is_none() {
            transaction.insert(Node {
                node_id: folded_id(name),
                name: name.to_owned(),
                is_self: true,
                online: true,
                ..blank(LOCAL_NODE)
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(super) fn node_names(&self, snapshot: &RTransaction<'_>) -> Result<NodeNames> {
        NodeNames::load(snapshot, self.manifest()?.node)
    }

    fn amend(&self, id: i64, change: impl FnOnce(Node) -> Node) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Node> = transaction.get().primary(id)?;
        let stored = stored.ok_or_else(|| IndexError::not_found("node", id.to_string()))?;
        transaction.upsert(change(stored))?;
        transaction.commit()?;
        Ok(())
    }
}

/// What every entry, document and repository detail spells as its node, read once per
/// transaction. An id no node row claims reads as this node rather than failing: a row can
/// outlive the peer that filed it, and a listing must still render.
pub(super) struct NodeNames {
    local: String,
    by_row: HashMap<i64, String>,
    by_identity: HashMap<String, String>,
}

impl NodeNames {
    pub(super) fn load(snapshot: &RTransaction<'_>, local: String) -> Result<Self> {
        let stored: Vec<Node> = snapshot
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut by_row = HashMap::with_capacity(stored.len());
        let mut by_identity = HashMap::with_capacity(stored.len());
        for node in stored {
            by_identity.insert(node.node_id, node.name.clone());
            by_row.insert(node.id, node.name);
        }
        Ok(Self {
            local,
            by_row,
            by_identity,
        })
    }

    pub(super) fn of(&self, row: i64) -> &str {
        self.by_row.get(&row).map_or(&self.local, String::as_str)
    }

    /// The node whose rows one source holds: a peer's source names it, anything else is
    /// filled by a scan, an ingest or a pull and so belongs to this node.
    pub(super) fn of_source(&self, source: &str) -> &str {
        source
            .strip_prefix(PEER_SOURCE)
            .and_then(|identity| self.by_identity.get(identity))
            .map_or(&self.local, String::as_str)
    }
}

fn blank(id: i64) -> Node {
    Node {
        id,
        node_id: String::new(),
        name: String::new(),
        url: None,
        is_self: false,
        online: false,
        indexed_bytes: 0,
        last_seen: None,
        cursor: None,
        synced: None,
        pass: 0,
        filed: 0,
    }
}

/// A display name read as an identity, for a catalog that has not been told one.
fn folded_id(name: &str) -> String {
    let folded: String = name
        .chars()
        .map(|letter| match letter.is_ascii_alphanumeric() {
            true => letter,
            false => '-',
        })
        .collect();
    match folded.trim_matches('-') {
        "" => FALLBACK_ID.to_owned(),
        trimmed => trimmed.to_owned(),
    }
}
