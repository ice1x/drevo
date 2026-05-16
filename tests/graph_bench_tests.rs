//! Integration tests for graph benchmark scenario.
//!
//! Validates the benchmark pattern at small scale: insert nodes,
//! create edges between them, then read all neighbors for each node.

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};

fn make_node(i: usize) -> NewNode {
    NewNode {
        kind: format!("kind_{}", i % 10),
        title: format!("node_{i:06}"),
        body: format!("Body of node {i}"),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_edge(from_id: u64, to_id: u64, i: usize) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: format!("rel_{}", i % 5),
        weight: 1.0,
        properties: Properties::default(),
    }
}

#[test]
fn small_scale_insert_nodes_and_edges() {
    let db = Drevo::open_in_memory().unwrap();

    let num_nodes = 100;
    let mut node_ids = Vec::with_capacity(num_nodes);

    // Insert nodes
    for i in 0..num_nodes {
        let node = db.create_node(make_node(i)).unwrap();
        node_ids.push(node.id);
    }
    assert_eq!(node_ids.len(), num_nodes);

    // Insert edges: 5 edges per node (500 total)
    let num_edges_per_node = 5;
    let mut edge_count = 0;
    for (i, &from_id) in node_ids.iter().enumerate() {
        for j in 0..num_edges_per_node {
            let to_idx = (i + j + 1) % num_nodes;
            let to_id = node_ids[to_idx];
            db.create_edge(make_edge(from_id, to_id, edge_count))
                .unwrap();
            edge_count += 1;
        }
    }
    assert_eq!(edge_count, num_nodes * num_edges_per_node);

    // Verify: each node should have outgoing edges
    for &node_id in &node_ids {
        let edges = db.edges_of(node_id, Direction::Outgoing).unwrap();
        assert_eq!(edges.len(), num_edges_per_node);
    }
}

#[test]
fn small_scale_read_neighbors_both_directions() {
    let db = Drevo::open_in_memory().unwrap();

    // Create 20 nodes
    let mut node_ids = Vec::new();
    for i in 0..20 {
        let node = db.create_node(make_node(i)).unwrap();
        node_ids.push(node.id);
    }

    // Create edges: node_0 -> node_1..5
    for j in 1..=5 {
        db.create_edge(make_edge(node_ids[0], node_ids[j], j))
            .unwrap();
    }
    // Create edges: node_6..10 -> node_0
    for j in 6..=10 {
        db.create_edge(make_edge(node_ids[j], node_ids[0], j))
            .unwrap();
    }

    let outgoing = db.edges_of(node_ids[0], Direction::Outgoing).unwrap();
    assert_eq!(outgoing.len(), 5);

    let incoming = db.edges_of(node_ids[0], Direction::Incoming).unwrap();
    assert_eq!(incoming.len(), 5);

    let both = db.edges_of(node_ids[0], Direction::Both).unwrap();
    assert_eq!(both.len(), 10);
}

#[test]
fn small_scale_kind_index_after_bulk_insert() {
    let db = Drevo::open_in_memory().unwrap();

    let num_nodes = 100;
    let mut node_ids = Vec::new();
    for i in 0..num_nodes {
        let node = db.create_node(make_node(i)).unwrap();
        node_ids.push(node.id);
    }

    // 10 kinds, 10 nodes each
    for kind_idx in 0..10 {
        let kind = format!("kind_{kind_idx}");
        let nodes = db.list_nodes_by_kind(&kind, 100, 0).unwrap();
        assert_eq!(nodes.len(), 10, "kind_{kind_idx} should have 10 nodes");
    }
}

#[test]
fn small_scale_edge_kind_index_after_bulk_insert() {
    let db = Drevo::open_in_memory().unwrap();

    let num_nodes = 50;
    let mut node_ids = Vec::new();
    for i in 0..num_nodes {
        let node = db.create_node(make_node(i)).unwrap();
        node_ids.push(node.id);
    }

    // 2 edges per node = 100 edges, 5 edge kinds
    let mut edge_count = 0;
    for (i, &from_id) in node_ids.iter().enumerate() {
        for j in 0..2 {
            let to_idx = (i + j + 1) % num_nodes;
            db.create_edge(make_edge(from_id, node_ids[to_idx], edge_count))
                .unwrap();
            edge_count += 1;
        }
    }

    // 5 edge kinds, each should have 100/5 = 20 edges
    for kind_idx in 0..5 {
        let kind = format!("rel_{kind_idx}");
        let edges = db.list_edges_by_kind(&kind, 100, 0).unwrap();
        assert_eq!(edges.len(), 20, "{kind} should have 20 edges");
    }
}
