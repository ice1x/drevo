//! A write transaction gets its own copy of the store, and that copy costs
//! what the transaction changes, not the size of the graph: records,
//! adjacency lists, embeddings and the kind / title indexes are shared with
//! the committed store until written.
//!
//! The scenario is a road graph — junctions and the roads between them —
//! edited one junction at a time, measured with a counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

fn bump() {
    PEAK.fetch_max(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; the counters are
// only bookkeeping.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
            bump();
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded with the caller's pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded with the caller's arguments.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
            bump();
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn road_graph(junctions: u64, roads: u64) -> NativeGraph {
    let g = NativeGraph::new();
    for start in (0..junctions).step_by(5_000) {
        let batch = (start..(start + 5_000).min(junctions))
            .map(|i| NewNode {
                kind: "Junction".to_string(),
                title: String::new(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::from([("i", serde_json::json!(i))]),
            })
            .collect();
        g.create_nodes(batch).unwrap();
    }
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut pick = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        1 + state % junctions
    };
    for _ in (0..roads).step_by(5_000) {
        let batch = (0..5_000)
            .map(|_| NewEdge {
                from_id: pick(),
                to_id: pick(),
                kind: "ROAD".to_string(),
                weight: 1.0,
                properties: Properties::new(),
            })
            .collect();
        g.create_edges(batch).unwrap();
    }
    for id in 1..=500 {
        g.set_embedding(id, vec![0.5; 1536]).unwrap();
    }
    g
}

/// The peak heap, in MB, while one junction is changed in a transaction.
fn one_change_in_a_transaction(g: &NativeGraph, id: u64) -> f64 {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let before = LIVE.load(Ordering::Relaxed);
    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(
            id,
            NodePatch {
                properties: Some(Properties::from([("closed", serde_json::json!(true))])),
                ..NodePatch::default()
            },
        )
        .unwrap();
    g.tx_commit(tx).unwrap();
    (PEAK.load(Ordering::Relaxed) - before) as f64 / 1e6
}

#[test]
fn a_small_write_transaction_costs_what_it_changes() {
    let g = road_graph(50_000, 100_000);
    let heap = LIVE.load(Ordering::Relaxed) as f64 / 1e6;
    one_change_in_a_transaction(&g, 1);
    let peak = (2..12)
        .map(|id| one_change_in_a_transaction(&g, id * 997))
        .fold(0.0, f64::max);
    // Copying the adjacency lists, embeddings and indexes took 13.5 MB here.
    assert!(peak < 1.0, "peak {peak:.2} MB over a {heap:.0} MB store");
    // The change is there, and the committed store is otherwise unchanged.
    let changed = g.get_node(997 * 2).unwrap().unwrap();
    assert_eq!(
        changed.properties.get("closed"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(g.edge_count(), 100_000);
    assert_eq!(g.get_embedding(7).map(|v| v.len()), Some(1536));
}
