//! Filesystem discovery, artifact sniffing, role inference, search grammar and the native
//! catalog.
//!
//! This crate owns every fact hoard knows about a file. It never serves HTTP and never
//! depends on axum: `hoard-mirror` and `hoard-server` read from it.

pub mod catalog;
pub mod config;
pub mod coordinate;
pub mod discover;
pub mod error;
pub mod hash;
pub mod infer;
pub(crate) mod layout;
pub mod projects;
pub mod row;
pub mod search;
pub mod shard;
pub mod sniff;
pub(crate) mod time;
pub mod triage;
pub mod watch;

pub use error::{IndexError, Result};

pub use layout::model_directory;
