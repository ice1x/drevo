//! Version-vector delta exchange for multi-writer convergence
//! (issue #389, primitive #4).
//!
//! Once every write carries a causal [`Stamp`](crate::lww::Stamp) `(hlc, origin)`
//! (see [`crate::native::NativeGraph::next_stamp`]), a replica can summarise
//! everything it has seen as a [`VersionVector`](crate::delta::VersionVector): the greatest [`Hlc`](crate::hlc::Hlc)
//! observed from each origin. Handing that vector to a peer lets the peer compute
//! the **minimal** set of writes the holder is missing —
//! [`delta_since`](crate::native::NativeGraph::delta_since) — and the holder
//! folds them back in by Last-Writer-Wins on the stamp
//! ([`apply_delta`](crate::native::NativeGraph::apply_delta)). Two replicas that
//! exchange deltas in both directions converge.
//!
//! # Deletes converge too
//!
//! A delete is carried as a **tombstone**: the write path records the causal
//! stamp of every deletion (see
//! [`stamp_tombstone`](crate::native::NativeGraph)), so a delta ships
//! [`DeleteNode`](crate::delta::StampedChange::DeleteNode) /
//! [`DeleteEdge`](crate::delta::StampedChange::DeleteEdge) alongside its
//! upserts. Each entity therefore has a single Last-Writer-Wins timeline —
//! upsert or tombstone, whichever stamp is greater — and a delete on one replica
//! removes the record on the other once the delta is applied. A later upsert
//! (a greater stamp) resurrects it, exactly as LWW dictates.
//!
//! # Scope of this slice
//!
//! This is the **state-transfer** half: full or incremental sync to a *fresh or
//! lagging* replica, where node/edge ids do not collide (the receiver has never
//! minted an id the sender also minted). That covers bootstrapping a new replica
//! and catching up one that fell behind — the common offline-first case.
//!
//! One caveat remains, an explicitly-gated follow-up:
//!
//! * **Independent concurrent writers need id reconciliation.** drevo's ids are
//!   per-replica monotonic `u64`s, so two replicas that both minted node id `1`
//!   for *different* entities cannot be merged by id — that requires remapping on
//!   the globally-unique `uuid` each record carries. Until then, apply is correct
//!   only when the receiver's id space does not clash with the sender's.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;
use crate::lww::{OriginId, Stamp};
use crate::model::{Edge, Node};

#[cfg(test)]
use crate::model::Properties;

/// A summary of the greatest causal timestamp a replica has observed from each
/// origin. Comparing a remote vector against local stamps yields the minimal
/// delta the remote is missing.
///
/// The empty vector ([`VersionVector::new`]) has seen nothing, so a
/// `delta_since` against it returns the sender's entire live state — a full
/// bootstrap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector {
    /// The highest [`Hlc`] seen per origin. An origin absent from the map has
    /// never been observed (its implied floor is [`Hlc::default`], the minimum).
    max: HashMap<OriginId, Hlc>,
}

impl VersionVector {
    /// An empty vector that has observed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold `stamp` in, keeping the greatest [`Hlc`] seen from its origin.
    pub fn observe(&mut self, stamp: Stamp) {
        let slot = self.max.entry(stamp.origin()).or_default();
        if stamp.hlc() > *slot {
            *slot = stamp.hlc();
        }
    }

    /// The greatest [`Hlc`] observed from `origin`, or `None` if never seen.
    #[must_use]
    pub fn get(&self, origin: OriginId) -> Option<Hlc> {
        self.max.get(&origin).copied()
    }

    /// Whether this vector has already observed `stamp` — that is, it has seen a
    /// write from the same origin at an equal-or-later [`Hlc`]. A dominated
    /// stamp is *not* new to the holder and is excluded from a delta.
    #[must_use]
    pub fn dominates(&self, stamp: Stamp) -> bool {
        self.get(stamp.origin())
            .is_some_and(|seen| seen >= stamp.hlc())
    }

    /// Number of distinct origins observed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.max.len()
    }

    /// Whether no origin has been observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max.is_empty()
    }
}

/// One change carried in a [`Delta`]: an upsert (its full record plus the causal
/// [`Stamp`] that produced it) or a tombstone (an id plus the stamp of its
/// delete), so the receiver can Last-Writer-Wins merge purely by comparing
/// stamps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StampedChange {
    /// A node upsert at this stamp.
    Node(Node, Stamp),
    /// An edge upsert at this stamp.
    Edge(Edge, Stamp),
    /// A node deletion at this stamp — the tombstoned node id and the causal
    /// stamp of the delete. Wins over any node upsert with a lesser stamp.
    DeleteNode(u64, Stamp),
    /// An edge deletion at this stamp — the tombstoned edge id and the causal
    /// stamp of the delete.
    DeleteEdge(u64, Stamp),
}

impl StampedChange {
    /// The causal stamp of this change.
    #[must_use]
    pub fn stamp(&self) -> Stamp {
        match self {
            Self::Node(_, s)
            | Self::Edge(_, s)
            | Self::DeleteNode(_, s)
            | Self::DeleteEdge(_, s) => *s,
        }
    }

    /// Whether this change carries a node upsert (an upserted node's endpoints
    /// must exist before an edge referencing them — see [`apply_phase`]).
    ///
    /// [`apply_phase`]: Self::apply_phase
    #[must_use]
    pub fn is_node(&self) -> bool {
        matches!(self, Self::Node(..))
    }

    /// The phase in which this change must be applied within a delta, lowest
    /// first: upsert nodes (0) → upsert edges (1) → delete edges (2) → delete
    /// nodes (3). Endpoints thus exist before an edge that references them, and a
    /// node deletion — which cascades to its incident edges — runs last.
    #[must_use]
    pub fn apply_phase(&self) -> u8 {
        match self {
            Self::Node(..) => 0,
            Self::Edge(..) => 1,
            Self::DeleteEdge(..) => 2,
            Self::DeleteNode(..) => 3,
        }
    }
}

/// A batch of stamped upserts the receiver is missing, produced by
/// [`delta_since`](crate::native::NativeGraph::delta_since).
///
/// The batch is unordered on the wire; [`apply_delta`](crate::native::NativeGraph::apply_delta)
/// applies it in [`apply_phase`](StampedChange::apply_phase) order (upsert nodes,
/// upsert edges, delete edges, delete nodes) so an edge never lands before its
/// endpoints and a cascading node delete runs last.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delta {
    /// The stamped upserts, in no particular order.
    pub changes: Vec<StampedChange>,
}

impl Delta {
    /// Number of changes in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether the batch is empty — the receiver is already caught up.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Outcome of [`apply_delta`](crate::native::NativeGraph::apply_delta): how many
/// incoming changes won (were newer than the local stamp) versus were skipped
/// (the local write already dominated).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyStats {
    /// Changes whose stamp beat the local one and were installed.
    pub applied: usize,
    /// Changes the local state already dominated (older-or-equal stamp).
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(n: u64) -> OriginId {
        OriginId(n)
    }

    fn stamp(wall: i64, counter: u32, org: u64) -> Stamp {
        Stamp::new(Hlc::new(wall, counter), origin(org))
    }

    #[test]
    fn empty_vector_dominates_nothing() {
        let vv = VersionVector::new();
        assert!(vv.is_empty());
        assert!(!vv.dominates(stamp(1, 0, 7)));
    }

    #[test]
    fn observe_keeps_the_greatest_per_origin() {
        let mut vv = VersionVector::new();
        vv.observe(stamp(5, 0, 1));
        vv.observe(stamp(3, 9, 1)); // older — ignored
        vv.observe(stamp(2, 0, 2)); // different origin
        assert_eq!(vv.get(origin(1)), Some(Hlc::new(5, 0)));
        assert_eq!(vv.get(origin(2)), Some(Hlc::new(2, 0)));
        assert_eq!(vv.get(origin(3)), None);
        assert_eq!(vv.len(), 2);
    }

    #[test]
    fn dominates_is_greater_or_equal_on_the_same_origin() {
        let mut vv = VersionVector::new();
        vv.observe(stamp(5, 2, 1));
        assert!(vv.dominates(stamp(5, 2, 1))); // equal — seen
        assert!(vv.dominates(stamp(4, 9, 1))); // older — seen
        assert!(!vv.dominates(stamp(5, 3, 1))); // newer — missing
        assert!(!vv.dominates(stamp(5, 2, 2))); // other origin — missing
    }

    #[test]
    fn stamped_change_reports_its_stamp_and_kind() {
        let n = Node {
            id: 1,
            uuid: [0u8; 16],
            kind: "note".into(),
            title: "t".into(),
            body: String::new(),
            body_html: String::new(),
            created_at: 0,
            updated_at: 0,
            properties: Properties::default(),
        };
        let c = StampedChange::Node(n, stamp(1, 0, 1));
        assert_eq!(c.stamp(), stamp(1, 0, 1));
        assert!(c.is_node());
        assert_eq!(c.apply_phase(), 0);
    }

    #[test]
    fn delete_variants_report_stamp_and_order_after_upserts() {
        let del_node = StampedChange::DeleteNode(7, stamp(9, 0, 1));
        let del_edge = StampedChange::DeleteEdge(3, stamp(9, 1, 1));
        assert_eq!(del_node.stamp(), stamp(9, 0, 1));
        assert_eq!(del_edge.stamp(), stamp(9, 1, 1));
        assert!(!del_node.is_node());
        assert!(!del_edge.is_node());
        // Phases: upsert node (0) < upsert edge (1) < delete edge (2) < delete node (3).
        assert!(del_edge.apply_phase() < del_node.apply_phase());
        assert!(
            del_edge.apply_phase() > StampedChange::Edge(edge_stub(), stamp(1, 0, 1)).apply_phase()
        );
    }

    fn edge_stub() -> Edge {
        Edge {
            id: 3,
            uuid: [0u8; 16],
            from_id: 1,
            to_id: 2,
            kind: "links".into(),
            weight: 1.0,
            created_at: 0,
            properties: Properties::default(),
        }
    }
}
