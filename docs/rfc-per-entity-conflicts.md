# RFC: Per-entity conflict detection for explicit transactions (#749)

**Status:** proposed 2026-10-25. Decision D1 (default isolation) is the maintainer's; D2–D4 as
proposed unless objected to. Implementation follows this RFC in the slices of section 6.
**Motivation:** two explicit transactions that write *different* nodes cannot both commit today.
Reproduced on `ice1x/drevo:0.0.62` with the neo4j Python driver (issue #749): A writes nodes 1→2
while B writes 3→4; whichever commits second fails with
`Neo.TransientError.Transaction.Outdated`. Eight driver threads each running ten managed
transactions on disjoint nodes took **6.23 s with 12 retries**, against **0.14 s** for the same
eighty transactions one after another: the work is trivial, the time is the drivers' exponential
back-off between attempts. PostgreSQL commits both sides of every one of those pairs because its
locks are per row. The goal is the same outcome without giving up what drevo has: no waits, no
deadlocks, retryable conflicts, one fsynced batch per commit.

## 1. Where the engine is today (evidence)

- `NativeGraph::tx_begin` clones the live `Arc<Inner>` twice into a `TxSlot { base, working, ops }`.
  Statements mutate `working` through `Arc::make_mut` (copy-on-write) and append `WalOp`s to `ops`.
- `commit_parts` (`drevo-core/src/native.rs`) takes the write lock and fails with
  `CommitError::Conflict` unless `Arc::ptr_eq(&live, &base)`: the transaction commits only if **no
  commit of any kind** happened since its `begin`. It then validates constraints on `working`,
  records `ops` as one group-committed WAL batch and swaps `*live = working`.
- Auto-commit statements that write run the same way (`NativeService`, `ATTEMPTS = 8`): the server
  retries them itself, so clients only see conflicts from explicit transactions.
- The rule is what makes every committed history serializable and the engine deadlock-free: nothing
  waits, so there is no cycle to detect. Its cost is that explicit transactions take turns.
- Ids are allocated from the working copy (`Inner::next_node_id`, `next_edge_id`, and the `u32`
  interning of edge kinds in `kind_ids`), so two concurrent transactions that each create a node
  hand out the **same id**. Harmless today, because at most one of them can commit.
- Node titles are globally unique (`DuplicateTitle`), and `UNIQUE` / `EXISTS` constraints are
  validated on the working copy at commit (`validate_constraints`).
- Two-phase commit (`tx_prepare`) reuses the same check and raises a fence that fails **every**
  other write until the prepared transaction is resolved.
- The change feed (`ChangeFeed`, `changes_since`, `trim_before`) already carries every committed
  op in commit order, trimmed to what the indexes consumed and the event history.

## 2. Goal and non-goals

Goal: an explicit transaction commits when nothing it wrote, deleted or depends on was written or
deleted by a commit it did not see, regardless of how many *other* commits happened. Conflicts stay
detected at commit, with the same error codes (`Neo.TransientError.Transaction.Outdated`, HTTP
409, drevo-py `TransactionConflict`), so drivers and applications keep working unchanged.

Non-goals: property-level granularity (two transactions setting different properties of the same
node still conflict), pessimistic locks (`FOR UPDATE`), savepoints, and changing auto-commit
statements, which already retry server-side.

## 3. Design

### 3.1 The conflict set of a transaction

Derived from `ops`, at commit, without touching statement execution:

| op | writes | deletes | depends on |
|---|---|---|---|
| `UpsertNode(n)` | `Node(n.id)` | | |
| `DeleteNode(id)` | `Node(id)` | `Node(id)` | |
| `UpsertEdge(e)` | `Edge(e.id)` | | `Node(e.from_id)`, `Node(e.to_id)` |
| `DeleteEdge(id)` | `Edge(id)` | `Edge(id)` | |
| `SetEmbedding(id, _)`, `DeleteEmbedding(id)` | `Node(id)` | | |

(`Prepare` / `CommitPrepared` / `RollbackPrepared` records are not produced by a registered
transaction.) The set is small: a `HashSet<Key>` with `Key = Node(u64) | Edge(u64)` per class.

Two transactions **conflict** when any of these intersect:

- `writes(A) ∩ writes(B)` — both touched the same node or relationship;
- `deps(A) ∩ deletes(B)` — A added or changed a relationship whose endpoint B deleted;
- `deletes(A) ∩ deps(B)` — the mirror image, so the answer does not depend on commit order.

A node deletion in A cascades to the relationships A could see; a relationship B attached to that
node after A's snapshot is caught by the third rule. Nothing else can make the merged graph violate
the engine's invariants (endpoint existence, cascade), because every other combination writes
disjoint records.

### 3.2 What a commit compares against

`Inner` gains `commit_seq: u64`, bumped once per committed batch (auto-commit statement,
`tx_commit`, `commit_prepared`). `base.commit_seq` names the snapshot a transaction started from.

`NativeGraph` keeps a bounded ring of recent commits, `recent: Mutex<VecDeque<(u64, ConflictSet)>>`
(sequence and the three sets of 3.1), appended under the write lock by every commit path.
`DREVO_TX_CONFLICT_HISTORY` bounds its length (default 10,000 commits); the memory is a few
hundred bytes per committed batch.

At commit, with the write lock held:

1. `live.commit_seq == base.commit_seq` → no one committed in between; commit exactly as today.
2. Otherwise, every ring entry with `seq > base.commit_seq` is compared against the transaction's
   set with the rules of 3.1. The first intersection is a `Conflict`.
3. If the ring no longer reaches back to `base.commit_seq` (the transaction outlived the history),
   the commit is a `Conflict` too — conservative, retryable, and counted separately so an operator
   can raise the history.

The change feed was considered instead of a ring: it has the same ops, but its entries are full
records (node bodies, property maps), it is trimmed on the indexes' schedule, and scanning it per
commit would re-derive the sets every time. The ring stores only keys and has its own bound.

### 3.3 Merging a non-conflicting commit

Today's swap `*live = working` is only valid when `live` *is* `base`. When other commits happened
and nothing intersects, the transaction's ops are **replayed onto the current live state**:

```text
let mut merged = Arc::clone(&live);                 // O(1)
let m = Arc::make_mut(&mut merged);                 // one copy-on-write of Inner's maps
for op in ops { m.apply_wal_op(op.clone()); }       // the WAL replay path used at open
m.validate_constraints()?;  m.validate_titles()?;   // on the MERGED state
record(ops)?;                                       // one fsynced batch, as today
m.commit_seq += 1;  *live = merged;  ring.push(...);
```

`apply_wal_op` is the function that rebuilds a graph from its log at open, so adjacency, title and
kind indexes, embeddings and the unique-constraint index are maintained by construction, and a
replayed `UpsertEdge` re-interns its kind *name* in the merged state — the `u32` a transaction's
working copy assigned is never trusted. The copy-on-write costs what any write after a snapshot
costs today (a clone of `Inner`'s maps of `Arc`s, not of the records); it is paid only by commits
that actually overlapped with another one.

**Constraints under concurrency.** Two transactions may each create a node with the same unique
property value or title; their keys are disjoint, so only the merged validation catches it. The
failure is reported as a **retryable `Conflict`** when the working copy was valid against its own
base (the violation exists only because of the other commit) and as the usual
`ConstraintValidationFailed` / `DuplicateTitle` when the transaction was invalid on its own. The
retry then sees the committed node, which is what a `MERGE`-style upsert needs.

### 3.4 Id allocation

Node and relationship ids move from `Inner` to shared atomics on `NativeGraph`
(`next_node_id: AtomicU64`, `next_edge_id`): a transaction's `create_node` takes the next id from
the shared counter, so concurrent transactions never collide and a rolled-back transaction leaves a
gap, exactly like a PostgreSQL sequence. `Inner::next_*` stay as the high-water marks recovery
computes from the log (`apply_wal_op` already takes `max`), and the atomics are seeded from them at
open. Kinds need no change (3.3).

### 3.5 Two-phase commit

A `PreparedEntry` stores the prepared transaction's conflict set. While it is prepared, another
commit fails with `PreparedPending` **only if it intersects** that set under the rules of 3.1;
everything else commits and is merged as in 3.3. `commit_prepared` then replays the prepared write
set onto live (it cannot fail: nothing that reached live since intersects it). The RFC on two-phase
commit and the admin guide's "refuses every other write" change accordingly.

### 3.6 Isolation

Write-set detection gives **snapshot isolation**: two transactions that each *read* both nodes and
each *write* one of them both commit (write skew), which today's rule forbids. Reads are not
tracked — a `MATCH` scan reads the whole graph, so a read set would bring the whole-graph rule back.

- **D1 (maintainer):** which default?
  - *Proposed:* snapshot isolation by default. It is what the Neo4j drivers and their users expect
    (Neo4j itself offers no serializable mode), and it is PostgreSQL's `REPEATABLE READ`.
  - *Alternative:* keep serializable (today's rule) as the default and make the per-entity rule
    opt-in. Safer for existing deployments, but the slowdown in the motivation stays the default.
- **D2:** either way, the other mode stays available per transaction through Bolt
  `tx_metadata: {"drevo.isolation": "serializable" | "snapshot"}` (drivers pass the map through;
  drevo-py gets an `isolation=` argument), and server-wide through `DREVO_TX_ISOLATION`.
  Serializable means: conflict whenever `live.commit_seq != base.commit_seq`.

### 3.7 Observability and errors

- Error codes, messages and HTTP statuses are unchanged; the message of a history miss (3.2 step 3)
  says so: *the transaction outlived the conflict history (N commits); retry, or raise
  `DREVO_TX_CONFLICT_HISTORY`*.
- `drevo_tx_conflicts_total{reason="overlap"|"constraint"|"history"|"serializable"}`.
- `SHOW TRANSACTIONS` is unchanged; `drevo.info()` reports the isolation default.

## 4. Decisions

- **D1** default isolation — see 3.6, the maintainer's call.
- **D2** per-transaction override via `tx_metadata` and `DREVO_TX_ISOLATION` — proposed.
- **D3** node-level granularity (no property-level keys) — proposed; property-level keys would
  need per-property ops in the WAL and are not worth it for the measured workloads.
- **D4** a bounded ring of commit keys rather than the change feed — proposed (3.2).

## 5. Tests

- Core (`drevo-core/tests/`): disjoint writers both commit, in both commit orders; same node →
  `Conflict`; A deletes a node while B attaches a relationship to it → the second committer gets
  `Conflict`, in both orders; cascade: A deletes node X with its relationships while B updates one of
  them → `Conflict`; two transactions creating nodes get different ids; same title or same unique
  value from two transactions → the second gets `Conflict`, a retry gets `DuplicateTitle` /
  `ConstraintValidationFailed`; a transaction older than the history gets `Conflict` with the
  history message; prepared: a non-intersecting write commits while a transaction is prepared, an
  intersecting one gets `PreparedPending`; `commit_prepared` after unrelated commits applies the
  prepared write set; serializable mode reproduces today's behaviour.
- Server (`tests/`): the Bolt experiment from #749 as an integration test — cross-order writes end
  with one `Transaction.Outdated`; disjoint writes **both commit**; 8 writers × 10 managed
  transactions on disjoint nodes finish with **zero retries** and within 2× of the sequential time;
  `tx_metadata` isolation override; drevo-py `isolation=`.
- The `docs_examples` and the Concurrency section of the Cypher reference updated with the new
  rule ("conflict only when the same nodes or relationships were written").

## 6. Slices

1. **Measure first, no behaviour change:** `commit_seq`, conflict-set extraction from `ops`, the
   ring, and `drevo_tx_conflicts_total{reason}` — including a `reason="would_pass"` count for
   commits today's rule rejects that 3.1 would accept. Ship, run on live, read the counter.
2. Shared id allocation (3.4). Behaviour-neutral; unlocks 3.
3. Merge commit (3.3) behind the isolation switch of 3.6, default per D1. The integration tests of
   section 5 and the benchmark.
4. Per-entity fence for prepared transactions (3.5); RFC and admin-guide text updated.
5. Docs: Concurrency section, drevo-py reference, CHANGELOG; `drevo.info()` field.

## 7. Risks

- **Replay fidelity:** `apply_wal_op` is written for recovery, where ops arrive in commit order on
  an otherwise idle graph. Merging relies on it maintaining every derived index on a live state;
  slice 1 adds a test that replaying any op sequence onto a snapshot equals applying it directly.
- **Validation cost:** `validate_constraints` runs per commit today already; the merged validation
  adds nothing when no other commit happened and one more pass when one did.
- **Semantics change (D1):** write skew becomes possible by default. The per-transaction override
  and the server-wide switch exist for the cases that need serializability.
- **History bound:** a very long transaction under heavy write load can outlive the ring; the
  error names the remedy and the counter shows how often it happens.
