//! Per-entity conflict detection (issue #749, slices 2–3): explicit
//! transactions that touched different records both commit; only a real
//! overlap — the same node or relationship, or a relationship whose endpoint
//! the other deleted — still fails with `Conflict`. `Isolation::Serializable`
//! keeps the older whole-graph rule, per graph or per transaction.

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{CommitError, Constraint, Isolation, NativeGraph};
use serde_json::Value;

fn note(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn ticket(title: &str, key: &str) -> NewNode {
    let mut properties = Properties::default();
    properties.insert("key", Value::String(key.to_string()));
    NewNode {
        kind: "ticket".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties,
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

fn body(text: &str) -> NodePatch {
    NodePatch {
        body: Some(text.to_string()),
        ..Default::default()
    }
}

fn body_of(g: &NativeGraph, id: u64) -> String {
    g.get_node(id).unwrap().expect("node exists").body.clone()
}

#[test]
fn snapshot_is_the_default() {
    assert_eq!(NativeGraph::new().isolation(), Isolation::Snapshot);
}

#[test]
fn transactions_on_different_nodes_both_commit() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let a = g.tx_begin();
    let b = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(n1.id, body("a"))
        .unwrap();
    g.tx_engine(b)
        .unwrap()
        .update_node(n3.id, body("b"))
        .unwrap();
    g.tx_commit(b).unwrap();
    g.tx_commit(a).expect("a touched nothing b touched");
    assert_eq!(body_of(&g, n1.id), "a");
    assert_eq!(body_of(&g, n3.id), "b", "b's write survived a's merge");
    let stats = g.conflict_stats();
    assert_eq!(stats.merged, 1);
    assert_eq!(stats.would_pass, 0, "nothing was rejected");
}

#[test]
fn the_cross_order_writes_of_the_deadlock_demo_still_conflict() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n2 = g.create_node(note("2")).unwrap();
    let a = g.tx_begin();
    let b = g.tx_begin();
    for (tx, first, second) in [(a, n1.id, n2.id), (b, n2.id, n1.id)] {
        let e = g.tx_engine(tx).unwrap();
        e.update_node(first, body("x")).unwrap();
        e.update_node(second, body("x")).unwrap();
    }
    g.tx_commit(b).unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(g.conflict_stats().overlap, 1);
}

#[test]
fn an_auto_commit_write_and_a_transaction_on_other_nodes_both_land() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(n1.id, body("tx"))
        .unwrap();
    let elsewhere = g.create_node(note("elsewhere")).unwrap();
    g.tx_commit(a).unwrap();
    assert_eq!(body_of(&g, n1.id), "tx");
    assert!(g.get_node(elsewhere.id).unwrap().is_some());
}

#[test]
fn concurrent_creates_get_distinct_ids_and_both_land() {
    let g = NativeGraph::new();
    let a = g.tx_begin();
    let b = g.tx_begin();
    let na = g.tx_engine(a).unwrap().create_node(note("from a")).unwrap();
    let nb = g.tx_engine(b).unwrap().create_node(note("from b")).unwrap();
    assert_ne!(na.id, nb.id, "two transactions never hand out the same id");
    g.tx_commit(a).unwrap();
    g.tx_commit(b).unwrap();
    assert_eq!(g.get_node(na.id).unwrap().unwrap().title, "from a");
    assert_eq!(g.get_node(nb.id).unwrap().unwrap().title, "from b");
    let direct = g.create_node(note("after")).unwrap();
    assert!(direct.id > na.id.max(nb.id));
}

#[test]
fn a_rolled_back_transaction_leaves_a_gap_in_the_ids() {
    let g = NativeGraph::new();
    let a = g.tx_begin();
    let lost = g.tx_engine(a).unwrap().create_node(note("lost")).unwrap();
    g.tx_rollback(a);
    let next = g.create_node(note("next")).unwrap();
    assert!(next.id > lost.id, "ids are never reused, like a sequence");
    assert!(g.get_node(lost.id).unwrap().is_none());
}

#[test]
fn concurrent_relationships_get_distinct_ids() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n2 = g.create_node(note("2")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let a = g.tx_begin();
    let b = g.tx_begin();
    let ea = g
        .tx_engine(a)
        .unwrap()
        .create_edge(link(n1.id, n2.id))
        .unwrap();
    let eb = g
        .tx_engine(b)
        .unwrap()
        .create_edge(link(n2.id, n3.id))
        .unwrap();
    assert_ne!(ea.id, eb.id);
    g.tx_commit(a).unwrap();
    g.tx_commit(b)
        .expect("edges only read their endpoints; nothing was deleted");
    assert_eq!(g.get_edge(ea.id).unwrap().unwrap().to_id, n2.id);
    assert_eq!(g.get_edge(eb.id).unwrap().unwrap().from_id, n2.id);
}

#[test]
fn attaching_to_a_node_another_transaction_deleted_conflicts_in_either_order() {
    // The attach commits second.
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n2 = g.create_node(note("2")).unwrap();
    let attach = g.tx_begin();
    let delete = g.tx_begin();
    g.tx_engine(attach)
        .unwrap()
        .create_edge(link(n1.id, n2.id))
        .unwrap();
    g.tx_engine(delete).unwrap().delete_node(n2.id).unwrap();
    g.tx_commit(delete).unwrap();
    assert!(matches!(g.tx_commit(attach), Err(CommitError::Conflict)));
    assert_eq!(g.edge_count(), 0, "no relationship to a deleted node");

    // The delete commits second.
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n2 = g.create_node(note("2")).unwrap();
    let attach = g.tx_begin();
    let delete = g.tx_begin();
    g.tx_engine(attach)
        .unwrap()
        .create_edge(link(n1.id, n2.id))
        .unwrap();
    g.tx_engine(delete).unwrap().delete_node(n2.id).unwrap();
    g.tx_commit(attach).unwrap();
    assert!(matches!(g.tx_commit(delete), Err(CommitError::Conflict)));
    assert!(g.get_node(n2.id).unwrap().is_some());
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn a_unique_value_claimed_by_two_transactions_lets_one_win_and_the_other_retry() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "ticket".into(),
        property: "key".into(),
    })
    .unwrap();
    let a = g.tx_begin();
    let b = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .create_node(ticket("a", "BUG-1"))
        .unwrap();
    g.tx_engine(b)
        .unwrap()
        .create_node(ticket("b", "BUG-1"))
        .unwrap();
    g.tx_commit(a).unwrap();
    // B was valid on its own snapshot: the clash exists only because A won,
    // so B gets the retryable conflict, not a constraint error.
    assert!(matches!(g.tx_commit(b), Err(CommitError::Conflict)));
    // The retry sees A's ticket and fails for real.
    let retry = g.tx_begin();
    let err = g
        .tx_engine(retry)
        .unwrap()
        .create_node(ticket("b", "BUG-1"));
    let at_commit = match err {
        Err(_) => true,
        Ok(_) => matches!(g.tx_commit(retry), Err(CommitError::Constraint(_))),
    };
    assert!(at_commit, "the retry reports the violation");
    assert_eq!(g.node_count(), 1);
}

#[test]
fn a_transaction_invalid_on_its_own_keeps_its_constraint_error() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::PropertyExists {
        kind: "ticket".into(),
        property: "key".into(),
    })
    .unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .create_node(note("not a ticket"))
        .unwrap();
    let bad = NewNode {
        kind: "ticket".into(),
        ..note("no key")
    };
    let _ = g.tx_engine(a).unwrap().create_node(bad);
    g.create_node(note("someone else")).unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Constraint(_))));
}

#[test]
fn serializable_keeps_the_whole_graph_rule() {
    let g = NativeGraph::new();
    g.set_isolation(Isolation::Serializable);
    assert_eq!(g.isolation(), Isolation::Serializable);
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(n1.id, body("a"))
        .unwrap();
    g.update_node(n3.id, body("direct")).unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(g.conflict_stats().would_pass, 1);
}

#[test]
fn one_transaction_can_ask_for_serializable() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let strict = g.tx_begin_with(Isolation::Serializable);
    let relaxed = g.tx_begin();
    g.tx_engine(strict)
        .unwrap()
        .update_node(n1.id, body("s"))
        .unwrap();
    g.tx_engine(relaxed)
        .unwrap()
        .update_node(n1.id, body("ignored"))
        .ok();
    g.tx_rollback(relaxed);
    g.update_node(n3.id, body("direct")).unwrap();
    assert!(matches!(g.tx_commit(strict), Err(CommitError::Conflict)));

    let g2 = NativeGraph::new();
    g2.set_isolation(Isolation::Serializable);
    let m1 = g2.create_node(note("1")).unwrap();
    let m3 = g2.create_node(note("3")).unwrap();
    let relaxed = g2.tx_begin_with(Isolation::Snapshot);
    g2.tx_engine(relaxed)
        .unwrap()
        .update_node(m1.id, body("r"))
        .unwrap();
    g2.update_node(m3.id, body("direct")).unwrap();
    g2.tx_commit(relaxed)
        .expect("snapshot asked for explicitly");
}

#[test]
fn a_transaction_older_than_the_history_conflicts() {
    let g = NativeGraph::new();
    g.set_conflict_history(2);
    let n1 = g.create_node(note("1")).unwrap();
    let a = g.tx_begin();
    for i in 0..3 {
        g.create_node(note(&format!("later {i}"))).unwrap();
    }
    g.tx_engine(a)
        .unwrap()
        .update_node(n1.id, body("a"))
        .unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(g.conflict_stats().history, 1);
}

#[test]
fn the_embedded_transaction_merges_too() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let mut tx = g.begin();
    tx.update_node(n1.id, body("tx")).unwrap();
    g.update_node(n3.id, body("direct")).unwrap();
    tx.commit().unwrap();
    assert_eq!(body_of(&g, n1.id), "tx");
    assert_eq!(body_of(&g, n3.id), "direct");
}

#[test]
fn merged_commits_reach_the_change_feed_in_commit_order() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let head = g.change_head();
    let a = g.tx_begin();
    let b = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(n1.id, body("a"))
        .unwrap();
    g.tx_engine(b)
        .unwrap()
        .update_node(n3.id, body("b"))
        .unwrap();
    g.tx_commit(b).unwrap();
    g.tx_commit(a).unwrap();
    let batch = g.changes_since(head);
    assert_eq!(batch.ops.len(), 2, "{:?}", batch.ops);
}

#[test]
fn merged_commits_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_si_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    let (ids, expected) = {
        let g = NativeGraph::open_durable(&wal).unwrap();
        let n1 = g.create_node(note("1")).unwrap();
        let n3 = g.create_node(note("3")).unwrap();
        let a = g.tx_begin();
        let b = g.tx_begin();
        g.tx_engine(a)
            .unwrap()
            .update_node(n1.id, body("a"))
            .unwrap();
        let created = g.tx_engine(a).unwrap().create_node(note("from a")).unwrap();
        g.tx_engine(b)
            .unwrap()
            .update_node(n3.id, body("b"))
            .unwrap();
        let linked = g
            .tx_engine(b)
            .unwrap()
            .create_edge(link(n3.id, n1.id))
            .unwrap();
        g.tx_commit(b).unwrap();
        g.tx_commit(a).unwrap();
        let ids = (n1.id, n3.id, created.id, linked.id);
        (ids, (g.node_count(), g.edge_count()))
    };
    let g = NativeGraph::open_durable(&wal).unwrap();
    assert_eq!((g.node_count(), g.edge_count()), expected);
    assert_eq!(body_of(&g, ids.0), "a");
    assert_eq!(body_of(&g, ids.1), "b");
    assert_eq!(g.get_node(ids.2).unwrap().unwrap().title, "from a");
    assert!(g.get_edge(ids.3).unwrap().is_some());
    let next = g.create_node(note("after reopen")).unwrap();
    assert!(next.id > ids.2, "the id counter is recovered from the log");
    std::fs::remove_dir_all(&dir).ok();
}
