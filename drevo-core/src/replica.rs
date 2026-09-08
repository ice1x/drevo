//! WAL-shipping read replica for the native engine (issue #383, Phase 9 —
//! horizontal scale / HA).
//!
//! A [`NativeReplica`](crate::replica::NativeReplica) keeps its own in-memory
//! [`NativeGraph`](crate::native::NativeGraph) converged with a *source* (the
//! primary) by **tailing the source's change-feed**
//! ([`changes_since`](crate::native::NativeGraph::changes_since)) and applying
//! each committed op verbatim — ids, uuids and timestamps preserved — so the
//! replica is a faithful, byte-identical mirror. This is single-master
//! log-shipping replication, distinct from the id-remapping peer-to-peer merge
//! in [`apply_delta`](crate::native::NativeGraph::apply_delta): a follower does
//! not invent its own ids, it reproduces the leader's.
//!
//! [`sync_from`](crate::replica::NativeReplica::sync_from) pulls whatever the
//! source has produced since the replica's cursor, applies it, and reports how
//! far behind the replica still is
//! ([`SyncReport::lag`](crate::replica::SyncReport::lag)). If the replica has
//! fallen behind the source's *retained* feed window (the source trimmed history
//! the replica had not yet seen), the batch comes back `lagged` and the replica
//! **re-snapshots** from the source's current state rather than applying a gap.
//!
//! # This slice
//!
//! In-process tailing of a source `NativeGraph` handle — the core apply / lag /
//! resume-from-cursor mechanism. Shipping the feed across a process or network
//! boundary (a WAL-file tail or a streamed feed) and Raft-based failover are
//! later slices of #383; they feed the same
//! [`sync_from`](crate::replica::NativeReplica::sync_from) loop.

use crate::error::Result;
use crate::native::NativeGraph;

/// The outcome of one [`NativeReplica::sync_from`] round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of change-feed ops applied this round (for a `resynced` round,
    /// the size of the full state the replica rebuilt from).
    pub applied: usize,
    /// Whether the replica had fallen behind the source's retained feed and
    /// re-snapshotted from the source's current state instead of tailing.
    pub resynced: bool,
    /// Remaining lag after this round: `source.change_head() - cursor`. `0`
    /// means the replica is fully caught up (as of this instant).
    pub lag: u64,
}

/// A read replica: an in-memory mirror of a source [`NativeGraph`], advanced by
/// tailing the source's change-feed. Serve reads from [`graph`](Self::graph).
pub struct NativeReplica {
    graph: NativeGraph,
    cursor: u64,
}

impl Default for NativeReplica {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeReplica {
    /// A fresh, empty replica positioned before the source's first change.
    #[must_use]
    pub fn new() -> Self {
        Self {
            graph: NativeGraph::new(),
            cursor: 0,
        }
    }

    /// The replicated graph — serve reads from here.
    #[must_use]
    pub fn graph(&self) -> &NativeGraph {
        &self.graph
    }

    /// The replica's change-feed cursor: the sequence number through which it
    /// has applied the source's log.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// How far behind `source` this replica currently is — the number of
    /// committed changes it has not yet applied. `0` means caught up.
    #[must_use]
    pub fn lag(&self, source: &NativeGraph) -> u64 {
        source.change_head().saturating_sub(self.cursor)
    }

    /// Pull and apply everything `source` has committed since this replica's
    /// cursor, advancing it. Returns a [`SyncReport`]. Idempotent: a second call
    /// with no new source writes applies nothing and reports `lag = 0`.
    ///
    /// If the replica has fallen behind the source's retained feed window, the
    /// source reports `lagged` and the replica re-snapshots from the source's
    /// current full state (a fresh, converged mirror) rather than applying an
    /// incomplete tail.
    pub fn sync_from(&mut self, source: &NativeGraph) -> Result<SyncReport> {
        let batch = source.changes_since(self.cursor);
        if batch.lagged {
            // History we hadn't seen was trimmed — rebuild from the source's
            // current state instead of applying a gapped tail.
            let ops = source.dump_wal();
            let applied = ops.len();
            self.graph = NativeGraph::replay(ops);
            self.cursor = source.change_head();
            return Ok(SyncReport {
                applied,
                resynced: true,
                lag: self.lag(source),
            });
        }
        let applied = batch.ops.len();
        self.graph.apply_wal_ops(&batch.ops)?;
        self.cursor = batch.cursor;
        Ok(SyncReport {
            applied,
            resynced: false,
            lag: self.lag(source),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::GraphEngine;
    use crate::model::{NewEdge, NewNode};

    fn nn(kind: &str, title: &str) -> NewNode {
        NewNode {
            kind: kind.into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Default::default(),
        }
    }

    fn edge(from: u64, to: u64) -> NewEdge {
        NewEdge {
            from_id: from,
            to_id: to,
            kind: "links".into(),
            weight: 1.0,
            properties: Default::default(),
        }
    }

    #[test]
    fn replica_bootstraps_as_an_exact_mirror() {
        let src = NativeGraph::new();
        let a = src.create_node(nn("n", "a")).unwrap();
        let b = src.create_node(nn("n", "b")).unwrap();
        let e = src.create_edge(edge(a.id, b.id)).unwrap();

        let mut replica = NativeReplica::new();
        assert_eq!(replica.lag(&src), 3, "2 nodes + 1 edge behind");
        let rep = replica.sync_from(&src).unwrap();
        assert_eq!(rep.applied, 3);
        assert!(!rep.resynced);
        assert_eq!(rep.lag, 0);

        // Byte-identical: same ids, same records.
        assert_eq!(
            replica.graph().get_node(a.id).unwrap(),
            src.get_node(a.id).unwrap()
        );
        assert_eq!(
            replica.graph().get_node(b.id).unwrap(),
            src.get_node(b.id).unwrap()
        );
        assert_eq!(
            replica.graph().get_edge(e.id).unwrap(),
            src.get_edge(e.id).unwrap()
        );
    }

    #[test]
    fn replica_applies_incrementally_and_tracks_lag() {
        let src = NativeGraph::new();
        let a = src.create_node(nn("n", "a")).unwrap();
        let mut replica = NativeReplica::new();
        assert_eq!(replica.sync_from(&src).unwrap().applied, 1);
        assert!(
            replica.sync_from(&src).unwrap().applied == 0,
            "idempotent when caught up"
        );

        // A write on the source shows up as lag until the next sync.
        let b = src.create_node(nn("n", "b")).unwrap();
        assert_eq!(replica.lag(&src), 1);
        let rep = replica.sync_from(&src).unwrap();
        assert_eq!(rep.applied, 1, "only the new op");
        assert_eq!(rep.lag, 0);
        assert!(replica.graph().get_node(b.id).unwrap().is_some());
        // The earlier node is still there.
        assert!(replica.graph().get_node(a.id).unwrap().is_some());
    }

    #[test]
    fn replica_mirrors_deletes() {
        let src = NativeGraph::new();
        let a = src.create_node(nn("n", "gone")).unwrap();
        let b = src.create_node(nn("n", "keep")).unwrap();
        let mut replica = NativeReplica::new();
        replica.sync_from(&src).unwrap();
        assert!(replica.graph().get_node(a.id).unwrap().is_some());

        src.delete_node(a.id).unwrap();
        let rep = replica.sync_from(&src).unwrap();
        assert_eq!(rep.applied, 1);
        assert!(
            replica.graph().get_node(a.id).unwrap().is_none(),
            "delete propagated"
        );
        assert!(replica.graph().get_node(b.id).unwrap().is_some());
    }

    #[test]
    fn replica_resnapshots_when_it_falls_behind_the_retained_feed() {
        let src = NativeGraph::new();
        let a = src.create_node(nn("n", "a")).unwrap();
        src.create_node(nn("n", "b")).unwrap();

        // The source trims its whole retained feed before the replica (cursor 0)
        // ever tails it — so the next sync must re-snapshot, not apply a gap.
        src.trim_before(src.change_head());

        let mut replica = NativeReplica::new();
        let rep = replica.sync_from(&src).unwrap();
        assert!(rep.resynced, "fell behind the retained feed → re-snapshot");
        assert_eq!(rep.lag, 0);
        // Still an exact mirror after the re-snapshot.
        assert_eq!(
            replica.graph().get_node(a.id).unwrap(),
            src.get_node(a.id).unwrap()
        );
        assert_eq!(replica.cursor(), src.change_head());
    }
}
