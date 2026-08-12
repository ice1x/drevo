//! Managed / explicit Bolt transactions across pooled connections — issue #298.
//!
//! A Neo4j driver's `session.execute_write` runs `BEGIN … RUN … COMMIT` as a
//! *managed* transaction over a **connection pool**, retrying the whole unit on
//! a transient failure — and the retry routinely lands on a **different**
//! connection. Before this fix drevo held a single **global** explicit-tx slot
//! per `Drevo` handle, so:
//!
//!  * two concurrent managed transactions collided — the second `BEGIN` got
//!    `Neo.TransientError.Transaction.Outdated: transaction already active`;
//!  * a statement failure left that global slot occupied, so the driver's
//!    retry on another connection hit the very same error; and
//!  * one session's `ROLLBACK` replayed a *concurrent* session's autocommit
//!    writes too, because every mutation journaled into the one global slot.
//!
//! These tests pin the fixed contract: explicit transactions are **per
//! connection** — independent, non-colliding, and each rolls back only its own
//! mutations. The `rollback_undoes_*` battery covers every one of the 11
//! mutation sites the Cypher executor can reach inside a transaction, so the
//! journaling refactor cannot silently drop a site.

#![cfg(all(not(target_arch = "wasm32"), feature = "redb-backend"))]

use std::collections::BTreeMap;

use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session, State};
use drevo::db::Drevo;

fn open() -> Drevo {
    Drevo::open_in_memory().expect("open_in_memory")
}

fn ready(d: &Drevo) -> Session<'_> {
    let mut s = Session::new(d);
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn begin(s: &mut Session) -> Vec<ServerMessage> {
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    })
}
fn commit(s: &mut Session) -> Vec<ServerMessage> {
    s.handle(ClientMessage::Commit)
}
fn rollback(s: &mut Session) -> Vec<ServerMessage> {
    s.handle(ClientMessage::Rollback)
}

fn is_success(m: &ServerMessage) -> bool {
    matches!(m, ServerMessage::Success { .. })
}
fn is_failure(m: &ServerMessage) -> bool {
    matches!(m, ServerMessage::Failure { .. })
}

/// Run a statement and immediately drain its stream, returning the RECORD rows.
/// A mutation `RUN` leaves the session in `(Tx)Streaming`; the following `PULL`
/// drains it back to `(Tx)Ready`, which every subsequent message requires.
fn run_pull(s: &mut Session, query: &str) -> Vec<Vec<Value>> {
    let r = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert!(
        !r.iter().any(is_failure),
        "RUN `{query}` failed: {:?}",
        r.iter().find(|m| is_failure(m))
    );
    let pulled = s.handle(ClientMessage::Pull { extra: dict_n(-1) });
    pulled
        .into_iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect()
}

fn dict_n(n: i64) -> BTreeMap<String, Value> {
    let mut d = BTreeMap::new();
    d.insert("n".to_string(), Value::Integer(n));
    d
}

/// Number of rows a read query returns (drained via autocommit on `s`).
fn count(s: &mut Session, query: &str) -> usize {
    run_pull(s, query).len()
}

/// First column of the first row, if any.
fn scalar(s: &mut Session, query: &str) -> Option<Value> {
    run_pull(s, query).into_iter().next().and_then(|mut r| {
        if r.is_empty() {
            None
        } else {
            Some(r.swap_remove(0))
        }
    })
}

// ---------------------------------------------------------------------------
// Headline: the #298 concurrency contract.
// ---------------------------------------------------------------------------

#[test]
fn two_sessions_hold_explicit_transactions_concurrently() {
    let d = open();
    let mut a = ready(&d);
    let mut b = ready(&d);

    assert!(is_success(&begin(&mut a)[0]), "session A BEGIN");
    // Before the fix this collided with A's tx on the global slot and returned
    // Neo.TransientError.Transaction.Outdated "transaction already active".
    let rb = begin(&mut b);
    assert!(
        is_success(&rb[0]),
        "a second connection must open its own transaction concurrently, got {:?}",
        rb[0]
    );
    assert_eq!(a.state(), State::TxReady);
    assert_eq!(b.state(), State::TxReady);

    // Both can commit independently.
    assert!(is_success(&commit(&mut a)[0]));
    assert!(is_success(&commit(&mut b)[0]));
}

#[test]
fn failed_tx_on_one_connection_does_not_block_begin_on_another() {
    // The exact production tell from #298: a managed tx fails a statement (left
    // open per the Bolt contract until RESET/ROLLBACK), and the driver retries
    // on a *fresh pooled connection*. That retry's BEGIN must succeed.
    let d = open();

    // Seed a node with a fixed title so a duplicate CREATE fails mid-tx.
    let mut setup = ready(&d);
    run_pull(&mut setup, "CREATE (:Item {title: 'dup'})");

    let mut c1 = ready(&d);
    assert!(is_success(&begin(&mut c1)[0]));
    // Colliding CREATE → c1 goes Failed, its tx stays open (contract).
    let bad = c1.handle(ClientMessage::Run {
        query: "CREATE (:Item {title: 'dup'})".to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert!(is_failure(&bad[0]), "duplicate CREATE should fail");
    assert_eq!(c1.state(), State::Failed);

    // c1 is still alive in the pool (not reset, not dropped). The retry lands
    // on a different connection:
    let mut c2 = ready(&d);
    let rb = begin(&mut c2);
    assert!(
        is_success(&rb[0]),
        "a failed-but-open tx on another connection must not block BEGIN here, got {:?}",
        rb[0]
    );
    assert!(is_success(&commit(&mut c2)[0]));
}

#[test]
fn rollback_is_isolated_to_its_own_sessions_writes() {
    // A session's ROLLBACK must undo only *its own* mutations — never a
    // concurrent connection's autocommit write. The global journal got this
    // wrong: B's autocommit CREATE was recorded into A's journal and A's
    // rollback deleted it.
    let d = open();
    let mut a = ready(&d);
    let mut b = ready(&d);

    assert!(is_success(&begin(&mut a)[0]));
    run_pull(&mut a, "CREATE (:Item {title: 'a-node'})"); // journaled in A's tx
    run_pull(&mut b, "CREATE (:Item {title: 'b-node'})"); // B autocommit

    assert!(is_success(&rollback(&mut a)[0]));

    let mut probe = ready(&d);
    assert_eq!(
        count(&mut probe, "MATCH (n:Item {title: 'a-node'}) RETURN n"),
        0,
        "A's own CREATE must be rolled back"
    );
    assert_eq!(
        count(&mut probe, "MATCH (n:Item {title: 'b-node'}) RETURN n"),
        1,
        "a concurrent session's autocommit write must survive A's rollback"
    );
}

#[test]
fn transient_error_is_still_returned_for_nested_begin_on_one_session() {
    // Per-connection transactions still forbid *nesting* on a single
    // connection: a second BEGIN before COMMIT/ROLLBACK is a protocol error.
    let d = open();
    let mut s = ready(&d);
    assert!(is_success(&begin(&mut s)[0]));
    let again = begin(&mut s);
    assert!(is_failure(&again[0]), "nested BEGIN must be rejected");
    assert_eq!(s.state(), State::Failed);
}

// ---------------------------------------------------------------------------
// Per-site rollback battery — every mutation the executor can reach in a tx.
// Each test performs the mutation inside an explicit tx, rolls back, and
// asserts the pre-transaction state is restored. Regression guard for the
// journaling refactor: all 11 sites must keep journaling.
// ---------------------------------------------------------------------------

/// Begin a tx on a fresh session, run `mutation` (drained), roll back, and
/// return a probe session for assertions.
fn seed(d: &Drevo, statements: &[&str]) {
    let mut s = ready(d);
    for q in statements {
        run_pull(&mut s, q);
    }
}

#[test]
fn rollback_undoes_create_node() {
    // Site: executor create_node.
    let d = open();
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "CREATE (:Item {title: 'created'})");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        count(&mut p, "MATCH (n:Item {title: 'created'}) RETURN n"),
        0
    );
}

#[test]
fn rollback_undoes_create_edge() {
    // Site: executor create_edge.
    let d = open();
    seed(
        &d,
        &["CREATE (:Item {title: 'a'})", "CREATE (:Item {title: 'b'})"],
    );
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(
        &mut s,
        "MATCH (a:Item {title: 'a'}), (b:Item {title: 'b'}) CREATE (a)-[:LINK]->(b)",
    );
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(count(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r"), 0);
}

#[test]
fn rollback_undoes_delete_node() {
    // Site: executor delete_node.
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'keep'})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'keep'}) DELETE n");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(count(&mut p, "MATCH (n:Item {title: 'keep'}) RETURN n"), 1);
}

#[test]
fn rollback_undoes_delete_edge() {
    // Site: executor delete_edge.
    let d = open();
    seed(
        &d,
        &[
            "CREATE (:Item {title: 'a'})",
            "CREATE (:Item {title: 'b'})",
            "MATCH (a:Item {title: 'a'}), (b:Item {title: 'b'}) CREATE (a)-[:LINK]->(b)",
        ],
    );
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (:Item)-[r:LINK]->(:Item) DELETE r");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(count(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r"), 1);
}

#[test]
fn rollback_undoes_set_node_property() {
    // Site: executor write_node_property (SET n.prop = v).
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'p', weight: 1})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'p'}) SET n.weight = 999");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        scalar(&mut p, "MATCH (n:Item {title: 'p'}) RETURN n.weight"),
        Some(Value::Integer(1)),
        "property SET must roll back to its pre-tx value"
    );
}

#[test]
fn rollback_undoes_set_edge_property() {
    // Site: executor write_edge_property (SET r.prop = v).
    let d = open();
    seed(
        &d,
        &[
            "CREATE (:Item {title: 'a'})",
            "CREATE (:Item {title: 'b'})",
            "MATCH (a:Item {title: 'a'}), (b:Item {title: 'b'}) CREATE (a)-[:LINK {w: 1}]->(b)",
        ],
    );
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (:Item)-[r:LINK]->(:Item) SET r.w = 999");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        scalar(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r.w"),
        Some(Value::Integer(1)),
    );
}

#[test]
fn rollback_undoes_replace_node_properties() {
    // Site: executor replace_node_properties (SET n = {..}).
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'p', weight: 1})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'p'}) SET n = {title: 'p2'}");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(count(&mut p, "MATCH (n:Item {title: 'p'}) RETURN n"), 1);
    assert_eq!(count(&mut p, "MATCH (n:Item {title: 'p2'}) RETURN n"), 0);
}

#[test]
fn rollback_undoes_merge_node_properties() {
    // Site: executor replace_node_properties (SET n += {..}).
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'p'})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'p'}) SET n += {extra: 7}");
    rollback(&mut s);
    let mut p = ready(&d);
    // Absent property projects as Null (not the merged-in 7).
    assert_eq!(
        scalar(&mut p, "MATCH (n:Item {title: 'p'}) RETURN n.extra"),
        Some(Value::Null),
        "a merged-in property must be gone after rollback"
    );
}

#[test]
fn rollback_undoes_replace_edge_properties() {
    // Site: executor replace_edge_properties (SET r = {..} / r += {..}).
    let d = open();
    seed(
        &d,
        &[
            "CREATE (:Item {title: 'a'})",
            "CREATE (:Item {title: 'b'})",
            "MATCH (a:Item {title: 'a'}), (b:Item {title: 'b'}) CREATE (a)-[:LINK {w: 1}]->(b)",
        ],
    );
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(
        &mut s,
        "MATCH (:Item)-[r:LINK]->(:Item) SET r += {extra: 7}",
    );
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        scalar(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r.extra"),
        Some(Value::Null),
    );
    assert_eq!(
        scalar(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r.w"),
        Some(Value::Integer(1)),
    );
}

#[test]
fn rollback_undoes_remove_node_property() {
    // Site: executor remove_node_property (REMOVE n.prop).
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'p', weight: 5})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'p'}) REMOVE n.weight");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        scalar(&mut p, "MATCH (n:Item {title: 'p'}) RETURN n.weight"),
        Some(Value::Integer(5)),
    );
}

#[test]
fn rollback_undoes_remove_edge_property() {
    // Site: executor remove_edge_property (REMOVE r.prop).
    let d = open();
    seed(
        &d,
        &[
            "CREATE (:Item {title: 'a'})",
            "CREATE (:Item {title: 'b'})",
            "MATCH (a:Item {title: 'a'}), (b:Item {title: 'b'}) CREATE (a)-[:LINK {w: 5}]->(b)",
        ],
    );
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (:Item)-[r:LINK]->(:Item) REMOVE r.w");
    rollback(&mut s);
    let mut p = ready(&d);
    assert_eq!(
        scalar(&mut p, "MATCH (:Item)-[r:LINK]->(:Item) RETURN r.w"),
        Some(Value::Integer(5)),
    );
}

#[test]
fn rollback_undoes_set_node_label() {
    // Site: executor persist_node_labels (SET n:ExtraLabel).
    let d = open();
    seed(&d, &["CREATE (:Item {title: 'p'})"]);
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "MATCH (n:Item {title: 'p'}) SET n:Extra");
    rollback(&mut s);
    let mut p = ready(&d);
    // The secondary label must be gone: a MATCH on :Extra finds nothing.
    assert_eq!(count(&mut p, "MATCH (n:Extra) RETURN n"), 0);
    assert_eq!(count(&mut p, "MATCH (n:Item {title: 'p'}) RETURN n"), 1);
}

// ---------------------------------------------------------------------------
// Commit persists (counterpart to the rollback battery).
// ---------------------------------------------------------------------------

#[test]
fn commit_persists_create_node() {
    let d = open();
    let mut s = ready(&d);
    begin(&mut s);
    run_pull(&mut s, "CREATE (:Item {title: 'committed'})");
    assert!(is_success(&commit(&mut s)[0]));
    let mut p = ready(&d);
    assert_eq!(
        count(&mut p, "MATCH (n:Item {title: 'committed'}) RETURN n"),
        1
    );
}
