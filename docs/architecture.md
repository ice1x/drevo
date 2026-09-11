# Architecture & Design

> Stable design reference extracted from the README: vision, requirements, data model, storage engine, the Rust API surface, serialization, error handling, performance targets, crate layout and dependencies.

## Long-term Vision: Graph-Vector Database

Beyond the embedded knowledge-base use case described above, drevo is on a multi-phase trajectory toward becoming a full graph-vector database with capabilities equivalent to Neo4j and Memgraph:

- **Cypher query language** — full support for CREATE / MATCH / MERGE / SET / DELETE / WHERE / RETURN / WITH / UNWIND / FOREACH / aggregations / variable-length paths (Phase 10)
- **Bolt wire protocol** — Neo4j-compatible, so `cypher-shell`, `neo4j-python-driver`, and `neo4j-javascript-driver` connect out of the box (Phase 11)
- **Native vector search** — `Value::Vector` type, HNSW index, joint graph+vector queries for RAG and semantic search (Phase 12)
- **MVCC concurrency** — readers never block writers, multiple configurable isolation levels (Phase 13)
- **Cost-based query planner** — statistics, cardinality estimates, plan caching, supernode handling (Phase 14)
- **Production ecosystem** — MCP server, web UI, Python SDK, replication, streaming ingestion, CDC, RBAC, observability (Prometheus `/metrics`, structured query log) (Phase 15)
- **Keyword analytics & lexical search** — BM25 ranking, `keywords()` extraction from properties/labels, similarity-collapsed faceted group-by-keyword (Phase 17)
- **Graph analytics procedures** — seven whole-graph algorithms exposed as Cypher `CALL drevo.<algo>() YIELD …` procedures (over HTTP, Bolt, and MCP), on both the KV and native engines: centrality (`pagerank`, `betweenness`, `closeness`), community detection (`louvain`), connectivity (`wcc` / `scc`), and density (`triangles` + local clustering coefficient) (RFC #307 Phase 8)

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

> **Note (engine evolution).** The design below describes the original **redb** key-value backend. Since the native-core program (see [`../PHASE-HISTORY.md`](../PHASE-HISTORY.md), Phase 7+ and the native-core baseline), the default deployment engine is **`native-durable`** — an in-memory graph with a JSON-Lines **write-ahead log** as the store of record (zero redb). The redb backend remains selectable via `DREVO_ENGINE=kv`; the WAL engine keeps its indexes in memory and rebuilds them from the log on open. Treat the redb table layout in this section as the **legacy KV** contract, not the current default. See [`native-core-baseline.md`](native-core-baseline.md) and [`native-load.md`](native-load.md) for the current engine.

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

> Competitor numbers are approximate, derived from published benchmarks and vendor claims — a rough positioning sketch, not a measured head-to-head. For a reproducible comparison, Phase 15 task `00101` ships the [`comparison_bench`](https://github.com/ice1x/drevo/blob/main/benches/comparison_bench.rs) harness and the [benchmarks guide](benchmarks.md): drevo's side is measured on your machine and the identical workload is specified as runnable code against each competitor, so the cross-engine numbers are something you *run* rather than copy from a slide.

### Native-core scoreboard — measured history

The native graph engine ([RFC #307](rfc-native-core.md)) measured on a **copy of real production data** (2 596-node knowledge-graph snapshot), Apple M1 Max, criterion midpoints. Each run appends to the full [native-core baseline](native-core-baseline.md) — the summary below is the headline per milestone.

| Date | Version / milestone | Headline result |
|---|---|---|
| 2026-08-26 | native engine, in-process (runs 1–4) | native + secondary indexes vs today's KV on real data: full scan **537 µs (≈560×)**, property-equality **9.3 µs (≈32 000×)**, hub 1-hop **60 µs (≈230×)** |
| 2026-08-27 | drevo `0.0.18` KV over Bolt vs Memgraph v3.12 (run 5) | production KV **loses** to Memgraph 24–535× over the same Bolt client; native in-process is already in Memgraph's class |
| 2026-08-28 | `DREVO_ENGINE=native` read mirror over Bolt (run 6) | engine flip: drevo **wins 4/6** rows vs Memgraph; Memgraph keeps ~1.2× only on two bare count-scans |
| 2026-08-28 | + count pushdown (run 7) | drevo **wins 6/6**, **1.4–5.8×** ahead through the identical Bolt client — "surpass Memgraph" met on this scoreboard |
| 2026-09-01 | `DREVO_ENGINE=native-durable`, zero redb (run 8) | WAL-backed store of record **wins 6/6, 1.2–6.8×**; durability is free on reads (matches the in-memory mirror within noise) |

> Numbers are drevo vs Memgraph on identical Cypher over the same Bolt client (runs 5–8) or native vs the KV engine in-process (runs 1–4). Reproduce with `scripts/memgraph_baseline_bench.py` (cross-DB) or `DREVO_BASELINE_GRAPHML=<graphml> cargo bench --bench real_data_baseline_bench` (KV-vs-native); the harness asserts both engines return identical rows before timing, so a wrong-answer speedup never counts.

### Native-core load & concurrency — measured

Beyond single-op latency: throughput under a concurrent thread sweep, deep
traversal, and durable writes (`native-durable` with a real WAL vs the KV
engine, same real-data snapshot, Apple M1 Max). Full numbers + p50/p95/p99 in
[docs/native-load.md](native-load.md).

| Workload | native-durable | KV |
|---|---:|---:|
| point read, 8 threads | **3.1 M ops/s** | 56 k |
| 3-hop BFS, 1 thread | **77 k ops/s** | 27 k |
| edge write — autocommit (fsync each) | 174 /s | **8.1 k** |
| edge write — **tx-batched** (one fsync/commit) | **172 k /s** | ~8 k |

> Reads win 3–700× and scale. The write path is the honest caveat: autocommit
> fsyncs every edge (~174/s), so **write-heavy callers must batch into a
> transaction** — one fsync per commit lifts it to ~172 k/s (~1 000×), ~20×
> above KV. Reproduce with `DREVO_BASELINE_GRAPHML=<graphml> cargo run --release
> --example native_load`.

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

