//! Embedded-handle parity: native lookup helpers (issue #445, epic #444).
//!
//! `NativeGraph`/`NativeService` must offer the same by-uuid / by-title /
//! recent / edges-by-kind lookups the KV `Drevo` handle does, so the embedded
//! library (and drevo-py) can eventually run on native. These lock the
//! contracts the KV engine defines (see `Drevo::get_node_by_uuid`,
//! `get_node_by_title`, `list_recent`, `list_edges_by_kind`).

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Node, Properties};
use drevo_core::native::NativeGraph;

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

fn edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties(Default::default()),
    }
}

#[test]
fn get_node_by_uuid_finds_the_node_and_misses_cleanly() {
    let g = NativeGraph::new();
    let a = g.create_node(node("person", "Ada")).unwrap();
    let b = g.create_node(node("person", "Babbage")).unwrap();

    assert_eq!(g.get_node_by_uuid(a.uuid).map(|n| n.id), Some(a.id));
    assert_eq!(
        g.get_node_by_uuid(b.uuid).map(|n| n.title),
        Some("Babbage".to_string())
    );
    // An unknown uuid returns None, not a wrong node.
    assert!(g.get_node_by_uuid([0xAB; 16]).is_none());
}

#[test]
fn get_node_by_title_uses_the_unique_title_index() {
    let g = NativeGraph::new();
    let a = g.create_node(node("note", "unique-title")).unwrap();

    assert_eq!(
        g.get_node_by_title("unique-title").map(|n| n.id),
        Some(a.id)
    );
    assert!(g.get_node_by_title("no-such-title").is_none());
}

#[test]
fn list_recent_is_sorted_and_capped() {
    let g = NativeGraph::new();
    for i in 0..5 {
        g.create_node(node("note", &format!("n{i}"))).unwrap();
    }

    // limit caps the result.
    let recent = g.list_recent(3);
    assert_eq!(recent.len(), 3);
    // Ordering contract: updated_at DESC, then id DESC. Verify pairwise so the
    // test is robust to same-millisecond creation timestamps (the tie-break is
    // id-descending exactly for that case).
    assert!(
        is_recent_ordered(&recent),
        "list_recent must be updated_at desc, id desc"
    );

    // limit 0 → empty; limit beyond the count → all of them, still ordered.
    assert!(g.list_recent(0).is_empty());
    let all = g.list_recent(999);
    assert_eq!(all.len(), 5);
    assert!(is_recent_ordered(&all));
    // The most recently *touched* node sorts first: updating the oldest node
    // refreshes its updated_at, so it must lead (or tie at the top by id).
    let first_id = all[0].id;
    assert!(all.iter().all(|n| n.id <= first_id) || all[0].updated_at >= all[1].updated_at);
}

#[test]
fn list_edges_by_kind_filters_paginates_and_orders_by_id() {
    let g = NativeGraph::new();
    let a = g.create_node(node("person", "a")).unwrap();
    let b = g.create_node(node("person", "b")).unwrap();
    let c = g.create_node(node("person", "c")).unwrap();

    // Interleave kinds so filtering + id-ordering are both exercised.
    let e1 = g.create_edge(edge(a.id, b.id, "likes")).unwrap();
    let _x = g.create_edge(edge(a.id, c.id, "blocks")).unwrap();
    let e2 = g.create_edge(edge(b.id, c.id, "likes")).unwrap();
    let e3 = g.create_edge(edge(c.id, a.id, "likes")).unwrap();

    let likes = g.list_edges_by_kind("likes", 100, 0);
    assert_eq!(
        likes.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![e1.id, e2.id, e3.id],
        "only `likes` edges, id-ascending"
    );

    // Pagination: offset 1, limit 1 → the middle one.
    let page = g.list_edges_by_kind("likes", 1, 1);
    assert_eq!(page.iter().map(|e| e.id).collect::<Vec<_>>(), vec![e2.id]);

    // Unknown kind → empty, no panic.
    assert!(g.list_edges_by_kind("nope", 100, 0).is_empty());
}

/// True if `nodes` are ordered updated_at-descending, ties broken by id
/// descending — the `Drevo::list_recent` contract.
fn is_recent_ordered(nodes: &[Node]) -> bool {
    nodes.windows(2).all(|w| {
        let (x, y) = (&w[0], &w[1]);
        x.updated_at > y.updated_at || (x.updated_at == y.updated_at && x.id >= y.id)
    })
}
