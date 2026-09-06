//! Last-Writer-Wins register + map CRDTs for multi-writer convergence
//! (issue #389, primitive #2).
//!
//! Built on the [Hybrid Logical Clock](crate::hlc): every write carries a
//! [`Stamp`](crate::lww::Stamp) `(hlc, origin)`, and [`merge`](crate::lww::LwwRegister::merge)
//! keeps the value with the greatest stamp. Because stamps are **totally
//! ordered** (HLC first, then a unique per-replica origin id to break ties) and
//! each write's stamp is unique, the merge is commutative, associative, and
//! idempotent — two peers that see the same set of writes in any order converge
//! to the same value. This is the minimal viable convergence for offline-first
//! shared records: a [`LwwMap`](crate::lww::LwwMap) of a node's properties is a
//! register-LWW CRDT over the graph.
//!
//! Deletes are deliberately **not** modelled here: a key that has never been
//! written is simply absent, and a per-key remove that converges against a
//! concurrent edit needs a *causal tombstone*, which is primitive #3 (a
//! follow-up slice). Dependency-free, infallible, always compiled, WASM-safe.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hlc::Hlc;

/// A replica / origin identifier: which peer produced a write. Paired with an
/// [`Hlc`] it forms a [`Stamp`], giving a total order over writes even when two
/// peers stamp concurrent edits with the same HLC. A replica picks one id
/// (e.g. a random `u64`) for its lifetime.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct OriginId(pub u64);

/// A total, deterministic version stamp for a single write: the [`Hlc`] first,
/// the [`OriginId`] as the tie-breaker. Ordering is lexicographic via the
/// derived [`Ord`] (fields declared HLC-then-origin), so a later causal time
/// always wins and two truly concurrent writes are ordered by their origin.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Stamp {
    /// Causal timestamp of the write.
    hlc: Hlc,
    /// Replica that produced the write (tie-breaker for equal HLCs).
    origin: OriginId,
}

impl Stamp {
    /// Construct a stamp from its causal timestamp and origin.
    #[must_use]
    pub fn new(hlc: Hlc, origin: OriginId) -> Self {
        Self { hlc, origin }
    }

    /// The causal timestamp component.
    #[must_use]
    pub fn hlc(&self) -> Hlc {
        self.hlc
    }

    /// The origin component.
    #[must_use]
    pub fn origin(&self) -> OriginId {
        self.origin
    }
}

/// A Last-Writer-Wins register: a value tagged with the [`Stamp`] of the write
/// that produced it. [`merge`](Self::merge) keeps the value with the greater
/// stamp, so concurrent replicas converge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwRegister<T> {
    value: T,
    stamp: Stamp,
}

impl<T: Clone> LwwRegister<T> {
    /// A register holding `value`, written at `stamp`.
    pub fn new(value: T, stamp: Stamp) -> Self {
        Self { value, stamp }
    }

    /// The current value.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// The stamp of the write that produced the current value.
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    /// Apply a local write: adopt `value`/`stamp` when `stamp` is greater than
    /// the current one (a well-formed local write always uses a fresh, greater
    /// stamp), otherwise keep the current value. Returns whether the register
    /// changed.
    pub fn set(&mut self, value: T, stamp: Stamp) -> bool {
        if stamp > self.stamp {
            self.value = value;
            self.stamp = stamp;
            true
        } else {
            false
        }
    }

    /// Merge `other` in, keeping the value with the greater stamp. Commutative,
    /// associative, and idempotent. Returns whether `self` changed.
    pub fn merge(&mut self, other: &Self) -> bool {
        if other.stamp > self.stamp {
            self.value = other.value.clone();
            self.stamp = other.stamp;
            true
        } else {
            false
        }
    }
}

/// A Last-Writer-Wins map: per-key [`LwwRegister`]s that converge under
/// [`merge`](Self::merge). Keys present on either side survive a merge; a key's
/// value is resolved by its register's LWW rule. (Removal that converges needs
/// tombstones — issue #389 primitive #3 — and is intentionally out of scope
/// here.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwMap<K: Ord, V> {
    entries: BTreeMap<K, LwwRegister<V>>,
}

impl<K: Ord + Clone, V: Clone> Default for LwwMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone, V: Clone> LwwMap<K, V> {
    /// An empty map.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Apply a local write of `value` at `key`, stamped `stamp`. A newer stamp
    /// than the key's current one wins (or the key is created). Returns whether
    /// the map changed.
    pub fn set(&mut self, key: K, value: V, stamp: Stamp) -> bool {
        match self.entries.get_mut(&key) {
            Some(reg) => reg.set(value, stamp),
            None => {
                self.entries.insert(key, LwwRegister::new(value, stamp));
                true
            }
        }
    }

    /// The current value at `key`, if the key is present.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(LwwRegister::value)
    }

    /// The register at `key` (value + stamp), if present.
    pub fn register(&self, key: &K) -> Option<&LwwRegister<V>> {
        self.entries.get(key)
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map has no keys.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate `(key, value)` pairs in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(k, reg)| (k, reg.value()))
    }

    /// Merge `other` in: union of keys, per-key LWW. Commutative, associative,
    /// idempotent — replicas converge regardless of merge order. Returns
    /// whether `self` changed.
    pub fn merge(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (key, reg) in &other.entries {
            match self.entries.get_mut(key) {
                Some(local) => changed |= local.merge(reg),
                None => {
                    self.entries.insert(key.clone(), reg.clone());
                    changed = true;
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(wall: i64, counter: u32, origin: u64) -> Stamp {
        Stamp::new(Hlc::new(wall, counter), OriginId(origin))
    }

    #[test]
    fn stamp_orders_by_hlc_then_origin() {
        // Later HLC wins regardless of origin.
        assert!(stamp(5, 0, 999) < stamp(6, 0, 0));
        // Equal HLC: origin breaks the tie.
        assert!(stamp(5, 0, 1) < stamp(5, 0, 2));
    }

    #[test]
    fn register_set_keeps_the_greater_stamp() {
        let mut r = LwwRegister::new("a", stamp(1, 0, 1));
        assert!(r.set("b", stamp(2, 0, 1)));
        assert_eq!(r.value(), &"b");
        // A stale write is ignored.
        assert!(!r.set("c", stamp(1, 5, 1)));
        assert_eq!(r.value(), &"b");
    }

    #[test]
    fn register_merge_is_commutative_and_idempotent() {
        let a = LwwRegister::new("a", stamp(1, 0, 1));
        let b = LwwRegister::new("b", stamp(2, 0, 1));

        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab, ba, "merge must be commutative");
        assert_eq!(ab.value(), &"b");

        // Idempotent: merging the same thing again changes nothing.
        assert!(!ab.merge(&b));
        assert_eq!(ab.value(), &"b");
    }

    #[test]
    fn register_concurrent_writes_resolve_by_origin() {
        // Two peers write the SAME HLC (truly concurrent) with different
        // origins — the higher origin wins, deterministically, either order.
        let p1 = LwwRegister::new("from-1", stamp(10, 0, 1));
        let p2 = LwwRegister::new("from-2", stamp(10, 0, 2));
        let mut a = p1.clone();
        a.merge(&p2);
        let mut b = p2.clone();
        b.merge(&p1);
        assert_eq!(a, b);
        assert_eq!(a.value(), &"from-2");
    }

    #[test]
    fn map_set_and_get() {
        let mut m: LwwMap<String, i32> = LwwMap::new();
        assert!(m.is_empty());
        assert!(m.set("x".into(), 1, stamp(1, 0, 1)));
        assert!(m.set("y".into(), 2, stamp(1, 0, 1)));
        assert_eq!(m.get(&"x".to_string()), Some(&1));
        assert_eq!(m.get(&"y".to_string()), Some(&2));
        assert_eq!(m.get(&"z".to_string()), None);
        assert_eq!(m.len(), 2);
        // A newer write to an existing key wins; a stale one is dropped.
        assert!(m.set("x".into(), 9, stamp(2, 0, 1)));
        assert_eq!(m.get(&"x".to_string()), Some(&9));
        assert!(!m.set("x".into(), 8, stamp(1, 9, 1)));
        assert_eq!(m.get(&"x".to_string()), Some(&9));
    }

    #[test]
    fn map_merge_unions_keys_and_converges_regardless_of_order() {
        // Peer A: sets shared=A1 (older) and a1-only.
        let mut a: LwwMap<String, &str> = LwwMap::new();
        a.set("shared".into(), "A", stamp(1, 0, 1));
        a.set("only-a".into(), "a", stamp(1, 0, 1));

        // Peer B: sets shared=B2 (newer, wins) and b1-only.
        let mut b: LwwMap<String, &str> = LwwMap::new();
        b.set("shared".into(), "B", stamp(2, 0, 2));
        b.set("only-b".into(), "b", stamp(1, 0, 2));

        // Merge in both directions; the results must be identical (convergence).
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab, ba, "LWW-map merge must converge regardless of order");

        // Union of keys; the newer stamp wins the conflicting key.
        assert_eq!(ab.get(&"shared".to_string()), Some(&"B"));
        assert_eq!(ab.get(&"only-a".to_string()), Some(&"a"));
        assert_eq!(ab.get(&"only-b".to_string()), Some(&"b"));
        assert_eq!(ab.len(), 3);
    }

    #[test]
    fn map_merge_is_idempotent() {
        let mut a: LwwMap<String, i32> = LwwMap::new();
        a.set("k".into(), 1, stamp(1, 0, 1));
        let mut b: LwwMap<String, i32> = LwwMap::new();
        b.set("k".into(), 2, stamp(2, 0, 1));
        a.merge(&b);
        // A second identical merge is a no-op.
        assert!(!a.merge(&b));
        assert_eq!(a.get(&"k".to_string()), Some(&2));
    }

    #[test]
    fn map_merge_is_associative() {
        let mk = |v: &'static str, s: Stamp| {
            let mut m: LwwMap<String, &str> = LwwMap::new();
            m.set("k".into(), v, s);
            m
        };
        let a = mk("a", stamp(1, 0, 1));
        let b = mk("b", stamp(2, 0, 1));
        let c = mk("c", stamp(3, 0, 1));

        // (a ∪ b) ∪ c
        let mut left = a.clone();
        left.merge(&b);
        left.merge(&c);
        // a ∪ (b ∪ c)
        let mut bc = b.clone();
        bc.merge(&c);
        let mut right = a.clone();
        right.merge(&bc);

        assert_eq!(left, right, "merge must be associative");
        assert_eq!(left.get(&"k".to_string()), Some(&"c"));
    }

    #[test]
    fn serde_round_trips() {
        let mut m: LwwMap<String, i32> = LwwMap::new();
        m.set("k".into(), 7, stamp(123, 4, 5));
        let json = serde_json::to_string(&m).unwrap();
        let back: LwwMap<String, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
