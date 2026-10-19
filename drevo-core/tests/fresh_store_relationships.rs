//! Relationships created in a fresh store, between nodes far apart in the id
//! space, are all visible: from both endpoints, by id, in the edge list, and
//! after the store is reopened.
//!
//! The scenario is a road importer: tens of thousands of junctions first,
//! then the roads between them, in one batch.

use drevo_core::engine::GraphEngine;
use drevo_core::model::{Direction, NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

const JUNCTIONS: u64 = 60_000;
const ROADS: usize = 2_000;

fn junction(i: u64) -> NewNode {
    NewNode {
        kind: "Junction".to_string(),
        title: format!("j{i}"),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::new(),
    }
}

/// Deterministic random pairs of node ids spread over the whole store.
fn roads(ids: &[u64]) -> Vec<NewEdge> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut pick = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ids[(state % ids.len() as u64) as usize]
    };
    (0..ROADS)
        .map(|_| NewEdge {
            from_id: pick(),
            to_id: pick(),
            kind: "ROAD".to_string(),
            weight: 5.3,
            properties: Properties::new(),
        })
        .collect()
}

fn seed(graph: &NativeGraph) -> Vec<u64> {
    let mut ids = Vec::new();
    for start in (0..JUNCTIONS).step_by(2_000) {
        let batch = (start..(start + 2_000).min(JUNCTIONS))
            .map(junction)
            .collect();
        ids.extend(graph.create_nodes(batch).unwrap().into_iter().map(|n| n.id));
    }
    ids
}

#[track_caller]
fn assert_all_visible(graph: &NativeGraph, created: &[drevo_core::model::Edge]) {
    assert_eq!(graph.edge_count(), created.len() as u64);
    let mut missing = Vec::new();
    for edge in created {
        let out = graph.edges_of(edge.from_id, Direction::Outgoing).unwrap();
        let inc = graph.edges_of(edge.to_id, Direction::Incoming).unwrap();
        if !out.iter().any(|e| e.id == edge.id)
            || !inc.iter().any(|e| e.id == edge.id)
            || graph.get_edge(edge.id).unwrap().is_none()
        {
            missing.push(edge.id);
        }
    }
    assert!(
        missing.is_empty(),
        "{} of {} relationships are not visible: {:?}",
        missing.len(),
        created.len(),
        &missing[..missing.len().min(12)]
    );
}

#[test]
fn far_apart_relationships_in_a_fresh_store_are_all_visible() {
    let graph = NativeGraph::new();
    let ids = seed(&graph);
    let created = graph.create_edges(roads(&ids)).unwrap();
    assert_all_visible(&graph, &created);
}

#[test]
fn they_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("drevo_fresh_roads_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    let created = {
        let graph = NativeGraph::open_durable(&wal).unwrap();
        let ids = seed(&graph);
        graph.create_edges(roads(&ids)).unwrap()
    };
    let graph = NativeGraph::open_durable(&wal).unwrap();
    assert_all_visible(&graph, &created);
    drop(graph);
    let _ = std::fs::remove_dir_all(&dir);
}
