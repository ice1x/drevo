//! Durable native embedding store (issue #446, epic #444) — Rust tests against
//! the `drevo-core` [`NativeGraph`] seam.
//!
//! The native counterpart of the KV `vec:` keyspace: set/get/delete/count/batch
//! with node-existence validation, and — the point of "durable" — embeddings
//! survive a reopen (WAL recovery) and a compaction.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::NativeGraph;

// std-only temp dir (drevo-core is dependency-light; no `tempfile` dev-dep).
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_emb_{}_{}",
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
        kind: "doc".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

#[test]
fn set_get_delete_count_in_memory() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap().id;
    let b = g.create_node(node("b")).unwrap().id;

    assert_eq!(g.embedding_count(), 0);
    assert_eq!(g.get_embedding(a), None);

    g.set_embedding(a, vec![1.0, 2.0, 3.0]).unwrap();
    g.set_embedding(b, vec![4.0, 5.0, 6.0]).unwrap();
    assert_eq!(g.embedding_count(), 2);
    assert_eq!(g.get_embedding(a), Some(vec![1.0, 2.0, 3.0]));

    // Replacing an embedding overwrites, does not add.
    g.set_embedding(a, vec![9.0, 9.0, 9.0]).unwrap();
    assert_eq!(g.get_embedding(a), Some(vec![9.0, 9.0, 9.0]));
    assert_eq!(g.embedding_count(), 2);

    // Delete is idempotent.
    g.delete_embedding(a).unwrap();
    g.delete_embedding(a).unwrap();
    assert_eq!(g.get_embedding(a), None);
    assert_eq!(g.embedding_count(), 1);
}

#[test]
fn set_embedding_requires_the_node_to_exist() {
    let g = NativeGraph::new();
    assert!(matches!(
        g.set_embedding(999, vec![1.0]),
        Err(CoreError::NodeNotFound(999))
    ));
    assert_eq!(g.embedding_count(), 0);
}

#[test]
fn batch_is_all_or_nothing() {
    let g = NativeGraph::new();
    let a = g.create_node(node("a")).unwrap().id;
    // Second entry references a non-existent node → nothing is written.
    let err = g.set_embeddings_batch(&[(a, vec![1.0]), (999, vec![2.0])]);
    assert!(matches!(err, Err(CoreError::NodeNotFound(999))));
    assert_eq!(g.embedding_count(), 0);

    // A fully-valid batch writes every entry.
    let b = g.create_node(node("b")).unwrap().id;
    g.set_embeddings_batch(&[(a, vec![1.0]), (b, vec![2.0])])
        .unwrap();
    assert_eq!(g.embedding_count(), 2);
    assert_eq!(g.get_embedding(b), Some(vec![2.0]));
}

#[test]
fn embeddings_survive_a_reopen() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    let (a, b);
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        a = g.create_node(node("a")).unwrap().id;
        b = g.create_node(node("b")).unwrap().id;
        g.set_embedding(a, vec![1.0, 2.0]).unwrap();
        g.set_embedding(b, vec![3.0, 4.0]).unwrap();
        g.delete_embedding(b).unwrap(); // a delete must also be durable
    } // drop without explicit close — simulate a crash after the acks

    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g.embedding_count(), 1);
    assert_eq!(g.get_embedding(a), Some(vec![1.0, 2.0]));
    assert_eq!(g.get_embedding(b), None);
}

#[test]
fn embeddings_survive_a_compaction() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    let g = NativeGraph::open_durable(&path).unwrap();
    let a = g.create_node(node("a")).unwrap().id;
    // Churn the same embedding so compaction has history to collapse.
    for i in 0..5 {
        g.set_embedding(a, vec![i as f32, 0.0]).unwrap();
    }
    g.compact_wal().unwrap();
    assert_eq!(g.get_embedding(a), Some(vec![4.0, 0.0]));

    // ...and the compacted state still recovers on reopen.
    drop(g);
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g.get_embedding(a), Some(vec![4.0, 0.0]));
    assert_eq!(g.embedding_count(), 1);
}
