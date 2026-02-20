# GrapeVine — Graph + Vector Embedded Database in Rust

> A learning project: an embedded database combining a graph store with vector search (HNSW).

## Why

Existing solutions are split: Neo4j for graphs, Qdrant/Milvus for vectors. Combined queries ("find semantically similar nodes among graph neighbors at depth N") require gluing two systems together. GrapeVine is a single store where graph and vectors coexist.

## Architecture

```
┌─────────────────────────────┐
│  HTTP API (axum) / CLI REPL │
├─────────────────────────────┤
│   Query Engine              │
├─────────────────────────────┤
│   Graph Engine   │  Vector  │
│   (traversal,    │  Engine  │
│    pathfinding)  │  (HNSW)  │
├──────────────────┴──────────┤
│   Storage Engine (trait)    │
├─────────────────────────────┤
│   Backend: memory / redb    │
└─────────────────────────────┘
```

Each layer is isolated behind a trait and can be replaced independently.

## Key Features (MVP)

- **Graph store** — nodes with labels and properties, typed edges, prefix scan for neighbors
- **Vector search** — HNSW index, cosine similarity, approximate nearest neighbor
- **Combined queries** — embedding search scoped to graph neighbors
- **Pluggable storage** — `StorageBackend` trait with in-memory and redb implementations
- **CLI interface** — REPL for interactive use
- **HTTP API** — JSON REST API (axum) for programmatic access
- **Docker** — multi-stage build, single container deployment

## Quick Start

### Native

```bash
cargo build --release
cargo run -- --storage memory    # in-memory mode
cargo run -- --storage redb      # persistent mode (data.redb file)
```

### Docker

```bash
docker compose up -d
# GrapeVine HTTP API available at http://localhost:8080
```

### CLI (REPL)

```
grapevine> INSERT NODE 1 labels=["server"] props={"name": "web-01"} embedding=[0.1, 0.2, 0.3]
grapevine> INSERT NODE 2 labels=["server"] props={"name": "db-01"} embedding=[0.4, 0.5, 0.6]
grapevine> INSERT EDGE 1 DEPENDS_ON 2
grapevine> NEIGHBORS 1 DEPTH 2
grapevine> SIMILAR [0.11, 0.19, 0.31] LIMIT 5
grapevine> SIMILAR_NEIGHBORS 1 DEPTH 2 VECTOR [0.11, 0.19, 0.31] LIMIT 3
```

### HTTP API

```bash
# Insert node
curl -X POST http://localhost:8080/nodes \
  -H "Content-Type: application/json" \
  -d '{"id": 1, "labels": ["server"], "properties": {"name": "web-01"}, "embedding": [0.1, 0.2, 0.3]}'

# Vector search
curl -X POST http://localhost:8080/search/similar \
  -H "Content-Type: application/json" \
  -d '{"vector": [0.11, 0.19, 0.31], "limit": 5}'
```

## Project Structure

```
grapevine/
├── Cargo.toml
├── Dockerfile                   # Multi-stage build
├── docker-compose.yml           # Local dev setup
├── .dockerignore
├── PYTHON_CLIENT_SPEC.md        # Python client contract specification
├── src/
│   ├── main.rs                 # CLI + HTTP server entrypoint
│   ├── lib.rs                  # public API
│   ├── storage/
│   │   ├── mod.rs              # StorageBackend trait
│   │   ├── memory.rs           # In-memory + optional persist
│   │   └── redb_backend.rs     # redb implementation
│   ├── graph/
│   │   ├── mod.rs              # Graph engine public API
│   │   ├── types.rs            # Node, Edge, NodeId, Value
│   │   ├── store.rs            # Graph CRUD over StorageBackend
│   │   └── traversal.rs        # BFS, DFS, shortest path
│   ├── vector/
│   │   ├── mod.rs              # Vector engine public API
│   │   ├── hnsw.rs             # HNSW index implementation
│   │   └── distance.rs         # Cosine similarity, L2, dot product
│   ├── query/
│   │   ├── mod.rs              # Query engine
│   │   ├── parser.rs           # CLI query parser
│   │   └── executor.rs         # Query execution, combined queries
│   └── api/
│       ├── cli.rs              # REPL interface
│       └── http.rs             # HTTP REST API (axum)
├── tests/
│   ├── graph_tests.rs
│   ├── vector_tests.rs
│   ├── http_tests.rs           # HTTP API integration tests
│   └── integration_tests.rs
├── benches/
│   └── benchmarks.rs           # criterion benchmarks
├── docs/
│   ├── ROADMAP.md
│   └── context/                # Context files for AI sessions
│       ├── ARCHITECTURE.md
│       ├── CONVENTIONS.md
│       └── CURRENT_STATUS.md
└── README.md
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `serde` + `bincode` | Struct serialization to bytes |
| `redb` | Embedded KV store (persistent backend) |
| `ordered-float` | `f32` in sorted collections and BTreeMap |
| `rand` | HNSW level selection |
| `thiserror` | Typed errors |
| `criterion` | Benchmarks |
| `clap` | CLI argument parsing |
| `axum` + `tokio` | HTTP API server |
| `serde_json` | JSON serialization for HTTP |

## Python Client

The Python client is developed in a **separate repository** (`grapevine-py`). It communicates with GrapeVine via the HTTP REST API.

The API contract is defined in [`PYTHON_CLIENT_SPEC.md`](PYTHON_CLIENT_SPEC.md) in this repository.

```python
from grapevine import GrapeVineClient

db = GrapeVineClient("http://localhost:8080")
db.insert_node(1, labels=["server"], props={"name": "web-01"}, embedding=[0.1, 0.2, 0.3])
similar = db.similar([0.11, 0.19, 0.31], limit=5)
```

## License

MIT
