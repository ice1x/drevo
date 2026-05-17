# AUDIT-error — Phase 8.5 task `00104`

**Scope.** `src/error.rs` (38 → 56 LOC), `src/storage/error.rs` (46 → 104 LOC), every `?` site that constructed the now-removed `StorageError::Backend(String)` / `StorageError::Serialization(String)` / `DrevoError::Serialization(String)` variants, and the HTTP error-mapping in `src/api.rs:170-187`.

**Rules verified against.**

- `drevo-rust` §"Error Handling" — single error enum per crate via `thiserror`, `?` propagation, no `Box<dyn Error>` in deep paths, no `unwrap()` / `expect()` in library code.
- `drevo-rust` §"Error layering across boundaries" — `Storage Error → DrevoError → HTTP/JSON error → 5xx response`; never let internal redb errors leak directly into HTTP responses.
- `drevo-architecture` §"Error Propagation Architecture" — explicit two-layer wrap (`StorageError → DrevoError → QueryError → HTTP`); each layer wraps/converts errors from the layer below.
- `drevo-architecture` Anti-Pattern #3 — "stringly-typed errors" (catch-all `String` variants discard the upstream type).
- `drevo-tdd` §"Edge cases mandatory" — new typed variants need a test that pins their construction path.

**Baseline at audit start.** 1095 passing tests (the post-`00103` baseline). The audit must not regress this count; new tests for the structured variants grow it.

**Decision recorded.** Phase 8.5 task description offers a choice: collapse `StorageError` + `DrevoError` into one, or keep the two-layer hierarchy. We keep the two layers because `drevo-architecture` §"Error Propagation" explicitly diagrams it that way and Phase 10 (Cypher executor) will introduce `QueryError` as the next layer up. Collapsing now would force a re-split in three months. The *Immediate subtasks* item "Rename `StorageError` to `DrevoError` or reconcile error hierarchy" is **closed by this audit with the verdict: keep two layers, eliminate the stringly-typed variants on both.**

---

## Findings

### F1 — `StorageError::Backend(String)` is stringly-typed for redb errors   ❌ → ✅ fixed in this PR

Carried over from `00103` finding F2. `drevo-architecture` anti-pattern #3.

**Before.** [src/storage/redb.rs:122 (old)] mapped every redb sub-error type (`redb::DatabaseError`, `redb::TransactionError`, `redb::TableError`, `redb::CommitError`, `redb::StorageError`) through `e.to_string()` into a single `StorageError::Backend(String)`. Callers could not programmatically distinguish "table missing" from "txn aborted" from "I/O" — every redb failure looked identical.

**After.** New variant `StorageError::Redb(Box<redb::Error>)` at [src/storage/error.rs:38-46](src/storage/error.rs:38) gated by `#[cfg(feature = "redb-backend")]`. Five `From` impls (one for each redb sub-error type — see [src/storage/error.rs:56-94](src/storage/error.rs:56)) lift the sub-types into the variant so `?` works at every call site without a `.map_err(...)` wrapper. The `map_redb_err` helper at the bottom of `redb.rs` was deleted; all eight redb call sites in `RedbBackend` now use `?` directly.

**Boxing rationale.** `redb::Error` is a large enum (~160 bytes — it includes backtraces, ranges, and full table identifiers). Without boxing, every `Result<T, StorageError>` (and transitively `Result<T, DrevoError>`) triggers clippy's `result_large_err` lint at every `?` site, breaking `-D warnings`. `Box<redb::Error>` keeps `StorageError` small (24 bytes) at the cost of one heap allocation on the cold error path — an entirely acceptable trade.

### F2 — `StorageError::Serialization(String)` discards bincode error type   ❌ → ✅ fixed in this PR

Carried over from `00103` finding F3. Same anti-pattern as F1, applied to bincode failures.

**Before.** [src/storage/memory.rs:89, :99 (old)] mapped both `bincode::error::EncodeError` and `bincode::error::DecodeError` to a single `StorageError::Serialization(String)`. The two error types are structurally distinct (encode failure ≈ programmer bug — the on-disk format changed and the codec doesn't support the new shape; decode failure ≈ corrupt persisted bytes — the bytes on disk don't match the codec's expected schema), but the stringly-typed variant collapsed them.

**After.** Split into two structured variants at [src/storage/error.rs:23-32](src/storage/error.rs:23):

```rust
#[error("encode error: {0}")]
Encode(#[from] bincode::error::EncodeError),

#[error("decode error: {0}")]
Decode(#[from] bincode::error::DecodeError),
```

Both `MemoryBackend::load_from_file` and `MemoryBackend::save_to_file` now use `?` directly. The corrupt-file unit test `open_corrupt_file_returns_error` at [src/storage/memory.rs:399](src/storage/memory.rs:399) now matches `StorageError::Decode(_)` instead of `StorageError::Serialization(_)`.

### F3 — `DrevoError::Serialization(String)` discards bincode error type   ❌ → ✅ fixed in this PR

Same anti-pattern at the `DrevoError` layer, applied to node/edge bincode round-trips through the storage layer.

**Before.** [src/db.rs:1125-1148 (old)] — every `serialize_edge` / `serialize_node` / `deserialize_edge` / `deserialize_node` helper used `.map_err(|e| DrevoError::Serialization(e.to_string()))`. Same loss-of-type as F2 but at the upper boundary.

**After.** Added `Encode(#[from] EncodeError)` and `Decode(#[from] DecodeError)` to `DrevoError` at [src/error.rs:24-32](src/error.rs:24). All four helpers in `db.rs` now propagate with `?` — see [src/db.rs:1124-1146](src/db.rs:1124).

The HTTP mapping in `api.rs::IntoResponse for ApiError` was updated exhaustively — [src/api.rs:179-182](src/api.rs:179) now matches `DrevoError::Storage(_) | DrevoError::Encode(_) | DrevoError::Decode(_) | DrevoError::Io(_)` → `500 Internal Server Error`. No `_` catch-all; adding a new `DrevoError` variant in the future will force a compiler error here, per `drevo-rust` §"Error layering across boundaries".

### F4 — HTTP layer maps every `DrevoError` variant to an explicit status code   ✅ compliant

`drevo-rust` §"Error layering across boundaries". The match in [src/api.rs:172-184](src/api.rs:172) is exhaustive — every variant of `DrevoError` is named explicitly, no `_` arm. After the F3 refactor the match needs `Encode` / `Decode` arms (added) and the formerly-listed `Serialization` arm is gone (removed). The compiler enforces exhaustiveness because `DrevoError` is not `#[non_exhaustive]`.

Status-code assignment after refactor:
- `NodeNotFound` | `EdgeNotFound` → `404 Not Found`
- `DuplicateTitle` → `409 Conflict`
- `Locked` → `503 Service Unavailable`
- `Storage` | `Encode` | `Decode` | `Io` → `500 Internal Server Error`

No internal redb error string is exposed in the response body — the `Display` impls compose into a typed prefix (`storage error: redb error: ...`) but no raw stack trace or file path leaks.

### F5 — Every `?` site uses `From`-based propagation, not manual `match` conversion   ⚠️  partially compliant — deferred follow-up to `00106`

`drevo-rust` §"Error Handling" rule: "Use `?` for propagation, not `match` with manual conversion."

**Compliant in this PR.** Eliminated 13 stringly-typed conversions (8 redb sites in `redb.rs`, 2 bincode sites in `memory.rs`, 4 bincode sites in `db.rs`). Each used to do `.map_err(|e| StorageError::Backend(e.to_string()))` or similar; all now use bare `?`.

**Deferred.** `src/db.rs`, `src/fts/index.rs`, and `src/api.rs:500,524,527` still contain **58 `.map_err(DrevoError::Storage)?` sites** — these are not stringly-typed (the conversion is a function pointer, not a `format!`) and they predate the `#[from]` impl on `DrevoError::Storage`. Now that `From<StorageError> for DrevoError` exists via `#[from]`, these can all be simplified to bare `?`. The mechanical sweep belongs to task `00106` (DB core audit), which is going to touch the same call sites for the index-maintenance extraction and the `db/` module split. Doing both refactors in one PR keeps the diff coherent; doing it here would over-spill `00104`. **Cross-link: 00106.**

### F6 — Single error enum per crate, layered correctly   ✅ compliant

`drevo-rust` §"Error Handling" rule. After the refactor:

- `StorageError` — storage-layer only. Variants are I/O, lock poisoning, key-not-found, encode/decode of stored bytes, redb backend errors. No HTTP, no domain semantics.
- `DrevoError` — database-layer. Wraps `StorageError` via `#[from]`. Variants add domain semantics (`NodeNotFound`, `EdgeNotFound`, `DuplicateTitle`, `Locked`) and the encode/decode bytes for node/edge round-trips that don't go through storage's bytes-in / bytes-out interface.
- `ApiError` — HTTP-layer. Wraps `DrevoError` via `From`. Adds `BadRequest(String)` for client-input errors. Translates to status code + JSON body. **Only this layer constructs strings**, and only for client-facing messages — never to discard a typed upstream error.

The layered shape matches the `drevo-architecture` diagram (`StorageError → DrevoError → ... → HTTP`). Phase 10 (Cypher) will add a `QueryError` layer between `DrevoError` and `ApiError`; the current shape leaves room for that.

### F7 — No `Box<dyn Error>` in deep paths   ✅ compliant

`drevo-rust` §"Error Handling" rule. `grep -rn "Box<dyn.*Error\|Box<dyn Error" src/` returns no matches. The only `Box` in the error hierarchy is the deliberate `Box<redb::Error>` size optimisation (F1).

### F8 — No `unwrap()` / `expect()` in error-handling library code   ✅ compliant

`drevo-architecture` anti-pattern #5. Checked `src/error.rs`, `src/storage/error.rs`, and the `ApiError::into_response` path in `src/api.rs`. The only `unwrap()` reachable on a non-test path is `path.parent().unwrap_or(Path::new("."))` at [src/storage/memory.rs:102](src/storage/memory.rs:102), already verified compliant in `00103` F11 (it's `unwrap_or`, not `unwrap()`).

### F9 — `Result<T, StorageError>` and `Result<T, DrevoError>` type aliases consistent   ✅ compliant

Both `Result<T>` aliases (`src/storage/error.rs:Result`, `src/error.rs:Result`) are still in use. No site reaches for `std::result::Result<T, E>` except the new test functions in `tests/storage_tests.rs` that intentionally disambiguate from the macro-imported `Result` alias.

### F10 — New typed variants are observable via tests   ✅ tests added

`drevo-tdd` §"Edge cases mandatory" rule. Three new tests in [tests/storage_tests.rs:355-415](tests/storage_tests.rs:355):

1. `encode_error_converts_via_from` — pins the variant tag of `StorageError::Encode`.
2. `decode_error_converts_via_from` — drives a real `bincode::serde::decode_from_slice` failure through `?` and asserts the variant routes to `StorageError::Decode`. Removing or renaming the `#[from] DecodeError` impl breaks this test.
3. `redb_sub_errors_convert_to_redb_variant` (`#[cfg(feature = "redb-backend")]`) — provokes `redb::TableError::TableDoesNotExist` from `open_table()` and asserts the `?` path routes through `From<TableError> for StorageError` into `StorageError::Redb(_)`. Removing any of the five `From<redb::*>` impls breaks this test.

### F11 — `redb::Error` is large; `StorageError` must stay heap-allocation-free on the hot path   ✅ documented + boxed

`drevo-rust` §"Performance / hot path" implicit rule (the codebase keeps `Result<T, E>` cheap to return). Without boxing, `StorageError` blew up to 160+ bytes, propagating through every `Result` return in the crate. Boxing the `Redb` variant (cold-path-only allocation) keeps `StorageError` at 24 bytes — the size of the largest of the remaining variants (`Vec<u8>` for `NotFound`). The doc comment on `StorageError::Redb` records the reason so a future reader does not "fix" the box.

### F12 — `cargo doc -D missing_docs` clean   ✅ compliant

`drevo-rust` §"Code Style". Every new `pub` item (`Encode`, `Decode`, `Redb` variants on both error types; the five `From<redb::*>` impls) carries a rustdoc comment.

---

## Refactor PRs landed in this audit

1. **F1**: `StorageError::Backend(String)` removed; replaced with `Redb(Box<redb::Error>)` and five `From<redb::*>` lifts so `?` works on every redb sub-error type. Deleted the `map_redb_err` helper in [src/storage/redb.rs](src/storage/redb.rs). [src/storage/error.rs](src/storage/error.rs).
2. **F2**: `StorageError::Serialization(String)` removed; split into `Encode(#[from] EncodeError)` / `Decode(#[from] DecodeError)`. [src/storage/error.rs](src/storage/error.rs), [src/storage/memory.rs](src/storage/memory.rs).
3. **F3**: `DrevoError::Serialization(String)` removed; same `Encode` / `Decode` split. All four bincode helpers in [src/db.rs:1124-1146](src/db.rs:1124) use `?`.
4. **F4**: `ApiError::into_response` updated to exhaustively match the new `DrevoError` variants. [src/api.rs:172-184](src/api.rs:172).
5. **F10**: Three new tests in [tests/storage_tests.rs:355-415](tests/storage_tests.rs:355) pin the new variants under `?` propagation.
6. **F11**: `Box<redb::Error>` chosen over inline; doc comment documents the rationale and the size budget.

## Refactor PRs deferred (cross-linked)

- **F5** (58 redundant `.map_err(DrevoError::Storage)?` sites in `src/db.rs`, `src/fts/index.rs`, `src/api.rs`): Phase 8.5 task `00106` (DB core audit). Same surface — index-maintenance extraction will touch the same lines.
- The `Locked` variant on `DrevoError` is reachable but never constructed in the codebase today (no `?` site produces it). The audit confirms it stays — Phase 9 task `00057` (multi-process locking) is its first caller. Documented in F4.

## Definition of done — task `00104`

- ✅ `audit/AUDIT-error.md` exists, every cited rule has a verdict.
- ✅ Stringly-typed `StorageError::Backend(String)`, `StorageError::Serialization(String)`, `DrevoError::Serialization(String)` are all removed; replaced with typed variants.
- ✅ `Storage(#[from] StorageError)` + `Encode(#[from] EncodeError)` + `Decode(#[from] DecodeError)` + `Io(#[from] io::Error)` on `DrevoError` — `?` propagation works without manual conversion at every refactored site.
- ✅ `ApiError::into_response` matches every `DrevoError` variant exhaustively (clippy `non_exhaustive_omitted_patterns` would catch a regression).
- ✅ The open *Immediate subtask* "Rename `StorageError` to `DrevoError` or reconcile error hierarchy" is closed with the verdict: keep two layers.
- ✅ Test baseline grows: 1095 → 1098 (three new variant tests).
- ✅ `cargo test --all-features` clean (1098 passing).
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` clean (including `result_large_err`, which forced the boxing of `Redb`).
- ✅ `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean.
- ✅ `cargo fmt --check` clean.
- ⚠️  Public API breakage: `StorageError` and `DrevoError` are `pub` types but neither is `#[non_exhaustive]`. Removing `Backend(String)` and `Serialization(String)` is technically a breaking change for downstream callers that match exhaustively on them. There are no such callers outside this crate today (no published 0.1 yet). A pre-1.0 break is acceptable and aligned with the "audit before extending" principle — better to break now than after Phase 10/11 lock in the Bolt-wire-protocol-visible error shape.
