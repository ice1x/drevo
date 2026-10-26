//! The per-record two-phase-commit fence: while a transaction is prepared,
//! another transaction's commit waits (`CommitError::PreparedPending`) only
//! if it touches what the prepared one wrote, or would break a declared
//! constraint once the prepared one commits. Everything else commits, and
//! resolving the prepared transaction afterwards still cannot fail, also
//! after a restart.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{CommitError, Constraint, NativeGraph, NativeTxId};
use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_fence_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("native.wal")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
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
        kind: "knows".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn rename(title: &str) -> NodePatch {
    NodePatch {
        title: Some(title.to_string()),
        ..NodePatch::default()
    }
}

fn titles(g: &NativeGraph) -> Vec<String> {
    let mut t: Vec<String> = g
        .all_nodes()
        .unwrap()
        .iter()
        .map(|n| n.title.clone())
        .collect();
    t.sort();
    t
}

fn pending(r: Result<(), CommitError>) -> bool {
    matches!(r, Err(CommitError::PreparedPending(ref gids)) if gids == &["g"])
}

/// A transaction that applies `f` to its engine.
fn tx_doing(g: &NativeGraph, f: impl FnOnce(&dyn GraphEngine)) -> NativeTxId {
    let tx = g.tx_begin();
    f(&g.tx_engine(tx).expect("open"));
    tx
}

#[test]
fn a_commit_on_other_records_goes_through_and_the_prepared_one_still_commits() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();
    let prepared = tx_doing(&g, |e| {
        e.update_node(a.id, rename("a2")).unwrap();
    });
    let before = tx_doing(&g, |e| {
        e.update_node(b.id, rename("b2")).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    let after = tx_doing(&g, |e| {
        e.create_node(node("c")).unwrap();
    });
    g.tx_commit(before)
        .expect("began before the prepare, other record");
    g.tx_commit(after)
        .expect("began after the prepare, new record");
    assert_eq!(titles(&g), vec!["a", "b2", "c"]);
    g.commit_prepared("g").unwrap();
    assert_eq!(titles(&g), vec!["a2", "b2", "c"]);
}

#[test]
fn a_commit_on_a_record_the_prepared_one_wrote_waits() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let prepared = tx_doing(&g, |e| {
        e.update_node(a.id, rename("mine")).unwrap();
    });
    let rival = tx_doing(&g, |e| {
        e.update_node(a.id, rename("theirs")).unwrap();
    });
    let deleter = tx_doing(&g, |e| {
        e.delete_node(a.id).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    assert!(pending(g.tx_commit(rival)));
    assert!(pending(g.tx_commit(deleter)));
    g.commit_prepared("g").unwrap();
    assert_eq!(titles(&g), vec!["mine"]);
}

#[test]
fn relationships_and_the_nodes_they_need_are_fenced_both_ways() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();
    let c = g.create_node(node("c")).unwrap();
    // The prepared transaction links a→b and deletes c.
    let prepared = tx_doing(&g, |e| {
        e.create_edge(link(a.id, b.id)).unwrap();
        e.delete_node(c.id).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    // Deleting an endpoint its relationship needs waits.
    let t = tx_doing(&g, |e| e.delete_node(b.id).unwrap());
    assert!(pending(g.tx_commit(t)));
    // Linking to the node it deleted waits.
    let t = tx_doing(&g, |e| {
        e.create_edge(link(a.id, c.id)).unwrap();
    });
    assert!(pending(g.tx_commit(t)));
    // Renaming an endpoint is fine: the relationship does not depend on it.
    let t = tx_doing(&g, |e| {
        e.update_node(a.id, rename("a2")).unwrap();
    });
    g.tx_commit(t).expect("endpoint properties are not fenced");
    g.commit_prepared("g").unwrap();
    assert_eq!(titles(&g), vec!["a2", "b"]);
    assert_eq!(g.all_edges().unwrap().len(), 1);
}

#[test]
fn a_commit_that_would_break_a_constraint_once_the_prepared_one_commits_waits() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "ticket".into(),
        property: "key".into(),
    })
    .unwrap();
    let prepared = tx_doing(&g, |e| {
        e.create_node(ticket("mine", "BUG-1")).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    let same_key = tx_doing(&g, |e| {
        e.create_node(ticket("theirs", "BUG-1")).unwrap();
    });
    assert!(pending(g.tx_commit(same_key)));
    let other_key = tx_doing(&g, |e| {
        e.create_node(ticket("other", "BUG-2")).unwrap();
    });
    g.tx_commit(other_key).expect("a different key commits");
    g.commit_prepared("g").expect("resolution cannot fail");
    assert_eq!(titles(&g), vec!["mine", "other"]);
}

#[test]
fn after_a_rollback_the_held_back_commit_succeeds_on_retry() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let prepared = tx_doing(&g, |e| {
        e.update_node(a.id, rename("mine")).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    let rival = tx_doing(&g, |e| {
        e.update_node(a.id, rename("theirs")).unwrap();
    });
    assert!(pending(g.tx_commit(rival)));
    g.rollback_prepared("g").unwrap();
    let retry = tx_doing(&g, |e| {
        e.update_node(a.id, rename("theirs")).unwrap();
    });
    g.tx_commit(retry).unwrap();
    assert_eq!(titles(&g), vec!["theirs"]);
}

#[test]
fn direct_writes_still_wait_whatever_they_touch() {
    let g = NativeGraph::new();
    let prepared = tx_doing(&g, |e| {
        e.create_node(node("mine")).unwrap();
    });
    g.tx_prepare(prepared, "g").unwrap();
    assert!(matches!(
        g.create_node(node("direct")),
        Err(CoreError::PreparedTransactionPending(_))
    ));
    g.commit_prepared("g").unwrap();
}

#[test]
fn the_order_survives_a_restart_before_and_after_resolution() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let a = g.create_node(node("a")).unwrap();
        let prepared = tx_doing(&g, |e| {
            e.update_node(a.id, rename("a2")).unwrap();
            e.create_node(node("p")).unwrap();
        });
        g.tx_prepare(prepared, "g").unwrap();
        let t = tx_doing(&g, |e| {
            e.create_node(node("meanwhile")).unwrap();
        });
        g.tx_commit(t).unwrap();
    }
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        assert_eq!(titles(&g), vec!["a", "meanwhile"], "still prepared");
        assert_eq!(g.list_prepared().len(), 1);
        g.commit_prepared("g").unwrap();
        assert_eq!(titles(&g), vec!["a2", "meanwhile", "p"]);
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(titles(&g), vec!["a2", "meanwhile", "p"]);
    assert!(g.list_prepared().is_empty());
}
