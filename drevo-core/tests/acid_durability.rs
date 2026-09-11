//! ACID conformance — **D (Durability)** for the default `native-durable`
//! engine, exercised as **Rust** tests directly against the `drevo-core`
//! [`NativeGraph`] seam (no HTTP layer).
//!
//! Durability means an **acknowledged** write survives process death: the
//! write-ahead log is fsync'd before a write/commit returns, so a hard exit
//! after the acknowledgement still recovers the data on reopen. Replay of the
//! log is deterministic and idempotent, and id allocation stays monotonic
//! across restarts.
//!
//! Part of the ACID conformance set (issue #427).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::{NativeGraph, WalOp};

// std-only temp dir (drevo-core is dependency-light; no `tempfile` dev-dep).
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_acid_d_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("native.wal")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

/// An acknowledged write is fsync'd before the call returns and survives a
/// reopen with **no** explicit close (simulating a crash after the ack).
#[test]
fn acknowledged_writes_are_fsynced_and_survive_reopen() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        g.create_node(node("ada")).unwrap();
        g.create_node(node("bob")).unwrap();
        assert!(
            g.wal_fsync_count() >= 2,
            "each acknowledged write must fsync the log before returning"
        );
        // No close/flush call — just drop, as a crash would.
    }
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(
        g.node_count(),
        2,
        "acknowledged writes are recovered from the WAL after a crash-like reopen"
    );
}

/// Reopening replays the log to exactly the pre-crash state (nodes + edges).
#[test]
fn reopen_replays_the_log_to_the_same_state() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    let (n_before, e_before) = {
        let g = NativeGraph::open_durable(&path).unwrap();
        let a = g.create_node(node("a")).unwrap();
        let b = g.create_node(node("b")).unwrap();
        g.create_edge(drevo_core::model::NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "links".into(),
            weight: 1.0,
            properties: Properties(Default::default()),
        })
        .unwrap();
        (g.node_count(), g.edge_count())
    };
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!((g.node_count(), g.edge_count()), (n_before, e_before));
    assert_eq!((n_before, e_before), (2, 1));
}

/// Replaying the same WAL op sequence more than once yields the same state:
/// upserts carry the applied record, so replay is idempotent (at-least-once
/// recovery is safe).
#[test]
fn wal_replay_is_idempotent() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    let ops: Vec<WalOp> = {
        let g = NativeGraph::open_durable(&path).unwrap();
        g.create_node(node("a")).unwrap();
        g.create_node(node("b")).unwrap();
        g.dump_wal()
    };

    let once = NativeGraph::replay(ops.clone());
    let doubled: Vec<WalOp> = ops.iter().cloned().chain(ops.iter().cloned()).collect();
    let twice = NativeGraph::replay(doubled);

    assert_eq!(
        (once.node_count(), once.edge_count()),
        (twice.node_count(), twice.edge_count()),
        "replaying the ops twice must not double the graph — replay is idempotent"
    );
    assert_eq!(once.node_count(), 2);
}

/// Id allocation is monotonic across reopens: recovered state never re-hands an
/// id, even after deletes.
#[test]
fn ids_stay_monotonic_across_reopens() {
    let tmp = TmpDir::new();
    let path = tmp.wal();

    let id1 = {
        let g = NativeGraph::open_durable(&path).unwrap();
        g.create_node(node("a")).unwrap().id
    };
    let id2 = {
        let g = NativeGraph::open_durable(&path).unwrap();
        g.create_node(node("b")).unwrap().id
    };
    assert!(id2 > id1, "id after reopen must exceed the prior id");

    let id3 = {
        let g = NativeGraph::open_durable(&path).unwrap();
        // Delete everything, then allocate again: the counter must not rewind.
        g.delete_node(id1).ok();
        g.delete_node(id2).ok();
        g.create_node(node("c")).unwrap().id
    };
    assert!(
        id3 > id2,
        "id allocation stays monotonic across reopen even after deletes"
    );
}
