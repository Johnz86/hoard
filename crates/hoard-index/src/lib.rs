//! Filesystem discovery, artifact sniffing, role inference, search grammar and the SQLite
//! index.
//!
//! This crate owns every fact hoard knows about a file. It never serves HTTP and never
//! depends on axum: `hoard-mirror` and `hoard-server` read from it.

pub mod config;
pub mod discover;
pub mod error;
pub mod hash;
pub mod infer;
pub mod scan;
pub mod search;
pub mod shard;
pub mod sniff;
pub mod store;
pub mod triage;
pub mod watch;

pub use error::{IndexError, Result};
