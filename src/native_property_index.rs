//! An in-memory **property-value** index that tails a
//! [`NativeGraph`](crate::native::NativeGraph)'s change-feed
//! (RFC `docs/rfc-native-core.md`, #307, Phase 6.7).
//!
//! Maps every indexable `(property key, property value)` pair a node carries to
//! the ids that hold it, so a Cypher equality pattern such as
//! `MATCH (n {status: "open"})` resolves through an `O(matches)` lookup instead
//! of an `O(N)` full-node scan. Numeric values are additionally kept in ordered
//! maps so an inequality (`WHERE n.age > 30`) range-scans through `range_ids`
//! rather than scanning every node.
//! It is the native counterpart of the KV store's
//! [`property_index`](crate::property_index) (`Drevo::nodes_by_property`), kept —
//! like the trigram FTS and the secondary-label index — off the core graph seam
//! and current by **tailing the change-feed** (see
//! [`NativeGraph::changes_since`](crate::native::NativeGraph::changes_since)).
//!
//! # Which values are indexed
//!
//! Only **strings, booleans, and integers** are indexed, and only those are
//! looked up. This is deliberate: the executor turns a pattern value back into
//! JSON to probe the index, and that round-trip is byte-exact only for these
//! types. Restricting both the index and the probe to them guarantees the index
//! is a true **superset** of the matches (never a false negative) — floats,
//! lists, maps, and null fall back to the full scan, and the executor's exact
//! per-candidate check runs regardless. The reserved `_labels` property is
//! skipped (the [`NativeLabelIndex`](crate::native_label_index::NativeLabelIndex)
//! owns it).
//!
//! # Usage
//!
//! Snapshot-then-tail: build the index, then
//! [`sync`](crate::native_property_index::NativePropertyIndex::sync) after each
//! batch of writes; a feed trimmed past the cursor triggers a transparent
//! rebuild.
//!
//! ```
//! use drevo::native::NativeGraph;
//! use drevo::native_property_index::NativePropertyIndex;
//! use drevo::engine::GraphEngine; // brings `create_node` into scope
//! use drevo::model::{NewNode, Properties};
//! use std::collections::HashMap;
//!
//! # fn main() -> drevo::error::Result<()> {
//! let g = NativeGraph::new();
//! let props = Properties(HashMap::from([(
//!     "status".to_string(),
//!     serde_json::json!("open"),
//! )]));
//! let n = g.create_node(NewNode { kind: "task".into(), title: "t".into(),
//!     body: String::new(), body_html: String::new(), properties: props })?;
//!
//! let mut idx = NativePropertyIndex::new();
//! idx.sync(&g);
//! assert_eq!(idx.node_ids("status", &serde_json::json!("open")), vec![n.id]);
//! assert!(idx.node_ids("status", &serde_json::json!("closed")).is_empty());
//! # Ok(())
//! # }
//! ```

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;

use serde_json::Value as JsonValue;

use crate::engine::GraphEngine;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};
use crate::property_index::encode_value;

/// Reserved property key holding a node's secondary Cypher labels — owned by the
/// [`NativeLabelIndex`](crate::native_label_index::NativeLabelIndex), so this
/// index skips it.
const SECONDARY_LABELS_KEY: &str = "_labels";

/// Whether `value` is one of the types this index stores and can be probed for
/// with a byte-exact round-trip (string, bool, or an `i64` integer).
///
/// The executor uses this to decide which inline pattern properties may narrow
/// candidates through the index: a non-indexable value (float, list, map, null)
/// must be skipped rather than looked up, since its empty posting would wrongly
/// drop real matches instead of falling through to the exact per-candidate
/// check.
pub fn is_indexable(value: &JsonValue) -> bool {
    match value {
        JsonValue::String(_) | JsonValue::Bool(_) => true,
        JsonValue::Number(n) => n.is_i64(),
        _ => false,
    }
}

/// A total-order wrapper over `f64` so floats can key an ordered map. Ordering is
/// `f64::total_cmp` (which is a total order and puts `-0.0 < 0.0`); equality is
/// defined by that same order so `Ord`/`Eq` stay consistent. Only finite values
/// are ever stored.
#[derive(Clone, Copy)]
struct TotalF64(f64);

impl PartialEq for TotalF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}
impl Eq for TotalF64 {}
impl PartialOrd for TotalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TotalF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// A range (inequality) comparison operator for
/// [`NativePropertyIndex::range_ids`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOp {
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
}

/// A node's numeric value as it was indexed, so it can be removed later. Split
/// by JSON number kind because integers and floats live in separate ordered
/// maps (an integer is compared exactly; a float via [`TotalF64`]).
#[derive(Clone, Copy)]
enum NumEntry {
    Int(i64),
    Float(TotalF64),
}

/// A property-value index maintained by tailing a [`NativeGraph`]'s change-feed.
/// See the module docs.
#[derive(Default)]
pub struct NativePropertyIndex {
    /// property key → (canonical value bytes → ids carrying it, ascending).
    postings: HashMap<String, HashMap<Vec<u8>, BTreeSet<u64>>>,
    /// node id → the `(key, value-bytes)` pairs it was indexed under (the
    /// forward index, so a node can be removed or re-indexed cheaply).
    docs: HashMap<u64, Vec<(String, Vec<u8>)>>,
    /// `key → (integer value → ids)`, ordered, for range scans over integer
    /// property values.
    numeric_int: HashMap<String, BTreeMap<i64, BTreeSet<u64>>>,
    /// `key → (float value → ids)`, ordered, for range scans over floating-point
    /// property values.
    numeric_float: HashMap<String, BTreeMap<TotalF64, BTreeSet<u64>>>,
    /// node id → the numeric `(key, value)` entries it was indexed under, so a
    /// node's contributions to the ordered maps can be removed.
    numeric_docs: HashMap<u64, Vec<(String, NumEntry)>>,
    /// `key → count of nodes holding a **non-numeric, non-null** value` there
    /// (string / bool / list / map). A range scan is refused for any such key:
    /// in drevo, ordering a number against one of those raises a type error, and
    /// serving the range from the numeric maps would silently *exclude* the
    /// offending node instead — turning an error into a wrong success. Only a
    /// purely numeric (plus optionally null) key is range-served.
    range_blocked: HashMap<String, u64>,
    /// node id → the keys it contributes a range-blocking value to, so the
    /// [`range_blocked`](Self::range_blocked) counts can be decremented on remove.
    blocked_docs: HashMap<u64, Vec<String>>,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

/// Whether `value` blocks range scans on its key — a non-numeric, non-null value
/// (string / bool / list / map) that an ordering comparison against a number
/// would type-error on in drevo. `null` does not block: `null <op> n` is `null`
/// (the row filters out) rather than an error, so excluding it never diverges.
fn is_range_blocking(value: &JsonValue) -> bool {
    matches!(
        value,
        JsonValue::String(_) | JsonValue::Bool(_) | JsonValue::Array(_) | JsonValue::Object(_)
    )
}

impl NativePropertyIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this index has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of nodes that carry at least one indexable property.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether no node carries an indexable property.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Node ids whose `key` property equals `value`, ascending. Empty when none
    /// match or when `value` is not an indexable type (the caller then falls
    /// back to a full scan, so this can only ever be a superset gap the exact
    /// per-candidate check would close anyway).
    pub fn node_ids(&self, key: &str, value: &JsonValue) -> Vec<u64> {
        if !is_indexable(value) {
            return Vec::new();
        }
        let Ok(bytes) = encode_value(value) else {
            return Vec::new();
        };
        self.postings
            .get(key)
            .and_then(|by_val| by_val.get(&bytes))
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Node ids whose `key` property satisfies `value OP bound` for a **numeric**
    /// `bound`, as a *superset* of the real matches (the caller's exact check
    /// trims false positives). Returns `None` when `bound` is not a number — a
    /// numeric range never matches a non-numeric property (Cypher compares them
    /// as `null`), and a string/other bound is simply not served here, so the
    /// caller falls back to a full scan.
    ///
    /// Both the integer and float ordered maps are scanned and unioned, so every
    /// numeric value of `key` is covered regardless of how it was stored. Integer
    /// bounds probe the integer map exactly and widen the float map by one; float
    /// bounds probe the float map exactly and widen the integer map to the
    /// enclosing integers. All widening is outward, so the result can only ever
    /// be a superset.
    pub fn range_ids(&self, key: &str, op: RangeOp, bound: &JsonValue) -> Option<BTreeSet<u64>> {
        let (int_bound, float_bound): (Option<i64>, f64) = match bound {
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    (Some(i), i as f64)
                } else {
                    let f = n.as_f64()?;
                    if !f.is_finite() {
                        return None;
                    }
                    (None, f)
                }
            }
            _ => return None,
        };

        // A key that also holds a non-numeric value cannot be range-served
        // without diverging from drevo's type-error semantics (see the field
        // docs) — leave the whole predicate to the exact filter.
        if self.range_blocked.get(key).copied().unwrap_or(0) > 0 {
            return None;
        }

        let mut out = BTreeSet::new();
        if let Some(map) = self.numeric_int.get(key) {
            for (_, ids) in map.range(int_map_bounds(int_bound, float_bound, op)) {
                out.extend(ids.iter().copied());
            }
        }
        if let Some(map) = self.numeric_float.get(key) {
            for (_, ids) in map.range(float_map_bounds(int_bound, float_bound, op)) {
                out.extend(ids.iter().copied());
            }
        }
        Some(out)
    }

    /// Bring the index up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor). A feed trimmed past the cursor
    /// (a `lagged` batch) triggers a rebuild from a fresh snapshot.
    pub fn sync(&mut self, graph: &NativeGraph) {
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => self.remove_node(id),
                // Edge properties are not indexed here.
                WalOp::UpsertEdge(_) | WalOp::DeleteEdge(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }

    // ----- maintenance -------------------------------------------------------

    /// Discard everything and re-index every node in `graph`.
    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.postings.clear();
        self.docs.clear();
        self.numeric_int.clear();
        self.numeric_float.clear();
        self.numeric_docs.clear();
        self.range_blocked.clear();
        self.blocked_docs.clear();
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// Insert or replace a node's property postings — exact (string / bool /
    /// integer) and ordered numeric (integer / float) — so it can be found by an
    /// equality lookup or a range scan. A node with no such property is untracked.
    fn index_node(&mut self, node: &Node) {
        self.remove_node(node.id);
        let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
        let mut nums: Vec<(String, NumEntry)> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        for (key, value) in node.properties.0.iter() {
            if key == SECONDARY_LABELS_KEY {
                continue;
            }
            // A non-numeric value blocks range scans on this key.
            if is_range_blocking(value) {
                *self.range_blocked.entry(key.clone()).or_default() += 1;
                blocked.push(key.clone());
            }
            // Exact posting for equality lookups (string / bool / i64).
            if is_indexable(value) {
                if let Ok(bytes) = encode_value(value) {
                    self.postings
                        .entry(key.clone())
                        .or_default()
                        .entry(bytes.clone())
                        .or_default()
                        .insert(node.id);
                    pairs.push((key.clone(), bytes));
                }
            }
            // Ordered numeric entry for range scans. Every numeric value is
            // indexed (integer exactly, float via TotalF64) so a range query
            // covers the whole key regardless of how each node stored it.
            if let JsonValue::Number(n) = value {
                if let Some(i) = n.as_i64() {
                    self.numeric_int
                        .entry(key.clone())
                        .or_default()
                        .entry(i)
                        .or_default()
                        .insert(node.id);
                    nums.push((key.clone(), NumEntry::Int(i)));
                } else if let Some(f) = n.as_f64() {
                    if f.is_finite() {
                        let f = TotalF64(f);
                        self.numeric_float
                            .entry(key.clone())
                            .or_default()
                            .entry(f)
                            .or_default()
                            .insert(node.id);
                        nums.push((key.clone(), NumEntry::Float(f)));
                    }
                }
            }
        }
        if !pairs.is_empty() {
            self.docs.insert(node.id, pairs);
        }
        if !nums.is_empty() {
            self.numeric_docs.insert(node.id, nums);
        }
        if !blocked.is_empty() {
            self.blocked_docs.insert(node.id, blocked);
        }
    }

    /// Remove a node's postings (exact and numeric), dropping any bucket / map
    /// entry that empties.
    fn remove_node(&mut self, id: u64) {
        if let Some(pairs) = self.docs.remove(&id) {
            for (key, bytes) in &pairs {
                if let Some(by_val) = self.postings.get_mut(key) {
                    if let Some(ids) = by_val.get_mut(bytes) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            by_val.remove(bytes);
                        }
                    }
                    if by_val.is_empty() {
                        self.postings.remove(key);
                    }
                }
            }
        }
        if let Some(nums) = self.numeric_docs.remove(&id) {
            for (key, entry) in &nums {
                match entry {
                    NumEntry::Int(i) => remove_from_ordered(&mut self.numeric_int, key, i, id),
                    NumEntry::Float(f) => remove_from_ordered(&mut self.numeric_float, key, f, id),
                }
            }
        }
        if let Some(keys) = self.blocked_docs.remove(&id) {
            for key in &keys {
                if let Some(count) = self.range_blocked.get_mut(key) {
                    *count -= 1;
                    if *count == 0 {
                        self.range_blocked.remove(key);
                    }
                }
            }
        }
    }
}

/// Remove `id` from `maps[key][value]`, dropping the value entry and then the
/// key's map when they empty.
fn remove_from_ordered<K: Ord>(
    maps: &mut HashMap<String, BTreeMap<K, BTreeSet<u64>>>,
    key: &str,
    value: &K,
    id: u64,
) {
    if let Some(map) = maps.get_mut(key) {
        if let Some(ids) = map.get_mut(value) {
            ids.remove(&id);
            if ids.is_empty() {
                map.remove(value);
            }
        }
        if map.is_empty() {
            maps.remove(key);
        }
    }
}

/// Bounds selecting the integer map keys that satisfy `OP bound`. An integer
/// bound is exact; a float bound widens outward to the enclosing integers
/// (`floor` for a lower bound, `ceil` for an upper) so the result is a superset.
fn int_map_bounds(
    int_bound: Option<i64>,
    float_bound: f64,
    op: RangeOp,
) -> (Bound<i64>, Bound<i64>) {
    match int_bound {
        Some(i) => match op {
            RangeOp::Gt => (Bound::Excluded(i), Bound::Unbounded),
            RangeOp::Ge => (Bound::Included(i), Bound::Unbounded),
            RangeOp::Lt => (Bound::Unbounded, Bound::Excluded(i)),
            RangeOp::Le => (Bound::Unbounded, Bound::Included(i)),
        },
        None => match op {
            // `i > f` / `i >= f`: any integer ≥ floor(f) is a safe superset.
            RangeOp::Gt | RangeOp::Ge => (
                Bound::Included(float_bound.floor() as i64),
                Bound::Unbounded,
            ),
            // `i < f` / `i <= f`: any integer ≤ ceil(f) is a safe superset.
            RangeOp::Lt | RangeOp::Le => {
                (Bound::Unbounded, Bound::Included(float_bound.ceil() as i64))
            }
        },
    }
}

/// Bounds selecting the float map keys that satisfy `OP bound`. A float bound is
/// exact; an integer bound widens outward by one whole unit (`i-1` for a lower
/// bound, `i+1` for an upper) so lossy `i as f64` rounding can never drop a match.
fn float_map_bounds(
    int_bound: Option<i64>,
    float_bound: f64,
    op: RangeOp,
) -> (Bound<TotalF64>, Bound<TotalF64>) {
    match int_bound {
        Some(i) => match op {
            RangeOp::Gt | RangeOp::Ge => (
                Bound::Included(TotalF64(i.saturating_sub(1) as f64)),
                Bound::Unbounded,
            ),
            RangeOp::Lt | RangeOp::Le => (
                Bound::Unbounded,
                Bound::Included(TotalF64(i.saturating_add(1) as f64)),
            ),
        },
        None => {
            let b = TotalF64(float_bound);
            match op {
                RangeOp::Gt => (Bound::Excluded(b), Bound::Unbounded),
                RangeOp::Ge => (Bound::Included(b), Bound::Unbounded),
                RangeOp::Lt => (Bound::Unbounded, Bound::Excluded(b)),
                RangeOp::Le => (Bound::Unbounded, Bound::Included(b)),
            }
        }
    }
}
