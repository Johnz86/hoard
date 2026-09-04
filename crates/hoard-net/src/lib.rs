//! Node identity, LAN discovery, index gossip and peer pulls.
//!
//! Every install is a node. Nodes find each other on the LAN, exchange compact index
//! digests, and pull from each other through the Hub face, so a peer is just another
//! upstream rather than a second protocol.

pub mod digest;
pub mod discovery;
pub mod error;
pub mod identity;
pub mod peer;

pub use error::{NetError, Result};
