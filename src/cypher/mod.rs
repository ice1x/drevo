//! Cypher query language — Phase 10 of the drevo roadmap.
//!
//! The Cypher subsystem is built bottom-up over a sequence of roadmap
//! tasks: `00061` lexer (this module), `00062` parser, `00063` executor
//! (CREATE / MATCH / RETURN), `00064` mutations (SET / DELETE / MERGE),
//! `00065` WHERE, `00066` aggregations, `00067` OPTIONAL MATCH, `00068`
//! WITH, `00069` variable-length paths. Each layer plugs into the next
//! and reuses the existing `Drevo` storage API — Cypher is
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
/// Entity resolution: candidate pairs, merges and aliases
/// (`drevo.entity.*`).
pub(crate) mod entity_resolution;
/// Executor — walks the [`ast::Query`] produced by [`parser::parse`]
/// and runs it against a `Drevo` handle. Task `00063`.
pub mod executor;
pub mod expand;
pub mod hybrid;
/// Lexical analyser — turns a Cypher source string into a stream of
/// [`lexer::Token`]s for the parser.
pub mod lexer;
/// The config map of `drevo.pagerank` and `drevo.pagerank.personalized`.
pub mod pagerank_config;
/// Recursive-descent + Pratt parser that turns a `Cypher` source string
/// into an [`ast::Query`].
pub mod parser;
/// The `provenance` map of the long-term memory procedures and the chunk
/// list of `drevo.memory.addChunks`.
pub mod provenance;
/// Read-only classification for the engine flip — decides whether the
/// native read mirror may serve a parsed query (#307 Phase 6).
/// A small dependency-free regular-expression engine backing the Cypher
/// `=~` operator. Task `00140`.
pub mod regex;
pub mod security_admin;
