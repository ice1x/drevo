//! Live stamps survive reopening a durable replica (issue #612).
//!
//! A replica's version vector and its `delta_since` upserts are built from the
//! per-entity causal stamps. They are kept next to the WAL, so after a reopen:
//!
//! - a fresh peer still bootstraps the whole graph from the replica;
//! - a local edit keeps its original stamp, so it still beats a peer's older
//!   copy, and a peer's newer edit still beats it.
//!
//! An entity whose stamp was never persisted (a store written before stamps
//! were kept, or a crash before the stamp reached disk) is loaded with the
//! lowest stamp: it still seeds a fresh peer but never beats a real edit.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::delta::VersionVector;
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{NativeGraph, StampTarget};

// std-only temp dir (drevo-core is dependency-light; no `tempfile` dev-dep).
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_stamps_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("native.wal")
    }
    fn stamp_log(&self) -> PathBuf {
        self.0.join("stamps.jsonl")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "links".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn retitle(g: &NativeGraph, id: u64, title: &str) {
    g.update_node(
        id,
        NodePatch {
            title: Some(title.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn title_by_uuid(g: &NativeGraph, uuid: [u8; 16]) -> String {
    let id = g.node_id_of_uuid(uuid).expect("node present");
    g.get_node(id).unwrap().unwrap().title.clone()
}

#[test]
fn stamps_and_version_vector_survive_reopen() {
    let dir = TmpDir::new();
    let (a, b, ab, vv) = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let a = g.create_node(node("a")).unwrap();
        let b = g.create_node(node("b")).unwrap();
        retitle(&g, a.id, "a2");
        let ab = g.create_edge(edge(a.id, b.id)).unwrap();
        (a, b, ab, g.version_vector())
    };
    let stamps_before = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let s = [
            g.stamp_of(StampTarget::Node(a.id)),
            g.stamp_of(StampTarget::Node(b.id)),
            g.stamp_of(StampTarget::Edge(ab.id)),
        ];
        assert!(
            s.iter().all(Option::is_some),
            "every live entity keeps a stamp"
        );
        assert_eq!(
            g.version_vector(),
            vv,
            "the version vector is the one before reopen"
        );
        s
    };
    // A second reopen (after the log was rewritten on open) changes nothing.
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(g.stamp_of(StampTarget::Node(a.id)), stamps_before[0]);
    assert_eq!(g.stamp_of(StampTarget::Edge(ab.id)), stamps_before[2]);
    assert_eq!(g.version_vector(), vv);
}

/// The issue's acceptance case: write nodes and edges, reopen, and a fresh
/// replica reconstructs all of them from `delta_since(empty)`.
#[test]
fn fresh_peer_bootstraps_from_a_reopened_replica() {
    let dir = TmpDir::new();
    let mut uuids = Vec::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let seed = g.create_node(node("Seed")).unwrap();
        uuids.push(seed.uuid);
        for i in 0..5 {
            let n = g.create_node(node(&format!("n{i}"))).unwrap();
            g.create_edge(edge(seed.id, n.id)).unwrap();
            uuids.push(n.uuid);
        }
    }
    let alice = NativeGraph::open_durable(dir.wal()).unwrap();
    assert!(!alice.version_vector().is_empty());

    let fresh = NativeGraph::new();
    fresh
        .apply_delta(&alice.delta_since(&VersionVector::new()))
        .unwrap();
    assert_eq!(fresh.node_count(), 6);
    assert_eq!(fresh.edge_count(), 5);
    for uuid in uuids {
        assert!(fresh.node_id_of_uuid(uuid).is_some());
    }
    assert_eq!(
        title_by_uuid(&fresh, alice.get_node(1).unwrap().unwrap().uuid),
        "Seed"
    );
}

/// A local edit made before the restart keeps its stamp, so a peer's older
/// copy of the same node cannot overwrite it after the restart.
#[test]
fn local_edit_still_beats_a_stale_peer_after_reopen() {
    let dir = TmpDir::new();
    let peer = NativeGraph::new();
    let uuid = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let n = g.create_node(node("v1")).unwrap();
        peer.apply_delta(&g.delta_since(&peer.version_vector()))
            .unwrap();
        retitle(&g, n.id, "v2"); // newer than the peer's copy
        n.uuid
    };
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    // The peer still holds v1 and sends it.
    g.apply_delta(&peer.delta_since(&VersionVector::new()))
        .unwrap();
    assert_eq!(title_by_uuid(&g, uuid), "v2", "the newer local edit wins");
    peer.apply_delta(&g.delta_since(&peer.version_vector()))
        .unwrap();
    assert_eq!(title_by_uuid(&peer, uuid), "v2", "the peer converges on it");
}

/// A peer's edit made after the restart still beats the reopened copy.
#[test]
fn newer_peer_edit_wins_after_reopen() {
    let dir = TmpDir::new();
    let peer = NativeGraph::new();
    let uuid = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let n = g.create_node(node("v1")).unwrap();
        peer.apply_delta(&g.delta_since(&peer.version_vector()))
            .unwrap();
        n.uuid
    };
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    let pid = peer.node_id_of_uuid(uuid).unwrap();
    retitle(&peer, pid, "peer-v2");
    g.apply_delta(&peer.delta_since(&g.version_vector()))
        .unwrap();
    assert_eq!(title_by_uuid(&g, uuid), "peer-v2");
}

/// Stamps taken from a peer's delta are kept too: the receiver's version vector
/// after a reopen still says it has seen the peer's writes.
#[test]
fn applied_remote_stamps_survive_reopen() {
    let dir = TmpDir::new();
    let origin = NativeGraph::new();
    let n = origin.create_node(node("remote")).unwrap();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        g.apply_delta(&origin.delta_since(&VersionVector::new()))
            .unwrap();
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    let local = g.node_id_of_uuid(n.uuid).unwrap();
    assert_eq!(
        g.stamp_of(StampTarget::Node(local)),
        origin.stamp_of(StampTarget::Node(n.id))
    );
    assert!(
        origin.delta_since(&g.version_vector()).is_empty(),
        "nothing to resend after the receiver restarts"
    );
}

/// A deleted entity stays a tombstone after reopen; its old live stamp does not
/// come back.
#[test]
fn deleted_entity_has_no_live_stamp_after_reopen() {
    let dir = TmpDir::new();
    let gone = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let gone = g.create_node(node("gone")).unwrap();
        g.create_node(node("kept")).unwrap();
        g.delete_node(gone.id).unwrap();
        gone
    };
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(g.stamp_of(StampTarget::Node(gone.id)), None);
    let fresh = NativeGraph::new();
    fresh
        .apply_delta(&g.delta_since(&VersionVector::new()))
        .unwrap();
    assert_eq!(fresh.node_count(), 1);
    assert!(fresh.node_id_of_uuid(gone.uuid).is_none());
}

/// A store without a stamp log (written before stamps were kept) still seeds a
/// fresh peer, and its unstamped records lose to any real edit.
#[test]
fn store_without_a_stamp_log_still_seeds_a_fresh_peer() {
    let dir = TmpDir::new();
    let uuid = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let n = g.create_node(node("old")).unwrap();
        g.create_node(node("other")).unwrap();
        n.uuid
    };
    fs::remove_file(dir.stamp_log()).expect("stamps are kept in stamps.jsonl");

    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    let fresh = NativeGraph::new();
    fresh
        .apply_delta(&g.delta_since(&VersionVector::new()))
        .unwrap();
    assert_eq!(
        fresh.node_count(),
        2,
        "every live record is offered to a new peer"
    );

    // Any real edit beats the unstamped copy, in both directions.
    let fid = fresh.node_id_of_uuid(uuid).unwrap();
    retitle(&fresh, fid, "edited");
    g.apply_delta(&fresh.delta_since(&VersionVector::new()))
        .unwrap();
    assert_eq!(title_by_uuid(&g, uuid), "edited");
}

/// Compacting the WAL keeps the stamps.
#[test]
fn compact_wal_keeps_stamps() {
    let dir = TmpDir::new();
    let (id, stamp) = {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let n = g.create_node(node("a")).unwrap();
        retitle(&g, n.id, "b");
        g.compact_wal().unwrap();
        (n.id, g.stamp_of(StampTarget::Node(n.id)))
    };
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(g.stamp_of(StampTarget::Node(id)), stamp);
}

/// Repeated edits to the same records do not grow the stamp log without bound:
/// it is rewritten to one line per live entity once it gets large.
#[test]
fn stamp_log_stays_bounded_under_repeated_edits() {
    let dir = TmpDir::new();
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    let n = g.create_node(node("hot")).unwrap();
    for i in 0..2500 {
        retitle(&g, n.id, &format!("t{i}"));
    }
    let lines = fs::read_to_string(dir.stamp_log()).unwrap().lines().count();
    assert!(
        lines <= 2048,
        "stamp log has {lines} lines for 1 live entity"
    );
    drop(g);
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert!(g.stamp_of(StampTarget::Node(n.id)).is_some());
    let lines = fs::read_to_string(dir.stamp_log()).unwrap().lines().count();
    assert_eq!(
        lines, 1,
        "reopening rewrites the log to one line per live entity"
    );
}
