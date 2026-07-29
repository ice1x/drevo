//! Cypher query language — Phase 10 of the drevo roadmap.
//!
//! The Cypher subsystem is built bottom-up over a sequence of roadmap
//! tasks: `00061` lexer (this module), `00062` parser, `00063` executor
//! (CREATE / MATCH / RETURN), `00064` mutations (SET / DELETE / MERGE),
//! `00065` WHERE, `00066` aggregations, `00067` OPTIONAL MATCH, `00068`
//! WITH, `00069` variable-length paths. Each layer plugs into the next
//! and reuses the existing [`crate::db::Drevo`] storage API — Cypher is
//! a thin query layer, not a new storage engine.
//!
//! The lexer (task `00061`) produces the `Token` stream that the parser
//! (task `00062`) consumes; the parser produces the `Query` AST tree
//! (see the `ast` module) that the upcoming executor (task `00063`)
//! will walk.

/// Abstract syntax tree types produced by [`parser::parse`] and consumed
/// by the executor (task `00063`). See the module docs for the shape of
/// the tree.
/// Catalog-level admin commands (`SHOW DATABASES`, `USE`, `CREATE
/// DATABASE`) recognised at the string level and handled by the
/// catalog-aware caller, outside the single-database graph executor.
pub mod admin;
pub mod ast;
/// Executor — walks the [`ast::Query`] produced by [`parser::parse`]
/// and runs it against a [`crate::db::Drevo`] handle. Task `00063`.
pub mod executor;
/// Lexical analyser — turns a Cypher source string into a stream of
/// [`lexer::Token`]s for the parser.
pub mod lexer;
/// Recursive-descent + Pratt parser that turns a `Cypher` source string
/// into an [`ast::Query`].
pub mod parser;
/// A small dependency-free regular-expression engine backing the Cypher
/// `=~` operator. Task `00140`.
pub mod regex;
