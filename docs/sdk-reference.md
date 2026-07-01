# SDK Reference

How to call drevo from code and over the wire. Signatures are grounded in the source; the
authoritative definitions live in [`src/db.rs`](../src/db.rs), [`src/model.rs`](../src/model.rs),
and the Python type stubs under [`drevo-py/`](../drevo-py).

- [Rust API](#rust-api)
- [Python SDK](#python-sdk)
- [Graph-RAG helpers](#graph-rag-helpers)
- [HTTP API](#http-api)
- [Bolt protocol](#bolt-protocol)
- [MCP tools](#mcp-tools)
- [Cargo features](#cargo-features)

---

## Rust API

The whole surface hangs off the [`Drevo`](../src/db.rs) handle. All fallible methods return
`Result<T, DrevoError>`.

### Lifecycle

```rust
Drevo::open(path: &Path) -> Result<Self>          // disk-backed (needs `redb-backend`)
Drevo::open_in_memory() -> Result<Self>           // ephemeral
Drevo::recover(path: &Path) -> Result<(Self, IntegrityReport)>
fn close(self) -> Result<()>
fn compact(&mut self) -> Result<CompactReport>
fn health_check(&self) -> Result<()>
```

### Node & edge CRUD

```rust
fn create_node(&self, new_node: NewNode) -> Result<Node>
fn get_node(&self, id: u64) -> Result<Option<Node>>
fn get_node_by_uuid(&self, uuid: &[u8; 16]) -> Result<Option<Node>>
fn get_node_by_title(&self, title: &str) -> Result<Option<Node>>
fn update_node(&self, id: u64, patch: NodePatch) -> Result<Node>
fn delete_node(&self, id: u64) -> Result<()>

fn create_edge(&self, new_edge: NewEdge) -> Result<Edge>
fn get_edge(&self, id: u64) -> Result<Option<Edge>>
fn update_edge(&self, id: u64, patch: EdgePatch) -> Result<Edge>
fn delete_edge(&self, id: u64) -> Result<()>
fn edges_of(&self, node_id: u64, direction: Direction) -> Result<Vec<Edge>>
```

### Listing & property lookup

```rust
fn list_nodes_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Node>>
fn list_edges_by_kind(&self, kind: &str, limit: usize, offset: usize) -> Result<Vec<Edge>>
fn nodes_by_property(&self, key: &str, value: &serde_json::Value) -> Result<Vec<Node>>
fn count_nodes_by_property(&self, key: &str, value: &serde_json::Value) -> Result<usize>
fn list_recent(&self, limit: usize) -> Result<Vec<Node>>
```

### Traversal

```rust
fn bfs(&self, start: u64, max_depth: u8, direction: Direction, edge_kind: Option<&str>) -> Result<Vec<Node>>
fn dfs(&self, start: u64, max_depth: u8, direction: Direction, edge_kind: Option<&str>) -> Result<Vec<Node>>
fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>>
fn shortest_path_filtered(&self, from: u64, to: u64, edge_kind: Option<&str>) -> Result<Option<Vec<u64>>>
fn subgraph(&self, root: u64, depth: u8) -> Result<SubGraph>
fn subgraph_filtered(&self, root: u64, depth: u8, edge_kind: Option<&str>) -> Result<SubGraph>
fn neighbors(&self, node_id: u64, direction: Direction, kind: Option<&str>) -> Result<Vec<Node>>
```

`Direction` is `Outgoing | Incoming | Both`. `shortest_path` is weighted (Dijkstra over
`Edge::weight`).

### Full-text search

```rust
fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ScoredNode>>        // Okapi BM25
fn search_fts_ranked(&self, query: &str, limit: usize, ranking: FtsRanking) -> Result<Vec<ScoredNode>>
fn facets(&self, kind: &str, property: &str, k: usize, collapse: &FacetCollapse) -> Result<Vec<Facet>>
```

`FtsRanking` is `Bm25 { k1, b }` (default `k1 = 1.2`, `b = 0.75`) or `TfIdf` (legacy).

### Vector search

```rust
fn set_embedding(&self, node_id: u64, embedding: Vector) -> Result<()>
fn set_embeddings_batch(&self, embeddings: &[(u64, Vector)]) -> Result<()>
fn get_embedding(&self, node_id: u64) -> Result<Option<Vector>>
fn delete_embedding(&self, node_id: u64) -> Result<()>
fn embedding_count(&self) -> Result<usize>
fn build_vector_index(&self, config: HnswConfig) -> Result<HnswIndex>
```

Distances (cosine, Euclidean, dot product) live in [`src/vector/`](../src/vector); the HNSW
index gives approximate nearest-neighbour search.

### Graph analytics

```rust
fn pagerank(&self, config: &PageRankConfig) -> Result<PageRankResult>
fn louvain_communities(&self, config: &LouvainConfig) -> Result<LouvainResult>
```

Both run over an in-memory adjacency snapshot of the whole graph and are deterministic. See
[`src/algorithms/`](../src/algorithms).

### Transactions

```rust
fn tx_begin(&self) -> Result<()>
fn tx_commit(&self) -> Result<()>
fn tx_rollback(&self) -> Result<()>   // replays inverse ops
fn is_tx_active(&self) -> bool
```

### Core model types

`Node { id, uuid, kind, title, body, body_html, created_at, updated_at, properties }`,
`Edge { id, uuid, from_id, to_id, kind, weight, created_at, properties }`, and their creation
(`NewNode`, `NewEdge`) and patch (`NodePatch`, `EdgePatch`) companions are defined in
[`src/model.rs`](../src/model.rs). `properties` is a `HashMap<String, serde_json::Value>`.

---

## Python SDK

The Python package wraps the Rust core via PyO3 (releasing the GIL around storage work). The
authoritative type stubs are [`drevo-py/python/drevo/__init__.pyi`](../drevo-py/python/drevo/__init__.pyi).

```python
import drevo

# Open (context manager closes for you)
with drevo.Drevo.open("graph.redb") as db:        # or Drevo.open_in_memory()
    n = db.create_node(drevo.NewNode(kind="Task", title="Ship docs",
                                     properties={"priority": 3}))
    got = db.get_node(n.id)
    db.update_node(n.id, drevo.NodePatch(properties={"status": "done"}))

    # Traversal — Direction is an IntEnum: OUT / IN / BOTH
    neighbours = db.bfs(n.id, max_depth=2, direction=drevo.Direction.OUT)

    # Search
    hits = db.search_fts("docs", limit=10)         # -> list[ScoredNode]

    # Vector
    db.set_embedding(n.id, [0.1, 0.2, 0.3])
    matches = db.vector_search([0.1, 0.2, 0.3], k=5)  # -> list[(node_id, distance)]
```

Classes mirror the Rust model: `Node`, `Edge`, `NewNode`, `NewEdge`, `NodePatch`, `EdgePatch`,
`Direction`, `ScoredNode`, `SubGraph`, `CompactReport`. The exception hierarchy is rooted at
`DrevoError`, with `NodeNotFoundError`, `EdgeNotFoundError`, `DuplicateTitleError`,
`StorageError`, and friends.

### Graph-RAG helpers

The pure-Python [`drevo.rag`](../drevo-py/python/drevo/rag) module packages the
search → expand → format pipeline for retrieval-augmented generation:

```python
from drevo.rag import Retriever, embed_and_store, vector_search

# Store embeddings for a set of nodes using your embedding function
embed_and_store(db, nodes, embedder=my_embedder)

# Semantic search returning rich hits (node + distance + similarity)
hits = vector_search(db, "anxious thoughts about work", embedder=my_embedder, k=10)

# Retrieve a seed node's neighbourhood as LLM-ready context
retriever = Retriever(db, hops=2)
context = retriever.retrieve("Ship docs", limit=10)
prompt_text = context.to_text(format="markdown")
```

Other building blocks: `ingest_documents` (load LangChain-style `Document`s into the graph),
`expand_neighborhood`, `Embedder` / `VectorHit`, and `MMRReranker` for diversity-aware reranking.

---

## HTTP API

Enabled by the `http` feature (default). The [`drevo-server`](../src/bin/server.rs) binary
listens on `${DREVO_HOST}:${DREVO_PORT}` (default `0.0.0.0:8080`). All bodies are JSON.

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/` | Server metadata (name, version). |
| `GET` | `/health` | Liveness (503 while shutting down). |
| `GET` | `/ready` | Readiness — exercises the backend. |
| `GET` | `/status` | Uptime and process info. |
| `GET` | `/metrics` | Prometheus exposition format. |
| `POST` | `/nodes` | Create a node. |
| `GET` | `/nodes?kind=&limit=&offset=` | List nodes by kind. |
| `GET` | `/nodes/{id}` | Fetch a node. |
| `PATCH` | `/nodes/{id}` | Partial update. |
| `DELETE` | `/nodes/{id}` | Delete. |
| `GET` | `/nodes/{id}/edges?direction=` | Incident edges. |
| `GET` | `/nodes/{id}/neighbors?direction=&kind=&depth=` | BFS neighbours. |
| `GET` | `/nodes/{id}/subgraph?depth=` | Bounded subgraph. |
| `POST` | `/edges` | Create an edge. |
| `GET` | `/edges?kind=&limit=&offset=` | List edges by kind. |
| `GET`/`PATCH`/`DELETE` | `/edges/{id}` | Fetch / update / delete. |
| `GET` | `/paths/shortest?from=&to=` | Dijkstra shortest path. |
| `POST` | `/search/fts` | Full-text search (`{query, limit?}`). |
| `GET` | `/facets?kind=&property=&k=&collapse=` | Faceted aggregation. |
| `GET` | `/export/json` / `POST /import/json` | Full-graph dump / load. |
| `GET` | `/export/graphml` | GraphML 1.0 export. |
| `GET` | `/ui` | Embedded Cytoscape.js graph explorer. |

---

## Bolt protocol

drevo speaks the **Bolt** wire protocol, so official Neo4j drivers (Python, JavaScript, Go,
Java) connect out of the box. The TCP listener is enabled by the `http` feature; the standard
port is `7687`. The session understands `HELLO`, `RUN`, `PULL`, `DISCARD`, `RESET`, and
`GOODBYE`, replying with `SUCCESS` / `RECORD` / `FAILURE` / `IGNORED`.

```python
from neo4j import GraphDatabase

driver = GraphDatabase.driver("bolt://localhost:7687", auth=("neo4j", "password"))
with driver.session() as session:
    for record in session.run("MATCH (n:Task) RETURN n.title AS title LIMIT 10"):
        print(record["title"])
```

The Cypher accepted over Bolt is exactly the [Cypher Reference](cypher-reference.md) subset.
TLS is available behind the `bolt-tls` feature, authentication behind `bolt-auth`
(see the [Admin Guide](admin-guide.md#authentication)).

---

## MCP tools

The Model Context Protocol server for AI agents (Claude Code, Cline, Claude
Desktop) is maintained in a separate repository —
[github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp) — and connects
to a running `drevo-server` over HTTP / the Neo4j-compatible Bolt port rather
than opening the redb file, so it never contends for redb's single-process lock.
See that repo's README for the tool list and client setup.

---

## Cargo features

| Feature | Default | Enables |
|---------|---------|---------|
| `redb-backend` | ✅ | Disk-backed redb storage; `Drevo::open` / `recover`. |
| `http` | ✅ | Axum HTTP API server **and** the Bolt TCP listener. |
| `bolt-auth` | — | Argon2id user store + session tokens (transport-agnostic auth). |
| `bolt-tls` | — | rustls TLS for Bolt; implies `http`. |
| `wasm` | — | WebAssembly build (excludes `bolt`, `ffi`). |
| `cbindgen` | ✅ | Generate the C header (`drevo.h`) from the FFI surface. |

The in-memory backend (`Drevo::open_in_memory`) needs no features and is always available,
which is why the test suites and the WASM target use it.
