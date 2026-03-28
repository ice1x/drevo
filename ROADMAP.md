# GrapeVine — Roadmap

Tasks are numbered in `XXXX` format. Statuses: `[ ]` — not started, `[~]` — in progress, `[x]` — done.

---

## Phase 1: Foundation — Storage Engine

> Goal: storage abstraction that allows swapping backends without touching upper layers.

- `0001` [x] Define `StorageBackend` trait (get, put, delete, scan_prefix, flush)
- `0002` [x] Define error types (`StorageError`) via `thiserror`
- `0003` [ ] Implement `MemoryBackend` backed by `BTreeMap<Vec<u8>, Vec<u8>>`
- `0004` [ ] Add persist/load to `MemoryBackend` — serialize entire BTreeMap to disk on flush
- `0005` [ ] Implement `RedbBackend` — wrapper over the `redb` crate
- `0006` [ ] Write integration tests: same test suite runs against both backends
- `0007` [ ] Benchmark: put/get/scan_prefix on 100K entries for both backends (criterion)

**Definition of done:** `cargo test` passes on both backends, benchmark is reproducible.

---

## Phase 2: Graph Store — CRUD

> Goal: store nodes and edges on top of the KV store, efficiently retrieve neighbors.

- `0008` [ ] Define types: `NodeId`, `Node`, `Edge`, `Value` (enum: String, Int, Float, Bool, VecF32)
- `0009` [ ] Define key schema: `n:{id}` for nodes, `e:{src}:{type}:{dst}` for edges, `r:{dst}:{type}:{src}` for reverse
- `0010` [ ] Implement `GraphStore` — CRUD for nodes (insert, get, update, delete)
- `0011` [ ] Implement CRUD for edges with automatic reverse key creation
- `0012` [ ] Implement `outgoing_edges(node_id)` and `incoming_edges(node_id)` via scan_prefix
- `0013` [ ] Implement `neighbors(node_id, edge_type: Option)` — return IDs of adjacent nodes
- `0014` [ ] Write tests: CRUD, cascading edge deletion on node removal, edge type filtering
- `0015` [ ] Benchmark: insert 100K nodes + 500K edges, read all neighbors of a random node

**Definition of done:** graph operations work, tests pass, reverse edges are consistent.

---

## Phase 3: Graph Traversal

> Goal: graph traversal — BFS, DFS, shortest path.

- `0016` [ ] Implement BFS with depth limit and optional edge type filter
- `0017` [ ] Implement DFS with depth limit
- `0018` [ ] Implement shortest_path (unweighted BFS between two nodes)
- `0019` [ ] Implement `subgraph(node_id, depth)` — return all nodes and edges within radius
- `0020` [ ] Tests: cycles, disconnected graphs, empty graph, single node, depth 0
- `0021` [ ] Benchmark: BFS on a 100K-node graph with average degree 10, depth 3

**Definition of done:** traversals are correct on all edge cases, performance is measured.

---

## Phase 4: Vector Engine — HNSW

> Goal: HNSW index for approximate nearest neighbor search.

- `0022` [ ] Implement distance functions: cosine_similarity, euclidean_distance, dot_product
- `0023` [ ] Implement `DistanceMetric` trait for pluggable metrics
- `0024` [ ] Implement `HnswIndex` struct: levels, parameters M, ef_construction
- `0025` [ ] Implement `insert` — level selection, neighbor search at each level, edge creation
- `0026` [ ] Implement `search` — greedy descent from top level to bottom, return top-K
- `0027` [ ] Implement `delete` — lazy deletion (marking) with periodic rebuild
- `0028` [ ] Tests: recall on synthetic data (random vectors, verify brute-force vs HNSW)
- `0029` [ ] Tests: edge cases — single vector, duplicates, zero vector, high dimensionality
- `0030` [ ] Benchmark: insert 100K vectors dim=128, search top-10, measure recall@10 and latency
- `0031` [ ] Implement HNSW index serialization/deserialization (bincode)

**Definition of done:** recall@10 >= 95% at ef_search=64, search latency < 1ms on 100K, index is persistent.

---

## Phase 5: Integration — Graph × Vector

> Goal: unify graph and vector into a single engine.

- `0032` [ ] Define `GraphVectorStore` — unified facade over GraphStore and HnswIndex
- `0033` [ ] On node insertion with embedding — automatically add vector to HNSW index
- `0034` [ ] On node deletion — remove vector from HNSW index
- `0035` [ ] Implement `similar_nodes(vector, k)` — ANN search, return nodes with properties
- `0036` [ ] Implement `similar_neighbors(node_id, depth, vector, k)` — graph traversal + embedding filter
- `0037` [ ] Implement `subgraph_similar(node_id, depth, k)` — take node's embedding, find similar within its subgraph
- `0038` [ ] Tests: combined queries, graph-vector consistency on insert/delete
- `0039` [ ] Benchmark: similar_neighbors on a 50K-node graph with embedding dim=128

**Definition of done:** combined queries work, node deletion leaves no "ghosts" in HNSW.

---

## Phase 6: Query Engine + CLI

> Goal: query parser and interactive REPL.

- `0040` [ ] Define `Query` enum with all query types
- `0041` [ ] Define `QueryResult` enum (nodes, edges, scores, paths)
- `0042` [ ] Implement text command parser → Query (hand-rolled parser or nom)
- `0043` [ ] Implement `QueryExecutor` — execute Query via GraphVectorStore
- `0044` [ ] Implement REPL (read-eval-print loop) with command history (rustyline)
- `0045` [ ] Add commands: HELP, STATUS (node/edge/vector counts), DUMP, LOAD
- `0046` [ ] Pretty-print results: tables for nodes, ASCII graph for paths
- `0047` [ ] Tests: parser on valid and invalid queries

**Definition of done:** the database is fully operable through CLI — insert, search, traverse.

---

## Phase 6.5: HTTP API + Docker

> Goal: expose GrapeVine over HTTP for programmatic access (Python client, etc.) and package as a Docker image.

### HTTP API (axum)

- `0063` [ ] Add `axum` + `tokio` dependencies, create `src/api/http.rs` module
- `0064` [ ] Implement node CRUD endpoints: `POST /nodes`, `GET /nodes/{id}`, `PATCH /nodes/{id}`, `DELETE /nodes/{id}`
- `0065` [ ] Implement edge endpoints: `POST /edges`, `GET /nodes/{id}/edges`, `DELETE /edges/{src}/{type}/{dst}`
- `0066` [ ] Implement graph traversal endpoints: `GET /nodes/{id}/neighbors`, `GET /paths/shortest`, `GET /nodes/{id}/subgraph`
- `0067` [ ] Implement vector search endpoints: `POST /search/similar`, `POST /search/similar_neighbors`, `POST /search/subgraph_similar`
- `0068` [ ] Implement admin endpoints: `GET /health`, `GET /status`
- `0069` [ ] JSON error handling — unified error responses with status codes
- `0070` [ ] Integration tests: HTTP endpoints against in-memory backend
- `0071` [ ] Benchmark: HTTP throughput (insert + search) via `criterion` or `wrk`

### Docker

- `0072` [ ] Create `Dockerfile` — multi-stage build (rust:slim → debian:bookworm-slim)
- `0073` [ ] Create `.dockerignore` — exclude target/, .git/, docs/
- `0074` [ ] Create `docker-compose.yml` — service with volume mount and port mapping
- `0075` [ ] Test: build image, run container, verify HTTP endpoints respond
- `0076` [ ] Document Docker usage in README.md

**Definition of done:** `docker compose up` starts GrapeVine with HTTP API accessible on port 8080; all endpoints match `PYTHON_CLIENT_SPEC.md` contract.

---

## Phase 7: Hardening

> Goal: reliability, documentation, preparation for extension.

- `0048` [ ] Add WAL (write-ahead log) for crash recovery
- `0049` [ ] Add RwLock / concurrent access to GraphVectorStore
- `0050` [ ] Add property-based tests (proptest) for graph invariants
- `0051` [ ] Add fuzz tests for the query parser
- `0052` [ ] Write rustdoc for all public APIs
- `0053` [~] CI: GitHub Actions (test, clippy, fmt — done; benchmark comparison — pending benchmarks)
- `0054` [ ] Write ARCHITECTURE.md with diagrams and rationale

**Definition of done:** CI is green, documentation is complete, crash recovery works.

---

## Phase 8: Future — Production Path (post-MVP)

> Not implemented as part of the learning project, but architecture must support it.

- `0055` [ ] gRPC API (tonic) — alternative to HTTP for high-throughput use
- `0056` [ ] SIMD-accelerated distance functions (std::simd or packed_simd)
- `0057` [ ] Product Quantization for vector compression
- `0058` [ ] Graph sharding by partition key
- `0059` [ ] Transactions with MVCC
- `0060` [ ] Query planner with cost-based optimization
- `0061` [ ] Cypher or GraphQL support
- `0062` [ ] Observability: metrics, tracing (OpenTelemetry)

---

## Phase Dependencies

```
Phase 1 (Storage)
    ↓
Phase 2 (Graph CRUD)
    ↓
Phase 3 (Traversal)  ←──── Phase 4 (HNSW) — in parallel
    ↓                           ↓
    └──────── Phase 5 (Integration) ─────┘
                    ↓
              Phase 6 (CLI)
                    ↓
              Phase 6.5 (HTTP API + Docker)  ← Python client spec drives the API contract
                    ↓
              Phase 7 (Hardening)
                    ↓
              Phase 8 (Future)
```

Phase 3 and Phase 4 can be developed in parallel — they depend only on Phase 1/2 and not on each other.

Phase 6.5 depends on Phase 5 (Integration) — HTTP API wraps the same `GraphVectorStore` that CLI uses.

## Cross-Repository: Python Client

> The Python client (`grapevine-py`) is developed in a **separate repository**.
> Its specification lives in this repo: `PYTHON_CLIENT_SPEC.md`.
> The spec serves as the **contract** between the Rust server and the Python client.
> It MUST be reviewed and updated after every task/subtask that affects the public API.
