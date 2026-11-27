//! The steps `EXPLAIN` and `PROFILE` report (issue #868): one line for each
//! thing the executor does with a clause, in the order it does them.
//!
//! The lines follow the executor's own choices. How the first node of a
//! pattern is found is decided the way [`Executor`](super::Executor) decides
//! it: a node bound earlier is taken as it is; `id(n) = ...` is a lookup by
//! id; a property with a known value (`{name: 'Ann'}` or `WHERE n.name =
//! 'Ann'`) is a lookup in the property index when the store has one;
//! otherwise the nodes of the label are scanned, or all of them.

use std::collections::{HashMap, HashSet};

use super::{contains_aggregation, extract_pushdown_constraints, PushConstraint};
use crate::cypher::ast::{
    CallSubquery, Clause, Direction, Expression, NamedPattern, NodePattern, OrderItem,
    ProjectionItem, Query, RelLength, RelationshipPattern, SubqueryImports,
};
use crate::planner::plan::describe_expression;

/// One line of a plan.
pub(super) struct Step {
    /// What is done: `NodeIndexSeek`, `Expand`, `Filter`, `Sort`, ...
    pub operator: &'static str,
    /// What it is done to, in the words of the query.
    pub details: String,
}

/// What the store offers the executor, as far as a plan depends on it.
#[derive(Clone, Copy)]
pub(super) struct Indexes {
    /// The property index answers `n.key = value`.
    pub properties: bool,
    /// The label index answers `(n:Label)`.
    pub labels: bool,
}

/// The variables bound so far while a plan is written, and the indexes.
pub(super) struct Planner {
    indexes: Indexes,
    bound: HashSet<String>,
    depth: usize,
}

impl Planner {
    pub(super) fn new(indexes: Indexes) -> Self {
        Self {
            indexes,
            bound: HashSet::new(),
            depth: 0,
        }
    }

    /// A new `UNION` arm starts with no variables.
    pub(super) fn start_arm(&mut self) {
        self.bound.clear();
    }

    /// The steps of every arm of `query`, a `Union` line between arms.
    pub(super) fn query(&mut self, query: &Query) -> Vec<Step> {
        let mut steps = Vec::new();
        for part in &query.parts {
            if let Some(kind) = part.union {
                steps.push(self.step("Union", format!("{kind:?}").to_uppercase()));
            }
            self.start_arm();
            for clause in &part.query.clauses {
                steps.extend(self.clause(clause));
            }
        }
        steps
    }

    fn step(&self, operator: &'static str, details: impl Into<String>) -> Step {
        Step {
            operator,
            details: format!("{}{}", "  ".repeat(self.depth), details.into()),
        }
    }

    /// The steps of one clause, binding the variables it introduces.
    pub(super) fn clause(&mut self, clause: &Clause) -> Vec<Step> {
        let mut steps = Vec::new();
        match clause {
            Clause::Match(m) => {
                let mut pushed: HashMap<String, Vec<PushConstraint>> = HashMap::new();
                if let Some(predicate) = &m.where_clause {
                    extract_pushdown_constraints(predicate, &mut pushed);
                }
                for pattern in &m.patterns {
                    self.pattern(pattern, m.optional, &pushed, &mut steps);
                }
                if let Some(predicate) = &m.where_clause {
                    steps.push(self.step("Filter", describe_expression(predicate)));
                }
            }
            Clause::With(w) => {
                self.projection(&w.items, w.distinct, &w.order_by, &mut steps);
                self.cut(w.skip.as_ref(), w.limit.as_ref(), &mut steps);
                let all = w.items.iter().any(|i| matches!(i, ProjectionItem::Star));
                let names: Vec<String> = w.items.iter().filter_map(projected_name).collect();
                if !all {
                    self.bound.clear();
                }
                self.bound.extend(names);
                if let Some(predicate) = &w.where_clause {
                    steps.push(self.step("Filter", describe_expression(predicate)));
                }
            }
            Clause::Return(r) => {
                self.projection(&r.items, r.distinct, &r.order_by, &mut steps);
                self.cut(r.skip.as_ref(), r.limit.as_ref(), &mut steps);
            }
            Clause::Unwind(u) => {
                steps.push(self.step(
                    "Unwind",
                    format!("{} AS {}", describe_expression(&u.expression), u.alias),
                ));
                self.bound.insert(u.alias.clone());
            }
            Clause::Create(c) => {
                let patterns: Vec<String> = c.patterns.iter().map(render_pattern).collect();
                steps.push(self.step("Create", patterns.join(", ")));
                for pattern in &c.patterns {
                    self.bind(pattern);
                }
            }
            Clause::Merge(m) => {
                steps.push(self.step("Merge", render_pattern(&m.pattern)));
                self.bind(&m.pattern);
            }
            Clause::Set(s) => {
                steps.push(self.step("SetProperty", format!("{} item(s)", s.items.len())));
            }
            Clause::Remove(r) => {
                steps.push(self.step("Remove", format!("{} item(s)", r.items.len())));
            }
            Clause::Delete(d) => {
                let targets: Vec<String> = d.targets.iter().map(describe_expression).collect();
                let operator = if d.detach { "DetachDelete" } else { "Delete" };
                steps.push(self.step(operator, targets.join(", ")));
            }
            Clause::Foreach(f) => {
                steps.push(self.step(
                    "Foreach",
                    format!(
                        "{} IN {}, {} clause(s)",
                        f.variable,
                        describe_expression(&f.list),
                        f.clauses.len()
                    ),
                ));
            }
            Clause::Call(call) => {
                let yields: Vec<String> = call
                    .yields
                    .iter()
                    .flatten()
                    .map(|y| y.alias.clone().unwrap_or_else(|| y.name.clone()))
                    .collect();
                let mut details = call.name.join(".");
                if !yields.is_empty() {
                    details.push_str(&format!(" YIELD {}", yields.join(", ")));
                }
                steps.push(self.step("ProcedureCall", details));
                self.bound.extend(yields);
                if let Some(predicate) = &call.where_clause {
                    steps.push(self.step("Filter", describe_expression(predicate)));
                }
            }
            Clause::CallSubquery(sub) => self.subquery(sub, &mut steps),
            Clause::Search(s) => {
                steps.push(self.step(
                    "VectorSearch",
                    format!("{} IN {}.{}", s.variable, s.index_label, s.index_property),
                ));
                self.bound.insert(s.variable.clone());
                self.bound.extend(s.score_alias.iter().cloned());
            }
            Clause::CreateVectorIndex(_)
            | Clause::CreatePathIndex(_)
            | Clause::DropIndex(_)
            | Clause::CreateConstraint(_)
            | Clause::DropConstraint(_)
            | Clause::SchemaNoop(_) => steps.push(self.step("SchemaCommand", "")),
            // The parser allows it only as a whole statement.
            Clause::Explain(_) => {}
        }
        steps
    }

    /// The steps of one path: how its first node is found, then one step
    /// for each relationship followed.
    fn pattern(
        &mut self,
        pattern: &NamedPattern,
        optional: bool,
        pushed: &HashMap<String, Vec<PushConstraint>>,
        steps: &mut Vec<Step>,
    ) {
        let head = &pattern.path.head;
        let constraints = head.variable.as_ref().and_then(|v| pushed.get(v));
        let by_id = constraints.is_some_and(|cs| {
            cs.iter()
                .any(|c| matches!(c, PushConstraint::IdEq { .. } | PushConstraint::IdIn { .. }))
        });
        // The properties whose value the query gives, in the pattern or in
        // a WHERE term the executor pushes down.
        let mut keys: Vec<String> = head
            .properties
            .iter()
            .flat_map(|map| map.entries.iter().map(|(key, _)| key.clone()))
            .collect();
        for constraint in constraints.into_iter().flatten() {
            if let PushConstraint::Eq { path, .. }
            | PushConstraint::In { path, .. }
            | PushConstraint::Range { path, .. } = constraint
            {
                if let [key] = path.as_slice() {
                    keys.push(key.clone());
                }
            }
        }
        keys.sort();
        keys.dedup();
        let node = render_node(head);
        let step = match &head.variable {
            Some(variable) if self.bound.contains(variable) => self.step("Argument", node),
            _ if by_id => self.step("NodeByIdSeek", node),
            _ if self.indexes.properties && !keys.is_empty() => {
                self.step("NodeIndexSeek", format!("{node} by {}", keys.join(", ")))
            }
            _ if !head.labels.is_empty() && self.indexes.labels => {
                self.step("NodeByLabelScan", node)
            }
            _ if !head.labels.is_empty() => {
                self.step("AllNodesScan", format!("{node}, kept by label"))
            }
            _ => self.step("AllNodesScan", node),
        };
        steps.push(step);
        let mut from = head.variable.clone().unwrap_or_default();
        for segment in &pattern.path.tail {
            let rel = &segment.relationship;
            let operator = match (pattern.shortest.is_some(), rel.length.is_some(), optional) {
                (true, _, _) => "ShortestPath",
                (false, true, true) => "OptionalVarLengthExpand",
                (false, true, false) => "VarLengthExpand",
                (false, false, true) => "OptionalExpand",
                (false, false, false) => "Expand",
            };
            steps.push(self.step(
                operator,
                format!(
                    "({from}){}{}",
                    render_relationship(rel),
                    render_node(&segment.node)
                ),
            ));
            from = segment.node.variable.clone().unwrap_or_default();
        }
        self.bind(pattern);
    }

    fn projection(
        &self,
        items: &[ProjectionItem],
        distinct: bool,
        order_by: &[OrderItem],
        steps: &mut Vec<Step>,
    ) {
        let text = |item: &ProjectionItem| match item {
            ProjectionItem::Star => "*".to_string(),
            ProjectionItem::Expression { expr, alias, text } => {
                let written = if text.is_empty() {
                    describe_expression(expr)
                } else {
                    text.clone()
                };
                match alias {
                    Some(alias) => format!("{written} AS {alias}"),
                    None => written,
                }
            }
        };
        let aggregates = |item: &&ProjectionItem| matches!(item, ProjectionItem::Expression { expr, .. } if contains_aggregation(expr));
        let join = |items: Vec<&ProjectionItem>| -> String {
            items.into_iter().map(text).collect::<Vec<_>>().join(", ")
        };
        if items.iter().any(|item| aggregates(&item)) {
            let keys = join(items.iter().filter(|i| !aggregates(i)).collect());
            let folds = join(items.iter().filter(aggregates).collect());
            let details = if keys.is_empty() {
                folds
            } else {
                format!("{folds} for each {keys}")
            };
            steps.push(self.step("Aggregation", details));
        } else {
            steps.push(self.step("Projection", join(items.iter().collect())));
        }
        if !order_by.is_empty() {
            let keys: Vec<String> = order_by
                .iter()
                .map(|item| {
                    let direction = format!("{:?}", item.direction).to_uppercase();
                    format!("{} {direction}", describe_expression(&item.expression))
                })
                .collect();
            steps.push(self.step("Sort", keys.join(", ")));
        }
        if distinct {
            steps.push(self.step("Distinct", ""));
        }
    }

    fn cut(&self, skip: Option<&Expression>, limit: Option<&Expression>, steps: &mut Vec<Step>) {
        if let Some(skip) = skip {
            steps.push(self.step("Skip", describe_expression(skip)));
        }
        if let Some(limit) = limit {
            steps.push(self.step("Limit", describe_expression(limit)));
        }
    }

    /// A `Subquery` line, then the steps of its body one level in.
    fn subquery(&mut self, sub: &CallSubquery, steps: &mut Vec<Step>) {
        let imports = match &sub.imports {
            SubqueryImports::All => "imports every variable".to_string(),
            SubqueryImports::Variables(names) if names.is_empty() => "imports nothing".into(),
            SubqueryImports::Variables(names) => format!("imports {}", names.join(", ")),
            SubqueryImports::LeadingWith => "imports through a leading WITH".into(),
        };
        steps.push(self.step("Subquery", format!("once for each row; {imports}")));
        let outer = self.bound.clone();
        let mut returned = Vec::new();
        self.depth += 1;
        for part in &sub.body.parts {
            if let Some(kind) = part.union {
                steps.push(self.step("Union", format!("{kind:?}").to_uppercase()));
            }
            self.bound = match &sub.imports {
                SubqueryImports::Variables(names) => names.iter().cloned().collect(),
                SubqueryImports::All | SubqueryImports::LeadingWith => outer.clone(),
            };
            for clause in &part.query.clauses {
                steps.extend(self.clause(clause));
            }
            if let Some(Clause::Return(r)) = part.query.clauses.last() {
                returned.extend(r.items.iter().filter_map(projected_name));
            }
        }
        self.depth -= 1;
        self.bound = outer;
        self.bound.extend(returned);
    }

    fn bind(&mut self, pattern: &NamedPattern) {
        self.bound.extend(pattern.variable.iter().cloned());
        self.bound
            .extend(pattern.path.head.variable.iter().cloned());
        for segment in &pattern.path.tail {
            self.bound
                .extend(segment.relationship.variable.iter().cloned());
            self.bound.extend(segment.node.variable.iter().cloned());
        }
    }
}

/// The name a projection item gives its column, when it is a plain one.
fn projected_name(item: &ProjectionItem) -> Option<String> {
    match item {
        ProjectionItem::Expression {
            alias: Some(alias), ..
        } => Some(alias.clone()),
        ProjectionItem::Expression {
            expr: Expression::Variable(name, _),
            ..
        } => Some(name.clone()),
        _ => None,
    }
}

fn render_pattern(pattern: &NamedPattern) -> String {
    let mut text = render_node(&pattern.path.head);
    for segment in &pattern.path.tail {
        text.push_str(&render_relationship(&segment.relationship));
        text.push_str(&render_node(&segment.node));
    }
    match &pattern.variable {
        Some(variable) => format!("{variable} = {text}"),
        None => text,
    }
}

fn render_properties(entries: &[(String, Expression)]) -> String {
    let parts: Vec<String> = entries
        .iter()
        .map(|(key, value)| format!("{key}: {}", describe_expression(value)))
        .collect();
    format!(" {{{}}}", parts.join(", "))
}

fn render_node(node: &NodePattern) -> String {
    let mut text = String::from("(");
    text.push_str(node.variable.as_deref().unwrap_or_default());
    match &node.label_expr {
        Some(labels) => text.push_str(&format!(":{labels}")),
        None => {
            for label in &node.labels {
                text.push_str(&format!(":{label}"));
            }
        }
    }
    if let Some(map) = &node.properties {
        text.push_str(&render_properties(&map.entries));
    }
    if let Some(predicate) = &node.predicate {
        text.push_str(&format!(" WHERE {}", describe_expression(predicate)));
    }
    text.push(')');
    text
}

fn render_relationship(rel: &RelationshipPattern) -> String {
    let mut inner = rel.variable.clone().unwrap_or_default();
    if !rel.types.is_empty() {
        inner.push_str(&format!(":{}", rel.types.join("|")));
    }
    match &rel.length {
        None => {}
        Some(RelLength::Any) => inner.push('*'),
        Some(RelLength::Exact(n)) => inner.push_str(&format!("*{n}")),
        Some(RelLength::Range { from, to }) => {
            let bound = |b: &Option<i64>| b.map(|n| n.to_string()).unwrap_or_default();
            inner.push_str(&format!("*{}..{}", bound(from), bound(to)));
        }
    }
    if let Some(map) = &rel.properties {
        inner.push_str(&render_properties(&map.entries));
    }
    if let Some(predicate) = &rel.predicate {
        inner.push_str(&format!(" WHERE {}", describe_expression(predicate)));
    }
    let body = if inner.is_empty() {
        String::new()
    } else {
        format!("[{inner}]")
    };
    match rel.direction {
        Direction::Outgoing => format!("-{body}->"),
        Direction::Incoming => format!("<-{body}-"),
        Direction::Undirected => format!("-{body}-"),
    }
}
