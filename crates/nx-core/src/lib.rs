//! nx-core: native reader, analyzer, linters, store and queries for nx.
//! No Mova deps.

pub mod actions;
pub mod analyzer;
pub mod clojuredocs;
pub mod cli;
pub mod cst;
pub mod engine;
pub mod fmt;
pub mod io;
pub mod jars;
pub mod jdk;
pub mod met;
pub mod mova;
pub mod intern;
pub mod query;
pub mod reader;

pub use cst::{Cst, Kind, Node, NodeId, ParseError, Pos};
pub use intern::{intern, resolve, SymId};
pub use reader::{parse, parse_owned};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
