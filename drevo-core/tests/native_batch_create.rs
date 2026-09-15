//! Native batch create (issue #446 S3 prerequisite, epic #444) — the native
//! counterparts of `Drevo::create_nodes` / `create_edges`, needed before the
//! drevo-py binding can move onto native.
//!
//! Contract mirrored from KV: all-or-nothing (a validation failure writes
//! nothing), and the whole batch costs ONE fsync (group commit).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::error::CoreError;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_batch_{}_{}",
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

fn nn(title: &str) -> NewNode {
    NewNode {
        kind: "doc".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}
fn ne(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "link".into(),
        weight: 1.0,
        properties: Properties(Default::default()),
    }
}

#[test]
fn create_nodes_assigns_ascending_ids() {
    let g = NativeGraph::new();
    let nodes = g.create_nodes(vec![nn("a"), nn("b"), nn("c")]).unwrap();
    assert_eq!(
        nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(g.node_count(), 3);
    assert_eq!(g.get_node_by_title("b").map(|n| n.id), Some(2));
}

#[test]
fn create_nodes_is_all_or_nothing_on_duplicate_title() {
    let g = NativeGraph::new();
    // Duplicate WITHIN the batch → whole batch rejected, nothing written.
    let err = g.create_nodes(vec![nn("dup"), nn("ok"), nn("dup")]);
    assert!(matches!(err, Err(CoreError::DuplicateTitle(t)) if t == "dup"));
    assert_eq!(g.node_count(), 0);

    // Duplicate against an EXISTING node → same, nothing added.
    g.create_nodes(vec![nn("x")]).unwrap();
    let err = g.create_nodes(vec![nn("y"), nn("x")]);
    assert!(matches!(err, Err(CoreError::DuplicateTitle(t)) if t == "x"));
    assert_eq!(g.node_count(), 1);
}

#[test]
fn create_edges_requires_existing_endpoints() {
    let g = NativeGraph::new();
    let ns = g.create_nodes(vec![nn("a"), nn("b")]).unwrap();
    let (a, b) = (ns[0].id, ns[1].id);

    // A missing endpoint fails the whole batch.
    let err = g.create_edges(vec![ne(a, b), ne(a, 999)]);
    assert!(matches!(err, Err(CoreError::NodeNotFound(999))));
    assert_eq!(g.edge_count(), 0);

    // A fully-valid batch creates every edge.
    let es = g.create_edges(vec![ne(a, b), ne(b, a)]).unwrap();
    assert_eq!(es.len(), 2);
    assert_eq!(g.edge_count(), 2);
}

#[test]
fn create_nodes_batch_is_one_fsync() {
    let tmp = TmpDir::new();
    let g = NativeGraph::open_durable(tmp.wal()).unwrap();
    let before = g.wal_fsync_count();
    g.create_nodes(vec![nn("a"), nn("b"), nn("c"), nn("d"), nn("e")])
        .unwrap();
    let after = g.wal_fsync_count();
    assert_eq!(
        after - before,
        1,
        "a create_nodes batch must cost exactly one fsync"
    );
}

#[test]
fn batch_created_nodes_and_edges_survive_reopen() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        let ns = g.create_nodes(vec![nn("a"), nn("b")]).unwrap();
        g.create_edges(vec![ne(ns[0].id, ns[1].id)]).unwrap();
    }
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    assert_eq!(g.get_node_by_title("a").map(|n| n.id), Some(1));
}
