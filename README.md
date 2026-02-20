# GrapeVine — Graph + Vector Embedded Database in Rust

> A learning project: an embedded database combining a graph store with vector search (HNSW).

## Why

Existing solutions are split: Neo4j for graphs, Qdrant/Milvus for vectors. Combined queries ("find semantically similar nodes among graph neighbors at depth N") require gluing two systems together. GrapeVine is a single store where graph and vectors coexist.

## Architecture

```
┌─────────────────────────────┐
│   Query API (CLI / HTTP)    │
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

## Quick Start

```bash
cargo build --release
cargo run -- --storage memory    # in-memory mode
cargo run -- --storage redb      # persistent mode (data.redb file)
```

```
grapevine> INSERT NODE 1 labels=["server"] props={"name": "web-01"} embedding=[0.1, 0.2, 0.3]
grapevine> INSERT NODE 2 labels=["server"] props={"name": "db-01"} embedding=[0.4, 0.5, 0.6]
grapevine> INSERT EDGE 1 DEPENDS_ON 2
grapevine> NEIGHBORS 1 DEPTH 2
grapevine> SIMILAR [0.11, 0.19, 0.31] LIMIT 5
grapevine> SIMILAR_NEIGHBORS 1 DEPTH 2 VECTOR [0.11, 0.19, 0.31] LIMIT 3
```

## Project Structure

```
grapevine/
├── Cargo.toml
├── src/
│   ├── main.rs                 # CLI entrypoint
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
│       └── cli.rs              # REPL interface
├── tests/
│   ├── graph_tests.rs
│   ├── vector_tests.rs
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

## License

MIT
