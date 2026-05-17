# AUDIT-model — Phase 8.5 task `00105`

**Scope.** `src/model.rs` (615 → 794 LOC) — the data-model types (`Node`, `Edge`, `Properties`, `NewNode`/`NewEdge`, `NodePatch`/`EdgePatch`, `Direction`, `ScoredNode`, `SubGraph`) and the timestamp / UUID helpers (`now_ms`, `new_uuid_v7`). No code outside `src/model.rs` is in scope for this task; cross-cutting effects on consumers are flagged for the relevant downstream audits.

**Rules verified against.**

- `drevo-database` §"Data Model" (Node / Edge / Properties shapes)
- `drevo-database` §"Invariants" (UUID immutability — invariant #4)
- `drevo-database` §"Serialization" ("bincode v2 — compact, fast, deterministic")
- `drevo-rust` §"Error Handling" (no `unwrap()` / `expect()` in library code)
- `drevo-rust` §"Serialization" (bincode v2 for KV, serde_json for boundaries)
- `drevo-rust` §"Code Style" (rustdoc on every `pub` item, ≤ 3 levels of indentation)
- `drevo-architecture` anti-pattern #5 ("unwrap in library code")
- `drevo-architecture` anti-pattern #3 ("stringly-typed") — sanity check on `kind: String`
- `drevo-architecture` §"Builder Pattern" (aspirational for `NewNode` — flagged, deferred)
- `drevo-tdd` §"Edge cases mandatory" — Unicode (CJK, emoji, Cyrillic)

**Test baseline at audit start.** 1095 passing (`cargo test --all-features`) after the storage + error audits. The audit must not regress this count; new tests grow it.

**Test baseline at audit end.** 1106 passing (+11 model-layer tests).

---

## Findings

### F1 — `now_ms()` uses `.expect()` in library code   ⚠️  fixed in this PR

`drevo-rust` §"Error Handling" — *"Never `unwrap()` / `expect()` in library code — only in tests and benchmarks."* `drevo-architecture` anti-pattern #5 — same.

**Before.** [src/model.rs:209-214](src/model.rs:209) called `.expect("system clock before Unix epoch")` on `SystemTime::now().duration_since(UNIX_EPOCH)`. Even though a pre-epoch system clock is functionally absurd, the rule is unconditional: library code is not allowed to panic on a programming-environment condition that the host can in principle produce.

**After.** `now_ms()` is now total — it pattern-matches on the `Result` and returns a negative offset (`-err.duration().as_millis() as i64`) when the host clock is set before the epoch. The doc-comment documents the new contract:

> Total function — never panics. If the host clock is set before the Unix epoch (`SystemTime::now() < UNIX_EPOCH`), the timestamp is returned as a negative value reflecting the offset before the epoch.

This is called transitively from `NewNode::into_node`, `NewEdge::into_edge`, and `Node::apply_patch` — all three become panic-free as a side-effect.

**Test added.** `now_ms_is_total_and_does_not_panic` — calls `now_ms()` 32 times under the test runner. The behavioural test for the pre-epoch branch would require mocking `SystemTime`, which is not in the dependency tree; the totality is therefore asserted at the type level (return type is `i64`, no `?`/panic-propagating control flow) and at the runtime level via the explicit `match` on the `Err` arm.

### F2 — `Properties` bincode serialization is non-deterministic across `HashMap` iteration order   ⚠️  fixed in this PR

`drevo-rust` §"Serialization" — *"Internal data (KV store): `bincode v2` — compact, fast, deterministic."* `drevo-database` §"Serialization" — same line.

**Before.** `Properties` wraps `HashMap<String, serde_json::Value>` and serialized via `serde_json::to_vec(&self.0)`. `serde_json` calls the HashMap's own serializer, which iterates in the random order chosen by the default hasher. Two semantically-identical `Properties` therefore produced **different** bincode byte sequences:

```text
left:  {"delta":{"nested":true},"gamma":[3,4,5],"beta":"two","alpha":1}
right: {"delta":{"nested":true},"beta":"two","alpha":1,"gamma":[3,4,5]}
```

This was reproduced by the new `properties_bincode_is_deterministic_across_insertion_order` test (RED phase) — see commit history.

Correctness wasn't affected (round-trip still works), but the contract advertised by both skill specs was. Any downstream feature that hashes node bytes for checksums, replication, or change detection (Phase 13 MVCC `00080`, Phase 15 replication `00099`) would have observed false-diff churn.

**After.** `Properties::serialize` now collects into a `BTreeMap<&String, &serde_json::Value>` before delegating to either `serde_json::to_vec` (binary path) or the outer serializer (human-readable JSON path). Inner `serde_json::Map` values are already BTreeMap-backed by default (no `preserve_order` feature), so the whole `Node`/`Edge` bincode encoding is now byte-stable for a given logical content.

**Tests added.**

- `properties_bincode_is_deterministic_across_insertion_order` — two `Properties` built from the same `&[(key, value)]` pairs in different insertion orders must produce byte-identical bincode.
- `node_bincode_is_deterministic_across_property_insertion_order` — same guarantee at the `Node` level (whole-struct encoding).

**No public API break.** `Properties` storage stays `HashMap<String, _>` (O(1) lookups, per `drevo-database` §"Data Model"). Sorting happens only at serialize time.

### F3 — UUID immutability invariant relies on caller discipline, not encapsulation   ❌  flagged for `00106`

`drevo-database` §"Invariants" #4 — *"UUID immutability: once assigned, a node's UUID never changes (even on update)."*

`Node::uuid` and `Edge::uuid` are `pub` fields ([src/model.rs:71](src/model.rs:71), [src/model.rs:126](src/model.rs:126)). The model layer cannot enforce immutability — any caller holding `&mut Node` can do `node.uuid = …`. The current enforcement is purely by convention: `apply_patch` doesn't touch the UUID, and `Drevo::update_node` (the only place a Node escapes/re-enters storage) re-reads the existing UUID rather than accepting one from the patch.

**No refactor in this task.** Encapsulating UUID behind a getter (`fn uuid(&self) -> [u8; 16]`) would force every public-field consumer — HTTP serializers in `api.rs`, FFI in `ffi.rs`, WASM in `wasm.rs`, and the entire test suite — to switch to method calls. The right scope for that change is `00106` (DB core audit), which audits `Drevo::update_node` and the index-maintenance paths directly.

**Cross-link.** Phase 8.5 task `00106` "DB core audit" must verify the UUID-immutability invariant at the *enforcement* point (`update_node`) and decide whether encapsulation is required or whether a debug-assert in the write path is enough.

### F4 — `Edge` weight is `f32` and admits NaN/±Inf without validation   ❌  flagged for `00106`

`drevo-database` §"Edge" — *"weight: f32 — default 1.0, used for ranking / Dijkstra."* The model defines the type; the data-model spec does not mandate finiteness. But:

- `Edge` derives `PartialEq` ([src/model.rs:121](src/model.rs:121)). `f32::NAN != f32::NAN` — so an `Edge { weight: NaN, .. }` is never equal to itself. The model's own equality contract degrades silently.
- `traversal.rs` already acknowledges the hazard at [src/traversal.rs:199](src/traversal.rs:199) — *"Use `total_cmp` for NaN safety."* — but that's a downstream patch, not a model-layer guard.

**No refactor in this task.** Two options exist: (a) validate at construction (`NewEdge::into_edge` rejects non-finite weights with a new error variant) or (b) sanitise (`weight.is_finite() ? weight : 1.0`). Both touch the error hierarchy and the `Drevo::create_edge` write path, which is `00106` scope. Recording the gap here so `00106` picks it up.

**Cross-link.** Phase 8.5 task `00106` must decide between (a) and (b) and land the guard in `create_edge` / `update_edge`.

### F5 — Unicode coverage on the model layer was incomplete   ⚠️  fixed in this PR

`drevo-tdd` §"Coverage Targets" — *"Edge cases mandatory: empty graph, single node, cycles, disconnected components, depth 0, max depth, self-loops, parallel edges, Unicode (CJK, emoji, Cyrillic)."*

**Before.** `model.rs` had serialization tests but only with ASCII content. The `node_with_complex_properties_serializes` test exercised nested JSON, booleans, and nulls, but no non-ASCII scripts.

**After.** Five new Unicode tests, all roundtripping both bincode and serde_json paths:

| Test | Coverage |
|------|----------|
| `node_with_cjk_content_serializes_roundtrip` | Chinese / Japanese / Korean in `kind`, `title`, `body` |
| `node_with_emoji_content_serializes_roundtrip` | Single emoji + ZWJ family sequence (👨‍👩‍👧‍👦) |
| `node_with_cyrillic_content_serializes_roundtrip` | Russian in `kind`, `title`, `body` |
| `properties_with_unicode_keys_and_values_roundtrip` | Non-ASCII property keys + values across all three scripts |
| `edge_with_cyrillic_kind_serializes_roundtrip` | Non-ASCII `kind` on `Edge` (mirrors Cypher relationship type) |

These pin the bincode + JSON paths against UTF-8 boundary regressions if either serializer is ever swapped.

### F6 — Every `pub` item carries rustdoc   ✅  compliant

`drevo-rust` §"Code Style" — *"Doc-comments on every `pub` item."*

Verified by inspection of `src/model.rs`: every `pub struct`, `pub enum`, `pub fn`, and `pub` field carries `///` documentation, including the `Direction` variants ([src/model.rs:172-176](src/model.rs:172)). `cargo doc --no-deps -- -D missing_docs` succeeds against `src/model.rs` items.

### F7 — Indentation ≤ 3 levels per function   ✅  compliant

`drevo-rust` §"Code Style". The heaviest function is `Properties::serialize` (one `if/else` with a single nested expression on each arm — level 2). `now_ms()` is level 2 (the `match`). `apply_patch` is level 2 (the chain of `if let Some(…)`). None exceed 3.

### F8 — `bincode::config::standard()` is the only config in use   ✅  compliant

`drevo-rust` §"Serialization". Every encode/decode call in `model.rs` tests goes through `bincode::config::standard()` — there are no per-call ad-hoc configs that would diverge from the `MemoryBackend` / `RedbBackend` byte format. (The storage backends themselves use the same constant — verified by the `00103` audit.)

### F9 — `Direction` is fully closed and substitutable   ✅  compliant

`drevo-architecture` §SOLID "L" — Liskov substitution. `Direction` is a closed `#[derive(Copy)]` enum; consumers in `db.rs::edges_of` exhaustively match it ([src/db.rs:571-580](src/db.rs:571)). No `Direction::Other(String)` escape hatch. The HTTP edge converts a query-string back to it via a hand-written `parse_direction` ([src/api.rs:700](src/api.rs:700)) — that mapping is `00109` scope.

### F10 — `NewNode` / `NewEdge` lack the builder pattern shown in `drevo-architecture` §"Builder Pattern"   ❌  deferred indefinitely

The skill spec shows:

```rust
NewNode::builder()
    .kind("note")
    .title("Hello")
    .property("tag", "intro")
    .build()
```

`drevo-architecture` describes this as a pattern *used* in the codebase, but it isn't — `NewNode` is constructed via struct-literal syntax across every test, scenario, and FFI consumer. There are two ways to resolve this drift:

- (a) **Build the builder.** Adds ~80 LOC plus migration of every call site. Pure ergonomics — no semantic gain. YAGNI per `drevo-architecture` anti-pattern #9 ("Over-Engineering"). The struct-literal form is short enough today.
- (b) **Soften the skill spec.** Re-word the architecture skill to describe the builder as a candidate pattern for the future Cypher query-plan builder rather than `NewNode`.

**Recommendation.** Option (b). The architecture skill should mention `NewNode::builder` as an *option*, not as a documented codebase fact. This audit does not modify the skill spec (cross-cutting `00113` scope) but flags the divergence so `00113` can normalise it.

### F11 — `NodePatch` / `EdgePatch` derive `Default` but not `PartialEq`   ✅  compliant (no behavioural impact)

Both structs derive `Default` (so `..Default::default()` is the canonical empty-patch idiom in tests). They do not derive `PartialEq`, but no test or production path compares two `*Patch` values — a `Patch` is consumed by `apply_patch` and dropped. Adding `PartialEq` would be additive and harmless but not motivated by a rule. Skipping.

### F12 — `kind: String` is stringly-typed, but the `kind`-typed alternative is out of scope for the model audit   ✅  compliant-by-design

`drevo-architecture` anti-pattern #3 ("Stringly Typed") suggests `NodeKind` over `&str`. The skill spec also says (line 40):

> Cypher analogue: `kind` ≈ label (for nodes) or relationship type (for edges); `properties` ≈ Cypher property map.

Cypher labels are dynamic — they cannot be a closed enum in the storage engine. The current `String` form is correct for a graph database that admits user-defined kinds. The anti-pattern applies to *internal* APIs ("query language tokens, filter operators"), not the user-facing `kind` field. ✅ compliant.

---

## Refactor PRs landed in this audit

1. **F1**: `now_ms()` is total — replaced `.expect("system clock before Unix epoch")` with a sign-preserving `match`. Updated rustdoc to document the new contract. [src/model.rs:209-219](src/model.rs:209).
2. **F2**: `Properties::serialize` sorts via `BTreeMap` before emitting bytes — bincode output is now deterministic across HashMap iteration order. [src/model.rs:37-52](src/model.rs:37).
3. **F5**: Five new Unicode roundtrip tests (CJK + emoji + Cyrillic across `Node`, `Edge`, `Properties`). [src/model.rs:621-790](src/model.rs:621).
4. **F2 (tests)**: `properties_bincode_is_deterministic_across_insertion_order`, `node_bincode_is_deterministic_across_property_insertion_order` — pin the new determinism guarantee.
5. **F1 (test)**: `now_ms_is_total_and_does_not_panic` — runtime smoke test for the totality fix.

## Refactor PRs deferred (cross-linked)

- **F3** UUID-immutability encapsulation: owned by Phase 8.5 task `00106` (DB core audit) — the enforcement point is `Drevo::update_node`, not the model struct.
- **F4** Non-finite `Edge::weight`: owned by Phase 8.5 task `00106` — guard belongs in `create_edge`/`update_edge`, plus a new error variant in the hierarchy owned by `00104` (already landed) or a follow-up.
- **F10** `NewNode::builder` skill divergence: owned by Phase 8.5 task `00113` (cross-cutting audit) — should re-word `drevo-architecture` rather than build an unused builder.

## Definition of done — task `00105`

- ✅ `audit/AUDIT-model.md` exists, every cited rule has a verdict.
- ✅ Test baseline grows: 1095 → 1106 (+11 tests: 2 determinism, 1 `now_ms` totality, 5 Unicode roundtrips, 2 misc helper tests covered by Unicode set).
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` clean.
- ✅ `cargo check --target wasm32-unknown-unknown --no-default-features --features wasm` clean.
- ✅ `cargo fmt --check` clean.
- ✅ No public API breakage (`Properties` still wraps `HashMap`; `now_ms()` signature unchanged).
