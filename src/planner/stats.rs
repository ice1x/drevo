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

/// Immutable snapshot of the graph catalogue figures used to estimate query
/// cardinality.
///
/// All counts are best-effort estimates, not transactionally exact — a planner
/// only needs them to choose between plans, so a slightly stale snapshot is
/// acceptable. Construct one with [`GraphStatistics::new`] plus the chainable
/// `with_*` / `set_*` setters, or via [`StatisticsCollector::finish`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphStatistics {
    total_nodes: u64,
    total_relationships: u64,
    nodes_by_label: HashMap<String, u64>,
    relationships_by_type: HashMap<String, u64>,
    distinct_values: HashMap<(String, String), u64>,
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
}

impl StatisticsCollector {
    /// Create an empty collector.
    pub fn new() -> Self {
        Self::default()
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
}
