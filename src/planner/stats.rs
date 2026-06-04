//! Graph statistics — the inputs a cost-based planner needs to estimate
//! how many rows a query will touch.
//!
//! [`GraphStatistics`] is an immutable snapshot of the catalogue figures the
//! [`crate::planner::CardinalityEstimator`] reads: total node / relationship
//! counts, per-label and per-type breakdowns, and per-`(label, property)`
//! distinct-value counts (the basis for equality selectivity). It is built
//! either directly through the chainable setters or — the common path — by
//! feeding a [`StatisticsCollector`] one observation per entity while scanning
//! a graph, then calling [`StatisticsCollector::finish`].
//!
//! The module is deliberately source-agnostic: nothing here reaches into
//! [`crate::db::Drevo`]. Wiring a live scan into the collector is left to the
//! task that makes the executor consume the planner (`00086`), exactly as the
//! MVCC engine stayed standalone until it was wired in.

use std::collections::{HashMap, HashSet};

/// Multiple of the average degree at or above which a node is treated as a
/// **supernode** when no explicit threshold is configured (task `00087`).
///
/// On real graphs the degree distribution is heavily skewed: a handful of
/// "hub" nodes (a popular tag, a prolific author, a shared dependency) carry
/// orders of magnitude more edges than the average. Driving a traversal *out
/// of* such a node fans out across its entire degree, so the planner needs to
/// recognise one. A node whose degree is this many times the graph average is
/// considered a supernode.
pub const DEFAULT_SUPERNODE_THRESHOLD_FACTOR: f64 = 100.0;

/// Floor for the derived supernode threshold, so that on small or sparse graphs
/// — where the average degree is tiny — an ordinary, well-connected node is not
/// mistaken for a supernode (task `00087`).
pub const MIN_SUPERNODE_THRESHOLD: u64 = 100;

/// Immutable snapshot of the graph catalogue figures used to estimate query
/// cardinality.
///
/// All counts are best-effort estimates, not transactionally exact — a planner
/// only needs them to choose between plans, so a slightly stale snapshot is
/// acceptable. Construct one with [`GraphStatistics::new`] plus the chainable
/// `with_*` / `set_*` setters, or via [`StatisticsCollector::finish`].
///
/// Besides the count catalogue, the snapshot carries a coarse picture of the
/// **degree distribution** (the maximum node degree overall and per label, an
/// optional supernode threshold, and a supernode count) so the planner can spot
/// hub nodes and avoid driving traversals through them (task `00087`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphStatistics {
    total_nodes: u64,
    total_relationships: u64,
    nodes_by_label: HashMap<String, u64>,
    relationships_by_type: HashMap<String, u64>,
    distinct_values: HashMap<(String, String), u64>,
    max_degree: u64,
    max_degree_by_label: HashMap<String, u64>,
    supernode_threshold: u64,
    supernode_count: u64,
}

impl GraphStatistics {
    /// Create an empty statistics snapshot (an empty graph).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the total node count (chainable).
    pub fn with_total_nodes(mut self, total: u64) -> Self {
        self.total_nodes = total;
        self
    }

    /// Set the total relationship count (chainable).
    pub fn with_total_relationships(mut self, total: u64) -> Self {
        self.total_relationships = total;
        self
    }

    /// Record the number of nodes carrying `label`.
    pub fn set_label_count(&mut self, label: impl Into<String>, count: u64) {
        self.nodes_by_label.insert(label.into(), count);
    }

    /// Record the number of relationships of `relationship_type`.
    pub fn set_relationship_type_count(
        &mut self,
        relationship_type: impl Into<String>,
        count: u64,
    ) {
        self.relationships_by_type
            .insert(relationship_type.into(), count);
    }

    /// Record how many distinct values `property` takes among nodes labelled
    /// `label`. Used to estimate the selectivity of `n.property = <value>`.
    pub fn set_distinct_values(
        &mut self,
        label: impl Into<String>,
        property: impl Into<String>,
        distinct: u64,
    ) {
        self.distinct_values
            .insert((label.into(), property.into()), distinct);
    }

    /// Total number of nodes in the graph.
    pub fn total_nodes(&self) -> u64 {
        self.total_nodes
    }

    /// Total number of relationships in the graph.
    pub fn total_relationships(&self) -> u64 {
        self.total_relationships
    }

    /// Number of nodes carrying `label`, or `None` when the catalogue holds no
    /// figure for it (an unseen label, distinct from a known zero).
    pub fn nodes_with_label(&self, label: &str) -> Option<u64> {
        self.nodes_by_label.get(label).copied()
    }

    /// Number of relationships of `relationship_type`, or `None` when unknown.
    pub fn relationships_with_type(&self, relationship_type: &str) -> Option<u64> {
        self.relationships_by_type.get(relationship_type).copied()
    }

    /// Distinct-value count for `(label, property)`, or `None` when unknown.
    pub fn distinct_values(&self, label: &str, property: &str) -> Option<u64> {
        self.distinct_values
            .get(&(label.to_string(), property.to_string()))
            .copied()
    }

    /// Average out-degree across the whole graph
    /// (`total_relationships / total_nodes`), or `0.0` for an empty graph.
    pub fn average_degree(&self) -> f64 {
        if self.total_nodes == 0 {
            0.0
        } else {
            self.total_relationships as f64 / self.total_nodes as f64
        }
    }

    // ---- Degree distribution / supernodes (task `00087`) ----

    /// Set the maximum degree of any single node in the graph (chainable).
    pub fn with_max_degree(mut self, degree: u64) -> Self {
        self.max_degree = degree;
        self
    }

    /// Record the maximum degree among nodes carrying `label` — how fanned-out
    /// the busiest node of that label is.
    pub fn set_max_degree_for_label(&mut self, label: impl Into<String>, degree: u64) {
        self.max_degree_by_label.insert(label.into(), degree);
    }

    /// Set an explicit supernode threshold (chainable). A node whose degree is
    /// at or above this value is a supernode. Pass `0` to fall back to the
    /// derived [`effective_supernode_threshold`](Self::effective_supernode_threshold).
    pub fn with_supernode_threshold(mut self, threshold: u64) -> Self {
        self.supernode_threshold = threshold;
        self
    }

    /// Record how many nodes in the graph are supernodes.
    pub fn set_supernode_count(&mut self, count: u64) {
        self.supernode_count = count;
    }

    /// Maximum degree of any single node, or `0` when unknown.
    pub fn max_degree(&self) -> u64 {
        self.max_degree
    }

    /// Maximum degree among nodes carrying `label`, or `None` when unknown.
    pub fn max_degree_for_label(&self, label: &str) -> Option<u64> {
        self.max_degree_by_label.get(label).copied()
    }

    /// The explicit supernode threshold, or `0` when none is configured (the
    /// derived [`effective_supernode_threshold`](Self::effective_supernode_threshold)
    /// is used instead).
    pub fn supernode_threshold(&self) -> u64 {
        self.supernode_threshold
    }

    /// Number of supernodes recorded for the graph.
    pub fn supernode_count(&self) -> u64 {
        self.supernode_count
    }

    /// The degree at or above which a node is treated as a supernode: the
    /// explicit [`supernode_threshold`](Self::supernode_threshold) when set
    /// (`> 0`), otherwise a value derived from the average degree
    /// (`average_degree * `[`DEFAULT_SUPERNODE_THRESHOLD_FACTOR`]) floored at
    /// [`MIN_SUPERNODE_THRESHOLD`].
    pub fn effective_supernode_threshold(&self) -> u64 {
        if self.supernode_threshold > 0 {
            self.supernode_threshold
        } else {
            let derived = (self.average_degree() * DEFAULT_SUPERNODE_THRESHOLD_FACTOR) as u64;
            derived.max(MIN_SUPERNODE_THRESHOLD)
        }
    }

    /// Whether a node of the given `degree` qualifies as a supernode under the
    /// [`effective_supernode_threshold`](Self::effective_supernode_threshold).
    pub fn is_supernode_degree(&self, degree: u64) -> bool {
        degree >= self.effective_supernode_threshold()
    }

    /// Whether nodes carrying `label` include a supernode — the maximum degree
    /// recorded for the label meets the threshold. `false` when no per-label
    /// maximum degree is known for `label`.
    pub fn label_has_supernode(&self, label: &str) -> bool {
        self.max_degree_for_label(label)
            .is_some_and(|d| self.is_supernode_degree(d))
    }

    /// Whether any of `labels` is known to contain a supernode.
    pub fn any_label_has_supernode(&self, labels: &[String]) -> bool {
        labels.iter().any(|l| self.label_has_supernode(l))
    }

    /// How skewed the degree distribution is: the maximum degree divided by the
    /// average (`1.0` ≈ uniform, large ⇒ a few hubs dominate). `0.0` when the
    /// graph has no edges or the maximum degree is unknown.
    pub fn degree_skew(&self) -> f64 {
        let avg = self.average_degree();
        if avg <= 0.0 || self.max_degree == 0 {
            0.0
        } else {
            self.max_degree as f64 / avg
        }
    }
}

/// Accumulates one observation per entity into a [`GraphStatistics`] snapshot.
///
/// The intended use is a single pass over a graph: call [`record_node`] for
/// each node (with its labels), [`record_relationship`] for each relationship
/// (with its type), and [`record_property`] for each indexed property value;
/// then [`finish`] to freeze the tallies into an immutable snapshot. Distinct
/// values are tracked exactly via per-`(label, property)` value sets, so the
/// finished `distinct_values` figure is precise for the observed sample.
///
/// [`record_node`]: StatisticsCollector::record_node
/// [`record_relationship`]: StatisticsCollector::record_relationship
/// [`record_property`]: StatisticsCollector::record_property
/// [`finish`]: StatisticsCollector::finish
#[derive(Debug, Clone, Default)]
pub struct StatisticsCollector {
    total_nodes: u64,
    total_relationships: u64,
    nodes_by_label: HashMap<String, u64>,
    relationships_by_type: HashMap<String, u64>,
    property_values: HashMap<(String, String), HashSet<String>>,
    max_degree: u64,
    max_degree_by_label: HashMap<String, u64>,
    supernode_threshold: u64,
    supernode_count: u64,
}

impl StatisticsCollector {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the supernode threshold used while counting supernodes (chainable).
    /// Nodes observed with [`record_node_degree`](Self::record_node_degree)
    /// after this call count toward [`GraphStatistics::supernode_count`] when
    /// their degree meets the threshold. With no explicit threshold the
    /// [`MIN_SUPERNODE_THRESHOLD`] floor is used for counting.
    pub fn with_supernode_threshold(mut self, threshold: u64) -> Self {
        self.supernode_threshold = threshold;
        self
    }

    /// Observe one node carrying `labels`. Increments the total node count and
    /// the per-label count for each label (a node with two labels counts once
    /// toward the total and once toward each label).
    pub fn record_node<S: AsRef<str>>(&mut self, labels: &[S]) {
        self.total_nodes += 1;
        for label in labels {
            *self
                .nodes_by_label
                .entry(label.as_ref().to_string())
                .or_insert(0) += 1;
        }
    }

    /// Observe one node carrying `labels` together with its `degree` (the count
    /// of relationships incident to it).
    ///
    /// Like [`record_node`](Self::record_node) it tallies the node and its
    /// labels, and additionally tracks the maximum degree overall and per label
    /// and counts the node as a supernode when its degree meets the configured
    /// threshold (or the [`MIN_SUPERNODE_THRESHOLD`] floor when none is set).
    /// Call this *instead of* [`record_node`](Self::record_node) when a node's
    /// degree is known — calling both for the same node would double-count it.
    pub fn record_node_degree<S: AsRef<str>>(&mut self, labels: &[S], degree: u64) {
        self.record_node(labels);
        if degree > self.max_degree {
            self.max_degree = degree;
        }
        for label in labels {
            let entry = self
                .max_degree_by_label
                .entry(label.as_ref().to_string())
                .or_insert(0);
            if degree > *entry {
                *entry = degree;
            }
        }
        let threshold = if self.supernode_threshold > 0 {
            self.supernode_threshold
        } else {
            MIN_SUPERNODE_THRESHOLD
        };
        if degree >= threshold {
            self.supernode_count += 1;
        }
    }

    /// Observe one relationship of `relationship_type`.
    pub fn record_relationship(&mut self, relationship_type: &str) {
        self.total_relationships += 1;
        *self
            .relationships_by_type
            .entry(relationship_type.to_string())
            .or_insert(0) += 1;
    }

    /// Observe one value of `property` on a node labelled `label`. Repeated
    /// identical values collapse, so the finished distinct count reflects the
    /// number of *distinct* values seen.
    pub fn record_property(&mut self, label: &str, property: &str, value: &str) {
        self.property_values
            .entry((label.to_string(), property.to_string()))
            .or_default()
            .insert(value.to_string());
    }

    /// Freeze the accumulated observations into an immutable
    /// [`GraphStatistics`] snapshot.
    pub fn finish(self) -> GraphStatistics {
        let distinct_values = self
            .property_values
            .into_iter()
            .map(|(key, values)| (key, values.len() as u64))
            .collect();
        GraphStatistics {
            total_nodes: self.total_nodes,
            total_relationships: self.total_relationships,
            nodes_by_label: self.nodes_by_label,
            relationships_by_type: self.relationships_by_type,
            distinct_values,
            max_degree: self.max_degree,
            max_degree_by_label: self.max_degree_by_label,
            supernode_threshold: self.supernode_threshold,
            supernode_count: self.supernode_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_statistics_report_zero() {
        let stats = GraphStatistics::new();
        assert_eq!(stats.total_nodes(), 0);
        assert_eq!(stats.total_relationships(), 0);
        assert_eq!(stats.nodes_with_label("Person"), None);
        assert_eq!(stats.average_degree(), 0.0);
    }

    #[test]
    fn setters_record_label_and_type_counts() {
        let mut stats = GraphStatistics::new()
            .with_total_nodes(100)
            .with_total_relationships(250);
        stats.set_label_count("Person", 60);
        stats.set_relationship_type_count("KNOWS", 250);
        assert_eq!(stats.total_nodes(), 100);
        assert_eq!(stats.nodes_with_label("Person"), Some(60));
        assert_eq!(stats.relationships_with_type("KNOWS"), Some(250));
    }

    #[test]
    fn average_degree_is_relationships_over_nodes() {
        let stats = GraphStatistics::new()
            .with_total_nodes(10)
            .with_total_relationships(30);
        assert!((stats.average_degree() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_label_is_distinct_from_zero() {
        let mut stats = GraphStatistics::new();
        stats.set_label_count("Task", 0);
        assert_eq!(stats.nodes_with_label("Task"), Some(0));
        assert_eq!(stats.nodes_with_label("Ghost"), None);
    }

    #[test]
    fn collector_tallies_nodes_and_labels() {
        let mut collector = StatisticsCollector::new();
        collector.record_node(&["Person"]);
        collector.record_node(&["Person", "Admin"]);
        collector.record_node(&["Task"]);
        let stats = collector.finish();
        assert_eq!(stats.total_nodes(), 3);
        assert_eq!(stats.nodes_with_label("Person"), Some(2));
        assert_eq!(stats.nodes_with_label("Admin"), Some(1));
        assert_eq!(stats.nodes_with_label("Task"), Some(1));
    }

    #[test]
    fn collector_tallies_relationship_types() {
        let mut collector = StatisticsCollector::new();
        collector.record_relationship("KNOWS");
        collector.record_relationship("KNOWS");
        collector.record_relationship("ASSIGNED_TO");
        let stats = collector.finish();
        assert_eq!(stats.total_relationships(), 3);
        assert_eq!(stats.relationships_with_type("KNOWS"), Some(2));
        assert_eq!(stats.relationships_with_type("ASSIGNED_TO"), Some(1));
    }

    #[test]
    fn collector_counts_distinct_property_values() {
        let mut collector = StatisticsCollector::new();
        collector.record_property("Task", "status", "open");
        collector.record_property("Task", "status", "open");
        collector.record_property("Task", "status", "done");
        collector.record_property("Task", "status", "blocked");
        let stats = collector.finish();
        // Three distinct values despite four observations.
        assert_eq!(stats.distinct_values("Task", "status"), Some(3));
        assert_eq!(stats.distinct_values("Task", "missing"), None);
    }

    #[test]
    fn collector_finish_is_empty_for_empty_graph() {
        let stats = StatisticsCollector::new().finish();
        assert_eq!(stats, GraphStatistics::new());
    }

    // ---- Degree distribution / supernodes (task `00087`) ----

    #[test]
    fn degree_setters_and_getters_round_trip() {
        let mut s = GraphStatistics::new().with_max_degree(5000);
        s.set_max_degree_for_label("Tag", 5000);
        s.set_supernode_count(2);
        assert_eq!(s.max_degree(), 5000);
        assert_eq!(s.max_degree_for_label("Tag"), Some(5000));
        assert_eq!(s.max_degree_for_label("Post"), None);
        assert_eq!(s.supernode_count(), 2);
    }

    #[test]
    fn effective_threshold_prefers_explicit_value() {
        let s = GraphStatistics::new()
            .with_total_nodes(1000)
            .with_total_relationships(2000)
            .with_supernode_threshold(42);
        assert_eq!(s.supernode_threshold(), 42);
        assert_eq!(s.effective_supernode_threshold(), 42);
    }

    #[test]
    fn effective_threshold_derives_from_average_when_unset() {
        // avg degree 10 → derived threshold = 10 * 100 = 1000.
        let s = GraphStatistics::new()
            .with_total_nodes(1000)
            .with_total_relationships(10_000);
        assert_eq!(s.supernode_threshold(), 0);
        assert_eq!(s.effective_supernode_threshold(), 1000);
    }

    #[test]
    fn effective_threshold_is_floored_on_sparse_graphs() {
        // avg degree 2 → 2 * 100 = 200, but a tiny graph would otherwise let an
        // ordinary node through; the floor keeps it at MIN_SUPERNODE_THRESHOLD.
        let sparse = GraphStatistics::new()
            .with_total_nodes(10)
            .with_total_relationships(5); // avg 0.5 → 50, floored to 100.
        assert_eq!(
            sparse.effective_supernode_threshold(),
            MIN_SUPERNODE_THRESHOLD
        );
    }

    #[test]
    fn label_has_supernode_uses_per_label_max_degree() {
        let mut s = GraphStatistics::new()
            .with_total_nodes(10_000)
            .with_total_relationships(20_000) // avg 2 → threshold floored to 100.
            .with_max_degree(8000);
        s.set_max_degree_for_label("Tag", 8000); // the hub
        s.set_max_degree_for_label("Post", 3); // ordinary
        assert!(s.label_has_supernode("Tag"));
        assert!(!s.label_has_supernode("Post"));
        // Unknown label has no per-label max degree, so it is not a supernode.
        assert!(!s.label_has_supernode("Ghost"));
        assert!(s.any_label_has_supernode(&["Post".into(), "Tag".into()]));
        assert!(!s.any_label_has_supernode(&["Post".into()]));
    }

    #[test]
    fn is_supernode_degree_compares_against_threshold() {
        let s = GraphStatistics::new().with_supernode_threshold(100);
        assert!(s.is_supernode_degree(100));
        assert!(s.is_supernode_degree(101));
        assert!(!s.is_supernode_degree(99));
    }

    #[test]
    fn degree_skew_is_max_over_average() {
        let s = GraphStatistics::new()
            .with_total_nodes(1000)
            .with_total_relationships(2000) // avg 2.
            .with_max_degree(500);
        assert!((s.degree_skew() - 250.0).abs() < 1e-9);
        // No max degree / no edges → zero skew, never NaN.
        assert_eq!(GraphStatistics::new().degree_skew(), 0.0);
        assert_eq!(
            GraphStatistics::new().with_max_degree(10).degree_skew(),
            0.0
        );
    }

    #[test]
    fn collector_tracks_max_degree_and_counts_supernodes() {
        let mut collector = StatisticsCollector::new().with_supernode_threshold(100);
        collector.record_node_degree(&["Tag"], 5000); // hub supernode
        collector.record_node_degree(&["Post"], 2);
        collector.record_node_degree(&["Post"], 3);
        collector.record_node_degree(&["Author"], 150); // also a supernode
        let stats = collector.finish();
        // record_node_degree also counts the node + its labels.
        assert_eq!(stats.total_nodes(), 4);
        assert_eq!(stats.nodes_with_label("Post"), Some(2));
        // Degree aggregates.
        assert_eq!(stats.max_degree(), 5000);
        assert_eq!(stats.max_degree_for_label("Tag"), Some(5000));
        assert_eq!(stats.max_degree_for_label("Post"), Some(3));
        // Two nodes at/above the threshold of 100.
        assert_eq!(stats.supernode_count(), 2);
        assert_eq!(stats.supernode_threshold(), 100);
    }

    #[test]
    fn collector_default_supernode_count_uses_floor() {
        // No explicit threshold → counting uses MIN_SUPERNODE_THRESHOLD (100).
        let mut collector = StatisticsCollector::new();
        collector.record_node_degree(&["Hub"], MIN_SUPERNODE_THRESHOLD);
        collector.record_node_degree(&["Leaf"], MIN_SUPERNODE_THRESHOLD - 1);
        let stats = collector.finish();
        assert_eq!(stats.supernode_count(), 1);
    }
}
