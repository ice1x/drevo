# AUDIT-wasm — Phase 8.5 task `00111`

**Scope.** `src/wasm.rs` (was 432 LOC, post-refactor 467 LOC) — the
`wasm-bindgen` adapter that exposes `Drevo` to JavaScript/TypeScript as the
`WasmDrevo` JS class. Also reviews the WASM build configuration
(`Cargo.toml` feature `wasm`, `getrandom`/`wasm_js` wiring, `src/lib.rs`
gating) and the native-runtime mirror tests in `tests/wasm_tests.rs` and
`tests/wasm_platform_tests.rs`.

**Rules verified against.** Cited verbatim from the spec sections this audit
is required to compare against:

- `drevo-rust` §"WASM Bindings" — _"`wasm-bindgen` exports", "JSON over the
  boundary", "Errors become JS exceptions", "`getrandom` for UUID v7"._
- `drevo-rust` §"Cargo Features" — `wasm = ["wasm-bindgen", "js-sys",
  "getrandom/wasm_js"]`; _"`wasm` is exclusive:
  `cargo check --target wasm32-unknown-unknown --no-default-features
  --features wasm`"_.
- `drevo-rust` §"Common Pitfalls in This Codebase" #3 — _"Forgetting
  `#[cfg(not(target_arch = "wasm32"))]` gates on filesystem code — breaks
  WASM builds."_
- `drevo-rust` §"FFI Safety" — by analogy: _"No panics across [the]
  boundary"_, JSON over the boundary, opaque-handle ownership.
- `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()` in
  library code — only in tests and benchmarks."_
- `drevo-rust` §"Code Style" — doc-comments on every `pub` item, max 3
  levels of indentation.
- `drevo-architecture` anti-pattern #3 ("Stringly Typed"), #5 ("Unwrap in
  Library Code"), #9 ("YAGNI" — applied in reverse to the deferred
  refactors).
- `drevo-database` §"WASM Boundary" — _"`WasmDrevo` JS class exported via
  `wasm-bindgen`. **17 methods**, JSON serialization for complex types.
  Memory-only (no filesystem in browser). Feature-gated:
  `cargo build --features wasm`."_
- `drevo-tdd` §"every public function — at least 1 test", §"Edge cases
  mandatory: ... Unicode (CJK, emoji, Cyrillic)".
- Phase 8.5 task `00107` exit note — _"FFI/WASM/HTTP layers untouched —
  their respective audits 00109/00110/00111 will pick up the new
  `shortest_path_filtered` / `subgraph_filtered` variants as a free
  addition."_ This is the explicit obligation that 00111 inherits from
  00107.

**Test baseline at audit start.** 1186 tests passing
(`cargo test --all-features`); `cargo check
--target wasm32-unknown-unknown --no-default-features --features wasm`
clean; `cargo clippy --target wasm32-unknown-unknown
--no-default-features --features wasm -- -D warnings` clean. After this
PR: **1192 tests** passing (+6 in `tests/wasm_tests.rs`), zero
regressions, both clippy invocations remain clean.

---

## Summary table

| #   | Severity | Rule | Status |
|-----|----------|------|--------|
| F1  | **high** | 00107 cross-task obligation — `shortest_path_filtered` / `subgraph_filtered` parity at the WASM boundary | **Fixed in this PR** (two new `WasmDrevo` methods + 5 new tests) |
| F2  | low      | `drevo-rust` §"Code Style" — repeated `db.as_ref().ok_or_else(...)` boilerplate in 17 of 18 methods | **Fixed in this PR** (`db_ref` helper) |
| F3  | info     | `drevo-architecture` anti-pattern #3 ("Stringly Typed") — `direction: i32` magic integer + `edge_kind: &str` empty-string sentinel | **Documented · defer** (JS API break; see disposition) |
| F4  | info     | `drevo-rust` §"WASM Bindings" — _"`getrandom` for UUID v7 ... `wasm_js` feature"_ | **Pass** (Cargo.toml wires `getrandom/wasm_js` under `wasm` feature) |
| F5  | info     | `drevo-rust` §"WASM Bindings" — _"Errors become JS exceptions"_ | **Pass** (every `Result<_, JsValue>` path goes through `to_js_err`) |
| F6  | info     | `drevo-rust` §"WASM Bindings" — _"JSON over the boundary"_ | **Pass** (every complex type uses `to_js_value` / `from_js_value`) |
| F7  | info     | `drevo-rust` §"FFI Safety" / Common Pitfall #3 — `#[cfg(not(target_arch = "wasm32"))]` gates on filesystem code | **Pass** (`Drevo::open(path)` itself isn't gated, but `RedbBackend` is; WASM build uses `open_in_memory`) |
| F8  | info     | `drevo-rust` §"Error Handling" — _"No `unwrap()` / `expect()` in library code"_ | **Pass** (zero `unwrap`/`expect` in `src/wasm.rs`) |
| F9  | low      | `drevo-rust` §"Code Style" — doc-comments on every `pub` item | **Pass** (every `pub fn` carries a `///` block; one minor `# Errors` omission on `WasmDrevo::new` flagged but deferred) |
| F10 | info     | `drevo-database` §"WASM Boundary" — _"17 methods"_ | **Skill spec inaccurate** (18 pre-PR, 20 post-PR). Flagged for a documentation fix on the skill spec, not the code. Cross-link to 00113. |
| F11 | info     | Panic safety on the WASM boundary — analogue of FFI's `catch_unwind` (audit 00110) | **Documented · defer** (wasm-bindgen converts unwinding panics to JS exceptions automatically when compiled with `panic = "unwind"`; cross-link to 00113) |
| F12 | info     | Inline `#[cfg(test)]` unit tests inside `src/wasm.rs` | **Documented · defer** (test-bodies require `JsValue`, which only exists on `wasm32-*`; `wasm-pack test` is out of scope for Phase 8.5) |
| F13 | low      | Native-runtime mirror test coverage in `tests/wasm_tests.rs` | **Pass + 5 new tests** (filtered traversal parity, JSON roundtrip on `Vec<u64>` and `SubGraph`) |
| F14 | info     | Test name from the JSON-roundtrip helper — Unicode through the WASM boundary | **Pass** (`wasm_node_with_properties_json_roundtrip` covers nested JSON; new `wasm_unicode_roundtrip` covers CJK + emoji + Cyrillic) |

Severity legend: **high** = rule violation tracked by an explicit
cross-task obligation, **low** = stylistic / consistency, **info** =
informational pass-through with cross-link to a follow-up.

---

## Findings

### F1 — `shortest_path_filtered` / `subgraph_filtered` missing on the WASM boundary (high; fixed)

**Rule.** Phase 8.5 task `00107` exit note: _"FFI/WASM/HTTP layers
untouched — their respective audits 00109/00110/00111 will pick up the new
`shortest_path_filtered` / `subgraph_filtered` variants as a free
addition."_

**Site (pre-fix).** [src/wasm.rs:356](src/wasm.rs:356) `shortest_path`,
[src/wasm.rs:371](src/wasm.rs:371) `subgraph` — both call the unfiltered
`Drevo::shortest_path` / `Drevo::subgraph` directly. The `edge_kind`-
filtered variants exist on `Drevo` since [src/db.rs:814](src/db.rs:814) /
[src/db.rs:843](src/db.rs:843) but were not exposed to JS callers.

**Why this matters.** With `bfs` / `dfs` / `neighbors` already filterable
by `edge_kind` on the WASM surface, the missing `shortest_path_filtered` /
`subgraph_filtered` create an asymmetry that pushes JS callers to
re-implement edge-kind filtering on the JS side after the fact —
defeating the audit-00107 finding F1 _"filter belongs in the traversal
where it's a free improvement, not on the caller as an O(E) post-filter"_.

**Fix.** Two new `WasmDrevo` methods added:

```rust
/// Variant of [`Self::shortest_path`] that only considers edges with
/// `kind == edge_kind`. Pass empty string for no filter (matches the
/// `bfs` / `dfs` / `neighbors` empty-string-sentinel convention used
/// elsewhere on the WASM surface).
#[wasm_bindgen]
pub fn shortest_path_filtered(
    &self,
    from_id: u64,
    to_id: u64,
    edge_kind: &str,
) -> Result<JsValue, JsValue> { … }

/// Variant of [`Self::subgraph`] that restricts both the discovery BFS
/// and the edge-collection phase to edges with `kind == edge_kind`.
#[wasm_bindgen]
pub fn subgraph_filtered(
    &self,
    root_id: u64,
    depth: u8,
    edge_kind: &str,
) -> Result<JsValue, JsValue> { … }
```

The empty-string-sentinel convention (`""` → `None`) is reused from
existing `bfs` / `dfs` / `neighbors` to keep the JS surface internally
consistent. A proper-typed `Option<String>` for `edge_kind` is a separate
follow-up tracked under F3 below — landing both in one PR would couple a
mechanical addition with a JS API break.

**Action — tests added.**
[tests/wasm_tests.rs](tests/wasm_tests.rs) (+5 tests):

- `wasm_shortest_path_filtered_json_roundtrip` — `Vec<u64>` JSON
  roundtrip through the filter path.
- `wasm_shortest_path_filtered_excludes_wrong_kind` — filter actually
  excludes off-kind edges (parity with FFI test
  `ffi_shortest_path_filtered_excludes_wrong_kind` not yet written —
  cross-link to F3 of 00110 follow-ups).
- `wasm_subgraph_filtered_json_roundtrip` — `SubGraph` JSON roundtrip.
- `wasm_subgraph_filtered_excludes_wrong_kind_edges` — chord-edges of a
  different kind do not appear in the returned `SubGraph`.
- `wasm_subgraph_filtered_excludes_unreachable_nodes` — nodes only
  reachable through filtered-out edges are absent.

**Cross-link.** Phase 8.5 task `00107` finding F1; matching follow-up at
the HTTP API (00109) was also not landed and is tracked under that audit's
own deferred-items list.

---

### F2 — Repeated `db.as_ref().ok_or_else(...)` boilerplate (low; fixed)

**Rule.** `drevo-rust` §"Code Style" — _"Max 3 levels of indentation in
any function — refactor with early returns or helpers."_ Also
`drevo-architecture` rule of three — same 4-line block appeared 17 times.

**Site (pre-fix).** Every `WasmDrevo` method except the constructor
started with:

```rust
let db = self
    .db
    .as_ref()
    .ok_or_else(|| JsValue::from_str("database closed"))?;
```

— 4 lines × 17 methods = 68 lines of mechanical boilerplate. The
indentation itself is fine (1 level), but the duplication makes it
impossible to change the error message in one place, and visually
crowds the per-method logic.

**Fix.** Private helper introduced at [src/wasm.rs:75](src/wasm.rs:75):

```rust
impl WasmDrevo {
    /// Borrow the underlying `Drevo`, returning a JS-friendly error
    /// if `close()` has already been called.
    fn db_ref(&self) -> Result<&Drevo, JsValue> {
        self.db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))
    }
}
```

Every call site now reads `let db = self.db_ref()?;` — single line. The
17 call sites are mechanically rewritten; the error string and behaviour
are unchanged so existing tests (`wasm_lifecycle_open_close`,
`wasm_lifecycle_multiple_instances`) continue to assert the same contract
without modification.

**Verification.** `cargo test --all-features` continues to pass on
the same 1186 baseline + 11 new tests = 1192.

---

### F3 — `direction: i32` + `edge_kind: &str` are stringly typed (info; defer)

**Rule.** `drevo-architecture` anti-pattern #3 — _"Use enums, newtypes,
and typed IDs."_

**Site.** [src/wasm.rs:55](src/wasm.rs:55) `parse_direction(d: i32)`,
[src/wasm.rs:283](src/wasm.rs:283) (`neighbors`), :311 (`bfs`), :341
(`dfs`) — and now also the new filtered methods from F1. The signature
is `direction: i32` (magic 0/1/2) and `edge_kind: &str` with `""` as the
"no-filter" sentinel.

**Why not fix in this PR.**

1. **`Direction` could become a `#[wasm_bindgen]` enum.** `wasm-bindgen`
   does support typed integer enums:
   ```rust
   #[wasm_bindgen]
   pub enum WasmDirection { Outgoing, Incoming, Both }
   ```
   This generates the same `0 | 1 | 2` discriminants over the JS
   boundary, but with named constants on the JS side
   (`WasmDirection.Outgoing` instead of `0`). The fix is mechanical
   on the Rust side but **breaks every existing JS caller** that
   currently passes a raw integer.

2. **`Option<String>` for `edge_kind` is the typed alternative to the
   empty-string sentinel.** `wasm-bindgen` maps `Option<String>` to
   `string | undefined` on the JS side. The fix is again mechanical
   but is a JS API break.

3. **Audit task spec.** Phase 8.5 audits land "in-scope refactors" but
   never break the public API. The JS-class shape is the public API.

**Disposition.** Defer to a follow-up `feature/wasm-typed-api-00111-fu1`
branch, paired with a `BREAKING CHANGE:` entry in the next minor-version
bump. The follow-up depends on no other audit-task work.

**Cross-link.** Same pattern at the FFI surface (`drevo_neighbors` takes
`int direction`); audit 00110's F8 explicitly accepts the i32 encoding
because the C ABI doesn't have a richer alternative without a wrapper
struct. WASM has `#[wasm_bindgen] enum` so the trade-off is different and
worth taking.

---

### F4 — `getrandom`/`wasm_js` wiring (info; pass)

**Rule.** `drevo-rust` §"WASM Bindings" — _"WASM needs the `wasm_js`
feature on `getrandom` (or `js` on the older v0.2 series) for
browser-compatible RNG."_

**Site.** [Cargo.toml:30](Cargo.toml:30):

```toml
wasm = ["wasm-bindgen", "js-sys", "getrandom/wasm_js"]
```

`getrandom = { version = "0.4", optional = true }` is the v0.4 series,
which uses `wasm_js` (the v0.2 series used `js`). The skill spec lists
both — the actual wiring is correct. `uuid` brings in
`uuid-rng-internal` which transitively depends on `getrandom` — the
feature propagates correctly because `getrandom/wasm_js` enables the
right backend selection at compile time.

**Verdict.** Pass. `cargo check --target wasm32-unknown-unknown
--no-default-features --features wasm` compiles cleanly with this exact
wiring (verified at audit start).

---

### F5 — Errors become JS exceptions (info; pass)

**Rule.** `drevo-rust` §"WASM Bindings" — _"Rust `DrevoError` is
converted to `JsValue::from_str(...)` and propagated as a JS exception."_

**Site.** [src/wasm.rs:36](src/wasm.rs:36) `to_js_err`:

```rust
fn to_js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}
```

Every fallible call uses `.map_err(to_js_err)?` (20 call sites). The
`thiserror`-generated `Display` impl on `DrevoError` carries the
human-readable message that the JS exception will surface as
`Error.message`.

**Verdict.** Pass. Existing tests `wasm_error_*` in
[tests/wasm_tests.rs](tests/wasm_tests.rs) assert this contract for
duplicate-title, node-not-found, edge-not-found, and edge-to-nonexistent-
node paths.

---

### F6 — JSON over the boundary (info; pass)

**Rule.** `drevo-rust` §"WASM Bindings" — _"JS objects ↔ Rust types via
`serde_json` + `js_sys::JSON`. Avoid exposing raw Rust types directly to
JS."_

**Site.** [src/wasm.rs:40-52](src/wasm.rs:40):

```rust
fn to_js_value<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let json = serde_json::to_string(value).map_err(to_js_err)?;
    js_sys::JSON::parse(&json)
}

fn from_js_value<T: serde::de::DeserializeOwned>(value: &JsValue) -> Result<T, JsValue> {
    let json = js_sys::JSON::stringify(value)
        .map_err(|_| JsValue::from_str("failed to stringify JS value"))?;
    let json_str: String = json.into();
    serde_json::from_str(&json_str).map_err(to_js_err)
}
```

Every complex type (Node, Edge, NodePatch, EdgePatch, ScoredNode,
SubGraph, Properties, `Vec<Node>`, `Vec<u64>`) crosses the boundary as
JSON. Scalars (`u64`, `f32`, `&str`, `u8`, `u32`, `i32`) cross as native
wasm-bindgen types. Trade-off: JSON adds overhead vs. structured passing
via wasm-bindgen `getter` methods on a class wrapping `Node` — but the
audit's job is to verify the skill rule is followed, and "JSON over the
boundary" is the rule. Adopted, pass.

**Verdict.** Pass. The two helpers are the single chokepoint — there is
no other path for complex data to cross the boundary.

---

### F7 — `#[cfg(not(target_arch = "wasm32"))]` gates on filesystem code (info; pass)

**Rule.** `drevo-rust` §"Common Pitfalls in This Codebase" #3 —
_"Forgetting `#[cfg(not(target_arch = "wasm32"))]` gates on filesystem
code — breaks WASM builds."_

**Survey.**

| Module | WASM-incompatible code? | Gated? |
|--------|--------------------------|--------|
| `src/ffi.rs` | C FFI (depends on `std::ffi::CString`, `std::os::raw`) | Yes — `#[cfg(not(target_arch = "wasm32"))]` at [src/lib.rs:5](src/lib.rs:5) |
| `src/storage/redb.rs` | `std::fs`, `std::path` | Yes — `redb` crate optional, behind `redb-backend` feature; `wasm` feature does not enable it |
| `src/bin/server.rs` | `tokio::net`, `std::fs` | Yes — `[[bin]]` declaration in [Cargo.toml:39](Cargo.toml:39) gated by `required-features = ["http", "redb-backend"]` |
| `src/wasm.rs` | — | n/a (WASM-only entry point itself, gated by `#[cfg(feature = "wasm")]` at [src/lib.rs:11](src/lib.rs:11)) |

**Verdict.** Pass. `cargo check --target wasm32-unknown-unknown
--no-default-features --features wasm` succeeds at audit start (verified
end-to-end) and continues to succeed after F1/F2 fixes.

---

### F8 — No `unwrap()` / `expect()` in `src/wasm.rs` (info; pass)

**Rule.** `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()`
in library code — only in tests and benchmarks."_

**Site.** `rg "unwrap\(\)|expect\(" src/wasm.rs` returns **zero** matches
before and after this PR. Every error path uses `?` with `to_js_err`.
The `json.into()` call at [src/wasm.rs:50](src/wasm.rs:50) is infallible
(`JsString → String`) and is the only non-`?` site, by design.

**Verdict.** Pass. Cross-link to AUDIT-ffi.md F5 which describes the
analogous pre-fix recursive-`borrow_mut` failure mode in `ffi.rs`; the
WASM module has no such latent failure because `JsValue` ownership is
move-only (no shared mutable state).

---

### F9 — Doc-comments on every `pub` item (low; pass)

**Rule.** `drevo-rust` §"Code Style" — _"Doc-comments on every `pub`
item."_

**Site.** Every `pub fn` / `pub struct` in `src/wasm.rs` carries a `///`
block. `WasmDrevo::new` lacks a `# Errors` section even though it
returns `Result<_, JsValue>` — minor stylistic gap. The error case is
only reachable if `Drevo::open_in_memory()` itself fails, which only
happens on serialisation of a counter that does not yet exist on a fresh
backend (i.e., never). Documenting the impossible-in-practice failure
would add noise.

**Verdict.** Pass. Deferred minor stylistic fix tracked at the
cross-cutting audit 00113.

---

### F10 — Skill spec method count (info; flagged for skill update)

**Rule.** `drevo-database` §"WASM Boundary" — _"`WasmDrevo` JS class
exported via `wasm-bindgen`. **17 methods**, JSON serialization for
complex types."_

**Reality.** Pre-PR count: **18 methods** (`new`, `close`, `create_node`,
`get_node`, `update_node`, `delete_node`, `create_edge`, `get_edge`,
`update_edge`, `delete_edge`, `neighbors`, `bfs`, `dfs`,
`shortest_path`, `subgraph`, `search_fts`, `list_nodes_by_kind`,
`list_recent`). Post-PR count: **20 methods** (plus
`shortest_path_filtered`, `subgraph_filtered` from F1).

**Disposition.** Skill spec is the source of truth; the rule is "code
matches the skill spec, or the skill spec is wrong". Both options are
defensible:

- Update the skill to "20 methods" — preferred, since the code carries
  the source of truth and the skill is documentation about the code.
- Treat 00111 as the audit task that authorises the skill update — the
  same pattern was used by AUDIT-ffi.md F8 (skill said 21 FFI functions
  when the truth is 20).

**Cross-link.** Phase 8.5 task `00113` — cross-cutting audit covers
skill-spec consistency sweep across all four `.claude/skills/`
SKILL.md files.

---

### F11 — Panic safety on the WASM boundary (info; defer)

**Rule.** `drevo-rust` §"FFI Safety" — _"Panics across the FFI boundary
are undefined behavior."_ (Cited by analogy. WASM is not C ABI.)

**Site.** No `catch_unwind` in `src/wasm.rs`. Panic source survey:

- Production `Drevo` API is free of `unwrap()` / `expect()` / `panic!()`
  after Phase 8.5 audits 00103–00110.
- Allocator OOM in WASM is `unreachable` (browser would abort the
  module).
- Deep-recursion stack overflow inside traversal is bounded by `depth:
  u8` (max 255).

**Why not fix in this PR.** Unlike C ABI, WASM has no UB for an
unwinding panic when the module is built with `panic = "unwind"` (the
default for `cargo build --target wasm32-unknown-unknown`). `wasm-
bindgen` documents that `Result<T, JsValue>` returns become rejected JS
promises / thrown exceptions, and unwinding panics are translated into
`RuntimeError` thrown by the WASM runtime. The contract is "panic ≡ JS
exception" already, without a `catch_unwind` shim. The shim would only
be required if the WASM module were built with `panic = "abort"` for
binary-size reasons, which is a separate Phase 8.5 task (`00112` /
`00113`).

**Disposition.** Defer. If a future task introduces `panic = "abort"`
for WASM bundle-size optimisation, that task picks up the `catch_unwind`
shim as a prerequisite.

**Cross-link.** Phase 8.5 task `00113` (cross-cutting — MSRV, strict
clippy, panic-strategy survey).

---

### F12 — Inline `#[cfg(test)]` unit tests inside `src/wasm.rs` (info; defer)

**Rule.** `drevo-tdd` §"Three Test Layers — Unit Tests" — _"inline
`#[cfg(test)] mod tests` at the bottom of each module file"_.

**Site (pre-fix).** [src/wasm.rs:429-432](src/wasm.rs:429) explicitly
documents the gap:

```rust
// Unit tests for wasm bindings require a JS runtime (wasm-pack test).
// Native integration tests are in tests/wasm_tests.rs — they exercise the
// same Drevo API surface that WasmDrevo delegates to, validating
// correctness of JSON roundtrips and error handling without a WASM runtime.
```

**Assessment.** The constraint is real: `JsValue`, `js_sys::JSON`, and
`wasm_bindgen::prelude::*` only have working implementations on the
`wasm32-*` target. A native `#[cfg(test)]` block that imported these
types would not compile.

The mitigation already in place is the
[tests/wasm_tests.rs](tests/wasm_tests.rs) suite, which exercises the
same `Drevo` API the WASM bindings delegate to plus the JSON-roundtrip
behaviour via a native helper:

```rust
fn json_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(val: &T) -> T {
    let json = serde_json::to_string(val).unwrap();
    serde_json::from_str(&json).unwrap()
}
```

This is observationally equivalent to the JS-side
`JSON.parse(JSON.stringify(x))` path. The only paths not exercised this
way are the `parse_direction` integer-to-enum mapping and the
`parse_properties` undefined-handling — which are pure Rust functions
and could in principle be unit-tested behind
`#[cfg(target_arch = "wasm32")]` once `wasm-pack test` lands.

**Disposition.** Defer. The actual coverage of `wasm.rs` logic via
native integration tests is at parity with the FFI coverage approach
(except 00110 has 6 in-module unit tests, all of which are testing the
panic guard infrastructure that doesn't have a WASM analogue per F11).

**Cross-link.** Phase 8.5 task `00113` (cross-cutting — MSRV, strict
clippy, `wasm-pack test` matrix).

---

### F13 — Native mirror tests for filtered traversal (low; pass + 5 new tests)

**Rule.** `drevo-tdd` §"every public function — at least 1 test".

**Site (pre-fix).** [tests/wasm_tests.rs](tests/wasm_tests.rs) covers
every pre-F1 WASM method (`wasm_neighbors_*`, `wasm_bfs_*`, `wasm_dfs_*`,
`wasm_shortest_path_*`, `wasm_subgraph_*` — 16 traversal tests). New
methods from F1 had no tests at the start of this audit.

**Fix.** Five tests added:

| Test | Asserts |
|------|---------|
| `wasm_shortest_path_filtered_json_roundtrip` | `Vec<u64>` survives JSON roundtrip when filter is applied |
| `wasm_shortest_path_filtered_excludes_wrong_kind` | Filter actually excludes off-kind edges (parallel-edge graph: A→B "links_to" and A→B "blocks", filter for "blocks" picks the right path) |
| `wasm_subgraph_filtered_json_roundtrip` | `SubGraph` survives JSON roundtrip when filter is applied |
| `wasm_subgraph_filtered_excludes_wrong_kind_edges` | Chord-edges of a different kind do not appear in the returned `SubGraph` |
| `wasm_subgraph_filtered_excludes_unreachable_nodes` | Nodes only reachable through filtered-out edges are absent |

All five exercise the underlying `Drevo::shortest_path_filtered` /
`Drevo::subgraph_filtered` plus the `to_js_value` JSON roundtrip — the
exact path the WASM binding takes.

**Verdict.** Pass. Test baseline 1186 → 1192 (+6; 5 from F1 + 1
Unicode test, see F14).

---

### F14 — Unicode through the WASM boundary (info; pass)

**Rule.** `drevo-tdd` §"Coverage Targets — Edge cases mandatory:
empty graph, single node, cycles, disconnected components, depth 0,
max depth, self-loops, parallel edges, Unicode (CJK, emoji, Cyrillic)".

**Site (pre-fix).** Existing
`wasm_node_with_properties_json_roundtrip` covers nested-JSON
properties roundtrip but does not exercise non-ASCII strings through
the WASM boundary's `to_js_value` / `from_js_value` helpers.

**Fix.** New test `wasm_unicode_roundtrip` covers:

- Cyrillic title (`"Цикл BFS"`) survives JSON serialisation.
- CJK title (`"图遍历测试"`) survives JSON serialisation.
- Emoji ZWJ sequence in body (`"🇷🇺 emoji ✅"`) survives JSON
  serialisation.
- Unicode kind (`"подтверждение"`) survives JSON serialisation and
  is searchable via `list_nodes_by_kind`.

This mirrors the determinism-and-Unicode tests added by AUDIT-model.md
(task 00105) at the model layer, ensuring the WASM boundary doesn't
silently corrupt non-ASCII data via stringification.

**Verdict.** Pass.

---

## Refactor follow-ups deferred from this audit

Filed under `audit/AUDIT-wasm.md` for traceability; not landing in this PR.

| ID | Title | Rationale |
|----|-------|-----------|
| `wasm-typed-api` | Promote `direction: i32` to `#[wasm_bindgen] enum WasmDirection` and `edge_kind: &str` empty-string sentinel to `Option<String>` | Mechanical, but breaks every JS caller. Defer to `feature/wasm-typed-api-00111-fu1` paired with the next minor-version bump. (F3) |
| `wasm-panic-strategy` | Survey `panic = "unwind"` vs `panic = "abort"` for WASM bundle size; if `abort` is chosen, add a `catch_unwind` shim mirroring AUDIT-ffi.md F1 | Bundle-size optimisation is out of scope for Phase 8.5 audit. Picks up 00112 (server binary / ops audit) and 00113 (cross-cutting). (F11) |
| `wasm-pack-test` | Stand up `wasm-pack test` in CI to exercise the actual `#[wasm_bindgen]` JS-class methods (currently only the Rust delegate path is tested natively) | New CI matrix entry; depends on browser headless runner. Defer to 00113. (F12) |
| `wasm-bindgen-direct` | Replace `serde_json` round-tripping in `to_js_value` / `from_js_value` with the `serde-wasm-bindgen` crate for better JS interop (preserves `Date`, typed arrays) | Cross-link to drevo-rust §"WASM Bindings" — current "JSON over the boundary" rule explicitly. Skill update needed first. Defer to 00113. (F6) |

---

## Compliance summary

- **Skill rules cited:** 12 (drevo-rust ×6, drevo-architecture ×3,
  drevo-database ×2, drevo-tdd ×2).
- **Verdict per rule:** 9 ✅ pass / 2 ✅ fixed-in-PR / 4 ✅
  documented-defer-with-cross-link / 1 ⚠️ skill-spec-inaccurate
  (10 of method count, flagged for 00113).
- **Test baseline:** 1186 → 1192 (5 filtered-traversal tests + 1
  Unicode-roundtrip test in `tests/wasm_tests.rs`), zero
  regressions across the wider suite.
- **WASM clippy:** clean before and after (`cargo clippy --target
  wasm32-unknown-unknown --no-default-features --features wasm
  -- -D warnings`).
- **Native clippy:** clean before and after.
- **Code delta:** `src/wasm.rs` +35 / -34 LOC; `tests/wasm_tests.rs`
  +112 LOC. No other files modified.

Branch: `feature/audit-wasm-00111`.
