//! `tx_prepare` under the per-record rule (#828).
//!
//! A one-phase commit conflicts only with a commit that touched the same
//! node or relationship, and waits only for a prepared transaction it
//! overlaps (`prepared_fence.rs`). Phase one of a two-phase commit follows
//! the same rule:
//!
//! - a commit to other records since the transaction began does not make its
//!   prepare a conflict;
//! - several transactions can be prepared at once, as long as they wrote
//!   different records and can all commit together;
//! - whatever is prepared, in whatever order it is resolved, resolution
//!   cannot fail - also after a restart.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{
    CommitError, Constraint, Isolation, NativeGraph, NativeTxId, PrepareError,
};
use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_prepare_overlap_{}_{}",
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

fn rekey(key: &str) -> NodePatch {
    let mut properties = Properties::default();
    properties.insert("key", Value::String(key.to_string()));
    NodePatch {
        properties: Some(properties),
        ..NodePatch::default()
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

fn gids(g: &NativeGraph) -> Vec<String> {
    g.list_prepared().into_iter().map(|p| p.gid).collect()
}

fn unique_ticket_keys(g: &NativeGraph) {
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "ticket".into(),
        property: "key".into(),
    })
    .unwrap();
}

/// A transaction that applies `f` to its engine.
fn tx_doing(g: &NativeGraph, f: impl FnOnce(&dyn GraphEngine)) -> NativeTxId {
    let tx = g.tx_begin();
    f(&g.tx_engine(tx).expect("open"));
    tx
}

/// Commit a one-node transaction: a commit by "someone else".
fn commit_creating(g: &NativeGraph, title: &str) {
    let tx = tx_doing(g, |e| {
        e.create_node(node(title)).unwrap();
    });
    g.tx_commit(tx).expect("an unrelated commit");
}

// ── a commit since the transaction began ─────────────────────────────────

#[test]
fn a_prepare_after_a_commit_to_other_records_succeeds() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let tx = tx_doing(&g, |e| {
        e.update_node(a.id, rename("a2")).unwrap();
        e.create_node(node("mine")).unwrap();
    });
    commit_creating(&g, "theirs");

    g.tx_prepare(tx, "g").expect("no record in common");
    assert_eq!(gids(&g), vec!["g"]);
    assert_eq!(titles(&g), vec!["a", "theirs"], "prepared, not applied");
    g.commit_prepared("g").expect("resolution cannot fail");
    assert_eq!(titles(&g), vec!["a2", "mine", "theirs"]);
}

#[test]
fn a_prepare_after_a_direct_write_to_other_records_succeeds() {
    let g = NativeGraph::new();
    let tx = tx_doing(&g, |e| {
        e.create_node(node("mine")).unwrap();
    });
    g.create_node(node("theirs")).unwrap();

    g.tx_prepare(tx, "g").expect("no record in common");
    g.commit_prepared("g").unwrap();
    assert_eq!(titles(&g), vec!["mine", "theirs"]);
}

#[test]
fn a_prepare_after_a_commit_to_the_same_record_conflicts() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let tx = tx_doing(&g, |e| {
        e.update_node(a.id, rename("mine")).unwrap();
    });
    g.update_node(a.id, rename("theirs")).unwrap();

    assert_eq!(g.tx_prepare(tx, "g"), Err(PrepareError::Conflict));
    assert!(gids(&g).is_empty());
    assert!(
        !g.tx_rollback(tx),
        "a failed prepare closes the transaction"
    );
    assert_eq!(titles(&g), vec!["theirs"]);
}

#[test]
fn a_prepare_deleting_a_node_that_got_a_relationship_conflicts() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();
    let tx = tx_doing(&g, |e| {
        e.delete_node(a.id).unwrap();
    });
    g.create_edge(link(b.id, a.id)).unwrap();

    assert_eq!(g.tx_prepare(tx, "g"), Err(PrepareError::Conflict));
    assert_eq!(titles(&g), vec!["a", "b"]);
}

#[test]
fn a_prepare_claiming_a_unique_value_someone_committed_meanwhile_conflicts() {
    let g = NativeGraph::new();
    unique_ticket_keys(&g);
    let tx = tx_doing(&g, |e| {
        e.create_node(ticket("mine", "BUG-1")).unwrap();
    });
    g.create_node(ticket("theirs", "BUG-1")).unwrap();

    assert_eq!(g.tx_prepare(tx, "g"), Err(PrepareError::Conflict));
    assert!(gids(&g).is_empty());
    assert_eq!(titles(&g), vec!["theirs"]);
}

#[test]
fn a_serializable_prepare_conflicts_with_any_commit_since_it_began() {
    let g = NativeGraph::new();
    let tx = g.tx_begin_with(Isolation::Serializable);
    g.tx_engine(tx).unwrap().create_node(node("mine")).unwrap();
    commit_creating(&g, "theirs");

    assert_eq!(g.tx_prepare(tx, "g"), Err(PrepareError::Conflict));
    assert_eq!(titles(&g), vec!["theirs"]);
}

// ── several prepared transactions ────────────────────────────────────────

#[test]
fn two_transactions_on_different_records_are_prepared_together() {
    // Resolved in both orders, and with one of them rolled back.
    for (first, second, rolled_back) in [
        ("g1", "g2", None),
        ("g2", "g1", None),
        ("g1", "g2", Some("g1")),
        ("g2", "g1", Some("g1")),
    ] {
        let g = NativeGraph::new();
        let a = g.create_node(node("a")).unwrap();
        let one = tx_doing(&g, |e| {
            e.update_node(a.id, rename("a2")).unwrap();
        });
        let two = tx_doing(&g, |e| {
            e.create_node(node("b")).unwrap();
        });
        g.tx_prepare(one, "g1").unwrap();
        g.tx_prepare(two, "g2")
            .expect("a second prepare on other records");
        assert_eq!(gids(&g), vec!["g1", "g2"]);
        assert_eq!(titles(&g), vec!["a"], "prepared, not applied");

        for gid in [first, second] {
            if rolled_back == Some(gid) {
                g.rollback_prepared(gid).unwrap();
            } else {
                g.commit_prepared(gid).expect("resolution cannot fail");
            }
        }
        assert!(gids(&g).is_empty());
        let expected: Vec<&str> = if rolled_back == Some("g1") {
            vec!["a", "b"]
        } else {
            vec!["a2", "b"]
        };
        assert_eq!(titles(&g), expected, "{first} then {second}");
    }
}

#[test]
fn a_prepare_on_a_record_a_prepared_transaction_wrote_waits() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let one = tx_doing(&g, |e| {
        e.update_node(a.id, rename("mine")).unwrap();
    });
    let two = tx_doing(&g, |e| {
        e.update_node(a.id, rename("theirs")).unwrap();
    });
    g.tx_prepare(one, "g1").unwrap();

    assert_eq!(
        g.tx_prepare(two, "g2"),
        Err(PrepareError::PreparedPending(vec!["g1".to_string()]))
    );
    assert_eq!(gids(&g), vec!["g1"]);
    g.commit_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["mine"]);
}

#[test]
fn a_prepare_linking_to_a_node_a_prepared_transaction_deletes_waits() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();
    let one = tx_doing(&g, |e| {
        e.delete_node(a.id).unwrap();
    });
    let two = tx_doing(&g, |e| {
        e.create_edge(link(b.id, a.id)).unwrap();
    });
    g.tx_prepare(one, "g1").unwrap();

    assert!(matches!(
        g.tx_prepare(two, "g2"),
        Err(PrepareError::PreparedPending(_))
    ));
    g.commit_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["b"]);
}

#[test]
fn a_prepare_claiming_a_unique_value_a_prepared_transaction_claimed_waits() {
    let g = NativeGraph::new();
    unique_ticket_keys(&g);
    let one = tx_doing(&g, |e| {
        e.create_node(ticket("mine", "BUG-1")).unwrap();
    });
    let two = tx_doing(&g, |e| {
        e.create_node(ticket("theirs", "BUG-1")).unwrap();
    });
    let three = tx_doing(&g, |e| {
        e.create_node(ticket("other", "BUG-2")).unwrap();
    });
    g.tx_prepare(one, "g1").unwrap();

    assert_eq!(
        g.tx_prepare(two, "g2"),
        Err(PrepareError::PreparedPending(vec!["g1".to_string()]))
    );
    g.tx_prepare(three, "g3").expect("a different key");
    g.commit_prepared("g3").unwrap();
    g.commit_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["mine", "other"]);
}

/// A value a prepared transaction would free is not free yet: the prepared
/// transaction may still be rolled back. (Holds before and after #828: the
/// second transaction reads the committed graph, where the value is taken.)
#[test]
fn a_unique_value_a_prepared_transaction_frees_cannot_be_taken_yet() {
    let g = NativeGraph::new();
    unique_ticket_keys(&g);
    let old = g.create_node(ticket("old", "BUG-1")).unwrap();
    let one = tx_doing(&g, |e| {
        e.update_node(old.id, rekey("BUG-9")).unwrap();
    });
    g.tx_prepare(one, "g1").unwrap();

    let two = g.tx_begin();
    let created = g
        .tx_engine(two)
        .unwrap()
        .create_node(ticket("new", "BUG-1"));
    assert!(
        created.is_err() || g.tx_prepare(two, "g2").is_err(),
        "BUG-1 is still taken"
    );
    assert_eq!(gids(&g), vec!["g1"]);
    g.rollback_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["old"]);
}

#[test]
fn a_commit_waits_for_any_of_several_prepared_transactions_it_overlaps() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();
    let one = tx_doing(&g, |e| {
        e.update_node(a.id, rename("a2")).unwrap();
    });
    let two = tx_doing(&g, |e| {
        e.update_node(b.id, rename("b2")).unwrap();
    });
    g.tx_prepare(one, "g1").unwrap();
    g.tx_prepare(two, "g2").unwrap();

    let rival = tx_doing(&g, |e| {
        e.update_node(b.id, rename("theirs")).unwrap();
    });
    assert!(matches!(
        g.tx_commit(rival),
        Err(CommitError::PreparedPending(_))
    ));
    commit_creating(&g, "free");
    g.commit_prepared("g2").unwrap();
    g.commit_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["a2", "b2", "free"]);
}

// ── resolution cannot fail ───────────────────────────────────────────────

/// Many transactions, each on its own records, prepared while others commit
/// in between; resolved in a shuffled order, some rolled back. Every
/// resolution succeeds and the graph is what the committed ones wrote.
#[test]
fn resolution_never_fails_whatever_the_order() {
    for round in 0..30u64 {
        let g = NativeGraph::new();
        unique_ticket_keys(&g);
        let seeds: Vec<u64> = (0..6)
            .map(|i| g.create_node(node(&format!("seed{i}"))).unwrap().id)
            .collect();
        let mut x = round.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };

        // Every transaction begins before any other prepares or commits.
        let txs: Vec<NativeTxId> = (0..6usize)
            .map(|i| {
                tx_doing(&g, |e| {
                    e.update_node(seeds[i], rename(&format!("seed{i}-by-{i}")))
                        .unwrap();
                    e.create_node(ticket(&format!("t{i}"), &format!("KEY-{i}")))
                        .unwrap();
                })
            })
            .collect();
        let mut expected: Vec<String> = Vec::new();
        for (i, tx) in txs.into_iter().enumerate() {
            if next() % 2 == 0 {
                commit_creating(&g, &format!("between{i}"));
                expected.push(format!("between{i}"));
            }
            g.tx_prepare(tx, &format!("g{i}"))
                .unwrap_or_else(|e| panic!("round {round}: prepare {i}: {e:?}"));
        }
        assert_eq!(gids(&g).len(), 6);

        let mut order: Vec<usize> = (0..6).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, (next() % (i as u64 + 1)) as usize);
        }
        for i in order {
            let gid = format!("g{i}");
            if next() % 3 == 0 {
                g.rollback_prepared(&gid).unwrap();
                expected.push(format!("seed{i}"));
            } else {
                g.commit_prepared(&gid)
                    .unwrap_or_else(|e| panic!("round {round}: commit {gid}: {e:?}"));
                expected.push(format!("seed{i}-by-{i}"));
                expected.push(format!("t{i}"));
            }
        }
        expected.sort();
        assert_eq!(titles(&g), expected, "round {round}");
        assert!(gids(&g).is_empty());
    }
}

#[test]
fn several_prepared_transactions_survive_a_restart() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let a = g.create_node(node("a")).unwrap();
        let one = tx_doing(&g, |e| {
            e.update_node(a.id, rename("a2")).unwrap();
        });
        let two = tx_doing(&g, |e| {
            e.create_node(node("b")).unwrap();
        });
        let three = tx_doing(&g, |e| {
            e.create_node(node("c")).unwrap();
        });
        g.tx_prepare(one, "g1").unwrap();
        commit_creating(&g, "meanwhile");
        g.tx_prepare(two, "g2").unwrap();
        g.tx_prepare(three, "g3").unwrap();
    }
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        assert_eq!(titles(&g), vec!["a", "meanwhile"], "still prepared");
        assert_eq!(gids(&g), vec!["g1", "g2", "g3"]);
        g.commit_prepared("g2").unwrap();
        g.rollback_prepared("g3").unwrap();
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(gids(&g), vec!["g1"]);
    g.commit_prepared("g1").unwrap();
    assert_eq!(titles(&g), vec!["a2", "b", "meanwhile"]);
}
