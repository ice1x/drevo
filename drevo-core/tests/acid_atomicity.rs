//! ACID conformance — **A (Atomicity)** for the default `native-durable`
//! engine, exercised as **Rust** tests directly against the `drevo-core`
//! [`NativeGraph`] seam (no HTTP layer).
//!
//! A transaction is all-or-nothing: on commit every buffered write lands; on
//! rollback (or drop) none does; a commit rejected by a constraint applies
//! nothing (not even the writes that were individually valid); and — on the
//! durable engine — a crash that tears a committed batch on disk replays the
//! whole batch or none of it, never a fraction.
//!
//! Part of the ACID conformance set (issue #424).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::{CommitError, Constraint, NativeGraph};

// A std-only temp directory (drevo-core is deliberately dependency-light, so no
// `tempfile` dev-dependency): a unique dir under the system temp, removed on drop.
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_acid_a_{}_{}",
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

fn node_with_email(title: &str, email: &str) -> NewNode {
    let mut nn = node(title);
    nn.kind = "user".to_string();
    nn.properties = Properties(std::collections::HashMap::from([(
        "email".to_string(),
        serde_json::Value::String(email.to_string()),
    )]));
    nn
}

fn edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "links".to_string(),
        weight: 1.0,
        properties: Properties(Default::default()),
    }
}

/// In memory: rollback applies nothing; commit applies everything.
#[test]
fn commit_is_all_or_nothing_in_memory() {
    let g = NativeGraph::new();

    let mut tx = g.begin();
    tx.create_node(node("a")).unwrap();
    tx.create_node(node("b")).unwrap();
    tx.rollback();
    assert_eq!(g.node_count(), 0, "rollback applied none of the batch");

    let mut tx = g.begin();
    tx.create_node(node("a")).unwrap();
    tx.create_node(node("b")).unwrap();
    tx.commit().unwrap();
    assert_eq!(g.node_count(), 2, "commit applied the whole batch");
}

/// Dropping a transaction without committing discards its writes.
#[test]
fn dropping_a_transaction_discards_its_writes() {
    let g = NativeGraph::new();
    {
        let mut tx = g.begin();
        tx.create_node(node("a")).unwrap();
        // no commit — dropped here
    }
    assert_eq!(
        g.node_count(),
        0,
        "an un-committed, dropped tx leaves nothing"
    );
}

/// A commit rejected by a constraint applies nothing — not even the first,
/// individually-valid write (atomic abort).
#[test]
fn a_constraint_rejected_commit_applies_nothing() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "user".into(),
        property: "email".into(),
    })
    .unwrap();

    let mut tx = g.begin();
    tx.create_node(node_with_email("u1", "a@x")).unwrap(); // individually fine
    tx.create_node(node_with_email("u2", "a@x")).unwrap(); // dup → whole tx fails
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(
        g.node_count(),
        0,
        "atomic abort: the first, valid write did not partially land"
    );
}

/// Durable: a committed multi-op batch (nodes + an edge) survives a reopen as
/// a whole — one atomic on-disk batch.
#[test]
fn a_committed_batch_is_durable_as_a_whole() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        let mut tx = g.begin();
        let a = tx.create_node(node("a")).unwrap();
        let b = tx.create_node(node("b")).unwrap();
        tx.create_edge(edge(a.id, b.id)).unwrap();
        tx.commit().unwrap();
    }
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g.node_count(), 2, "both nodes of the batch are durable");
    assert_eq!(g.edge_count(), 1, "the edge of the same batch is durable");
}

/// Durable: a crash that tears a committed batch on disk replays **none** of
/// that transaction — never one of its two writes — while the earlier,
/// separately-acknowledged write survives. Mirrors the top crate's WAL guard
/// but stated as the atomicity conformance proof.
#[test]
fn a_torn_committed_batch_replays_none_of_the_transaction() {
    let tmp = TmpDir::new();
    let path = tmp.wal();
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        g.create_node(node("ada")).unwrap(); // acknowledged on its own line
        let mut tx = g.begin();
        tx.create_node(node("bob")).unwrap();
        tx.create_node(node("cy")).unwrap();
        tx.commit().unwrap(); // the 2-op batch is the final record
    }
    // A clean reopen sees all three.
    assert_eq!(NativeGraph::open_durable(&path).unwrap().node_count(), 3);

    // Cut the file at several points inside that final batch record: however
    // many of the batch bytes survived, recovery must apply none of the batch.
    let bytes = fs::read(&path).unwrap();
    let body = &bytes[..bytes.len() - 1];
    let batch_start = body.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let batch_len = bytes.len() - batch_start;
    assert!(batch_len > 8, "expected a real batch record at the tail");
    for fraction in [1u64, 2, 3] {
        let cut = batch_start + (batch_len as u64 * fraction / 4) as usize;
        let tmp2 = TmpDir::new();
        let path2 = tmp2.wal();
        fs::write(&path2, &bytes[..cut]).unwrap();
        let g = NativeGraph::open_durable(&path2)
            .unwrap_or_else(|e| panic!("cut at {cut} must recover, got {e}"));
        assert_eq!(
            g.node_count(),
            1,
            "a torn batch (cut at {cut}) replays none of the 2-op transaction"
        );
    }

    // Sanity: an untouched, fully-fsynced tail keeps working.
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"UpsertNode\":{\"id\":99,\"uu").unwrap(); // torn *new* partial tail
    drop(f);
    let g = NativeGraph::open_durable(&path).expect("a torn new tail is truncated, log usable");
    assert_eq!(g.node_count(), 3, "the fully-committed history is intact");
}
