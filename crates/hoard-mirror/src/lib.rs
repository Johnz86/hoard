//! HuggingFace Hub protocol: local repo serving, upstream proxy and block-level file cache.
//!
//! Transport agnostic by design. Handlers in `hoard-server` turn the values produced here
//! into axum responses; nothing in this crate knows what axum is.

pub mod access;
pub mod batch;
pub mod cache;
pub mod config;
pub mod error;
pub mod git;
pub mod ingest;
pub mod local;
pub mod model;
pub mod resolve;
pub mod upstream;
pub mod usage;

pub use error::{MirrorError, Result};
