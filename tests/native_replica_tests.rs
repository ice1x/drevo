//! Cross-process WAL-shipping replication for the native engine (issue #383,
//! Phase 9). A replica in a *separate* engine handle catches up with a durable
//! primary using only the primary's on-disk WAL file as the channel — a
//! `WalTailer` reads newly-appended records and `apply_wal_ops` replays them
//! verbatim onto the replica's graph.

use drevo::engine::GraphEngine;
use drevo::model::{NewEdge, NewNode};
use drevo::native::{NativeGraph, WalOp};
use drevo::replica::WalTailer;

fn nn(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn edge(from: u64, to: u64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "links".to_string(),
        weight: 1.0,
        properties: Default::default(),
    }
}

/// The primary writes to its durable WAL; a replica (a distinct `NativeGraph`
/// handle) converges by tailing that WAL file — nothing else is shared.
#[test]
fn wal_tailer_replicates_a_durable_primary_across_handles() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");

    let primary = NativeGraph::open_durable(&wal).unwrap();
    let a = primary.create_node(nn("n", "a")).unwrap();
    let b = primary.create_node(nn("n", "b")).unwrap();
    let e = primary.create_edge(edge(a.id, b.id)).unwrap();

    // The replica shares only the WAL path.
    let replica = NativeGraph::new();
    let mut tailer = WalTailer::new(&wal);
    let ops = tailer.poll().unwrap();
    assert_eq!(ops.len(), 3, "2 node upserts + 1 edge upsert");
    replica.apply_wal_ops(&ops).unwrap();

    // Byte-identical mirror, by id.
    assert_eq!(
        replica.get_node(a.id).unwrap(),
        primary.get_node(a.id).unwrap()
    );
    assert_eq!(
        replica.get_edge(e.id).unwrap(),
        primary.get_edge(e.id).unwrap()
    );

    // Incremental: a later write + delete on the primary ships on the next poll.
    let c = primary.create_node(nn("n", "c")).unwrap();
    primary.delete_node(b.id).unwrap();
    let ops = tailer.poll().unwrap();
    assert_eq!(ops.len(), 2, "1 upsert + 1 delete");
    replica.apply_wal_ops(&ops).unwrap();
    assert!(replica.get_node(c.id).unwrap().is_some());
    assert!(replica.get_node(b.id).unwrap().is_none(), "delete shipped");

    // Nothing new → an empty poll, and the cursor holds.
    let off = tailer.offset();
    assert!(tailer.poll().unwrap().is_empty());
    assert_eq!(tailer.offset(), off);
}

/// Failover: a warm replica is promoted to a durable primary — it keeps the
/// mirrored data, accepts new writes, and those writes survive a reopen.
#[test]
fn promote_durable_takes_over_as_a_crash_safe_primary() {
    use drevo::replica::NativeReplica;

    let dir = tempfile::tempdir().unwrap();
    let primary_wal = dir.path().join("primary.wal");
    let new_wal = dir.path().join("promoted.wal");

    let primary = NativeGraph::open_durable(&primary_wal).unwrap();
    let a = primary.create_node(nn("n", "a")).unwrap();
    primary.create_node(nn("n", "b")).unwrap();

    // Replica catches up by tailing the primary's WAL.
    let replica_graph = NativeGraph::new();
    let mut tailer = WalTailer::new(&primary_wal);
    replica_graph
        .apply_wal_ops(&tailer.poll().unwrap())
        .unwrap();
    let mut replica = NativeReplica::new();
    // (Use the in-process replica for promotion; feed it the same source.)
    replica.sync_from(&primary).unwrap();
    drop(replica_graph);

    // Primary "dies"; promote the replica to a durable primary at a fresh WAL.
    let promoted = replica.promote_durable(&new_wal).unwrap();
    assert!(
        promoted.get_node(a.id).unwrap().is_some(),
        "kept mirrored data"
    );
    let c = promoted.create_node(nn("n", "c")).unwrap();
    drop(promoted);

    // The promoted primary is crash-safe: reopening its WAL recovers everything.
    let recovered = NativeGraph::open_durable(&new_wal).unwrap();
    assert!(recovered.get_node(a.id).unwrap().is_some());
    assert!(
        recovered.get_node(c.id).unwrap().is_some(),
        "post-failover write survived"
    );
}

/// A record still being written (no trailing newline yet) must not be consumed
/// until it is complete — a reader never applies half a line.
#[test]
fn wal_tailer_leaves_a_partial_trailing_line() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");

    let line1 = format!(
        "{}\n",
        serde_json::to_string(&WalOp::DeleteNode(1)).unwrap()
    );
    let line2_body = serde_json::to_string(&WalOp::DeleteNode(2)).unwrap();

    // One complete record, then a partial second line with no newline yet.
    {
        let mut f = std::fs::File::create(&wal).unwrap();
        f.write_all(line1.as_bytes()).unwrap();
        f.write_all(line2_body.as_bytes()).unwrap();
    }

    let mut tailer = WalTailer::new(&wal);
    let ops = tailer.poll().unwrap();
    assert_eq!(ops.len(), 1, "only the completed record");
    assert_eq!(
        tailer.offset() as usize,
        line1.len(),
        "cursor past the newline"
    );

    // Finish the partial line; the next poll picks it up.
    {
        use std::fs::OpenOptions;
        let mut f = OpenOptions::new().append(true).open(&wal).unwrap();
        f.write_all(b"\n").unwrap();
    }
    let ops = tailer.poll().unwrap();
    assert_eq!(ops.len(), 1, "the now-complete second record");
}
