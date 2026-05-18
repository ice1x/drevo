---
name: drevo-database
description: drevo internals — storage layout, indexes, traversal, FTS, transactions, and the path toward Cypher executor, Bolt protocol, and vector search
---

# drevo — Database Internals

## When to Use
When implementing or modifying anything that touches storage, indexes, traversal, FTS, the HTTP API, the future Cypher executor, the future Bolt protocol, or the future vector index.

---

## Data Model

### Node
```
id:         u64            auto-increment, unique
uuid:       [u8; 16]       UUID v7, sortable, globally unique
kind:       String         "note", "tag", "person", "task", etc.
title:      String
body:       String         raw Markdown
body_html:  String         rendered, cached
created_at: i64            Unix milliseconds
updated_at: i64            Unix milliseconds
properties: HashMap<String, serde_json::Value>   arbitrary metadata
```

### Edge
```
id:         u64
uuid:       [u8; 16]
from_id:    u64            source node
to_id:      u64            target node
kind:       String         "links_to", "tagged_with", "depends_on", etc.
weight:     f32            default 1.0, used for ranking / Dijkstra
created_at: i64
properties: HashMap<String, serde_json::Value>
```

Cypher analogue: `kind` ≈ label (for nodes) or relationship type (for edges); `properties` ≈ Cypher property map.

---

## Storage Layout (redb tables)

```
nodes:        u64        -> bincode(Node)
edges:        u64        -> bincode(Edge)
node_uuid:    [u8;16]    -> u64                (UUID lookup)
edge_uuid:    [u8;16]    -> u64
out_edges:    u64        -> Vec<u64>           adjacency: from_id -> edge_ids
in_edges:     u64        -> Vec<u64>           reverse adjacency: to_id -> edge_ids
kind_index:   String     -> Vec<u64>           kind -> node_ids
title_index:  String     -> u64                exact title lookup
fts_index:    String     -> Vec<u64>           trigram -> node_ids
updated_idx:  i64        -> u64                inverted timestamp for list_recent
meta:         String     -> Vec<u8>            schema version, stats
```

### Invariants
1. **Adjacency consistency**: every edge in `out_edges[from_id]` is mirrored in `in_edges[to_id]`. Tested by cascading deletion tests.
2. **Cascading delete**: deleting a node removes all incident edges + their adjacency entries + their FTS entries.
3. **FTS reindex on update**: changing `title` or `body` requires deindexing the old text and indexing the new.
4. **UUID immutability**: once assigned, a node's UUID never changes (even on update). Used as a stable external identifier.

---

## Storage Backend Abstraction

```rust
pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&self, key: &[u8]) -> Result<()>;
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn flush(&self) -> Result<()>;
}
```

Two backends:
- **`MemoryBackend`** — `BTreeMap<Vec<u8>, Vec<u8>>` wrapped in `RwLock`. Used for tests, WASM, and ephemeral workloads. Optional `flush()` snapshots to disk via bincode.
- **`RedbBackend`** — wraps `redb::Database`. ACID, persistent, B+tree. Default on native.

All integration tests run against both backends via macro parameterization. This is the foundation that lets WASM exist at all.

---

## Indexes

### `title_index`
- B-tree map `String → u64` (one node per title)
- Used for `get_node_by_title` (O(log N))
- `DuplicateTitle` error if a title collision is attempted

### `kind_index`
- B-tree map `String → Vec<u64>`
- Powers `list_nodes_by_kind(kind, limit, offset)` — pagination is O(limit)
- Future Cypher `MATCH (n:Person)` maps to a `kind_index` lookup

### FTS index (`fts/`)
- Trigram inverted index — WASM-safe, no external dependencies
- On `create_node` / `update_node`: tokenize `title + body` into trigrams, store `trigram → Vec<node_id>`
- On `search_fts(query)`: extract query trigrams, intersect posting lists, rank by TF-IDF
- Normalization: lowercase, strip punctuation; CJK characters become bigrams (one Han / Hiragana / Katakana glyph = 1.5 trigrams of value)
- Optional Phase 2: integrate `tantivy` for BM25 scoring on native (not WASM)

### `updated_idx`
- Inverted-timestamp B-tree (newest first) for `list_recent(limit)`
- Updated atomically on every node mutation

### Future: persistent property index (Phase 14, task `00088`)
- Currently `properties` are stored inline on the node. Filtering by property requires a full kind scan.
- Phase 14 introduces a spill-to-disk HashMap: in-memory until threshold (>10M entries), then redb B-tree.

---

## Graph Traversal (`traversal.rs`)

All implemented; complexity is the algorithm's natural bound:

| Algorithm | Function | Complexity | Notes |
|-----------|----------|------------|-------|
| BFS | `bfs(start, depth, dir, kind_filter)` | O(V+E) on reachable | depth-limited; optional edge-kind filter |
| DFS | `dfs(start, depth, dir, kind_filter)` | O(V+E) on reachable | stack-based, LIFO |
| Dijkstra | `shortest_path(from, to)` | O((V+E) log V) | weighted by `edge.weight` |
| Subgraph | `subgraph(root, depth)` | O((V+E) within radius) | BFS Both directions, returns SubGraph |

Edge-kind filtering at the traversal level is dramatic — measured at ~50µs vs ~245µs at depth 2 in the criterion benches. Always push filters into the traversal when possible.

### Direction
```rust
pub enum Direction { Outgoing, Incoming, Both }
```
Outgoing = follow `out_edges`; Incoming = follow `in_edges`; Both = union.

---

## Transactions

Current implementation: redb's `begin_write()` / `commit()`. Single writer, multiple readers in different sessions.

The `transaction<F, T>(&self, f: F) -> Result<T>` API on `Drevo` wraps a closure in a write transaction; mutations inside the closure either all commit or all abort. Undo-log–based rollback (and explicit `BEGIN/COMMIT/ROLLBACK`) is Phase 9 task `00053` / Phase 11 task `00072`.

Future: MVCC (Phase 13) replaces single-writer locking with multi-version concurrency control.

---

## FFI Boundary (`ffi.rs`, `drevo.h`)

20 FFI functions:
- Lifecycle (3): `drevo_open`, `drevo_open_in_memory`, `drevo_close`
- Node CRUD (4): create / get / update / delete
- Edge CRUD (4): create / get / update / delete
- Traversal (5): `drevo_neighbors`, `drevo_bfs`, `drevo_dfs`, `drevo_shortest_path`, `drevo_subgraph` (`Drevo::edges_of` is reachable via `drevo_neighbors`; no dedicated FFI entry)
- Search (3): `drevo_search_fts`, `drevo_list_nodes_by_kind`, `drevo_list_recent`
- Utility (2): `drevo_last_error`, `drevo_free_string`

JSON over the boundary — see `drevo-rust` skill for details.

Every entry is wrapped in `std::panic::catch_unwind` via the `ffi_guard_ptr!` / `ffi_guard_int!` macros (see audit task `00110`). A panic across the boundary is caught, recorded as a thread-local error, and the function returns its error sentinel (`NULL` or `-1`) — never UB.

---

## WASM Boundary (`wasm.rs`)

`WasmDrevo` JS class exported via `wasm-bindgen`. 17 methods, JSON serialization for complex types. Memory-only (no filesystem in browser). Feature-gated: `cargo build --features wasm`.

---

## HTTP API (`api.rs`)

`axum` + `tokio` server. Mirrors the Rust API as REST endpoints:

```
POST   /nodes              create
GET    /nodes/{id}         get
PATCH  /nodes/{id}         update
DELETE /nodes/{id}         delete

POST   /edges              create
GET    /edges/{id}         get
PATCH  /edges/{id}         update
DELETE /edges/{id}         delete

GET    /nodes/{id}/neighbors?direction=...&kind=...
GET    /nodes/{id}/subgraph?depth=...
GET    /paths/shortest?from=...&to=...

POST   /search/fts         { query, limit }
GET    /nodes/by-kind/{kind}?limit=...&offset=...
GET    /nodes/recent?limit=...

GET    /health             liveness
GET    /status             stats
```

Unified JSON error handling: `DrevoError` → HTTP status + JSON body `{ error: "...", details: ... }`.

---

## Roadmap-Aware Hooks

When implementing new code, leave the door open for these upcoming subsystems. None of them ship in the current code, but they all sit on top of the existing storage.

### Cypher Executor (Phase 10, tasks `00061`–`00069`)
- Will live in `src/cypher/` (lexer.rs, parser.rs, ast.rs, executor.rs)
- Maps Cypher labels → `kind`; properties → `properties` HashMap; relationships → edges
- Reuses existing traversal for `MATCH (a)-[*1..3]->(b)`
- Pattern matching is the new code — everything below it is already there

### Bolt Protocol (Phase 11, tasks `00070`–`00074`)
- Will live in `src/bolt/` (codec.rs, session.rs, handshake.rs)
- PackStream codec → Cypher executor → result rows back as PackStream
- Reuses HTTP-layer auth model (after `00074`)

### Vector Storage (Phase 12, tasks `00075`–`00079`)
- New `Value::Vector(Vec<f32>)` variant on the property value enum
- HNSW index in `src/vector/` with persistence in a new redb table
- Cypher extension: `similar(n.embedding, $q, threshold)` predicate
- SIMD-accelerated cosine / euclidean / dot product

### MVCC (Phase 13, tasks `00080`–`00084`)
- Per-tuple `xmin` / `xmax` columns on Node / Edge
- Transaction snapshot ID → visibility check on every read
- GC thread compacts dead versions
- Touches every read and write path — the highest-risk refactor in the roadmap

### Query Optimizer (Phase 14, tasks `00085`–`00089`)
- Statistics collected during writes: cardinality per kind, average degree
- Cost model: B+tree seeks vs full scans
- Plan caching keyed by parameterized Cypher hash
- `EXPLAIN` returns the plan tree

---

## Performance Watch List

From the current benchmarks (see README's `Current Status` section):

- `search_fts` on broad single-word queries: ~800ms — exceeds the 50ms target. Bottleneck: `scan_prefix` on large posting lists. Optimization opportunities: cached posting-list lengths, batch scan, inverted-index compaction.
- `shortest_path` on dense graphs: ~1s — Dijkstra explores the full reachable set. Use bidirectional BFS for unweighted shortest paths once Phase 14 lands.
- `subgraph depth 3`: ~60ms — edge collection across discovered nodes is the cost. Lazy edge iteration (Phase 14 supernode handling) will help.
- `RedbBackend` bulk insert: per-operation transactions are unusable at scale. Always batch into one write transaction per logical operation.

---

## Where Things Live

```
src/
  lib.rs           module exports
  db.rs            Drevo struct (facade)
  model.rs         Node, Edge, NewNode, NodePatch, Value
  error.rs         DrevoError enum
  storage/         StorageBackend trait + MemoryBackend + RedbBackend
  fts/             trigram tokenizer + index
  traversal.rs     BFS, DFS, Dijkstra, subgraph
  api.rs           HTTP API (axum) — feature-gated `http`
  ffi.rs           C FFI — not compiled on WASM
  wasm.rs          wasm-bindgen wrapper — feature-gated `wasm`
  bin/             server binaries (drevo-server)
tests/
  storage_tests.rs       backend parity (Memory + Redb)
  node_crud_tests.rs     node CRUD
  edge_crud_tests.rs     edge CRUD
  cascade_delete_tests.rs cascading deletion
  fts_*_tests.rs         FTS tokenizer, index, recall, search
  *_traversal*_tests.rs  BFS, DFS, shortest path, subgraph, edge cases
  smoke_tests.rs         full-stack smoke per platform
  scenario_*.rs          CBT, story, task, ERP, bug tracker E2E
benches/
  storage_bench.rs       put/get/scan_prefix
  graph_bench.rs         insert/read
  fts_bench.rs           search/index
  traversal_bench.rs     BFS/DFS/Dijkstra/subgraph
```
