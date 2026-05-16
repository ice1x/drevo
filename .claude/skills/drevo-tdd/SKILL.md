---
name: drevo-tdd
description: Strict Test-Driven Development workflow for drevo — RED→GREEN→REFACTOR, three test layers, no code without tests
---

# drevo — TDD Workflow

## When to Use
Always. Every feature, bug fix, or refactor in this project follows TDD.

---

## The Iron Rule

**No code without tests.** It is unacceptable to add any functionality without a corresponding test. Every commit must include test coverage for the change it introduces.

---

## RED → GREEN → REFACTOR

For every task:

1. **RED — Write failing tests first.** Define the expected behavior as test cases before any implementation. Run `cargo test` and confirm the new tests fail (compilation error or assertion failure).
2. **GREEN — Write the minimum code to pass.** Implement just enough to make the failing tests pass. Resist the urge to add features, optimizations, or abstractions not driven by a test.
3. **REFACTOR — Improve without changing behavior.** With green tests as a safety net, clean up: extract helpers, eliminate duplication, rename for clarity. Run `cargo test` after each change.

If you find yourself writing implementation before tests — stop, delete the code, write the test first.

---

## Three Test Layers (all required for every task from `00001`)

### 1. Unit Tests
- Location: inline `#[cfg(test)] mod tests` at the bottom of each module file
- Scope: per-module, fast, no I/O
- Run: `cargo test --lib`
- Target: ~100% line coverage on public functions
- Example modules with unit tests: `model.rs`, `storage.rs`, `fts/`, `traversal.rs`, `db.rs`, `api.rs`

### 2. Integration Tests
- Location: `tests/<topic>.rs` files
- Scope: cross-module interactions, in-process
- Run: `cargo test --test '*'`
- Storage tests parameterized by backend (`MemoryBackend` + `RedbBackend`) — same test suite runs against both via macro parameterization
- Example: `tests/storage_tests.rs`, `tests/node_crud_tests.rs`, `tests/fts_recall_tests.rs`, `tests/edge_crud_tests.rs`

### 3. Scenario E2E Tests
- Location: `tests/scenario_<name>.rs`
- Scope: full domain workflows from the use cases (CBT, story editor, task manager, ERP, bug tracker)
- Validates that `kind` / `properties` / `edges` patterns work end-to-end for each domain
- Each scenario file is large (40–80 tests) and exercises CRUD, traversal, FTS, subgraph, and edge cases
- Already implemented: `scenario_cbt_journal`, `scenario_story_editor`, `scenario_task_manager`, `scenario_erp`, `scenario_bug_tracker`

---

## Coverage Targets

- **Unit tests**: as close to 100% as practical for `pub` functions and public types
- **Integration tests**: every public API method tested through at least one realistic workflow
- **Edge cases mandatory**: empty graph, single node, cycles, disconnected components, depth 0, max depth, self-loops, parallel edges, Unicode (CJK, emoji, Cyrillic)

Coverage tools: `cargo tarpaulin --out Html` (or `cargo llvm-cov` if available).

---

## CI Gates (must be green on every push)

```
cargo fmt --check          # formatting
cargo clippy -- -W clippy::all   # zero warnings
cargo test                       # all unit + integration + doc tests
cargo bench --no-run             # benchmarks compile (not run in CI)
```

The CI workflow is in `.github/workflows/`. Cross-compilation (iOS / Android / WASM) runs in a separate workflow.

---

## Benchmarks

- Tool: `criterion`
- Location: `benches/<topic>_bench.rs`
- Existing benches: storage layer (`put/get/scan_prefix`), graph layer (insert / read), FTS (search / index insert / list_recent), traversal (BFS / DFS / shortest_path / subgraph at multiple depths)
- Run: `cargo bench`
- A bench is NOT a substitute for a test — bench measures speed, test verifies correctness. Both are required for performance-sensitive code.

---

## Writing Good Tests

### Name tests by behavior, not by function
- ❌ `fn test_create_node()`
- ✅ `fn create_node_assigns_monotonic_id()`
- ✅ `fn create_node_with_duplicate_title_returns_error()`

### Arrange-Act-Assert
```rust
#[test]
fn delete_node_cascades_to_edges() {
    // Arrange
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(NewNode { kind: "x".into(), title: "A".into(), ..Default::default() }).unwrap();
    let b = db.create_node(NewNode { kind: "x".into(), title: "B".into(), ..Default::default() }).unwrap();
    db.create_edge(NewEdge { from_id: a.id, to_id: b.id, kind: "links_to".into(), ..Default::default() }).unwrap();

    // Act
    db.delete_node(a.id).unwrap();

    // Assert
    assert!(db.edges_of(b.id, Direction::Incoming).unwrap().is_empty());
}
```

### One assertion per test (loose rule)
Multiple assertions are OK when they verify a single behavior from multiple angles. Don't smush unrelated behaviors into one test.

### Property-based tests for invariants
For algorithmic code (FTS tokenizer, graph invariants), use `proptest`. Tracked as Phase 9 task `00057`.

---

## Order of Operations for a New Feature

1. Read the relevant section of `README.md` (task `0XXXX`, definition of done)
2. Add unit tests describing the expected behavior — confirm they fail
3. Add integration tests describing realistic usage — confirm they fail
4. Implement the feature with the minimum code to pass
5. Run `cargo fmt && cargo clippy && cargo test` — all must be green
6. Add a scenario test if the feature touches an end-user workflow
7. Add a benchmark if the feature is on a performance-critical path
8. Update `Current Status` section of `README.md` (mark task `[x]`)
9. Commit with format `type(scope): description [task_id]`

---

## Project-Specific Conventions

- `cargo fmt` before every commit (enforced by CI)
- `cargo clippy -- -W clippy::all` — zero warnings
- No `unwrap()` / `expect()` in library code — `Result<T, DrevoError>` everywhere
- `unwrap()` allowed ONLY in tests and benchmarks
- No `unsafe` without explicit justification in a comment
- Doc-comments on every `pub` API surface
- All test data in English (CLAUDE.md mandates English-only comments / docs / commit messages)
