# GraphNote DB — Embedded Graph Database for Knowledge Management

> **Status**: Historical specification. This document was the original design for an Obsidian-like knowledge base DB.
> The project evolved into **GrapeVine** — which adds vector search (HNSW) and a server mode (HTTP API + Docker)
> on top of the graph engine concepts described here. Key ideas preserved: redb storage, trait-based backend,
> BFS/DFS traversal, node/edge CRUD. See `ARCHITECTURE.md` for the current GrapeVine design.

## Project Goal

Build a lightweight, embeddable graph database in Rust, purpose-built for a cross-platform Obsidian-like knowledge base application. The database must run natively on desktop (via FFI/Tauri), mobile (iOS/Android via C bindings or WASM), and in the browser (via WebAssembly). No server required — everything runs in-process.

---

## Core Requirements

### Platform targets
- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin` / `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-apple-ios` / `aarch64-linux-android`
- `wasm32-unknown-unknown` (browser, Tauri v2 WASM)

### Non-goals
- No network protocol, no server mode
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
- `title_idx`:    BTreeMap<String, u64>
- `kind_idx`:     BTreeMap<String, Vec<u64>>
- `fts_idx`:      inverted index over `title + body` (trigram or BM25)
- `updated_idx`:  BTreeMap<i64, u64>  (for recent notes)

---

## Storage Engine

### File layout
```
<vault_dir>/
  graphnote.db          ← single binary file (custom format or redb)
  graphnote.db.lock     ← advisory lock
  graphnote.db.wal      ← write-ahead log (optional, for crash recovery)
```

### Recommended storage backend

Use **[redb](https://github.com/cberner/redb)** as the underlying key-value store.
- Pure Rust, no C dependencies
- ACID transactions
- WASM-compatible (with feature flags)
- Supports multiple named tables in one file
- Actively maintained

Alternative if redb has WASM issues: **[sled](https://github.com/spacejam/sled)** (embedded, pure Rust, LSM-tree).

### Tables in redb
```
nodes:       u64 → bincode(Node)
edges:       u64 → bincode(Edge)
node_uuid:   [u8;16] → u64
edge_uuid:   [u8;16] → u64
out_edges:   u64 → Vec<u64>      (adjacency list: from_id → edge_ids)
in_edges:    u64 → Vec<u64>      (reverse: to_id → edge_ids)
kind_index:  String → Vec<u64>
title_index: String → u64        (exact title lookup)
fts_index:   String → Vec<u64>   (trigram → node_ids)
meta:        String → Vec<u8>    (schema version, stats)
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

Implement a **trigram index** (simple, no external deps, WASM-safe):
- On `create_node` / `update_node`: tokenize `title + body` into trigrams, store `trigram → Vec<node_id>` in `fts_index`
- On `search_fts(query)`: extract query trigrams, intersect posting lists, rank by TF-IDF or hit count
- Normalize: lowercase, strip punctuation, CJK character support

Optional phase 2: integrate **[tantivy](https://github.com/quickwit-oss/tantivy)** for BM25 scoring (desktop only, not WASM).

---

## Crate Structure

```
graphnote-db/
  Cargo.toml
  src/
    lib.rs
    db.rs           ← GraphNoteDb impl
    model.rs        ← Node, Edge, NewNode, NodePatch, etc.
    storage.rs      ← redb table definitions and low-level ops
    index/
      mod.rs
      title.rs
      kind.rs
      fts.rs        ← trigram index
    traversal.rs    ← BFS/DFS, shortest path, subgraph
    transaction.rs  ← Txn wrapper
    export.rs       ← JSON / GraphML
    error.rs        ← GraphNoteError enum
    uuid.rs         ← UUID v7 generation
  benches/
    basic_ops.rs
  tests/
    crud.rs
    traversal.rs
    fts.rs
    concurrent.rs
```

---

## Dependencies (Cargo.toml)

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

## Serialization

Use **bincode v2** for all stored values:
- Compact binary, fast encode/decode
- Deterministic (important for hashing/checksums)
- `serde`-compatible — just `#[derive(Serialize, Deserialize)]` on model structs

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

## Phase Plan

### Phase 1 — Core (start here)
- `redb` storage setup, table definitions
- Node + Edge CRUD with adjacency lists
- Title index, kind index
- Basic unit tests

### Phase 2 — Search
- Trigram FTS index
- `list_recent`, `list_by_kind`

### Phase 3 — Traversal
- BFS/DFS neighbors
- Shortest path (Dijkstra, weighted by `edge.weight`)
- Subgraph extraction

### Phase 4 — Bindings
- C FFI header (`graphnote.h`) for iOS/Android
- WASM bindings via `wasm-bindgen`
- Optional: Python bindings via PyO3

### Phase 5 — Hardening
- WAL / crash recovery
- Compaction
- JSON import/export
- Benchmarks

---

## First Task for the New Session

> Initialize the crate, implement Phase 1 fully: storage layer with redb, Node/Edge CRUD, adjacency list maintenance, title and kind indexes, and a complete test suite in `tests/crud.rs` covering create, read, update, delete, and edge queries.
