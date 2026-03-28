# GraphNote DB — Embedded Graph Database for Knowledge Management

Build a lightweight, embeddable graph database in Rust, purpose-built for a cross-platform Obsidian-like knowledge base application. The database must run natively on desktop (via FFI/Tauri), mobile (iOS/Android via C bindings or WASM), and in the browser (via WebAssembly). No server required — everything runs in-process.

---

## Use Cases

GraphNote DB is the storage engine for a cross-platform graph notebook. Target scenarios:

### CBT Journal (Cognitive Behavioral Therapy)

Nodes: `thought`, `emotion`, `situation`, `cognitive_distortion`, `rational_response`. Edges: `triggered_by`, `leads_to`, `challenges`, `reframed_as`. The graph enables tracing chains of thoughts and finding recurring distortion patterns via traversal.

### Scenario / Book / Story Editor

Tree-structured narratives: nodes are `chapter`, `scene`, `character`, `location`, `plot_point`. Edges: `contains`, `follows`, `involves`, `takes_place_in`. Subgraph extraction gives a complete context for a scene. MCP integration allows AI agents to read/write the graph for co-authoring.

### IT Task Manager

Nodes: `task`, `epic`, `sprint`, `developer`, `component`. Edges: `assigned_to`, `blocks`, `part_of`, `depends_on`. BFS from a blocked task reveals the full dependency chain. Kind index enables board views (all tasks in a sprint).

### ERP System

Nodes: `order`, `product`, `customer`, `warehouse`, `invoice`. Edges: `ordered_by`, `contains`, `stored_in`, `billed_to`. Transactions ensure consistency when updating order status and inventory simultaneously.

### Bug Tracker / Control System

Nodes: `bug`, `feature`, `release`, `test_case`, `assignee`. Edges: `reported_in`, `fixed_by`, `verified_by`, `blocks_release`. FTS over bug descriptions, traversal for impact analysis.

### Common patterns across all scenarios

- **Node kinds** define domain entities — the `kind` field + `kind_index` provide filtered views
- **Edge kinds** define relationships — `scan_prefix` retrieves all edges of a given type
- **Properties** (HashMap) store domain-specific metadata without schema migration
- **FTS** enables search across all content (titles, bodies, properties)
- **Subgraph** extraction provides bounded context for AI agents (MCP)
- **Transactions** ensure consistency for multi-step operations
- **Cross-platform**: all scenarios must work identically on desktop, mobile (iOS/Android), and WASM

---

## Core Requirements

### Platform targets

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin` / `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-apple-ios` / `aarch64-linux-android`
- `wasm32-unknown-unknown` (browser, Tauri v2 WASM)

### Deployment targets

- **Embedded (in-process)**: Tauri desktop, iOS/Android via C FFI, browser via WASM
- **Containerized (server mode)**: Docker image with HTTP API, Kubernetes-ready
  - Official Docker image published to registry (like PostgreSQL, Redis, Neo4j)
  - Helm chart / K8s manifests for orchestrated deployments
  - Volume-based persistence (`/data`), health checks, graceful shutdown

### Non-goals

- No SQL compatibility layer
- No distributed/cluster support
- No ACID transactions across network

---

## Data Model

### Node

```
id:         u64            (auto-increment, unique)
uuid:       [u8; 16]       (UUID v7, sortable, globally unique)
kind:       String         (e.g. "note", "tag", "person", "concept")
title:      String
body:       String         (raw Markdown)
body_html:  String         (rendered, cached)
created_at: i64            (Unix ms)
updated_at: i64            (Unix ms)
properties: HashMap<String, Value>   (arbitrary JSON-compatible metadata)
```

### Edge

```
id:         u64
uuid:       [u8; 16]
from_id:    u64            (source node)
to_id:      u64            (target node)
kind:       String         (e.g. "links_to", "tagged_with", "derived_from", "alias_of")
weight:     f32            (default 1.0, used for ranking/traversal)
created_at: i64
properties: HashMap<String, Value>
```

### Index entries (internal)

- `title_idx`:    `BTreeMap<String, u64>`
- `kind_idx`:     `BTreeMap<String, Vec<u64>>`
- `fts_idx`:      inverted index over `title + body` (trigram or BM25)
- `updated_idx`:  `BTreeMap<i64, u64>` (for recent notes)

---

## Storage Engine

### File layout

```
<vault_dir>/
  graphnote.db          <- single binary file (redb)
  graphnote.db.lock     <- advisory lock
  graphnote.db.wal      <- write-ahead log (optional, for crash recovery)
```

### Backend: redb

[redb](https://github.com/cberner/redb) — pure Rust, no C dependencies, ACID transactions, WASM-compatible, actively maintained.

Alternative if redb has WASM issues: [sled](https://github.com/spacejam/sled).

### Storage abstraction trait

```rust
pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&self, key: &[u8]) -> Result<()>;
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn flush(&self) -> Result<()>;
}
```

Two backends planned: `MemoryBackend` (BTreeMap) and `RedbBackend` (ACID, B-tree).

### Tables in redb

```
nodes:       u64 -> bincode(Node)
edges:       u64 -> bincode(Edge)
node_uuid:   [u8;16] -> u64
edge_uuid:   [u8;16] -> u64
out_edges:   u64 -> Vec<u64>      (adjacency list: from_id -> edge_ids)
in_edges:    u64 -> Vec<u64>      (reverse: to_id -> edge_ids)
kind_index:  String -> Vec<u64>
title_index: String -> u64        (exact title lookup)
fts_index:   String -> Vec<u64>   (trigram -> node_ids)
meta:        String -> Vec<u8>    (schema version, stats)
```

---

## API Surface (Rust)

```rust
pub struct GraphNoteDb { /* opaque */ }

impl GraphNoteDb {
    // Lifecycle
    pub fn open(path: &Path) -> Result<Self>;
    pub fn open_in_memory() -> Result<Self>;
    pub fn close(self) -> Result<()>;
    pub fn compact(&self) -> Result<()>;

    // Node CRUD
    pub fn create_node(&self, node: NewNode) -> Result<Node>;
    pub fn get_node(&self, id: u64) -> Result<Option<Node>>;
    pub fn get_node_by_uuid(&self, uuid: Uuid) -> Result<Option<Node>>;
    pub fn get_node_by_title(&self, title: &str) -> Result<Option<Node>>;
    pub fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node>;
    pub fn delete_node(&self, id: u64) -> Result<()>;

    // Edge CRUD
    pub fn create_edge(&self, edge: NewEdge) -> Result<Edge>;
    pub fn get_edge(&self, id: u64) -> Result<Option<Edge>>;
    pub fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge>;
    pub fn delete_edge(&self, id: u64) -> Result<()>;

    // Graph traversal
    pub fn neighbors(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Result<Vec<Node>>;
    pub fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>>;
    pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>>;
    pub fn subgraph(&self, root: u64, depth: u8) -> Result<SubGraph>;

    // Search
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ScoredNode>>;
    pub fn list_nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>>;
    pub fn list_recent(&self, limit: usize) -> Result<Vec<Node>>;

    // Batch / transactions
    pub fn transaction<F, T>(&self, f: F) -> Result<T>
    where F: FnOnce(&mut Txn) -> Result<T>;

    // Export / import
    pub fn export_json(&self, writer: &mut dyn Write) -> Result<()>;
    pub fn import_json(&self, reader: &mut dyn Read) -> Result<ImportStats>;
    pub fn export_graphml(&self, writer: &mut dyn Write) -> Result<()>;
}

pub enum Direction { Outgoing, Incoming, Both }

pub struct SubGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub struct ScoredNode {
    pub node: Node,
    pub score: f32,
}
```

---

## Full-Text Search

Trigram index (simple, no external deps, WASM-safe):

- On `create_node` / `update_node`: tokenize `title + body` into trigrams, store `trigram -> Vec<node_id>` in `fts_index`
- On `search_fts(query)`: extract query trigrams, intersect posting lists, rank by TF-IDF or hit count
- Normalize: lowercase, strip punctuation, CJK character support

Optional phase 2: integrate [tantivy](https://github.com/quickwit-oss/tantivy) for BM25 scoring (desktop only, not WASM).

---

## Serialization

**bincode v2** for all stored values — compact binary, fast encode/decode, deterministic, serde-compatible.

`properties: HashMap<String, Value>` — use `serde_json::Value` to allow arbitrary metadata without schema migration.

---

## Error Handling

```rust
#[derive(thiserror::Error, Debug)]
pub enum GraphNoteError {
    #[error("storage error: {0}")]
    Storage(#[from] redb::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] bincode::error::EncodeError),
    #[error("node not found: {0}")]
    NodeNotFound(u64),
    #[error("edge not found: {0}")]
    EdgeNotFound(u64),
    #[error("duplicate title: {0}")]
    DuplicateTitle(String),
    #[error("database locked")]
    Locked,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GraphNoteError>;
```

---

## Performance Targets

| Operation | Target |
|---|---|
| `create_node` | < 1ms |
| `get_node` by id | < 0.1ms |
| `search_fts` (10k nodes) | < 50ms |
| `subgraph` depth=2 (100 neighbors) | < 5ms |
| Cold open (50k nodes) | < 200ms |
| Memory footprint (idle) | < 10MB |

---

## Crate Structure

```
graphnote-db/
  Cargo.toml
  src/
    lib.rs
    db.rs           <- GraphNoteDb impl
    model.rs        <- Node, Edge, NewNode, NodePatch, etc.
    storage.rs      <- redb table definitions and low-level ops
    index/
      mod.rs
      title.rs
      kind.rs
      fts.rs        <- trigram index
    traversal.rs    <- BFS/DFS, shortest path, subgraph
    transaction.rs  <- Txn wrapper
    export.rs       <- JSON / GraphML
    error.rs        <- GraphNoteError enum
    uuid.rs         <- UUID v7 generation
  benches/
    basic_ops.rs
  tests/
    storage_tests.rs          <- StorageBackend trait contract tests
    crud.rs                   <- Node/Edge CRUD
    traversal.rs              <- BFS, DFS, shortest path
    fts.rs                    <- full-text search
    concurrent.rs             <- concurrent access
    scenarios/
      cbt_journal.rs          <- CBT thought chains, distortion patterns
      story_editor.rs         <- tree-structured narratives, scene subgraphs
      task_manager.rs         <- task dependencies, blocking chains
      erp.rs                  <- orders, inventory, transactional consistency
      bug_tracker.rs          <- bug impact analysis, release blocking
```

---

## Dependencies

```toml
[dependencies]
redb        = "2"
bincode     = "2"
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
uuid        = { version = "1", features = ["v7"] }
thiserror   = "2"

[dev-dependencies]
criterion   = "0.5"
tempfile    = "3"

[features]
default   = ["redb-storage"]
wasm      = ["getrandom/js"]   # UUID entropy for WASM

[target.'cfg(target_arch = "wasm32")'.dependencies]
getrandom = { version = "0.2", features = ["js"] }
```

---

## Phase Plan

Development is split into two milestones:

- **PoC** — a fully functional embedded graph DB that can power the notebook app on all platforms (desktop, mobile, WASM). After PoC, the notebook can be built and used.
- **MVP** — a distributable product: Docker image, HTTP API, Kubernetes support. After MVP, GraphNote DB can be deployed as a standalone service (like PostgreSQL or Redis).

```
PoC: Phases 1-6    →  notebook can be built on top of GraphNote DB
MVP: Phases 7-9    →  GraphNote DB ships as a Docker/K8s product
```

---

### PoC — Proof of Concept

> After PoC completion, the graph notebook app can use GraphNote DB on all target platforms.

#### Phase 1 — Storage Engine

> Goal: storage abstraction that allows swapping backends without touching upper layers.

- [x] `00001` Define `StorageBackend` trait (get, put, delete, scan_prefix, flush)
- [x] `00002` Define error types (`StorageError`) via `thiserror`
- [ ] `00003` Implement `MemoryBackend` backed by `BTreeMap<Vec<u8>, Vec<u8>>`
- [ ] `00004` Add persist/load to `MemoryBackend` — serialize entire BTreeMap to disk on flush
- [ ] `00005` Implement `RedbBackend` — wrapper over the `redb` crate
- [ ] `00006` Write integration tests: same test suite runs against both backends
- [ ] `00007` Benchmark: put/get/scan_prefix on 100K entries for both backends (criterion)

**Definition of done:** `cargo test` passes on both backends, benchmark is reproducible.

#### Phase 2 — Graph Store (CRUD + Indexes)

> Goal: store nodes and edges on top of the KV store, efficiently retrieve neighbors.

- [ ] `00008` Define types: Node, Edge, NewNode, NodePatch, UUID v7
- [ ] `00009` Implement `GraphNoteDb::open` / `open_in_memory` / `close`
- [ ] `00010` Implement Node CRUD: create_node, get_node, update_node, delete_node
- [ ] `00011` Implement Edge CRUD with adjacency list maintenance (out_edges, in_edges)
- [ ] `00012` Implement title_index and kind_index
- [ ] `00013` Write tests: CRUD, cascading edge deletion on node removal
- [ ] `00014` Benchmark: insert 100K nodes + 500K edges, read all neighbors

**Definition of done:** graph operations work, tests pass, indexes are consistent.

#### Phase 3 — Full-Text Search

> Goal: trigram-based FTS — WASM-safe, no external dependencies.

- [ ] `00015` Implement trigram tokenizer (lowercase, strip punctuation, CJK)
- [ ] `00016` Implement FTS index: trigram -> posting list storage
- [ ] `00017` Implement `search_fts` with TF-IDF ranking
- [ ] `00018` Implement `list_recent` and `list_nodes_by_kind`
- [ ] `00019` Tests: FTS recall, edge cases (empty query, single char, CJK)
- [ ] `00020` Benchmark: FTS on 10K nodes

**Definition of done:** FTS returns relevant results, recall is measured.

#### Phase 4 — Graph Traversal

> Goal: BFS, DFS, shortest path, subgraph extraction.

- [ ] `00021` Implement BFS with depth limit and optional edge kind filter
- [ ] `00022` Implement DFS with depth limit
- [ ] `00023` Implement shortest_path (Dijkstra, weighted by `edge.weight`)
- [ ] `00024` Implement `subgraph(root, depth)` — return all nodes and edges within radius
- [ ] `00025` Tests: cycles, disconnected graphs, empty graph, single node, depth 0
- [ ] `00026` Benchmark: BFS on a 100K-node graph with average degree 10, depth 3

**Definition of done:** traversals are correct on all edge cases, performance is measured.

#### Phase 5 — Platform Bindings

> Goal: GraphNote DB works on every target platform — desktop, mobile, browser.

- [ ] `00027` C FFI header (`graphnote.h`) — exposes GraphNoteDb API for iOS/Android native apps
- [ ] `00028` WASM bindings via `wasm-bindgen` — exposes GraphNoteDb API for browser and Tauri v2 WASM
- [ ] `00029` Verify redb works on WASM target; if not, implement fallback (IndexedDB adapter or memory-only)
- [ ] `00030` Cross-compilation CI: build for `aarch64-apple-ios`, `aarch64-linux-android`, `wasm32-unknown-unknown`
- [ ] `00031` Smoke test on each platform: open DB, CRUD a node, search, close

**Definition of done:** `graphnote.h` compiles on iOS/Android; WASM build loads in browser; all platforms pass smoke test.

#### Phase 6 — Scenario Integration Tests

> Goal: validate the DB against real-world use cases from the notebook app. After this phase, GraphNote DB is proven ready for the notebook.

- [ ] `00032` CBT journal scenario: thought chains, distortion pattern search, reframing edges
- [ ] `00033` Story editor scenario: tree structure (book→chapter→scene), character graph, subgraph for AI context
- [ ] `00034` Task manager scenario: dependency chains, blocking BFS, sprint board via kind_index
- [ ] `00035` ERP scenario: order→product→warehouse edges, transactional inventory updates
- [ ] `00036` Bug tracker scenario: impact analysis traversal, release-blocking queries

**Definition of done:** all 5 scenarios pass on both MemoryBackend and RedbBackend. The notebook app team can start building on top of GraphNote DB.

---

### MVP — Minimum Viable Product

> After MVP, GraphNote DB is distributed as a standalone service with Docker image and HTTP API.

#### Phase 7 — HTTP API (Server Mode)

> Goal: expose GraphNote DB over HTTP for programmatic access and container deployment.

- [ ] `00037` HTTP API server (axum + tokio) — thin JSON adapter over GraphNoteDb
- [ ] `00038` Node CRUD endpoints: `POST/GET/PATCH/DELETE /nodes/{id}`
- [ ] `00039` Edge endpoints: `POST/GET/DELETE /edges/...`
- [ ] `00040` Traversal endpoints: `GET /nodes/{id}/neighbors`, `/paths/shortest`, `/nodes/{id}/subgraph`
- [ ] `00041` Search endpoint: `POST /search/fts`
- [ ] `00042` Admin endpoints: `GET /health`, `GET /status`
- [ ] `00043` JSON error handling — unified error responses with status codes
- [ ] `00044` Integration tests: HTTP endpoints against in-memory backend

**Definition of done:** all endpoints respond correctly, tests pass.

#### Phase 8 — Docker & Kubernetes

> Goal: distribute GraphNote DB as an official container image, like PostgreSQL or Redis.

- [ ] `00045` Dockerfile — multi-stage build (rust:slim builder → debian:bookworm-slim runtime, ~80MB)
- [ ] `00046` `.dockerignore` — exclude target/, .git/
- [ ] `00047` `docker-compose.yml` — volume mount `/data`, port 8080, env vars
- [ ] `00048` Health check endpoint (`GET /health`) and graceful shutdown (SIGTERM)
- [ ] `00049` Kubernetes manifests: Deployment, Service, PersistentVolumeClaim
- [ ] `00050` Helm chart (optional) or Kustomize overlay
- [ ] `00051` CI: build + push Docker image to GitHub Container Registry (ghcr.io)
- [ ] `00052` Integration test: spin up container, run CRUD via HTTP, verify persistence across restart

**Definition of done:** `docker run ghcr.io/ice1x/graphnote-db` starts the DB with HTTP API on port 8080; K8s manifests deploy successfully.

#### Phase 9 — Hardening

- [ ] `00053` WAL / crash recovery
- [ ] `00054` Compaction
- [ ] `00055` JSON import/export (`export_json`, `import_json`)
- [ ] `00056` GraphML export (`export_graphml`)
- [ ] `00057` Property-based tests (proptest) for graph invariants
- [ ] `00058` Fuzz tests for FTS tokenizer
- [x] `00059` CI: GitHub Actions — test, clippy, fmt (benchmarks pending)
- [ ] `00060` Rustdoc for all public APIs

**Definition of done:** CI is green, crash recovery works, documentation is complete.

---

### Immediate subtasks

> Tasks that require code changes to align with this spec but are not yet reflected in the implementation.

- [ ] Rename crate from `grapevine` to `graphnote-db` (Cargo.toml, lib.rs)
- [ ] Rename `StorageError` to `GraphNoteError` or reconcile error hierarchy
- [ ] Add `serde`, `bincode`, `uuid`, `redb` to Cargo.toml dependencies
- [ ] Create `src/model.rs` with Node, Edge, NewNode, NodePatch structs per spec
- [ ] Create `src/db.rs` with `GraphNoteDb` struct skeleton

---

## Coding Conventions

### Language

- **All code comments, documentation, README, and commit messages — English only**

### Rust style

- Edition 2021, MSRV latest stable
- `cargo fmt` before every commit
- `cargo clippy -- -W clippy::all` with zero warnings
- No `unwrap()` / `expect()` in library code — `Result` only
- `unwrap()` allowed only in tests and benchmarks
- No `unsafe` without explicit justification

### Naming

- Modules: `snake_case` — Files: `snake_case.rs`
- Structs/traits: `PascalCase` — Functions: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE` — Type aliases: `PascalCase`

### Errors

Use `thiserror` for all error definitions. `Result<T>` type alias everywhere.

### Serialization

- Internal data (KV store): `bincode` — compact, fast
- Configs and dumps: `serde_json` — human-readable
- All persistable structs: `#[derive(Serialize, Deserialize)]`

### Testing

- Every public method — at least 1 test
- Edge cases mandatory: empty graph, single node, cycles
- Storage tests parameterized by backend (MemoryBackend, RedbBackend)
- Benchmarks with `criterion`
- **Scenario integration tests**: each use case (CBT, story editor, task manager, ERP, bug tracker) gets a dedicated test file that exercises the full API through realistic workflows
- Scenario tests validate that `kind`/`properties`/`edges` patterns work end-to-end for each domain

### Git

- Format: `type(scope): description [task_id]`
- Types: `feat`, `fix`, `test`, `bench`, `docs`, `refactor`, `chore`

---

## Agent Instructions

### Role

Senior Rust developer working on GraphNote DB. The project is educational, but the architecture must be production-grade.

### Session start

1. Read this README — it is the single source of truth
2. Check "Current Status" section below for the current task
3. Ask the user if they want to continue or switch tasks

### Working on a task

1. Before writing code — briefly describe the plan
2. Write code incrementally — one file/function at a time
3. After each logical block — run `cargo check` / `cargo test`
4. Do not write the entire project at once

### Code rules

- `Result<T, GraphNoteError>` on every public function
- Tests for every public method
- `#[derive(Debug, Clone, Serialize, Deserialize)]` where applicable
- Doc-comments on pub API
- No `unwrap()` in lib code, no `unsafe` without justification

---

## Current Status

**Phase:** 1 — Core (Storage Engine)

**Completed:**

- [x] `00001` StorageBackend trait
- [x] `00002` StorageError types
- [x] `00059` GitHub Actions CI — test, clippy, fmt

**Test status:**

```
cargo test: 14 passed, 0 failed
cargo clippy: 0 warnings
CI: GitHub Actions — check, test, clippy, fmt (all green)
```

**Next steps:**

1. Rename crate from `grapevine` to `graphnote-db`
2. `0003` — Implement `MemoryBackend` backed by `BTreeMap`
3. `0004` — Add persist/load to `MemoryBackend`
4. `0005` — Implement `RedbBackend`

---

## License

MIT
