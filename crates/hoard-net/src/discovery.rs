//! Finding the other nodes on the LAN.
//!
//! mDNS is the convenient way and the blocked way: plenty of networks drop multicast, and
//! a container often has no route for it at all. So the static `[peers]` list is not a
//! fallback bolted on afterwards; both sources feed the same channel of [`PeerEvent`], and
//! everything downstream of that channel cannot tell them apart.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use hoard_core::NodeId;
use hoard_index::store::Store;
use hoard_index::store::node::{NewNode, list_nodes, node_by_name, set_node_online, upsert_node};
use jiff::Timestamp;
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;

use crate::error::{NetError, Result};
use crate::identity::{NodeIdentity, SERVICE_TYPE};

/// Where a node looks for its peers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiscoveryMode {
    /// Advertise and browse `_hoard._tcp`, alongside any statically configured peers.
    #[default]
    Mdns,
    /// Use the configured peers only, which is what a network without multicast needs.
    Static,
    /// Find nothing; this node keeps to itself.
    None,
}

impl DiscoveryMode {
    /// Reads the `[node] discovery` spelling, defaulting to mDNS.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "static" => DiscoveryMode::Static,
            "none" | "off" => DiscoveryMode::None,
            _ => DiscoveryMode::Mdns,
        }
    }

    pub const fn uses_mdns(self) -> bool {
        matches!(self, DiscoveryMode::Mdns)
    }
}

/// How many peer events wait to be recorded before the oldest are refused.
///
/// Anything on the LAN can register a `_hoard._tcp` instance, so the queue in front of the
/// gossip loop is bounded like every other input from the network.
pub const ARRIVALS: usize = 256;

/// How many spellings of one name are tried before a peer is named after its node id.
const NAME_ATTEMPTS: u32 = 16;

/// A peer appearing or disappearing, whatever found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    Arrived(NodeIdentity),
    Departed(NodeId),
}

/// A running advertisement and browse.
///
/// Dropping it stops both. The daemon runs its own thread, so shutdown is explicit rather
/// than left to the reactor.
pub struct Discovery {
    daemon: Option<ServiceDaemon>,
    registered: Option<String>,
    browser: Option<JoinHandle<()>>,
    /// Held so the arrivals channel stays open for as long as discovery does, whether or
    /// not multicast is available to send anything down it.
    _sender: Sender<PeerEvent>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery")
            .field("advertising", &self.daemon.is_some())
            .field("service", &self.registered)
            .finish()
    }
}

impl Discovery {
    /// Starts discovery and returns the stream of peers it finds.
    ///
    /// Static peers are delivered before this returns, so a caller that never receives an
    /// mDNS packet still has everything `hoard.toml` named. A daemon that cannot start is
    /// logged and skipped: a node with no multicast route must still serve.
    pub fn start(
        identity: &NodeIdentity,
        mode: DiscoveryMode,
        statics: &[String],
    ) -> (Self, Receiver<PeerEvent>) {
        let (sender, receiver) = mpsc::channel(ARRIVALS);
        announce_statics(&sender, identity, statics);
        let mut discovery = Self {
            daemon: None,
            registered: None,
            browser: None,
            _sender: sender.clone(),
        };
        if mode.uses_mdns() {
            match start_mdns(identity, sender) {
                Ok((daemon, registered, browser)) => {
                    discovery.daemon = Some(daemon);
                    discovery.registered = Some(registered);
                    discovery.browser = Some(browser);
                }
                Err(error) => {
                    tracing::warn!(%error, "mDNS is unavailable; using the configured peers only");
                }
            }
        }
        (discovery, receiver)
    }

    /// Whether this node is advertising and browsing over multicast.
    pub fn is_advertising(&self) -> bool {
        self.daemon.is_some()
    }

    /// Withdraws the advertisement and stops browsing.
    pub fn stop(&mut self) {
        if let Some(browser) = self.browser.take() {
            browser.abort();
        }
        if let Some(daemon) = self.daemon.take() {
            if let Some(name) = self.registered.take() {
                let _ = daemon.unregister(&name);
            }
            if let Err(error) = daemon.shutdown() {
                tracing::debug!(%error, "the mDNS daemon was already stopped");
            }
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sends every statically configured peer as an arrival.
fn announce_statics(sender: &Sender<PeerEvent>, local: &NodeIdentity, statics: &[String]) {
    for address in statics {
        match NodeIdentity::from_address(address) {
            Ok(peer) if peer.url() == local.url() => {
                tracing::debug!(peer = %address, "the configured peer is this node");
            }
            Ok(peer) => {
                offer(sender, PeerEvent::Arrived(peer));
            }
            Err(error) => tracing::warn!(peer = %address, %error, "peer address is unusable"),
        }
    }
}

/// Offers one event to the gossip loop, and says whether anyone is still listening.
///
/// A full queue drops the event rather than waiting on it: discovery answers the network and
/// must not stall behind a loop that is busy pulling.
fn offer(sender: &Sender<PeerEvent>, event: PeerEvent) -> bool {
    match sender.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(dropped)) => {
            tracing::warn!(
                ?dropped,
                "the arrivals queue is full; a peer event was dropped"
            );
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

fn start_mdns(
    identity: &NodeIdentity,
    sender: Sender<PeerEvent>,
) -> Result<(ServiceDaemon, String, JoinHandle<()>)> {
    let daemon = ServiceDaemon::new().map_err(|error| NetError::Discovery(error.to_string()))?;
    let service = ServiceInfo::new(
        SERVICE_TYPE,
        &identity.instance_name(),
        &identity.host_name(),
        (),
        identity.port(),
        txt_properties(identity),
    )
    .map_err(|error| NetError::Discovery(error.to_string()))?
    .enable_addr_auto();
    let registered = service.get_fullname().to_owned();
    daemon
        .register(service)
        .map_err(|error| NetError::Discovery(error.to_string()))?;
    let events = daemon
        .browse(SERVICE_TYPE)
        .map_err(|error| NetError::Discovery(error.to_string()))?;
    let local = identity.clone();
    let browser = tokio::spawn(async move {
        let mut seen: HashMap<String, NodeId> = HashMap::new();
        while let Ok(event) = events.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(resolved) => {
                    if resolved.fullname == registered_name(&local) {
                        continue;
                    }
                    match peer_of(&resolved, &local) {
                        Ok(peer) => {
                            seen.insert(resolved.fullname.clone(), peer.id().clone());
                            if !offer(&sender, PeerEvent::Arrived(peer)) {
                                return;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(service = %resolved.fullname, %error, "peer ignored");
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    if let Some(id) = seen.remove(&fullname)
                        && !offer(&sender, PeerEvent::Departed(id))
                    {
                        return;
                    }
                }
                _ => {}
            }
        }
    });
    Ok((daemon, registered, browser))
}

/// The TXT record in the shape `mdns-sd` registers.
fn txt_properties(identity: &NodeIdentity) -> HashMap<String, String> {
    identity.txt_records().into_iter().collect()
}

fn registered_name(identity: &NodeIdentity) -> String {
    format!("{}.{SERVICE_TYPE}", identity.instance_name())
}

/// The identity a resolved service carries, with its resolved address as the fallback URL.
fn peer_of(resolved: &mdns_sd::ResolvedService, local: &NodeIdentity) -> Result<NodeIdentity> {
    let records: Vec<(String, String)> = resolved
        .txt_properties
        .iter()
        .map(|property| (property.key().to_owned(), property.val_str().to_owned()))
        .collect();
    let mut addresses: Vec<IpAddr> = resolved
        .addresses
        .iter()
        .map(ScopedIp::to_ip_addr)
        .collect();
    addresses.sort_by_key(|address| (address.is_ipv6(), *address));
    let address = addresses
        .first()
        .map(|ip| format!("http://{}", SocketAddr::new(*ip, resolved.port)));
    NodeIdentity::from_txt(&records, address.as_deref())?.disambiguated_from(local.id())
}

/// Records a peer in the index and marks it online, which is what the NODES tab reads.
///
/// Returns the node row every later gossip and pull is filed against.
pub async fn record_peer(store: &Store, peer: &NodeIdentity) -> Result<i64> {
    let id = peer.id().clone();
    let name = unique_name(store, peer).await?;
    let url = peer.url().to_string();
    let row = store
        .call_mut(move |conn| {
            let row = upsert_node(conn, &NewNode::peer(id, name, url))?;
            set_node_online(conn, row, true)?;
            Ok(row)
        })
        .await?;
    Ok(row)
}

/// Marks a peer offline without forgetting what it holds: its REMOTE entries stay, so the
/// index still knows where the bytes are when it comes back.
pub async fn forget_peer(store: &Store, id: &NodeId) -> Result<()> {
    let id = id.clone();
    store
        .call_mut(move |conn| {
            let peer = list_nodes(conn)?
                .into_iter()
                .find(|node| !node.is_self && node.node_id == id);
            match peer {
                Some(node) => set_node_online(conn, node.id, false),
                None => Ok(()),
            }
        })
        .await?;
    Ok(())
}

/// Records that a peer answered, with the bytes its index covers.
pub async fn touch_peer(store: &Store, row: i64, indexed_bytes: u64) -> Result<()> {
    let at = Timestamp::now();
    store
        .call_mut(move |conn| hoard_index::store::node::touch_node(conn, row, at, indexed_bytes))
        .await?;
    Ok(())
}

/// A display name no other node has already taken.
///
/// Two machines can be called `workstation`; the index is keyed by node id, but the NODES
/// tab and the `node:` facet are keyed by name, so a collision would merge them on screen.
async fn unique_name(store: &Store, peer: &NodeIdentity) -> Result<String> {
    let wanted = peer.name().to_owned();
    let id = peer.id().clone();
    let host = peer.host().to_owned();
    let name = store
        .call(move |conn| {
            let taken = |candidate: &str| -> hoard_index::Result<bool> {
                Ok(match node_by_name(conn, candidate)? {
                    Some(row) => row.node_id != id,
                    None => false,
                })
            };
            if !taken(&wanted)? {
                return Ok(wanted);
            }
            let qualified = format!("{wanted} ({host})");
            if !taken(&qualified)? {
                return Ok(qualified);
            }
            for ordinal in 2..=NAME_ATTEMPTS {
                let candidate = format!("{qualified} {ordinal}");
                if !taken(&candidate)? {
                    return Ok(candidate);
                }
            }
            Ok(format!("{wanted} ({})", id.as_str()))
        })
        .await?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use hoard_index::store::node::list_nodes;

    use super::*;

    fn local() -> NodeIdentity {
        NodeIdentity::new(
            NodeId::new("workstation-1").expect("id"),
            "workstation",
            "http://127.0.0.1:7099",
        )
        .expect("identity builds")
    }

    async fn store_with_local() -> Store {
        let store = Store::open_in_memory().expect("store opens");
        store
            .call_mut(|conn| {
                upsert_node(
                    conn,
                    &NewNode::local(NodeId::new("workstation-1").expect("id"), "workstation"),
                )?;
                Ok(())
            })
            .await
            .expect("local node records");
        store
    }

    #[test]
    fn the_discovery_mode_reads_the_config_spelling() {
        assert_eq!(DiscoveryMode::parse("mdns"), DiscoveryMode::Mdns);
        assert_eq!(DiscoveryMode::parse(" STATIC "), DiscoveryMode::Static);
        assert_eq!(DiscoveryMode::parse("none"), DiscoveryMode::None);
        assert_eq!(DiscoveryMode::parse("anything"), DiscoveryMode::Mdns);
        assert!(DiscoveryMode::default().uses_mdns());
        assert!(!DiscoveryMode::Static.uses_mdns());
    }

    #[tokio::test]
    async fn static_peers_arrive_on_the_same_channel_without_any_multicast() {
        let (mut discovery, mut events) = Discovery::start(
            &local(),
            DiscoveryMode::Static,
            &[
                "laptop.lan".to_owned(),
                "http://rig-02:7100".to_owned(),
                "http://127.0.0.1:7099".to_owned(),
                "  ".to_owned(),
            ],
        );
        assert!(!discovery.is_advertising());

        let mut found = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event {
                PeerEvent::Arrived(peer) => found.push(peer),
                PeerEvent::Departed(_) => unreachable!("nothing departed"),
            }
        }
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].url().as_str(), "http://laptop.lan:7099/");
        assert_eq!(found[0].id().as_str(), "laptop-lan-7099");
        assert_eq!(found[1].url().as_str(), "http://rig-02:7100/");
        discovery.stop();
    }

    #[tokio::test]
    async fn a_discovered_peer_becomes_an_online_node_row() {
        let store = store_with_local().await;
        let peer = NodeIdentity::from_address("laptop.lan:7099").expect("peer identity");
        let row = record_peer(&store, &peer).await.expect("peer records");
        let again = record_peer(&store, &peer).await.expect("peer re-records");
        assert_eq!(row, again);

        let nodes = store.call(list_nodes).await.expect("nodes read");
        assert_eq!(nodes.len(), 2);
        let recorded = &nodes[1];
        assert!(!recorded.is_self);
        assert!(recorded.online);
        assert_eq!(recorded.url.as_deref(), Some("http://laptop.lan:7099/"));

        touch_peer(&store, row, 4096).await.expect("peer answers");
        forget_peer(&store, peer.id()).await.expect("peer departs");
        let nodes = store.call(list_nodes).await.expect("nodes read");
        assert!(!nodes[1].online);
        assert_eq!(nodes[1].indexed_bytes, 4096);
    }

    #[tokio::test]
    async fn a_peer_never_takes_a_name_another_node_already_answers_to() {
        let store = store_with_local().await;
        let twin = NodeIdentity::new(
            NodeId::new("laptop-1").expect("id"),
            "workstation",
            "http://laptop.lan:7099",
        )
        .expect("identity builds");
        record_peer(&store, &twin).await.expect("peer records");
        let nodes = store.call(list_nodes).await.expect("nodes read");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].name, "workstation (laptop.lan)");
    }

    #[tokio::test]
    async fn peers_colliding_on_name_and_host_are_still_told_apart() {
        let store = store_with_local().await;
        for (id, port) in [("laptop-1", 7099), ("laptop-2", 7100), ("laptop-3", 7101)] {
            let twin = NodeIdentity::new(
                NodeId::new(id).expect("id"),
                "workstation",
                &format!("http://laptop.lan:{port}"),
            )
            .expect("identity builds");
            record_peer(&store, &twin).await.expect("peer records");
        }
        let mut names: Vec<String> = store
            .call(list_nodes)
            .await
            .expect("nodes read")
            .into_iter()
            .map(|node| node.name)
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 4, "no two nodes answer to one name");
    }

    #[tokio::test]
    async fn forgetting_an_unknown_peer_is_not_an_error() {
        let store = store_with_local().await;
        forget_peer(&store, &NodeId::new("nobody").expect("id"))
            .await
            .expect("an unknown peer is ignored");
    }

    /// Multicast is not available in every environment this suite runs in, so the mDNS path
    /// is exercised on request rather than on every build.
    #[tokio::test]
    #[ignore = "needs a multicast route"]
    async fn mdns_advertises_and_browses_the_hoard_service() {
        let (mut discovery, mut events) = Discovery::start(&local(), DiscoveryMode::Mdns, &[]);
        assert!(discovery.is_advertising());
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await;
        discovery.stop();
        match found {
            Ok(Some(PeerEvent::Arrived(peer))) => {
                assert_ne!(peer.url(), local().url(), "a node never browses itself");
            }
            Ok(Some(PeerEvent::Departed(_))) => unreachable!("nothing had arrived to depart"),
            Ok(None) => unreachable!("discovery holds the channel open"),
            Err(_) => {}
        }
    }
}
