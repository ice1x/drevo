//! Cross-algorithm traversal edge-case tests (task 00025).
//!
//! Tests verify consistent behavior of BFS, DFS, shortest_path, subgraph,
//! and neighbors across the same graph topologies: cycles, disconnected
//! components, empty graph, single node, depth 0, self-loops, diamonds,
//! long chains, and large fan-out.

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use std::collections::HashSet;

fn make_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_edge(from: u64, to: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn make_weighted_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

fn node_ids(nodes: &[drevo::model::Node]) -> HashSet<u64> {
    nodes.iter().map(|n| n.id).collect()
}

// ===================================================================
// 1. Empty graph — nonexistent start node
// ===================================================================

#[test]
fn empty_graph_bfs_returns_error_or_empty() {
    let db = Drevo::open_in_memory().unwrap();
    // BFS on nonexistent node — should return empty (node not found
    // is handled gracefully: start node lookup returns None → empty result).
    let result = db.bfs(999, 5, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn empty_graph_dfs_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let result = db.dfs(999, 5, Direction::Outgoing, None).unwrap();
    assert!(result.is_empty());
}

#[test]
fn empty_graph_shortest_path_returns_none() {
    let db = Drevo::open_in_memory().unwrap();
    let result = db.shortest_path(999, 1000).unwrap();
    assert!(result.is_none());
}

#[test]
fn empty_graph_subgraph_returns_error() {
    let db = Drevo::open_in_memory().unwrap();
    assert!(db.subgraph(999, 3).is_err());
}

#[test]
fn empty_graph_neighbors_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let result = db.neighbors(999, Direction::Both, None).unwrap();
    assert!(result.is_empty());
}

// ===================================================================
// 2. Single isolated node — no edges
// ===================================================================

#[test]
fn single_node_all_algorithms_consistent() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(make_node("note", "alone")).unwrap();

    // BFS: no neighbors
    let bfs = db.bfs(n.id, 5, Direction::Both, None).unwrap();
    assert!(bfs.is_empty());

    // DFS: no neighbors
    let dfs = db.dfs(n.id, 5, Direction::Both, None).unwrap();
    assert!(dfs.is_empty());

    // shortest_path to self
    let sp = db.shortest_path(n.id, n.id).unwrap();
    assert_eq!(sp, Some(vec![n.id]));

    // subgraph: only the root node, no edges
    let sg = db.subgraph(n.id, 5).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.nodes[0].id, n.id);
    assert!(sg.edges.is_empty());

    // neighbors: empty
    let nb = db.neighbors(n.id, Direction::Both, None).unwrap();
    assert!(nb.is_empty());
}

// ===================================================================
// 3. Depth 0 — all algorithms
// ===================================================================

#[test]
fn depth_zero_bfs_dfs_return_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();

    // BFS depth 0: empty result
    let bfs = db.bfs(a.id, 0, Direction::Outgoing, None).unwrap();
    assert!(bfs.is_empty());

    // DFS depth 0: empty result
    let dfs = db.dfs(a.id, 0, Direction::Outgoing, None).unwrap();
    assert!(dfs.is_empty());

    // subgraph depth 0: only root, no edges
    let sg = db.subgraph(a.id, 0).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.nodes[0].id, a.id);
    assert!(sg.edges.is_empty());
}

// ===================================================================
// 4. Simple cycle: A -> B -> C -> A
// ===================================================================

#[test]
fn cycle_three_nodes_all_algorithms() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "next")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "next")).unwrap();
    db.create_edge(make_edge(c.id, a.id, "next")).unwrap();

    // BFS from A, outgoing, depth 10 — should find B, C (no duplicates, no infinite loop)
    let bfs = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([b.id, c.id]));

    // DFS from A, outgoing, depth 10 — same set
    let dfs = db.dfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), HashSet::from([b.id, c.id]));

    // shortest_path A -> C: A -> B -> C (2 hops)
    let sp = db.shortest_path(a.id, c.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id, b.id, c.id]);

    // shortest_path C -> A: C -> A (1 hop)
    let sp_back = db.shortest_path(c.id, a.id).unwrap().unwrap();
    assert_eq!(sp_back, vec![c.id, a.id]);

    // subgraph from A depth 10: all 3 nodes, all 3 edges
    let sg = db.subgraph(a.id, 10).unwrap();
    assert_eq!(sg.nodes.len(), 3);
    assert_eq!(sg.edges.len(), 3);
}

// ===================================================================
// 5. Self-loop: A -> A
// ===================================================================

#[test]
fn self_loop_all_algorithms() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let sl = db.create_edge(make_edge(a.id, a.id, "self")).unwrap();

    // BFS: self-loop should not add start node to results
    let bfs = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(bfs.is_empty());

    // DFS: same
    let dfs = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(dfs.is_empty());

    // shortest_path A -> A: trivially [A]
    let sp = db.shortest_path(a.id, a.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id]);

    // subgraph: root node + self-loop edge
    let sg = db.subgraph(a.id, 5).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert_eq!(sg.edges.len(), 1);
    assert_eq!(sg.edges[0].id, sl.id);
}

// ===================================================================
// 6. Disconnected components: {A->B} {C->D}
// ===================================================================

#[test]
fn disconnected_components_all_algorithms() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(c.id, d.id, "link")).unwrap();

    // BFS from A: only finds B, not C or D
    let bfs = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([b.id]));

    // DFS from A: same
    let dfs = db.dfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), HashSet::from([b.id]));

    // shortest_path A -> D: unreachable
    let sp = db.shortest_path(a.id, d.id).unwrap();
    assert!(sp.is_none());

    // shortest_path A -> B: direct
    let sp_ab = db.shortest_path(a.id, b.id).unwrap().unwrap();
    assert_eq!(sp_ab, vec![a.id, b.id]);

    // subgraph from A depth 10: only A and B
    let sg = db.subgraph(a.id, 10).unwrap();
    assert_eq!(node_ids(&sg.nodes), HashSet::from([a.id, b.id]));
    assert_eq!(sg.edges.len(), 1);

    // subgraph from C depth 10: only C and D
    let sg_c = db.subgraph(c.id, 10).unwrap();
    assert_eq!(node_ids(&sg_c.nodes), HashSet::from([c.id, d.id]));
    assert_eq!(sg_c.edges.len(), 1);
}

// ===================================================================
// 7. Diamond graph: A -> B, A -> C, B -> D, C -> D
// ===================================================================

#[test]
fn diamond_graph_all_algorithms() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, c.id, "link", 5.0))
        .unwrap();
    db.create_edge(make_weighted_edge(b.id, d.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(c.id, d.id, "link", 1.0))
        .unwrap();

    // BFS from A: finds B, C, D — no duplicates
    let bfs = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([b.id, c.id, d.id]));

    // DFS from A: same set, no duplicates
    let dfs = db.dfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), HashSet::from([b.id, c.id, d.id]));

    // shortest_path A -> D: prefers A->B->D (weight 2.0) over A->C->D (weight 6.0)
    let sp = db.shortest_path(a.id, d.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id, b.id, d.id]);

    // subgraph from A depth 2: all 4 nodes, all 4 edges
    let sg = db.subgraph(a.id, 2).unwrap();
    assert_eq!(sg.nodes.len(), 4);
    assert_eq!(sg.edges.len(), 4);
}

// ===================================================================
// 8. Long chain: N1 -> N2 -> ... -> N20
// ===================================================================

#[test]
fn long_chain_depth_limits() {
    let db = Drevo::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for i in 0..20 {
        let n = db
            .create_node(make_node("chain", &format!("N{}", i)))
            .unwrap();
        ids.push(n.id);
    }
    for i in 0..19 {
        db.create_edge(make_edge(ids[i], ids[i + 1], "next"))
            .unwrap();
    }

    // BFS depth 5 from N0: N1..N5
    let bfs = db.bfs(ids[0], 5, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 5);
    for (i, node) in bfs.iter().enumerate() {
        assert_eq!(node.id, ids[i + 1]);
    }

    // DFS depth 5 from N0: same 5 nodes (linear chain, BFS==DFS)
    let dfs = db.dfs(ids[0], 5, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), node_ids(&bfs));

    // shortest_path N0 -> N19: full chain
    let sp = db.shortest_path(ids[0], ids[19]).unwrap().unwrap();
    assert_eq!(sp.len(), 20);
    assert_eq!(sp[0], ids[0]);
    assert_eq!(sp[19], ids[19]);

    // subgraph depth 3 from N10: N7..N13 (3 hops in each direction)
    let sg = db.subgraph(ids[10], 3).unwrap();
    let expected: HashSet<u64> = (7..=13).map(|i| ids[i]).collect();
    assert_eq!(node_ids(&sg.nodes), expected);
}

// ===================================================================
// 9. Bidirectional edges: A <-> B
// ===================================================================

#[test]
fn bidirectional_edges_consistency() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "link")).unwrap();

    // BFS outgoing from A: finds B
    let bfs_out = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs_out), HashSet::from([b.id]));

    // BFS incoming from A: also finds B (B->A edge, so B is the "incoming neighbor")
    let bfs_in = db.bfs(a.id, 5, Direction::Incoming, None).unwrap();
    assert_eq!(node_ids(&bfs_in), HashSet::from([b.id]));

    // DFS outgoing: same
    let dfs_out = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs_out), HashSet::from([b.id]));

    // shortest_path both directions
    let sp_ab = db.shortest_path(a.id, b.id).unwrap().unwrap();
    assert_eq!(sp_ab, vec![a.id, b.id]);
    let sp_ba = db.shortest_path(b.id, a.id).unwrap().unwrap();
    assert_eq!(sp_ba, vec![b.id, a.id]);

    // subgraph: 2 nodes, 2 edges
    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 2);
    assert_eq!(sg.edges.len(), 2);
}

// ===================================================================
// 10. Direction::Incoming vs Outgoing vs Both
// ===================================================================

#[test]
fn direction_filtering_across_algorithms() {
    // Graph: A -> B -> C
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "link")).unwrap();

    // From B, outgoing: only C
    let bfs_out = db.bfs(b.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs_out), HashSet::from([c.id]));

    // From B, incoming: only A
    let bfs_in = db.bfs(b.id, 5, Direction::Incoming, None).unwrap();
    assert_eq!(node_ids(&bfs_in), HashSet::from([a.id]));

    // From B, both: A and C
    let bfs_both = db.bfs(b.id, 5, Direction::Both, None).unwrap();
    assert_eq!(node_ids(&bfs_both), HashSet::from([a.id, c.id]));

    // DFS matches
    let dfs_out = db.dfs(b.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs_out), HashSet::from([c.id]));
    let dfs_in = db.dfs(b.id, 5, Direction::Incoming, None).unwrap();
    assert_eq!(node_ids(&dfs_in), HashSet::from([a.id]));
    let dfs_both = db.dfs(b.id, 5, Direction::Both, None).unwrap();
    assert_eq!(node_ids(&dfs_both), HashSet::from([a.id, c.id]));

    // neighbors(B, Both) = {A, C}
    let nb = db.neighbors(b.id, Direction::Both, None).unwrap();
    assert_eq!(node_ids(&nb), HashSet::from([a.id, c.id]));
}

// ===================================================================
// 11. Edge kind filtering across BFS and DFS
// ===================================================================

#[test]
fn edge_kind_filter_consistency() {
    // A -[link]-> B -[ref]-> C
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "ref")).unwrap();

    // BFS from A, kind="link": only B (stops at B because B->C is "ref")
    let bfs_link = db.bfs(a.id, 10, Direction::Outgoing, Some("link")).unwrap();
    assert_eq!(node_ids(&bfs_link), HashSet::from([b.id]));

    // DFS from A, kind="link": same
    let dfs_link = db.dfs(a.id, 10, Direction::Outgoing, Some("link")).unwrap();
    assert_eq!(node_ids(&dfs_link), HashSet::from([b.id]));

    // BFS from A, kind="ref": no results (A->B is "link", not "ref")
    let bfs_ref = db.bfs(a.id, 10, Direction::Outgoing, Some("ref")).unwrap();
    assert!(bfs_ref.is_empty());

    // neighbors from A, kind="link": B
    let nb = db
        .neighbors(a.id, Direction::Outgoing, Some("link"))
        .unwrap();
    assert_eq!(node_ids(&nb), HashSet::from([b.id]));

    // neighbors from A, kind="ref": empty
    let nb_ref = db
        .neighbors(a.id, Direction::Outgoing, Some("ref"))
        .unwrap();
    assert!(nb_ref.is_empty());
}

// ===================================================================
// 12. Large fan-out: hub with 50 spokes
// ===================================================================

#[test]
fn large_fan_out_50_spokes() {
    let db = Drevo::open_in_memory().unwrap();
    let hub = db.create_node(make_node("hub", "Hub")).unwrap();
    let mut spoke_ids = Vec::new();
    for i in 0..50 {
        let s = db
            .create_node(make_node("spoke", &format!("S{}", i)))
            .unwrap();
        db.create_edge(make_edge(hub.id, s.id, "connects")).unwrap();
        spoke_ids.push(s.id);
    }

    let expected: HashSet<u64> = spoke_ids.iter().cloned().collect();

    // BFS depth 1: all 50 spokes
    let bfs = db.bfs(hub.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), expected);

    // DFS depth 1: all 50 spokes
    let dfs = db.dfs(hub.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), expected);

    // subgraph depth 1: hub + 50 spokes + 50 edges
    let sg = db.subgraph(hub.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 51);
    assert_eq!(sg.edges.len(), 50);

    // neighbors: all 50
    let nb = db.neighbors(hub.id, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&nb), expected);
}

// ===================================================================
// 13. Complex cycle: A -> B -> C -> D -> B (cycle at B)
// ===================================================================

#[test]
fn complex_cycle_with_tail() {
    // A -> B -> C -> D -> B (B-C-D form a cycle, A is a tail)
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "next")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "next")).unwrap();
    db.create_edge(make_edge(c.id, d.id, "next")).unwrap();
    db.create_edge(make_edge(d.id, b.id, "next")).unwrap();

    // BFS from A: all of B, C, D (cycle doesn't cause infinite loop)
    let bfs = db.bfs(a.id, 20, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([b.id, c.id, d.id]));

    // DFS from A: same set
    let dfs = db.dfs(a.id, 20, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&dfs), HashSet::from([b.id, c.id, d.id]));

    // shortest_path A -> D: A->B->C->D
    let sp = db.shortest_path(a.id, d.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id, b.id, c.id, d.id]);

    // shortest_path D -> A: not reachable (D->B->C->D cycle, never reaches A)
    let sp_da = db.shortest_path(d.id, a.id).unwrap();
    assert!(sp_da.is_none());
}

// ===================================================================
// 14. Node deleted mid-graph — traversal skips it
// ===================================================================

#[test]
fn deleted_node_not_traversed() {
    // A -> B -> C; delete B; BFS/DFS from A should return empty
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, c.id, "link")).unwrap();

    // Delete B (cascading: edges A->B and B->C removed)
    db.delete_node(b.id).unwrap();

    // BFS from A: no edges left, empty
    let bfs = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(bfs.is_empty());

    // DFS from A: empty
    let dfs = db.dfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert!(dfs.is_empty());

    // shortest_path A -> C: no path
    let sp = db.shortest_path(a.id, c.id).unwrap();
    assert!(sp.is_none());

    // subgraph from A: only A, no edges
    let sg = db.subgraph(a.id, 5).unwrap();
    assert_eq!(sg.nodes.len(), 1);
    assert!(sg.edges.is_empty());
}

// ===================================================================
// 15. Multiple self-loops + cycle: A -> A, A -> B -> A
// ===================================================================

#[test]
fn self_loop_plus_cycle() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, a.id, "self")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();
    db.create_edge(make_edge(b.id, a.id, "link")).unwrap();

    // BFS from A: finds B only (A excluded as start node)
    let bfs = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([b.id]));

    // subgraph from A depth 1: A + B, 3 edges (self, a->b, b->a)
    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.nodes.len(), 2);
    assert_eq!(sg.edges.len(), 3);
}

// ===================================================================
// 16. Max depth u8 on small graph — no panic
// ===================================================================

#[test]
fn max_depth_u8_no_panic() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "link")).unwrap();

    // depth = 255 (u8::MAX) on a 2-node graph should just work
    let bfs = db.bfs(a.id, 255, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 1);

    let dfs = db.dfs(a.id, 255, Direction::Outgoing, None).unwrap();
    assert_eq!(dfs.len(), 1);

    let sg = db.subgraph(a.id, 255).unwrap();
    assert_eq!(sg.nodes.len(), 2);
}

// ===================================================================
// 17. BFS and DFS find same node set (on an acyclic graph)
// ===================================================================

#[test]
fn bfs_dfs_same_reachable_set_tree() {
    //       A
    //      / \
    //     B   C
    //    / \   \
    //   D   E   F
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    let e = db.create_node(make_node("note", "E")).unwrap();
    let f = db.create_node(make_node("note", "F")).unwrap();
    db.create_edge(make_edge(a.id, b.id, "child")).unwrap();
    db.create_edge(make_edge(a.id, c.id, "child")).unwrap();
    db.create_edge(make_edge(b.id, d.id, "child")).unwrap();
    db.create_edge(make_edge(b.id, e.id, "child")).unwrap();
    db.create_edge(make_edge(c.id, f.id, "child")).unwrap();

    let bfs = db.bfs(a.id, 10, Direction::Outgoing, None).unwrap();
    let dfs = db.dfs(a.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), node_ids(&dfs));
    assert_eq!(
        node_ids(&bfs),
        HashSet::from([b.id, c.id, d.id, e.id, f.id])
    );
}

// ===================================================================
// 18. Weighted shortest path prefers lighter edges
// ===================================================================

#[test]
fn shortest_path_weight_preference_complex() {
    // A -> B (weight 10), A -> C (weight 1), C -> D (weight 1), D -> B (weight 1)
    // Shortest A->B should be A->C->D->B (total 3) not A->B direct (weight 10)
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let c = db.create_node(make_node("note", "C")).unwrap();
    let d = db.create_node(make_node("note", "D")).unwrap();
    db.create_edge(make_weighted_edge(a.id, b.id, "link", 10.0))
        .unwrap();
    db.create_edge(make_weighted_edge(a.id, c.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(c.id, d.id, "link", 1.0))
        .unwrap();
    db.create_edge(make_weighted_edge(d.id, b.id, "link", 1.0))
        .unwrap();

    let sp = db.shortest_path(a.id, b.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id, c.id, d.id, b.id]);
}

// ===================================================================
// 19. Multiple edges between same pair
// ===================================================================

#[test]
fn parallel_edges_between_nodes() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(make_node("note", "A")).unwrap();
    let b = db.create_node(make_node("note", "B")).unwrap();
    let e1 = db
        .create_edge(make_weighted_edge(a.id, b.id, "link", 5.0))
        .unwrap();
    let e2 = db
        .create_edge(make_weighted_edge(a.id, b.id, "ref", 1.0))
        .unwrap();

    // BFS: finds B once (not duplicated)
    let bfs = db.bfs(a.id, 5, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 1);
    assert_eq!(bfs[0].id, b.id);

    // shortest_path: uses the lighter edge (weight 1.0)
    let sp = db.shortest_path(a.id, b.id).unwrap().unwrap();
    assert_eq!(sp, vec![a.id, b.id]);

    // subgraph: both parallel edges included
    let sg = db.subgraph(a.id, 1).unwrap();
    assert_eq!(sg.edges.len(), 2);
    let edge_ids: HashSet<u64> = sg.edges.iter().map(|e| e.id).collect();
    assert!(edge_ids.contains(&e1.id));
    assert!(edge_ids.contains(&e2.id));
}

// ===================================================================
// 20. Use-case scenario: CBT journal — thought chain with cycle
// ===================================================================

#[test]
fn scenario_cbt_journal_thought_cycle() {
    let db = Drevo::open_in_memory().unwrap();

    let situation = db
        .create_node(make_node("situation", "Work presentation"))
        .unwrap();
    let thought = db.create_node(make_node("thought", "I will fail")).unwrap();
    let emotion = db.create_node(make_node("emotion", "Anxiety")).unwrap();
    let distortion = db
        .create_node(make_node("distortion", "Catastrophizing"))
        .unwrap();
    let response = db
        .create_node(make_node("response", "I have prepared well"))
        .unwrap();

    // Cycle: situation -> thought -> emotion -> situation (rumination loop)
    db.create_edge(make_edge(situation.id, thought.id, "triggered"))
        .unwrap();
    db.create_edge(make_edge(thought.id, emotion.id, "leads_to"))
        .unwrap();
    db.create_edge(make_edge(emotion.id, situation.id, "reinforces"))
        .unwrap();
    // Breaking the cycle: thought -> distortion -> response
    db.create_edge(make_edge(thought.id, distortion.id, "identified_as"))
        .unwrap();
    db.create_edge(make_edge(distortion.id, response.id, "reframed_as"))
        .unwrap();

    // BFS from situation: all 4 other nodes
    let bfs = db.bfs(situation.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 4);

    // Subgraph from thought depth 2: thought + emotion + situation + distortion + response
    let sg = db.subgraph(thought.id, 2).unwrap();
    assert_eq!(sg.nodes.len(), 5);

    // shortest_path from situation to response
    let sp = db
        .shortest_path(situation.id, response.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        sp,
        vec![situation.id, thought.id, distortion.id, response.id]
    );
}

// ===================================================================
// 21. Use-case scenario: story editor — tree with disconnected notes
// ===================================================================

#[test]
fn scenario_story_editor_disconnected_notes() {
    let db = Drevo::open_in_memory().unwrap();

    let book = db.create_node(make_node("book", "My Novel")).unwrap();
    let ch1 = db.create_node(make_node("chapter", "Chapter 1")).unwrap();
    let ch2 = db.create_node(make_node("chapter", "Chapter 2")).unwrap();
    let scene1 = db.create_node(make_node("scene", "Opening scene")).unwrap();
    // Disconnected note (draft idea, not linked yet)
    let draft = db.create_node(make_node("note", "Character idea")).unwrap();

    db.create_edge(make_edge(book.id, ch1.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(book.id, ch2.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(ch1.id, scene1.id, "contains"))
        .unwrap();

    // subgraph from book depth 10: does NOT include draft
    let sg = db.subgraph(book.id, 10).unwrap();
    let sg_ids = node_ids(&sg.nodes);
    assert!(sg_ids.contains(&book.id));
    assert!(sg_ids.contains(&ch1.id));
    assert!(sg_ids.contains(&ch2.id));
    assert!(sg_ids.contains(&scene1.id));
    assert!(!sg_ids.contains(&draft.id));

    // BFS from book: 3 nodes (ch1, ch2, scene1), not draft
    let bfs = db.bfs(book.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 3);
    assert!(!node_ids(&bfs).contains(&draft.id));
}

// ===================================================================
// 22. Use-case scenario: task manager — blocking chain depth limit
// ===================================================================

#[test]
fn scenario_task_manager_blocking_chain() {
    let db = Drevo::open_in_memory().unwrap();

    let t1 = db.create_node(make_node("task", "Deploy API")).unwrap();
    let t2 = db.create_node(make_node("task", "Run migrations")).unwrap();
    let t3 = db
        .create_node(make_node("task", "Write migration SQL"))
        .unwrap();
    let t4 = db.create_node(make_node("task", "Review schema")).unwrap();

    // t1 blocked by t2, t2 blocked by t3, t3 blocked by t4
    db.create_edge(make_edge(t1.id, t2.id, "blocks")).unwrap();
    db.create_edge(make_edge(t2.id, t3.id, "blocks")).unwrap();
    db.create_edge(make_edge(t3.id, t4.id, "blocks")).unwrap();

    // BFS depth 1 from t1: only direct blocker t2
    let bfs_d1 = db.bfs(t1.id, 1, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs_d1.len(), 1);
    assert_eq!(bfs_d1[0].id, t2.id);

    // BFS depth 2: t2, t3
    let bfs_d2 = db.bfs(t1.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs_d2.len(), 2);

    // Full chain via shortest_path
    let sp = db.shortest_path(t1.id, t4.id).unwrap().unwrap();
    assert_eq!(sp, vec![t1.id, t2.id, t3.id, t4.id]);
}

// ===================================================================
// 23. Use-case scenario: ERP — order graph with self-referencing warehouse
// ===================================================================

#[test]
fn scenario_erp_warehouse_self_loop() {
    let db = Drevo::open_in_memory().unwrap();

    let order = db.create_node(make_node("order", "ORD-001")).unwrap();
    let product = db.create_node(make_node("product", "Widget")).unwrap();
    let warehouse = db.create_node(make_node("warehouse", "Main WH")).unwrap();

    db.create_edge(make_edge(order.id, product.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(product.id, warehouse.id, "stored_in"))
        .unwrap();
    // Self-loop: warehouse audits itself
    db.create_edge(make_edge(warehouse.id, warehouse.id, "audits"))
        .unwrap();

    // BFS from order: product, warehouse
    let bfs = db.bfs(order.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(node_ids(&bfs), HashSet::from([product.id, warehouse.id]));

    // subgraph includes self-loop edge
    let sg = db.subgraph(order.id, 10).unwrap();
    assert_eq!(sg.edges.len(), 3); // contains + stored_in + audits
}

// ===================================================================
// 24. Use-case scenario: bug tracker — impact analysis with diamond
// ===================================================================

#[test]
fn scenario_bug_tracker_impact_diamond() {
    let db = Drevo::open_in_memory().unwrap();

    let bug = db.create_node(make_node("bug", "Login crash")).unwrap();
    let comp_auth = db
        .create_node(make_node("component", "Auth module"))
        .unwrap();
    let comp_session = db
        .create_node(make_node("component", "Session manager"))
        .unwrap();
    let release = db.create_node(make_node("release", "v2.0")).unwrap();

    // Diamond: bug affects auth and session; both block release
    db.create_edge(make_edge(bug.id, comp_auth.id, "affects"))
        .unwrap();
    db.create_edge(make_edge(bug.id, comp_session.id, "affects"))
        .unwrap();
    db.create_edge(make_edge(comp_auth.id, release.id, "blocks"))
        .unwrap();
    db.create_edge(make_edge(comp_session.id, release.id, "blocks"))
        .unwrap();

    // BFS from bug: 3 unique nodes (no duplicate release)
    let bfs = db.bfs(bug.id, 10, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 3);
    assert_eq!(
        node_ids(&bfs),
        HashSet::from([comp_auth.id, comp_session.id, release.id])
    );

    // subgraph: 4 nodes, 4 edges
    let sg = db.subgraph(bug.id, 2).unwrap();
    assert_eq!(sg.nodes.len(), 4);
    assert_eq!(sg.edges.len(), 4);

    // shortest_path bug -> release: 2 hops (either via auth or session)
    let sp = db.shortest_path(bug.id, release.id).unwrap().unwrap();
    assert_eq!(sp.len(), 3); // bug -> X -> release
}
