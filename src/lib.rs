//! drevo — an embedded graph database for cross-platform knowledge
//! management.
//!
//! drevo is a lightweight, embeddable graph database written in Rust. It is
//! the storage engine for a cross-platform graph notebook and runs natively
//! on desktop (via FFI / Tauri), on mobile (iOS / Android via C bindings),
//! and in the browser (via WebAssembly). It also ships as a standalone HTTP
//! server for containerised deployments.
//!
//! The crate is organised into a small number of layered modules, each with
//! its own audit report under `audit/AUDIT-<domain>.md`:
//!
//! - [`storage`] — pluggable [`storage::StorageBackend`] trait plus the two
//!   shipping implementations: [`storage::MemoryBackend`] (ephemeral, the
//!   only backend on WASM) and `storage::RedbBackend` (ACID + B-tree, the
//!   default on native targets, gated behind the `redb-backend` Cargo
//!   feature). Audited in `audit/AUDIT-storage.md` (task `00103`).
//! - [`error`] — the crate-wide [`error::DrevoError`] hierarchy. Audited in
//!   `audit/AUDIT-error.md` (task `00104`).
//! - [`model`] — public data types ([`model::Node`], [`model::Edge`], …)
//!   plus their `New*` / `*Patch` companions and the shared
//!   [`model::Properties`] map. Audited in `audit/AUDIT-model.md` (task
//!   `00105`).
//! - [`db`] — the [`db::Drevo`] handle: node / edge CRUD, kind index, FTS
//!   index maintenance, transactional list operations. Audited in
//!   `audit/AUDIT-db.md` (task `00106`).
//! - [`traversal`] — BFS / DFS / shortest-path / weighted-shortest-path /
//!   subgraph extraction, with kind filters. Audited in
//!   `audit/AUDIT-traversal.md` (task `00107`).
//! - [`fts`] — trigram tokenizer + inverted index for full-text search.
//!   Audited in `audit/AUDIT-fts.md` (task `00108`).
//! - `api` (cfg `http`) — axum router translating REST requests into
//!   [`db::Drevo`] calls. Audited in `audit/AUDIT-http-api.md` (task
//!   `00109`).
//! - `ffi` (non-WASM) — `extern "C"` surface for desktop / mobile
//!   embedders. Every entry is wrapped in [`std::panic::catch_unwind`].
//!   Audited in `audit/AUDIT-ffi.md` (task `00110`).
//! - `wasm` (cfg `wasm`) — `wasm-bindgen` exports for the browser /
//!   Tauri-WASM build. Audited in `audit/AUDIT-wasm.md` (task `00111`).
//! - `server` (cfg `http`) — extracted env-var parser, validator, and
//!   bind/serve loop for the `drevo-server` binary. Audited in
//!   `audit/AUDIT-server.md` (task `00112`).
//!
//! Cross-cutting compliance (MSRV, doc coverage, dead-code, `make audit`,
//! coverage) is audited in `audit/AUDIT-crosscut.md` (task `00113`).
//!
//! See [`README.md`](https://github.com/ice1x/drevo#readme) for the user
//! guide, the data model, the storage layout, and the long-term roadmap
//! toward Cypher (Phase 10), Bolt wire protocol (Phase 11), and native
//! vector search (Phase 12).
#![warn(missing_docs)]

/// HTTP API surface — axum router translating JSON requests into
/// [`db::Drevo`] calls. Compiled only with the `http` feature.
#[cfg(feature = "http")]
pub mod api;
/// Cypher query language — Phase 10. Today only the lexer (task `00061`)
/// is implemented; the parser, executor, and downstream clause handlers
/// will land in tasks `00062` onwards.
pub mod cypher;
pub mod db;
/// JSON import / export — Phase 9 task `00055`. Defines the schema-versioned
/// `drevo-json-v1` wire format plus the `Drevo::export_json` / `import_json`
/// methods. Filesystem-bound `*_to_path` / `*_from_path` variants are gated
/// off WASM.
pub mod dump;
pub mod error;
/// `extern "C"` FFI surface for desktop / mobile embedders. Not built on
/// `wasm32-unknown-unknown` because the platform has no C ABI.
#[cfg(not(target_arch = "wasm32"))]
pub mod ffi;
pub mod fts;
/// Model Context Protocol (MCP) server — Phase 15 task `00090`. Stdio
/// JSON-RPC 2.0 binding so AI clients (Cline / Claude Code / Claude
/// Desktop) can drive an embedded [`db::Drevo`] handle without going
/// through Docker / HTTP / Bolt. Pure-Rust, no extra dependencies
/// beyond `serde_json` + `tracing` (already in the tree). Not built on
/// `wasm32-unknown-unknown` because stdio has no meaning there.
#[cfg(not(target_arch = "wasm32"))]
pub mod mcp;
pub mod model;
/// Extracted env-var parser, validator, and bind/serve loop for the
/// `drevo-server` binary. Compiled only with the `http` feature.
#[cfg(feature = "http")]
pub mod server;
pub mod storage;
pub mod traversal;
/// `wasm-bindgen` exports for the browser / Tauri-WASM build. Compiled
/// only with the `wasm` feature.
#[cfg(feature = "wasm")]
pub mod wasm;
/// Phase 15 task `00092` — embedded Web UI handlers (HTML / JS / CSS
/// shipped via `include_str!`). Cytoscape.js graph explorer served by
/// the same `axum` router as the HTTP API. Compiled only with the
/// `http` feature.
#[cfg(feature = "http")]
pub mod web_ui;
