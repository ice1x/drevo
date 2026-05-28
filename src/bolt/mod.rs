//! Bolt wire protocol — Phase 11 task `00070`.
//!
//! Goal: speak Neo4j's Bolt protocol on the wire so existing official
//! drivers (`cypher-shell`, `neo4j-python-driver`, `neo4j-javascript-driver`)
//! can connect to drevo without code changes. This module is the
//! protocol *layer* — it owns the bytes on the wire but does not yet
//! drive a Cypher query. The Bolt session layer (HELLO / RUN / PULL /
//! DISCARD / RESET / GOODBYE) lands in task `00071` on top of this
//! foundation.
//!
//! ## Module layout
//!
//! - [`crate::bolt::error`] — [`crate::bolt::error::BoltError`] (kept
//!   separate from [`crate::error::DrevoError`] so the layered modules
//!   don't have to match against Bolt-specific failure modes — see the
//!   module-level docs in [`crate::bolt::error`] for the rationale).
//! - [`crate::bolt::packstream`] — value model + binary encoder /
//!   decoder for the PackStream serialisation format that every Bolt
//!   message uses.
//! - [`crate::bolt::chunked`] — 16-bit-prefixed chunked-message
//!   framing. Pure `std::io::Read` / `Write`, no async — that way the
//!   codec is testable without spawning a runtime.
//! - [`crate::bolt::handshake`] — magic preamble check, four-slot
//!   version negotiation, and the supported-version registry.
//! - `crate::bolt::listener` (feature `http`) — async `tokio` TCP
//!   listener that runs the handshake; gated on `http` because that's
//!   already what pulls `tokio` into the build graph.
//!
//! ## Why a hand-rolled codec
//!
//! Like the MCP module (Phase 15 task `00090`), the surface area we
//! need is small enough to write from scratch — PackStream is roughly
//! a dozen marker buckets, the framing layer is a 2-byte prefix, and
//! the handshake is a 20-byte exchange. Pulling in a published Bolt
//! crate would add transitive dependencies (often async runtimes,
//! futures glue, sometimes a partial Neo4j driver) for code that
//! amounts to a few hundred lines. The hand-rolled codec also lets us
//! pin byte-for-byte expectations in tests, which a thirdparty
//! library would not let us hold steady across version bumps.
//!
//! Not built on `wasm32-unknown-unknown` because TCP listeners have
//! no meaning there. The codec itself is portable, but exposing only
//! a wire protocol with no transport would just be dead code.

#![cfg(not(target_arch = "wasm32"))]

pub mod chunked;
pub mod error;
pub mod handshake;
pub mod packstream;

/// Async TCP listener for incoming Bolt connections. Compiled only
/// with the `http` feature (the same feature that pulls `tokio` in
/// for the HTTP server).
#[cfg(feature = "http")]
pub mod listener;
