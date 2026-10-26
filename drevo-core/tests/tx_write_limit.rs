//! The write limit of an explicit transaction: a transaction buffers its
//! writes in memory until it commits, and `NativeGraph::set_max_tx_writes`
//! caps how many it may buffer. The write that would go over the limit fails
//! with `CoreError::TransactionTooLarge` and changes nothing.

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;

fn note(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn link(from: u64, to: u64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "links".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn too_large(err: &CoreError, limit: usize) -> bool {
    matches!(err, CoreError::TransactionTooLarge { limit: l } if *l == limit)
}

#[test]
fn a_new_graph_has_no_limit() {
    let g = NativeGraph::new();
    assert_eq!(g.max_tx_writes(), None);
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).expect("open");
    for i in 0..50 {
        engine
            .create_node(note(&format!("n{i}")))
            .expect("unlimited");
    }
    assert_eq!(g.tx_write_count(tx), Some(50));
}

#[test]
fn the_limit_is_set_and_cleared() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(10));
    assert_eq!(g.max_tx_writes(), Some(10));
    g.set_max_tx_writes(None);
    assert_eq!(g.max_tx_writes(), None);
    g.set_max_tx_writes(Some(0));
    assert_eq!(g.max_tx_writes(), None, "zero means no limit");
}

#[test]
fn a_registered_transaction_at_the_limit_commits() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(3));
    let tx = g.tx_begin();
    {
        let engine = g.tx_engine(tx).expect("open");
        for i in 0..3 {
            engine
                .create_node(note(&format!("n{i}")))
                .expect("within the limit");
        }
    }
    assert_eq!(g.tx_write_count(tx), Some(3));
    g.tx_commit(tx).expect("commit");
    assert_eq!(g.all_nodes().unwrap().len(), 3);
}

#[test]
fn the_write_over_the_limit_fails_and_changes_nothing() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(3));
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).expect("open");
    let a = engine.create_node(note("a")).unwrap();
    let b = engine.create_node(note("b")).unwrap();
    engine.create_edge(link(a.id, b.id)).unwrap();

    let err = engine.create_node(note("c")).unwrap_err();
    assert!(too_large(&err, 3), "got {err:?}");
    assert!(
        err.to_string().contains('3'),
        "the message names the limit: {err}"
    );

    // Every kind of write is counted.
    let patch = NodePatch {
        title: Some("a2".to_string()),
        ..NodePatch::default()
    };
    assert!(too_large(&engine.update_node(a.id, patch).unwrap_err(), 3));
    assert!(too_large(&engine.delete_node(b.id).unwrap_err(), 3));

    // The refused writes left the transaction as it was.
    assert_eq!(g.tx_write_count(tx), Some(3));
    assert_eq!(engine.all_nodes().unwrap().len(), 2);
    assert_eq!(engine.get_node(a.id).unwrap().unwrap().title, "a");
    g.tx_commit(tx).expect("what fit still commits");
    assert_eq!(g.all_nodes().unwrap().len(), 2);
}

#[test]
fn an_in_process_transaction_has_the_same_limit() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(2));
    let mut tx = g.begin();
    tx.create_node(note("a")).unwrap();
    tx.create_node(note("b")).unwrap();
    let err = tx.create_node(note("c")).unwrap_err();
    assert!(too_large(&err, 2), "got {err:?}");
    assert_eq!(tx.all_nodes().len(), 2);
    tx.commit().expect("commit");
    assert_eq!(g.all_nodes().unwrap().len(), 2);
}

#[test]
fn the_limit_is_per_transaction() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(2));
    let t1 = g.tx_begin();
    let t2 = g.tx_begin();
    for tx in [t1, t2] {
        let engine = g.tx_engine(tx).expect("open");
        engine.create_node(note("x")).unwrap();
        engine.create_node(note("y")).unwrap();
    }
    g.tx_commit(t1).expect("t1");
    g.tx_commit(t2).expect("t2");
    assert_eq!(g.all_nodes().unwrap().len(), 4);
}

#[test]
fn auto_commit_writes_are_not_limited() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(1));
    for i in 0..5 {
        g.create_node(note(&format!("n{i}"))).expect("auto-commit");
    }
    assert_eq!(g.all_nodes().unwrap().len(), 5);
}

#[test]
fn a_lower_limit_applies_to_open_transactions() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).expect("open");
    engine.create_node(note("a")).unwrap();
    engine.create_node(note("b")).unwrap();
    g.set_max_tx_writes(Some(2));
    assert!(too_large(&engine.create_node(note("c")).unwrap_err(), 2));
}

#[test]
fn a_closed_transaction_has_no_write_count() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    assert_eq!(g.tx_write_count(tx), Some(0));
    g.tx_rollback(tx);
    assert_eq!(g.tx_write_count(tx), None);
}

#[test]
fn a_single_statement_transaction_is_not_limited() {
    let g = NativeGraph::new();
    g.set_max_tx_writes(Some(2));
    let tx = g.tx_begin_statement();
    {
        let engine = g.tx_engine(tx).expect("open");
        for i in 0..5 {
            engine
                .create_node(note(&format!("n{i}")))
                .expect("one statement's transaction has no write limit");
        }
    }
    g.tx_commit(tx).expect("commit");
    assert_eq!(g.all_nodes().unwrap().len(), 5);
}
