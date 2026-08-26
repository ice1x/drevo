//! The native engine's change-feed (RFC `docs/rfc-native-core.md`, #307,
//! Phase 6.2).
//!
//! Every committed write lands on an ordered [`WalOp`] feed that a secondary
//! index (FTS, vector) tails to stay current without touching the write path:
//! snapshot the graph once, remember [`change_head`], then poll
//! [`changes_since`] and fold each change into the derived index. These tests
//! lock the feed's ordering, cursor arithmetic, transaction semantics
//! (committed writes appear, rolled-back ones do not), and — the headline — a
//! subscriber that reconstructs the exact node set purely from the feed.
//!
//! [`change_head`]: drevo::native::NativeGraph::change_head
//! [`changes_since`]: drevo::native::NativeGraph::changes_since

use std::collections::HashMap;

use drevo::engine::GraphEngine;
use drevo::model::{NewEdge, NewNode, NodePatch};
use drevo::native::{NativeGraph, WalOp};

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn new_edge(from: u64, to: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight: 1.0,
        properties: Default::default(),
    }
}

/// A stand-in secondary index: `id -> title`, maintained purely by folding the
/// change-feed — exactly the shape an FTS/vector indexer would keep.
#[derive(Default)]
struct TitleIndex {
    titles: HashMap<u64, String>,
    cursor: u64,
}

impl TitleIndex {
    fn pull(&mut self, g: &NativeGraph) {
        let batch = g.changes_since(self.cursor);
        assert!(!batch.lagged, "no trimming yet, so a subscriber never lags");
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(n) => {
                    self.titles.insert(n.id, n.title);
                }
                WalOp::DeleteNode(id) => {
                    self.titles.remove(&id);
                }
                WalOp::UpsertEdge(_) | WalOp::DeleteEdge(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }
}

/// The source engine's live `id -> title` node map, for comparison.
fn live_titles(g: &NativeGraph) -> HashMap<u64, String> {
    g.all_nodes()
        .unwrap()
        .into_iter()
        .map(|n| (n.id, n.title.clone()))
        .collect()
}

#[test]
fn feed_records_writes_in_commit_order_and_advances_head() {
    let g = NativeGraph::new();
    assert_eq!(g.change_head(), 0);

    let a = g.create_node(new_node("k", "a")).unwrap();
    let b = g.create_node(new_node("k", "b")).unwrap();
    g.create_edge(new_edge(a.id, b.id, "KNOWS")).unwrap();
    assert_eq!(g.change_head(), 3);

    let batch = g.changes_since(0);
    assert_eq!(batch.cursor, 3);
    assert!(!batch.lagged);
    assert!(matches!(&batch.ops[0], WalOp::UpsertNode(n) if n.title == "a"));
    assert!(matches!(&batch.ops[1], WalOp::UpsertNode(n) if n.title == "b"));
    assert!(matches!(&batch.ops[2], WalOp::UpsertEdge(e) if e.from_id == a.id && e.to_id == b.id));

    // A caught-up cursor yields nothing.
    let tail = g.changes_since(3);
    assert!(tail.ops.is_empty());
    assert_eq!(tail.cursor, 3);
}

#[test]
fn changes_since_returns_only_the_suffix_after_the_cursor() {
    let g = NativeGraph::new();
    g.create_node(new_node("k", "a")).unwrap();
    let mid = g.change_head();
    g.create_node(new_node("k", "b")).unwrap();
    g.create_node(new_node("k", "c")).unwrap();

    let batch = g.changes_since(mid);
    assert_eq!(batch.ops.len(), 2);
    assert!(matches!(&batch.ops[0], WalOp::UpsertNode(n) if n.title == "b"));
    assert!(matches!(&batch.ops[1], WalOp::UpsertNode(n) if n.title == "c"));
}

#[test]
fn subscriber_reconstructs_state_by_tailing_the_feed() {
    let g = NativeGraph::new();
    let mut index = TitleIndex::default();

    // Round 1: some creates, then catch up.
    let a = g.create_node(new_node("k", "alice")).unwrap();
    let b = g.create_node(new_node("k", "bob")).unwrap();
    index.pull(&g);
    assert_eq!(index.titles, live_titles(&g));

    // Round 2: an update (rename), a new node, and a delete — then catch up
    // incrementally from the previous cursor.
    g.update_node(
        a.id,
        NodePatch {
            title: Some("alice2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    g.create_node(new_node("k", "carol")).unwrap();
    g.delete_node(b.id).unwrap();
    index.pull(&g);
    assert_eq!(index.titles, live_titles(&g));
    assert!(!index.titles.values().any(|t| t == "bob"));
    assert!(index.titles.values().any(|t| t == "alice2"));
}

#[test]
fn committed_transaction_appears_on_feed_but_rollback_does_not() {
    let g = NativeGraph::new();

    // A committed transaction contributes its ops to the feed.
    let mut tx = g.begin();
    tx.create_node(new_node("k", "committed")).unwrap();
    tx.commit().unwrap();
    assert_eq!(g.change_head(), 1);

    // A dropped (rolled-back) transaction contributes nothing.
    let mut tx2 = g.begin();
    tx2.create_node(new_node("k", "discarded")).unwrap();
    drop(tx2);
    assert_eq!(g.change_head(), 1);

    let ops = g.changes_since(0).ops;
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], WalOp::UpsertNode(n) if n.title == "committed"));
}

#[test]
fn delete_emits_delete_ops_on_the_feed() {
    let g = NativeGraph::new();
    let a = g.create_node(new_node("k", "a")).unwrap();
    let b = g.create_node(new_node("k", "b")).unwrap();
    let e = g.create_edge(new_edge(a.id, b.id, "KNOWS")).unwrap();
    let head = g.change_head();

    g.delete_edge(e.id).unwrap();
    g.delete_node(a.id).unwrap();

    let ops = g.changes_since(head).ops;
    assert!(matches!(ops[0], WalOp::DeleteEdge(id) if id == e.id));
    assert!(matches!(ops[1], WalOp::DeleteNode(id) if id == a.id));
    let _ = b;
}

// ---------------------------------------------------------------------------
// History trimming (bounded memory + the lagged/re-snapshot path)
// ---------------------------------------------------------------------------

#[test]
fn trim_before_drops_history_and_advances_the_floor() {
    let g = NativeGraph::new();
    for i in 0..5 {
        g.create_node(new_node("k", &format!("n{i}"))).unwrap();
    }
    assert_eq!(g.change_head(), 5);
    assert_eq!(g.change_floor(), 0);

    // A subscriber that has consumed through seq 3 lets the owner trim <= 3.
    let floor = g.trim_before(3);
    assert_eq!(floor, 3);
    assert_eq!(g.change_floor(), 3);

    // Reading from the floor still returns the retained suffix (seq 4, 5).
    let batch = g.changes_since(3);
    assert!(!batch.lagged);
    assert_eq!(batch.ops.len(), 2);
    assert!(matches!(&batch.ops[0], WalOp::UpsertNode(n) if n.title == "n3"));
    assert!(matches!(&batch.ops[1], WalOp::UpsertNode(n) if n.title == "n4"));
    assert_eq!(batch.cursor, 5);
}

#[test]
fn trim_is_clamped_and_never_loses_unproduced_or_reverses() {
    let g = NativeGraph::new();
    g.create_node(new_node("k", "a")).unwrap();
    g.create_node(new_node("k", "b")).unwrap();

    // Trimming beyond head is clamped to head — never trims a future change.
    assert_eq!(g.trim_before(999), 2);
    assert_eq!(g.change_floor(), 2);

    // A lower cursor cannot move the floor backwards.
    assert_eq!(g.trim_before(1), 2);
    assert_eq!(g.change_floor(), 2);

    // trim_before(0) on a fresh feed is a no-op.
    let g2 = NativeGraph::new();
    assert_eq!(g2.trim_before(0), 0);
}

#[test]
fn lagged_subscriber_re_snapshots_then_tails_to_correct_state() {
    let g = NativeGraph::new();

    // A subscriber catches up, then goes idle.
    let mut index = TitleIndex::default();
    g.create_node(new_node("k", "alice")).unwrap();
    index.pull(&g);
    assert_eq!(index.titles, live_titles(&g));
    let stale_cursor = index.cursor;

    // Meanwhile the graph churns and the owner trims past the idle subscriber.
    let b = g.create_node(new_node("k", "bob")).unwrap();
    g.create_node(new_node("k", "carol")).unwrap();
    g.delete_node(b.id).unwrap();
    g.trim_before(g.change_head());

    // The idle subscriber's cursor is now below the floor: it must be told to
    // re-snapshot rather than silently miss the trimmed changes.
    let batch = g.changes_since(stale_cursor);
    assert!(batch.lagged, "a cursor below the floor must report lagged");

    // On `lagged`, the subscriber discards its state, re-seeds from a fresh
    // snapshot, and resumes tailing from the reported cursor — reaching the
    // correct state.
    let mut resynced = TitleIndex {
        titles: live_titles(&g),
        cursor: batch.cursor,
    };
    assert_eq!(resynced.titles, live_titles(&g));
    // A subsequent write is picked up incrementally on the resumed cursor.
    g.create_node(new_node("k", "dave")).unwrap();
    resynced.pull(&g);
    assert_eq!(resynced.titles, live_titles(&g));
    assert!(resynced.titles.values().any(|t| t == "dave"));
}
