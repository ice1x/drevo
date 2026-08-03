//! Bounded / paginated adjacency scan — public-API behavior (#243 slice 3).
//!
//! `outgoing_adjacency_page` / `incoming_adjacency_page` walk a node's
//! adjacency index in bounded-memory chunks (at most `limit` entries per call)
//! with an opaque cursor, instead of materialising the whole neighbor set the
//! way `edges_of` does. These tests pin the observable contract: each page is
//! bounded by `limit`, the cursor protocol reconstructs the *complete* edge set
//! exactly once, entries carry the denormalized neighbor id + kind, and a
//! supernode is consumed in `ceil(N / limit)` pages of `<= limit` entries.

use std::collections::BTreeSet;

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};

fn node(db: &Drevo, title: &str) -> u64 {
    db.create_node(NewNode {
        kind: "n".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .expect("create node")
    .id
}

fn edge(db: &Drevo, from: u64, to: u64, kind: &str) -> u64 {
    db.create_edge(NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .expect("create edge")
    .id
}

/// Drain every outgoing page for `node`, `limit` at a time, and return the
/// flattened entries in page order.
fn drain_outgoing(db: &Drevo, node: u64, limit: usize) -> Vec<drevo::db::AdjacencyEntry> {
    let mut all = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page = db
            .outgoing_adjacency_page(node, cursor.as_deref(), limit)
            .expect("page");
        assert!(
            page.entries.len() <= limit,
            "a page must not exceed the limit"
        );
        all.extend(page.entries);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    all
}

#[test]
fn pages_reconstruct_the_full_outgoing_edge_set_exactly_once() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let mut expected_edges = BTreeSet::new();
    for i in 0..25 {
        let t = node(&db, &format!("t{i}"));
        expected_edges.insert(edge(&db, a, t, "knows"));
    }

    // Paginate at several page sizes; each must recover the same edge set.
    for limit in [1usize, 3, 7, 25, 100] {
        let drained = drain_outgoing(&db, a, limit);
        let got: BTreeSet<u64> = drained.iter().map(|e| e.edge_id).collect();
        assert_eq!(
            got, expected_edges,
            "limit={limit} must cover every edge once"
        );
        assert_eq!(
            drained.len(),
            expected_edges.len(),
            "limit={limit} must yield no duplicates"
        );
    }
}

#[test]
fn page_entries_match_edges_of_ids_and_kinds() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    let c = node(&db, "c");
    edge(&db, a, b, "knows");
    edge(&db, a, c, "likes");

    // A page large enough to hold everything is one page, next = None.
    let page = db.outgoing_adjacency_page(a, None, 10).unwrap();
    assert!(page.next.is_none(), "a non-full page ends iteration");

    // Entry (neighbor_id, kind) must agree with the full edges_of view.
    let mut from_page: Vec<(u64, String)> = page
        .entries
        .iter()
        .map(|e| (e.neighbor_id, e.kind.clone()))
        .collect();
    from_page.sort();
    let mut from_edges: Vec<(u64, String)> = db
        .edges_of(a, Direction::Outgoing)
        .unwrap()
        .into_iter()
        .map(|e| (e.to_id, e.kind))
        .collect();
    from_edges.sort();
    assert_eq!(from_page, from_edges);
}

#[test]
fn incoming_page_reports_from_id_as_neighbor() {
    let db = Drevo::open_in_memory().unwrap();
    let hub = node(&db, "hub");
    let x = node(&db, "x");
    let y = node(&db, "y");
    edge(&db, x, hub, "src");
    edge(&db, y, hub, "src");

    let page = db.incoming_adjacency_page(hub, None, 10).unwrap();
    let mut neighbors: Vec<u64> = page.entries.iter().map(|e| e.neighbor_id).collect();
    neighbors.sort_unstable();
    assert_eq!(
        neighbors,
        vec![x, y],
        "incoming neighbor is the edge's from_id"
    );
}

#[test]
fn limit_zero_is_empty_and_terminal() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    edge(&db, a, b, "knows");

    let page = db.outgoing_adjacency_page(a, None, 0).unwrap();
    assert!(page.entries.is_empty());
    assert!(page.next.is_none(), "limit 0 does not offer a next cursor");
}

#[test]
fn empty_node_yields_empty_terminal_page() {
    let db = Drevo::open_in_memory().unwrap();
    let lonely = node(&db, "lonely");
    let page = db.outgoing_adjacency_page(lonely, None, 10).unwrap();
    assert!(page.entries.is_empty());
    assert!(page.next.is_none());
}

#[test]
fn supernode_is_consumed_in_bounded_pages() {
    let db = Drevo::open_in_memory().unwrap();
    let hub = node(&db, "hub");
    let spokes: Vec<u64> = (0..500).map(|i| node(&db, &format!("s{i}"))).collect();
    let mut expected = BTreeSet::new();
    for (i, &s) in spokes.iter().enumerate() {
        expected.insert(edge(&db, hub, s, &format!("k{}", i % 4)));
    }

    let limit = 50usize;
    let mut pages = 0usize;
    let mut cursor: Option<Vec<u8>> = None;
    let mut seen = BTreeSet::new();
    loop {
        let page = db
            .outgoing_adjacency_page(hub, cursor.as_deref(), limit)
            .unwrap();
        assert!(page.entries.len() <= limit, "no page exceeds the limit");
        pages += 1;
        for e in &page.entries {
            assert!(seen.insert(e.edge_id), "no edge appears on two pages");
        }
        match page.next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    assert_eq!(seen, expected, "every out-edge surfaced across the pages");
    // Lookahead means no trailing empty page: an exact division is N/limit.
    assert_eq!(
        pages,
        500 / limit,
        "exactly N/limit pages for a full division"
    );
}

#[test]
fn cursor_from_one_node_does_not_leak_into_another() {
    // Two hubs; paging one must never surface the other's edges.
    let db = Drevo::open_in_memory().unwrap();
    let h1 = node(&db, "h1");
    let h2 = node(&db, "h2");
    let mut h1_edges = BTreeSet::new();
    for i in 0..10 {
        let t = node(&db, &format!("h1t{i}"));
        h1_edges.insert(edge(&db, h1, t, "e"));
    }
    for i in 0..10 {
        let t = node(&db, &format!("h2t{i}"));
        edge(&db, h2, t, "e");
    }

    let drained = drain_outgoing(&db, h1, 3);
    let got: BTreeSet<u64> = drained.iter().map(|e| e.edge_id).collect();
    assert_eq!(got, h1_edges, "paging h1 stays within h1's out-prefix");
}
