//! Query plans — the annotated operator tree a cost-based planner produces.
//!
//! A [`PlanNode`] is one operator (a scan, an expand, a filter, a projection,
//! …) carrying its estimated output-row count and a cumulative cost, with its
//! input operators as children. [`plan_single_query`] / [`plan_query`] walk a
//! parsed Cypher [`SingleQuery`] / [`Query`] and build a naive **left-deep**
//! plan: scan the leading node, expand along each relationship leg, apply the
//! `WHERE` filter, project the `RETURN`. Every operator is annotated by the
//! [`CardinalityEstimator`] as it is built.
//!
//! This is the *un-optimised* plan: operators stay in source order. Choosing a
//! cheaper order (pattern reordering, index selection, join strategies) is the
//! next task (`00086`); this task delivers the plan representation, the cost
//! annotations, and the [`PlanNode::explain`] rendering the phase's
//! `EXPLAIN`-style output is built on.

use crate::cypher::ast::{
    BinaryOp, Clause, Direction, Expression, NodePattern, PathPattern, PathSegment, ProjectionItem,
    Query, SingleQuery, UnaryOp,
};
use crate::planner::cardinality::CardinalityEstimator;
use crate::planner::stats::GraphStatistics;
use std::collections::HashMap;

/// Assumed average list length for an `UNWIND` when no better estimate exists.
const DEFAULT_UNWIND_LENGTH: f64 = 10.0;

/// A single operator in a query plan, annotated with its estimated output-row
/// count and cumulative cost, plus its input operators as `children`.
///
/// Construct one by planning a query ([`plan_single_query`] / [`plan_query`]);
/// inspect it with the accessor methods or render it with
/// [`explain`](PlanNode::explain).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    operator: Operator,
    estimated_rows: f64,
    estimated_cost: f64,
    children: Vec<PlanNode>,
}

/// The kind of work a [`PlanNode`] performs.
#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    /// Scan every node in the graph (`MATCH (n)`).
    AllNodesScan {
        /// Variable bound to each scanned node.
        variable: String,
    },
    /// Scan nodes carrying the given labels (`MATCH (n:Label)`).
    NodeByLabelScan {
        /// Variable bound to each scanned node.
        variable: String,
        /// Labels constraining the scan.
        labels: Vec<String>,
    },
    /// Expand a relationship from an already-bound node to a new one.
    Expand {
        /// Source variable the expansion starts from.
        from: String,
        /// Variable bound to the traversed relationship, if named.
        rel_variable: Option<String>,
        /// Allowed relationship types (empty = any type).
        rel_types: Vec<String>,
        /// Direction of the expansion.
        direction: Direction,
        /// Variable bound to the destination node.
        to_variable: String,
        /// Labels constraining the destination node.
        to_labels: Vec<String>,
    },
    /// Discard input rows that fail a `WHERE` predicate.
    Filter {
        /// Rendered predicate, for the `EXPLAIN` output.
        predicate: String,
    },
    /// The cartesian product of two disconnected sub-plans.
    CartesianProduct,
    /// Project `WITH` / `RETURN` columns (optionally `DISTINCT`).
    Projection {
        /// Rendered projection columns.
        columns: Vec<String>,
        /// `true` for `DISTINCT`.
        distinct: bool,
    },
    /// Expand a list into one row per element (`UNWIND list AS x`).
    Unwind {
        /// Variable bound to each element.
        variable: String,
    },
    /// Drop the first `count` rows (`SKIP count`).
    Skip {
        /// Number of rows skipped.
        count: u64,
    },
    /// Keep at most `count` rows (`LIMIT count`).
    Limit {
        /// Row cap.
        count: u64,
    },
    /// `UNION` of several sub-plans.
    Union,
    /// A single empty input row — the seed for a `RETURN`/`UNWIND` with no
    /// preceding `MATCH` (`RETURN 1`).
    SingleRow,
    /// No rows — a write-only query with nothing to read back.
    EmptyResult,
}

impl PlanNode {
    /// The operator this node performs.
    pub fn operator(&self) -> &Operator {
        &self.operator
    }

    /// The estimated number of rows this operator produces.
    pub fn estimated_rows(&self) -> f64 {
        self.estimated_rows
    }

    /// The estimated cumulative cost of this operator and its inputs.
    pub fn estimated_cost(&self) -> f64 {
        self.estimated_cost
    }

    /// The input operators feeding this one.
    pub fn children(&self) -> &[PlanNode] {
        &self.children
    }

    /// A leaf with no children whose cost equals its row count.
    fn leaf(operator: Operator, rows: f64) -> Self {
        Self {
            operator,
            estimated_rows: rows,
            estimated_cost: rows,
            children: vec![],
        }
    }

    /// A single-empty-row seed (1 row, cost 1).
    fn single_row() -> Self {
        Self::leaf(Operator::SingleRow, 1.0)
    }

    /// An empty result (0 rows, cost 0).
    fn empty_result() -> Self {
        Self::leaf(Operator::EmptyResult, 0.0)
    }

    /// Wrap `child` in a unary operator with the given row estimate and an
    /// added cost over the child's cumulative cost.
    fn unary(operator: Operator, rows: f64, added_cost: f64, child: PlanNode) -> Self {
        let estimated_cost = child.estimated_cost + added_cost;
        Self {
            operator,
            estimated_rows: rows,
            estimated_cost,
            children: vec![child],
        }
    }

    /// Render the plan as an indented operator tree (an `EXPLAIN`-style view).
    /// The root prints first at the shallowest indent; each input is indented
    /// one level deeper with a `| ` guide and a leading `+`.
    pub fn explain(&self) -> String {
        let mut out = String::new();
        self.render(0, &mut out);
        out
    }

    fn render(&self, depth: usize, out: &mut String) {
        for _ in 0..depth {
            out.push_str("| ");
        }
        out.push('+');
        out.push_str(self.operator.label());
        let detail = self.operator.detail();
        if !detail.is_empty() {
            out.push(' ');
            out.push_str(&detail);
        }
        out.push_str(&format!(
            "  (estRows={:.0}, cost={:.0})\n",
            self.estimated_rows.max(0.0),
            self.estimated_cost.max(0.0)
        ));
        for child in &self.children {
            child.render(depth + 1, out);
        }
    }
}

impl Operator {
    /// Short operator name shown in [`PlanNode::explain`].
    pub fn label(&self) -> &'static str {
        match self {
            Operator::AllNodesScan { .. } => "AllNodesScan",
            Operator::NodeByLabelScan { .. } => "NodeByLabelScan",
            Operator::Expand { .. } => "Expand",
            Operator::Filter { .. } => "Filter",
            Operator::CartesianProduct => "CartesianProduct",
            Operator::Projection { .. } => "Projection",
            Operator::Unwind { .. } => "Unwind",
            Operator::Skip { .. } => "Skip",
            Operator::Limit { .. } => "Limit",
            Operator::Union => "Union",
            Operator::SingleRow => "SingleRow",
            Operator::EmptyResult => "EmptyResult",
        }
    }

    /// Operator-specific detail shown after the label in `EXPLAIN`.
    fn detail(&self) -> String {
        match self {
            Operator::AllNodesScan { variable } => format!("({variable})"),
            Operator::NodeByLabelScan { variable, labels } => {
                format!("({variable}{})", render_labels(labels))
            }
            Operator::Expand {
                from,
                rel_variable,
                rel_types,
                direction,
                to_variable,
                to_labels,
            } => render_expand(
                from,
                rel_variable,
                rel_types,
                *direction,
                to_variable,
                to_labels,
            ),
            Operator::Filter { predicate } => predicate.clone(),
            Operator::Projection { columns, distinct } => {
                let prefix = if *distinct { "DISTINCT " } else { "" };
                format!("{prefix}{}", columns.join(", "))
            }
            Operator::Unwind { variable } => format!("AS {variable}"),
            Operator::Skip { count } => count.to_string(),
            Operator::Limit { count } => count.to_string(),
            Operator::CartesianProduct
            | Operator::Union
            | Operator::SingleRow
            | Operator::EmptyResult => String::new(),
        }
    }
}

/// Plan a parsed [`Query`], handling `UNION` by summing the per-arm row
/// estimates under a [`Operator::Union`] node. A single-arm query plans
/// directly to its arm's plan.
pub fn plan_query(query: &Query, stats: &GraphStatistics) -> PlanNode {
    let mut arms: Vec<PlanNode> = query
        .parts
        .iter()
        .map(|part| plan_single_query(&part.query, stats))
        .collect();
    match arms.len() {
        0 => PlanNode::empty_result(),
        1 => arms.remove(0),
        _ => {
            let rows: f64 = arms.iter().map(PlanNode::estimated_rows).sum();
            let cost: f64 = arms.iter().map(PlanNode::estimated_cost).sum::<f64>() + rows;
            PlanNode {
                operator: Operator::Union,
                estimated_rows: rows,
                estimated_cost: cost,
                children: arms,
            }
        }
    }
}

/// Plan a single (`UNION`-free) Cypher query into an annotated [`PlanNode`].
pub fn plan_single_query(query: &SingleQuery, stats: &GraphStatistics) -> PlanNode {
    let mut builder = PlanBuilder {
        estimator: CardinalityEstimator::new(stats),
        bindings: HashMap::new(),
        anon: 0,
    };
    builder.build(query)
}

/// Threads the cardinality estimator and variable→label bindings through the
/// clause walk while building the plan tree.
struct PlanBuilder<'s> {
    estimator: CardinalityEstimator<'s>,
    bindings: HashMap<String, Vec<String>>,
    anon: usize,
}

impl PlanBuilder<'_> {
    fn build(&mut self, query: &SingleQuery) -> PlanNode {
        let mut current: Option<PlanNode> = None;
        for clause in &query.clauses {
            match clause {
                Clause::Match(m) => {
                    let mut plan = current.take();
                    for pattern in &m.patterns {
                        plan = Some(self.plan_path(plan, &pattern.path));
                    }
                    let mut plan = plan.unwrap_or_else(PlanNode::empty_result);
                    if let Some(predicate) = &m.where_clause {
                        plan = self.filter(plan, predicate);
                    }
                    current = Some(plan);
                }
                Clause::Unwind(u) => {
                    let input = current.take().unwrap_or_else(PlanNode::single_row);
                    current = Some(self.unwind(input, &u.alias));
                }
                Clause::With(w) => {
                    let input = current.take().unwrap_or_else(PlanNode::single_row);
                    let plan = self.projection(
                        input,
                        &w.items,
                        w.distinct,
                        w.where_clause.as_ref(),
                        w.skip.as_ref(),
                        w.limit.as_ref(),
                    );
                    current = Some(plan);
                }
                Clause::Return(r) => {
                    let input = current.take().unwrap_or_else(PlanNode::single_row);
                    let plan = self.projection(
                        input,
                        &r.items,
                        r.distinct,
                        None,
                        r.skip.as_ref(),
                        r.limit.as_ref(),
                    );
                    current = Some(plan);
                }
                // Write clauses do not change the read cardinality the planner
                // reasons about in this task; they pass the current plan
                // through unchanged.
                Clause::Create(_)
                | Clause::Merge(_)
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete(_) => {}
            }
        }
        current.unwrap_or_else(PlanNode::empty_result)
    }

    /// Plan one path pattern, extending `current` when present.
    fn plan_path(&mut self, current: Option<PlanNode>, path: &PathPattern) -> PlanNode {
        let (mut plan, mut from_var) = match current {
            None => self.scan_node(&path.head),
            Some(existing) => {
                if self.is_bound(&path.head) {
                    // The head re-uses an already-bound variable: continue
                    // expanding from it without a fresh scan.
                    let var = path.head.variable.clone().unwrap_or_default();
                    (existing, var)
                } else {
                    // A disconnected pattern: cartesian product with a scan.
                    let (scan, var) = self.scan_node(&path.head);
                    (self.cartesian(existing, scan), var)
                }
            }
        };
        for segment in &path.tail {
            let (next, to_var) = self.expand(plan, &from_var, segment);
            plan = next;
            from_var = to_var;
        }
        plan
    }

    /// Build a node scan and register its binding; returns the plan node and
    /// the (possibly synthesised) variable name.
    fn scan_node(&mut self, np: &NodePattern) -> (PlanNode, String) {
        let var = self.name_for(np.variable.as_ref());
        self.bindings.insert(var.clone(), np.labels.clone());
        let rows = self.estimator.estimate_node_scan(&np.labels);
        let operator = if np.labels.is_empty() {
            Operator::AllNodesScan {
                variable: var.clone(),
            }
        } else {
            Operator::NodeByLabelScan {
                variable: var.clone(),
                labels: np.labels.clone(),
            }
        };
        (PlanNode::leaf(operator, rows), var)
    }

    /// Build an expand from `from_var` along `segment`; returns the plan node
    /// and the destination variable name.
    fn expand(
        &mut self,
        input: PlanNode,
        from_var: &str,
        segment: &PathSegment,
    ) -> (PlanNode, String) {
        let to_var = self.name_for(segment.node.variable.as_ref());
        self.bindings
            .insert(to_var.clone(), segment.node.labels.clone());
        let rel = &segment.relationship;
        let rows = self
            .estimator
            .estimate_expand(input.estimated_rows, &rel.types, rel.direction);
        let operator = Operator::Expand {
            from: from_var.to_string(),
            rel_variable: rel.variable.clone(),
            rel_types: rel.types.clone(),
            direction: rel.direction,
            to_variable: to_var.clone(),
            to_labels: segment.node.labels.clone(),
        };
        (PlanNode::unary(operator, rows, rows, input), to_var)
    }

    fn cartesian(&self, left: PlanNode, right: PlanNode) -> PlanNode {
        let rows = left.estimated_rows * right.estimated_rows;
        let cost = left.estimated_cost + right.estimated_cost + rows;
        PlanNode {
            operator: Operator::CartesianProduct,
            estimated_rows: rows,
            estimated_cost: cost,
            children: vec![left, right],
        }
    }

    fn filter(&self, input: PlanNode, predicate: &Expression) -> PlanNode {
        let selectivity = self
            .estimator
            .selectivity_with_bindings(predicate, &self.bindings);
        let rows = input.estimated_rows * selectivity;
        // A filter must examine every input row, so its added cost is the
        // input cardinality.
        let added = input.estimated_rows;
        PlanNode::unary(
            Operator::Filter {
                predicate: describe_expression(predicate),
            },
            rows,
            added,
            input,
        )
    }

    fn unwind(&self, input: PlanNode, variable: &str) -> PlanNode {
        let rows = input.estimated_rows * DEFAULT_UNWIND_LENGTH;
        PlanNode::unary(
            Operator::Unwind {
                variable: variable.to_string(),
            },
            rows,
            rows,
            input,
        )
    }

    fn projection(
        &self,
        input: PlanNode,
        items: &[ProjectionItem],
        distinct: bool,
        where_clause: Option<&Expression>,
        skip: Option<&Expression>,
        limit: Option<&Expression>,
    ) -> PlanNode {
        // A `WITH ... WHERE` filter applies after projection in source order
        // but is most naturally modelled before, against the same bindings.
        let mut plan = input;
        if let Some(predicate) = where_clause {
            plan = self.filter(plan, predicate);
        }
        let columns = items.iter().map(describe_projection).collect();
        let rows = plan.estimated_rows;
        plan = PlanNode::unary(Operator::Projection { columns, distinct }, rows, rows, plan);
        if let Some(count) = literal_count(skip) {
            let rows = (plan.estimated_rows - count as f64).max(0.0);
            plan = PlanNode::unary(Operator::Skip { count }, rows, 0.0, plan);
        }
        if let Some(count) = literal_count(limit) {
            let rows = plan.estimated_rows.min(count as f64);
            plan = PlanNode::unary(Operator::Limit { count }, rows, 0.0, plan);
        }
        plan
    }

    /// Whether `np`'s variable is already bound.
    fn is_bound(&self, np: &NodePattern) -> bool {
        np.variable
            .as_ref()
            .is_some_and(|v| self.bindings.contains_key(v))
    }

    /// Resolve a pattern variable to a name, synthesising a unique anonymous
    /// name when the pattern omits one.
    fn name_for(&mut self, variable: Option<&String>) -> String {
        match variable {
            Some(v) => v.clone(),
            None => {
                let name = format!("anon{}", self.anon);
                self.anon += 1;
                name
            }
        }
    }
}

/// Whether `expr` is a non-negative integer literal usable as a SKIP/LIMIT.
fn literal_count(expr: Option<&Expression>) -> Option<u64> {
    match expr {
        Some(Expression::Integer(n, _)) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

/// Render `:Label1:Label2` (empty for no labels).
fn render_labels(labels: &[String]) -> String {
    labels.iter().map(|l| format!(":{l}")).collect()
}

fn render_expand(
    from: &str,
    rel_variable: &Option<String>,
    rel_types: &[String],
    direction: Direction,
    to_variable: &str,
    to_labels: &[String],
) -> String {
    let rel_var = rel_variable.as_deref().unwrap_or("");
    let types = if rel_types.is_empty() {
        String::new()
    } else {
        format!(":{}", rel_types.join("|"))
    };
    let body = format!("[{rel_var}{types}]");
    let (open, close) = match direction {
        Direction::Outgoing => ("-", "->"),
        Direction::Incoming => ("<-", "-"),
        Direction::Undirected => ("-", "-"),
    };
    format!(
        "({from}){open}{body}{close}({to_variable}{})",
        render_labels(to_labels)
    )
}

/// Render a projection item for the `EXPLAIN` output.
fn describe_projection(item: &ProjectionItem) -> String {
    match item {
        ProjectionItem::Star => "*".to_string(),
        ProjectionItem::Expression { expr, alias } => match alias {
            Some(name) => format!("{} AS {name}", describe_expression(expr)),
            None => describe_expression(expr),
        },
    }
}

/// Render an expression to a compact source-like string for `EXPLAIN`. Covers
/// the common shapes; anything else collapses to `…`.
fn describe_expression(expr: &Expression) -> String {
    match expr {
        Expression::Integer(n, _) => n.to_string(),
        Expression::Float(f, _) => f.to_string(),
        Expression::String(s, _) => format!("'{s}'"),
        Expression::True(_) => "true".to_string(),
        Expression::False(_) => "false".to_string(),
        Expression::Null(_) => "null".to_string(),
        Expression::Variable(v, _) => v.clone(),
        Expression::Parameter(p, _) => format!("${p}"),
        Expression::Property { base, name, .. } => {
            format!("{}.{name}", describe_expression(base))
        }
        Expression::Star(_) => "*".to_string(),
        Expression::List { items, .. } => {
            let parts: Vec<String> = items.iter().map(describe_expression).collect();
            format!("[{}]", parts.join(", "))
        }
        Expression::FunctionCall {
            name,
            args,
            distinct,
            ..
        } => {
            let arg_str: Vec<String> = args.iter().map(describe_expression).collect();
            let prefix = if *distinct { "DISTINCT " } else { "" };
            format!("{}({prefix}{})", name.join("."), arg_str.join(", "))
        }
        Expression::Unary { op, expr, .. } => match op {
            UnaryOp::Not => format!("NOT {}", describe_expression(expr)),
            UnaryOp::Neg => format!("-{}", describe_expression(expr)),
            UnaryOp::Plus => format!("+{}", describe_expression(expr)),
        },
        Expression::Binary { op, lhs, rhs, .. } => {
            format!(
                "{} {} {}",
                describe_expression(lhs),
                binary_op_str(*op),
                describe_expression(rhs)
            )
        }
        Expression::IsNull { expr, negated, .. } => {
            let kw = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("{} {kw}", describe_expression(expr))
        }
        Expression::In { expr, list, .. } => {
            format!(
                "{} IN {}",
                describe_expression(expr),
                describe_expression(list)
            )
        }
        Expression::Map(_)
        | Expression::Index { .. }
        | Expression::Slice { .. }
        | Expression::Case { .. } => "…".to_string(),
    }
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Pow => "^",
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::RegexMatch => "=~",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Xor => "XOR",
        BinaryOp::StartsWith => "STARTS WITH",
        BinaryOp::EndsWith => "ENDS WITH",
        BinaryOp::Contains => "CONTAINS",
    }
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
        s.set_label_count("Task", 200);
        s.set_relationship_type_count("KNOWS", 4000);
        s.set_relationship_type_count("ASSIGNED_TO", 1000);
        s.set_distinct_values("Task", "status", 4);
        s
    }

    fn plan(query: &str) -> PlanNode {
        let ast = parse(query).expect("query parses");
        plan_query(&ast, &stats())
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn all_nodes_scan_estimates_total() {
        let p = plan("MATCH (n) RETURN n");
        // Root is the projection over the scan.
        assert!(matches!(p.operator(), Operator::Projection { .. }));
        let scan = &p.children()[0];
        assert!(matches!(scan.operator(), Operator::AllNodesScan { .. }));
        approx(scan.estimated_rows(), 1000.0);
    }

    #[test]
    fn label_scan_estimates_label_count() {
        let p = plan("MATCH (n:Person) RETURN n");
        let scan = &p.children()[0];
        assert!(matches!(scan.operator(), Operator::NodeByLabelScan { .. }));
        approx(scan.estimated_rows(), 600.0);
    }

    #[test]
    fn expand_builds_on_scan_rows() {
        let p = plan("MATCH (a:Person)-[:KNOWS]->(b) RETURN b");
        // Projection -> Expand -> NodeByLabelScan.
        let expand = &p.children()[0];
        assert!(matches!(expand.operator(), Operator::Expand { .. }));
        // 600 Persons * (4000 KNOWS / 1000 nodes) = 2400.
        approx(expand.estimated_rows(), 2400.0);
        let scan = &expand.children()[0];
        approx(scan.estimated_rows(), 600.0);
    }

    #[test]
    fn filter_reduces_rows_by_selectivity() {
        let p = plan("MATCH (n:Task) WHERE n.status = 'open' RETURN n");
        // Projection -> Filter -> Scan.
        let filter = &p.children()[0];
        assert!(matches!(filter.operator(), Operator::Filter { .. }));
        // 200 Tasks * (1/4 from distinct(Task.status)=4) = 50.
        approx(filter.estimated_rows(), 50.0);
    }

    #[test]
    fn limit_caps_rows() {
        let p = plan("MATCH (n:Person) RETURN n LIMIT 10");
        assert!(matches!(p.operator(), Operator::Limit { count: 10 }));
        approx(p.estimated_rows(), 10.0);
    }

    #[test]
    fn skip_subtracts_rows() {
        let p = plan("MATCH (n:Task) RETURN n SKIP 50");
        assert!(matches!(p.operator(), Operator::Skip { count: 50 }));
        // 200 - 50 = 150.
        approx(p.estimated_rows(), 150.0);
    }

    #[test]
    fn disconnected_patterns_form_cartesian_product() {
        let p = plan("MATCH (a:Person), (b:Task) RETURN a, b");
        let cartesian = &p.children()[0];
        assert!(matches!(cartesian.operator(), Operator::CartesianProduct));
        // 600 * 200 = 120000.
        approx(cartesian.estimated_rows(), 120_000.0);
    }

    #[test]
    fn return_without_match_seeds_single_row() {
        let p = plan("RETURN 1 AS one");
        assert!(matches!(p.operator(), Operator::Projection { .. }));
        approx(p.estimated_rows(), 1.0);
        assert!(matches!(p.children()[0].operator(), Operator::SingleRow));
    }

    #[test]
    fn write_only_query_has_empty_result() {
        let p = plan("CREATE (n:Person {name: 'Ada'})");
        assert!(matches!(p.operator(), Operator::EmptyResult));
        approx(p.estimated_rows(), 0.0);
    }

    #[test]
    fn union_sums_arm_rows() {
        let p = plan("MATCH (n:Person) RETURN n UNION MATCH (n:Task) RETURN n");
        assert!(matches!(p.operator(), Operator::Union));
        // 600 + 200.
        approx(p.estimated_rows(), 800.0);
        assert_eq!(p.children().len(), 2);
    }

    #[test]
    fn cost_increases_down_the_pipeline() {
        let p = plan("MATCH (a:Person)-[:KNOWS]->(b) WHERE b.active = true RETURN b LIMIT 5");
        // The root's cumulative cost must exceed the leaf scan's cost.
        let leaf_cost = {
            let mut node = &p;
            while let Some(child) = node.children().first() {
                node = child;
            }
            node.estimated_cost()
        };
        assert!(p.estimated_cost() > leaf_cost);
    }

    #[test]
    fn explain_renders_indented_tree() {
        let p = plan("MATCH (n:Person) WHERE n.age > 30 RETURN n LIMIT 5");
        let text = p.explain();
        // Root operator on the first line with no indent.
        assert!(text.starts_with("+Limit"), "got:\n{text}");
        // Deeper operators are indented with the guide.
        assert!(text.contains("| +Projection"), "got:\n{text}");
        assert!(text.contains("+Filter n.age > 30"), "got:\n{text}");
        assert!(text.contains("+NodeByLabelScan (n:Person)"), "got:\n{text}");
        // Estimates are present.
        assert!(text.contains("estRows="), "got:\n{text}");
        assert!(text.contains("cost="), "got:\n{text}");
    }

    #[test]
    fn explain_renders_expand_arrow() {
        let p = plan("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN b");
        let text = p.explain();
        assert!(
            text.contains("+Expand (a)-[r:KNOWS]->(b:Person)"),
            "got:\n{text}"
        );
    }

    #[test]
    fn explain_renders_incoming_and_undirected() {
        let incoming = plan("MATCH (a)<-[:ASSIGNED_TO]-(b) RETURN a").explain();
        assert!(incoming.contains("<-[:ASSIGNED_TO]-"), "got:\n{incoming}");
        let undirected = plan("MATCH (a)-[:KNOWS]-(b) RETURN a").explain();
        assert!(undirected.contains(")-[:KNOWS]-("), "got:\n{undirected}");
    }
}
