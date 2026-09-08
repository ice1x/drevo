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
//! # Transports
//!
//! [`NativeReplica`](crate::replica::NativeReplica) tails an in-process source
//! `NativeGraph` handle. [`WalTailer`](crate::replica::WalTailer) tails a
//! primary's **on-disk WAL file** across a process or machine boundary — the
//! real cross-process feed transport — yielding the same
//! [`WalOp`](crate::native::WalOp)s to apply. A streamed network feed is a
//! later slice of #383; all feed the same verbatim apply path.
//!
//! # Failover
//!
//! [`into_primary`](crate::replica::NativeReplica::into_primary) /
//! [`promote_durable`](crate::replica::NativeReplica::promote_durable) turn a
//! replica into a writable primary once the old primary is gone — **coordinated
//! (manual) failover**, since a warm mirror already holds the full state.
//! *Automatic*, consensus-based failover (leader election, split-brain
//! arbitration via Raft/`openraft`) is a separate, heavier track tracked in its
//! own issue.

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

    /// Promote this replica to a **writable primary**, handing back its mirrored
    /// graph. The replica role ends (`self` is consumed) and the returned
    /// [`NativeGraph`] now accepts writes — the failover path when the old
    /// primary is gone.
    ///
    /// This is **coordinated / manual** failover: the caller is responsible for
    /// ensuring the old primary is truly down (stop tailing it, fence it off).
    /// There is no consensus or split-brain arbitration here — that is automatic
    /// Raft-based failover, tracked separately (issue #383's follow-up). The
    /// returned graph is in-memory, as the replica was; use
    /// [`promote_durable`](Self::promote_durable) to take over durably.
    #[must_use]
    pub fn into_primary(self) -> NativeGraph {
        self.graph
    }

    /// Promote to a **durable** primary: seed a fresh durable engine at `path`
    /// with this replica's current mirrored state (replayed and logged to the
    /// new WAL) and return it, so writes accepted after failover are crash-safe.
    /// `self` is consumed. `path` should be a fresh WAL location for the new
    /// primary; seeding an existing non-empty WAL re-logs the overlapping ops
    /// (harmless — upserts are idempotent by id — but it grows the log).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn promote_durable(self, path: impl AsRef<std::path::Path>) -> Result<NativeGraph> {
        let primary = NativeGraph::open_durable(path)?;
        primary.apply_wal_ops(&self.graph.dump_wal())?;
        Ok(primary)
    }
}

/// Tails a primary's on-disk WAL file — the append-only JSON-Lines op log the
/// durable engine writes (`native.wal`) — across a process or machine boundary
/// (issue #383). This is the cross-process feed transport: a replica process
/// opens the leader's WAL path and polls for newly-appended records.
///
/// [`poll`](Self::poll) returns the [`WalOp`](crate::native::WalOp)s appended
/// since the last call and advances a byte cursor past every **complete**
/// (newline-terminated) record; a trailing partial line — a write still in
/// flight — is left for the next poll, so a reader never applies half a record.
/// Feed the returned ops to
/// [`NativeGraph::apply_wal_ops`](crate::native::NativeGraph::apply_wal_ops) (or
/// let a [`NativeReplica`] own the graph). The byte cursor is the resume point:
/// persist it to survive a replica restart.
#[cfg(not(target_arch = "wasm32"))]
pub struct WalTailer {
    path: std::path::PathBuf,
    offset: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl WalTailer {
    /// A tailer for the WAL at `path`, starting from the beginning.
    #[must_use]
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
        }
    }

    /// A tailer resuming at a previously-persisted byte `offset`.
    #[must_use]
    pub fn resume_at(path: impl Into<std::path::PathBuf>, offset: u64) -> Self {
        Self {
            path: path.into(),
            offset,
        }
    }

    /// The byte cursor — how far into the WAL this tailer has consumed. Persist
    /// it to resume after a restart via [`resume_at`](Self::resume_at).
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read WAL records appended since the last poll, parse them into ops, and
    /// advance the byte cursor past every complete line. A trailing partial line
    /// is left unconsumed. A not-yet-created WAL file yields no ops.
    pub fn poll(&mut self) -> Result<Vec<crate::native::WalOp>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        // Consume only up to the last newline; a trailing partial line is an
        // in-flight append and waits for the next poll.
        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let complete = &buf[..=last_nl];
        let mut ops = Vec::new();
        for line in complete.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(mut recs) = NativeGraph::parse_wal_record(line) {
                ops.append(&mut recs);
            }
        }
        self.offset += complete.len() as u64;
        Ok(ops)
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
    fn promoted_replica_keeps_its_data_and_accepts_writes() {
        // A replica mirrors a source, then the source "dies" and the replica is
        // promoted to a writable primary — it keeps the mirrored data and now
        // takes new writes.
        let src = NativeGraph::new();
        let a = src.create_node(nn("n", "a")).unwrap();
        let mut replica = NativeReplica::new();
        replica.sync_from(&src).unwrap();

        let primary = replica.into_primary();
        // Mirrored data is intact...
        assert!(primary.get_node(a.id).unwrap().is_some());
        // ...and the promoted graph accepts new writes.
        let b = primary.create_node(nn("n", "b")).unwrap();
        assert!(primary.get_node(b.id).unwrap().is_some());
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
