---
name: drevo-architecture
description: Software architecture patterns, SOLID principles, anti-patterns, algorithm design — for Rust graph database development
---

# drevo — Architecture & Design Patterns

## When to Use
Always. This skill defines the architectural philosophy for all drevo code.

---

## SOLID Principles (Adapted for Rust)

### S — Single Responsibility
- Each module/file has ONE reason to change
- `cypher/lexer.rs` → only tokenization
- `cypher/parser.rs` → only AST construction
- `executor/mod.rs` → only query execution (not parsing, not storage)
- **Violation signal**: "I need to change this file for an unrelated reason"

### O — Open/Closed (via Traits)
- Open for extension, closed for modification
- Use traits to define behavior boundaries:
  ```rust
  trait StorageBackend {
      fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
      fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;
  }
  ```
- New backends (in-memory, redb, future RocksDB) implement the trait without changing existing code
- **In drevo**: `Value` enum is closed (we control variants), but operations on it are extensible via match arms

### L — Liskov Substitution
- Any implementation of a trait must be substitutable
- If `RedbBackend: StorageBackend` works, swapping to `MemoryBackend: StorageBackend` must also work
- Contracts defined by trait docs, not just type signatures

### I — Interface Segregation
- Small, focused traits over fat interfaces
- Don't force implementors to provide methods they don't need:
  ```rust
  // GOOD: separate concerns
  trait NodeStorage { fn get_node(&self, id: u64) -> Result<Node>; }
  trait EdgeStorage { fn get_edge(&self, id: u64) -> Result<Edge>; }

  // BAD: god interface
  trait Storage { fn get_node(); fn get_edge(); fn create_index(); fn run_query(); }
  ```

### D — Dependency Inversion
- High-level modules (executor) should NOT depend on low-level modules (redb)
- Both depend on abstractions (traits)
- **In drevo**: `Drevo` depends on `StorageBackend` (abstraction), not on `redb::Database` directly

---

## Design Patterns Used

### Builder Pattern
- For complex object construction (query plans, node builders)
```rust
NewNode::builder()
    .kind("note")
    .title("Hello")
    .property("tag", "intro")
    .build()
```

### Strategy Pattern (via Trait Objects or Enums)
- Different traversal strategies (BFS, DFS, bidirectional)
- Different distance functions (cosine, euclidean, dot product)
```rust
enum DistanceMetric { Cosine, Euclidean, DotProduct }
fn similarity(a: &[f32], b: &[f32], metric: DistanceMetric) -> f32
```

### Visitor Pattern (AST Walking)
- Cypher AST traversal for optimization passes
- Query plan rewriting (push predicates down, eliminate redundant scans)

### Command Pattern (Undo Log)
- Transaction rollback: each mutation records its inverse
```rust
enum UndoOp {
    DeleteNode(u64, Node),                    // inverse of CreateNode
    CreateNode(u64),                          // inverse of DeleteNode
    SetProperty(u64, String, Option<Value>),  // restore old value
}
```

### Iterator Pattern
- Query results are lazy iterators (don't materialize all rows)
- Graph traversal yields nodes one at a time (memory-efficient BFS/DFS)

### Facade Pattern
- `Drevo` is a facade over: Storage + Indexes + Adjacency + FTS
- HTTP API in `api.rs` is a facade over `Drevo`

---

## Anti-Patterns to AVOID

### 1. God Object
- ❌ One struct with 50+ methods that does everything
- ✅ Split into focused modules: `Drevo` delegates to `Storage`, `Index`, `Adjacency`, `Fts`

### 2. Premature Abstraction
- ❌ Creating trait hierarchies before the second implementation exists
- ✅ Start concrete. Extract a trait only when you ACTUALLY need a second impl
- "Three strikes and you refactor" rule

### 3. Stringly Typed
- ❌ `fn query(kind: &str, filter: &str) -> String`
- ✅ `fn query(kind: NodeKind, filter: Filter) -> QueryResult`
- Use enums, newtypes, and typed IDs

### 4. Clone Everything
- ❌ `.clone()` to avoid lifetime issues
- ✅ Restructure ownership. Use `&str`, `Cow`, or `Arc` where shared
- Clone is acceptable for: small values (u64, bool), rare paths (error messages)

### 5. Unwrap in Library Code
- ❌ `map.get("key").unwrap()` — panics on missing key
- ✅ `map.get("key").ok_or(DrevoError::MissingKey("key".into()))?`
- `.unwrap()` is allowed ONLY in tests and benchmarks

### 6. Deep Nesting
- ❌ 5+ levels of `if/match/for` nesting
- ✅ Early returns, helper functions, `?` operator
- Max 3 levels of indentation in any function

### 7. Leaky Abstractions
- ❌ Exposing `redb` transaction types in the public API
- ✅ Wrap in domain types (`Transaction`, `ReadGuard`)
- Internal implementation details must not leak through public interfaces

### 8. N+1 Query Problem (Graph Version)
- ❌ For each node, make a separate storage call to get properties
- ✅ Batch reads, pre-fetch adjacency lists, denormalize hot paths
- In redb: use cursor iteration instead of random point lookups

### 9. Over-Engineering
- ❌ Abstract factory for node creation when there's only one kind of node
- ✅ Simple `Node::new(kind, properties)` until complexity demands more
- YAGNI: You Aren't Gonna Need It

### 10. Mixing Concerns in Match Arms
- ❌ Giant match with 500 lines per arm
- ✅ Each arm calls a dedicated function: `handle_create(...)`, `handle_match(...)`

---

## Algorithm Design Principles

### Time Complexity Awareness
| Operation | Acceptable | Unacceptable |
|-----------|-----------|--------------|
| Point lookup by ID | O(1) | — |
| Lookup by indexed property | O(1) | O(N) full scan |
| BFS/DFS traversal | O(V + E) for reachable | O(V × E) naive |
| Shortest path | O(V + E) BFS for unweighted | O(V³) Floyd-Warshall for sparse |
| Sort results | O(N log N) | O(N²) bubble sort |
| Set membership (kinds) | O(1) HashSet | O(N) linear scan |

### Space Complexity
- Adjacency list (current): O(V + E) — optimal for sparse graphs
- Adjacency matrix: O(V²) — only for dense graphs (NOT our use case)
- Property storage: O(P) where P = total properties across all nodes/edges

### Graph Algorithm Patterns
1. **BFS** — shortest path (unweighted), level-order traversal, connected components
2. **DFS** — cycle detection, topological sort, path existence
3. **Bidirectional BFS** — shortest path optimization (meet in the middle)
4. **Priority Queue (Dijkstra)** — weighted shortest path (already implemented in `traversal.rs`)
5. **Union-Find** — connected components, cycle detection in undirected
6. **PageRank** — iterative until convergence, damping factor 0.85

### Index Design
- **B+tree (redb)**: ordered range scans, prefix queries — used for `kind_index`, `title_index`
- **HashMap**: O(1) exact match, no ordering
- **Bitmap index**: fast set operations (AND/OR) for kind filtering
- **Inverted index**: trigram FTS over titles + bodies — already implemented in `fts/`
- Rule: index anything queried in WHERE more than once per session

---

## Rust-Specific Architecture Wisdom

### Ownership as Architecture
- Ownership IS your architecture diagram
- If you can't draw a clear ownership tree → redesign
- Cycles = `Rc<RefCell<>>` = code smell → break with IDs instead of references

### Use IDs, Not References
```rust
// BAD: self-referential, lifetime nightmare
struct Node<'a> { neighbors: Vec<&'a Node<'a>> }

// GOOD: ID-based, stored separately
struct Node { id: u64, neighbor_ids: Vec<u64> }
struct Graph { nodes: HashMap<u64, Node> }
```
This is exactly what drevo does — nodes reference each other by ID via the `out_edges` / `in_edges` adjacency tables.

### Error Propagation Architecture
```
Storage Error → Database Error → Executor Error → HTTP / Bolt Error → Client
     ↓                ↓                ↓                 ↓
 StorageError      DrevoError      QueryError       HTTP 5xx
```
Each layer wraps/converts errors from the layer below.

### Zero-Cost Abstractions
- Traits with static dispatch (`impl Trait`) → monomorphized, no vtable
- Enums over trait objects for closed sets of variants
- Iterators → compiled to loops (no allocation)
- Newtypes → zero runtime cost, compile-time safety

### Module Boundaries = API Contracts
- `pub` items in a module = its API contract
- Everything else is an implementation detail
- Breaking a `pub` signature = breaking change
- Use `pub(crate)` for internal-but-shared items

---

## Database-Specific Patterns

### Write-Ahead Log (WAL)
- Log mutations BEFORE applying to main storage
- On crash: replay WAL to restore consistency
- Checkpoint: flush WAL to main storage periodically
- Tracked as Phase 9 task `00053`

### MVCC (Multi-Version Concurrency Control)
- Readers see snapshot at transaction start time
- Writers create new versions, don't block readers
- Garbage collect old versions after all readers finish
- Tracked as Phase 13 tasks `00080`–`00084`

### LSM vs B+tree Trade-offs
| | B+tree (redb) | LSM (RocksDB) |
|---|---|---|
| Read | Fast (1 seek) | Slower (multi-level) |
| Write | Moderate (in-place) | Fast (sequential append) |
| Space | Compact | Write amplification |
| **Our choice** | ✅ redb | — |

We chose B+tree (redb) because graph workloads are read-heavy (traversals).

### Connection Pooling Pattern
- Bolt connections will be stateful (authenticated, may have an open transaction)
- Pool reuses connections, resets state between uses
- Max connections = bounded concurrency

### Batch vs Single Operations
- Single: good for interactive queries
- Batch: good for migration, bulk import
- Always provide both APIs:
  ```rust
  fn create_node(&mut self, node: NewNode) -> Result<Node>;
  fn create_nodes_batch(&mut self, nodes: &[NewNode]) -> Result<Vec<Node>>;
  ```
