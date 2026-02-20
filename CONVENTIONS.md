# GrapeVine — Coding Conventions

> This file is meant to be passed as context to an AI assistant.
> Follow these conventions when generating code.

## Rust Style

### General

- Edition: 2021
- MSRV: latest stable
- `cargo fmt` before every commit
- `cargo clippy -- -W clippy::all` with zero warnings
- No `unwrap()` / `expect()` in library code — `Result` only
- `unwrap()` is allowed only in tests and benchmarks
- No `unsafe` without explicit justification in a comment

### Naming

- Modules: `snake_case` (`graph_store.rs`)
- Structs and traits: `PascalCase` (`StorageBackend`, `HnswIndex`)
- Functions and methods: `snake_case` (`scan_prefix`, `insert_node`)
- Constants: `SCREAMING_SNAKE_CASE` (`DEFAULT_M`, `MAX_LEVEL`)
- Type aliases: `PascalCase` (`type NodeId = u64`)

### File Structure

```rust
// 1. Imports (std → external crates → internal modules)
use std::collections::HashMap;

use serde::{Serialize, Deserialize};

use crate::storage::StorageBackend;

// 2. Constants
const DEFAULT_M: usize = 16;

// 3. Types / Structs / Enums
pub struct HnswIndex { ... }

// 4. Trait implementations
impl HnswIndex { ... }

// 5. Tests at the bottom
#[cfg(test)]
mod tests { ... }
```

### Errors

Use `thiserror` for error definitions:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("key not found: {0:?}")]
    NotFound(Vec<u8>),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
}
```

### Serialization

- Internal data (KV store): `bincode` — compact, fast
- Configs and dumps: `serde_json` — human-readable
- All persistable structs: `#[derive(Serialize, Deserialize)]`

### Traits

- Each public trait — in its own file or in the module's `mod.rs`
- Trait must be `Send + Sync` if stored in `Arc`
- Dyn dispatch (`Box<dyn Trait>`) preferred for pluggable components
- Generics preferred for hot paths (distance functions)

## Testing

### Structure

```
tests/
├── storage_tests.rs      # StorageBackend — tests via trait
├── graph_tests.rs         # GraphStore CRUD + traversal
├── vector_tests.rs        # HNSW correctness + recall
└── integration_tests.rs   # Combined queries end-to-end
```

### Rules

- Every public method — at least 1 test
- Edge cases are mandatory: empty graph, single node, cycles, zero vector
- Storage tests are parameterized by backend:

```rust
fn test_with_backend(backend: impl StorageBackend) {
    backend.put(b"key", b"value").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
}

#[test]
fn test_memory() { test_with_backend(MemoryBackend::new()); }

#[test]
fn test_redb() { test_with_backend(RedbBackend::new_temp().unwrap()); }
```

- HNSW tests: compare recall with brute-force

```rust
fn brute_force_knn(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> { ... }

#[test]
fn test_hnsw_recall() {
    // insert 10K random vectors
    // search top-10
    // compare with brute-force
    // assert recall >= 0.95
}
```

### Benchmarks

Use `criterion`:

```rust
fn bench_hnsw_search(c: &mut Criterion) {
    // setup: build index with 100K vectors
    c.bench_function("hnsw_search_100k", |b| {
        b.iter(|| index.search(&query, 10, 64))
    });
}
```

## Git

### Commit Messages

```
feat(graph): implement BFS with depth limit [0016]
fix(hnsw): correct level selection probability [0025]
test(storage): add scan_prefix edge cases [0006]
bench(vector): add 100K search benchmark [0030]
docs: update ARCHITECTURE.md with HNSW details
refactor(storage): extract key encoding to separate module
```

Format: `type(scope): description [task_id]`

Types: `feat`, `fix`, `test`, `bench`, `docs`, `refactor`, `chore`

### Branches

- `main` — stable code, all tests pass
- `phase-N/description` — branch per phase or major feature
- Small tasks can be committed directly to the phase branch

## Dependencies (Cargo.toml)

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
bincode = "1"
redb = "2"
ordered-float = "4"
rand = "0.8"
thiserror = "2"
clap = { version = "4", features = ["derive"] }

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
proptest = "1"
tempfile = "3"
```

## Documentation

- `///` doc-comments on all `pub` functions and structs
- Examples in doc-comments (`/// # Examples`)
- `//` regular comments only for non-obvious logic
- Do not comment obvious code
