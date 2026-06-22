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
//! [`plan_single_query`] / [`plan_query`] produce the *un-optimised* plan:
//! operators stay in source order. The cost-based optimiser added in task
//! `00086` — [`optimize_single_query`] / [`optimize_query`], also wrapped by
//! [`PlanOptimizer`] — produces a cheaper plan from the same statistics by
//! **anchoring the scan at the most selective node** and expanding outward
//! (pattern reordering), **seeking an index** when a node carries an equality
//! on a statistically-known property ([`Operator::NodeIndexSeek`], index
//! selection), and **ordering disconnected pattern components cheapest-first**
//! (join ordering). Task `00087` adds **supernode handling**: a candidate
//! anchor whose label contains a hub node is scored with the worst-case fan-out
//! of its first hop, so the optimiser drives from the bounded side and expands
//! *into* the hub rather than out of it. Both planners share the [`PlanNode`]
//! representation, the cost annotations, and the [`PlanNode::explain`] rendering
//! the phase's `EXPLAIN`-style output is built on.
//!
//! [`optimize_single_query`]: crate::planner::plan::optimize_single_query
//! [`optimize_query`]: crate::planner::plan::optimize_query
//! [`PlanOptimizer`]: crate::planner::plan::PlanOptimizer

use crate::cypher::ast::{
    BinaryOp, Clause, Direction, Expression, NamedPattern, NodePattern, PathPattern, PathSegment,
    ProjectionItem, Query, RelationshipPattern, SingleQuery, UnaryOp,
};
use crate::planner::cardinality::{CardinalityEstimator, DEFAULT_EQUALITY_SELECTIVITY};
use crate::planner::stats::GraphStatistics;
use std::collections::{HashMap, HashSet};

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
    /// Seek nodes by an equality on a statistically-known property
    /// (`MATCH (n:Label {property: value})`), the index-selection alternative
    /// to a [`NodeByLabelScan`](Operator::NodeByLabelScan) followed by a
    /// [`Filter`](Operator::Filter).
    NodeIndexSeek {
        /// Variable bound to each matched node.
        variable: String,
        /// Label whose property index is used.
        label: String,
        /// Property the equality is on.
        property: String,
        /// Rendered sought value, for the `EXPLAIN` output.
        value: String,
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
            Operator::NodeIndexSeek { .. } => "NodeIndexSeek",
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
            Operator::NodeIndexSeek {
                variable,
                label,
                property,
                value,
            } => format!("({variable}:{label} {{{property} = {value}}})"),
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
///
/// This is the *naive* planner: operators stay in source order. See
/// [`optimize_query`] for the cost-based alternative.
pub fn plan_query(query: &Query, stats: &GraphStatistics) -> PlanNode {
    build_query(query, stats, false)
}

/// Plan a single (`UNION`-free) Cypher query into an annotated [`PlanNode`],
/// keeping operators in source order. See [`optimize_single_query`] for the
/// cost-based alternative.
pub fn plan_single_query(query: &SingleQuery, stats: &GraphStatistics) -> PlanNode {
    build_single_query(query, stats, false)
}

/// Cost-based counterpart to [`plan_query`] (task `00086`): produces a cheaper
/// plan from the same statistics by anchoring scans at the most selective node,
/// seeking property indexes, and ordering disconnected components
/// cheapest-first. `UNION` arms are each optimised independently.
pub fn optimize_query(query: &Query, stats: &GraphStatistics) -> PlanNode {
    build_query(query, stats, true)
}

/// Cost-based counterpart to [`plan_single_query`] (task `00086`). See
/// [`optimize_query`] for the optimisations applied.
pub fn optimize_single_query(query: &SingleQuery, stats: &GraphStatistics) -> PlanNode {
    build_single_query(query, stats, true)
}

/// A cost-based query optimiser bound to a [`GraphStatistics`] snapshot.
///
/// A thin handle over [`optimize_query`] / [`optimize_single_query`] for callers
/// that plan several queries against the same statistics. It holds no mutable
/// state, so a single optimiser can be shared across threads.
#[derive(Debug, Clone, Copy)]
pub struct PlanOptimizer<'s> {
    stats: &'s GraphStatistics,
}

impl<'s> PlanOptimizer<'s> {
    /// Create an optimiser backed by `stats`.
    pub fn new(stats: &'s GraphStatistics) -> Self {
        Self { stats }
    }

    /// The statistics snapshot this optimiser plans against.
    pub fn statistics(&self) -> &GraphStatistics {
        self.stats
    }

    /// Produce a cost-based plan for a parsed [`Query`]. See [`optimize_query`].
    pub fn optimize_query(&self, query: &Query) -> PlanNode {
        optimize_query(query, self.stats)
    }

    /// Produce a cost-based plan for a single [`SingleQuery`]. See
    /// [`optimize_single_query`].
    pub fn optimize_single_query(&self, query: &SingleQuery) -> PlanNode {
        optimize_single_query(query, self.stats)
    }
}

/// Shared `UNION` handling for the naive and optimised planners.
fn build_query(query: &Query, stats: &GraphStatistics, optimize: bool) -> PlanNode {
    let mut arms: Vec<PlanNode> = query
        .parts
        .iter()
        .map(|part| build_single_query(&part.query, stats, optimize))
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

fn build_single_query(query: &SingleQuery, stats: &GraphStatistics, optimize: bool) -> PlanNode {
    let mut builder = PlanBuilder {
        estimator: CardinalityEstimator::new(stats),
        bindings: HashMap::new(),
        anon: 0,
        optimize,
    };
    builder.build(query)
}

/// Threads the cardinality estimator and variable→label bindings through the
/// clause walk while building the plan tree. `optimize` selects the cost-based
/// pattern planning (anchor selection, index seeks, join ordering) over the
/// naive source-order planning.
struct PlanBuilder<'s> {
    estimator: CardinalityEstimator<'s>,
    bindings: HashMap<String, Vec<String>>,
    anon: usize,
    optimize: bool,
}

impl PlanBuilder<'_> {
    fn build(&mut self, query: &SingleQuery) -> PlanNode {
        let mut current: Option<PlanNode> = None;
        for clause in &query.clauses {
            match clause {
                Clause::Match(m) => {
                    let mut plan = if self.optimize {
                        self.plan_match_optimized(current.take(), &m.patterns)
                    } else {
                        let mut plan = current.take();
                        for pattern in &m.patterns {
                            plan = Some(self.plan_path(plan, &pattern.path));
                        }
                        plan.unwrap_or_else(PlanNode::empty_result)
                    };
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
                // `CALL` produces rows from a procedure rather than from a
                // graph scan; the cost model has no procedure statistics, so
                // it passes the current plan through like the write clauses.
                Clause::Create(_)
                | Clause::Merge(_)
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete(_)
                | Clause::Foreach(_)
                | Clause::Call(_) => {}
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

    // ---- Cost-based optimisation (task `00086`) ----

    /// Plan a `MATCH` clause's patterns with cost-based optimisation. Each path
    /// is anchored at its most selective node and expanded outward; when the
    /// clause starts fresh and its patterns share no variables, the independent
    /// components are ordered cheapest-first under a left-deep cartesian product
    /// (join ordering). Otherwise the patterns are planned in source order so
    /// variable continuation across patterns is preserved.
    fn plan_match_optimized(
        &mut self,
        current: Option<PlanNode>,
        patterns: &[NamedPattern],
    ) -> PlanNode {
        if current.is_none() && patterns.len() > 1 && patterns_are_disjoint(patterns) {
            let mut subs: Vec<PlanNode> = patterns
                .iter()
                .map(|p| self.plan_fresh_path_opt(&p.path))
                .collect();
            subs.sort_by(|a, b| {
                a.estimated_rows
                    .partial_cmp(&b.estimated_rows)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut iter = subs.into_iter();
            let mut acc = iter.next().unwrap_or_else(PlanNode::empty_result);
            for sub in iter {
                acc = self.cartesian(acc, sub);
            }
            return acc;
        }
        let mut plan = current;
        for pattern in patterns {
            plan = Some(self.plan_path_opt(plan, &pattern.path));
        }
        plan.unwrap_or_else(PlanNode::empty_result)
    }

    /// Optimised counterpart to [`plan_path`](Self::plan_path): a fresh path is
    /// anchored at its cheapest node; a path whose head re-uses an already-bound
    /// variable continues expanding from it; a disconnected path joins via a
    /// cartesian product.
    fn plan_path_opt(&mut self, current: Option<PlanNode>, path: &PathPattern) -> PlanNode {
        match current {
            None => self.plan_fresh_path_opt(path),
            Some(existing) => {
                if self.is_bound(&path.head) {
                    let from = path.head.variable.clone().unwrap_or_default();
                    self.expand_chain_from_bound(existing, &from, path)
                } else {
                    let fresh = self.plan_fresh_path_opt(path);
                    self.cartesian(existing, fresh)
                }
            }
        }
    }

    /// Plan a fresh (no prior binding) path: pick the most selective node as the
    /// scan/seek anchor, then expand outward — rightward in source direction and
    /// leftward with the relationship directions flipped (the same edges,
    /// traversed backwards).
    fn plan_fresh_path_opt(&mut self, path: &PathPattern) -> PlanNode {
        // 1. Gather node patterns and assign stable variable names + bindings.
        let mut nodes: Vec<&NodePattern> = Vec::with_capacity(path.tail.len() + 1);
        nodes.push(&path.head);
        for seg in &path.tail {
            nodes.push(&seg.node);
        }
        let vars: Vec<String> = nodes
            .iter()
            .map(|np| self.name_for(np.variable.as_ref()))
            .collect();
        for (var, np) in vars.iter().zip(&nodes) {
            self.bindings.insert(var.clone(), np.labels.clone());
        }

        // 2. Score each node by the cheapest cardinality it can be reached at —
        //    an index seek when an equality on a known property is available,
        //    otherwise the label/all-nodes scan. The lowest wins (ties keep the
        //    earliest node, preferring the source-order head).
        let seeks: Vec<Option<IndexSeek>> = nodes
            .iter()
            .map(|np| self.indexed_inline_seek(np))
            .collect();
        // Supernode handling (task `00087`): anchoring at a hub looks cheap by
        // node count, but the first hop then fans out across the hub's entire
        // degree. When the path has an expansion, a candidate whose label
        // contains a supernode is scored with that worst-case fan-out factored
        // in, so the planner drives from the bounded side and expands *into* the
        // hub instead. With no degree statistics the multiplier is 1.0 and the
        // choice is unchanged.
        let has_expansion = !path.tail.is_empty();
        let skew_penalty = self.estimator.statistics().degree_skew().max(1.0);
        let mut anchor = 0usize;
        let mut best = f64::INFINITY;
        for (i, np) in nodes.iter().enumerate() {
            let base = self.estimator.estimate_node_scan(&np.labels);
            let eff = match &seeks[i] {
                Some(seek) => base * seek.selectivity,
                None => base,
            };
            let score = if has_expansion
                && self
                    .estimator
                    .statistics()
                    .any_label_has_supernode(&np.labels)
            {
                eff * skew_penalty
            } else {
                eff
            };
            if score < best {
                best = score;
                anchor = i;
            }
        }

        // 3. Build the anchor operator (seek or scan) + its residual filters.
        let mut plan = self.anchor_node(&vars[anchor], nodes[anchor], seeks[anchor].as_ref());

        // 4. Expand rightward from the anchor (original directions).
        for i in (anchor + 1)..nodes.len() {
            let seg = &path.tail[i - 1];
            let dir = seg.relationship.direction;
            plan = self.expand_between(
                plan,
                &vars[i - 1],
                &seg.relationship,
                dir,
                &vars[i],
                nodes[i],
            );
        }
        // 5. Expand leftward (directions flipped — the edge is traversed back).
        for i in (0..anchor).rev() {
            let seg = &path.tail[i];
            let dir = flip_direction(seg.relationship.direction);
            plan = self.expand_between(
                plan,
                &vars[i + 1],
                &seg.relationship,
                dir,
                &vars[i],
                nodes[i],
            );
        }
        plan
    }

    /// Expand the tail of `path` in source order from an already-bound head; no
    /// anchor reordering is possible because the head is fixed by its binding.
    fn expand_chain_from_bound(
        &mut self,
        plan: PlanNode,
        head_var: &str,
        path: &PathPattern,
    ) -> PlanNode {
        let mut plan = self.apply_inline_filters(plan, head_var, &path.head, None);
        let mut from = head_var.to_string();
        for seg in &path.tail {
            let to_var = self.name_for(seg.node.variable.as_ref());
            self.bindings
                .insert(to_var.clone(), seg.node.labels.clone());
            let dir = seg.relationship.direction;
            plan = self.expand_between(plan, &from, &seg.relationship, dir, &to_var, &seg.node);
            from = to_var;
        }
        plan
    }

    /// Build the anchor operator: a [`NodeIndexSeek`](Operator::NodeIndexSeek)
    /// when an equality on a known property is available, otherwise a scan.
    /// Inline equality properties other than the sought one become filters.
    fn anchor_node(&self, var: &str, np: &NodePattern, seek: Option<&IndexSeek>) -> PlanNode {
        let base = self.estimator.estimate_node_scan(&np.labels);
        let plan = match seek {
            Some(seek) => PlanNode::leaf(
                Operator::NodeIndexSeek {
                    variable: var.to_string(),
                    label: seek.label.clone(),
                    property: seek.property.clone(),
                    value: seek.value.clone(),
                },
                base * seek.selectivity,
            ),
            None => {
                let operator = if np.labels.is_empty() {
                    Operator::AllNodesScan {
                        variable: var.to_string(),
                    }
                } else {
                    Operator::NodeByLabelScan {
                        variable: var.to_string(),
                        labels: np.labels.clone(),
                    }
                };
                PlanNode::leaf(operator, base)
            }
        };
        let skip = seek.map(|s| s.property.as_str());
        self.apply_inline_filters(plan, var, np, skip)
    }

    /// Build an expand operator between two already-named variables, then apply
    /// the destination node's inline equality properties as filters.
    ///
    /// When the source variable is bound to a label known to contain a supernode
    /// (task `00087`), the expansion is costed with the *worst-case* fan-out —
    /// the busiest node's degree — instead of the average, so a plan that drives
    /// out of a hub is honestly more expensive than one that expands into it.
    fn expand_between(
        &self,
        input: PlanNode,
        from_var: &str,
        rel: &RelationshipPattern,
        direction: Direction,
        to_var: &str,
        to_node: &NodePattern,
    ) -> PlanNode {
        let from_is_supernode = self
            .bindings
            .get(from_var)
            .is_some_and(|labels| self.estimator.statistics().any_label_has_supernode(labels));
        let rows = if from_is_supernode {
            self.estimator
                .estimate_expand_worst_case(input.estimated_rows, &rel.types, direction)
        } else {
            self.estimator
                .estimate_expand(input.estimated_rows, &rel.types, direction)
        };
        let operator = Operator::Expand {
            from: from_var.to_string(),
            rel_variable: rel.variable.clone(),
            rel_types: rel.types.clone(),
            direction,
            to_variable: to_var.to_string(),
            to_labels: to_node.labels.clone(),
        };
        let plan = PlanNode::unary(operator, rows, rows, input);
        self.apply_inline_filters(plan, to_var, to_node, None)
    }

    /// Wrap `plan` in one [`Filter`](Operator::Filter) per inline constant
    /// equality property of `np`, skipping the property named in `skip` (the one
    /// already consumed by an index seek). Inline properties are otherwise
    /// invisible to the planner, so this is also where the optimiser accounts
    /// for their selectivity.
    fn apply_inline_filters(
        &self,
        mut plan: PlanNode,
        var: &str,
        np: &NodePattern,
        skip: Option<&str>,
    ) -> PlanNode {
        let Some(map) = &np.properties else {
            return plan;
        };
        for (key, value) in &map.entries {
            if Some(key.as_str()) == skip || !is_constant_value(value) {
                continue;
            }
            let selectivity = self.inline_equality_selectivity(var, key);
            let rows = plan.estimated_rows * selectivity;
            let added = plan.estimated_rows;
            let predicate = format!("{var}.{key} = {}", describe_expression(value));
            plan = PlanNode::unary(Operator::Filter { predicate }, rows, added, plan);
        }
        plan
    }

    /// The most selective inline equality on a statistically-known property of
    /// `np` (the index the optimiser would seek), if any. Among candidates the
    /// rarest value (largest distinct count → smallest selectivity) wins.
    fn indexed_inline_seek(&self, np: &NodePattern) -> Option<IndexSeek> {
        let map = np.properties.as_ref()?;
        let mut best: Option<IndexSeek> = None;
        for (key, value) in &map.entries {
            if !is_constant_value(value) {
                continue;
            }
            for label in &np.labels {
                let Some(distinct) = self.estimator.statistics().distinct_values(label, key) else {
                    continue;
                };
                if distinct == 0 {
                    continue;
                }
                let selectivity = 1.0 / distinct as f64;
                if best.as_ref().is_none_or(|b| selectivity < b.selectivity) {
                    best = Some(IndexSeek {
                        label: label.clone(),
                        property: key.clone(),
                        value: describe_expression(value),
                        selectivity,
                    });
                }
            }
        }
        best
    }

    /// Selectivity of `var.property = <const>` using the catalogue's
    /// distinct-value count (taking the rarest applicable label), or the default
    /// equality selectivity when unknown.
    fn inline_equality_selectivity(&self, var: &str, property: &str) -> f64 {
        let distinct = self.bindings.get(var).and_then(|labels| {
            labels
                .iter()
                .filter_map(|l| self.estimator.statistics().distinct_values(l, property))
                .max()
        });
        match distinct {
            Some(d) if d > 0 => 1.0 / d as f64,
            _ => DEFAULT_EQUALITY_SELECTIVITY,
        }
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

/// A chosen index seek: an equality on `label`.`property` with the given
/// rendered `value` and resulting `selectivity` (`1 / distinct_values`).
struct IndexSeek {
    label: String,
    property: String,
    value: String,
    selectivity: f64,
}

/// Flip an expansion direction — traversing the same edge backwards.
fn flip_direction(direction: Direction) -> Direction {
    match direction {
        Direction::Outgoing => Direction::Incoming,
        Direction::Incoming => Direction::Outgoing,
        Direction::Undirected => Direction::Undirected,
    }
}

/// Whether `patterns` share no named variable, so they may be planned as
/// independent components and reordered. A repeated variable means the patterns
/// are joined and must keep their source order for correct continuation.
fn patterns_are_disjoint(patterns: &[NamedPattern]) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    for pattern in patterns {
        let vars = path_variables(&pattern.path);
        if vars.iter().any(|v| seen.contains(v)) {
            return false;
        }
        seen.extend(vars);
    }
    true
}

/// The set of named node and relationship variables in a path pattern.
fn path_variables(path: &PathPattern) -> HashSet<String> {
    let mut vars = HashSet::new();
    if let Some(v) = &path.head.variable {
        vars.insert(v.clone());
    }
    for seg in &path.tail {
        if let Some(v) = &seg.relationship.variable {
            vars.insert(v.clone());
        }
        if let Some(v) = &seg.node.variable {
            vars.insert(v.clone());
        }
    }
    vars
}

/// Whether `expr` is a constant the optimiser can treat as a fixed value for an
/// equality (a literal or a query parameter).
fn is_constant_value(expr: &Expression) -> bool {
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
        | Expression::Case { .. }
        | Expression::ListComprehension { .. }
        | Expression::ListPredicate { .. }
        | Expression::Reduce { .. }
        | Expression::MapProjection { .. }
        | Expression::PatternComprehension { .. }
        | Expression::PatternPredicate { .. } => "…".to_string(),
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

    // ---- Cost-based optimiser (task `00086`) ----

    /// Optimise a query against the shared test [`stats`].
    fn opt(query: &str) -> PlanNode {
        let ast = parse(query).expect("query parses");
        optimize_query(&ast, &stats())
    }

    /// The deepest left-most leaf of a plan (the driving scan/seek).
    fn driving_leaf(plan: &PlanNode) -> &PlanNode {
        let mut node = plan;
        while let Some(child) = node.children().first() {
            node = child;
        }
        node
    }

    /// Count the scan/seek operators in a plan tree.
    fn scan_count(plan: &PlanNode) -> usize {
        let here = matches!(
            plan.operator(),
            Operator::AllNodesScan { .. }
                | Operator::NodeByLabelScan { .. }
                | Operator::NodeIndexSeek { .. }
        ) as usize;
        here + plan.children().iter().map(scan_count).sum::<usize>()
    }

    #[test]
    fn optimizer_anchors_at_the_more_selective_node() {
        // Naive scans Person (600); the optimiser anchors at the rarer Task
        // (200) and expands back to Person.
        let p = opt("MATCH (a:Person)-[:ASSIGNED_TO]->(b:Task) RETURN a, b");
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeByLabelScan { labels, .. } => assert_eq!(labels, &["Task"]),
            other => panic!("expected NodeByLabelScan(Task), got {other:?}"),
        }
        approx(leaf.estimated_rows(), 200.0);
    }

    #[test]
    fn optimized_plan_is_cheaper_than_naive_for_reorderable_path() {
        let q = "MATCH (a:Person)-[:ASSIGNED_TO]->(b:Task) RETURN a, b";
        let naive = plan(q);
        let optimized = opt(q);
        assert!(
            optimized.estimated_cost() < naive.estimated_cost(),
            "optimized cost {} should beat naive {}",
            optimized.estimated_cost(),
            naive.estimated_cost()
        );
    }

    #[test]
    fn optimizer_reverses_direction_when_expanding_leftward() {
        // Anchoring at Task means the ASSIGNED_TO edge is traversed backwards.
        let text = opt("MATCH (a:Person)-[:ASSIGNED_TO]->(b:Task) RETURN a").explain();
        assert!(text.contains("<-[:ASSIGNED_TO]-"), "got:\n{text}");
    }

    #[test]
    fn optimizer_selects_index_seek_for_inline_equality() {
        let p = opt("MATCH (t:Task {status: 'open'}) RETURN t");
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeIndexSeek {
                label,
                property,
                value,
                ..
            } => {
                assert_eq!(label, "Task");
                assert_eq!(property, "status");
                assert_eq!(value, "'open'");
            }
            other => panic!("expected NodeIndexSeek, got {other:?}"),
        }
        // 200 Tasks / distinct(Task.status)=4 = 50.
        approx(leaf.estimated_rows(), 50.0);
    }

    #[test]
    fn naive_planner_ignores_inline_properties() {
        // Contrast: the naive planner does not seek; it scans every Task.
        let p = plan("MATCH (t:Task {status: 'open'}) RETURN t");
        let leaf = driving_leaf(&p);
        assert!(matches!(leaf.operator(), Operator::NodeByLabelScan { .. }));
        approx(leaf.estimated_rows(), 200.0);
    }

    #[test]
    fn index_seek_does_not_re_filter_the_sought_property() {
        // The sought property must not also appear as a Filter.
        let text = opt("MATCH (t:Task {status: 'open'}) RETURN t").explain();
        assert!(text.contains("+NodeIndexSeek"), "got:\n{text}");
        assert!(!text.contains("+Filter"), "got:\n{text}");
    }

    #[test]
    fn inline_property_on_non_anchor_node_becomes_a_filter() {
        // Task seeks (50 rows) so it anchors; Person.name is unindexed and
        // arrives via expand, so it becomes a default-selectivity filter.
        let p = opt(
            "MATCH (t:Task {status: 'open'})-[:ASSIGNED_TO]->(p:Person {name: 'Ada'}) RETURN p",
        );
        let text = p.explain();
        assert!(text.contains("+NodeIndexSeek"), "got:\n{text}");
        assert!(text.contains("+Filter p.name = 'Ada'"), "got:\n{text}");
    }

    #[test]
    fn optimizer_orders_disconnected_components_cheapest_first() {
        // Naive cartesian puts Person first; the optimiser drives with Task.
        let p = opt("MATCH (a:Person), (b:Task) RETURN a, b");
        // Projection -> CartesianProduct -> [left, right].
        let cartesian = &p.children()[0];
        assert!(matches!(cartesian.operator(), Operator::CartesianProduct));
        let left = &cartesian.children()[0];
        match left.operator() {
            Operator::NodeByLabelScan { labels, .. } => assert_eq!(labels, &["Task"]),
            other => panic!("expected cheaper Task scan on the left, got {other:?}"),
        }
    }

    #[test]
    fn optimizer_preserves_source_order_when_patterns_share_a_variable() {
        // (a)-->(b), (b)-->(c) shares `b`; reordering would break continuation,
        // so the optimiser keeps one driving scan and continues from `b`.
        let p = opt("MATCH (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(c) RETURN c");
        assert_eq!(scan_count(&p), 1, "expected a single driving scan");
    }

    #[test]
    fn trivial_query_optimizes_to_the_same_plan() {
        // Nothing to reorder: the optimised plan equals the naive one.
        let q = "MATCH (n:Person) RETURN n";
        assert_eq!(opt(q), plan(q));
    }

    #[test]
    fn explain_renders_index_seek() {
        let text = opt("MATCH (t:Task {status: 'open'}) RETURN t").explain();
        assert!(
            text.contains("+NodeIndexSeek (t:Task {status = 'open'})"),
            "got:\n{text}"
        );
    }

    #[test]
    fn plan_optimizer_handle_optimizes_each_union_arm() {
        let s = stats();
        let optimizer = PlanOptimizer::new(&s);
        let ast = parse("MATCH (n:Person) RETURN n UNION MATCH (t:Task {status: 'open'}) RETURN t")
            .expect("query parses");
        let p = optimizer.optimize_query(&ast);
        assert!(matches!(p.operator(), Operator::Union));
        // The second arm seeks the Task.status index.
        let second_arm_text = p.children()[1].explain();
        assert!(
            second_arm_text.contains("+NodeIndexSeek"),
            "got:\n{second_arm_text}"
        );
    }

    #[test]
    fn non_indexed_inline_property_uses_default_equality_selectivity() {
        // Person.name has no distinct-value statistic → default 0.1.
        let p = opt("MATCH (p:Person {name: 'Ada'}) RETURN p");
        let filter = &p.children()[0];
        assert!(matches!(filter.operator(), Operator::Filter { .. }));
        // 600 Persons * DEFAULT_EQUALITY_SELECTIVITY (0.1) = 60.
        approx(filter.estimated_rows(), 60.0);
    }

    #[test]
    fn parameter_equality_seeks_the_index() {
        // An equality against a query parameter is still a seekable constant.
        let p = opt("MATCH (t:Task {status: $s}) RETURN t");
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeIndexSeek {
                property, value, ..
            } => {
                assert_eq!(property, "status");
                assert_eq!(value, "$s");
            }
            other => panic!("expected NodeIndexSeek, got {other:?}"),
        }
        approx(leaf.estimated_rows(), 50.0);
    }

    // ---- Supernode handling (task `00087`) ----

    /// Statistics with a `Tag` hub: 9000 Posts, 50 Tags, but one Tag is linked
    /// to almost every node (max degree 9000) while the average degree is low.
    fn supernode_stats() -> GraphStatistics {
        let mut s = GraphStatistics::new()
            .with_total_nodes(9050)
            .with_total_relationships(18_000) // avg degree ≈ 2.
            .with_max_degree(9000);
        s.set_label_count("Post", 9000);
        s.set_label_count("Tag", 50);
        s.set_relationship_type_count("TAGGED", 18_000);
        s.set_max_degree_for_label("Tag", 9000); // the hub
        s.set_max_degree_for_label("Post", 4); // ordinary
        s
    }

    /// Optimise a query against the given statistics.
    fn opt_with(query: &str, stats: &GraphStatistics) -> PlanNode {
        let ast = parse(query).expect("query parses");
        optimize_query(&ast, stats)
    }

    #[test]
    fn optimizer_avoids_anchoring_at_a_supernode() {
        // Tag (50) is rarer than Post (9000), so a count-only optimiser would
        // anchor at Tag — but driving out of the Tag hub fans out across 9000
        // edges. Supernode handling drives from Post and expands into the Tag.
        let s = supernode_stats();
        let p = opt_with("MATCH (p:Post)-[:TAGGED]->(t:Tag) RETURN p, t", &s);
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeByLabelScan { labels, .. } => assert_eq!(labels, &["Post"]),
            other => panic!("expected to drive from Post (avoiding the hub), got {other:?}"),
        }
    }

    #[test]
    fn without_degree_stats_optimizer_anchors_at_the_rarer_label() {
        // The same shape with no degree statistics falls back to pure
        // cardinality and anchors at the rarer Tag — the choice supernode
        // handling overrides above.
        let mut s = GraphStatistics::new()
            .with_total_nodes(9050)
            .with_total_relationships(18_000);
        s.set_label_count("Post", 9000);
        s.set_label_count("Tag", 50);
        s.set_relationship_type_count("TAGGED", 18_000);
        let p = opt_with("MATCH (p:Post)-[:TAGGED]->(t:Tag) RETURN p, t", &s);
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeByLabelScan { labels, .. } => assert_eq!(labels, &["Tag"]),
            other => panic!("expected the rarer Tag anchor without degree stats, got {other:?}"),
        }
    }

    #[test]
    fn driving_into_a_supernode_uses_average_fanout() {
        // When the hub is on the destination side, the expansion is costed with
        // the (cheap) average fan-out — only driving *out* of a hub is penalised.
        let s = supernode_stats();
        let p = opt_with("MATCH (p:Post)-[:TAGGED]->(t:Tag) RETURN p, t", &s);
        // Anchor is Post (9000); the single expand into Tag uses average degree
        // (18000/9050 ≈ 1.99), so the expand row estimate stays near the anchor
        // count rather than exploding to Post × max_degree.
        let expand = {
            let mut node = &p;
            while !matches!(node.operator(), Operator::Expand { .. }) {
                node = &node.children()[0];
            }
            node
        };
        let expected = 9000.0 * (18_000.0 / 9050.0);
        approx(expand.estimated_rows(), expected);
    }

    #[test]
    fn supernode_handling_is_inert_without_an_expansion() {
        // A lone hub scan (no hop) is never penalised — there is nothing to
        // fan out — so the plan matches the plain optimiser.
        let s = supernode_stats();
        let p = opt_with("MATCH (t:Tag) RETURN t", &s);
        let leaf = driving_leaf(&p);
        match leaf.operator() {
            Operator::NodeByLabelScan { labels, .. } => assert_eq!(labels, &["Tag"]),
            other => panic!("expected a plain Tag scan, got {other:?}"),
        }
        approx(leaf.estimated_rows(), 50.0);
    }
}
