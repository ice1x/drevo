# RFC: Two-phase commit (#556)

**Status:** accepted 2026-10-02. D1 (fail fast) and D2 (no auto-resolution, alert, opt-in heuristic timeout) were confirmed by the maintainer; D3 and D4 as proposed. Implementation follows this RFC.
**Motivation:** drevo should be able to take part in a distributed transaction run by an external
coordinator, for example the [datorium](https://github.com/ice1x/datorium) Unit of Work, next to
PostgreSQL's `PREPARE TRANSACTION`. That needs the classic resource-manager contract:

- `prepare(tx, gid)` validates and **durably** records the transaction. After a successful prepare,
  `commit_prepared(gid)` **must not fail** for any reason except I/O.
- `commit_prepared(gid)` and `rollback_prepared(gid)` resolve it, possibly from another session or
  process, and possibly after a restart.
- `list_prepared()` lets a coordinator find and resolve in-doubt transactions.

## 1. Where the engine is today (evidence)

- Registered transactions (`NativeGraph::tx_begin` / `tx_engine` / `tx_commit` / `tx_rollback`,
  surfaced by `NativeService` and #554) run on a **working copy**: an `Arc<Inner>` cloned from the
  live graph at `begin`.
- `commit_parts` (`drevo-core/src/native.rs`) is **whole-graph optimistic**: it fails with
  `CommitError::Conflict` unless `Arc::ptr_eq(live, base)`, i.e. if *any* commit happened since
  `begin`. It then validates constraints, appends the transaction's `WalOp`s as one fsynced group
  and swaps `live = working`.
- Autocommit statements are writes on the live graph too, so they also move `live` and make an open
  transaction's commit conflict.
- The WAL is JSON-lines of serde `WalOp` (`UpsertNode(Node)` with the full record, `DeleteNode(id)`,
  `UpsertEdge`, `DeleteEdge`, embeddings), replayed in order on open (`NativeGraph::replay`). It is
  also shipped to read replicas (`replica.rs`).

**Consequence.** Because conflicts are detected at whole-graph granularity, and `UpsertNode` writes
whole records, a prepared transaction can only be guaranteed to commit if **nothing else changes the
graph between `prepare` and its resolution**. Re-applying its ops on a newer graph at
`commit_prepared` would either conflict (breaking the contract) or silently overwrite concurrent
changes to the same records (a lost update).

## 2. Proposed semantics

### 2.1 Prepare

`NativeService::prepare_tx(tx, gid) -> Result<(), PrepareError>`:

1. Take the writer lock that `commit_parts` takes.
2. Run the commit checks: base is live (else `Conflict`), and constraints hold on the working copy
   (else `Constraint`).
3. Append `WalOp::Prepare { gid, ops }` (the transaction's write set) and fsync.
4. Move the transaction from the registered set into a **prepared table** keyed by `gid`, holding
   `{ base, working, ops, prepared_at }`.
5. Set the graph's **prepared fence**: while any prepared transaction exists, every other commit
   (registered transactions and autocommit writes) is refused (§2.3). Reads are unaffected.

A `gid` is a caller-chosen string, unique among unresolved prepared transactions. A duplicate is
rejected (`PrepareError::DuplicateGid`), as in PostgreSQL. An empty (read-only) transaction may be
prepared; it writes a `Prepare` record with no ops, so that resolution is idempotent and listable.

### 2.2 Resolve

- `commit_prepared(gid)`: append `WalOp::CommitPrepared { gid }`, fsync, swap `live = working`,
  remove the entry, and lift the fence if the table is now empty. With the fence in place `live` is
  still `base`, so this cannot conflict. The only failure is I/O, which the coordinator retries.
- `rollback_prepared(gid)`: append `WalOp::RollbackPrepared { gid }`, fsync, drop the entry, and lift
  the fence if the table is empty.
- An unknown `gid` returns `UnknownGid`. Resolving a `gid` twice is therefore an error the
  coordinator can treat as "already resolved". It must still record its own decision first, as in
  any 2PC.

### 2.3 Other writers while a transaction is prepared

Proposal (**decision D1**): **fail fast**. While the fence is up, any other commit or autocommit
write returns a new retryable error `PreparedTransactionPending { gids }`:

- Bolt maps it to `Neo.TransientError.Transaction.LockClientStopped`, a retryable class drivers
  already back off on;
- HTTP returns `503`;
- drevo-py raises `TransactionConflict`.

Reads keep working.

Alternative D1-b is to block writers until resolution (with a deadline). It is friendlier to
callers, but it parks threads, interacts with the statement timeout (#547), and turns a stuck
coordinator into a server-wide stall. Fail-fast keeps the server responsive and makes the cause
visible.

The prepared window is meant to be short (milliseconds to seconds, between the coordinator's
prepare round and its decision), so blocking all writes is acceptable for v1. Finer-grained write-set
locking would need key-level conflict detection in `commit_parts` instead of the whole-graph check.
That is listed in §7, not proposed here.

### 2.4 Abandoned prepared transactions

Proposal (**decision D2**): **never resolve automatically by default**, as in PostgreSQL. A prepared
transaction is a promise to the coordinator, and resolving it unilaterally can break atomicity
across stores.

Because the fence blocks writers, an abandoned one is an outage, so:

- `list_prepared()` reports `{gid, prepared_at, op_count}` on every surface;
- `/metrics` exports `drevo_prepared_transactions` (gauge) and the age of the oldest;
- an ERROR goes to the problem feed (#552) once a prepared transaction is older than
  `DREVO_PREPARED_TX_WARN_SECS` (default 60);
- an operator can resolve it manually (`CALL drevo.tx.rollbackPrepared(gid)`);
- opt-in `DREVO_PREPARED_TX_TIMEOUT_SECS` performs a **heuristic rollback** after the deadline. It
  is logged at ERROR and recorded in the WAL as `RollbackPrepared { gid, heuristic: true }`, so a
  later `commit_prepared` reports `HeuristicRollback` instead of `UnknownGid` (the XA "heuristic
  outcome" model).

### 2.4a Implementation notes (from mapping the engine)

- **Prepared state lives in `Inner`.** A `prepared: BTreeMap<gid, (prepared_at, Vec<WalOp>)>` sits
  next to the graph, so that a single mechanism serves runtime, `open_durable` / `replay`, WAL-tailing
  replicas and compaction. `Inner::apply_wal_op` stores the ops on `Prepare`, applies them on
  `CommitPrepared` and drops them on `RollbackPrepared`.
- **Apply at resolution, don't swap a snapshot.** Prepare removes the transaction's slot, keeping only
  its ops. `commit_prepared` applies those ops to live, which the fence guarantees is still the
  transaction's base, so the result is exactly the working copy. This matters for recovery. Writers
  that mutate in place release the lock *before* their WAL line is queued, so an autocommit that
  became visible before `begin` can be logged after the `Prepare` line. Applying the prepared ops at
  the `CommitPrepared` position reproduces the runtime order; applying them at `Prepare`, or swapping
  a snapshot taken there, would let that late line overwrite them.
- **The fence is checked under the writer lock.** A fallible `write_for_commit()` replaces the bare
  `write()` at every writer call site (autocommit node/edge/embedding writes, batches, imports,
  replica apply, `commit_parts`). The writer lock, the resolvers and `compact_wal` keep the bare lock.
  The fence is simply `!live.prepared.is_empty()`.
- **WAL line and change-feed may differ.** `Prepare` writes a WAL line but nothing to the feed, since
  prepared state is invisible. `CommitPrepared{gid}` writes the small record to the WAL but the
  expanded write set to the feed, so indexes and in-process replicas see ordinary upserts and
  deletes. Feed subscribers that match `WalOp` exhaustively get no-op arms for the new variants.
  They only ever see them in the open-time seed from `to_wal_ops()`, which includes unresolved
  `Prepare` records after the graph snapshot.

### 2.5 Crash recovery

Replay is extended so that:

- `Prepare { gid, ops }` re-creates the prepared entry, with `working = base + ops`, where `base` is
  the state rebuilt so far;
- `CommitPrepared` and `RollbackPrepared` resolve it.

Any `Prepare` without a resolution at the end of the log is **restored as prepared**, with the fence
up. This is correct because nothing else committed after the prepare: the fence guaranteed that
before the crash, and replay reproduces the same order.

WAL compaction (`compact_wal`, which rewrites the log as a state snapshot) must carry unresolved
`Prepare` records forward after the snapshot, so a compaction never forgets an in-doubt transaction.

Read replicas ship the new records too. A replica applies `CommitPrepared` and ignores the rest,
since it serves only committed state.

### 2.6 Format compatibility

The three records are new `WalOp` variants. A binary without them cannot parse a log that contains
them. That is the safe failure (refuse to open) rather than silently dropping a prepare, but it is a
format change.

**Caveat (older binaries):** `open_durable` treats an unparseable *last* line as a torn tail and
truncates it. An older binary opening a log whose final line is an unresolved `Prepare` would
therefore **drop it silently** rather than refuse to open. Older WAL tailers likewise skip unknown
lines. This is why the downgrade procedure below is mandatory, not advisory.

The native WAL has no format version marker today. The `FORMAT_MAJOR` stamp from #216 belonged to
the removed redb store. So a downgrade requires resolving every prepared transaction and compacting
first, which removes the records, and the release notes must say so. Adding a WAL format marker is
worth doing alongside, but it is not required for correctness.

Logs that never used 2PC are byte-for-byte unchanged.

## 3. Surfaces

| Surface | Prepare | Resolve | List |
|---|---|---|---|
| Rust `NativeService` | `prepare_tx(tx, gid)` | `commit_prepared(gid)`, `rollback_prepared(gid)` | `list_prepared()` |
| drevo-py | `Transaction.prepare(gid)` (closes the handle to it) | `Drevo.commit_prepared(gid)`, `Drevo.rollback_prepared(gid)` | `Drevo.list_prepared()` |
| Cypher (Bolt / HTTP) | `CALL drevo.tx.prepare($gid)` inside an explicit Bolt transaction | `CALL drevo.tx.commitPrepared($gid)`, `CALL drevo.tx.rollbackPrepared($gid)` | `CALL drevo.tx.listPrepared()` |
| HTTP admin | — | `POST /transactions/prepared/{gid}/commit`, `/rollback` | `GET /transactions/prepared` |

Bolt has no native 2PC messages. Exposing it through procedures keeps every Neo4j driver usable as
a coordinator client.

## 4. Errors

```
PrepareError   = Conflict | Constraint(ConstraintViolation) | DuplicateGid | Io(String)
ResolveError   = UnknownGid | HeuristicRollback | Io(String)
CommitError   += PreparedTransactionPending { gids }      // other writers while fenced
```

drevo-py adds `PreparedTransactionError(TransactionError)` with `UnknownGidError` and
`HeuristicRollbackError`. `PreparedTransactionPending` surfaces as `TransactionConflict`, because it
is retryable.

## 5. Acceptance tests (from #556, made concrete)

1. prepare → commit_prepared: the writes are visible and survive a reopen.
2. prepare → rollback_prepared: no trace, including after a reopen.
3. **Crash between prepare and resolution:** cut the log after `Prepare` is fsynced, in the style of
   the existing truncation-based crash tests (`tests/native_wal_crash_tests.rs`,
   `drevo-core/tests/acid_atomicity.rs`). On reopen the transaction is
   listed as prepared, the fence is up, `commit_prepared` succeeds, and the writes are visible.
4. Same as 3, resolved with `rollback_prepared`.
5. A crash after `CommitPrepared` is fsynced but before the swap is visible: replay applies it.
6. **Fence:** while prepared, autocommit and other transactions get `PreparedTransactionPending`,
   reads succeed, and after resolution writes succeed again.
7. **commit_prepared never conflicts:** property test over random interleavings of other writers
   and resolution order. No `Conflict` ever reaches `commit_prepared`.
8. Duplicate gid is rejected. An unknown gid gives `UnknownGid`. Double resolution is an error.
9. Compaction with an unresolved prepare keeps it, both across reopen and in `list_prepared`.
10. The heuristic timeout (opt-in) rolls back and reports `HeuristicRollback` on a late commit.
11. Replica: applies `CommitPrepared` only.
12. Python and Cypher surfaces: round-trip of the above through drevo-py and Bolt.

These extend the ACID conformance suites in `drevo-core/tests/acid_*.rs`.

## 6. Decisions (accepted)

- **D1:** while a transaction is prepared, other writers **fail fast** with a retryable error (§2.3).
  The alternative is to block until resolution.
- **D2:** no automatic resolution by default. Warn in the problem feed and offer an opt-in heuristic
  rollback timeout (§2.4).
- **D3:** a `gid` is any caller string, unique among unresolved prepared transactions; a duplicate
  is rejected.
- **D4:** 2PC is exposed over Bolt and HTTP as `drevo.tx.*` procedures (§3), not new wire messages.

## 7. Out of scope / future

- Key-level (write-set) conflict detection, which would let unrelated writers proceed while a
  transaction is prepared. That is a change to `commit_parts` that benefits ordinary transactions
  too, and deserves its own RFC.
- drevo acting as a **coordinator**.
- XA wire compatibility.
