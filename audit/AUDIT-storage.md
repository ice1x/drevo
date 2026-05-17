# AUDIT-storage — Phase 8.5 task `00103`

**Scope.** `src/storage/{backend.rs, error.rs, memory.rs, mod.rs, redb.rs}` (~819 LOC) plus the parameterised contract suite in `tests/storage_tests.rs`.

**Rules verified against.**

- `drevo-database` §"Storage Backend Abstraction"
- `drevo-database` §"Indexes" (only the trait-level contracts; per-domain indexes live in `db.rs` and are audited under `00106`)
- `drevo-rust` §"Error Handling"
- `drevo-rust` §"WASM Bindings" (`#[cfg(not(target_arch = "wasm32"))]` gates)
- `drevo-architecture` §"SOLID — Liskov Substitution"
- `drevo-architecture` anti-pattern #3 ("stringly-typed errors")
- `drevo-architecture` anti-pattern #5 ("`unwrap()` in library code")
- `drevo-tdd` §"Storage tests parameterized by backend"

**Test baseline at audit start.** 1092 passing (`cargo test --all-features`). The audit must not regress this count; new parity tests grow it.

---

## Findings

### F1 — Mutex poisoning maps to a stringly-typed error variant   ⚠️  fixed in this PR

`drevo-rust` §"Error Handling" requires structured, typed error variants. `drevo-architecture` anti-pattern #5 forbids `unwrap()` / `expect()` in library code — but the next-best thing, the stringly-typed catch-all, is anti-pattern #3.

**Before.** [src/storage/memory.rs:123](src/storage/memory.rs:123), :131, :140, :149, :167 mapped `PoisonError` to `StorageError::Backend(format!("{e}"))`, which is the same variant `redb` errors use, so a caller cannot programmatically distinguish "the lock is poisoned, the process is in an unrecoverable state" from "redb returned an I/O error."

**After.** New variant `StorageError::LockPoisoned` introduced at [src/storage/error.rs](src/storage/error.rs). All five `Mutex::lock` sites now map to it directly. Caller can match on it as a distinct, typed condition.

### F2 — `StorageError::Backend(String)` is stringly-typed for redb errors   ❌  flagged for `00104`

`drevo-architecture` anti-pattern #3: `Backend(String)` is used to wrap every redb error in [src/storage/redb.rs:122](src/storage/redb.rs:122). The skill spec example in `drevo-rust` §"Error Handling" shows `Storage(#[from] redb::Error)` as the structured form.

**No refactor in this task.** The fix — replacing `Backend(String)` with a structured `Redb(redb::Error)` variant gated by `#[cfg(feature = "redb-backend")]` — is the explicit scope of task `00104` and touches the entire `?` propagation chain through `DrevoError`. Doing it here would over-spill `00103`.

**Cross-link.** Phase 8.5 task `00104` "Error hierarchy audit" must close this finding.

### F3 — `StorageError::Serialization(String)` discards bincode error type   ❌  flagged for `00104`

Same anti-pattern as F2, applied to bincode failures in [src/storage/memory.rs:89](src/storage/memory.rs:89) and :99. The skill example uses `#[from] bincode::error::EncodeError`. Bincode v2 has two separate error types (`EncodeError`, `DecodeError`), so the structured form needs two variants.

**No refactor in this task.** Same rationale as F2: belongs to `00104`.

### F4 — `scan_prefix` ordering doc-contract   ✅  compliant

`drevo-database` storage-abstraction doc-contract requires `scan_prefix` to return lexicographically-ordered keys on both backends. [src/storage/backend.rs:44–46](src/storage/backend.rs:44) documents this on the trait. Both implementations honour it:

- `MemoryBackend` ranges over `BTreeMap` which iterates in key order ([src/storage/memory.rs:150–155](src/storage/memory.rs:150)).
- `RedbBackend` walks `table.range(prefix..)` which is also key-ordered ([src/storage/redb.rs:99–110](src/storage/redb.rs:99)).

The integration tests assert sorted output (`tests/storage_tests.rs::scan_prefix_returns_matching_sorted`, `::many_keys_scan_prefix`). The new parity sequence test (F8) re-asserts it under a randomised key population.

### F5 — `flush()` semantics documented and divergent paths gated   ✅  compliant

`drevo-database` §"Storage Backend Abstraction" allows `MemoryBackend::flush()` to be either a no-op (ephemeral) or a snapshot-to-disk (persistent), and `RedbBackend::flush()` to be a no-op (redb already commits on every write).

- The trait docstring documents both modes ([src/storage/backend.rs:54–58](src/storage/backend.rs:54)).
- `MemoryBackend::flush()` is split: `path.is_none()` returns `Ok(())` immediately; the snapshot branch lives behind `#[cfg(not(target_arch = "wasm32"))]` ([src/storage/memory.rs:158–171](src/storage/memory.rs:158)).
- `RedbBackend::flush()` is documented as a no-op with a rationale comment ([src/storage/redb.rs:113–117](src/storage/redb.rs:113)).

### F6 — `#[cfg(not(target_arch = "wasm32"))]` gates correct on FS paths   ✅  compliant

`drevo-rust` §"WASM Bindings" common pitfall #3: forgetting the cfg gate breaks the WASM build. Every FS-touching item in `MemoryBackend` is gated:

- `use std::{fs, io::Write, path::{Path, PathBuf}}` ([src/storage/memory.rs:2–7](src/storage/memory.rs:2))
- `MemoryBackend::path` field ([src/storage/memory.rs:36](src/storage/memory.rs:36))
- `open` / `path` / `load_from_file` / `save_to_file` methods
- The `TempFile` struct and `tempfile_in` helper
- The snapshot branch in `flush()`

`RedbBackend` is gated at the **module** level via the `redb-backend` feature in `src/storage/mod.rs:4`. The skill mandates this because `redb` itself doesn't build for `wasm32-unknown-unknown`.

Verified: `cargo check --target wasm32-unknown-unknown --no-default-features --features wasm` is invoked by the CI matrix.

### F7 — LSP: `MemoryBackend` and `RedbBackend` are observationally identical   ✅  compliant (parity test added)

`drevo-architecture` §SOLID "L" requires the two backends to be substitutable behind `Arc<dyn StorageBackend>`. The parameterised contract suite (`backend_contract_tests!` macro at [tests/storage_tests.rs:7](tests/storage_tests.rs:7)) already covers the per-operation surface — 16 operations on both, 32 generated tests.

What was missing: **a parity test over a long, randomised operation sequence.** Two backends that handle every individual contract test correctly can still diverge on long mixed-operation traces (think: order-of-effect bugs, hidden caching, prefix-scan windowing differences).

**Refactor in this task.** Added a deterministic, seeded `random_operation_sequence_parity` test (F8) — see below. A `proptest`-driven version is the natural follow-up, but `proptest` is not a project dependency yet and pulling it in is Phase 9 task `00057`'s scope.

### F8 — Backend parity over a randomised operation sequence   ✅  test added

Added in this PR: `random_operation_sequence_parity` in `tests/storage_tests.rs`. It draws 1000 operations from `{put, get, delete, scan_prefix}` against a 32-key universe using a tiny xorshift32 RNG seeded deterministically (so failures reproduce). After every operation, it asserts the two backends agree on the observed result — values, `None`s, the full `(key, value)` list returned by `scan_prefix`, and its lex-sorted order.

The test is the first cross-backend parity guarantee that exercises ordering and reinserts under realistic interleaving. It runs in `< 50ms` against `RedbBackend` (the slow side).

### F9 — Mutex poisoning is observable as a typed error   ✅  test added

Added `mutex_poisoning_maps_to_lock_poisoned` in `src/storage/memory.rs`'s unit-test module: spawns a thread that panics while holding the lock, then asserts that the next call on the parent thread returns `StorageError::LockPoisoned` rather than the previous stringly-typed `Backend("…")`.

This is the smallest test that pins the new variant's behaviour. Removing or rewording the variant breaks this test.

### F10 — `Result` type alias surface   ✅  compliant

[src/storage/error.rs:36](src/storage/error.rs:36) exposes a `Result<T> = std::result::Result<T, StorageError>` alias and the storage layer uses it consistently. No `Box<dyn Error>` anywhere in `src/storage/*`. Conforms to `drevo-rust` §"Error Handling" rule "no ad-hoc `Box<dyn Error>` in deep paths."

### F11 — No `unwrap()` / `expect()` in library code   ✅  compliant

`grep -n "unwrap\|expect" src/storage/*.rs` outside `#[cfg(test)]` blocks: only one occurrence remains — `path.parent().unwrap_or(Path::new("."))` at [src/storage/memory.rs:102](src/storage/memory.rs:102), which is `unwrap_or`, not `unwrap()`. `drevo-rust` §"Error Handling" rule satisfied; `drevo-architecture` anti-pattern #5 satisfied.

### F12 — Every `pub` item carries rustdoc   ✅  compliant

`drevo-rust` §"Code Style" rule. Verified by `cargo doc --no-deps -- -D missing_docs` against `src/storage/*`. Every public type, method, and trait item carries a doc comment.

### F13 — Indentation ≤ 3 levels per function   ✅  compliant

`drevo-rust` §"Code Style" rule. Spot-checked: `memory.rs::scan_prefix` (3 levels max, the chained iterator), `redb.rs::scan_prefix` (3 levels, the `for` over the range), `redb.rs::delete` (3 levels, the nested `match`). None exceed 3.

### F14 — `bincode::config::standard()` is the only config in use   ✅  compliant

`drevo-rust` §"Serialization" rule (currently scoped to `00105`'s model audit, spot-checked here). `memory.rs` uses `bincode::serde::{encode_to_vec, decode_from_slice}` with `bincode::config::standard()` consistently ([memory.rs:88](src/storage/memory.rs:88), :98).

---

## Refactor PRs landed in this audit

1. **F1**: Introduce `StorageError::LockPoisoned`; map all five `Mutex::lock` sites in `MemoryBackend` to it. [src/storage/error.rs](src/storage/error.rs), [src/storage/memory.rs](src/storage/memory.rs).
2. **F8**: `random_operation_sequence_parity` parity test using a seeded xorshift32 RNG. [tests/storage_tests.rs](tests/storage_tests.rs).
3. **F9**: `mutex_poisoning_maps_to_lock_poisoned` (data accessors) + `mutex_poisoning_maps_to_lock_poisoned_on_flush` (persistent-mode `flush()`) unit tests for the new variant. [src/storage/memory.rs](src/storage/memory.rs).

## Refactor PRs deferred (cross-linked)

- **F2 + F3**: Replace `StorageError::Backend(String)` and `StorageError::Serialization(String)` with structured `Redb(redb::Error)` / bincode encode/decode variants — owned by Phase 8.5 task `00104` (error hierarchy).
- **Proptest-driven parity**: Phase 9 task `00057` (property-based tests) — when `proptest` becomes a project dev-dependency, port `random_operation_sequence_parity` to a `proptest!` macro and generate the operation sequence with `proptest::collection`.

## Definition of done — task `00103`

- ✅ `audit/AUDIT-storage.md` exists, every cited rule has a verdict.
- ✅ Test baseline grows: 1092 → 1095 (three new tests: parity sequence + two poisoning tests).
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` clean.
- ✅ `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean.
- ✅ `cargo fmt --check` clean.
- ✅ No public API breakage (new error variant is additive; `StorageError` is `#[non_exhaustive]`-free today, so adding a variant is technically a breaking change for downstream callers exhaustively matching it — but `StorageError` has no external consumers outside this crate yet, and the matching call sites inside the crate use catch-alls or are updated in this PR).
