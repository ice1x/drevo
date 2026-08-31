//! Registered (session-owned) native transactions (RFC #307, ACID I/A —
//! the executor-facing form of `NativeGraph::begin`): a Bolt session keeps
//! only a [`NativeTxId`] between statements, and each statement runs
//! against an ephemeral [`NativeTxEngine`] that implements `GraphEngine`
//! over the transaction's private working copy — buffered writes,
//! read-your-writes, snapshot-isolated from the live graph, applied
//! atomically (one fsynced WAL batch) at commit or discarded at rollback.

use std::collections::HashMap;
use std::sync::Arc;

use drevo::cypher::executor::{execute_on_engine, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::engine::GraphEngine;
use drevo::native::{CommitError, NativeGraph};

fn run_on(engine: &dyn GraphEngine, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute_on_engine(&q, engine, HashMap::new()).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

fn int(result: &ExecResult) -> i64 {
    match result.rows[0].as_slice() {
        [Value::Integer(n)] => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn tx_buffers_writes_and_reads_its_own_view() {
    let g = NativeGraph::new();
    run_on(&g, "CREATE (:Person {title: 'ada'})");

    let tx = g.tx_begin();
    {
        let engine = g.tx_engine(tx).expect("open transaction");
        run_on(&engine, "CREATE (:Person {title: 'bob'})");
        // Read-your-writes inside the transaction (the executor drives the
        // same GraphEngine seam).
        assert_eq!(int(&run_on(&engine, "MATCH (n:Person) RETURN count(*)")), 2);
    }
    // The live graph is untouched until commit.
    assert_eq!(int(&run_on(&g, "MATCH (n:Person) RETURN count(*)")), 1);

    g.tx_commit(tx).expect("commit");
    assert_eq!(int(&run_on(&g, "MATCH (n:Person) RETURN count(*)")), 2);
}

#[test]
fn rollback_discards_and_closes_the_slot() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    {
        let engine = g.tx_engine(tx).expect("open transaction");
        run_on(&engine, "CREATE (:Ghost {title: 'never'})");
    }
    assert!(g.tx_rollback(tx), "first rollback closes the slot");
    assert_eq!(int(&run_on(&g, "MATCH (n) RETURN count(*)")), 0);
    assert!(
        g.tx_engine(tx).is_none(),
        "closed transaction has no engine"
    );
    assert!(!g.tx_rollback(tx), "second rollback is a no-op");
    assert!(
        g.tx_commit(tx).is_err(),
        "commit of a closed transaction errors"
    );
}

#[test]
fn concurrent_commit_conflicts_like_begin_based_transactions() {
    let g = NativeGraph::new();
    let a = g.tx_begin();
    let b = g.tx_begin();
    {
        let ea = g.tx_engine(a).expect("a");
        run_on(&ea, "CREATE (:A {title: 'a'})");
    }
    {
        let eb = g.tx_engine(b).expect("b");
        run_on(&eb, "CREATE (:B {title: 'b'})");
    }
    g.tx_commit(a).expect("first commit wins");
    match g.tx_commit(b) {
        Err(CommitError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
    // Only the winner's write is live.
    assert_eq!(int(&run_on(&g, "MATCH (n) RETURN count(*)")), 1);
}

#[test]
fn commit_is_one_atomic_wal_batch_that_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wal");
    {
        let g = NativeGraph::open_durable(&path).expect("open");
        let tx = g.tx_begin();
        {
            let engine = g.tx_engine(tx).expect("tx");
            run_on(&engine, "CREATE (:Person {title: 'ada'})");
            run_on(&engine, "CREATE (:Person {title: 'bob'})");
        }
        g.tx_commit(tx).expect("commit");
    }
    let g = NativeGraph::open_durable(&path).expect("reopen");
    assert_eq!(int(&run_on(&g, "MATCH (n:Person) RETURN count(*)")), 2);
}

#[test]
fn uncommitted_registered_tx_is_invisible_to_concurrent_readers() {
    let g = Arc::new(NativeGraph::new());
    run_on(&*g, "CREATE (:Seed {title: 's'})");
    let tx = g.tx_begin();
    {
        let engine = g.tx_engine(tx).expect("tx");
        run_on(
            &engine,
            "MATCH (s:Seed) CREATE (s)-[:HAS]->(:Item {title: 'i'})",
        );
        assert_eq!(int(&run_on(&engine, "MATCH (n) RETURN count(*)")), 2);
    }
    // A concurrent reader on the live graph sees the pre-transaction state.
    assert_eq!(int(&run_on(&*g, "MATCH (n) RETURN count(*)")), 1);
    g.tx_commit(tx).expect("commit");
    assert_eq!(
        int(&run_on(&*g, "MATCH (a)-[:HAS]->(b) RETURN count(*)")),
        1
    );
}
