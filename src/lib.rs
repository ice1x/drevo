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

/// The version string the running server reports — from `GET /`, `GET /status`,
/// the Bolt `server` agent, and the metrics `version` label.
///
/// Injected at build time by `build.rs` as the `DREVO_VERSION` env var, which
/// resolves (in order) the `DREVO_VERSION` build-arg the release image passes,
/// `git describe --tags` on a dev checkout, then `CARGO_PKG_VERSION`. It is
/// **not** `env!("CARGO_PKG_VERSION")`: the release flow keeps the git tag as
/// the source of truth and leaves `Cargo.toml` at `0.0.0`, so reporting the
/// crate version would show `0.0.0` on every deployed build.
pub const VERSION: &str = env!("DREVO_VERSION");

/// Short git commit the binary was built from, or `None` when unavailable
/// (e.g. a release image whose Docker context excludes `.git` and whose build
/// did not pass a `DREVO_GIT_SHA` build-arg). Surfaced by `CALL drevo.info()`
/// (issue #303). Injected by `build.rs` as the optional `DREVO_GIT_SHA` env.
pub const GIT_SHA: Option<&str> = option_env!("DREVO_GIT_SHA");

/// ISO-8601 build timestamp, or `None` when the build did not supply one.
/// Surfaced by `CALL drevo.info()` (issue #303). Injected by `build.rs` as the
/// optional `DREVO_BUILD_DATE` env.
pub const BUILD_DATE: Option<&str> = option_env!("DREVO_BUILD_DATE");

/// Coarse capability/protocol level clients can gate on without parsing semver
/// (issue #303). Bumped when the Bolt/Cypher surface gains a
/// backwards-incompatible-to-assume capability. Surfaced by `CALL drevo.info()`.
pub const INFO_PROTOCOL: i64 = 1;

/// Built-in global graph algorithms — Phase 15 task `00098`. Adds the two
/// whole-graph analytics algorithms that complement the local traversals in
/// [`traversal`]: [`algorithms::pagerank`] (weighted PageRank centrality via
/// power iteration) and [`algorithms::louvain`] (Louvain community detection by
/// multi-level modularity optimisation). Both run over an in-memory
/// [`algorithms::AdjacencyList`] snapshot and are exposed on the database facade
/// as [`db::Drevo::pagerank`] / [`db::Drevo::louvain_communities`] (mirroring
/// the existing [`db::Drevo::shortest_path`] Dijkstra wiring). Dependency-free,
/// always compiled, and WASM-safe; keeps its own [`algorithms::AlgorithmError`]
/// channel for config validation rather than widening `DrevoError`.
pub mod algorithms;
/// HTTP API surface — axum router translating JSON requests into
/// [`db::Drevo`] calls. Compiled only with the `http` feature.
#[cfg(feature = "http")]
pub mod api;
/// Authorization & role-based access control — Phase 15 task `00094`. A
/// dependency-free, always-compiled RBAC engine: [`authz::Action`]s scoped by
/// [`authz::Scope`], bundled into reusable [`authz::Role`]s (with inheritance
/// and `reader`/`editor`/`admin` presets), evaluated by the
/// [`authz::AccessPolicy`] engine under deny-overrides, closed-world semantics
/// into an authorization [`authz::Decision`]. The authorization half that
/// pairs with the Bolt authentication layer ([`bolt::auth`], task `00074`).
/// Keeps its own [`authz::AuthzError`] channel; not yet wired into the
/// executor / HTTP API / Bolt session. WASM-safe.
pub mod authz;
/// Bolt wire protocol — Phase 11. Task `00070` ships the bytes-on-the-
/// wire layer (PackStream codec, chunked framing, handshake + async
/// TCP listener). The session layer (HELLO / RUN / PULL / DISCARD /
/// RESET / GOODBYE on top of [`db::Drevo`]) lands in task `00071`.
/// Not built on `wasm32-unknown-unknown`.
#[cfg(not(target_arch = "wasm32"))]
pub mod bolt;
/// Named-database catalog — manage multiple [`db::Drevo`] databases (one
/// redb file each) in a single process, with create / list / switch. Gated
/// on `http`: its only consumers are the HTTP API and the server binary, so
/// it is absent from the `wasm` build (which has neither).
#[cfg(feature = "http")]
pub mod catalog;
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
/// OpenAI-compatible text-embedding endpoint (`/v1/embeddings`, Phase 19,
/// issue #217). Gated on `http`: its only consumer is the HTTP server. The
/// `reqwest`-backed proxy backend is further gated on `embeddings-proxy`.
#[cfg(feature = "http")]
pub mod embeddings;
/// The `GraphEngine` seam (RFC `docs/rfc-native-core.md`, issue #307) — the
/// graph-level abstraction (nodes / edges / adjacency) the query layers will
/// depend on instead of a concrete store's KV-encoded internals. Introduced
/// additively: [`db::Drevo`] implements it by delegating to its inherent
/// methods, so a future native `drevo-core` engine can be a drop-in
/// alternative.
pub mod engine;
pub mod error;
/// `extern "C"` FFI surface for desktop / mobile embedders. Not built on
/// `wasm32-unknown-unknown` because the platform has no C ABI.
#[cfg(not(target_arch = "wasm32"))]
pub mod ffi;
pub mod fts;
/// Cross-engine data migration (RFC `docs/rfc-native-core.md`, #307). Moves a
/// live graph between any two [`engine::GraphEngine`] implementations
/// (KV-backed [`db::Drevo`] ⇄ native [`native::NativeGraph`]) over the shared
/// `drevo-json-v1` dump interchange, preserving every node/edge id.
pub mod migrate;
/// Hybrid Logical Clock ([`hlc::Hlc`] / [`hlc::HlcClock`]) — the causal
/// versioning primitive for multi-writer / P2P convergence (issue #389,
/// primitive #1). Re-exported from [`drevo-core`](drevo_core).
pub use drevo_core::hlc;
/// Core domain types ([`model::Node`], [`model::Edge`], …) — re-exported from
/// the extracted [`drevo-core`](drevo_core) crate so `drevo::model::…` and
/// in-crate `crate::model::…` paths keep resolving unchanged.
pub use drevo_core::model;
/// Multi-version concurrency control — Phase 13 task `00081`. The
/// transaction-id allocator + commit log ([`mvcc::TransactionManager`]),
/// snapshot capture ([`mvcc::Snapshot`]), `xmin`/`xmax` tuple versioning
/// ([`mvcc::Version`]), and the snapshot-isolated [`mvcc::VersionedStore`]
/// the rest of Phase 13 (GC `00082`, OCC `00083`, isolation levels
/// `00084`) builds on. Dependency-free and always compiled.
pub mod mvcc;
/// The native in-memory graph engine (RFC `docs/rfc-native-core.md`, #307,
/// Phase 2) — an implementation of [`engine::GraphEngine`] that holds nodes and
/// edges directly with maintained adjacency, instead of encoding them as
/// byte-keyed rows over a [`storage::StorageBackend`]. Correctness-first seed of
/// the arena/CSR core; pinned against [`db::Drevo`] by differential test.
/// Extracted to the [`drevo-core`](drevo_core) crate (Phase 7 slice 6) and
/// re-exported so `crate::native::…` / `drevo::native::…` paths keep resolving.
pub use drevo_core::native;
/// In-memory full-text index that tails a [`native::NativeGraph`]'s change-feed
/// (RFC `docs/rfc-native-core.md`, #307, Phase 6.3) — the first secondary index
/// kept current off the graph seam, matching the KV store's trigram BM25
/// full-text semantics so `fts.search` can be served on the native engine.
/// Extracted to `drevo-core` (Phase 7 slice 7) and re-exported.
pub use drevo_core::native_fts;
/// In-memory secondary-label index that tails a [`native::NativeGraph`]'s
/// change-feed (RFC `docs/rfc-native-core.md`, #307, Phase 6.6) — indexes the
/// `_labels` Cypher labels the primary-kind index does not cover, so a native
/// `MATCH (n:Label)` gathers candidates from an index union instead of a full
/// node scan. Extracted to `drevo-core` (Phase 7 slice 6) and re-exported.
pub use drevo_core::native_label_index;
/// HTTP surface for the durable-native server mode
/// (`DREVO_ENGINE=native-durable`, RFC `docs/rfc-native-core.md`, #307,
/// Phase 4/7) — a minimal router over [`native_service::NativeService`]:
/// liveness, identity, and Cypher. Compiled with the `http` feature.
pub mod native_api;
/// Native read mirror — the engine-flip execution router (RFC
/// `docs/rfc-native-core.md`, #307, Phase 6 slice A). Serves fresh read-only
/// Cypher from a [`native::NativeGraph`] snapshot with the native indexes and
/// value cache synced; routes every write (and any stale or non-mirrorable
/// read) to the durable KV engine, detecting staleness via
/// [`db::Drevo::mutation_epoch`].
pub mod native_mirror;
/// Durable-native serving layer (RFC `docs/rfc-native-core.md`, #307,
/// Phase 4/7) — a WAL-backed [`native::NativeGraph`] as the store of
/// record, serving Cypher with the full native index stack (label,
/// property, value cache, full-text) tailed off the change-feed. The
/// step past the read mirror on the track toward retiring redb.
pub mod native_service;
/// In-memory property-value index that tails a [`native::NativeGraph`]'s
/// change-feed (RFC `docs/rfc-native-core.md`, #307, Phase 6.7) — the native
/// counterpart of the KV [`property_index`], so a `MATCH (n {key: value})`
/// equality pattern resolves through an index instead of a full node scan.
/// Extracted to `drevo-core` (Phase 7 slice 6) and re-exported.
pub use drevo_core::native_property_index;
/// Change-feed-maintained memo of the executor's `NodeValue` projection
/// ([`native_value_cache::NativeValueCache`]) — a hit is validated against the
/// live record with `Arc::ptr_eq`, so a stale cache costs speed, never answers
/// (RFC `docs/rfc-native-core.md`, #307).
pub mod native_value_cache;
/// Observability — Phase 15 task `00130`. A dependency-free, lock-free
/// metrics registry ([`observability::Registry`] with
/// [`observability::Counter`] / [`observability::Gauge`] /
/// [`observability::Histogram`]) that renders the Prometheus text exposition
/// format, plus a structured query log ([`observability::QueryObservation`] /
/// [`observability::DrevoMetrics::record_query`]) that updates the standard
/// [`observability::DrevoMetrics`] and emits an OpenTelemetry-semantic
/// `tracing` event. Always compiled and WASM-safe; the `/metrics` HTTP route
/// and per-request instrumentation live in [`api`] behind the `http` feature.
pub mod observability;
/// Extracted env-var parser, validator, and bind/serve loop for the
/// `drevo-server` binary. Compiled only with the `http` feature.
/// Phase 14 task `00085` — cost-based query planner foundation: graph
/// statistics ([`planner::GraphStatistics`]) + cardinality estimation
/// ([`planner::CardinalityEstimator`]) + the annotated plan tree
/// ([`planner::PlanNode`], with [`planner::PlanNode::explain`]) + a bounded
/// plan cache ([`planner::PlanCache`]). Task `00089` adds the memory budget &
/// backpressure submodule ([`planner::MemoryBudget`],
/// [`planner::estimate_peak_memory`], [`planner::Backpressure`]) — the OOM
/// guard for memory-limited query execution. Dependency-free, always compiled,
/// WASM-safe; not yet wired into the executor.
pub mod planner;
/// Phase 14 task `00088` — persistent property index. A durable
/// `(property key, value) -> node ids` map maintained on every node
/// mutation alongside the kind and FTS indexes, turning equality lookups
/// (`MATCH (n {prop: value})`) into an `O(matches)` prefix scan instead of
/// an `O(N)` full-node scan. Queried via [`db::Drevo::nodes_by_property`].
pub mod property_index;
/// Phase 15 task `00095` — WAL-based MAIN / REPLICA replication. A
/// [`replication::Primary`] tees every write into a
/// [`replication::WriteAheadLog`] of [`replication::WalRecord`]s (each stamped
/// with a monotonic [`replication::Lsn`]); read-only
/// [`replication::Replica`] followers replay that log in order to serve scaled
/// reads. Dependency-free, always compiled, WASM-safe; keeps its own
/// [`replication::ReplicationError`] channel and is not yet wired into the
/// executor / HTTP / Bolt request path.
pub mod replication;
/// Semantic-index state machine (Phase 21) — the pure, dependency-free control
/// plane that governs whether and how a `(label, property)` is auto-embedded
/// for semantic search. Off by default; serialisable for redb persistence and
/// the `drevo.embeddings.*` Cypher procedures. Performs no embedding itself.
pub mod semantic_index;
#[cfg(feature = "http")]
pub mod server;
pub mod storage;
/// Phase 15 task `00096` — streaming ingestion. A transport-agnostic engine
/// that turns a broker firehose of change events into graph mutations: an
/// [`streaming::IngestConsumer`] polls a [`streaming::StreamSource`] (Kafka /
/// NATS / CDC — drevo ships the in-memory [`streaming::MemorySource`]), decodes
/// each message into a tagged-JSON [`streaming::IngestEvent`], and applies it to
/// an [`streaming::IngestSink`] (the reference [`streaming::MemoryGraphSink`])
/// under an [`streaming::ErrorPolicy`], tracking [`streaming::Offset`]s for
/// at-least-once, idempotent ingestion with a [`streaming::DeadLetter`] queue.
/// Dependency-free, always compiled, WASM-safe; keeps its own
/// [`streaming::StreamError`] channel and is not yet wired into the executor /
/// HTTP / Bolt request path.
pub mod streaming;
pub mod traversal;
/// Phase 12 task `00075` — vector value type + similarity / distance
/// functions (cosine, euclidean, dot product) over `f32` embeddings.
/// Dependency-free and always compiled; the building block the HNSW
/// index (`00076`) and joint graph+vector Cypher queries (`00077`) sit
/// on top of.
pub mod vector;
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
