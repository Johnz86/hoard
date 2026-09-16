//! The query grammar: reading a query into tokens, the context those tokens compile
//! against, and the facet arithmetic a click depends on.
//!
//! Everything here is a pure transformation of strings and tokens. Nothing in this module
//! opens a database, so every rule the UI relies on is a unit test rather than a fixture.
//! The compiler that turns tokens into a search over the catalog lives beside the catalog.

pub mod brands;
pub mod context;
pub mod facets;
pub mod grammar;
pub mod token;

pub use context::{CompatTarget, CompileContext, SortKey, SortSpec};
pub use facets::{FACET_DIMENSIONS, Facets, mark_active, quoted, toggle_token};
pub use grammar::parse;
pub use token::{CompareOp, ParsedQuery, Token, TokenKind};
