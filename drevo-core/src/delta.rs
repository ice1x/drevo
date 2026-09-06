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
//! # Identity is the `uuid`, not the local id
//!
//! drevo's node/edge ids are per-replica monotonic `u64`s: two replicas
//! independently mint id `1`, `2`, … for *different* entities. Merging by id
//! would therefore collide. So every change on the wire is keyed by the
//! globally-unique `uuid` each record carries, and
//! [`apply_delta`](crate::native::NativeGraph::apply_delta) **remaps** the
//! sender's ids into the receiver's own id space:
//!
//! * a node/edge whose `uuid` the receiver already holds is merged in place at
//!   the receiver's local id;
//! * a `uuid` new to the receiver is minted a fresh local id;
//! * an edge carries its **endpoints by `uuid`**
//!   ([`from_uuid`](crate::delta::StampedEdge::from_uuid) /
//!   [`to_uuid`](crate::delta::StampedEdge::to_uuid)), so the receiver resolves
//!   them to *its* node ids regardless of what the sender called them;
//! * a tombstone ([`DeleteNode`](crate::delta::StampedChange::DeleteNode) /
//!   [`DeleteEdge`](crate::delta::StampedChange::DeleteEdge)) is keyed by `uuid`
//!   too.
//!
//! Independent concurrent writers therefore converge: this is symmetric,
//! bidirectional exchange, not just bootstrap of a fresh replica.
//!
//! # Remaining limitations
//!
//! * **Title uniqueness is not reconciled.** A node's `title` is a natural key
//!   inside one graph; two replicas that mint *different* uuids under the *same*
//!   title merge into two nodes sharing a title (the title index resolves to the
//!   last writer). Reconciling that is a semantic-merge concern beyond id remap.
//! * **The uuid resolution is rebuilt per apply.** `apply_delta` scans the live
//!   graph once to index `uuid → local id` (plus the in-memory tombstones), so a
//!   merge is `O(n + delta)`; a persistent uuid index is a later optimisation.
//! * **Stamps and tombstones live in memory**, rebuilt as writes happen after a
//!   restart — persisting them into the WAL touches the on-disk format and is a
//!   separate, explicitly-gated slice.

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

/// An edge on the wire, carrying its endpoints by `uuid` so a receiver can remap
/// them into its own id space. The wrapped [`Edge`]'s own `id`, `from_id` and
/// `to_id` are the *sender's* local ids — meaningful only to the sender; the
/// edge's identity is `edge.uuid` and its endpoints are [`from_uuid`] /
/// [`to_uuid`].
///
/// [`from_uuid`]: Self::from_uuid
/// [`to_uuid`]: Self::to_uuid
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampedEdge {
    /// The edge record. Identity is `edge.uuid`; the id fields are sender-local.
    pub edge: Edge,
    /// The `uuid` of the edge's source node.
    pub from_uuid: [u8; 16],
    /// The `uuid` of the edge's target node.
    pub to_uuid: [u8; 16],
    /// The causal stamp of this edge write.
    pub stamp: Stamp,
}

/// One change carried in a [`Delta`], keyed by `uuid`: an upsert (the full
/// record plus the causal [`Stamp`] that produced it) or a tombstone (the
/// deleted entity's `uuid` plus the stamp of its delete). The receiver
/// Last-Writer-Wins merges by comparing stamps and remaps ids by `uuid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StampedChange {
    /// A node upsert at this stamp. Identity is `node.uuid`.
    Node(Node, Stamp),
    /// An edge upsert at this stamp, with its endpoints carried by `uuid`.
    Edge(StampedEdge),
    /// A node deletion at this stamp — the tombstoned node's `uuid` and the
    /// causal stamp of the delete. Wins over any node upsert with a lesser stamp.
    DeleteNode([u8; 16], Stamp),
    /// An edge deletion at this stamp — the tombstoned edge's `uuid` and the
    /// causal stamp of the delete.
    DeleteEdge([u8; 16], Stamp),
}

impl StampedChange {
    /// The causal stamp of this change.
    #[must_use]
    pub fn stamp(&self) -> Stamp {
        match self {
            Self::Node(_, s) | Self::DeleteNode(_, s) | Self::DeleteEdge(_, s) => *s,
            Self::Edge(e) => e.stamp,
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
        let del_node = StampedChange::DeleteNode([7u8; 16], stamp(9, 0, 1));
        let del_edge = StampedChange::DeleteEdge([3u8; 16], stamp(9, 1, 1));
        assert_eq!(del_node.stamp(), stamp(9, 0, 1));
        assert_eq!(del_edge.stamp(), stamp(9, 1, 1));
        assert!(!del_node.is_node());
        assert!(!del_edge.is_node());
        // Phases: upsert node (0) < upsert edge (1) < delete edge (2) < delete node (3).
        assert!(del_edge.apply_phase() < del_node.apply_phase());
        let edge_upsert = StampedChange::Edge(StampedEdge {
            edge: edge_stub(),
            from_uuid: [1u8; 16],
            to_uuid: [2u8; 16],
            stamp: stamp(1, 0, 1),
        });
        assert_eq!(edge_upsert.stamp(), stamp(1, 0, 1));
        assert!(del_edge.apply_phase() > edge_upsert.apply_phase());
    }

    fn edge_stub() -> Edge {
        Edge {
            id: 3,
            uuid: [9u8; 16],
            from_id: 1,
            to_id: 2,
            kind: "links".into(),
            weight: 1.0,
            created_at: 0,
            properties: Properties::default(),
        }
    }
}
