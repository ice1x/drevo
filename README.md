# drevo — Embedded Graph Database for Knowledge Management

A lightweight, embeddable graph database written in Rust. Designed as the storage engine for cross-platform knowledge-base applications (similar to Obsidian), drevo runs natively on desktop (via FFI/Tauri), mobile (iOS/Android via C bindings), and in the browser (via WebAssembly). It also ships as a standalone HTTP server for containerised deployments.

---

## Use Cases

drevo is the storage engine for a cross-platform graph notebook. Target scenarios:

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

## Long-term Vision: Graph-Vector Database

Beyond the embedded knowledge-base use case described above, drevo is on a multi-phase trajectory toward becoming a full graph-vector database with capabilities equivalent to Neo4j and Memgraph:

- **Cypher query language** — full support for CREATE / MATCH / MERGE / SET / DELETE / WHERE / RETURN / WITH / aggregations / variable-length paths (Phase 10)
- **Bolt wire protocol** — Neo4j-compatible, so `cypher-shell`, `neo4j-python-driver`, and `neo4j-javascript-driver` connect out of the box (Phase 11)
- **Native vector search** — `Value::Vector` type, HNSW index, joint graph+vector queries for RAG and semantic search (Phase 12)
- **MVCC concurrency** — readers never block writers, multiple configurable isolation levels (Phase 13)
- **Cost-based query planner** — statistics, cardinality estimates, plan caching, supernode handling (Phase 14)
- **Production ecosystem** — MCP server, web UI, Python SDK, replication, streaming ingestion, CDC, RBAC (Phase 15)

Phases 1-9 (embedded DB + HTTP API + Docker) form the foundation. Phases 10-15 layer the query language, protocol, vector engine, concurrency, optimizer, and ecosystem on top of the existing storage and traversal engine — without rewriting it.

---

## Inspirations

drevo borrows architectural ideas from two existing graph databases. Their licenses prevent direct reuse, but we adopt their proven designs:

| Database | What we borrow | Why we cannot use it directly |
|----------|---------------|-------------------------------|
| **[HelixDB](https://github.com/HelixDB/helix-db)** | graph+vector native engine, compiled query plans, MCP tooling, memory-mapped storage, built-in embeddings | BSL (Business Source License) — incompatible with MIT |
| **[Memgraph](https://github.com/memgraph/memgraph)** | full Cypher support, Bolt protocol, MVCC, in-memory + WAL/snapshots, MAGE plugin system, Python query modules, streaming ingestion | AGPL / proprietary enterprise — incompatible with MIT |

drevo ships under MIT (see License section).

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
  drevo.db          <- single binary file (redb)
  drevo.db.lock     <- advisory lock
  drevo.db.wal      <- write-ahead log (optional, for crash recovery)
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
pub struct Drevo { /* opaque */ }

impl Drevo {
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
pub enum DrevoError {
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

pub type Result<T> = std::result::Result<T, DrevoError>;
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

### Performance Comparison vs Other Graph DBs

Target numbers vs published competitor benchmarks. CI tracks measured drevo metrics continuously; any regression > 5% fails the build.

| Metric | drevo (target) | HelixDB | Memgraph | Neo4j | FalkorDB |
|--------|----------------|---------|----------|-------|----------|
| Single-hop traversal | < 0.1 ms | ~0.1 ms | ~0.1 ms | ~1 ms | ~0.2 ms |
| 3-hop neighborhood (1K nodes) | < 1 ms | ~1 ms | ~1 ms | ~5 ms | ~2 ms |
| Deep traversal (6 hops) | < 10 ms | ~5 ms | ~8 ms | ~50 ms | ~15 ms |
| Node create (single) | < 0.05 ms | ~0.05 ms | ~0.1 ms | ~1 ms | ~0.1 ms |
| Bulk insert (100K nodes) | < 2 s | ~2 s | ~3 s | ~15 s | ~5 s |
| Vector similarity search (1M vectors) | < 5 ms | ~2 ms | N/A | N/A | N/A |
| Concurrent reads (100 threads) | > 500K ops/s | ~300K ops/s | ~400K ops/s | ~50K ops/s | ~200K ops/s |
| Memory per 1M nodes | < 500 MB | ~400 MB | ~600 MB | ~2 GB | ~800 MB |

> Competitor numbers are approximate, derived from published benchmarks and vendor claims. Replace with measured values as Phase 15 task `00101` (benchmark vs competitors) lands.

---

## Crate Structure

```
drevo/
  Cargo.toml
  src/
    lib.rs
    db.rs           <- Drevo impl
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
    error.rs        <- DrevoError enum
    uuid.rs         <- UUID v7 generation
  benches/
    storage_bench.rs      <- put/get/scan_prefix benchmarks (criterion)
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
- **MVP** — a distributable product: Docker image, HTTP API, Kubernetes support. After MVP, drevo can be deployed as a standalone service (like PostgreSQL or Redis).

```
PoC: Phases 1-6    →  notebook can be built on top of drevo
MVP: Phases 7-9    →  drevo ships as a Docker/K8s product
```

---

### PoC — Proof of Concept

> After PoC completion, the graph notebook app can use drevo on all target platforms.

#### Phase 1 — Storage Engine

> Goal: storage abstraction that allows swapping backends without touching upper layers.

- [x] `00001` Define `StorageBackend` trait (get, put, delete, scan_prefix, flush)
- [x] `00002` Define error types (`StorageError`) via `thiserror`
- [x] `00003` Implement `MemoryBackend` backed by `BTreeMap<Vec<u8>, Vec<u8>>`
- [x] `00004` Add persist/load to `MemoryBackend` — serialize entire BTreeMap to disk on flush
- [x] `00005` Implement `RedbBackend` — wrapper over the `redb` crate
- [x] `00006` Write integration tests: same test suite runs against both backends
- [x] `00007` Benchmark: put/get/scan_prefix on 100K entries for both backends (criterion)

**Definition of done:** `cargo test` passes on both backends, benchmark is reproducible.

#### Phase 2 — Graph Store (CRUD + Indexes)

> Goal: store nodes and edges on top of the KV store, efficiently retrieve neighbors.

- [x] `00008` Define types: Node, Edge, NewNode, NodePatch, UUID v7
- [x] `00009` Implement `Drevo::open` / `open_in_memory` / `close`
- [x] `00010` Implement Node CRUD: create_node, get_node, update_node, delete_node
- [x] `00011` Implement Edge CRUD with adjacency list maintenance (out_edges, in_edges)
- [x] `00012` Implement title_index and kind_index
- [x] `00013` Write tests: CRUD, cascading edge deletion on node removal
- [x] `00014` Benchmark: insert 100K nodes + 500K edges, read all neighbors

**Definition of done:** graph operations work, tests pass, indexes are consistent.

#### Phase 3 — Full-Text Search

> Goal: trigram-based FTS — WASM-safe, no external dependencies.

- [x] `00015` Implement trigram tokenizer (lowercase, strip punctuation, CJK)
- [x] `00016` Implement FTS index: trigram -> posting list storage
- [x] `00017` Implement `search_fts` with TF-IDF ranking
- [x] `00018` Implement `list_recent` and `list_nodes_by_kind`
- [x] `00019` Tests: FTS recall, edge cases (empty query, single char, CJK)
- [x] `00020` Benchmark: FTS on 10K nodes

**Definition of done:** FTS returns relevant results, recall is measured.

#### Phase 4 — Graph Traversal

> Goal: BFS, DFS, shortest path, subgraph extraction.

- [x] `00021` Implement BFS with depth limit and optional edge kind filter
- [x] `00022` Implement DFS with depth limit
- [x] `00023` Implement shortest_path (Dijkstra, weighted by `edge.weight`)
- [x] `00024` Implement `subgraph(root, depth)` — return all nodes and edges within radius
- [x] `00025` Tests: cycles, disconnected graphs, empty graph, single node, depth 0
- [x] `00026` Benchmark: BFS on a 100K-node graph with average degree 10, depth 3

**Definition of done:** traversals are correct on all edge cases, performance is measured.

#### Phase 5 — Platform Bindings

> Goal: drevo works on every target platform — desktop, mobile, browser.

- [x] `00027` C FFI header (`drevo.h`) — exposes Drevo API for iOS/Android native apps
- [x] `00028` WASM bindings via `wasm-bindgen` — exposes Drevo API for browser and Tauri v2 WASM
- [x] `00029` Verify redb works on WASM target; if not, implement fallback (IndexedDB adapter or memory-only)
- [x] `00030` Cross-compilation CI: build for `aarch64-apple-ios`, `aarch64-linux-android`, `wasm32-unknown-unknown`
- [x] `00031` Smoke test on each platform: open DB, CRUD a node, search, close

**Definition of done:** `drevo.h` compiles on iOS/Android; WASM build loads in browser; all platforms pass smoke test. **DONE** — all 5 tasks complete.

#### Phase 6 — Scenario Integration Tests

> Goal: validate the DB against real-world use cases from the notebook app. After this phase, drevo is proven ready for the notebook.

- [x] `00032` CBT journal scenario: thought chains, distortion pattern search, reframing edges
- [x] `00033` Story editor scenario: tree structure (book→chapter→scene), character graph, subgraph for AI context
- [x] `00034` Task manager scenario: dependency chains, blocking BFS, sprint board via kind_index
- [x] `00035` ERP scenario: order→product→warehouse edges, transactional inventory updates
- [x] `00036` Bug tracker scenario: impact analysis traversal, release-blocking queries

**Definition of done:** all 5 scenarios pass on both MemoryBackend and RedbBackend. The notebook app team can start building on top of drevo.

---

### MVP — Minimum Viable Product

> After MVP, drevo is distributed as a standalone service with Docker image and HTTP API.

#### Phase 7 — HTTP API (Server Mode)

> Goal: expose drevo over HTTP for programmatic access and container deployment.

- [x] `00037` HTTP API server (axum + tokio) — thin JSON adapter over Drevo
- [x] `00038` Node CRUD endpoints: `POST/GET/PATCH/DELETE /nodes/{id}`
- [x] `00039` Edge endpoints: `POST/GET/DELETE /edges/...`
- [x] `00040` Traversal endpoints: `GET /nodes/{id}/neighbors`, `/paths/shortest`, `/nodes/{id}/subgraph`
- [x] `00041` Search endpoint: `POST /search/fts`
- [x] `00042` Admin endpoints: `GET /health`, `GET /status`
- [x] `00043` JSON error handling — unified error responses with status codes
- [x] `00044` Integration tests: HTTP endpoints against in-memory backend

**Definition of done:** all endpoints respond correctly, tests pass.

#### Phase 8 — Docker & Kubernetes

> Goal: distribute drevo as an official container image, like PostgreSQL or Redis.

- [x] `00045` Dockerfile — multi-stage build (rust:slim builder → debian:bookworm-slim runtime, ~80MB)
- [x] `00046` `.dockerignore` — exclude target/, .git/
- [x] `00047` `docker-compose.yml` — volume mount `/data`, port 8080, env vars
- [x] `00048` Health check endpoint (`GET /health`) and graceful shutdown (SIGTERM) — `/health` (liveness, cheap, no DB) and `/ready` (readiness, probes redb) flip to 503 once SIGTERM/Ctrl+C drains the server, so Kubernetes Endpoints controllers withdraw traffic before SIGKILL.
- [x] `00049` Kubernetes manifests: Deployment, Service, PersistentVolumeClaim — `k8s/` ships plain-`kubectl`-applyable manifests plus a `kustomization.yaml` so `kubectl apply -k k8s/` brings the database up in one shot. Topology: single-replica `apps/v1` Deployment with `strategy.type: Recreate` (RWO PVC + single-writer redb means a rolling update would deadlock on the volume), `runAsNonRoot: true` + UID 999 mirrors the Dockerfile's `drevo` system user, `livenessProbe` → `GET /health` (cheap, no DB touch — task 00042), `readinessProbe` → `GET /ready` (probes redb — task 00048; flips to 503 during SIGTERM drain so Endpoints withdraws traffic before SIGKILL), `terminationGracePeriodSeconds: 30` keeps the graceful-shutdown budget explicit. ClusterIP `Service` on port 8080 (no Ingress baked in — left to the operator's controller of choice). 1 Gi RWO `PersistentVolumeClaim` named `drevo-data` bound at `/data`, matching `DREVO_DATA_DIR`. `tests/k8s_manifests_tests.rs` ships 26 text-level integration tests that lock every contract above (file layout, `apps/v1` / `v1` API versions, image pinned to `ghcr.io/ice1x/drevo:<tag>` with no bare `:latest`, port 8080, env vars, `claimName: drevo-data`, probe paths, `replicas: 1`, `strategy.type: Recreate`, `runAsNonRoot: true`, `ReadWriteOnce`, `kustomization.yaml` listing all three resources) — no `kubectl` or cluster required, mirroring the `tests/docker_compose_tests.rs` pattern from task 00047. CI gains a dedicated `K8s manifests` job that runs `kubectl kustomize k8s/` server-less so a syntactic regression in any of the four files (or a stray kustomize deprecation warning) fails the build. Baseline 1322 → 1348 stable tests (`cargo_doc_emits_no_warnings` from 00060 stays ignored by default).
- [x] `00050` Helm chart (optional) or Kustomize overlay — `k8s/` restructures the base manifests under `k8s/base/` (Deployment, Service, PVC, base kustomization) and adds per-env overlays under `k8s/overlays/{dev,prod}/`. Each overlay reuses the base via `../../base/` (a sibling subtree — sidesteps kustomize's load-restrictor security check that rejects upward directory references), pins a distinct `namespace:` (`drevo-dev` / `drevo-prod`), rewrites the image tag via `images.newTag` (`v0.1.0-dev` / `v0.1.0`), and ships strategic-merge patches that only touch container resources and PVC storage size: dev gets cpu 25m→250m, memory 32Mi→256Mi, 1Gi PVC, plus `RUST_LOG=debug`; prod gets cpu 200m→2000m, memory 256Mi→2Gi, 20Gi PVC, plus `imagePullPolicy: Always` so a `kubectl rollout restart` re-pulls the pinned tag. The single-writer redb invariants (`replicas: 1`, `strategy.type: Recreate`, `ReadWriteOnce`, `runAsNonRoot: true`) stay in the base and are explicitly forbidden from being relaxed by an overlay (`tests/k8s_overlays_tests.rs`). A thin top-level `k8s/kustomization.yaml` forwards to `./base/` so the historical `kubectl apply -k k8s/` still works. `tests/k8s_overlays_tests.rs` ships 19 text-level integration tests (overlay layout, kustomize v1beta1 api, base reference, distinct namespaces, image-tag override, patch list, no-replica-bump / no-relax-root / no-RWX guards, prod-≥-dev cpu/memory/storage ordering, README documents the overlays); `tests/k8s_manifests_tests.rs` gains 2 layout tests for the base/ move + top-level wrapper. CI's `K8s manifests` job now loops over `{k8s/, k8s/base/, k8s/overlays/dev/, k8s/overlays/prod/}` and fails on any kustomize warning/error. Baseline 1349 → 1370 stable tests (`cargo_doc_emits_no_warnings` from 00060 stays ignored by default).
- [x] `00051` CI: build + push Docker image to GitHub Container Registry (ghcr.io) — `.github/workflows/docker-publish.yml` builds the production image from the root `Dockerfile` (task 00045) and publishes it to `ghcr.io/ice1x/drevo` (same namespace the K8s Deployment from task 00049 pulls from). Triggers: push to `main` (publishes `latest` + `main` + `sha-<short>`), push of a `v*` semver tag (publishes `MAJOR.MINOR.PATCH` + `MAJOR.MINOR` + `sha-<short>` — unblocks the future release flow without a workflow edit), pull_request to `main` (builds the image to validate the Dockerfile but does NOT push — fork PRs cannot publish under the project namespace), and `workflow_dispatch` (manual re-publish from the Actions UI). Multi-arch matrix: `linux/amd64` (canonical x86_64 server target) and `linux/arm64` (the project's developer baseline is Apple Silicon per README "Performance Targets", and arm64 K8s nodes — Graviton, Ampere — are increasingly common in production) — QEMU emulates arm64 on the amd64 runner; buildx orchestrates the cross-arch layer cache. Token surface is minimal: `contents: read` + `packages: write` (no `actions: write`, no `id-token: write` — re-add only when cosign / SLSA lands). Auth uses `${{ github.actor }}` + `${{ secrets.GITHUB_TOKEN }}` against `ghcr.io` via `docker/login-action@v3` (no PAT, no deploy key). Tag derivation is fully delegated to `docker/metadata-action@v5` so hand-rolled shell tag logic cannot drift from semver: `type=sha`, `type=ref,event=branch`, `type=semver,pattern={{version}}`, `type=semver,pattern={{major}}.{{minor}}`, `type=raw,value=latest,enable={{is_default_branch}}` (the `latest` tag therefore only points at a `main` build — never at a feature branch or a tag rebuild). The GHA cache backend (`type=gha,scope=docker-publish`) keeps amd64 + arm64 layers warm between runs without paying GHCR egress. `tests/docker_publish_ci_tests.rs` ships 29 text-level integration tests that pin every wire-format invariant above (layout, triggers, permissions, registry/image, login-action / buildx / qemu / build-push-action / metadata-action usage, multi-arch platforms, sha + semver + gated-`latest` tag rules, README documentation, and a non-regression guard that the main `ci.yml` does NOT learn publish duties — keeping concerns split so a flaky test job can never push a half-broken image).
- [x] `00052` Integration test: spin up container, run CRUD via HTTP, verify persistence across restart — `tests/container_integration_tests.rs` builds the production Docker image (the same one task 00045 ships through `Dockerfile` and task 00051 publishes to `ghcr.io/ice1x/drevo`), starts a container against a Docker **named volume** mounted at `/data` (matching `VOLUME ["/data"]` + `DREVO_DATA_DIR=/data`), waits for `GET /ready` to flip green (task 00048's readiness probe), runs a full CRUD cycle over HTTP/1.1 (`POST /nodes` → `GET /nodes/{id}` → `PATCH /nodes/{id}` → `GET /nodes` listing → `DELETE /nodes/{id}` → `GET /nodes/{id}` returns 404), and finally tears the container down with `docker rm -f` and starts a **fresh** container against the **same** named volume to prove the redb file at `/data/drevo.redb` survives — the production replay of K8s `strategy.type: Recreate` from task 00049's `k8s/base/deployment.yaml`. The three live tests (`live_container_serves_health_and_status_endpoints`, `live_container_supports_node_crud_via_http`, `live_container_persists_node_across_restart`) are `#[ignore]`-by-default so the regular CI matrix (`ci.yml`) does not need a Docker daemon. The same file ships nine **structural** always-on guards that lock the contract surface without Docker: every `live_container_*` carries both `#[test]` and `#[ignore]`, the harness publishes the container's port 8080 (matching `EXPOSE 8080` + `k8s/base/service.yaml`), the volume binds to `:/data` (not a host bind-mount — the container's UID 999 non-root user could not write a host-owned path), `docker volume create` is the chosen volume strategy, all four CRUD verbs are exercised (`http_post`/`http_get`/`http_patch`/`http_delete`), the persistence test starts two distinct `LiveContainer::start` sites, the README marks 00052 done, the README documents the opt-in `cargo test --test container_integration_tests -- --include-ignored` command and the named-volume + restart semantics, and `ci.yml` is asserted to **not** carry `--include-ignored` (live container tests must stay opt-in to keep CI green without a Docker daemon). The HTTP client is hand-rolled over `std::net::TcpStream` (Connection: close, Content-Length-delimited bodies) to keep dev-deps lean — no `reqwest`, no `ureq`. Each live test picks a free host port via `TcpListener::bind("127.0.0.1:0")` and uses a unique named volume + container name so the suite is parallel-safe; RAII `Drop` impls on `LiveContainer` and `VolumeGuard` clean up containers and volumes even if a test panics. Run locally with: `cargo test --test container_integration_tests -- --include-ignored`. Baseline 1402 → 1413 stable tests (11 new structural).

**Definition of done:** `docker run ghcr.io/ice1x/drevo` starts the DB with HTTP API on port 8080; K8s manifests deploy successfully.

#### Phase 9 — Hardening

- [x] `00053` WAL / crash recovery — `src/db.rs` adds a documented crash-recovery model on top of redb's per-commit double-write+fsync (which is the WAL — each `RedbBackend::put`/`delete` is its own committed redb transaction with double-write+fsync inside the on-disk format). The pre-`00053` defect was an **id-counter durability gap**: `Drevo::create_node` / `create_edge` bumped an in-memory atomic counter on every allocation but only persisted `meta:next_node_id` / `meta:next_edge_id` via `Drevo::close()`. A process killed between two `create_node` calls therefore rewound the persisted counter on next open, and the very next `create_node` reused an already-stored id — `node:<id>` got silently overwritten and the title / UUID / kind indexes drifted out of sync. **Fix:** `Drevo::load_counters` now performs a max-id rescan at every `Drevo::open` — reads the persisted hint, scans `max(stored_id)` from the `node:` and `edge:` prefixes, and takes `max(persisted, scanned + 1)` as the effective counter. The persisted value becomes a *hint*, the on-disk rows are the source of truth; the rescan is O(N) over data-row keys (the per-key length guard filters `node_uuid:` / `node_kind:` / etc. so the cost is data-only, not all-keys). A new `counter_drift_repaired: AtomicBool` on `Drevo` is set when the rescan had to clamp the counter upward — operators see this through `IntegrityReport::counter_drift_repaired`. **API surface:** `pub struct IntegrityReport` (serde-serialisable so the report rides over HTTP / FFI / WASM boundaries) carries `node_count`, `edge_count`, `max_node_id`, `max_edge_id`, `next_node_id`, `next_edge_id`, `counter_drift_repaired`, `orphan_node_kind_entries`, `orphan_node_title_entries`, `orphan_node_uuid_entries`, `orphan_edge_kind_entries`, `orphan_edge_uuid_entries`, `orphan_adjacency_entries`, `dangling_edge_endpoints`, `corrupt_node_rows`, `corrupt_edge_rows`, plus an `is_clean()` predicate. `pub fn Drevo::check_integrity() -> Result<IntegrityReport>` scans every secondary index family (`node_kind:`, `node_title:`, `node_uuid:`, `edge_kind:`, `edge_uuid:`, `out:`, `in:`) and counts orphan / dangling references without short-circuiting on a single bad row — corrupt `node:` / `edge:` payloads are tallied via `corrupt_*_rows`. `pub fn Drevo::recover(path: &Path) -> Result<(Drevo, IntegrityReport)>` opens the database and runs `check_integrity` in one shot — the documented entry point for "I think we crashed, what did we lose". `tests/wal_crash_recovery_tests.rs` ships 23 integration tests: 5 crash-simulation tests (drop without `close()` is byte-equivalent to a process kill for redb — every redb txn was already durably committed), 4 counter-recovery tests covering the rescan path against synthetic persisted-counter rewinds via raw `RedbBackend::put`, 11 integrity-scan tests (empty / healthy / orphan / dangling / counter-drift), 2 `recover()` entry-point tests, plus 3 cross-file structural guards (README marks 00053 done + describes the model, `src/db.rs` module docs describe the WAL crash-recovery section, `IntegrityReport` stays `pub`).
- [x] `00054` Compaction — `src/db.rs` adds `pub struct CompactReport` (serde-serialisable so the report rides over HTTP / FFI / WASM boundaries) carrying `bytes_before` / `bytes_after` / `bytes_reclaimed` / `next_node_id` / `next_edge_id`, and replaces the old `Drevo::compact(&self) -> Result<()>` no-op with `pub fn Drevo::compact(&mut self) -> Result<CompactReport>` that **(1)** persists the in-memory next-id counters to `meta:next_node_id` / `meta:next_edge_id` *before* the reclaim so a process kill at any point during compaction leaves the file with a counter that reflects the pre-compaction id allocator state, **(2)** samples the backend file size before and after the reclaim, and **(3)** delegates the physical reclaim to the new `StorageBackend::compact(&mut self)` trait method. **Storage trait surface (`src/storage/backend.rs`):** `fn size_bytes(&self) -> Result<Option<u64>>` reports the on-disk footprint (`None` for ephemeral backends) and `fn compact(&mut self) -> Result<()>` reclaims unused storage — both have default implementations (`Ok(None)` and `self.flush()` respectively) so existing backends remain correct without changes. **`RedbBackend` (`src/storage/redb.rs`):** the wrapper now retains the database file path so `size_bytes` can `stat(2)` the file directly; `compact` uses `Arc::get_mut(&mut self.db)` to obtain the `&mut Database` that `redb::Database::compact` requires and surfaces `StorageError::CompactNotExclusive` (new variant) when the inner `Arc<Database>` has outstanding clones — the operator must drop the extra reference and retry. The `redb::CompactionError` returned by the upstream compactor (for outstanding savepoints, live read txns, or per-page I/O failures) funnels into `StorageError::Redb` via the existing `From<redb::Error>` chain. **`MemoryBackend` (`src/storage/memory.rs`):** `size_bytes` returns the snapshot file length for the persistent path (`None` for ephemeral and for persistent paths whose snapshot has not yet been written), and `compact` rewrites the snapshot — the BTreeMap serialisation naturally drops bytes for keys deleted since the last flush. `tests/compaction_tests.rs` ships 24 integration tests grouped into: 3 `CompactReport` API tests (serde round-trip, `Default`, `Debug` panic-safety), 4 ephemeral in-memory tests (`bytes_before == None`, counter persistence into `meta`, data preservation, idempotency), 3 persistent in-memory tests (snapshot file shrinks after delete + compact, `size_bytes` reports a positive number, ephemeral reports `None`), 7 redb tests (size reporting, reclaim after 500-node churn, node + edge preservation across compact, secondary-index preservation, counter checkpoint survives drop-without-close, idempotency, empty-DB success), 2 backend-level direct API tests (`size_bytes` matches `fs::metadata`, `compact` runs without error), 1 `Arc<Database>` exclusivity test (`StorageError::CompactNotExclusive` when a clone is held), 1 trait-object dispatch test, and 4 cross-file structural guards (README marks 00054 done, `db.rs` declares `pub struct CompactReport`, `Drevo::compact` takes `&mut self` and returns `Result<CompactReport>`, `StorageBackend` trait exposes both `size_bytes` and `compact`).
- [x] `00055` JSON import/export (`export_json`, `import_json`) — `src/dump.rs` adds the schema-versioned `drevo-json-v1` wire format with `Drevo::{export_json, export_json_to_path, import_json, import_json_from_path}` plus `HTTP GET /export/json` and `POST /import/json`. Import is idempotent (byte-identical rows skipped, counted in `ImportReport::*_skipped`), id-collision-safe (typed `DumpError::IdCollision` → `DrevoError::Io`), title-collision-safe (`DrevoError::DuplicateTitle`), and clamps the receiver's id counters above the imported range to prevent reuse anomalies after backup-restore. `tests/json_import_export_tests.rs` ships 25 integration tests (empty + single-node + full graph round-trips, cross-backend Memory↔Redb parity, index rebuild verification, malformed-JSON / unknown-format / id-collision / title-collision rejection); `tests/http_api_tests.rs` adds 4 endpoint tests.
- [x] `00056` GraphML export (`export_graphml`) — `src/dump.rs` adds read-only GraphML 1.0 export with `Drevo::{export_graphml, export_graphml_to_path}` plus `HTTP GET /export/graphml` (`application/xml`). The emitted document declares the GraphML XSD namespace, opens `<graph id="drevo" edgedefault="directed">` to match drevo's directional edge model, encodes node / edge ids as `n<id>` / `e<id>` (valid `xs:NMTOKEN`), serialises `Properties` as a single JSON-string `<data>` value so the format is lossless and re-parsable by yEd / Gephi / NetworkX / Cytoscape / igraph, and XML-escapes every text payload (`<`, `>`, `&`, `"`, `'` → entity refs; control characters → `U+FFFD`). Weights are emitted as GraphML `double`s with NaN / ±∞ surfaced as `"NaN"` / `"Infinity"` / `"-Infinity"` for downstream inspection. Output is deterministic — `collect_all_nodes` / `collect_all_edges` already sort by id, `Properties::Serialize` already sorts keys via BTreeMap. `tests/graphml_export_tests.rs` ships 17 integration tests (well-formedness, completeness, XML escaping, JSON-property round-trip, edge source/target wiring, byte-identical determinism, cross-backend Memory↔Redb parity, filesystem export, Unicode/CJK/emoji/Cyrillic survival, id-order emission); `tests/http_api_tests.rs` adds 3 endpoint tests (200 + `application/xml`, 405 on non-GET, empty-db well-formedness). Baseline 1282 → 1312 tests.
- [x] `00057` Property-based tests (proptest) for graph invariants — `proptest` dev-dep added, three test files (`tests/proptest_graph_invariants.rs`, `tests/proptest_fts_tokenizer.rs`, `tests/proptest_model_serialization.rs`) cover (a) the four storage invariants from `.claude/skills/drevo-database/SKILL.md` under arbitrary CRUD sequences, (b) tokenizer round-trip properties (idempotent `normalize`, sorted-and-deduped `trigrams`, 2-or-3-char window bound), and (c) bincode + JSON round-trip for `Node` / `Edge` plus order-independent bincode for `Properties`.
- [x] `00058` Fuzz tests for FTS tokenizer — `fuzz/` sub-crate (separate workspace, nightly-only build) ships three `cargo-fuzz` + `libFuzzer` targets (`fuzz_normalize`, `fuzz_trigrams`, `fuzz_extract_trigrams`) covering the public tokenizer surface. Each target carries a UTF-8 seed corpus under `fuzz/seed_corpus/<target>/` (ASCII, CJK, Cyrillic, Latin-1, Turkish dotless I, emoji, combining diacritics, digits, control chars, punctuation runs, single chars). The same assertion bodies are mirrored in `tests/fts_tokenizer_fuzz_harness_tests.rs` so stable `cargo test` replays the seed corpus on every PR (no nightly required); the harness also locks the `fuzz/Cargo.toml` `[[bin]]` declarations and per-target corpus presence against accidental deletion. Stable harness: 6 tests (3 invariant replays + char-boundary helper + 2 wiring guards). CI gains a non-blocking `fuzz` job that runs each target for 60 s under `cargo +nightly fuzz run -- -max_total_time=60` and uploads `fuzz/artifacts/` on a crash. Division of labour with `00057` (proptest, stable) and `00108` (audit-grade xorshift32) documented in `fuzz/README.md` and `audit/AUDIT-fts.md` §"#follow-up-tokenizer-fuzz".
- [x] `00059` CI: GitHub Actions — test, clippy, fmt (benchmarks pending)
- [x] `00060` Rustdoc for all public APIs — `cargo doc --no-deps --all-features` now passes under `RUSTDOCFLAGS="-D warnings"` on both native and `wasm32-unknown-unknown`. Every `pub` item is documented (the `#![warn(missing_docs)]` gate at `src/lib.rs` was already active; this task is the second half — making every intra-doc link resolve so `-D warnings` is actually enforceable). Module-level `//!` docs across `src/api.rs`, `src/dump.rs`, `src/ffi.rs`, `src/wasm.rs`, `src/traversal.rs`, `src/server.rs`, `src/model.rs`, `src/storage/mod.rs`, `src/storage/error.rs`, `src/db.rs`, `src/fts/mod.rs`, and `src/lib.rs` were repaired: bare `[`Drevo`]` rewritten as `[`crate::db::Drevo`]`; cfg-gated paths (`Drevo::open`, `Drevo::*_to_path`, `RedbBackend`, `redb::Error`, `crate::api`/`ffi`/`server`) demoted to plain code spans so the wasm target no longer breaks; redundant explicit link target `[`Properties`](crate::model::Properties)` collapsed to `[`crate::model::Properties`]`; markdown URL link to `audit/AUDIT-fts.md` repaired. The six `pub const` API limits (`DEFAULT_LIST_LIMIT`, `MAX_LIST_LIMIT`, `DEFAULT_NEIGHBORS_DEPTH`, `DEFAULT_SUBGRAPH_DEPTH`, `DEFAULT_SEARCH_LIMIT`, `MAX_SEARCH_LIMIT`) graduated from `const` to `pub const` so the doc links to them stop firing `rustdoc::private_intra_doc_links`. New `tests/rustdoc_public_apis_tests.rs` ships 6 tests: an ignored-by-default behavioural gate that spawns `cargo doc -D warnings` (run on every CI doc job and via `cargo test -- --include-ignored`) plus 5 source-level guards that catch the most common regression families without spawning `cargo doc` (Makefile `doc` target uses `RUSTDOCFLAGS=-D warnings`; CI workflow has a `Doc` job exporting `RUSTDOCFLAGS=-D warnings`; `dump.rs` carries no redundant explicit link targets; module-level docs qualify top-level symbols with `crate::`; `pub const` constants referenced from public docs stay public). CI gains a dedicated `Doc` job that runs `cargo doc --no-deps --all-features` under `RUSTDOCFLAGS=-D warnings` so a regression in any intra-doc link or a missing public-item doc fails the build. Baseline 1318 → 1322 tests (5 new stable; `cargo_doc_emits_no_warnings` ignored by default). `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` both clean.

**Definition of done:** CI is green, crash recovery works, documentation is complete.

---

### Phase 10 — Cypher Query Language

> Goal: full Cypher subset equivalent to the Neo4j core. Existing graph storage and traversal are reused; Cypher is a thin query layer over the current `Drevo` API.

Critical path: lexer → parser → executor (CREATE/MATCH/RETURN) → mutations (SET/DELETE/MERGE) → predicates (WHERE) → aggregations → OPTIONAL MATCH → WITH → variable-length paths.

- [ ] `00061` Cypher lexer — tokens for keywords, literals, identifiers, operators, parameters, comments
- [ ] `00062` Cypher parser — AST construction, error recovery, keyword-as-identifier support
- [ ] `00063` Executor — pattern matching, expression evaluation, CREATE / MATCH / RETURN
- [ ] `00064` Mutations — SET, DELETE, MERGE, MATCH...MERGE (idempotent relationship creation between bound variables)
- [ ] `00065` WHERE — boolean expressions, comparison operators, IN, EXISTS, IS NULL
- [ ] `00066` Aggregations — COUNT, SUM, AVG, MIN, MAX, COLLECT, GROUP BY, DISTINCT
- [ ] `00067` OPTIONAL MATCH — left-join semantics, null propagation for unmatched variables
- [ ] `00068` WITH clause — query pipelining, intermediate projection, aggregation-before-filter
- [ ] `00069` Variable-length paths — `(a)-[*1..3]->(b)` BFS traversals leveraging existing `traversal.rs`

**Definition of done:** all five scenario test suites (CBT, story, task manager, ERP, bug tracker) pass when expressed in Cypher — same workflows yield identical results via Cypher and the existing Rust API.

---

### Phase 11 — Bolt Protocol

> Goal: Neo4j-compatible wire protocol so existing official drivers (`cypher-shell`, `neo4j-python-driver`, `neo4j-javascript-driver`) connect without code changes. Requires Phase 10 — Bolt has nothing to execute without a working Cypher engine.

- [ ] `00070` Bolt v4 wire protocol — PackStream codec, TCP listener, message framing
- [ ] `00071` Bolt session — HELLO, RUN, PULL, DISCARD, RESET, GOODBYE handshake
- [ ] `00072` Bolt transaction integration — BEGIN / COMMIT / ROLLBACK over Bolt
- [ ] `00073` TLS for Bolt — `rustls` integration, certificate management
- [ ] `00074` Authentication — basic auth, session tokens, user table

**Definition of done:** `cypher-shell bolt://localhost:7687` connects, authenticates, runs queries; transactions complete end-to-end.

---

### Phase 12 — Vector Storage & Search

> Goal: native graph + vector hybrid queries (traverse AND similarity in one query) — enables RAG and semantic search over the knowledge graph. Independent of Phase 11; can run in parallel with Bolt work.

- [ ] `00075` `Value::Vector` variant — cosine, euclidean, dot product distance functions (SIMD-accelerated)
- [ ] `00076` HNSW vector index — built on top of redb adjacency storage
- [ ] `00077` Joint graph+vector queries — `MATCH (n) WHERE similar(n.embedding, $q, 0.85) RETURN n` (Cypher extension)
- [ ] `00078` Vector persistence — redb tables, batch insert API
- [ ] `00079` Embedding integration helpers — callable from Python SDK / FastMCP

**Definition of done:** 1M-vector search < 5 ms on Apple Silicon; recall@10 ≥ 0.95 vs brute-force ground truth.

---

### Phase 13 — Concurrency & MVCC

> Goal: production-grade concurrent reads and writes. Today drevo uses single-writer locking; this phase introduces MVCC so readers never block writers. Highest-risk phase — touches every storage path.

- [ ] `00080` Read-write separation — `RwLock` replacing `Mutex`, concurrent redb read transactions
- [ ] `00081` MVCC — tuple versioning (xmin/xmax), transaction snapshots, visibility checks
- [ ] `00082` Garbage collection — vacuum dead tuple versions, background GC thread
- [ ] `00083` Optimistic concurrency control — write-write conflict detection, retry semantics
- [ ] `00084` Isolation levels — Read Committed, Snapshot Isolation, Serializable (configurable)

**Definition of done:** 100 concurrent reader threads + 10 writer threads sustain > 500K ops/s on a 1M-node graph without deadlocks or anomalies.

---

### Phase 14 — Query Optimization

> Goal: keep Cypher queries fast on large graphs (1M+ nodes). Requires Phase 10 (Cypher) and Phase 13 (concurrency). Replaces O(N) full scans with O(1) index lookups; adds cost-based planning.

- [ ] `00085` Cost-based query planner — statistics collector, cardinality estimates, plan caching
- [ ] `00086` Traversal optimization — pattern reordering, index selection, join strategies
- [ ] `00087` Supernode handling — lazy edge iteration, cursor-based pagination, degree-aware planning
- [ ] `00088` Persistent property index — spill in-memory HashMap to redb B-tree at threshold (> 10M entries)
- [ ] `00089` Memory budget & backpressure — OOM guard, memory-limited query execution

**Definition of done:** query plan visible via `EXPLAIN`; representative queries on a 1M-node graph stay within current single-hop latency targets.

---

### Phase 15 — Ecosystem & Production Ops

> Goal: production deployment and ecosystem (MCP, Web UI, Python SDK, CDC, replication, fuzz testing, algorithms). Many tasks are parallelizable; order below is by user-visible value, not technical dependency.

- [ ] `00090` MCP server — `drevo-mcp` stdio binary for Cline / Claude Code, embedded storage, no Docker required
- [ ] `00091` MCP validation E2E suite — count, labels, rels, traversal, properties
- [ ] `00092` Web UI — `axum` + Cytoscape.js graph explorer (port 7474), query bar, node inspector
- [ ] `00093` Web UI kinetics — fcose physics layout, double-click expand, dynamic colors, tooltips
- [ ] `00094` Authorization & RBAC — role-based access control, scoped permissions
- [ ] `00095` Replication MAIN/REPLICA — WAL-based sync, read scaling
- [ ] `00096` Streaming ingestion — Kafka/NATS consumer for real-time graph updates
- [ ] `00097` CDC PostgreSQL sync — change data capture pipeline, schema mapping
- [ ] `00098` Built-in graph algorithms — PageRank, Dijkstra (already implemented — port to Cypher procedures), community detection (Louvain)
- [ ] `00099` Fuzz testing — grammar-aware Cypher fuzzer, multiple targets, regression corpus
- [ ] `00100` Python SDK — separate repository, PyO3 / FFI bindings, FastMCP tool wrappers, pip-installable
- [ ] `00101` Benchmark vs competitors — KuzuDB, Memgraph, Neo4j comparison runs in CI
- [ ] `00102` Comprehensive docs — user guide, admin guide, SDK reference, Cypher reference, migration guide

**Definition of done:** `docker run ghcr.io/ice1x/drevo` ships with Bolt + HTTP + Web UI + MCP integrated; Python SDK is published to PyPI; the comparison table above is updated with measured numbers from task `00101`.

---

### Phase 8.5 — Codebase Audit & Refactor (skill-anchored)

> **Re-ranked as the immediate next priority** (before remaining Phase 8/9 tasks). The 9.5k LOC of production code in this repo were written **before the project's four skill specs existed** (`drevo-tdd`, `drevo-rust`, `drevo-architecture`, `drevo-database` — under `.claude/skills/`). Phase 8.5 audits the existing code against those skill rules and refactors where it has drifted, BEFORE Phase 10 (Cypher) and Phase 13 (MVCC) put heavy new layers on top of the same surfaces.
>
> Each task below cites the exact skill rules it must verify against — auditors should load the relevant skill before starting and compare the code under audit line-by-line against the cited rules.
>
> **Cross-cutting acceptance criteria for every Phase 8.5 task:**
>
> - Output: a domain audit report `audit/AUDIT-{domain}.md` listing every divergence from a cited skill rule, with file:line references.
> - Each rule violation is either fixed by a follow-up refactor PR (cited in the report) or explicitly accepted ("no refactor — reason: …").
> - Test baseline must not regress: `cargo test --all-features` keeps producing ≥ 1092 passing tests; new property / proptest / fuzz cases added by the audit may grow the count.
> - `cargo clippy --all-targets --all-features -- -D warnings` clean.
> - `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean.
> - No public API breakage without an explicit `BREAKING:` line in the commit body.
> - `cargo fmt --check` clean.

#### Universal rules verified by every task (from `drevo-tdd` + `drevo-rust`)

- [ ] No `unwrap()` / `expect()` in library code (allowed only in `#[cfg(test)]` blocks and `benches/`). Cited: `drevo-rust` §"Error Handling"; `drevo-architecture` anti-pattern #5.
- [ ] No `unsafe` without an explicit justification comment. Cited: `drevo-rust` §"Code Style".
- [ ] Every `pub` item has a rustdoc comment. Cited: `drevo-rust` §"Code Style".
- [ ] Max 3 levels of indentation per function. Cited: `drevo-rust` §"Code Style"; `drevo-architecture` anti-pattern #6.
- [ ] Every `pub fn` returning a fallible result uses `Result<T, DrevoError>` — no ad-hoc `Box<dyn Error>`. Cited: `drevo-rust` §"Error Handling".
- [ ] All test data in English (per `CLAUDE.md`). Cited: `drevo-tdd` §"Project-Specific Conventions".

---

#### Per-domain audit tasks

- [ ] `00103` **Storage layer audit** — `src/storage/*` (~820 LOC). Verify against `drevo-database` §"Storage Backend Abstraction" + §"Indexes":
  - LSP — `MemoryBackend` and `RedbBackend` are observationally identical (`drevo-architecture` §SOLID "L"). Run the existing macro-parameterised test suite (`drevo-tdd` §"Storage tests parameterized by backend") and add a proptest that compares an arbitrary operation sequence between the two backends.
  - `scan_prefix` MUST return lexicographically-ordered keys on both backends (`drevo-database` "storage abstraction" doc-contract).
  - `flush()` semantics are documented and divergent paths are gated correctly (memory backend may snapshot to disk; redb is a no-op).
  - Mutex poisoning on `MemoryBackend` (`RwLock`) is mapped to a typed error variant, not a panic (`drevo-rust` §"Error Handling").
  - `#[cfg(not(target_arch = "wasm32"))]` gates correctly on FS-touching paths (`drevo-rust` §"WASM Bindings"; common pitfall #3).
  - **Refactor targets**: backend parity proptest; structured mutex-poisoning error; document `scan_prefix` ordering on the trait.

- [ ] `00104` **Error hierarchy audit** — `src/error.rs`, `src/storage/error.rs` + every `?` site in the codebase (~75 LOC of types + ~hundreds of call sites). Verify against `drevo-rust` §"Error Handling" + `drevo-architecture` §"Error Propagation Architecture":
  - Single error enum per crate via `thiserror` (`drevo-rust`). Currently the codebase has `DrevoError` AND `StorageError`. Decide: keep two-layer (Storage → DrevoError → HTTP) per the layered diagram, or collapse to one — and align with the *Immediate subtasks* item "Rename `StorageError` to `DrevoError` or reconcile error hierarchy" that has been open since phase 1.
  - No `StorageError::Backend(String)` stringly-typed errors (`drevo-architecture` anti-pattern #3): replace with `Redb(redb::Error)` / `Io(io::Error)` structured variants.
  - Every `?` site uses propagation, not manual `match` conversion (`drevo-rust`).
  - HTTP layer (`api.rs`) maps every `DrevoError` variant to a status code — no variant falls through to a default 500 (`drevo-rust` §"Error layering across boundaries").
  - **Refactor targets**: structured `StorageError` variants; close the open *immediate subtask*; exhaustive `ApiError::Db(_) → status` match (clippy `-W non_exhaustive_omitted_patterns`).

- [ ] `00105` **Model layer audit** — `src/model.rs` (~615 LOC). Verify against `drevo-database` §"Data Model" + `drevo-rust` §"Serialization":
  - Invariant: UUID immutability (`drevo-database` invariant #4) — proptest a `create → update → get` cycle and assert UUID unchanged.
  - `properties: HashMap<String, serde_json::Value>` serde round-trip on native + WASM (`drevo-database` "data model"; `drevo-rust` §"Serialization").
  - `NewNode` / `NodePatch` / `NewEdge` / `EdgePatch` patch semantics documented per field; partial-update edge cases (None vs Some(empty)) covered by tests.
  - Bincode v2 used for KV; serde_json for configs/dumps (`drevo-rust`). Verify `bincode::config::standard()` is the only config in use.
  - `Direction` enum is closed (`drevo-architecture` §SOLID "O" — `Value` enum is closed; the analogous reasoning applies).
  - **Refactor targets**: proptest serde round-trip on every `pub` struct in the module; field-level patch-semantics rustdoc.

- [x] `00106` **DB core audit** — `src/db.rs` (~1897 LOC; **split into 4 sub-passes if a single context is too tight**: 106a lifecycle, 106b node CRUD + indexes, 106c edge CRUD + adjacency, 106d query/scan paths). Verify against `drevo-database` §"Invariants" + `drevo-architecture` §"Anti-Patterns" + `drevo-tdd` §"Edge cases mandatory":
  - **Invariant #1 — Adjacency consistency** (`drevo-database`): every edge in `out_edges[from_id]` mirrored in `in_edges[to_id]`. Add a `Drevo::verify_invariants()` test-only helper and an end-to-end proptest that does N random mutations and asserts the invariant after each.
  - **Invariant #2 — Cascading delete**: deleting a node removes incident edges + adjacency entries + FTS entries (`drevo-database`; `drevo-rust` common pitfall #1).
  - **Invariant #3 — FTS reindex on update**: changing `title` or `body` deindexes the old text and indexes the new (`drevo-rust` common pitfall #2). Already partially tested — verify exhaustive coverage.
  - **Invariant #4 — UUID immutability**: cross-link with task `00105`.
  - God object signal (`drevo-architecture` anti-pattern #1) — 1897 LOC in one file is over the threshold; consider splitting into `db/{lifecycle, node_crud, edge_crud, indexes, query}.rs` once the audit confirms cohesion can be improved.
  - Per-operation redb txn pitfall (`drevo-rust` common pitfall #4) — confirm bulk paths batch writes.
  - **Refactor targets**: extract index maintenance into a dedicated module so mutation paths cannot forget an index update; introduce `verify_invariants()`; consider the `db/` split.

- [x] `00107` **Traversal audit** — `src/traversal.rs` (~1107 LOC). Verify against `drevo-database` §"Graph Traversal" + `drevo-architecture` §"Algorithm Design Principles" + `drevo-tdd` §"Edge cases mandatory":
  - BFS / DFS / Dijkstra / subgraph each hit the documented complexity bound (BFS/DFS O(V+E); Dijkstra O((V+E) log V)).
  - Edge-kind filter is pushed into the traversal (`drevo-database` §"edge-kind filtering at the traversal level is dramatic — 50µs vs 245µs"). Verify all four algorithms support it consistently.
  - Mandatory edge cases (`drevo-tdd`): empty graph, single node, cycles, disconnected components, depth 0, max depth, self-loops, parallel edges. Spot-check coverage in `tests/traversal_edge_case_tests.rs`.
  - Dijkstra preconditions: non-negative weights. Document and add a test that asserts behaviour on negative weights (panic? error? silent corruption?).
  - **Refactor targets**: unify edge-kind filter + direction handling behind a common cursor abstraction (`drevo-architecture` §"Strategy Pattern"); document weight preconditions in rustdoc.

- [x] `00108` **FTS audit** — `src/fts/*` (~535 LOC). Verify against `drevo-database` §"FTS index" + `drevo-tdd` §"Property-based tests for invariants":
  - Tokenizer: lowercase + strip punctuation; CJK → bigrams (`drevo-database`). Property-test on Unicode classes (CJK / Cyrillic / emoji / combining diacritics / RTL).
  - Posting-list intersection semantics (`drevo-database` "intersect posting lists, rank by TF-IDF").
  - Performance watch (`drevo-database` §"Performance Watch List"): `search_fts` on broad queries ~800ms vs 50ms target — document the gap and propose mitigation (cached posting-list lengths, batch scan, inverted-index compaction); landing the fix is out of scope for the audit task — flag for a separate refactor.
  - `list_recent` updates `updated_idx` on every node mutation (`drevo-database` §"updated_idx").
  - **Refactor targets**: tokenizer fuzz target (overlaps with Phase 9 task `00058` — clarify division of labour); extract scoring into a strategy trait so BM25 (`drevo-database` "Optional Phase 2") can swap in.

- [x] `00109` **HTTP API audit** — `src/api.rs` (~765 LOC). Verify against `drevo-database` §"HTTP API" + `drevo-architecture` §"Anti-Patterns" + `drevo-rust` §"FFI / WASM error layering" (the JSON boundary is conceptually the same):
  - Handler duplication across node/edge CRUD (`drevo-architecture` anti-pattern #2 "Premature Abstraction" vs anti-pattern #10 "Mixing Concerns in Match Arms" — there's now enough duplication that the "Three strikes and you refactor" rule applies).
  - Error mapping: every `DrevoError` variant → HTTP status (`drevo-rust` §"Error layering"). Use `#[deny(non_exhaustive_omitted_patterns)]` to prove it.
  - Query-parameter validation: `limit` cap, `offset` overflow, `depth: u8` saturation. Fuzz the query-string parser.
  - JSON contract regression: every wire-format field is documented in rustdoc on its struct.
  - Pre-existing `eprintln!` calls in handler error paths (if any) → structured logging (cross-link with `00112`).
  - **Refactor targets**: extract a generic CRUD handler trait (`drevo-architecture` §SOLID "I" — small focused traits); add `#[deny(non_exhaustive_omitted_patterns)]` on the `ApiError` match.

- [x] `00110` **FFI audit** — `src/ffi.rs` (~822 LOC). Verify against `drevo-rust` §"FFI Safety" + `drevo-database` §"FFI Boundary":
  - **CRITICAL — No panics across FFI** (`drevo-rust` §"No panics across FFI" — "Panics across the FFI boundary are undefined behavior"). Wrap every `extern "C"` function in `std::panic::catch_unwind`. Convert panics to error codes via the thread-local error mechanism.
  - Opaque handle pattern: `drevo_t*` is opaque; `drevo_open` / `drevo_close` are paired; double-free is detected (returns an error, never UB).
  - String ownership: returned strings freed via `drevo_free_string()` (`drevo-rust`).
  - UTF-8 validation on every `*const c_char` input.
  - Thread-local error correctness across reentrant calls.
  - `cbindgen` header generation is in sync with the Rust signatures.
  - **Refactor targets**: `with_panic_guard!` macro wrapping every entry; `cargo miri` smoke tests for the C surface; document the double-free behaviour.

- [ ] `00111` **WASM audit** — `src/wasm.rs` (~432 LOC). Verify against `drevo-rust` §"WASM Bindings" + `drevo-database` §"WASM Boundary":
  - JSON parity with native: every type that crosses the boundary serialises identically (`drevo-rust` §"JSON over the boundary"). Add a parity proptest that round-trips the same JSON through both code paths.
  - Errors become JS exceptions via `JsValue::from_str` (`drevo-rust`). No `panic!` in the WASM path.
  - `getrandom/wasm_js` feature is enabled and UUID v7 entropy works in browser (`drevo-rust` §"`getrandom` for UUID v7").
  - Memory-only persistence: no FS code paths leak into the WASM build. Verify with `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm`.
  - **Refactor targets**: parameterise the WASM test suite to also run under `wasm-pack test --headless` against a real browser (currently only Node.js); document the IndexedDB-fallback story.

- [x] `00112` **Server binary + ops audit** — `src/bin/server.rs` (was 93 LOC, post-refactor 52 LOC shim + new `src/server.rs` 340 LOC). Verify against `drevo-rust` §"Async / Tokio" + `drevo-database` §"HTTP API":
  - Env-var parsing: `DREVO_PORT` bounds (u16, 1024+ recommended in container); `DREVO_DATA_DIR` path validation.
  - Replace `eprintln!` with `tracing` + `tracing-subscriber` (the project doesn't have a logging story yet; introducing one here also unblocks `00109`).
  - Signal handling on Windows (currently `cfg(unix)` only) — either document the limitation or implement `Ctrl-Break` for Windows.
  - The newly-added `signal_shutdown()` flow from task `00048` is correct — cross-link with that task's PR.
  - **Refactor targets**: `tracing` integration; `--config-file` CLI flag; document Windows signal behaviour.

- [x] `00113` **Cross-cutting audit**. Verify against `drevo-tdd` §"Coverage Targets" + `drevo-rust` §"Code Style":
  - Test coverage by module — every `pub fn` has at least one direct test (`drevo-tdd` "every public method — at least 1 test"). Run `cargo llvm-cov` (or `cargo tarpaulin`) and produce a per-module heatmap.
  - Dead code: `cargo +nightly udeps`, `cargo machete`, `#[warn(dead_code)]` review for `pub` items with zero callers.
  - Doc coverage: `cargo doc --no-deps -- -D missing_docs`.
  - Strict clippy: triage `-W clippy::pedantic` and `-W clippy::nursery`. Adopt what fits the style.
  - MSRV: declare in `Cargo.toml` and CI matrix.
  - Bench parity: every performance-critical path (CRUD, traversal, FTS) has a criterion bench (`drevo-tdd` §"Benchmarks"). Identify gaps.
  - Scenario-test coverage of all five domains (CBT, story, task, ERP, bug tracker) is current; spot-check for new gaps post Phase 7/8.
  - **Refactor targets**: `make audit` Makefile target that runs the strict matrix; MSRV declaration; close any test-coverage gap below ~90% per module.

**Definition of done for Phase 8.5:** `audit/AUDIT-storage.md`–`audit/AUDIT-crosscut.md` exist, each citing the skill rules it verified; every cited rule is either ✅ compliant or has a follow-up refactor PR / accepted exception recorded; the 1092-test baseline grows with new property / proptest / fuzz cases added during the audit; clippy `-D warnings` stays clean across native + WASM; `cargo doc -D missing_docs` passes.

**Progress (2026-05-20, after task 00058):** Coverage-guided fuzz harness for the FTS tokenizer lands. A new `fuzz/` sub-crate (separate workspace, nightly-only build, declares `[workspace] members = ["."]` so `cargo` from the repo root ignores it) ships three `cargo-fuzz` + `libFuzzer` targets — `fuzz_normalize`, `fuzz_trigrams`, `fuzz_extract_trigrams` — each with a UTF-8 seed corpus under `fuzz/seed_corpus/<target>/` (15 seeds covering ASCII, CJK, Cyrillic, Latin-1 extended, Turkish dotless I, emoji, combining diacritics, digits, control chars, punctuation runs, single chars, multi-space). The same assertion bodies are mirrored in `tests/fts_tokenizer_fuzz_harness_tests.rs` (6 new tests, baseline 1312 → 1318) so the stable `cargo test` matrix replays the seed corpus on every PR without needing the nightly toolchain — and so the harness assertions cannot silently drift from the fuzz targets (the file's module-doc spells out the lock-step contract). Two wiring guards (`every_fuzz_target_has_a_seed_corpus`, `fuzz_cargo_toml_declares_every_target`) catch missing `[[bin]]` entries or empty corpus directories before they hit the nightly job. CI gains a non-blocking `fuzz` job that installs `cargo-fuzz`, runs each target for 60 s under `cargo +nightly fuzz run … -- -max_total_time=60`, and uploads `fuzz/artifacts/` on a crash. Division of labour with `00057` (proptest, stable, deterministic shrinking) and `00108` (audit-grade xorshift32 fuzzer in `tests/fts_audit_tests.rs`) is documented in `fuzz/README.md` and `audit/AUDIT-fts.md` §"#follow-up-tokenizer-fuzz". `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` both clean.

**Progress (2026-05-19, after task 00113):** 00103 (storage) ✅, 00104 (error) ✅, 00105 (model) ✅, 00106 (db core) ✅, 00107 (traversal) ✅, 00108 (fts) ✅, 00109 (http api) ✅, 00110 (ffi) ✅, 00111 (wasm) ✅, 00112 (server + ops) ✅, 00113 (cross-cutting) ✅ — `audit/AUDIT-crosscut.md` lands MSRV declaration (`rust-version = "1.85"` + dedicated CI job), `make audit` Makefile target, crate-level rustdoc + `#![warn(missing_docs)]`, `cargo machete` clean (`getrandom` documented), per-module coverage heatmap (90.95% region overall), and three adopted `clippy::nursery` wins (`missing_const_for_fn` ×3, `suboptimal_flops` ×1). Baseline 1216 → 1224 tests. **Phase 8.5 complete.**

**Progress (2026-05-19, after task 00057):** Phase 9 hardening kicks off — property-based tests for graph invariants land. `proptest` dev-dep added; three test files (`tests/proptest_graph_invariants.rs`, `tests/proptest_fts_tokenizer.rs`, `tests/proptest_model_serialization.rs`) ship 24 property tests covering (a) the four storage invariants from `.claude/skills/drevo-database/SKILL.md` under arbitrary CRUD op sequences (1..96 ops × 64 cases), (b) tokenizer round-trips — idempotent `normalize`, sorted-and-deduped `trigrams`, 2-or-3-char window bound, punctuation-only inputs produce no trigrams (512 cases each), and (c) bincode + JSON round-trips for `Node` / `Edge` with order-independent bincode for `Properties` (256 cases). Baseline 1224 → 1248 tests; `cargo clippy -- -D warnings` clean. Failing inputs are auto-shrunk by proptest and cached under `proptest-regressions/` for replay.

**Progress (2026-05-20, after task 00056):** GraphML export ships, extending the `src/dump.rs` module from task `00055`. Read-only GraphML 1.0 surface: `Drevo::{export_graphml, export_graphml_to_path}` plus `HTTP GET /export/graphml` (`application/xml; charset=utf-8`). The document declares the GraphML XSD namespace, opens `<graph id="drevo" edgedefault="directed">` (matches drevo's directional edges), emits node / edge ids as `n<id>` / `e<id>` (valid `xs:NMTOKEN`), and serialises `Properties` as a single JSON-string `<data>` value — lossless for any GraphML-aware tool (yEd, Gephi, NetworkX, Cytoscape, igraph). XML text escaping covers `<`, `>`, `&`, `"`, `'` and replaces C0 control characters with `U+FFFD`; weights are serialised as `xs:double`s with NaN / ±∞ surfaced as `"NaN"` / `"Infinity"` / `"-Infinity"` rather than empty `<data>` values. Output is deterministic (id-sorted nodes/edges, BTreeMap-sorted property keys). `tests/graphml_export_tests.rs` ships 17 integration tests (well-formedness, completeness, XML escaping, JSON-property round-trip, edge source/target wiring, byte-identical determinism, cross-backend Memory↔Redb parity, filesystem export, Unicode/CJK/emoji/Cyrillic survival, id-order emission); `tests/http_api_tests.rs` adds 3 endpoint tests; `src/dump.rs` adds 10 unit tests for the renderer / escaper / weight formatter / uuid helper. Baseline 1282 → 1312 tests. `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` both clean.

**Progress (2026-05-20, after task 00055):** JSON import/export ships under the new `src/dump.rs` module. The `drevo-json-v1` wire format carries `format` / `exported_at` / `next_node_id` / `next_edge_id` / `nodes` / `edges`; export is deterministic (`Properties` already sorts keys). Public surface: `Drevo::{export_json, export_json_to_path, import_json, import_json_from_path}` plus `HTTP GET /export/json` / `POST /import/json`. Import is idempotent for byte-identical rows (counted in `ImportReport::*_skipped`), rejects id collisions with different content via `DumpError::IdCollision → DrevoError::Io`, surfaces title conflicts as `DrevoError::DuplicateTitle`, and clamps `next_node_id` / `next_edge_id` above the imported range so future allocations never reuse imported ids. `pub(crate) Drevo::{collect_all_nodes, collect_all_edges, insert_node_raw, insert_edge_raw, bump_node_counter_to_at_least, bump_edge_counter_to_at_least}` are the new internal hooks. `tests/json_import_export_tests.rs` adds 25 integration tests (empty + single-node + full-graph round-trips on both backends, Memory↔Redb cross-backend parity, index rebuild verification, malformed-JSON / unknown-format / id-collision / title-collision rejection); `tests/http_api_tests.rs` adds 4 endpoint tests. Baseline 1248 → 1282 tests. `cargo clippy --all-targets --all-features -- -D warnings` and `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` both clean.

---

### Re-ranking Rationale (Senior PM Lens)

Why phases 10-15 are ordered this way, rather than appended in `ex/`-source order:

1. **Preserve real progress.** Phases 1-9 (tasks `00001`–`00060`) are real, tested, merged code. New work continues numbering from `00061` — no rewrites of history.
2. **Audit before extending — and against the skill specs.** Phase 8.5 (`00103`–`00113`) is re-ranked as the **immediate next priority**. The 9.5k LOC of production code in this repo was written **before the four project skill specs existed** (`.claude/skills/drevo-{tdd,rust,architecture,database}/SKILL.md`). Those specs now codify the project's TDD workflow, error handling, ownership patterns, redb transaction rules, FFI panic safety, SOLID + anti-patterns, and storage-layer invariants — so the audit is a compliance check of existing code against the skill rules, not a vague "look for issues" pass. Each task cites the exact skill rules it verifies. Audit findings → refactor PRs → continue Phase 8/9 → Phase 10.
3. **Critical path first.** Cypher (Phase 10) blocks Bolt (Phase 11) — Bolt has nothing to execute without a working Cypher engine.
4. **Parallelizable work next.** Vector storage (Phase 12) is independent of Cypher and Bolt; a second engineer can deliver it in parallel with Phases 10-11.
5. **Foundational concurrency before optimization.** MVCC (Phase 13) is touched by every read/write path and must land before the query optimizer (Phase 14) reasons about concurrent plans.
6. **Optimization once usage exists.** The query planner (Phase 14) only pays off after real Cypher queries run on real workloads — explicit dependency on Phase 10.
7. **Production / ecosystem last but parallelizable.** Phase 15 items (MCP, Web UI, replication, SDK, fuzz, algorithms, docs, CDC) run independently and concurrently. MCP and Web UI deliver visible value early.
8. **Risk weighting.** Phase 13 (MVCC) carries highest risk — schedule with buffer. Phase 12 (vector) is novel but isolated — failure mode is contained. Phase 11 (Bolt) is a well-documented protocol — low implementation risk once Cypher works.

---

### Immediate subtasks

> Tasks that require code changes to align with this spec but are not yet reflected in the implementation.

- [x] Rename crate from `grapevine` to `drevo` (Cargo.toml, lib.rs)
- [ ] Rename `StorageError` to `DrevoError` or reconcile error hierarchy
- [ ] Add `serde`, `bincode`, `uuid`, `redb` to Cargo.toml dependencies
- [x] Create `src/model.rs` with Node, Edge, NewNode, NodePatch structs per spec
- [x] Create `src/db.rs` with `Drevo` struct skeleton
- [x] Create `src/error.rs` with `DrevoError` enum

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

Senior Rust developer working on drevo. The project is educational, but the architecture must be production-grade.

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

- `Result<T, DrevoError>` on every public function
- Tests for every public method
- `#[derive(Debug, Clone, Serialize, Deserialize)]` where applicable
- Doc-comments on pub API
- No `unwrap()` in lib code, no `unsafe` without justification

---

## Current Status

**Phase:** 7 — HTTP API (Server Mode)

**Completed:**

- [x] `00001` StorageBackend trait
- [x] `00002` StorageError types
- [x] `00003` MemoryBackend (BTreeMap)
- [x] `00004` MemoryBackend persist/load (bincode snapshot to disk)
- [x] `00005` RedbBackend (ACID, B-tree, persistent)
- [x] `00059` GitHub Actions CI — test, clippy, fmt
- [x] Rename crate from `grapevine` to `drevo`
- [x] `00006` Shared integration test suite for both backends (macro-parameterized)
- [x] `00007` Benchmark: put/get/scan_prefix on 100K entries (criterion)
- [x] `00008` Define types: Node, Edge, NewNode, NodePatch, UUID v7
- [x] `00009` Drevo::open / open_in_memory / close / compact
- [x] `00010` Node CRUD: create_node, get_node, get_node_by_uuid, get_node_by_title, update_node, delete_node
- [x] `00011` Edge CRUD: create_edge, get_edge, get_edge_by_uuid, update_edge, delete_edge, edges_of
- [x] `00012` Kind index: list_nodes_by_kind, list_edges_by_kind with pagination
- [x] `00013` Cascading edge deletion on node removal + tests
- [x] `00014` Benchmark: insert 100K nodes + 500K edges, read all neighbors
- [x] `00015` Trigram tokenizer: normalize, trigrams, extract_trigrams (CJK bigrams, dedup, WASM-safe)
- [x] `00016` FTS index: trigram -> posting list storage (index/deindex on CRUD, intersect query)
- [x] `00017` search_fts with TF-IDF ranking (ScoredNode, smoothed IDF, limit, sorted results)
- [x] `00018` list_recent with inverted-timestamp updated_at index
- [x] `00019` FTS recall and edge-case tests (35 tests: query edge cases, IDF corners, Unicode, recall measurement)
- [x] `00020` FTS benchmark on 10K nodes (criterion: search, index insert, list_recent)
- [x] `00021` BFS with depth limit and optional edge kind filter (bfs, neighbors methods)
- [x] `00022` DFS with depth limit (dfs method, stack-based LIFO)
- [x] `00023` shortest_path via Dijkstra with edge weights
- [x] `00024` subgraph(root, depth) — BFS in Both directions, SubGraph struct
- [x] `00025` Cross-algorithm traversal edge-case tests (28 tests: cycles, disconnected, empty, single node, depth 0, self-loops, diamonds, long chains, direction filtering, edge kind filtering, parallel edges, max depth, 5 use-case scenarios)
- [x] `00026` Traversal benchmark: BFS/DFS/shortest_path/subgraph on 100K nodes, degree 10 (criterion)
- [x] `00027` C FFI header (`drevo.h`) — opaque handle, JSON serialization, thread-local error, cbindgen auto-generation
- [x] `00028` WASM bindings (`wasm-bindgen`) — WasmDrevo JS class, JSON serialization, memory-only backend, 30 integration tests
- [x] `00029` WASM redb verification + fallback — redb excluded on WASM via compile-time cfg, MemoryBackend as fallback, feature-gated Cargo.toml
- [x] `00030` Cross-compilation CI — GitHub Actions workflow for iOS (aarch64-apple-ios), Android (aarch64-linux-android), WASM (wasm32-unknown-unknown), plus 10 cross-compilation validation tests
- [x] `00031` Platform smoke tests — 6 tests: MemoryBackend full workflow, RedbBackend full workflow with persistence verification, FFI C API roundtrip, WASM-compatible API surface with JSON roundtrip, disk persistence, Unicode/i18n (CJK, emoji, Cyrillic)
- [x] `00032` CBT journal scenario — 42 tests: thought chains, distortion pattern search, reframing edges, BFS/DFS/shortest_path/subgraph, kind index, FTS, properties, multi-entry journal, weighted edges, update/delete workflows
- [x] `00033` Story editor scenario — 53 tests: tree structure (book→chapter→scene), character graph, scene ordering via follows edges, subgraph for AI context extraction, FTS across narrative content, kind index board views, location sharing, plot points, update/delete workflows
- [x] `00034` Task manager scenario — 64 tests: epic/sprint/task/developer/component graph, dependency chains, blocking BFS, reverse blocking chain, shortest path through blocking chain, sprint board via kind_index, developer workload, component ownership, FTS, subgraph, edge kind index, CRUD lifecycle, weight/property updates
- [x] `00035` ERP scenario — 76 tests: customer/warehouse/product/order/invoice graph, order → customer (ordered_by), order → product line items with qty/line_total edge properties, product → warehouse inventory with stock edge properties, invoice → order → customer billing chain, kind index board views, FTS across domain content, subgraph for order context, shortest path invoice→warehouse, transactional status updates, inventory restocking, order cancellation via edge deletion, cascade delete of products/orders, total stock and inventory value aggregations

**Test status:**

```
cargo test: 1077 passed, 0 failed (201 unit + 875 integration + 1 doctest)
cargo clippy: 0 warnings
CI: GitHub Actions — check, test, clippy, fmt (all green)
```

**Benchmark results (Apple Silicon, criterion):**

Storage layer (KV operations):

| Benchmark | MemoryBackend | RedbBackend |
|---|---|---|
| put (1K ops) | ~329 µs | ~5.26 s |
| get (single, from 100K) | ~570 ns | ~1.38 µs |
| scan_prefix (1K results, from 100K) | ~120 µs | ~163 µs |
| bulk_put 100K | ~43 ms | ~530 s (per-txn) |

Graph layer (MemoryBackend, 100K nodes + 500K edges):

| Benchmark | Time |
|---|---|
| insert 100K nodes | ~347 ms |
| insert 500K edges (into 100K nodes) | ~2.85 s |
| get_node (random, from 100K) | ~1.0 µs |
| edges_of outgoing (random node, 5 edges) | ~6.7 µs |
| edges_of both (random node, ~10 edges) | ~14.2 µs |
| list_nodes_by_kind (limit 100, 10K per kind) | ~697 µs |
| list_nodes_by_kind (limit 1000, 10K per kind) | ~1.26 ms |

> RedbBackend graph benchmarks skipped — per-operation ACID transactions make 100K+ inserts impractical (~8+ min). The graph layer will batch writes in transactions for production use.

FTS layer (MemoryBackend, 10K nodes):

| Benchmark | Time |
|---|---|
| search_fts single word (limit 10) | ~806 ms |
| search_fts two words (limit 10) | ~898 ms |
| search_fts three words (limit 10) | ~1.09 s |
| search_fts selective phrase (limit 10) | ~132 ms |
| search_fts rare term (limit 10) | ~894 ms |
| search_fts common term (limit 10) | ~772 ms |
| search_fts selective (kind_filter, limit 10) | ~20 ms |
| search_fts mixed 4 words (limit 10) | ~133 ms |
| index insert 1K nodes | ~228 ms |
| list_recent (limit 10, 10K nodes) | ~686 µs |
| list_recent (limit 50, 10K nodes) | ~748 µs |
| list_recent (limit 100, 10K nodes) | ~778 µs |
| list_recent (limit 500, 10K nodes) | ~1.21 ms |

> **Note:** search_fts on broad queries (single/two/three words) exceeds the 50ms target due to scan_prefix overhead on large posting lists. Selective queries (few matching trigrams) meet the target. Optimization opportunities: cached posting list lengths, batch scan, or inverted-index compaction. The limit parameter has negligible effect — bottleneck is posting list retrieval, not sorting/truncation.

Traversal layer (MemoryBackend, 100K nodes + 1M edges, degree 10):

| Benchmark | Time |
|---|---|
| BFS outgoing depth 1 | ~27 µs |
| BFS outgoing depth 2 | ~245 µs |
| BFS outgoing depth 3 | ~1.73 ms |
| BFS both depth 2 | ~849 µs |
| BFS filtered (edge kind) depth 2 | ~50 µs |
| DFS outgoing depth 3 | ~1.77 ms |
| DFS both depth 2 | ~874 µs |
| shortest_path nearby nodes | ~1.09 s |
| shortest_path distant nodes | ~969 ms |
| shortest_path same node | ~1.1 µs |
| subgraph depth 1 | ~717 µs |
| subgraph depth 2 | ~7.0 ms |
| subgraph depth 3 | ~60 ms |

> **Note:** BFS/DFS depth 3 on 100K nodes (degree 10) completes in ~1.7ms — well within interactive latency. Subgraph depth 3 is slower (~60ms) due to edge collection across the discovered node set. Shortest path (Dijkstra) on the full graph takes ~1s because it explores the entire reachable set before finding the target — expected for dense graphs with uniform weights. Edge kind filtering dramatically reduces traversal cost (~50µs vs ~245µs at depth 2).

**Phase 5 complete.** C FFI, WASM bindings, WASM redb verification, cross-compilation CI, and platform smoke tests all implemented.

**WASM platform strategy:**
- **redb does not compile for `wasm32-unknown-unknown`** — it depends on filesystem I/O (`std::fs`, `std::path`) unavailable in browser environments
- **Fallback: `MemoryBackend` exclusively** — compile-time `#[cfg]` gates ensure `RedbBackend` and disk-backed `Drevo::open()` are excluded on WASM
- **Feature-gated Cargo.toml**: `redb-backend` (default) enables redb on native; `wasm` enables `wasm-bindgen`, `js-sys`, `getrandom/wasm_js` for browser
- **`MemoryBackend` persistence methods** (`open(path)`, `flush()` to disk) are gated behind `#[cfg(not(target_arch = "wasm32"))]`
- **`cbindgen` build step** is feature-gated — skipped on WASM builds
- **UUID v7 entropy** uses `getrandom` with `wasm_js` feature for browser-compatible RNG
- **Verified**: `cargo check --target wasm32-unknown-unknown --no-default-features --features wasm` compiles cleanly

**FFI layer design:**
- Opaque handle pattern: C consumers receive `drevo_t*` — an opaque pointer
- JSON serialization: complex types (Node, Edge, SubGraph, ScoredNode) cross FFI as JSON C strings
- Thread-local error: `drevo_last_error()` returns last error, cleared on success
- Memory ownership: caller frees returned strings via `drevo_free_string()`
- Auto-generated header: `cbindgen` produces `drevo.h` at build time
- 21 FFI functions: lifecycle (3), node CRUD (4), edge CRUD (4), traversal (5), search (3), utility (2)

**WASM bindings design:**
- Wrapper class: `WasmDrevo` exported as JS class via `wasm-bindgen`
- JSON serialization: complex types cross WASM boundary as JS objects via `serde_json` + `js_sys::JSON`
- Error handling: Rust errors converted to JS exceptions via `JsValue::from_str`
- Memory-only: WASM targets use `MemoryBackend` exclusively (no filesystem in browser)
- Feature-gated: `cargo build --features wasm` to include WASM bindings
- 17 WASM methods: lifecycle (2), node CRUD (4), edge CRUD (4), traversal (5), search (3)

**Cross-compilation CI design:**
- Separate workflow (`cross-compile.yml`) to avoid slowing down the main CI
- **WASM job** (ubuntu): `cargo check` + `cargo build` with `--no-default-features --features wasm`, verifies `.wasm` artifact
- **iOS job** (macos): `cargo check` + `cargo build` with default features, verifies `.a` static library and `drevo.h` header
- **Android job** (ubuntu): installs Android NDK, configures linker, `cargo check` + `cargo build` with `--no-default-features --features redb-backend`
- **Host tests job**: runs `cross_compilation_tests.rs` (10 tests: feature gates, API surface, portability)

**Platform smoke test design:**
- 6 smoke tests in `tests/smoke_tests.rs` covering every platform path
- **MemoryBackend workflow**: open → create 3 nodes → get by id/uuid/title → update → create edges → edges_of → neighbors → BFS → DFS → shortest_path → subgraph → FTS search → list_by_kind → list_recent → delete with cascade → close
- **RedbBackend workflow**: same full workflow + persistence verification (reopen DB, check data survived)
- **FFI workflow**: same via C API (extern "C") — opaque handle, JSON serialization, string ownership, error checking
- **WASM-compatible**: full API surface + JSON roundtrip for all types (Node, Edge, SubGraph, ScoredNode, Properties)
- **Disk persistence**: MemoryBackend bincode persist/load on native
- **Unicode/i18n**: CJK, emoji, Cyrillic content roundtrip + FTS search
- CI: smoke tests run on Ubuntu and macOS hosts; WASM/iOS/Android verified via compilation checks

**Next steps (re-ranked per the long-term roadmap):**

1. **🔍 Audit & Refactor — Phase 8.5** (`00103`–`00113`, **immediate next priority**): per-domain compliance audit of the existing 9.5k LOC against the four `.claude/skills/drevo-*/SKILL.md` specifications (which were written AFTER the code). Each task cites the exact skill rules it verifies (TDD test-layer + edge-case rules, Rust error-handling / ownership / FFI safety, SOLID + anti-patterns, storage-layer invariants), produces an `audit/AUDIT-{domain}.md` report, and lands a targeted refactor PR for every rule violation found. Domains are independently scoped so they can be picked up in parallel. Run BEFORE the remaining Phase 8/9 tasks because the same refactor surface (`db.rs` index-maintenance, `error.rs` hierarchy, FFI panic safety) will be touched repeatedly by Phase 10 (Cypher) and Phase 13 (MVCC) — cheaper to clean it up now than refactor through three layers.
2. **Phase 8 — DONE** (`00045`–`00052`): Docker image, Compose, health/readiness, K8s manifests, Kustomize overlays, GHCR publish workflow, and container persistence integration test all complete.
3. **Finish Phase 9** (`00053`–`00054`): WAL / crash recovery and compaction. (`00055`–`00060` complete: JSON & GraphML import/export, property-based tests, FTS tokenizer fuzz, CI, rustdoc on public APIs.)
4. **Begin Phase 10** (`00061`–`00069`): Cypher query language — start with the lexer (`00061`), then parser, then executor for CREATE / MATCH / RETURN.
5. **Parallel track**: Phase 12 (`00075`–`00079`) — vector storage and HNSW index — can start independently as soon as Phase 10 is underway.
6. **Phase 15 early-value items**: `00090` MCP server and `00092` Web UI deliver visible value early and can run alongside Phases 10-12.

---

## MCP Server (Planned)

A planned `drevo-mcp` stdio binary will expose drevo as a [Model Context Protocol](https://modelcontextprotocol.io) server for Cline, Claude Code, and other MCP-compatible AI clients. The binary uses embedded storage (no Docker required) and is configured via the host's MCP settings file:

```json
{
  "mcpServers": {
    "drevo": {
      "command": "/path/to/drevo-mcp",
      "env": { "DREVO_DATA_DIR": "~/.drevo/data" }
    }
  }
}
```

The MCP server will expose node CRUD, edge CRUD, traversal, FTS, and (after Phase 10) Cypher query tools — enabling an AI agent to read and write the knowledge graph as part of a conversation.

Tracked as task `00090` (Phase 15).

---

## License

MIT
