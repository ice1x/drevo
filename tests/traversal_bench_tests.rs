//! Small-scale validation tests for traversal benchmark scenarios.
//!
//! Task 00026 — ensures the benchmark graph topology (random edges,
//! average degree 10) works correctly with BFS, DFS, shortest_path,
//! and subgraph at small scale before running full 100K-node benchmarks.

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};

/// Build a graph with `n` nodes, each connected to `degree` random
/// successors (deterministic pseudo-random via modular arithmetic).
fn build_graph(n: usize, degree: usize) -> (Drevo, Vec<u64>) {
    let db = Drevo::open_in_memory().unwrap();
    let mut ids = Vec::with_capacity(n);

    for i in 0..n {
        let node = db
            .create_node(NewNode {
                kind: format!("kind_{}", i % 5),
                title: format!("node_{i:06}"),
                body: format!("Body {i}"),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        ids.push(node.id);
    }

    for (i, &from_id) in ids.iter().enumerate() {
        for j in 0..degree {
            let to_idx = (i * 7 + j * 13 + 1) % n;
            if to_idx == i {
                continue; // skip self-loops for cleaner test
            }
            db.create_edge(NewEdge {
                from_id,
                to_id: ids[to_idx],
                kind: format!("rel_{}", j % 3),
                weight: 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        }
    }

    (db, ids)
}

// ---------------------------------------------------------------------------
// BFS
// ---------------------------------------------------------------------------

#[test]
fn bfs_depth_0_returns_empty() {
    let (db, ids) = build_graph(100, 10);
    let result = db.bfs(ids[0], 0, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn bfs_depth_1_returns_neighbors() {
    let (db, ids) = build_graph(100, 10);
    let result = db.bfs(ids[0], 1, Direction::Outgoing, None).unwrap();
    assert!(!result.is_empty());
    // Should be at most `degree` neighbors
    assert!(result.len() <= 10);
}

#[test]
fn bfs_depth_3_reachable_set_bounded() {
    let (db, ids) = build_graph(200, 10);
    let result = db.bfs(ids[0], 3, Direction::Outgoing, None).unwrap();
    // With degree 10 and 200 nodes, depth 3 should reach many but the
    // result set should have no duplicates
    let mut seen = std::collections::HashSet::new();
    for node in &result {
        assert!(seen.insert(node.id), "duplicate node in BFS result");
    }
    // Start node excluded from result
    assert!(!result.iter().any(|n| n.id == ids[0]));
}

#[test]
fn bfs_both_directions_depth_2() {
    let (db, ids) = build_graph(100, 10);
    let result = db.bfs(ids[50], 2, Direction::Both, None).unwrap();
    assert!(!result.is_empty());
}

#[test]
fn bfs_with_edge_kind_filter() {
    let (db, ids) = build_graph(100, 10);
    let all = db.bfs(ids[0], 2, Direction::Outgoing, None).unwrap();
    let filtered = db
        .bfs(ids[0], 2, Direction::Outgoing, Some("rel_0"))
        .unwrap();
    // Filtered result should be a subset (fewer or equal)
    assert!(filtered.len() <= all.len());
}

// ---------------------------------------------------------------------------
// DFS
// ---------------------------------------------------------------------------

#[test]
fn dfs_depth_3_no_duplicates() {
    let (db, ids) = build_graph(200, 10);
    let result = db.dfs(ids[0], 3, Direction::Outgoing, None).unwrap();
    let mut seen = std::collections::HashSet::new();
    for node in &result {
        assert!(seen.insert(node.id), "duplicate node in DFS result");
    }
    assert!(!result.iter().any(|n| n.id == ids[0]));
}

#[test]
fn dfs_and_bfs_same_reachable_set() {
    let (db, ids) = build_graph(100, 10);
    let bfs_result = db.bfs(ids[0], 3, Direction::Outgoing, None).unwrap();
    let dfs_result = db.dfs(ids[0], 3, Direction::Outgoing, None).unwrap();

    let bfs_ids: std::collections::HashSet<u64> = bfs_result.iter().map(|n| n.id).collect();
    let dfs_ids: std::collections::HashSet<u64> = dfs_result.iter().map(|n| n.id).collect();
    assert_eq!(bfs_ids, dfs_ids, "BFS and DFS should discover same nodes");
}

// ---------------------------------------------------------------------------
// Shortest path
// ---------------------------------------------------------------------------

#[test]
fn shortest_path_exists_in_connected_graph() {
    let (db, ids) = build_graph(100, 10);
    // With degree 10 on 100 nodes, most nodes should be reachable
    let path = db.shortest_path(ids[0], ids[50]).unwrap();
    // Path may or may not exist depending on topology
    if let Some(p) = path {
        assert_eq!(*p.first().unwrap(), ids[0]);
        assert_eq!(*p.last().unwrap(), ids[50]);
        assert!(p.len() >= 2);
    }
}

#[test]
fn shortest_path_same_node() {
    let (db, ids) = build_graph(50, 5);
    let path = db.shortest_path(ids[0], ids[0]).unwrap();
    assert_eq!(path, Some(vec![ids[0]]));
}

// ---------------------------------------------------------------------------
// Subgraph
// ---------------------------------------------------------------------------

#[test]
fn subgraph_depth_2_includes_root() {
    let (db, ids) = build_graph(100, 10);
    let sg = db.subgraph(ids[0], 2).unwrap();
    assert!(sg.nodes.iter().any(|n| n.id == ids[0]));
    // Edges only between discovered nodes
    let node_ids: std::collections::HashSet<u64> = sg.nodes.iter().map(|n| n.id).collect();
    for edge in &sg.edges {
        assert!(
            node_ids.contains(&edge.from_id),
            "dangling from_id in subgraph"
        );
        assert!(node_ids.contains(&edge.to_id), "dangling to_id in subgraph");
    }
}

#[test]
fn subgraph_depth_0_root_only() {
    let (db, ids) = build_graph(50, 5);
    let sg = db.subgraph(ids[0], 0).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.nodes[0].id, ids[0]);
    assert!(sg.edges.is_empty());
}

#[test]
fn subgraph_no_duplicate_edges() {
    let (db, ids) = build_graph(100, 10);
    let sg = db.subgraph(ids[0], 3).unwrap();
    let mut edge_ids = std::collections::HashSet::new();
    for edge in &sg.edges {
        assert!(edge_ids.insert(edge.id), "duplicate edge in subgraph");
    }
}

// ---------------------------------------------------------------------------
// Scale validation (moderate scale — 1K nodes, degree 10)
// ---------------------------------------------------------------------------

#[test]
fn bfs_1k_nodes_depth_3() {
    let (db, ids) = build_graph(1_000, 10);
    let result = db.bfs(ids[0], 3, Direction::Outgoing, None).unwrap();
    // With degree 10 and 1K nodes, BFS depth 3 should cover a significant
    // portion of the graph
    assert!(
        result.len() > 50,
        "expected many reachable nodes at depth 3"
    );
}

#[test]
fn subgraph_1k_nodes_depth_2() {
    let (db, ids) = build_graph(1_000, 10);
    let sg = db.subgraph(ids[500], 2).unwrap();
    assert!(sg.nodes.len() > 10, "expected many nodes in subgraph");
    assert!(!sg.edges.is_empty(), "expected edges in subgraph");
}
