# AUDIT-http-api — Phase 8.5 task 00109

**Scope.** `src/api.rs` (~767 LOC). The single-file HTTP adapter that lifts every
`Drevo` method onto an axum router, mapping `DrevoError` to HTTP status codes
and validating query-string / JSON-body input on the way in.

**Verification rules.** Cited verbatim from the spec sections this audit is
required to compare against:

- `drevo-database` §"HTTP API" — `DrevoError → HTTP status + JSON body { error, details }`, every endpoint listed in the spec REST table is present.
- `drevo-architecture` §"Anti-Patterns" #2 (Premature Abstraction), #3 (Stringly Typed), #5 (Unwrap in Library Code), #6 (Deep Nesting), #10 (Mixing Concerns in Match Arms).
- `drevo-rust` §"Error layering across boundaries" — _"Never let internal redb errors leak directly into HTTP responses."_
- `drevo-rust` §"Async / Tokio" — _"Don't make the public API async unless it actually awaits something."_
- `drevo-rust` §"Code Style" — `SCREAMING_SNAKE_CASE` constants, max 3 levels of indentation, doc-comments on every `pub` item.

**Outcome.** Module is in **good** shape relative to the rule set. One concrete
violation (magic numbers in list-limit clamping) is fixed in the same PR as the
audit. Two refactor targets called out by the README task description
(`#[deny(non_exhaustive_omitted_patterns)]` on the `ApiError` match,
generic CRUD handler trait) are **deliberately deferred** with rationale below
— they would currently violate the anti-patterns they claim to fix.

---

## Summary table

| # | Severity | Rule | Status |
|---|----------|------|--------|
| F1 | info | `drevo-architecture` anti-pattern #2 / #10 — handler duplication across node/edge CRUD | **Documented · defer** (premature abstraction; revisit at Phase 10 Cypher) |
| F2 | info | `drevo-rust` error layering — `DrevoError` → HTTP status is exhaustive | **Pass + regression test added** |
| F3 | low | `drevo-rust` §"Code Style" — magic-number `1000` cap for `limit` | **Fixed in this PR** (`MAX_LIST_LIMIT` constant) |
| F4 | info | `limit` / `offset` query-string overflow & saturation | **Pass + boundary tests added** |
| F5 | info | `depth: u8` saturation on `/neighbors` and `/subgraph` | **Pass** (type-bounded) + boundary test added |
| F6 | info | JSON contract regression — every wire-format field rustdoc'd | **Pass** |
| F7 | info | Pre-existing `eprintln!` in handler error paths | **Pass** (zero occurrences in `api.rs`; server binary tracked under 00112) |
| F8 | info | README refactor target — `#[deny(non_exhaustive_omitted_patterns)]` on `ApiError` match | **Deferred** — `DrevoError` is not `#[non_exhaustive]`, so exhaustiveness is already compiler-enforced |
| F9 | info | README refactor target — generic CRUD handler trait | **Deferred** — only two resource types (nodes, edges); meeting the rule would create the very Premature Abstraction the same skill rules forbid |
| F10 | low | `parse_direction` is a stringly-typed hand-rolled parser | **Documented · defer** (move to `FromStr for Direction` when a third caller appears) |
| F11 | trivial | Unused `State` extractor on `GET /` | **Documented · defer** |

Severity legend: **critical** = ship-blocking, **high** = rule violation that
materially affects correctness, **low** = stylistic / consistency, **info** =
informational pass-through with cross-link to a future refactor.

---

## Findings

### F1 — Handler duplication across node/edge CRUD (info; defer)

**Rule.** `drevo-architecture` §Anti-Patterns #2 ("Premature Abstraction —
Three strikes and you refactor") vs. #10 ("Mixing Concerns in Match Arms —
each arm calls a dedicated function").

**Sites.**
- [src/api.rs:251](src/api.rs:251) `create_node` / [src/api.rs:349](src/api.rs:349) `create_edge`
- [src/api.rs:261](src/api.rs:261) `get_node` / [src/api.rs:359](src/api.rs:359) `get_edge`
- [src/api.rs:271](src/api.rs:271) `update_node` / [src/api.rs:369](src/api.rs:369) `update_edge`
- [src/api.rs:283](src/api.rs:283) `delete_node` / [src/api.rs:381](src/api.rs:381) `delete_edge`
- [src/api.rs:293](src/api.rs:293) `list_nodes` / [src/api.rs:391](src/api.rs:391) `list_edges`

**Assessment.** Five near-duplicate handler pairs (~5 LOC each). The README
task wording explicitly invokes "three strikes and you refactor" — but the
rule is "extract a trait only when you ACTUALLY need a second impl" and we
have **two** resource types, not three. A `CrudHandler<T>` trait today would
satisfy the letter of rule #10 (smaller arms) while violating rule #2 (a
two-impl trait hierarchy with no third caller in sight).

**Disposition.** Defer until Phase 10 (Cypher executor, tasks `00061`–`00069`)
introduces a third resource concept (`Label` / `RelationshipType`). At that
point the abstraction has three concrete shapes and the trait pays for itself.
The five handler pairs are currently 25 lines of duplication — well below the
"refactor breakeven" point.

**Cross-link.** Phase 10 entry, task `00061`. No PR landing now.

---

### F2 — `DrevoError → HTTP status` is exhaustive; regression test added

**Rule.** `drevo-rust` §"Error layering across boundaries" — _"The HTTP layer
(`api.rs`) translates `DrevoError` to the right status code and JSON body.
Never let internal redb errors leak directly into HTTP responses."_

**Site.** [src/api.rs:170-189](src/api.rs:170) `impl IntoResponse for ApiError`.

**Mapping.**

| `DrevoError` variant | HTTP status | Source line |
|----------------------|-------------|-------------|
| `NodeNotFound(_)` | 404 | [src/api.rs:174](src/api.rs:174) |
| `EdgeNotFound(_)` | 404 | [src/api.rs:174](src/api.rs:174) |
| `DuplicateTitle(_)` | 409 | [src/api.rs:177](src/api.rs:177) |
| `InvalidWeight(_)` | 400 | [src/api.rs:178](src/api.rs:178) |
| `Locked` | 503 | [src/api.rs:179](src/api.rs:179) |
| `Storage(_)` | 500 | [src/api.rs:180](src/api.rs:180) |
| `Encode(_)` | 500 | [src/api.rs:181](src/api.rs:181) |
| `Decode(_)` | 500 | [src/api.rs:182](src/api.rs:182) |
| `Io(_)` | 500 | [src/api.rs:183](src/api.rs:183) |

The match is **exhaustive** (Rust enforces it on every non-`#[non_exhaustive]`
enum). Adding a new variant to `DrevoError` is a compile error in `api.rs`,
which is the desired behaviour. No internal redb error reaches the wire as a
trait-object string — `Storage(_)` is rendered with the user-facing
`Display` impl on `DrevoError` defined in [src/error.rs](src/error.rs:18).

**Action taken.** Added a regression test
`apierror_maps_every_drevoerror_variant_to_expected_status` in
[src/api.rs](src/api.rs) that constructs each `DrevoError` variant, runs it
through `IntoResponse::into_response`, and asserts the status code + JSON
shape. If a new variant is added to `DrevoError`, both the production match
**and** the regression test fail to compile until the mapping is decided.

---

### F3 — Magic-number `1000` cap in `list_nodes` / `list_edges` (low; fixed)

**Rule.** `drevo-rust` §"Code Style" — `SCREAMING_SNAKE_CASE` constants;
`drevo-architecture` §Anti-Pattern #3 ("Stringly Typed" — equally applies to
unnamed numeric magic numbers).

**Sites (pre-fix).** [src/api.rs:304](src/api.rs:304) and
[src/api.rs:402](src/api.rs:402):

```rust
let limit = limit.unwrap_or(50).min(1000);
```

Two-line duplication, no named constant. Contrast with `search_fts` at
[src/api.rs:557](src/api.rs:557)–[src/api.rs:562](src/api.rs:562), where
`DEFAULT_SEARCH_LIMIT` and `MAX_SEARCH_LIMIT` are named.

**Fix.** Hoisted `DEFAULT_LIST_LIMIT = 50` and `MAX_LIST_LIMIT = 1000` next
to the existing search constants. Both list handlers now reference the
constants. Doc-comments on the constants explain the rationale (protect the
ranker / scan from a pathological large `limit`).

---

### F4 — `limit` / `offset` overflow & saturation (info; tests added)

**Rule.** README task description — _"`limit` cap, `offset` overflow,
`depth: u8` saturation. Fuzz the query-string parser."_

**Sites.** [src/api.rs:296-307](src/api.rs:296), [src/api.rs:394-405](src/api.rs:394),
[src/api.rs:592-598](src/api.rs:592).

**Assessment.**
- `limit: Option<usize>` deserialized by serde — out-of-range values yield a
  `QueryRejection` → 400 before the handler runs.
- `offset: Option<usize>` — same.
- Inside the handler `.unwrap_or(50).min(MAX_LIST_LIMIT)` caps at 1000. No
  arithmetic on `offset + limit` happens in `api.rs`; the underlying scan
  applies the offset (validated separately under [task 00106](audit/AUDIT-db.md)).
- A negative `limit` (`-1`) is rejected by `serde::Deserialize<usize>`
  with a 400 before the handler.

**Action taken.** Added integration tests in
[tests/http_api_tests.rs](tests/http_api_tests.rs) under a new section
`§ Task 00109 — query-string boundary validation`:

- `list_nodes_limit_above_cap_is_clamped` — `?limit=9999` returns ≤
  `MAX_LIST_LIMIT` results, never errors.
- `list_nodes_negative_limit_returns_400` — `?limit=-1` is rejected with 400
  via `QueryRejection`.
- `list_edges_limit_above_cap_is_clamped` — same as above for edges.
- `list_nodes_huge_offset_returns_empty` — `?offset=999999999` returns
  `nodes: []` without an arithmetic overflow.

---

### F5 — `depth: u8` saturation (info; test added)

**Rule.** README task — _"`depth: u8` saturation."_

**Sites.** [src/api.rs:447](src/api.rs:447) `NeighborsQuery::depth: Option<u8>`,
[src/api.rs:469](src/api.rs:469) `SubgraphQuery::depth: Option<u8>`.

**Assessment.** `u8` is type-bounded to `0..=255`, so saturation is enforced
by serde. The traversal layer is audited under
[task 00107](audit/AUDIT-traversal.md) and handles `depth=0` as "return only
the start node" — already verified there.

**Action taken.** Added `get_node_neighbors_depth_zero_returns_empty` to
verify the HTTP contract aligns with the BFS contract proven in
[src/traversal.rs](src/traversal.rs) under `bfs_depth_zero_returns_empty`:
`depth=0` returns an empty neighbor list, **but** a missing start node
still returns 404 — the test asserts both halves so the
"empty neighborhood" vs. "missing node" distinction is preserved at the
HTTP layer. (`depth=255` is not tested at the HTTP layer — that's a
traversal-layer property and would make the HTTP test suite slow without
buying additional coverage.)

---

### F6 — JSON contract regression (info; pass)

**Rule.** README task — _"every wire-format field is documented in rustdoc on
its struct."_

**Survey.** Every `pub struct` returned over the wire carries doc-comments
on every field:

| Struct | Line | Fields documented |
|--------|------|-------------------|
| `ServerInfo` | [src/api.rs:206](src/api.rs:206) | name, version |
| `NodeListResponse` | [src/api.rs:243](src/api.rs:243) | nodes |
| `EdgeListResponse` | [src/api.rs:341](src/api.rs:341) | edges |
| `ShortestPathResponse` | [src/api.rs:475](src/api.rs:475) | path |
| `SearchFtsResponse` | [src/api.rs:581](src/api.rs:581) | results |
| `HealthResponse` | [src/api.rs:623](src/api.rs:623) | status |
| `StatusResponse` | [src/api.rs:635](src/api.rs:635) | name, version, uptime_seconds |
| `HealthStatus` | [src/api.rs:610](src/api.rs:610) | Ok / Ready / ShuttingDown |

**Verdict.** Pass.

---

### F7 — Pre-existing `eprintln!` in handler error paths (info; pass)

**Rule.** README task — _"Pre-existing `eprintln!` calls in handler error
paths (if any) → structured logging (cross-link with 00112)."_

**Survey.**

```
$ grep -nE "eprintln|println|panic!|todo!|unimplemented!" src/api.rs
(no matches)
```

All four `eprintln!` calls in the workspace live in
[src/bin/server.rs:40,54,63,68](src/bin/server.rs:40), which is the audit
surface for task `00112`. No action required in this PR; the rule is already
satisfied for `api.rs`.

---

### F8 — `#[deny(non_exhaustive_omitted_patterns)]` on `ApiError` match (info; defer)

**Rule.** README refactor target — _"Use `#[deny(non_exhaustive_omitted_patterns)]`
to prove it."_

**Assessment.** This attribute is only meaningful for enums marked
`#[non_exhaustive]` — it converts the **lint** that fires when an outside
crate's match against a `#[non_exhaustive]` enum omits a pattern into a hard
error. `DrevoError` is **not** `#[non_exhaustive]` (see
[src/error.rs:18](src/error.rs:18)), and `api.rs` lives in the same crate
as `DrevoError`, so:

1. The match at [src/api.rs:173](src/api.rs:173) is already enforced
   exhaustive by the compiler.
2. Adding `#[deny(non_exhaustive_omitted_patterns)]` here has zero observable
   effect.

**Defer rationale.** The rule the refactor target was reaching for —
"prove the mapping stays complete" — is achieved by F2's regression test
(every variant constructed and asserted). If `DrevoError` is ever marked
`#[non_exhaustive]` (e.g., for SemVer reasons when published as a separate
crate), revisit and apply the attribute.

---

### F9 — Generic CRUD handler trait (info; defer)

**Rule.** README refactor target — _"extract a generic CRUD handler trait
(`drevo-architecture` §SOLID 'I' — small focused traits)."_

**Assessment.** Same as F1 — only two resource types; the Interface
Segregation rule the README cites also says _"Start concrete. Extract a
trait only when you ACTUALLY need a second impl."_. The duplication is
under the "three strikes" threshold.

**Defer rationale.** Revisit at Phase 10 when Cypher introduces a third
concept (labels / relationship types) and supernode handling at Phase 14
introduces a fourth (vector index entries).

---

### F10 — `parse_direction` is a hand-rolled string parser (low; defer)

**Rule.** `drevo-architecture` §Anti-Pattern #3 ("Stringly Typed").

**Site.** [src/api.rs:701-711](src/api.rs:701).

**Assessment.** Hand-rolled lowercasing + match — three call sites
([src/api.rs:417](src/api.rs:417), [src/api.rs:496](src/api.rs:496), and
indirectly via `Direction::Both` fallback). A `FromStr for Direction`
impl on the `model.rs` side would let the handler use
`direction.unwrap_or_default().parse::<Direction>()?`. Tiny win, but
mixes API parsing into the model layer — fine for `Direction` because it
*is* a wire-format concept used in three different places already.

**Disposition.** Defer to a small cleanup PR when Phase 9 lands more
direction-aware endpoints (e.g., `/paths/all`). Not blocking.

---

### F11 — Unused `State` extractor on `GET /` (trivial; defer)

**Site.** [src/api.rs:216](src/api.rs:216):

```rust
async fn root(State(_state): State<ApiState>) -> Json<ServerInfo> {
```

The handler does not need state — the extractor is there for symmetry with
the other routes. Removing it would shorten the signature by 2 columns at
no behaviour cost.

**Disposition.** Defer. Not a rule violation.

---

## Cross-links

- **00104 (error hierarchy)** — `DrevoError` variants and their `Display`
  impls drive the JSON `error` message body. Already audited.
- **00106 (db core)** — `list_nodes_by_kind` / `list_edges_by_kind` apply
  the `offset` arithmetic this audit treats as black-box. Already audited.
- **00107 (traversal)** — `bfs`, `subgraph`, and `shortest_path` are the
  destinations of three handlers in this file. Already audited; their
  `depth=0` and unreachable-target semantics are the contracts F5 relies on.
- **00108 (fts)** — `search_fts` is the destination of the
  `POST /search/fts` handler. The `MAX_SEARCH_LIMIT = 1000` cap (already in
  place) protects the scoring pass — flagged for posting-list-length
  improvements under 00108's perf watch.
- **00112 (server binary + ops)** — owns `eprintln!` → `tracing` migration,
  including the future structured-log integration for handler error paths.
  This audit confirms `api.rs` is clean and the migration is purely a
  `src/bin/server.rs` and `Cargo.toml` change.

---

## Test additions

Two test groups were added in this PR to lock in the audit findings:

1. **`#[test] fn apierror_maps_every_drevoerror_variant_to_expected_status`**
   in [src/api.rs](src/api.rs) — covers F2. Constructs each `DrevoError`
   variant directly (using the `#[from]` constructors and unit variants),
   converts via `Into::into → ApiError → IntoResponse::into_response`, and
   asserts the resulting HTTP status code matches the table in F2. Adding a
   variant to `DrevoError` makes both the production match and this test
   fail to compile.

2. **§ "Task 00109 — query-string boundary validation"** in
   [tests/http_api_tests.rs](tests/http_api_tests.rs) — covers F4 / F5:
   - `list_nodes_limit_above_cap_is_clamped`
   - `list_nodes_negative_limit_returns_400`
   - `list_edges_limit_above_cap_is_clamped`
   - `list_nodes_huge_offset_returns_empty`
   - `get_node_neighbors_depth_zero_returns_empty` (also asserts missing-node 404)

---

## Conclusion

`src/api.rs` is in good shape against the four `.claude/skills/drevo-*/SKILL.md`
specs. The two refactor targets named in the README (generic CRUD trait,
`#[deny(non_exhaustive_omitted_patterns)]`) are **deferred with rationale**
— applying them today would either be a no-op or introduce the very
Premature Abstraction anti-pattern they were meant to avoid. The one
concrete fix (magic-number list-limit cap) is in this PR. The remaining
findings are tracked above with cross-links to the right downstream task.
