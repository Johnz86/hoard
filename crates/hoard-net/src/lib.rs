//! Node identity, LAN discovery, index gossip and peer pulls.
//!
//! Every install is a node. Nodes find each other on the LAN, exchange compact index
//! digests, and pull from each other through the Hub face, so a peer is just another
//! upstream rather than a second protocol.
//!
//! A server starts the whole of it with one call:
//!
//! ```no_run
//! # use std::time::Duration;
//! # use hoard_core::NodeId;
//! # use hoard_index::store::Store;
//! # use hoard_net::{DiscoveryMode, GossipConfig, NodeIdentity, spawn};
//! # fn wire(store: Store, id: NodeId) -> Result<(), hoard_net::NetError> {
//! let identity = NodeIdentity::new(id, "rig-02", "http://rig-02:7099")?;
//! let handle = spawn(
//!     store,
//!     GossipConfig::new(identity)
//!         .with_discovery(DiscoveryMode::Mdns)
//!         .with_statics(vec!["laptop.lan:7099".to_owned()]),
//! );
//! # handle.stop();
//! # Ok(())
//! # }
//! ```

pub mod digest;
pub mod discovery;
pub mod error;
pub mod identity;
pub mod peer;

pub use digest::{DigestCursor, DigestPage, DigestRow};
pub use discovery::{Discovery, DiscoveryMode, PeerEvent};
pub use error::{NetError, Result};
pub use identity::{NodeIdentity, SERVICE_TYPE};
pub use peer::{
    GossipConfig, GossipHandle, PULL_KIND, PeerClient, PullOutcome, PullRequest, Puller,
    StateFacts, derive_state, pin_is_stale, spawn,
};
