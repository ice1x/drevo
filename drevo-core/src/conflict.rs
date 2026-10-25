//! What a transaction touched, and whether two transactions could have
//! committed side by side.
//!
//! A [`ConflictSet`] is derived from a committed (or about-to-commit) batch of
//! [`WalOp`]s: the records it wrote, the records it deleted, and the nodes a
//! written relationship depends on for its endpoints. Two sets
//! [`intersect`](ConflictSet::intersects) when the transactions behind them
//! could not both have committed on top of each other:
//!
//! - both wrote the same node or relationship;
//! - one attached or changed a relationship whose endpoint the other deleted
//!   (checked in both directions, so the answer does not depend on which of the
//!   two committed first).
//!
//! A [`CommitHistory`] keeps the sets of the most recent commits, each tagged
//! with the commit sequence it produced, so a transaction that began at
//! sequence `b` can ask whether anything committed after `b` intersects with
//! what it wants to commit. Under
//! [`Isolation::Snapshot`](crate::native::Isolation::Snapshot) the engine
//! commits a transaction when the answer is [`ConflictReason::WouldPass`]
//! and rejects it on [`ConflictReason::Overlap`] or, when the transaction
//! outlived the history, [`ConflictReason::History`]. Under
//! [`Isolation::Serializable`](crate::native::Isolation::Serializable) every
//! one of them is rejected, and the reason is only counted.
//!
//! ```
//! use drevo_core::conflict::{ConflictSet, Key};
//! use drevo_core::native::WalOp;
//!
//! let a = ConflictSet::from_ops(&[WalOp::DeleteNode(7)]);
//! let b = ConflictSet::from_ops(&[WalOp::DeleteEdge(3)]);
//! assert!(!a.intersects(&b));
//! assert!(a.writes().contains(&Key::Node(7)));
//! ```

use std::collections::{HashSet, VecDeque};

use crate::native::WalOp;

/// A record a transaction touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A node, by id.
    Node(u64),
    /// A relationship, by id.
    Edge(u64),
}

/// The records a batch of operations wrote, deleted and depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictSet {
    writes: HashSet<Key>,
    deletes: HashSet<Key>,
    deps: HashSet<Key>,
    everything: bool,
}

impl ConflictSet {
    /// The set of a batch of operations, per the table in the module docs:
    /// an upsert writes its record; a delete writes and deletes it; a
    /// relationship upsert depends on both endpoint nodes; an embedding
    /// change writes its node.
    pub fn from_ops(ops: &[WalOp]) -> Self {
        let mut set = Self::default();
        for op in ops {
            match op {
                WalOp::UpsertNode(node) => {
                    set.writes.insert(Key::Node(node.id));
                }
                WalOp::DeleteNode(id) => {
                    set.writes.insert(Key::Node(*id));
                    set.deletes.insert(Key::Node(*id));
                }
                WalOp::UpsertEdge(edge) => {
                    set.writes.insert(Key::Edge(edge.id));
                    set.deps.insert(Key::Node(edge.from_id));
                    set.deps.insert(Key::Node(edge.to_id));
                }
                WalOp::DeleteEdge(id) => {
                    set.writes.insert(Key::Edge(*id));
                    set.deletes.insert(Key::Edge(*id));
                }
                WalOp::SetEmbedding(id, _) | WalOp::DeleteEmbedding(id) => {
                    set.writes.insert(Key::Node(*id));
                }
                // Two-phase-commit markers carry no graph change of their own;
                // the engine passes the expanded write set separately.
                WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
            }
        }
        set
    }

    /// The set of one node written (created, updated, its embedding changed).
    pub fn node(id: u64) -> Self {
        Self::from_ops(&[WalOp::DeleteEmbedding(id)])
    }

    /// The set of one node deleted.
    pub fn deleted_node(id: u64) -> Self {
        Self::from_ops(&[WalOp::DeleteNode(id)])
    }

    /// The set of one relationship written, with the endpoints it depends on.
    pub fn edge(id: u64, from: u64, to: u64) -> Self {
        let mut set = Self::default();
        set.writes.insert(Key::Edge(id));
        set.deps.insert(Key::Node(from));
        set.deps.insert(Key::Node(to));
        set
    }

    /// The set of one relationship deleted.
    pub fn deleted_edge(id: u64) -> Self {
        Self::from_ops(&[WalOp::DeleteEdge(id)])
    }

    /// Add everything `other` touched.
    pub fn extend(&mut self, other: Self) {
        self.writes.extend(other.writes);
        self.deletes.extend(other.deletes);
        self.deps.extend(other.deps);
        self.everything |= other.everything;
    }

    /// A change that touches the whole graph — a schema change such as a new
    /// constraint — and so conflicts with every other transaction.
    pub fn everything() -> Self {
        Self {
            everything: true,
            ..Self::default()
        }
    }

    /// `true` when nothing was touched.
    pub fn is_empty(&self) -> bool {
        !self.everything && self.writes.is_empty()
    }

    /// The records written (created, updated or deleted).
    pub fn writes(&self) -> &HashSet<Key> {
        &self.writes
    }

    /// The records deleted.
    pub fn deletes(&self) -> &HashSet<Key> {
        &self.deletes
    }

    /// The nodes that written relationships need as endpoints.
    pub fn depends_on(&self) -> &HashSet<Key> {
        &self.deps
    }

    /// Whether the two transactions could not both have committed: a shared
    /// write, or a relationship endpoint one of them deleted while the other
    /// wrote the relationship. Symmetric.
    pub fn intersects(&self, other: &Self) -> bool {
        if self.everything {
            return !other.is_empty();
        }
        if other.everything {
            return !self.is_empty();
        }
        self.writes.iter().any(|k| other.writes.contains(k))
            || self.deps.iter().any(|k| other.deletes.contains(k))
            || self.deletes.iter().any(|k| other.deps.contains(k))
    }
}

/// Why a commit was (or would have been) rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// A commit since the transaction began touched what it touched.
    Overlap,
    /// Every commit since the transaction began touched something else: a
    /// per-entity rule would have accepted it.
    WouldPass,
    /// The transaction began before the oldest commit the history still
    /// holds, so the question cannot be answered.
    History,
}

/// Counts of rejected commits by [`ConflictReason`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConflictStats {
    /// Rejections a per-entity rule would also make.
    pub overlap: u64,
    /// Rejections a per-entity rule would not make.
    pub would_pass: u64,
    /// Rejections that could not be classified: the transaction outlived the
    /// history.
    pub history: u64,
    /// Commits that went through although others committed since they began,
    /// because none of those touched the same records
    /// ([`Isolation::Snapshot`](crate::native::Isolation::Snapshot)).
    pub merged: u64,
}

/// The conflict sets of the most recent commits, newest last, each tagged
/// with the commit sequence it produced. Sequences are consecutive.
#[derive(Debug)]
pub struct CommitHistory {
    recent: VecDeque<(u64, ConflictSet)>,
    capacity: usize,
}

/// How many commits a [`CommitHistory`] keeps unless told otherwise.
pub const DEFAULT_HISTORY: usize = 10_000;

impl Default for CommitHistory {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_HISTORY)
    }
}

impl CommitHistory {
    /// A history that forgets commits beyond the newest `capacity`.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            recent: VecDeque::new(),
            capacity,
        }
    }

    /// Forget commits beyond the newest `capacity` from now on.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.trim();
    }

    /// How many commits are kept.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many commits are held right now.
    pub fn len(&self) -> usize {
        self.recent.len()
    }

    /// `true` when no commit is held.
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty()
    }

    /// Remember that commit `seq` touched `set`.
    pub fn push(&mut self, seq: u64, set: ConflictSet) {
        self.recent.push_back((seq, set));
        self.trim();
    }

    fn trim(&mut self) {
        while self.recent.len() > self.capacity {
            self.recent.pop_front();
        }
    }

    /// Classify a commit of `mine` by a transaction that began at commit
    /// sequence `base` and found the graph changed. Every commit after `base`
    /// must still be held, otherwise the answer is
    /// [`ConflictReason::History`].
    pub fn classify(&self, base: u64, mine: &ConflictSet) -> ConflictReason {
        let covers = match self.recent.front() {
            Some((oldest, _)) => *oldest <= base + 1,
            None => false,
        };
        if !covers {
            return ConflictReason::History;
        }
        let overlap = self
            .recent
            .iter()
            .rev()
            .take_while(|(seq, _)| *seq > base)
            .any(|(_, theirs)| theirs.intersects(mine));
        if overlap {
            ConflictReason::Overlap
        } else {
            ConflictReason::WouldPass
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node, Properties};

    fn node(id: u64) -> WalOp {
        WalOp::UpsertNode(Node {
            id,
            uuid: [0; 16],
            kind: "note".into(),
            title: format!("n{id}"),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
            created_at: 0,
            updated_at: 0,
        })
    }

    fn edge(id: u64, from: u64, to: u64) -> WalOp {
        WalOp::UpsertEdge(Edge {
            id,
            uuid: [0; 16],
            from_id: from,
            to_id: to,
            kind: "links".into(),
            weight: 1.0,
            created_at: 0,
            properties: Properties::default(),
        })
    }

    #[test]
    fn disjoint_writers_do_not_intersect() {
        let a = ConflictSet::from_ops(&[node(1), node(2)]);
        let b = ConflictSet::from_ops(&[node(3), node(4)]);
        assert!(!a.intersects(&b));
        assert!(!b.intersects(&a));
    }

    #[test]
    fn writing_the_same_node_intersects() {
        let a = ConflictSet::from_ops(&[node(1)]);
        let b = ConflictSet::from_ops(&[node(1)]);
        assert!(a.intersects(&b));
    }

    #[test]
    fn an_embedding_change_writes_its_node() {
        let a = ConflictSet::from_ops(&[WalOp::SetEmbedding(5, vec![1.0])]);
        let b = ConflictSet::from_ops(&[WalOp::DeleteNode(5)]);
        assert!(a.intersects(&b));
        assert!(ConflictSet::from_ops(&[WalOp::DeleteEmbedding(5)]).intersects(&b));
    }

    #[test]
    fn a_relationship_depends_on_its_endpoints_against_deletes_only() {
        let attach = ConflictSet::from_ops(&[edge(9, 1, 2)]);
        let delete_endpoint = ConflictSet::from_ops(&[WalOp::DeleteNode(2)]);
        let update_endpoint = ConflictSet::from_ops(&[node(2)]);
        assert!(attach.intersects(&delete_endpoint));
        assert!(delete_endpoint.intersects(&attach), "symmetric");
        assert!(
            !attach.intersects(&update_endpoint),
            "an update of the endpoint is fine"
        );
    }

    #[test]
    fn deleting_a_relationship_is_a_write_of_it() {
        let a = ConflictSet::from_ops(&[WalOp::DeleteEdge(9)]);
        let b = ConflictSet::from_ops(&[edge(9, 1, 2)]);
        assert!(a.intersects(&b));
        assert!(a.deletes().contains(&Key::Edge(9)));
    }

    #[test]
    fn the_shorthand_constructors_match_from_ops() {
        assert_eq!(ConflictSet::node(4), ConflictSet::from_ops(&[node(4)]));
        assert_eq!(
            ConflictSet::deleted_node(4),
            ConflictSet::from_ops(&[WalOp::DeleteNode(4)])
        );
        assert_eq!(
            ConflictSet::edge(9, 1, 2),
            ConflictSet::from_ops(&[edge(9, 1, 2)])
        );
        assert_eq!(
            ConflictSet::deleted_edge(9),
            ConflictSet::from_ops(&[WalOp::DeleteEdge(9)])
        );
        let mut both = ConflictSet::node(1);
        both.extend(ConflictSet::edge(9, 1, 2));
        assert_eq!(both, ConflictSet::from_ops(&[node(1), edge(9, 1, 2)]));
    }

    #[test]
    fn two_phase_markers_touch_nothing() {
        let set = ConflictSet::from_ops(&[WalOp::RollbackPrepared {
            gid: "g".into(),
            heuristic: false,
        }]);
        assert!(set.is_empty());
    }

    #[test]
    fn everything_intersects_with_any_change_but_not_with_nothing() {
        let all = ConflictSet::everything();
        assert!(all.intersects(&ConflictSet::from_ops(&[node(1)])));
        assert!(ConflictSet::from_ops(&[node(1)]).intersects(&all));
        assert!(!all.intersects(&ConflictSet::default()));
        assert!(all.intersects(&ConflictSet::everything()));
        assert!(!all.is_empty());
    }

    #[test]
    fn history_classifies_against_commits_after_base_only() {
        let mut h = CommitHistory::with_capacity(10);
        h.push(1, ConflictSet::from_ops(&[node(1)]));
        h.push(2, ConflictSet::from_ops(&[node(2)]));
        h.push(3, ConflictSet::from_ops(&[node(3)]));
        let mine = ConflictSet::from_ops(&[node(1)]);
        // Began after commit 1: the write of node 1 is already in its snapshot.
        assert_eq!(h.classify(1, &mine), ConflictReason::WouldPass);
        // Began before commit 1: that write overlaps.
        assert_eq!(h.classify(0, &mine), ConflictReason::Overlap);
        assert_eq!(
            h.classify(2, &ConflictSet::from_ops(&[node(3)])),
            ConflictReason::Overlap
        );
    }

    #[test]
    fn a_transaction_older_than_the_history_cannot_be_classified() {
        let mut h = CommitHistory::with_capacity(2);
        for seq in 1..=4 {
            h.push(seq, ConflictSet::from_ops(&[node(seq)]));
        }
        assert_eq!(h.len(), 2, "only the two newest commits are kept");
        let mine = ConflictSet::from_ops(&[node(42)]);
        assert_eq!(h.classify(1, &mine), ConflictReason::History);
        assert_eq!(h.classify(2, &mine), ConflictReason::WouldPass);
        assert_eq!(
            CommitHistory::with_capacity(5).classify(0, &mine),
            ConflictReason::History
        );
    }

    #[test]
    fn shrinking_the_capacity_forgets_the_oldest() {
        let mut h = CommitHistory::default();
        assert_eq!(h.capacity(), DEFAULT_HISTORY);
        for seq in 1..=5 {
            h.push(seq, ConflictSet::default());
        }
        h.set_capacity(2);
        assert_eq!(h.len(), 2);
        assert_eq!(
            h.classify(3, &ConflictSet::default()),
            ConflictReason::WouldPass
        );
        assert_eq!(
            h.classify(2, &ConflictSet::default()),
            ConflictReason::History
        );
    }
}
