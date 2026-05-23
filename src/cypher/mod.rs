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
//! Only the lexer module is implemented today. It produces the `Token`
//! stream that the upcoming parser will consume.

/// Lexical analyser — turns a Cypher source string into a stream of
/// [`lexer::Token`]s for the parser.
pub mod lexer;
