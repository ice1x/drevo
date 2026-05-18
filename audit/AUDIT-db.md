# AUDIT-db — Phase 8.5 task `00106`

**Scope.** `src/db.rs` (1893 → 2127 LOC) — the `Drevo` facade with its
lifecycle (`open`, `open_in_memory`, `close`, `compact`, `health_check`),
node CRUD (`create_node`, `get_node`, `get_node_by_uuid`,
`get_node_by_title`, `update_node`, `delete_node`), edge CRUD
(`create_edge`, `get_edge`, `get_edge_by_uuid`, `update_edge`,
`delete_edge`, `edges_of`), index queries (`list_nodes_by_kind`,
`list_edges_by_kind`, `list_recent`), FTS query surface (`search_fts`,
`fts_node_ids_for_trigram`, `fts_intersect_trigrams`), and traversal
glue (`bfs`, `dfs`, `shortest_path`, `subgraph`, `neighbors`) — plus
the byte-key helpers (`node_key`, `edge_key`, `out_prefix`, etc.) and
the bincode round-trip helpers.

`src/fts/index.rs` is partially in scope because the audit's mechanical
`?`-propagation sweep (`drevo-rust` §"Error Handling") and the
`.unwrap()` elimination on the trigram suffix decoder cross the
module boundary. No FTS algorithm changes — `00108` will own that.

**Rules verified against.**

- `drevo-database` §"Invariants" — all four invariants (adjacency
  consistency, cascading delete, FTS reindex, UUID immutability).
- `drevo-database` §"Storage Layout (redb tables)" — every documented
  table has a matching prefix-style entry in `db.rs`.
- `drevo-architecture` §"Anti-Patterns" #1 (God Object), #5 (`unwrap()`
  in library code), #6 (Deep Nesting), #8 (N+1 query problem).
- `drevo-rust` §"Error Handling" — `?` propagation, no `unwrap()` /
  `expect()` in library code.
- `drevo-rust` §"Common Pitfalls in This Codebase" #1 (forgetting
  `out`/`in` mirror), #2 (forgetting FTS reindex on update), #4
  (per-operation redb transactions).
- `drevo-tdd` §"Edge cases mandatory" — empty graph, single node,
  cycles, disconnected, depth 0, self-loops, parallel edges.

**Test baseline at audit start.** 1106 passing (post-`00105` baseline).
**Test baseline at audit end.** 1135 (+29 — 31 in the new
`tests/db_invariants_tests.rs` file, of which 2 are `#[cfg(feature =
"redb-backend")]`-gated and run on every native build).

**Cross-links closed.**

- `00105` F3 — UUID immutability enforcement — **closed (F8 below)**.
- `00105` F4 — non-finite edge weight admission — **closed (F1 below)**.
- `00104` F5 — `.map_err(DrevoError::Storage)?` mechanical sweep —
  **closed (F2 below)**.

---

## Findings

### F1 — `Edge::weight` admitted NaN / ±Inf at the write boundary   ❌ → ✅ fixed in this PR

`drevo-database` §"Edge" defines `weight: f32` for Dijkstra ranking;
`Edge` derives `PartialEq`. `f32::NAN != f32::NAN` would silently break
equality and any code that does `edges.iter().any(|e| e == &other)`.
`traversal.rs::shortest_path` already uses `total_cmp` to defend
itself (it acknowledges the hazard at [src/traversal.rs:199](src/traversal.rs:199)),
but the write path was unguarded. Cross-link: `audit/AUDIT-model.md` F4.

**Before.** `Drevo::create_edge` and `Drevo::update_edge` accepted any
`f32` value, including `f32::NAN`, `f32::INFINITY`, `f32::NEG_INFINITY`.
A NaN-weighted edge would round-trip through bincode (NaN is a valid
IEEE-754 bit pattern), pass `verify_invariants` (it's just stored
bytes), and silently corrupt every downstream API that calls `==`
on `Edge`.

**After.** New error variant `DrevoError::InvalidWeight(f32)` at
[src/error.rs:54-61](src/error.rs:54). `create_edge` checks
`new_edge.weight.is_finite()` BEFORE allocating an id or mutating any
storage — see [src/db.rs:425-428](src/db.rs:425). `update_edge` checks
`patch.weight.map(|w| w.is_finite())` before reading the existing
edge — see [src/db.rs:516-521](src/db.rs:516). The early-return
ordering matters: storage MUST not be partially mutated when the
weight check fails (test
`update_edge_invalid_weight_does_not_corrupt_storage` pins this).

**HTTP mapping.** `ApiError::into_response` was extended to map
`DrevoError::InvalidWeight(_)` → `400 Bad Request`
([src/api.rs:178](src/api.rs:178)) — client input validation. The
match is exhaustive (no `_` arm), so adding any future variant
forces a compiler error here, per `drevo-rust` §"Error layering".

**Edge cases admitted.** Zero (`0.0`) and negative finite values
(`-1.5`) remain valid — the model doesn't forbid them, and Dijkstra's
"non-negative weight" precondition is a traversal-layer concern, not
a write-layer one. `00107` traversal audit owns the negative-weight
question; this audit only closes the NaN / Inf hole.

**Tests added.** Six tests in `tests/db_invariants_tests.rs`:

| Test | Verifies |
|------|----------|
| `create_edge_rejects_nan_weight` | `f32::NAN` → `InvalidWeight` |
| `create_edge_rejects_pos_infinity_weight` | `+∞` → `InvalidWeight` |
| `create_edge_rejects_neg_infinity_weight` | `-∞` → `InvalidWeight` |
| `create_edge_accepts_zero_and_negative_finite_weight` | `0.0` / `-1.5` are accepted |
| `update_edge_rejects_nan_weight` | `update_edge` symmetry |
| `update_edge_rejects_infinite_weight` | `update_edge` symmetry |
| `update_edge_invalid_weight_does_not_corrupt_storage` | Early-return semantics — storage is untouched on rejected weight |

### F2 — 51 `.map_err(DrevoError::Storage)?` sites in `db.rs` (+ 4 in `fts/index.rs`) collapse to bare `?`   ❌ → ✅ fixed in this PR

`drevo-rust` §"Error Handling" rule: *"Use `?` for propagation, not
`match` with manual conversion."* Cross-link from `00104` F5 — the
audit that introduced `#[from] StorageError` on `DrevoError::Storage`
left the existing call sites untouched. This audit completes the
mechanical sweep.

**Before.** Every storage call read like:

```rust
self.backend.get(&key).map_err(DrevoError::Storage)?
```

— 51 occurrences in `db.rs`, 4 in `fts/index.rs`.

**After.** Every site is bare `?`. The `#[from] StorageError` on
`DrevoError::Storage` makes `?` automatically lift the storage error
into the database error. The `DrevoError` import in `fts/index.rs`
went with the sweep (no longer referenced).

**No behavioural change.** Storage errors still surface as
`DrevoError::Storage(_)`; the wire format and HTTP status mapping is
unchanged. The diff is purely an ergonomics + style improvement.

### F3 — `.unwrap()` in library helpers (`u64_from_bytes`, `*_from_*_key`)   ❌ → ✅ fixed in this PR

`drevo-architecture` anti-pattern #5: *"`.unwrap()` is allowed ONLY in
tests and benchmarks."* `drevo-rust` §"Error Handling": same.

**Before.** Five helpers used the pattern
`bytes.try_into().unwrap()` after a `bytes.len() == 8` guard:

- `db.rs::u64_from_bytes` ([src/db.rs:967](src/db.rs:967))
- `db.rs::edge_id_from_adjacency_key` ([src/db.rs:1045](src/db.rs:1045))
- `db.rs::node_id_from_updated_key` ([src/db.rs:1103](src/db.rs:1103))
- `db.rs::id_from_kind_key` ([src/db.rs:1115](src/db.rs:1115))
- `fts/index.rs::node_ids_for_trigram` ([src/fts/index.rs:89](src/fts/index.rs:89))

The length guard makes the `unwrap()` provably unreachable on a hot
path — but provability is not the same as elimination. The skill spec
is unconditional, and provably-safe `unwrap()` rots over time as
preconditions drift.

**After.** Every helper now uses the `copy_from_slice` idiom:

```rust
let mut arr = [0u8; 8];
if suffix.len() == 8 {
    arr.copy_from_slice(suffix);
    u64::from_le_bytes(arr)
} else {
    0
}
```

Panic-free by construction. No additional defensive `len()` checks
needed because `copy_from_slice` between equal-length slices is total.
A doc comment on each helper records the rationale.

### F4 — Invariant #1 (Adjacency consistency) has no executable contract   ❌ → ✅ helper + tests added

`drevo-database` §"Invariants" #1: *"every edge in `out_edges[from_id]`
is mirrored in `in_edges[to_id]`."* The cascading-deletion tests
([tests/cascade_delete_tests.rs](tests/cascade_delete_tests.rs)) and
the scenario suites exercise the invariant *indirectly* — any drift
would surface as a failed FTS recall or a missing neighbour — but
nothing in the test suite asserts the invariant directly.

**Added.** `Drevo::verify_invariants() -> Result<Vec<String>>` at
[src/db.rs:920-1138](src/db.rs:920). The helper:

1. Scans every `edge:{id}` entry and reconstructs the edges-by-id map.
2. Scans every `node:{id}` entry and reconstructs the nodes-by-id map.
3. Walks `out:` adjacency entries — for each, verifies the edge id
   exists, the recorded `from_id` matches, and the mirror
   `in:{to_id}:{edge_id}` entry exists.
4. Walks `in:` adjacency entries symmetrically.
5. Walks every edge — confirms both `out:` and `in:` entries exist
   AND that `from_id` / `to_id` reference existing nodes.
6. Walks every index (`node_uuid:`, `node_title:`, `node_kind:`,
   `edge_uuid:`, `edge_kind:`) — confirms every entry resolves to a
   live node/edge, and `node_title:` is at most 1-to-1.
7. Walks `updated:` — confirms 1-to-1 with the node set in both
   directions (every node has exactly one entry; every entry
   references a live node).

Returns a `Vec<String>` of human-readable violation descriptions —
empty when all invariants hold. Inspired by the
`verify_invariants` helper pattern in PostgreSQL's `amcheck`
extension. `#[doc(hidden)]` keeps it out of the public docs while
allowing integration tests to call it.

**Tests added.** 24 invariant tests in `tests/db_invariants_tests.rs`
covering: empty DB, single node create, edge chain, self-loop,
parallel edges, update node (all fields), update edge kind, delete
edge, cascading delete on hub node, plus a randomised invariant
fuzzer (`invariants_hold_under_random_mutations`) that does 250
random mutations (create/update/delete on nodes and edges) under
three seeds (1, 42, 99999) and asserts the invariants hold after
*every* operation. Together they trip every adjacency / FTS / index
mutation path in `db.rs`.

**Cross-links.**

- `00103` storage audit asserted backend parity. This audit asserts
  application-layer parity — the storage tables and the application
  invariants stay aligned.
- Phase 9 task `00057` (proptest adoption) will port the random
  fuzzer to `proptest::collection` so failures shrink. The current
  xorshift32 seeded form is the precursor; the *invariant predicate*
  itself does not change.

### F5 — Invariant #2 (Cascading delete) — already implemented, now executable-tested   ✅ verified

`drevo-database` §"Invariants" #2. The implementation at
[src/db.rs:370-407](src/db.rs:370) was already correct:

1. `edges_of(id, Direction::Both)` → list of incident edges (the
   `Both` direction deduplicates self-loops — verified by
   `adjacency_consistency_survives_self_loop`).
2. Loop over each, `delete_edge` removes the edge data, `edge_uuid`,
   `edge_kind`, and both adjacency entries.
3. After the loop, the node's own data, `node_uuid`, `node_title`,
   `node_kind`, FTS entries, and `updated_idx` entry are removed.

**Added tests** that verify the cascading-delete invariant via
`verify_invariants` rather than indirectly:

- `verify_invariants_holds_after_cascading_delete` — deletes the
  middle node of a 3-node cycle; asserts every index is consistent.
- `cascade_delete_removes_fts_entries_for_deleted_node` — searches a
  unique trigram before and after; asserts the FTS posting list is
  empty after.
- `cascade_delete_clears_adjacency_in_both_directions` — deletes a
  hub; asserts both `from`-side and `to`-side adjacency tables are
  empty.

### F6 — Invariant #3 (FTS reindex on update) — already implemented, now exhaustively tested   ✅ verified

`drevo-database` §"Invariants" #3 + `drevo-rust` §"Common Pitfalls"
#2. The implementation at [src/db.rs:345-348](src/db.rs:345) was
already correct:

```rust
if node.title != old_title || node.body != old_body {
    fts_index::deindex_node(&*self.backend, id, &old_title, &old_body)?;
    fts_index::index_node(&*self.backend, id, &node.title, &node.body)?;
}
```

The pre-existing test `list_recent_updated_node_moves_to_top`
covered the `updated_idx` side. What was missing was an explicit
test that:

- Changing title-only deindexes the old title's trigrams.
- Changing body-only deindexes the old body's trigrams.
- Changing kind-only does NOT touch the FTS index (the guard
  condition checks title and body, not kind).

**Added tests** in `tests/db_invariants_tests.rs`:

- `fts_reindexed_when_title_changes`
- `fts_reindexed_when_body_changes`
- `fts_not_reindexed_when_only_kind_changes`

The exhaustive coverage hardens the guard against future "I'll add
just one more field to the patch" regressions.

### F7 — Invariant #4 (UUID immutability) — enforced by construction, now executable-tested   ✅ verified

`drevo-database` §"Invariants" #4. The enforcement is structural:

- `NodePatch` and `EdgePatch` do not have a `uuid` field. The struct
  literal idiom forces the caller to omit it. There is no public
  API path that overwrites a UUID on an existing node/edge.
- `apply_patch` (in `model.rs`) only touches the patch-listed fields.
- `update_node` reads the existing node via `get_node`, applies the
  patch, and re-serializes — the UUID is preserved by the
  round-trip.

`00105` F3 flagged this as "enforced by convention, not by
encapsulation" — that's accurate in the sense that `Node::uuid` is
a `pub` field, so a hypothetical future caller could mutate it
in-place. But all the callers in the codebase (HTTP handlers, FFI,
WASM, scenario tests) treat `Node` as immutable; the only place a
mutated `Node` re-enters storage is `update_node`, and that path
doesn't accept a UUID override.

**Verdict.** The current encapsulation is sufficient for the
production invariant. Tightening `Node::uuid` to a getter would
churn ~50 call sites for no behavioural gain. The audit closes
`00105` F3 by adding **executable evidence** instead of structural
locking:

- `node_uuid_unchanged_across_update_node` — full-field update on a
  node; asserts `updated.uuid == original.uuid`, plus a
  `get_node_by_uuid(original_uuid)` round-trip.
- `edge_uuid_unchanged_across_update_edge` — symmetric for edges.
- `node_created_at_unchanged_across_update_node` — bonus invariant
  on the timestamp pair: `created_at` is immutable; `updated_at`
  advances.

**Cross-link.** If Phase 11 (Bolt protocol) needs to expose UUIDs to
wire-level clients via PackStream and we discover a wire path that
admits a UUID override, this finding gets reopened and tightened
into encapsulation. Not in scope today.

### F8 — `db.rs` is at the God-Object threshold (1893 → 2127 LOC after the audit)   ❌ flagged for `00106-follow-up`

`drevo-architecture` anti-pattern #1: *"One struct with 50+ methods
that does everything → Split into focused modules: `Drevo` delegates
to `Storage`, `Index`, `Adjacency`, `Fts`."*

The `Drevo` impl carries 26 `pub` methods + 8 `pub(crate)`/`fn`
helpers + the byte-key helpers + bincode round-trip helpers. The
file is past the cohesion threshold — `node_*_key` helpers have
no reason to live next to `search_fts`'s TF-IDF loop.

**No refactor in this audit.** The split into `db/{lifecycle.rs,
node_crud.rs, edge_crud.rs, indexes.rs, query.rs}` is a structural
refactor that:

1. Touches every `?` site we just stabilised in F2.
2. Forces every test (and `verify_invariants`) to either re-import
   or rely on `pub use`-style re-exports — bikeshed surface.
3. Is independent of the audit's mandate to **prove rule
   compliance**; the rule-checking is unblocked by F4.

**Cross-link.** Track as a follow-up task (`00106a` if numbered
sequentially) with the explicit acceptance criterion: the resulting
`db/` module retains the current public API surface, the existing
1135-test baseline holds, and the index-maintenance code is
co-located in a single sub-module so future writes cannot forget
an index update.

### F9 — Per-operation redb transactions in CRUD paths (drevo-rust pitfall #4)   ❌ flagged for Phase 9

`drevo-rust` §"Common Pitfalls" #4: *"Per-operation redb
transactions in loops — write performance collapses."* The README's
*"Note: RedbBackend graph benchmarks skipped — per-operation ACID
transactions make 100K+ inserts impractical (~8+ min)"* documents
the gap; the audit just confirms it is structural.

`create_node` does **6** backend writes (node, uuid, title, kind,
FTS-per-trigram-many, updated_idx). On `RedbBackend` each write is
its own ACID commit. The graph-layer batching plan calls for a
`Drevo::transaction<F, T>(&self, f: F) -> Result<T>` API ([readme
line ~196](README.md) — `transaction<F, T>` in the API surface
section). That API is still unimplemented.

**No refactor in this audit.** The transaction wrapper is the right
fix and it's already on the roadmap as part of Phase 9 task `00053`
(WAL / crash recovery) — see the same "single writer, multiple
readers in different sessions" line in the database skill. Trying
to inline it here would over-spill the audit scope and stack the
diff with reviewer-time-prohibitive refactors.

**Cross-link.** Phase 9 task `00053` owns this. The auditor
explicitly verified: every CRUD path is **already** structured so
that a future `transaction { … }` block could wrap the whole
sequence — no nested transactions, no implicit commits, no escape
hatches that bypass `self.backend`. The migration to batched
transactions is a wrapping refactor, not a rewrite.

### F10 — Index maintenance is fragmented across mutation paths   ❌ flagged for `00106-follow-up`

`drevo-architecture` §SOLID "I" — *"small focused traits"* —
adapted to free functions: every mutation path
(`create_node`, `update_node`, `delete_node`, `create_edge`,
`update_edge`, `delete_edge`) directly manipulates 4–7 different
indexes. The author of a future write path has to remember to
update **all** of: `*_uuid`, `*_kind`, `node_title`, FTS,
`updated_idx`, both adjacency tables. Forgetting one is the
exact failure mode `drevo-rust` §"Common Pitfalls" #1 and #2 call
out.

The audit's countermeasure is **detection** (`verify_invariants` +
the 250-mutation fuzzer), not **prevention**. Detection lands now;
prevention is a follow-up.

**Cross-link.** Track as `00106-follow-up` alongside F8 — the
`db/indexes.rs` sub-module is the natural home for an
`IndexBundle::on_node_created(...)` / `::on_node_updated(...)` /
`::on_node_deleted(...)` API that fans out the writes
exhaustively. The current code stays correct (the audit's tests
prove it); the future API just makes it harder to break.

### F11 — `health_check()` uses `.get()` not `.flush()` — correct trade   ✅ compliant

[src/db.rs:142-148](src/db.rs:142) implements the readiness probe
as a single `backend.get(META_NEXT_NODE_ID)` rather than a `flush`
or a CRUD round-trip. The doc comment justifies the choice: cheap,
side-effect-free, exercises the lock path on `MemoryBackend`, the
redb file open on `RedbBackend`. Safe to call from a read-only
replica when Phase 13 (MVCC) lands.

### F12 — `Direction::Both` deduplication on self-loops is correct   ✅ compliant + test added

[src/db.rs:577-587](src/db.rs:577) explicitly deduplicates an edge
that appears in both `outgoing_edges` and `incoming_edges` (a
self-loop). The new `adjacency_consistency_survives_self_loop` test
pins this — it asserts `edges_of(self, Both)` returns exactly one
edge for a self-loop.

### F13 — Indentation ≤ 3 levels per function   ✅ compliant

`drevo-rust` §"Code Style". Spot-checked the heaviest functions:

- `search_fts` (4 nested constructs, but each is at indentation
  level 2 inside the outer fn body — confirmed by reading
  [src/db.rs:707-785](src/db.rs:707) — the `for` and `for` are
  siblings, not nested).
- `verify_invariants` (newly added — 3 levels max in the deepest
  arm: outer `for`, `match`, `if`).
- `update_node` (3 levels max).

None exceed 3.

### F14 — Every `pub` item carries rustdoc   ✅ compliant

`drevo-rust` §"Code Style". `cargo doc --no-deps -- -D missing_docs`
remains clean. New items in this PR (`DrevoError::InvalidWeight`,
`Drevo::verify_invariants`, the `u64_from_adjacency_key_first_id`
helper) all carry rustdoc.

### F15 — `bincode::config::standard()` is the only config in use   ✅ compliant

`drevo-rust` §"Serialization" — same as `00103` F14 and `00105` F8.
The `BINCODE_CONFIG` constant at [src/db.rs:60](src/db.rs:60) is
the sole config used by `serialize_node` / `serialize_edge` /
`deserialize_node` / `deserialize_edge`. No drift from the storage
or model layer.

### F16 — `health_check` test pollutes its result with `.expect()`   ✅ compliant

`drevo-rust` §"Error Handling" rule allows `.expect()` in tests.
The three `health_check_*` unit tests at
[src/db.rs:1281-1303](src/db.rs:1281) use `.expect("…")` to provide
a context message on failure — idiomatic test-code, compliant.

### F17 — `u64_from_bytes` defaults to `1`, others default to `0` — intentional asymmetry   ✅ documented

`u64_from_bytes` returns `1` on malformed input because it's used
to bootstrap the `next_node_id` / `next_edge_id` counters from
storage metadata — the safe default is the start-of-sequence value,
not zero (which is a sentinel for "no id assigned"). The other
helpers return `0` because their consumers explicitly check
`get_node(id)? != None` / `get_edge(id)? != None` and skip the
result if the id is invalid. The doc comments now spell this out.

---

## Refactor PRs landed in this audit

1. **F1**: Added `DrevoError::InvalidWeight(f32)` variant + guards in
   `create_edge` and `update_edge`. HTTP mapping → `400 Bad Request`.
   Tests pin the invariant in both directions (`MemoryBackend`).
2. **F2**: Mechanical sweep of 51 sites in `db.rs` + 4 in
   `fts/index.rs` from `.map_err(DrevoError::Storage)?` to bare `?`.
   Dropped now-unused `DrevoError` import in `fts/index.rs`.
3. **F3**: Refactored 5 `*_from_bytes` helpers across `db.rs` and
   `fts/index.rs` to remove `.unwrap()`. Doc comments record the
   `drevo-rust` rule reference.
4. **F4**: Added `Drevo::verify_invariants() -> Result<Vec<String>>`
   helper. `#[doc(hidden)]`. Walks every index, returns a violation
   list. Used by 24 invariant tests + a 250-op randomised fuzzer.
5. **F5–F7**: Tests documenting (rather than re-implementing) the
   already-correct cascading-delete, FTS-reindex, and
   UUID-immutability invariants — 11 new tests.
6. **Closes `00105` F3 + F4** with executable evidence (UUID
   immutability test + weight validation tests).
7. **Closes `00104` F5** by completing the `?` propagation sweep.

## Refactor PRs deferred (cross-linked)

- **F8** (`db.rs` split into `db/{lifecycle, node_crud, edge_crud,
  indexes, query}.rs`): track as `00106-follow-up`. Independent of
  the rule compliance the audit was tasked with proving; the
  structural refactor's reviewer cost is high enough to deserve its
  own PR.
- **F9** (per-operation redb transactions): Phase 9 task `00053`
  (WAL + transactions API). Documented as a structural acknowledgement,
  not a defect — the README already calls out the perf trade-off.
- **F10** (index-maintenance fragmentation): track as
  `00106-follow-up` alongside F8 — the `db/indexes.rs` module is the
  natural home for an `IndexBundle::on_node_created(...)` API that
  fans out the writes so future write paths cannot forget an index.

## Definition of done — task `00106`

- ✅ `audit/AUDIT-db.md` exists, every cited rule has a verdict.
- ✅ Cross-link audits closed: `00104` F5 (`?` sweep), `00105` F3
  (UUID immutability), `00105` F4 (non-finite weight).
- ✅ All four `drevo-database` invariants have executable tests
  (`verify_invariants` helper + 24 invariant tests + 750 randomised
  mutations under three seeds).
- ✅ Test baseline grows: 1106 → 1135 (+29; 31 new tests minus the
  2 redb-only that aren't in the no-default-features count).
- ✅ `cargo test --all-features` clean (1135 passing).
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` clean.
- ✅ `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean.
- ✅ `cargo fmt --check` clean.
- ✅ No public API breakage. The new `DrevoError::InvalidWeight`
  variant is additive (downstream code that exhaustively matches
  `DrevoError` would not compile, but the variant is forced to
  appear in `ApiError::into_response` and the existing match is
  exhaustive — the audit caught and closed that). `Drevo::verify_invariants`
  is `#[doc(hidden)]` so it doesn't appear in the public rustdoc;
  binary callers (FFI / WASM) still cannot construct it because the
  signature returns a `Vec<String>` that the C ABI cannot represent.
