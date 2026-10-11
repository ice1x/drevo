//! Node titles are ordinary values: any number of nodes may share one, as in
//! Neo4j and Memgraph (issue #595). The title index maps a title to every node
//! carrying it; `get_node_by_title` returns the oldest (lowest id) and
//! `nodes_by_title` all of them.

use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, NodePatch};
use drevo_core::native::NativeGraph;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn note(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn ids(nodes: &[drevo_core::model::Node]) -> Vec<u64> {
    nodes.iter().map(|n| n.id).collect()
}

#[test]
fn two_nodes_can_share_a_title() {
    // A notebook: an embedded note and an external note with the same name.
    let g = NativeGraph::new();
    let external = g.create_node(note("note", "Meeting notes")).unwrap();
    let embedded = g.create_node(note("embedded", "Meeting notes")).unwrap();
    assert_ne!(external.id, embedded.id);
    assert_eq!(g.node_count(), 2);
    assert_eq!(
        g.get_node_by_title("Meeting notes").map(|n| n.id),
        Some(external.id),
        "the oldest node answers a single-title lookup"
    );
    assert_eq!(
        ids(&g.nodes_by_title("Meeting notes")),
        vec![external.id, embedded.id]
    );
    assert!(g.nodes_by_title("missing").is_empty());
}

#[test]
fn renames_and_deletes_keep_the_index_exact() {
    let g = NativeGraph::new();
    let a = g.create_node(note("tag", "rust")).unwrap();
    let b = g.create_node(note("note", "draft")).unwrap();

    // Renaming onto a taken title is allowed.
    g.update_node(
        b.id,
        NodePatch {
            title: Some("rust".into()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&g.nodes_by_title("rust")), vec![a.id, b.id]);
    assert!(g.nodes_by_title("draft").is_empty());

    // Deleting the oldest leaves the other findable.
    g.delete_node(a.id).unwrap();
    assert_eq!(g.get_node_by_title("rust").map(|n| n.id), Some(b.id));

    // Renaming away drops it from the old title.
    g.update_node(
        b.id,
        NodePatch {
            title: Some("go".into()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    assert!(g.get_node_by_title("rust").is_none());
    assert_eq!(g.get_node_by_title("go").map(|n| n.id), Some(b.id));
}

#[test]
fn batches_and_transactions_accept_repeated_titles() {
    let g = NativeGraph::new();
    let batch = g
        .create_nodes(vec![note("n", "dup"), note("n", "ok"), note("n", "dup")])
        .unwrap();
    assert_eq!(batch.len(), 3);
    assert_eq!(g.nodes_by_title("dup").len(), 2);

    let mut tx = g.begin();
    tx.create_node(note("n", "dup")).unwrap();
    tx.commit().unwrap();
    assert_eq!(g.nodes_by_title("dup").len(), 3);
}

#[test]
fn shared_titles_survive_replay_and_compaction() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_shared_titles_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("graph.wal");
    let (first, second) = {
        let g = NativeGraph::open_durable(&wal).unwrap();
        let first = g.create_node(note("note", "Inbox")).unwrap().id;
        let second = g.create_node(note("note", "Inbox")).unwrap().id;
        (first, second)
    };
    {
        let g = NativeGraph::open_durable(&wal).unwrap();
        assert_eq!(ids(&g.nodes_by_title("Inbox")), vec![first, second]);
        g.compact_wal().unwrap();
    }
    let g = NativeGraph::open_durable(&wal).unwrap();
    assert_eq!(ids(&g.nodes_by_title("Inbox")), vec![first, second]);
    assert_eq!(g.get_node_by_title("Inbox").map(|n| n.id), Some(first));
    let _ = std::fs::remove_dir_all(&dir);
}
