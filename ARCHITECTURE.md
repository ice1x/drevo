# GrapeVine — Architecture Context

> This file is meant to be passed as context to an AI assistant when working on the project.
> Update it when architectural decisions change.

## What This Is

GrapeVine is an embedded graph+vector database in Rust. A single store for graph data (nodes, edges, traversal) and vector search (HNSW ANN). The key feature is combined queries: "find semantically similar nodes among graph neighbors."

## Layers

```
Query API → Query Engine → Graph Engine + Vector Engine → Storage Engine → Backend
```

Each layer communicates with the one below **only through traits**. Concrete implementations are injected at initialization.

## Storage Engine

### Trait

```rust
pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&self, key: &[u8]) -> Result<()>;
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    fn flush(&self) -> Result<()>;
}
```

### Backends

- `MemoryBackend` — `BTreeMap<Vec<u8>, Vec<u8>>`, optional persist via bincode
- `RedbBackend` — wrapper over the `redb` crate, ACID, B-tree based

### Why `scan_prefix`

This is the key operation for the graph. All outgoing edges of node 42 have keys `e:42:*`, so `scan_prefix(b"e:42:")` returns all its edges in a single pass. Both BTreeMap and redb support range scans efficiently.

## Graph Store

### Key Schema in KV

```
n:{node_id}                      → bincode(Node)
e:{src_id}:{edge_type}:{dst_id}  → bincode(EdgeProperties)
r:{dst_id}:{edge_type}:{src_id}  → bincode(EdgeProperties)
```

- `n:` — nodes
- `e:` — outgoing edges (forward)
- `r:` — incoming edges (reverse), data duplication for efficient reverse traversal

### Types

```rust
type NodeId = u64;

struct Node {
    id: NodeId,
    labels: Vec<String>,
    properties: HashMap<String, Value>,
    embedding: Option<Vec<f32>>,
}

struct Edge {
    src: NodeId,
    dst: NodeId,
    edge_type: String,
    properties: HashMap<String, Value>,
}

enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    VecF32(Vec<f32>),
}
```

### Invariants

- On node deletion — delete all its edges (forward and reverse)
- On edge insertion — verify both nodes exist
- Forward and reverse edge keys are always created/deleted as a pair

## Vector Engine (HNSW)

### Parameters

- `M` — maximum number of neighbors per level (default: 16)
- `ef_construction` — candidate pool size during construction (default: 200)
- `ef_search` — candidate pool size during search (default: 64)
- `max_level` — computed as `floor(ln(N) * (1/ln(M)))`

### Structure

```rust
struct HnswIndex {
    nodes: Vec<HnswNode>,           // all nodes, indexed by position
    entry_point: Option<usize>,     // graph entry (node at top level)
    max_level: usize,
    m: usize,
    ef_construction: usize,
    distance_fn: Box<dyn DistanceMetric>,
}

struct HnswNode {
    id: NodeId,                     // reference to the graph node
    vector: Vec<f32>,
    neighbors: Vec<Vec<usize>>,     // neighbors[level] = [indices...]
    max_level: usize,               // level assigned to this node
}
```

### Distance

```rust
trait DistanceMetric: Send + Sync {
    fn distance(&self, a: &[f32], b: &[f32]) -> f32;
}
```

Implementations: CosineDistance (1 - cosine_similarity), EuclideanDistance, DotProduct (negated for min-heap).

### Persistence

The HNSW index is serialized in its entirety via bincode and stored in KV under the key `_meta:hnsw_index`. On startup — deserialized into memory. On flush — serialized back.

## Combined Queries

### `similar_neighbors(node_id, depth, vector, k)`

Algorithm:
1. BFS from node_id to the given depth → collect node set
2. From those nodes, keep only the ones that have an embedding
3. Among their embeddings, find top-k closest to vector (brute-force over the subset, not HNSW)

Why brute-force at step 3: the subset is typically small (hundreds to thousands), HNSW is unnecessary.

### `similar_nodes(vector, k)`

Direct HNSW search → map results to Node via GraphStore.

## Error Handling

Unified error type:

```rust
enum GrapeVineError {
    Storage(StorageError),
    NodeNotFound(NodeId),
    EdgeNotFound { src: NodeId, dst: NodeId },
    InvalidEmbedding(String),
    QueryParse(String),
    Serialization(String),
}
```

## Principles

- **Trait-first**: every layer behind a trait
- **No unsafe**: until SIMD is needed
- **Fail explicitly**: Result<T, GrapeVineError> everywhere, no unwrap in lib code
- **Test at boundaries**: integration tests at the trait level, not on specific implementations
