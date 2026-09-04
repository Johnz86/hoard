//! The query grammar: reading a query into tokens, compiling those tokens into SQL over the
//! `search_row` view, and the facet arithmetic a click depends on.
//!
//! Everything here is a pure transformation of strings and tokens. Nothing in this module
//! opens a database, so every rule the UI relies on is a unit test rather than a fixture.

pub mod facets;
pub mod grammar;
pub mod sql;
pub mod token;

pub use facets::{FACET_DIMENSIONS, Facets, mark_active, toggle_token};
pub use grammar::parse;
pub use sql::{
    CompatTarget, CompileContext, CompiledQuery, SortKey, SortSpec, compile, compile_excluding,
};
pub use token::{CompareOp, ParsedQuery, Token, TokenKind};
