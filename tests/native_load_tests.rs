//! Native load harness — authoritative tested pieces (#307).
//!
//! `examples/native_load.rs` is an on-demand measurement (concurrency sweep +
//! deep traversal + durable-write throughput, native-durable vs KV). This file
//! keeps the pieces it relies on honest on the normal PR gate, at small scale
//! and mirroring the harness's helpers (as `tests/http_load_tests.rs` does for
//! the HTTP client):
//!
//! * the deep-traversal BFS returns the right reachable set,
//! * a concurrent read workload over the shared engine runs error-free,
//! * a concurrent durable-write workload actually persists — the WAL survives a
//!   reopen with every edge recovered (the property the write throughput number
//!   is only meaningful if it holds).
//!
//! Gated on `redb-backend` for the KV comparison side.

#![cfg(feature = "redb-backend")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::native::NativeGraph;

// --- mirrors examples/native_load.rs ----------------------------------------

fn bfs_reach<E: GraphEngine>(engine: &E, start: u64, depth: u32) -> usize {
    let mut seen = HashSet::from([start]);
    let mut frontier = vec![start];
    for _ in 0..depth {
        let mut next = Vec::new();
        for &node in &frontier {
            if let Ok(neighbours) = engine.neighbor_ids(node, Direction::Outgoing, None) {
                for nb in neighbours {
                    if seen.insert(nb) {
                        next.push(nb);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    seen.len()
}

fn node(engine: &impl GraphEngine, title: &str) -> u64 {
    engine
        .create_node(NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .expect("create node")
        .id
}

fn edge(engine: &impl GraphEngine, from: u64, to: u64) {
    engine
        .create_edge(NewEdge {
            from_id: from,
            to_id: to,
            kind: "e".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .expect("create edge");
}

/// A path 0->1->2->3->4 so BFS depth is exactly countable from node 0.
fn path_graph(engine: &impl GraphEngine) -> Vec<u64> {
    let ids: Vec<u64> = (0..5).map(|i| node(engine, &format!("n{i}"))).collect();
    for w in ids.windows(2) {
        edge(engine, w[0], w[1]);
    }
    ids
}

#[test]
fn bfs_reach_counts_the_reachable_set_by_depth() {
    let g = Drevo::open_in_memory().expect("open");
    let ids = path_graph(&g);
    // From the head: depth 1 reaches {0,1}, depth 3 reaches {0,1,2,3}, depth 9
    // saturates at the whole path {0..4} = 5.
    assert_eq!(bfs_reach(&g, ids[0], 1), 2);
    assert_eq!(bfs_reach(&g, ids[0], 3), 4);
    assert_eq!(bfs_reach(&g, ids[0], 9), 5);
    // A tail node reaches only itself outgoing.
    assert_eq!(bfs_reach(&g, ids[4], 3), 1);
}

#[test]
fn kv_and_native_agree_on_bfs_reach() {
    let kv = Drevo::open_in_memory().expect("kv");
    let ids = path_graph(&kv);
    let native = NativeGraph::new();
    drevo::migrate::migrate(&kv, &native).expect("migrate");
    for depth in 0..6u32 {
        assert_eq!(
            bfs_reach(&kv, ids[0], depth),
            bfs_reach(&native, ids[0], depth),
            "engines disagree on BFS reach at depth {depth}"
        );
    }
}

#[test]
fn concurrent_reads_over_shared_engine_are_error_free() {
    let kv = Drevo::open_in_memory().expect("kv");
    let ids = path_graph(&kv);
    let native = NativeGraph::new();
    drevo::migrate::migrate(&kv, &native).expect("migrate");

    let errors = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for t in 0..4 {
            let errors = &errors;
            let ids = &ids;
            let native = &native;
            scope.spawn(move || {
                for i in 0..500u64 {
                    let id = ids[((t as u64 * 7 + i) % ids.len() as u64) as usize];
                    if native.get_node(id).is_err()
                        || native.neighbor_ids(id, Direction::Outgoing, None).is_err()
                        || bfs_reach(native, id, 3) == 0
                    {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    assert_eq!(
        errors.load(Ordering::Relaxed),
        0,
        "concurrent reads errored"
    );
}

#[test]
fn concurrent_durable_writes_persist_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wal = dir.path().join("load.wal");

    let base_ids;
    {
        let native = NativeGraph::open_durable(&wal).expect("open durable");
        base_ids = path_graph(&native); // 5 nodes, 4 edges
        let base_edges = GraphEngine::all_edges(&native).expect("edges").len();
        assert_eq!(base_edges, 4);

        // Four threads each append 25 edges between existing nodes, concurrently,
        // through the same durable store (every write fsyncs).
        let n = base_ids.len() as u64;
        std::thread::scope(|scope| {
            for t in 0..4u64 {
                let native = &native;
                let base_ids = &base_ids;
                scope.spawn(move || {
                    for i in 0..25u64 {
                        let from = base_ids[((t * 13 + i) % n) as usize];
                        let to = base_ids[((t * 7 + i + 1) % n) as usize];
                        edge(native, from, to);
                    }
                });
            }
        });
        let after = GraphEngine::all_edges(&native).expect("edges").len();
        assert_eq!(after, 4 + 4 * 25, "concurrent writes lost edges in memory");
    }

    // Reopen the WAL cold: every acknowledged write must recover.
    let reopened = NativeGraph::open_durable(&wal).expect("reopen durable");
    assert_eq!(
        GraphEngine::all_edges(&reopened).expect("edges").len(),
        4 + 4 * 25,
        "durable WAL did not recover all concurrently-written edges"
    );
    assert_eq!(GraphEngine::all_nodes(&reopened).expect("nodes").len(), 5);
}

#[test]
fn tx_batched_writes_commit_atomically_and_persist() {
    // The write-fsync mitigation the harness measures: many edges in one
    // transaction must land as one durable batch and survive a reopen.
    let dir = tempfile::tempdir().expect("tempdir");
    let wal = dir.path().join("txbatch.wal");
    let ids;
    {
        let native = NativeGraph::open_durable(&wal).expect("open durable");
        ids = path_graph(&native); // 5 nodes, 4 edges
        let n = ids.len() as u64;

        let tx = native.tx_begin();
        {
            let txe = native.tx_engine(tx).expect("tx engine");
            for i in 0..50u64 {
                txe.create_edge(NewEdge {
                    from_id: ids[(i % n) as usize],
                    to_id: ids[((i + 1) % n) as usize],
                    kind: "e".into(),
                    weight: 1.0,
                    properties: Properties::default(),
                })
                .expect("tx edge");
            }
            // Buffered, not yet visible on the base graph.
            assert_eq!(GraphEngine::all_edges(&native).expect("edges").len(), 4);
        }
        native.tx_commit(tx).expect("commit");
        assert_eq!(
            GraphEngine::all_edges(&native).expect("edges").len(),
            4 + 50,
            "committed transaction not visible on the base graph"
        );
    }
    let reopened = NativeGraph::open_durable(&wal).expect("reopen durable");
    assert_eq!(
        GraphEngine::all_edges(&reopened).expect("edges").len(),
        4 + 50,
        "tx-batched writes did not recover from the WAL"
    );
}

#[test]
fn sequential_durable_writes_fsync_once_each() {
    // Group commit must not weaken durability: an uncontended write is its own
    // group and still fsyncs, so N sequential writes cost exactly N fsyncs.
    let dir = tempfile::tempdir().expect("tempdir");
    let native = NativeGraph::open_durable(dir.path().join("seq.wal")).expect("open durable");
    let ids = path_graph(&native);
    let base = native.wal_fsync_count();
    for i in 0..20u64 {
        edge(&native, ids[(i % 5) as usize], ids[((i + 1) % 5) as usize]);
    }
    assert_eq!(
        native.wal_fsync_count() - base,
        20,
        "each uncontended durable write must fsync"
    );
}

#[test]
fn transaction_commit_is_a_single_fsync() {
    let dir = tempfile::tempdir().expect("tempdir");
    let native = NativeGraph::open_durable(dir.path().join("tx.wal")).expect("open durable");
    let ids = path_graph(&native);
    let base = native.wal_fsync_count();
    let tx = native.tx_begin();
    {
        let txe = native.tx_engine(tx).expect("tx engine");
        for i in 0..50u64 {
            txe.create_edge(NewEdge {
                from_id: ids[(i % 5) as usize],
                to_id: ids[((i + 1) % 5) as usize],
                kind: "e".into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .expect("tx edge");
        }
    }
    native.tx_commit(tx).expect("commit");
    assert_eq!(
        native.wal_fsync_count() - base,
        1,
        "a whole transaction must cost exactly one fsync"
    );
}

#[test]
fn group_commit_coalesces_concurrent_fsyncs() {
    // The point of group commit: N concurrent durable writes share fsyncs, so
    // the fsync count is far below the write count — while every write still
    // persists and recovers. Without group commit this is one fsync per write
    // (delta == ops), so the `< ops` assertion is the regression guard.
    let dir = tempfile::tempdir().expect("tempdir");
    let wal = dir.path().join("group.wal");
    const THREADS: u64 = 8;
    const PER: u64 = 100;
    let ops = THREADS * PER;

    let base;
    {
        let native = NativeGraph::open_durable(&wal).expect("open durable");
        let ids = path_graph(&native);
        base = native.wal_fsync_count();
        let n = ids.len() as u64;
        let barrier = Barrier::new(THREADS as usize);
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let native = &native;
                let ids = &ids;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for i in 0..PER {
                        edge(
                            native,
                            ids[((t * 31 + i) % n) as usize],
                            ids[((t * 17 + i + 1) % n) as usize],
                        );
                    }
                });
            }
        });
        let fsyncs = native.wal_fsync_count() - base;
        assert!(
            fsyncs < ops,
            "expected group commit to coalesce fsyncs below {ops} writes, got {fsyncs}"
        );
        assert!(fsyncs >= 1, "some fsync must have happened");
        assert_eq!(
            GraphEngine::all_edges(&native).expect("edges").len() as u64,
            4 + ops,
            "concurrent group-committed writes lost edges in memory"
        );
    }

    let reopened = NativeGraph::open_durable(&wal).expect("reopen durable");
    assert_eq!(
        GraphEngine::all_edges(&reopened).expect("edges").len() as u64,
        4 + ops,
        "group-committed writes did not all recover from the WAL"
    );
}
