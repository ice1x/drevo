//! Batched, single-fsync node deletion — `GraphEngine::delete_nodes` on the
//! native engine (issue #435). Deleting a subtree of `N` nodes must cost one
//! durability flush, not `N`, while preserving `delete_node`'s per-node effects
//! (cascade incident edges, durable tombstones per #389) and skipping ids that
//! are already gone.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::delta::{StampedChange, VersionVector};
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

// std-only temp dir (drevo-core is dependency-light; no `tempfile` dev-dep).
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_batch_del_{}_{}",
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
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

fn edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "child_of".to_string(),
        weight: 1.0,
        properties: Properties(Default::default()),
    }
}

/// Removes the nodes, cascades their incident edges, leaves survivors intact,
/// and returns the count actually deleted.
#[test]
fn delete_nodes_removes_nodes_and_cascades_edges() {
    let g = NativeGraph::new();
    // A folder with two notes, plus an unrelated survivor.
    let folder = g.create_node(node("folder")).unwrap();
    let n1 = g.create_node(node("note-1")).unwrap();
    let n2 = g.create_node(node("note-2")).unwrap();
    let survivor = g.create_node(node("keep")).unwrap();
    g.create_edge(edge(n1.id, folder.id)).unwrap();
    g.create_edge(edge(n2.id, folder.id)).unwrap();
    assert_eq!(g.node_count(), 4);
    assert_eq!(g.edge_count(), 2);

    let deleted = g.delete_nodes(&[folder.id, n1.id, n2.id]).unwrap();
    assert_eq!(deleted, 3, "all three requested nodes were present");
    assert_eq!(g.node_count(), 1, "only the survivor remains");
    assert_eq!(g.edge_count(), 0, "incident edges cascaded away");
    assert!(g.get_node(survivor.id).unwrap().is_some());
    assert!(g.get_node(folder.id).unwrap().is_none());
}

/// Ids that no longer exist are skipped, not errors; the count reflects only
/// the nodes actually removed.
#[test]
fn delete_nodes_skips_absent_ids() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap();
    let b = g.create_node(node("b")).unwrap();

    let deleted = g.delete_nodes(&[a.id, 9_999, b.id, 10_000]).unwrap();
    assert_eq!(deleted, 2, "only the two real ids counted");
    assert_eq!(g.node_count(), 0);

    // A second call over the now-absent ids is a no-op returning 0.
    assert_eq!(g.delete_nodes(&[a.id, b.id]).unwrap(), 0);
}

/// The durability contract: on the WAL engine a batched delete of `N` nodes
/// costs exactly **one** fsync, where `N` one-by-one deletes would cost `N`.
#[test]
fn delete_nodes_uses_a_single_fsync() {
    let tmp = TmpDir::new();
    let g = NativeGraph::open_durable(tmp.wal()).unwrap();

    let ids: Vec<u64> = (0..8)
        .map(|i| g.create_node(node(&format!("n{i}"))).unwrap().id)
        .collect();

    let before = g.wal_fsync_count();
    let deleted = g.delete_nodes(&ids).unwrap();
    let after = g.wal_fsync_count();

    assert_eq!(deleted, 8);
    assert_eq!(
        after - before,
        1,
        "deleting {} nodes must flush once, not {} times",
        ids.len(),
        ids.len()
    );
    assert_eq!(g.node_count(), 0);
}

/// The batch is durable: after a reopen the deleted nodes stay gone and the
/// survivors remain (the WAL replays the batch).
#[test]
fn delete_nodes_are_durable_across_reopen() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    let (keep_id, gone_id) = {
        let g = NativeGraph::open_durable(&path).unwrap();
        let keep = g.create_node(node("keep")).unwrap();
        let g1 = g.create_node(node("gone-1")).unwrap();
        let g2 = g.create_node(node("gone-2")).unwrap();
        assert_eq!(g.delete_nodes(&[g1.id, g2.id]).unwrap(), 2);
        (keep.id, g1.id)
    };
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g.node_count(), 1, "the deletions persisted across reopen");
    assert!(g.get_node(keep_id).unwrap().is_some());
    assert!(g.get_node(gone_id).unwrap().is_none());
}

/// A batched delete stamps a causal tombstone per node, exactly like the single
/// `delete_node` path (issue #389) — a peer/replica can tell "deleted" from
/// "never existed". Observed through the CRDT delta, which advertises each
/// tombstone as a `DeleteNode(uuid, stamp)`.
#[test]
fn delete_nodes_stamp_tombstones_like_single_delete() {
    let g = NativeGraph::new();
    let single = g.create_node(node("single")).unwrap();
    let batch = g.create_node(node("batch")).unwrap();
    let (single_uuid, batch_uuid) = (single.uuid, batch.uuid);

    g.delete_node(single.id).unwrap();
    assert_eq!(g.delete_nodes(&[batch.id]).unwrap(), 1);

    let deleted: Vec<[u8; 16]> = g
        .delta_since(&VersionVector::new())
        .changes
        .iter()
        .filter_map(|c| match c {
            StampedChange::DeleteNode(uuid, _) => Some(*uuid),
            _ => None,
        })
        .collect();
    assert!(
        deleted.contains(&single_uuid),
        "single delete advertises a node tombstone"
    );
    assert!(
        deleted.contains(&batch_uuid),
        "batched delete advertises a node tombstone too"
    );
}
