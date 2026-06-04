//! Cardinality estimation — turning [`GraphStatistics`] into row-count guesses
//! for the building blocks of a query plan.
//!
//! A [`CardinalityEstimator`] borrows a statistics snapshot and answers three
//! questions a planner asks of every operator:
//!
//! * **scan** — how many nodes does `MATCH (n:Label)` produce?
//!   ([`estimate_node_scan`](CardinalityEstimator::estimate_node_scan))
//! * **expand** — given `R` input rows, how many does a relationship hop
//!   produce? ([`estimate_expand`](CardinalityEstimator::estimate_expand))
//! * **filter** — what fraction of input rows survives a `WHERE` predicate?
//!   ([`selectivity`](CardinalityEstimator::selectivity) /
//!   [`estimate_filter`](CardinalityEstimator::estimate_filter))
//!
//! Estimates are deliberately coarse: when the catalogue carries no figure
//! (an unknown label, a property with no distinct-value count) the estimator
//! falls back to the documented `DEFAULT_*` constants below — the textbook
//! magic numbers a planner uses in the absence of statistics. Refining them
//! with histograms is future work; this task establishes the contract.

use std::collections::HashMap;

use crate::cypher::ast::{BinaryOp, Direction, Expression, UnaryOp};
use crate::planner::stats::{GraphStatistics, DEFAULT_SUPERNODE_THRESHOLD_FACTOR};

/// Default fraction of rows passing an equality predicate (`n.p = v`) when the
/// catalogue holds no distinct-value count for the property.
pub const DEFAULT_EQUALITY_SELECTIVITY: f64 = 0.1;
/// Default fraction of rows passing a range predicate (`<`, `<=`, `>`, `>=`).
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 1.0 / 3.0;
/// Default fraction of rows passing a string-match predicate (`STARTS WITH`,
/// `ENDS WITH`, `CONTAINS`, `=~`).
pub const DEFAULT_STRING_MATCH_SELECTIVITY: f64 = 0.1;
/// Default fraction of rows passing an `IS NULL` predicate (most properties
/// are present, so a null match is rare).
pub const DEFAULT_NULL_SELECTIVITY: f64 = 0.1;
/// Fallback fraction for any predicate shape the estimator does not model
/// (an opaque function call, a property-to-property comparison, …).
pub const DEFAULT_PREDICATE_SELECTIVITY: f64 = 0.25;

/// Estimates row counts for query-plan operators from a [`GraphStatistics`]
/// snapshot. Borrows the snapshot for its lifetime; holds no mutable state.
#[derive(Debug, Clone, Copy)]
pub struct CardinalityEstimator<'s> {
    stats: &'s GraphStatistics,
}

impl<'s> CardinalityEstimator<'s> {
    /// Create an estimator backed by `stats`.
    pub fn new(stats: &'s GraphStatistics) -> Self {
        Self { stats }
    }

    /// The statistics snapshot this estimator reads.
    pub fn statistics(&self) -> &GraphStatistics {
        self.stats
    }

    /// Estimate the rows produced by scanning nodes matching `labels`.
    ///
    /// With no labels the scan touches every node. With labels, the most
    /// selective *known* label count wins (a node carrying all the labels can
    /// be no more numerous than the rarest one). Labels absent from the
    /// catalogue contribute no information; if every label is unknown the
    /// estimate falls back to the total node count.
    pub fn estimate_node_scan(&self, labels: &[String]) -> f64 {
        if labels.is_empty() {
            return self.stats.total_nodes() as f64;
        }
        let mut best: Option<u64> = None;
        for label in labels {
            if let Some(count) = self.stats.nodes_with_label(label) {
                best = Some(best.map_or(count, |b| b.min(count)));
            }
        }
        match best {
            Some(count) => count as f64,
            None => self.stats.total_nodes() as f64,
        }
    }

    /// Estimate the rows produced by expanding a relationship from `input_rows`
    /// already-bound rows.
    ///
    /// The fan-out per row is the average degree for the relationship types:
    /// `count(type) / total_nodes` summed over the named types, or the overall
    /// average degree when no type is named (or a named type is unknown). An
    /// undirected hop sees both incoming and outgoing edges, so its fan-out is
    /// doubled.
    pub fn estimate_expand(
        &self,
        input_rows: f64,
        rel_types: &[String],
        direction: Direction,
    ) -> f64 {
        let total = self.stats.total_nodes();
        if total == 0 {
            return 0.0;
        }
        let nodes = total as f64;
        let fanout = if rel_types.is_empty() {
            self.stats.average_degree()
        } else {
            rel_types
                .iter()
                .map(|ty| match self.stats.relationships_with_type(ty) {
                    Some(count) => count as f64 / nodes,
                    None => self.stats.average_degree(),
                })
                .sum()
        };
        let directional = match direction {
            Direction::Undirected => fanout * 2.0,
            Direction::Outgoing | Direction::Incoming => fanout,
        };
        input_rows * directional
    }

    /// Estimate the rows produced by expanding a relationship from `input_rows`
    /// already-bound rows **in the worst case** — when the expansion drives out
    /// of a supernode (task `00087`).
    ///
    /// Where [`estimate_expand`](Self::estimate_expand) uses the *average*
    /// degree, this uses the graph's *maximum* node degree: the upper bound on
    /// how many edges a single row can fan out to, regardless of type or
    /// direction (a node cannot have more edges of one type/direction than it
    /// has in total). When no maximum degree is known it falls back to
    /// [`estimate_expand`](Self::estimate_expand).
    pub fn estimate_expand_worst_case(
        &self,
        input_rows: f64,
        rel_types: &[String],
        direction: Direction,
    ) -> f64 {
        let max_degree = self.stats.max_degree();
        if max_degree == 0 {
            return self.estimate_expand(input_rows, rel_types, direction);
        }
        input_rows * max_degree as f64
    }

    /// Whether expanding outward from nodes carrying `anchor_labels` risks
    /// driving through a supernode (task `00087`): any label is known to contain
    /// one, or — with no per-label degree information (an unlabelled anchor) —
    /// the graph's degree distribution is heavily skewed
    /// (`degree_skew >= `[`DEFAULT_SUPERNODE_THRESHOLD_FACTOR`]).
    pub fn expand_risks_supernode(&self, anchor_labels: &[String]) -> bool {
        if self.stats.any_label_has_supernode(anchor_labels) {
            return true;
        }
        anchor_labels.is_empty() && self.stats.degree_skew() >= DEFAULT_SUPERNODE_THRESHOLD_FACTOR
    }

    /// Estimate the fraction of rows in `[0.0, 1.0]` that satisfy `predicate`,
    /// ignoring variable→label bindings (equality falls back to the default
    /// constant). See [`selectivity_with_bindings`] to use distinct-value
    /// statistics.
    ///
    /// [`selectivity_with_bindings`]: CardinalityEstimator::selectivity_with_bindings
    pub fn selectivity(&self, predicate: &Expression) -> f64 {
        let bindings = HashMap::new();
        self.selectivity_with_bindings(predicate, &bindings)
    }

    /// Estimate the fraction of rows satisfying `predicate`, using `bindings`
    /// (variable → its labels) to resolve property equality against the
    /// catalogue's distinct-value counts.
    pub fn selectivity_with_bindings(
        &self,
        predicate: &Expression,
        bindings: &HashMap<String, Vec<String>>,
    ) -> f64 {
        match predicate {
            Expression::True(_) => 1.0,
            Expression::False(_) => 0.0,
            Expression::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => 1.0 - self.selectivity_with_bindings(expr, bindings),
            Expression::IsNull { negated, .. } => {
                if *negated {
                    1.0 - DEFAULT_NULL_SELECTIVITY
                } else {
                    DEFAULT_NULL_SELECTIVITY
                }
            }
            Expression::In { list, .. } => self.in_selectivity(list),
            Expression::Binary { op, lhs, rhs, .. } => {
                self.binary_selectivity(*op, lhs, rhs, bindings)
            }
            _ => DEFAULT_PREDICATE_SELECTIVITY,
        }
    }

    /// Estimate the rows surviving `predicate` applied to `input_rows`.
    pub fn estimate_filter(&self, input_rows: f64, predicate: &Expression) -> f64 {
        input_rows * self.selectivity(predicate)
    }

    /// As [`estimate_filter`](Self::estimate_filter), but binding-aware.
    pub fn estimate_filter_with_bindings(
        &self,
        input_rows: f64,
        predicate: &Expression,
        bindings: &HashMap<String, Vec<String>>,
    ) -> f64 {
        input_rows * self.selectivity_with_bindings(predicate, bindings)
    }

    fn binary_selectivity(
        &self,
        op: BinaryOp,
        lhs: &Expression,
        rhs: &Expression,
        bindings: &HashMap<String, Vec<String>>,
    ) -> f64 {
        match op {
            BinaryOp::And => {
                self.selectivity_with_bindings(lhs, bindings)
                    * self.selectivity_with_bindings(rhs, bindings)
            }
            BinaryOp::Or => {
                let a = self.selectivity_with_bindings(lhs, bindings);
                let b = self.selectivity_with_bindings(rhs, bindings);
                a + b - a * b
            }
            BinaryOp::Xor => {
                let a = self.selectivity_with_bindings(lhs, bindings);
                let b = self.selectivity_with_bindings(rhs, bindings);
                (a + b - 2.0 * a * b).clamp(0.0, 1.0)
            }
            BinaryOp::Eq => self.equality_selectivity(lhs, rhs, bindings),
            BinaryOp::Ne => 1.0 - self.equality_selectivity(lhs, rhs, bindings),
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => DEFAULT_RANGE_SELECTIVITY,
            BinaryOp::StartsWith
            | BinaryOp::EndsWith
            | BinaryOp::Contains
            | BinaryOp::RegexMatch => DEFAULT_STRING_MATCH_SELECTIVITY,
            // Arithmetic operators are not boolean predicates; treat any such
            // shape with the opaque fallback.
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::Pow => DEFAULT_PREDICATE_SELECTIVITY,
        }
    }

    /// Selectivity of `lhs = rhs`. When one side is a property access on a
    /// bound variable and the other is a constant, `1 / distinct_values` is
    /// used (taking the rarest applicable label); otherwise the default.
    fn equality_selectivity(
        &self,
        lhs: &Expression,
        rhs: &Expression,
        bindings: &HashMap<String, Vec<String>>,
    ) -> f64 {
        let resolved = self
            .property_distinct(lhs, rhs, bindings)
            .or_else(|| self.property_distinct(rhs, lhs, bindings));
        match resolved {
            Some(distinct) if distinct > 0 => 1.0 / distinct as f64,
            _ => DEFAULT_EQUALITY_SELECTIVITY,
        }
    }

    /// If `maybe_property` is `var.prop`, `var` is bound, and `other` is a
    /// constant, return the largest known distinct-value count across `var`'s
    /// labels (the rarest, most-selective interpretation uses the largest
    /// distinct count → smallest selectivity).
    fn property_distinct(
        &self,
        maybe_property: &Expression,
        other: &Expression,
        bindings: &HashMap<String, Vec<String>>,
    ) -> Option<u64> {
        if !is_constant(other) {
            return None;
        }
        let Expression::Property { base, name, .. } = maybe_property else {
            return None;
        };
        let Expression::Variable(var, _) = base.as_ref() else {
            return None;
        };
        let labels = bindings.get(var)?;
        labels
            .iter()
            .filter_map(|label| self.stats.distinct_values(label, name))
            .max()
    }

    /// Selectivity of `x IN list`: the equality selectivity scaled by the list
    /// length (capped at 1.0). A non-literal list falls back to the default.
    fn in_selectivity(&self, list: &Expression) -> f64 {
        let per_element = DEFAULT_EQUALITY_SELECTIVITY;
        match list {
            Expression::List { items, .. } => (per_element * items.len() as f64).min(1.0),
            _ => DEFAULT_PREDICATE_SELECTIVITY,
        }
    }
}

/// Whether `expr` is a constant the estimator can treat as a fixed value for
/// equality selectivity (a literal or a query parameter).
fn is_constant(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::Integer(..)
            | Expression::Float(..)
            | Expression::String(..)
            | Expression::True(_)
            | Expression::False(_)
            | Expression::Parameter(..)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;

    fn stats() -> GraphStatistics {
        let mut s = GraphStatistics::new()
            .with_total_nodes(1000)
            .with_total_relationships(5000);
        s.set_label_count("Person", 600);
        s.set_label_count("Admin", 20);
        s.set_relationship_type_count("KNOWS", 4000);
        s.set_relationship_type_count("MANAGES", 1000);
        s.set_distinct_values("Task", "status", 4);
        s
    }

    /// Parse a query and return the single `WHERE` predicate expression it
    /// contains, for selectivity tests.
    fn where_predicate(query: &str) -> Expression {
        let ast = parse(query).expect("query parses");
        for clause in &ast.parts[0].query.clauses {
            if let crate::cypher::ast::Clause::Match(m) = clause {
                if let Some(w) = &m.where_clause {
                    return w.clone();
                }
            }
        }
        panic!("query has no WHERE predicate");
    }

    #[test]
    fn unlabelled_scan_touches_every_node() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        assert!((est.estimate_node_scan(&[]) - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn labelled_scan_uses_label_count() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        assert!((est.estimate_node_scan(&["Person".into()]) - 600.0).abs() < 1e-9);
    }

    #[test]
    fn multi_label_scan_uses_rarest_label() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        // Person (600) ∧ Admin (20) → bounded by the rarer Admin.
        let rows = est.estimate_node_scan(&["Person".into(), "Admin".into()]);
        assert!((rows - 20.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_label_scan_falls_back_to_total() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        assert!((est.estimate_node_scan(&["Ghost".into()]) - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn expand_uses_average_degree_when_untyped() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        // 5000 / 1000 = 5 per row.
        let rows = est.estimate_expand(10.0, &[], Direction::Outgoing);
        assert!((rows - 50.0).abs() < 1e-9);
    }

    #[test]
    fn expand_uses_typed_degree() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        // KNOWS: 4000 / 1000 = 4 per row.
        let rows = est.estimate_expand(10.0, &["KNOWS".into()], Direction::Outgoing);
        assert!((rows - 40.0).abs() < 1e-9);
    }

    #[test]
    fn undirected_expand_doubles_fanout() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let rows = est.estimate_expand(10.0, &["KNOWS".into()], Direction::Undirected);
        assert!((rows - 80.0).abs() < 1e-9);
    }

    #[test]
    fn expand_on_empty_graph_is_zero() {
        let s = GraphStatistics::new();
        let est = CardinalityEstimator::new(&s);
        assert_eq!(est.estimate_expand(10.0, &[], Direction::Outgoing), 0.0);
    }

    #[test]
    fn equality_uses_default_without_bindings() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.status = 'open' RETURN n");
        assert!((est.selectivity(&pred) - DEFAULT_EQUALITY_SELECTIVITY).abs() < 1e-9);
    }

    #[test]
    fn equality_uses_distinct_values_with_bindings() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n:Task) WHERE n.status = 'open' RETURN n");
        let mut bindings = HashMap::new();
        bindings.insert("n".to_string(), vec!["Task".to_string()]);
        // distinct(Task.status) = 4 → 1/4.
        assert!((est.selectivity_with_bindings(&pred, &bindings) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn conjunction_multiplies_selectivities() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.a = 1 AND n.b = 2 RETURN n");
        // 0.1 * 0.1.
        assert!((est.selectivity(&pred) - 0.01).abs() < 1e-9);
    }

    #[test]
    fn disjunction_uses_inclusion_exclusion() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.a = 1 OR n.b = 2 RETURN n");
        // 0.1 + 0.1 - 0.01 = 0.19.
        assert!((est.selectivity(&pred) - 0.19).abs() < 1e-9);
    }

    #[test]
    fn negation_complements_selectivity() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE NOT n.a = 1 RETURN n");
        assert!((est.selectivity(&pred) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn range_predicate_uses_range_default() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.age > 30 RETURN n");
        assert!((est.selectivity(&pred) - DEFAULT_RANGE_SELECTIVITY).abs() < 1e-9);
    }

    #[test]
    fn string_match_uses_string_default() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.name STARTS WITH 'A' RETURN n");
        assert!((est.selectivity(&pred) - DEFAULT_STRING_MATCH_SELECTIVITY).abs() < 1e-9);
    }

    #[test]
    fn is_null_and_is_not_null_are_complementary() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let is_null = where_predicate("MATCH (n) WHERE n.x IS NULL RETURN n");
        let not_null = where_predicate("MATCH (n) WHERE n.x IS NOT NULL RETURN n");
        assert!((est.selectivity(&is_null) - DEFAULT_NULL_SELECTIVITY).abs() < 1e-9);
        assert!((est.selectivity(&not_null) - (1.0 - DEFAULT_NULL_SELECTIVITY)).abs() < 1e-9);
    }

    #[test]
    fn in_list_scales_with_length() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.x IN [1, 2, 3] RETURN n");
        // 0.1 * 3 = 0.3.
        assert!((est.selectivity(&pred) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn estimate_filter_applies_selectivity_to_rows() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let pred = where_predicate("MATCH (n) WHERE n.status = 'open' RETURN n");
        assert!((est.estimate_filter(600.0, &pred) - 60.0).abs() < 1e-9);
    }

    // ---- Supernode-aware expansion (task `00087`) ----

    /// Statistics with a single hub: `Tag` nodes are rare but one is linked to
    /// nearly every node, while the average degree stays low.
    fn supernode_stats() -> GraphStatistics {
        let mut s = GraphStatistics::new()
            .with_total_nodes(10_000)
            .with_total_relationships(20_000) // avg degree 2.
            .with_max_degree(9000);
        s.set_label_count("Post", 9000);
        s.set_label_count("Tag", 50);
        s.set_max_degree_for_label("Tag", 9000); // the hub tag
        s.set_max_degree_for_label("Post", 4); // ordinary
        s
    }

    #[test]
    fn worst_case_expand_uses_max_degree() {
        let s = supernode_stats();
        let est = CardinalityEstimator::new(&s);
        // 10 rows * max degree 9000 = 90_000, independent of type/direction.
        let rows = est.estimate_expand_worst_case(10.0, &[], Direction::Outgoing);
        assert!((rows - 90_000.0).abs() < 1e-6);
        // And far above the average-degree estimate of 10 * 2 = 20.
        assert!(rows > est.estimate_expand(10.0, &[], Direction::Outgoing));
    }

    #[test]
    fn worst_case_expand_falls_back_without_max_degree() {
        // No degree statistics → behaves exactly like the average estimate.
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        let avg = est.estimate_expand(10.0, &["KNOWS".into()], Direction::Outgoing);
        let worst = est.estimate_expand_worst_case(10.0, &["KNOWS".into()], Direction::Outgoing);
        assert!((avg - worst).abs() < 1e-9);
    }

    #[test]
    fn expand_risks_supernode_detects_a_hub_label() {
        let s = supernode_stats();
        let est = CardinalityEstimator::new(&s);
        assert!(est.expand_risks_supernode(&["Tag".into()]));
        assert!(!est.expand_risks_supernode(&["Post".into()]));
        // Unlabelled anchor over a heavily skewed graph is also a risk.
        assert!(est.expand_risks_supernode(&[]));
    }

    #[test]
    fn expand_risks_supernode_is_false_without_degree_stats() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        assert!(!est.expand_risks_supernode(&["Person".into()]));
        assert!(!est.expand_risks_supernode(&[]));
    }

    #[test]
    fn selectivity_is_always_within_unit_interval() {
        let s = stats();
        let est = CardinalityEstimator::new(&s);
        for q in [
            "MATCH (n) WHERE n.a = 1 OR n.b = 2 OR n.c = 3 RETURN n",
            "MATCH (n) WHERE n.x IN [1,2,3,4,5,6,7,8,9,10,11,12] RETURN n",
            "MATCH (n) WHERE n.a = 1 XOR n.b = 2 RETURN n",
            "MATCH (n) WHERE NOT (n.a > 1 AND n.b < 2) RETURN n",
        ] {
            let pred = where_predicate(q);
            let sel = est.selectivity(&pred);
            assert!(
                (0.0..=1.0).contains(&sel),
                "selectivity {sel} out of range for {q}"
            );
        }
    }
}
