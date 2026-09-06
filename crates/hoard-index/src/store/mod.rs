//! SQLite index. Every read and write of hoard's knowledge goes through here.

pub mod artifact;
pub mod blob;
pub(crate) mod convert;
pub mod digest;
pub mod entry;
#[cfg(test)]
pub(crate) mod fixture;
pub mod job;
pub mod loadout;
pub mod migrations;
pub mod node;
pub mod overrides;
pub mod pool;
pub mod projection;
pub mod repo;
pub mod revision;
pub mod row;
pub mod search_exec;
pub mod setting;
pub mod shelf;
pub mod source;
pub mod usage;

pub use pool::{Reader, Store, Writer};
pub use row::{DiskPressure, FacetBucket, SEARCH_COLUMNS, SearchRow, SqlValue};

/// The schema as applied by migration 1. Kept as a file so it reads as SQL.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Migration 2: the columns a sharded model needs, and the view that reads them.
pub const SCHEMA_V2_SQL: &str = include_str!("schema_v2.sql");
