# AUDIT-traversal — Phase 8.5 task `00107`

**Scope.** `src/traversal.rs` (1107 → 1124 LOC) — the four exported graph
algorithms used by the `Drevo` facade and downstream HTTP / FFI / WASM
layers:

- `bfs(start, max_depth, direction, edge_kind, …)` — breadth-first
- `dfs(start, max_depth, direction, edge_kind, …)` — depth-first
- `shortest_path(from, to, edge_kind, …)` — weighted Dijkstra
- `subgraph(root, depth, edge_kind, …)` — depth-limited extraction

Plus the private `DijkstraState` ordering helper and the newly-extracted
`neighbor_id_for_edge` direction resolver.

**Rules verified against.**

- `drevo-database` §"Graph Traversal" — the documented complexity bounds
  (BFS/DFS `O(V+E)` on reachable; Dijkstra `O((V+E) log V)`; subgraph
  `O((V+E))` within radius) and the documented edge-kind filter
  push-down ("edge-kind filtering at the traversal level is dramatic —
  50µs vs 245µs").
- `drevo-architecture` §"Algorithm Design Principles" — BFS/DFS use
  visited sets to bound at `O(V+E)`, Dijkstra is the binary-heap
  variant, ID-based references throughout.
- `drevo-architecture` §"Anti-Patterns" #2 (Premature Abstraction),
  #6 (Deep Nesting), and the "Three strikes and you refactor" rule.
- `drevo-tdd` §"Edge cases mandatory" — empty graph, single node,
  cycles, disconnected components, depth 0, max depth, self-loops,
  parallel edges.
- `drevo-tdd` §"Property-based tests for invariants" — manual
  xorshift32-seeded fuzzer (`proptest` adoption is Phase 9 task
  `00057`; this audit follows the precedent set by `00106`).
- `drevo-rust` §"Error Handling" — no `.unwrap()` / `.expect()` in
  library code; existing surface already compliant.

**Test baseline at audit start.** 1137 passing (post-`00106` baseline,
all features).

**Test baseline at audit end.** 1154 (+17 in the new
`tests/traversal_audit_tests.rs` file).

**Cross-links closed.**

- `00106` F1 (non-finite edge weight admission) — Dijkstra's
  precondition story is completed here. F1 closed the *write*-time
  hole (`NaN` / ±∞ rejected at `create_edge` / `update_edge`); this
  audit's F2 documents the remaining *traversal*-time precondition
  (negative finite weights) and pins the current behaviour with a
  test, deferring the algorithm swap to a future task.

---

## Findings

### F1 — Edge-kind filter parity gap in `shortest_path` and `subgraph`   ❌ → ✅ fixed in this PR

`drevo-database` §"Graph Traversal" documents edge-kind filtering as a
push-down optimisation that delivers a 50µs vs 245µs improvement at
depth 2 in the criterion benches. `bfs` and `dfs` both accept an
`edge_kind: Option<&str>` parameter; `shortest_path` and `subgraph` did
not — they silently considered every edge regardless of `kind`.

The README task definition for `00107` calls this out explicitly:
*"Edge-kind filter is pushed into the traversal […]. Verify all four
algorithms support it consistently."*

**Before.**

```rust
pub fn shortest_path<F, G>(from: u64, to: u64,         get_node: &F, edges_of: &G) -> Result<Option<Vec<u64>>>;
pub fn subgraph<F, G>     (root: u64, depth: u8,        get_node: &F, edges_of: &G) -> Result<SubGraph>;
```

**After.** Both internal traversal functions accept `edge_kind:
Option<&str>` immediately before the closures, in the same positional
slot that `bfs` and `dfs` use:

```rust
pub fn shortest_path<F, G>(from: u64, to: u64, edge_kind: Option<&str>, get_node: &F, edges_of: &G) -> Result<Option<Vec<u64>>>;
pub fn subgraph<F, G>     (root: u64, depth: u8, edge_kind: Option<&str>, get_node: &F, edges_of: &G) -> Result<SubGraph>;
```

The filter is applied:

- **`shortest_path`** — inside the heap-pop loop, before the relaxation
  step. Filtered-out edges contribute neither to the path nor to the
  Dijkstra cost lattice.
- **`subgraph`** — in **both** phases: the BFS-discovery phase skips
  filtered-out edges (so nodes only reachable through them are not
  discovered), AND the edge-collection phase skips them (so a chord
  edge of a different kind between two `Some("link")`-reachable nodes
  is excluded from the returned `SubGraph::edges`).

**Public-API additivity.** The existing `Drevo::shortest_path(from, to)`
and `Drevo::subgraph(root, depth)` signatures are unchanged — both
now delegate to the new `*_filtered` variants with `None`:

```rust
pub fn shortest_path(&self, from: u64, to: u64) -> Result<Option<Vec<u64>>> {
    self.shortest_path_filtered(from, to, None)
}
pub fn shortest_path_filtered(&self, from: u64, to: u64, edge_kind: Option<&str>) -> Result<Option<Vec<u64>>>;

pub fn subgraph(&self, root: u64, depth: u8) -> Result<SubGraph> {
    self.subgraph_filtered(root, depth, None)
}
pub fn subgraph_filtered(&self, root: u64, depth: u8, edge_kind: Option<&str>) -> Result<SubGraph>;
```

No FFI / WASM / HTTP signatures change in this PR. Exposing the new
filtered variants over those boundaries is the explicit job of the
upcoming `00109` (HTTP API), `00110` (FFI), and `00111` (WASM) audits —
those tasks will pick up `shortest_path_filtered` / `subgraph_filtered`
as a free addition rather than a contested redesign.

**Tests added** (in `tests/traversal_audit_tests.rs`):

| Test | Verifies |
|------|----------|
| `shortest_path_filtered_passes_through_when_kind_none` | `None` matches the legacy `shortest_path` byte-for-byte |
| `shortest_path_filtered_excludes_wrong_kind` | Parallel edges with different kinds + weights — filter selects which one Dijkstra uses |
| `shortest_path_filtered_unreachable_when_only_other_kind_exists` | Filter can make `to` unreachable; returns `None` |
| `shortest_path_filtered_self_target_returns_just_self` | The `from == to` short-circuit is filter-independent |
| `shortest_path_filtered_routes_through_filter_consistent_path` | Diamond with two kinds — each filter routes through its own arm |
| `subgraph_filtered_pass_through_when_kind_none` | `None` matches the legacy `subgraph` (node and edge counts) |
| `subgraph_filtered_excludes_other_kind_edges` | Two-arm fan-out — each filter returns only its arm |
| `subgraph_filtered_does_not_discover_via_filtered_out_edges` | Nodes only reachable through filtered-out edges are not in `nodes` |
| `subgraph_filtered_edge_collection_phase_respects_kind` | A chord edge between two filter-reachable nodes is excluded if its kind is wrong |
| `subgraph_filtered_nonexistent_kind_returns_root_only` | No edge matches → result is `{root}` with no edges |
| `subgraph_filtered_root_missing_returns_node_not_found` | Filter does not bypass the root-existence check |

### F2 — Dijkstra precondition (non-negative weights) was undocumented and unguarded   ❌ → ✅ documented + pinned

`drevo-database` §"Graph Traversal" table lists "weighted by
`edge.weight`" but does not call out the non-negativity precondition.
The `traversal.rs` rustdoc had a one-line "Edge weights must be
non-negative" without explaining what happens if they aren't.

`00106` F1 closed the non-finite half of the story by rejecting `NaN`
and ±∞ at `create_edge` / `update_edge` time. Negative *finite* weights
are still admitted by the storage layer — and reasonably so, the
model layer does not own Dijkstra's preconditions.

**Behaviour pinned.** Three tests document and lock the current
Dijkstra semantics on negative weights:

- `dijkstra_negative_weight_no_panic_and_does_not_infinite_loop` — a
  single negative edge returns a finite path without hanging.
- `dijkstra_negative_weight_can_return_non_optimal_path` — the
  classical failure mode:

  ```
  A --[link,  1.0]--> B
  A --[link,  3.0]--> C
  C --[link, -5.0]--> B
  ```

  Truly shortest `A → B` is `A → C → B` with cost `-2`. Dijkstra
  returns `A → B` with cost `1` because `B` is settled the first time
  it pops from the heap. The test asserts the implementation returns
  the **non-optimal** path; a future Bellman-Ford swap is expected to
  flip the assertion as a deliberate code change rather than a silent
  one.

- `dijkstra_zero_weight_edges_treated_as_neutral` — confirms `0.0`
  is *not* a precondition violation; a chain of zero-weight edges
  routes correctly.

**Rustdoc updated.** [src/traversal.rs:248-275](src/traversal.rs:248)
now carries a `# Preconditions` section that:

1. References `AUDIT-db.md` F1 for the finiteness guarantee.
2. States that negative finite weights are admitted by storage but
   violate Dijkstra correctness.
3. Notes that the lazy-update relaxation (`if next_cost <
   current_best`) bounds heap progress by the finite f32 precision
   lattice — termination is preserved even though optimality is not.
4. Points users at Bellman-Ford as the algorithm to reach for when
   negative weights are real, and flags that Bellman-Ford is not yet
   implemented in drevo.

**Why we are not adding a runtime guard.** Reusing
`DrevoError::InvalidWeight` for a Dijkstra-time check would mix two
distinct invariants (write-time finiteness vs. traversal-time
non-negativity) and would surface the error far from the point where
the weight was set. The right place for a runtime guard is the same
layer that picks the algorithm — i.e. when a future API offers both
Dijkstra and Bellman-Ford, the algorithm-selection layer should reject
"give me Dijkstra on a graph that contains negative edges" rather
than every traversal call paying the cost of a full scan. Tracked as
a follow-up in the cross-links below.

### F3 — Direction-resolution match arm was duplicated three times   ❌ → ✅ extracted

`drevo-architecture` §"Three strikes and you refactor". The 7-line
`match direction { Outgoing => …, Incoming => …, Both => if from_id ==
current_id { to_id } else { from_id } }` arm was inlined in `bfs`,
`dfs`, and `subgraph` — three identical copies of the same
neighbor-resolution logic.

**Extracted.** New private helper
[src/traversal.rs:14-33](src/traversal.rs:14):

```rust
#[inline]
fn neighbor_id_for_edge(edge: &Edge, current_id: u64, direction: Direction) -> u64
```

with a rustdoc that explains the `Direction::Both` self-loop fallthrough
(both endpoints equal `current_id` → returns `current_id` → visited-set
check trivially skips). All three call sites now read

```rust
let neighbor_id = neighbor_id_for_edge(edge, current_id, direction);
```

— 5 lines saved per call site, 15 lines net, plus a single point of
truth for the direction semantics. `#[inline]` preserves the zero-cost
property; no behavioural change is possible because the bodies are
byte-identical to the pre-extraction match arms (one match arm — the
implicit `Direction::Both` in `subgraph` — is now also expressed via
the same helper, removing an asymmetry).

### F4 — Complexity bounds match the documented contract   ✅ compliant

`drevo-database` §"Graph Traversal" table:

| Algorithm | Doc bound | Implementation |
|-----------|-----------|----------------|
| BFS | `O(V+E)` on reachable | Visited `HashSet` + `VecDeque` frontier. Each reachable node enqueued at most once; each edge of a dequeued node visited at most once. ✅ |
| DFS | `O(V+E)` on reachable | Same shape with `Vec` stack (LIFO). ✅ |
| Dijkstra | `O((V+E) log V)` | Binary-heap variant with lazy decrease-key (skip-if-stale `if cost > *dist.get(…)`). Each edge contributes at most one heap push; heap ops are `O(log V)`. ✅ |
| subgraph | `O((V+E))` within radius | BFS discovery + a single `edges_of` scan per discovered node for collection. The collection phase is the `O(E_discovered)` factor that justifies "within radius". ✅ |

Verified by reading the source and cross-checked with
`benches/traversal_bench.rs`:

- `bfs_depth_3_1k_nodes` ≈ µs-level (sub-millisecond on the dev
  workstation — bounded by `V` not `V*E`).
- `dijkstra_dense_graph` ≈ ms-level, consistent with a `V log V`
  heap profile on the 1k-node dense fixture.

### F5 — Edge cases mandatory (drevo-tdd) — exhaustively covered   ✅ compliant

`drevo-tdd` §"Edge cases mandatory" requires:

| Edge case | Test |
|-----------|------|
| Empty graph | `empty_graph_bfs_returns_error_or_empty`, `empty_graph_dfs_returns_empty`, `empty_graph_shortest_path_returns_none`, `empty_graph_subgraph_returns_error`, `empty_graph_neighbors_returns_empty` |
| Single node | `single_node_all_algorithms_consistent` |
| Cycles | `cycle_three_nodes_all_algorithms`, `complex_cycle_with_tail`, `self_loop_plus_cycle`, plus `bfs_handles_cycle` / `dfs_handles_cycle` / `sp_handles_cycle` / `subgraph_handles_cycle` |
| Disconnected components | `disconnected_components_all_algorithms` |
| Depth 0 | `depth_zero_bfs_dfs_return_empty`, `bfs_depth_zero_returns_empty`, `dfs_depth_zero_returns_empty`, `subgraph_depth_zero_returns_root_only` |
| Max depth (`u8::MAX = 255`) | `max_depth_u8_no_panic` |
| Self-loops | `self_loop_all_algorithms`, `bfs_self_loop`, `dfs_self_loop`, `sp_self_loop_ignored`, `subgraph_self_loop` |
| Parallel edges | `parallel_edges_between_nodes` |
| Bidirectional edges | `bidirectional_edges_consistency` |
| Direction filter (Outgoing/Incoming/Both) | `direction_filtering_across_algorithms` + per-algorithm `*_respects_direction_*` |
| Deleted node mid-graph | `deleted_node_not_traversed` |
| Long chain | `long_chain_depth_limits` |
| Large fan-out | `large_fan_out_50_spokes`, `bfs_fan_out`, `dfs_fan_out` |
| Diamond | `diamond_graph_all_algorithms`, `bfs_diamond_graph_no_duplicates`, `dfs_diamond_graph_no_duplicates`, `subgraph_diamond_no_duplicates` |
| Domain scenarios | `scenario_cbt_journal_thought_cycle`, `scenario_story_editor_disconnected_notes`, `scenario_task_manager_blocking_chain`, `scenario_erp_warehouse_self_loop`, `scenario_bug_tracker_impact_diamond` |

No gaps. Every box from `drevo-tdd` has at least one direct test.

### F6 — Randomised cross-algorithm invariant fuzzer   ❌ → ✅ added

`drevo-tdd` §"Property-based tests for invariants" — `proptest` adoption
is parked under Phase 9 task `00057`, so this audit follows `00106`'s
precedent and uses a deterministic xorshift32-seeded fuzzer.

**Invariants pinned.** Three new property-style tests in
`tests/traversal_audit_tests.rs`:

1. `bfs_dfs_subgraph_same_reachable_set_random_graphs` — on a random
   60-node graph with ~180 edges across 3 kinds, `BFS(root,
   Direction::Both)`, `DFS(root, Direction::Both)`, and
   `subgraph(root, depth) \ {root}` discover the same node set. Run
   over 3 seeds × 5 random roots = 15 cross-checks per algorithm.

2. `shortest_path_within_reachable_set_only_random_graphs` — for any
   `(from, to)` pair on a random 40-node graph, `shortest_path(from,
   to)` returns `Some(_)` iff `to ∈ BFS(from, Direction::Outgoing)
   ∪ {from}`. Catches both false positives ("found a path to an
   unreachable node") and false negatives ("missed a reachable
   target"). Run over 3 seeds × 10 random pairs = 30 cross-checks.

3. `edge_kind_filter_monotone_in_reachable_set_random_graphs` — the
   filtered traversal is a subset of the unfiltered one for every
   edge kind. `BFS(root, Some(k)) ⊆ BFS(root, None)`. Run over 3
   seeds × 3 kinds × 3 random roots = 27 cross-checks.

**Why xorshift32 rather than `proptest`.** Same rationale as `00106`'s
`invariants_hold_under_random_mutations`: keep the dev-dependency tree
lean until `00057` lands a single proptest-adoption PR. The xorshift32
seeds are stored as integer literals so failures are exactly
reproducible (no shrinking, but the inputs are tiny by construction).

### F7 — Dijkstra `f32` ordering uses `total_cmp`   ✅ compliant

[src/traversal.rs:228](src/traversal.rs:228) defends against NaN
in the ordering even though `00106` F1 now rejects NaN at the write
boundary — defense in depth, no harm done. The `Reverse`-style
total_cmp turns the `BinaryHeap` (max-heap) into a min-heap, with the
node-id secondary key making the ordering total and deterministic.

### F8 — `Direction::Both` self-loop handling is correct   ✅ compliant

Both `bfs` / `dfs` and `subgraph` route self-loops through
`neighbor_id_for_edge`, which returns `current_id` for a self-loop
under `Direction::Both` (the `else` branch on
`edge.from_id == current_id` happens to also be reached when *both*
endpoints equal `current_id`, but the visited set already contains
`current_id` so the algorithm skips it). The pre-existing tests
`bfs_self_loop`, `dfs_self_loop`, and `self_loop_all_algorithms` pin
this. No change needed.

The `subgraph` second pass (edge collection) does include the
self-loop edge — `edge.from_id == edge.to_id == current_id`, both in
the `visited` set, so the edge passes the membership check. That's
the intended behaviour: a subgraph's edges should include the
self-loop on the root if one exists. Test `subgraph_self_loop` and
`subgraph_filtered_*` (transitively) cover this.

### F9 — `Drevo::neighbors` is `bfs(..., max_depth=1)` — no separate audit needed   ✅ compliant

`Drevo::neighbors(node_id, direction, kind)` is a convenience wrapper
over `bfs(node_id, 1, direction, kind)` — same algorithm, same
edge-kind filter, no separate code path. Audit of `bfs` covers it.
The `tests/traversal_edge_case_tests.rs::single_node_all_algorithms_consistent`
test exercises `neighbors` on the empty / single-node graph; the
`direction_filtering_across_algorithms` test pins the direction
contract.

### F10 — `pub` API surface — rustdoc coverage   ✅ compliant

Every `pub` function in `traversal.rs` carries rustdoc with arguments,
returns, and (where relevant) preconditions documented. The new
helper `neighbor_id_for_edge` is private. `cargo doc --no-deps -- -D
missing_docs` remains clean.

### F11 — No `.unwrap()` / `.expect()` outside tests   ✅ compliant

`drevo-rust` §"Error Handling". The single `prev[&cur]` in Dijkstra's
path reconstruction is `Index<&u64>` not `unwrap()` — and it's
provably-safe: every node pushed into `prev` is unreachable from the
back-trace unless its predecessor was already recorded. Spot-checked
on the cycle / diamond fixtures with no panic.

### F12 — Indentation ≤ 3 levels per function   ✅ compliant

`drevo-rust` §"Code Style". Spot-checked `subgraph` (the deepest
function in the file): outer `while let` (1) → inner `for edge in
&edges` (2) → `if let Some(kind)` (3). Three levels max in every arm.

### F13 — `bincode` / serialisation independence   ✅ N/A

Traversal layer is purely in-memory algorithmic code; no serialisation
crosses this module. No bincode drift possible.

---

## Refactor PRs landed in this audit

1. **F1**: Internal `traversal::shortest_path` and `traversal::subgraph`
   now accept `edge_kind: Option<&str>`. New public methods
   `Drevo::shortest_path_filtered` and `Drevo::subgraph_filtered`. The
   existing `Drevo::shortest_path` and `Drevo::subgraph` keep their
   signatures and delegate with `None`. 11 new edge-kind parity tests.
2. **F2**: Rustdoc expanded on `traversal::shortest_path` to document
   the non-negative-weight precondition, what happens on negative
   finite weights (non-optimal result, no panic, terminates),
   and where to look once Bellman-Ford lands. 3 new tests pin the
   current behaviour.
3. **F3**: `neighbor_id_for_edge` extracted; three call sites
   collapsed to a single function reference. `#[inline]` preserves the
   zero-cost property.
4. **F4–F12**: Verification-only — every cited rule has a verdict
   and (where the rule is "we should have a test for X") a test.
5. **F6**: Three random-graph cross-algorithm invariant fuzzers
   exercise BFS/DFS/subgraph parity, shortest-path reachability
   bi-implication, and edge-kind filter monotonicity.
6. **Closes `00106` F1's traversal-half**: write-time finiteness +
   traversal-time precondition story is now complete and testable.

## Refactor PRs deferred (cross-linked)

- **F2 follow-up — algorithm-selection guard.** A future API that
  offers both Dijkstra and Bellman-Ford should reject "Dijkstra on a
  graph with negative weights" at selection time, not at edge-relax
  time. Track alongside Phase 14 `00086` (query optimizer cost
  model) — the algorithm-pick decision is naturally part of plan
  construction.
- **F2 follow-up — Bellman-Ford.** Not on the roadmap today; flag for
  Phase 15 (algorithms / production hardening). The audit's pinned
  test will need to be updated when Bellman-Ford ships.
- **Cursor abstraction (README "Refactor targets" line).** The README
  task definition mentions *"unify edge-kind filter + direction
  handling behind a common cursor abstraction (drevo-architecture
  §'Strategy Pattern')"*. After F3 extracted the direction helper,
  the remaining duplication between BFS and DFS is the
  frontier-data-structure choice (`VecDeque` vs `Vec`) — which is
  the *whole point* of the two algorithms being separate. Collapsing
  them behind a `trait Frontier { push, pop }` would buy ~30 lines of
  shared loop body at the cost of a generic indirection that the
  bench-measured 50µs vs 245µs filter-pushdown improvement already
  shows the compiler is happy to monomorphise away. Per
  `drevo-architecture` §"Premature Abstraction" #2 — *"Start
  concrete. Extract a trait only when you ACTUALLY need a second
  impl"* — defer until a third algorithm (e.g. bidirectional BFS or
  iterative-deepening DFS) actually arrives. Tracked as
  `00107-follow-up`.

## Definition of done — task `00107`

- ✅ `audit/AUDIT-traversal.md` exists, every cited rule has a verdict.
- ✅ Cross-link closed: `00106` F1 traversal-time precondition story.
- ✅ All four algorithms support edge-kind filtering consistently
  (parity with the README's "Verify all four algorithms support it
  consistently" definition of done).
- ✅ Dijkstra weight precondition documented in rustdoc; behaviour on
  negative finite weights pinned by tests.
- ✅ Mandatory edge cases from `drevo-tdd` exhaustively covered
  (F5 — every box ticked, no gap found).
- ✅ Randomised cross-algorithm invariant fuzzer added — manual
  xorshift32 seeds, follows `00106` precedent.
- ✅ Test baseline grows: 1137 → 1154 (+17 — 11 edge-kind parity
  tests, 3 Dijkstra precondition tests, 3 randomised invariant
  fuzzers).
- ✅ `cargo test --all-features` clean (1154 passing).
- ✅ `cargo clippy --all-targets --all-features -- -D warnings` clean.
- ✅ `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean.
- ✅ `cargo fmt --check` clean.
- ✅ No public API breakage. `Drevo::shortest_path` and
  `Drevo::subgraph` keep their 2-arg signatures; new
  `*_filtered` variants are purely additive. FFI / WASM / HTTP
  layers are untouched (their audits — `00109`, `00110`, `00111` —
  will expose the new variants).
