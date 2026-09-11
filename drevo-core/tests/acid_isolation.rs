//! ACID conformance — **I (Isolation)** for the default `native-durable`
//! engine, exercised as **Rust** tests directly against the `drevo-core`
//! [`NativeGraph`] seam (no HTTP layer).
//!
//! The engine's guaranteed level is **MVCC snapshot isolation**: a reader that
//! holds a [`GraphSnapshot`](drevo_core::native::GraphSnapshot) — or an open
//! transaction, which reads from the snapshot it began at — sees one frozen,
//! consistent version of the graph, regardless of concurrent writers, and a
//! write–write race is rejected as a conflict rather than silently losing an
//! update. This file is the authoritative proof of that level; deeper MVCC
//! mechanics live in the top crate's `mvcc_*` suites.
//!
//! Part of the ACID conformance set (issue #426).

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::{CommitError, NativeGraph};

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

/// No dirty read: uncommitted writes are invisible to any other reader —
/// a fresh snapshot and a concurrently-open transaction both see nothing,
/// while the writing transaction sees its own buffered write (read-your-writes).
#[test]
fn no_dirty_read_uncommitted_writes_are_invisible() {
    let g = NativeGraph::new();

    let mut writer = g.begin();
    writer.create_node(node("secret")).unwrap();

    assert_eq!(
        g.snapshot().all_nodes().len(),
        0,
        "a snapshot must not observe another transaction's uncommitted write"
    );
    let reader = g.begin();
    assert_eq!(
        reader.all_nodes().len(),
        0,
        "a concurrent transaction must not observe an uncommitted write (no dirty read)"
    );

    // The writer, however, reads its own buffered write.
    assert_eq!(
        writer.all_nodes().len(),
        1,
        "read-your-writes: the writing transaction sees its own buffered node"
    );
}

/// Repeatable read: a snapshot returns the same data for its whole life,
/// unaffected by commits that land after it was taken (no non-repeatable read
/// / phantom within the snapshot). A fresh snapshot sees the new state.
#[test]
fn repeatable_read_snapshot_is_unaffected_by_later_commits() {
    let g = NativeGraph::new();
    g.create_node(node("a")).unwrap();

    let pinned = g.snapshot();
    assert_eq!(pinned.all_nodes().len(), 1);

    // A concurrent committed write must NOT change what the pinned snapshot sees.
    g.create_node(node("b")).unwrap();
    assert_eq!(
        pinned.all_nodes().len(),
        1,
        "snapshot isolation: the pinned view is repeatable, immune to later commits"
    );

    assert_eq!(
        g.snapshot().all_nodes().len(),
        2,
        "a fresh snapshot observes the newly committed state"
    );
}

/// Lost update prevented: two transactions that began from the same state and
/// both write cannot both commit — the second is rejected as a
/// [`CommitError::Conflict`] (optimistic concurrency), and applies nothing.
#[test]
fn lost_update_is_prevented_by_a_commit_conflict() {
    let g = NativeGraph::new();

    let mut t1 = g.begin();
    let mut t2 = g.begin();
    t1.create_node(node("x")).unwrap();
    t2.create_node(node("y")).unwrap();

    t1.commit().expect("the first committer wins");
    assert!(
        matches!(t2.commit(), Err(CommitError::Conflict)),
        "the second transaction must conflict rather than silently lose/overwrite the update"
    );

    assert_eq!(
        g.node_count(),
        1,
        "the losing transaction applied none of its writes"
    );
}

/// A read-only snapshot never blocks or is blocked by writers, and stays
/// internally consistent: a multi-step read over a snapshot taken before a
/// concurrent delete still sees the whole pre-delete graph.
#[test]
fn snapshot_stays_consistent_across_a_concurrent_delete() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    g.create_node(node("b")).unwrap();

    let pinned = g.snapshot();
    // Concurrent autocommit delete of `a`.
    g.delete_node(a.id).unwrap();

    assert_eq!(
        pinned.all_nodes().len(),
        2,
        "the snapshot taken before the delete still sees both nodes (consistent view)"
    );
    assert!(
        pinned.get_node(a.id).is_some(),
        "the deleted node is still visible in the older snapshot"
    );
    assert_eq!(g.node_count(), 1, "the live graph reflects the delete");
}
