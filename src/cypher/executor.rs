//! Cypher executor — Phase 10 task `00063`.
//!
//! The executor consumes the [`crate::cypher::ast::Query`] produced by
//! [`crate::cypher::parser::parse`] and runs it against the underlying
//! [`crate::db::Drevo`] handle. The initial cut (`00063`) targets the
//! README "critical path" prefix — `CREATE`, `MATCH`, `RETURN` — with
//! enough expression evaluation to make `RETURN`, `ORDER BY`, `SKIP`,
//! `LIMIT`, `DISTINCT`, and inline property filters on patterns useful.
//!
//! Out of scope for `00063` (tracked under follow-on Phase 10 tasks):
//!
//! * `WHERE` on `MATCH` (`00065`).
//! * Aggregations (`COUNT`, `SUM`, `COLLECT`, …) (`00066`).
//! * `OPTIONAL MATCH` (`00067`).
//! * `WITH` query pipelining (`00068`).
//! * Variable-length paths (`*1..3`) (`00069`).
//! * Mutations beyond `CREATE`: `SET`, `DELETE`, `MERGE`, `REMOVE`
//!   (`00064`).
//! * `UNWIND` clause and `UNION` queries.
//!
//! Anything in that list surfaces as
//! [`ExecError::Unsupported`](crate::cypher::executor::ExecError::Unsupported)
//! with a pointer to the task that will ship it, so embedders get a
//! deterministic, actionable error rather than silent wrong answers.
//!
//! # Mapping between Cypher and drevo
//!
//! * A Cypher **label** maps to drevo's [`crate::model::Node::kind`].
//!   The first task (`00063`) supports exactly one label per node;
//!   multi-label nodes (`MERGE` / `SET :Label`) land with `00064`.
//! * A Cypher **relationship type** maps to drevo's
//!   [`crate::model::Edge::kind`].
//! * Cypher **properties** round-trip through drevo's
//!   [`crate::model::Properties`] map. The reserved property key
//!   `"title"` aliases [`crate::model::Node::title`] so the storage
//!   layer's title index stays useful from Cypher; if it is absent the
//!   executor synthesises a unique placeholder so multiple unnamed
//!   nodes of the same label don't collide on drevo's title uniqueness
//!   index. The `"body"` key aliases [`crate::model::Node::body`] for
//!   the same reason (the FTS index reads from `title` + `body`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::cypher::ast::{
    BinaryOp, Clause, CreateClause, Direction as AstDirection, Expression, MapLiteral, MatchClause,
    NamedPattern, NodePattern, OrderDirection, OrderItem, PathPattern, ProjectionItem, Query,
    RelLength, RelationshipPattern, ReturnClause, UnaryOp,
};
use crate::cypher::lexer::Span;
use crate::db::Drevo;
use crate::error::DrevoError;
use crate::model::{
    new_uuid_v7, Direction as ModelDirection, Edge, NewEdge, NewNode, Node, Properties,
};

// ===== Public types =========================================================

/// A Cypher runtime value.
///
/// `Node` and `Relationship` are reference-counted so they can be shared
/// cheaply across pattern bindings, ORDER BY keys, and RETURN rows.
#[derive(Debug, Clone)]
pub enum Value {
    /// The Cypher `NULL` literal — also the result of a missing property
    /// or an unresolved optional binding.
    Null,
    /// `TRUE` / `FALSE`.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Ordered list — order is preserved across all operations.
    List(Vec<Value>),
    /// Sorted-by-key map. Deterministic ordering keeps RETURN output
    /// stable across runs, which the parser-e2e suite relies on.
    Map(BTreeMap<String, Value>),
    /// A bound graph node.
    Node(Arc<NodeValue>),
    /// A bound relationship.
    Relationship(Arc<RelationshipValue>),
}

/// A node as seen by the Cypher runtime.
///
/// Carries the labels and properties surfaced by Cypher — the drevo
/// [`Node`] beneath is captured via `id` + `uuid` so callers can look it
/// up again.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeValue {
    /// Auto-increment storage id (matches [`crate::model::Node::id`]).
    pub id: u64,
    /// UUID v7 (matches [`crate::model::Node::uuid`]).
    pub uuid: [u8; 16],
    /// Cypher labels — one entry until `00064` lands multi-label nodes.
    pub labels: Vec<String>,
    /// Property map sourced from [`crate::model::Node::properties`]
    /// plus the synthesised `title` / `body` aliases (see module docs).
    pub properties: BTreeMap<String, Value>,
}

/// A relationship as seen by the Cypher runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipValue {
    /// Auto-increment storage id (matches [`crate::model::Edge::id`]).
    pub id: u64,
    /// UUID v7 (matches [`crate::model::Edge::uuid`]).
    pub uuid: [u8; 16],
    /// Source node id.
    pub from_id: u64,
    /// Target node id.
    pub to_id: u64,
    /// Relationship type (drevo `Edge::kind`).
    pub kind: String,
    /// Property map.
    pub properties: BTreeMap<String, Value>,
}

/// The full result of executing a query.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    /// Column names projected by the trailing `RETURN`. Empty if the
    /// query had no `RETURN`.
    pub columns: Vec<String>,
    /// Result rows, one inner `Vec` per row.
    pub rows: Vec<Vec<Value>>,
    /// Mutation statistics, mirroring Neo4j's `SummaryCounters`.
    pub stats: ExecStats,
}

/// Mutation statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecStats {
    /// Total number of nodes created during this query.
    pub nodes_created: usize,
    /// Total number of relationships created during this query.
    pub relationships_created: usize,
    /// Reserved for `00064` — currently always 0.
    pub properties_set: usize,
    /// Reserved for `00064` — currently always 0.
    pub nodes_deleted: usize,
    /// Reserved for `00064` — currently always 0.
    pub relationships_deleted: usize,
}

/// Errors raised by the executor.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// A clause or expression form is not supported by `00063` yet.
    ///
    /// The `task` field points at the follow-on roadmap task that will
    /// ship the missing feature so callers can present a deterministic
    /// "not yet" message.
    #[error("unsupported Cypher feature `{feature}` — lands with task {task}")]
    Unsupported {
        /// Short human-readable feature label (`"WHERE"`, `"MERGE"`).
        feature: String,
        /// Roadmap task id that will ship the feature (`"00065"`).
        task: String,
        /// Source span of the offending construct.
        span: Span,
    },
    /// A variable referenced in an expression was not bound by a prior
    /// pattern, projection, or parameter.
    #[error("unbound variable `{name}`")]
    UnboundVariable {
        /// Variable name as written in source.
        name: String,
        /// Span of the offending reference.
        span: Span,
    },
    /// A `$name` parameter was used but not provided.
    #[error("missing query parameter `{0}`")]
    MissingParameter(String),
    /// An expression operator received an operand of the wrong type.
    #[error("type mismatch: expected {expected}, got {got}")]
    TypeMismatch {
        /// Description of the expected type(s).
        expected: String,
        /// Description of the type actually received.
        got: String,
        /// Span of the offending operand.
        span: Span,
    },
    /// A `CREATE` pattern is structurally invalid for execution (e.g.
    /// `CREATE (a)-[r]->(b)` without a relationship type).
    #[error("invalid CREATE pattern: {0}")]
    InvalidCreate(String),
    /// Underlying storage / serialization failure.
    #[error("storage error: {0}")]
    Storage(#[from] DrevoError),
}

impl ExecError {
    /// Return the [`Span`] of the offending source construct, when
    /// available.
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Unsupported { span, .. }
            | Self::UnboundVariable { span, .. }
            | Self::TypeMismatch { span, .. } => Some(*span),
            Self::MissingParameter(_) | Self::InvalidCreate(_) | Self::Storage(_) => None,
        }
    }
}

/// Convenience alias for executor results.
pub type ExecResultT<T> = Result<T, ExecError>;

// ===== Value helpers ========================================================

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Integer(a), Self::Integer(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Integer(a), Self::Float(b)) | (Self::Float(b), Self::Integer(a)) => {
                (*a as f64) == *b
            }
            (Self::String(a), Self::String(b)) => a == b,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Map(a), Self::Map(b)) => a == b,
            (Self::Node(a), Self::Node(b)) => a.id == b.id,
            (Self::Relationship(a), Self::Relationship(b)) => a.id == b.id,
            _ => false,
        }
    }
}

impl Value {
    /// Human-readable type tag for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Bool(_) => "Boolean",
            Self::Integer(_) => "Integer",
            Self::Float(_) => "Float",
            Self::String(_) => "String",
            Self::List(_) => "List",
            Self::Map(_) => "Map",
            Self::Node(_) => "Node",
            Self::Relationship(_) => "Relationship",
        }
    }

    fn as_number(&self) -> Option<f64> {
        match self {
            Self::Integer(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    fn as_string(&self, span: Span) -> ExecResultT<&str> {
        match self {
            Self::String(s) => Ok(s.as_str()),
            other => Err(ExecError::TypeMismatch {
                expected: "String".into(),
                got: other.type_name().into(),
                span,
            }),
        }
    }
}

// ===== JSON <-> Value =======================================================

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

fn value_to_json(v: &Value) -> Option<serde_json::Value> {
    Some(match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(value_to_json(item)?);
            }
            serde_json::Value::Array(out)
        }
        Value::Map(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), value_to_json(v)?);
            }
            serde_json::Value::Object(out)
        }
        // Nodes and relationships round-trip as opaque ids — storing a
        // bound node value into a property map would be a programming
        // error and is rejected by InvalidCreate at the call site.
        Value::Node(_) | Value::Relationship(_) => return None,
    })
}

fn node_to_value(node: &Node) -> Arc<NodeValue> {
    let mut properties = BTreeMap::new();
    // Surface `title` and `body` as ordinary properties so Cypher code
    // sees a homogeneous map. The user can override either via the
    // inline map; the executor uses these names as aliases when
    // creating nodes.
    if !node.title.is_empty() {
        properties.insert("title".to_string(), Value::String(node.title.clone()));
    }
    if !node.body.is_empty() {
        properties.insert("body".to_string(), Value::String(node.body.clone()));
    }
    for (k, v) in node.properties.iter() {
        properties.insert(k.clone(), json_to_value(v));
    }
    Arc::new(NodeValue {
        id: node.id,
        uuid: node.uuid,
        labels: vec![node.kind.clone()],
        properties,
    })
}

fn edge_to_value(edge: &Edge) -> Arc<RelationshipValue> {
    let mut properties = BTreeMap::new();
    for (k, v) in edge.properties.iter() {
        properties.insert(k.clone(), json_to_value(v));
    }
    Arc::new(RelationshipValue {
        id: edge.id,
        uuid: edge.uuid,
        from_id: edge.from_id,
        to_id: edge.to_id,
        kind: edge.kind.clone(),
        properties,
    })
}

// ===== Public entry point ===================================================

/// Execute a parsed Cypher [`Query`] against a [`Drevo`] handle.
///
/// `params` provides bindings for `$name` parameters; pass an empty map
/// for queries that don't use them.
///
/// # Errors
///
/// Returns the first executor error encountered (see [`ExecError`]).
/// The executor is fail-fast — no partial result is returned for failed
/// queries. CREATE side effects performed before an error are *not*
/// rolled back; `00064` will introduce explicit transaction boundaries.
pub fn execute(
    query: &Query,
    drevo: &Drevo,
    params: HashMap<String, Value>,
) -> ExecResultT<ExecResult> {
    if query.parts.len() > 1 {
        let span = first_clause_span(&query.parts[1].query.clauses);
        return Err(ExecError::Unsupported {
            feature: "UNION".into(),
            task: "future Phase 10 follow-up".into(),
            span,
        });
    }
    let single = &query.parts[0].query;
    // Upfront sweep — surface unsupported constructs before any side
    // effects run, so a query that would eventually fail on a varlen
    // path or a function call gets the deterministic error even when
    // the underlying graph is empty.
    for clause in &single.clauses {
        validate_clause_supported(clause)?;
    }

    let mut executor = Executor {
        drevo,
        params,
        bindings: vec![HashMap::new()],
        stats: ExecStats::default(),
        result_columns: Vec::new(),
        result_rows: Vec::new(),
    };

    for clause in &single.clauses {
        executor.run_clause(clause)?;
    }

    // The trailing RETURN (if any) populated `result_rows`; if no
    // RETURN was present, we hand back an empty rowset with the stats.
    Ok(executor.take_result())
}

fn validate_clause_supported(clause: &Clause) -> ExecResultT<()> {
    match clause {
        Clause::Match(m) => {
            if m.optional {
                return Err(ExecError::Unsupported {
                    feature: "OPTIONAL MATCH".into(),
                    task: "00067".into(),
                    span: m.span,
                });
            }
            if let Some(expr) = &m.where_clause {
                return Err(ExecError::Unsupported {
                    feature: "WHERE on MATCH".into(),
                    task: "00065".into(),
                    span: expr.span(),
                });
            }
            for pattern in &m.patterns {
                validate_path_supported(&pattern.path, /*creating=*/ false)?;
            }
        }
        Clause::Create(c) => {
            for pattern in &c.patterns {
                validate_path_supported(&pattern.path, /*creating=*/ true)?;
            }
        }
        Clause::Return(r) => {
            for item in &r.items {
                if let ProjectionItem::Expression { expr, .. } = item {
                    validate_expr_supported(expr)?;
                }
            }
            for item in &r.order_by {
                validate_expr_supported(&item.expression)?;
            }
            if let Some(e) = &r.skip {
                validate_expr_supported(e)?;
            }
            if let Some(e) = &r.limit {
                validate_expr_supported(e)?;
            }
        }
        Clause::Merge(m) => {
            return Err(ExecError::Unsupported {
                feature: "MERGE".into(),
                task: "00064".into(),
                span: m.span,
            })
        }
        Clause::Delete(d) => {
            return Err(ExecError::Unsupported {
                feature: "DELETE".into(),
                task: "00064".into(),
                span: d.span,
            })
        }
        Clause::Set(s) => {
            return Err(ExecError::Unsupported {
                feature: "SET".into(),
                task: "00064".into(),
                span: s.span,
            })
        }
        Clause::Remove(r) => {
            return Err(ExecError::Unsupported {
                feature: "REMOVE".into(),
                task: "00064".into(),
                span: r.span,
            })
        }
        Clause::With(w) => {
            return Err(ExecError::Unsupported {
                feature: "WITH".into(),
                task: "00068".into(),
                span: w.span,
            })
        }
        Clause::Unwind(u) => {
            return Err(ExecError::Unsupported {
                feature: "UNWIND".into(),
                task: "future Phase 10 follow-up".into(),
                span: u.span,
            })
        }
    }
    Ok(())
}

fn validate_path_supported(path: &PathPattern, _creating: bool) -> ExecResultT<()> {
    if path.head.labels.len() > 1 {
        return Err(ExecError::Unsupported {
            feature: "multi-label patterns".into(),
            task: "00064".into(),
            span: path.head.span,
        });
    }
    if let Some(map) = &path.head.properties {
        for (_, expr) in &map.entries {
            validate_expr_supported(expr)?;
        }
    }
    for segment in &path.tail {
        let rel = &segment.relationship;
        if rel.length.is_some() && !matches!(rel.length, Some(RelLength::Exact(1))) {
            return Err(ExecError::Unsupported {
                feature: "variable-length paths".into(),
                task: "00069".into(),
                span: rel.span,
            });
        }
        if let Some(map) = &rel.properties {
            for (_, expr) in &map.entries {
                validate_expr_supported(expr)?;
            }
        }
        if segment.node.labels.len() > 1 {
            return Err(ExecError::Unsupported {
                feature: "multi-label patterns".into(),
                task: "00064".into(),
                span: segment.node.span,
            });
        }
        if let Some(map) = &segment.node.properties {
            for (_, expr) in &map.entries {
                validate_expr_supported(expr)?;
            }
        }
    }
    Ok(())
}

fn validate_expr_supported(expr: &Expression) -> ExecResultT<()> {
    match expr {
        Expression::FunctionCall { name, span, .. } => Err(ExecError::Unsupported {
            feature: format!("function call `{}`", name.join(".")),
            task: "00066".into(),
            span: *span,
        }),
        Expression::Case { span, .. } => Err(ExecError::Unsupported {
            feature: "CASE expression".into(),
            task: "future Phase 10 follow-up".into(),
            span: *span,
        }),
        Expression::Star(span) => Err(ExecError::Unsupported {
            feature: "`*` outside `count(*)`".into(),
            task: "00066".into(),
            span: *span,
        }),
        Expression::Index { span, .. } | Expression::Slice { span, .. } => {
            Err(ExecError::Unsupported {
                feature: "list / map indexing".into(),
                task: "future Phase 10 follow-up".into(),
                span: *span,
            })
        }
        Expression::Binary { lhs, rhs, op, span } => {
            if matches!(op, BinaryOp::RegexMatch) {
                return Err(ExecError::Unsupported {
                    feature: "regex match (`=~`)".into(),
                    task: "future Phase 10 follow-up".into(),
                    span: *span,
                });
            }
            validate_expr_supported(lhs)?;
            validate_expr_supported(rhs)?;
            Ok(())
        }
        Expression::Unary { expr, .. } => validate_expr_supported(expr),
        Expression::IsNull { expr, .. } => validate_expr_supported(expr),
        Expression::In { expr, list, .. } => {
            validate_expr_supported(expr)?;
            validate_expr_supported(list)
        }
        Expression::Property { base, .. } => validate_expr_supported(base),
        Expression::List { items, .. } => {
            for item in items {
                validate_expr_supported(item)?;
            }
            Ok(())
        }
        Expression::Map(m) => {
            for (_, expr) in &m.entries {
                validate_expr_supported(expr)?;
            }
            Ok(())
        }
        Expression::Integer(..)
        | Expression::Float(..)
        | Expression::String(..)
        | Expression::True(_)
        | Expression::False(_)
        | Expression::Null(_)
        | Expression::Variable(..)
        | Expression::Parameter(..) => Ok(()),
    }
}

fn first_clause_span(clauses: &[Clause]) -> Span {
    if let Some(c) = clauses.first() {
        match c {
            Clause::Match(m) => m.span,
            Clause::Create(c) => c.span,
            Clause::Merge(m) => m.span,
            Clause::Delete(d) => d.span,
            Clause::Set(s) => s.span,
            Clause::Remove(r) => r.span,
            Clause::With(w) => w.span,
            Clause::Return(r) => r.span,
            Clause::Unwind(u) => u.span,
        }
    } else {
        Span {
            start: 0,
            end: 0,
            line: 0,
            column: 0,
        }
    }
}

// ===== Executor state =======================================================

/// A row of variable bindings — column name to value.
type Bindings = HashMap<String, Value>;

/// A materialised projection row paired with the binding it came from.
/// Used by `RETURN`'s sort step so `ORDER BY` can reach both the
/// projected column names and the original pattern bindings (`n.name`).
type KeyedRows = Vec<(Vec<Value>, Bindings)>;

/// Pre-computed `ORDER BY` sort keys plus the [`KeyedRows`] entry they
/// describe. Used by [`Executor::sort_keyed`].
type SortableRows = Vec<(Vec<(Value, OrderDirection)>, (Vec<Value>, Bindings))>;

struct Executor<'a> {
    drevo: &'a Drevo,
    params: HashMap<String, Value>,
    /// Pattern bindings produced so far. Each `MATCH` multiplies the
    /// binding set; `CREATE` augments every existing binding (or
    /// produces a single empty binding if none exist yet).
    bindings: Vec<Bindings>,
    stats: ExecStats,
    result_columns: Vec<String>,
    result_rows: Vec<Vec<Value>>,
}

// Default initial result fields — Rust struct init helper.
impl<'a> Executor<'a> {
    fn take_result(self) -> ExecResult {
        ExecResult {
            columns: self.result_columns,
            rows: self.result_rows,
            stats: self.stats,
        }
    }

    fn run_clause(&mut self, clause: &Clause) -> ExecResultT<()> {
        match clause {
            Clause::Match(m) => self.run_match(m),
            Clause::Create(c) => self.run_create(c),
            Clause::Return(r) => self.run_return(r),
            Clause::Merge(m) => Err(ExecError::Unsupported {
                feature: "MERGE".into(),
                task: "00064".into(),
                span: m.span,
            }),
            Clause::Delete(d) => Err(ExecError::Unsupported {
                feature: "DELETE".into(),
                task: "00064".into(),
                span: d.span,
            }),
            Clause::Set(s) => Err(ExecError::Unsupported {
                feature: "SET".into(),
                task: "00064".into(),
                span: s.span,
            }),
            Clause::Remove(r) => Err(ExecError::Unsupported {
                feature: "REMOVE".into(),
                task: "00064".into(),
                span: r.span,
            }),
            Clause::With(w) => Err(ExecError::Unsupported {
                feature: "WITH".into(),
                task: "00068".into(),
                span: w.span,
            }),
            Clause::Unwind(u) => Err(ExecError::Unsupported {
                feature: "UNWIND".into(),
                task: "future Phase 10 follow-up".into(),
                span: u.span,
            }),
        }
    }

    // ----- MATCH -----------------------------------------------------------

    fn run_match(&mut self, m: &MatchClause) -> ExecResultT<()> {
        if m.optional {
            return Err(ExecError::Unsupported {
                feature: "OPTIONAL MATCH".into(),
                task: "00067".into(),
                span: m.span,
            });
        }
        if let Some(expr) = &m.where_clause {
            return Err(ExecError::Unsupported {
                feature: "WHERE on MATCH".into(),
                task: "00065".into(),
                span: expr.span(),
            });
        }

        let mut new_bindings: Vec<Bindings> = Vec::new();
        let prior = std::mem::take(&mut self.bindings);
        for existing in prior.into_iter() {
            // The MATCH may contain multiple comma-separated patterns;
            // each one further multiplies the current binding row.
            let mut current = vec![existing];
            for pattern in &m.patterns {
                let mut next = Vec::new();
                for row in current.drain(..) {
                    for produced in self.match_named_pattern(pattern, &row)? {
                        next.push(produced);
                    }
                }
                current = next;
            }
            new_bindings.extend(current);
        }
        self.bindings = new_bindings;
        Ok(())
    }

    fn match_named_pattern(
        &self,
        pattern: &NamedPattern,
        existing: &Bindings,
    ) -> ExecResultT<Vec<Bindings>> {
        if pattern.variable.is_some() {
            return Err(ExecError::Unsupported {
                feature: "named path bindings (`p = (a)-->(b)`)".into(),
                task: "future Phase 10 follow-up".into(),
                span: pattern.path.head.span,
            });
        }
        self.match_path(&pattern.path, existing)
    }

    fn match_path(&self, path: &PathPattern, existing: &Bindings) -> ExecResultT<Vec<Bindings>> {
        let mut rows = self.match_head(&path.head, existing)?;
        for segment in &path.tail {
            let mut next: Vec<Bindings> = Vec::new();
            // Previous endpoint is the node bound by the last completed
            // segment — either the head node or the tail of the
            // previously matched segment. Look it up by traversing the
            // path again from `head` (cheap — pattern lengths are short).
            for row in rows.drain(..) {
                let prev_node = last_bound_node(&row, path, segment_index_for(path, segment))?;
                next.extend(self.match_segment(&prev_node, segment, &row)?);
            }
            rows = next;
        }
        Ok(rows)
    }

    fn match_head(&self, head: &NodePattern, existing: &Bindings) -> ExecResultT<Vec<Bindings>> {
        // If the head's variable is already bound, just verify it
        // matches the requested label/properties — otherwise enumerate.
        if let Some(name) = &head.variable {
            if let Some(value) = existing.get(name) {
                if let Value::Node(nv) = value {
                    if !node_matches_pattern(nv, head, self)? {
                        return Ok(vec![]);
                    }
                    return Ok(vec![existing.clone()]);
                } else {
                    return Err(ExecError::TypeMismatch {
                        expected: "Node".into(),
                        got: value.type_name().into(),
                        span: head.span,
                    });
                }
            }
        }

        let candidates = self.enumerate_nodes(head)?;
        let mut out = Vec::with_capacity(candidates.len());
        for nv in candidates {
            let mut bindings = existing.clone();
            if let Some(name) = &head.variable {
                bindings.insert(name.clone(), Value::Node(nv));
            }
            out.push(bindings);
        }
        Ok(out)
    }

    fn enumerate_nodes(&self, pattern: &NodePattern) -> ExecResultT<Vec<Arc<NodeValue>>> {
        let nodes: Vec<Node> = if let Some(label) = pattern.labels.first() {
            self.drevo.list_nodes_by_kind(label, usize::MAX, 0)?
        } else {
            // No label: scan everything. Until `00065` we only have
            // `list_recent` (limit-bound). usize::MAX is fine — the
            // backend iterators stream lazily.
            self.drevo.list_recent(usize::MAX)?
        };
        let mut out = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let nv = node_to_value(node);
            if !node_matches_pattern(&nv, pattern, self)? {
                continue;
            }
            out.push(nv);
        }
        Ok(out)
    }

    fn match_segment(
        &self,
        prev_node: &Arc<NodeValue>,
        segment: &crate::cypher::ast::PathSegment,
        existing: &Bindings,
    ) -> ExecResultT<Vec<Bindings>> {
        let rel_pattern = &segment.relationship;
        if rel_pattern.length.is_some() && !matches!(rel_pattern.length, Some(RelLength::Exact(1)))
        {
            return Err(ExecError::Unsupported {
                feature: "variable-length paths".into(),
                task: "00069".into(),
                span: rel_pattern.span,
            });
        }
        let dir = rel_pattern.direction;
        let mut out = Vec::new();
        let edges = match dir {
            AstDirection::Outgoing => self
                .drevo
                .edges_of(prev_node.id, ModelDirection::Outgoing)?,
            AstDirection::Incoming => self
                .drevo
                .edges_of(prev_node.id, ModelDirection::Incoming)?,
            AstDirection::Undirected => self.drevo.edges_of(prev_node.id, ModelDirection::Both)?,
        };
        for edge in edges {
            if !edge_matches_pattern(&edge, rel_pattern, self)? {
                continue;
            }
            // Identify the "other" endpoint of this relationship for
            // the segment's target node pattern.
            let other_id = if edge.from_id == prev_node.id {
                edge.to_id
            } else {
                edge.from_id
            };
            // Honour direction: for Outgoing, the source must equal
            // prev_node; for Incoming, the target must equal prev_node.
            match dir {
                AstDirection::Outgoing if edge.from_id != prev_node.id => continue,
                AstDirection::Incoming if edge.to_id != prev_node.id => continue,
                _ => {}
            }
            let target = match self.drevo.get_node(other_id)? {
                Some(n) => node_to_value(&n),
                None => continue,
            };
            if !node_matches_pattern(&target, &segment.node, self)? {
                continue;
            }
            // Confirm previously-bound variables (if any) still agree.
            let mut bindings = existing.clone();
            if let Some(name) = &segment.relationship.variable {
                if let Some(existing_val) = existing.get(name) {
                    if let Value::Relationship(rv) = existing_val {
                        if rv.id != edge.id {
                            continue;
                        }
                    } else {
                        return Err(ExecError::TypeMismatch {
                            expected: "Relationship".into(),
                            got: existing_val.type_name().into(),
                            span: rel_pattern.span,
                        });
                    }
                }
                bindings.insert(name.clone(), Value::Relationship(edge_to_value(&edge)));
            }
            if let Some(name) = &segment.node.variable {
                if let Some(existing_val) = existing.get(name) {
                    if let Value::Node(nv) = existing_val {
                        if nv.id != target.id {
                            continue;
                        }
                    } else {
                        return Err(ExecError::TypeMismatch {
                            expected: "Node".into(),
                            got: existing_val.type_name().into(),
                            span: segment.node.span,
                        });
                    }
                }
                bindings.insert(name.clone(), Value::Node(target.clone()));
            }
            out.push(bindings);
        }
        Ok(out)
    }

    // ----- CREATE ----------------------------------------------------------

    fn run_create(&mut self, c: &CreateClause) -> ExecResultT<()> {
        // CREATE multiplies every existing binding row by the created
        // pattern. If there is no prior MATCH, the executor starts
        // with a single empty row so CREATE works in isolation.
        if self.bindings.is_empty() {
            self.bindings.push(HashMap::new());
        }
        let mut new_bindings = Vec::with_capacity(self.bindings.len());
        for mut row in std::mem::take(&mut self.bindings).into_iter() {
            for pattern in &c.patterns {
                if pattern.variable.is_some() {
                    return Err(ExecError::Unsupported {
                        feature: "named path bindings on CREATE".into(),
                        task: "future Phase 10 follow-up".into(),
                        span: pattern.path.head.span,
                    });
                }
                self.create_path(&pattern.path, &mut row)?;
            }
            new_bindings.push(row);
        }
        self.bindings = new_bindings;
        Ok(())
    }

    fn create_path(&mut self, path: &PathPattern, row: &mut Bindings) -> ExecResultT<()> {
        let head_value = self.ensure_node_for_create(&path.head, row)?;
        let mut prev_node = head_value;
        for segment in &path.tail {
            let target_value = self.ensure_node_for_create(&segment.node, row)?;
            self.create_relationship(&prev_node, &segment.relationship, &target_value, row)?;
            prev_node = target_value;
        }
        Ok(())
    }

    fn ensure_node_for_create(
        &mut self,
        pattern: &NodePattern,
        row: &mut Bindings,
    ) -> ExecResultT<Arc<NodeValue>> {
        // If the node already has a binding (e.g. it was matched in a
        // prior MATCH), reuse it — Cypher CREATE on a bound variable
        // does NOT re-create the node.
        if let Some(name) = &pattern.variable {
            if let Some(value) = row.get(name) {
                return match value {
                    Value::Node(nv) => Ok(nv.clone()),
                    other => Err(ExecError::TypeMismatch {
                        expected: "Node".into(),
                        got: other.type_name().into(),
                        span: pattern.span,
                    }),
                };
            }
        }

        let label = pattern.labels.first().cloned().ok_or_else(|| {
            ExecError::InvalidCreate(
                "CREATE node must have exactly one label (use `(n:Label)`)".into(),
            )
        })?;
        if pattern.labels.len() > 1 {
            return Err(ExecError::Unsupported {
                feature: "multi-label CREATE".into(),
                task: "00064".into(),
                span: pattern.span,
            });
        }

        let mut props = self.eval_map(&pattern.properties, row)?;
        let title = match props.remove("title") {
            Some(Value::String(s)) => s,
            Some(Value::Null) | None => synth_title(&label),
            Some(other) => {
                return Err(ExecError::TypeMismatch {
                    expected: "String".into(),
                    got: other.type_name().into(),
                    span: pattern.span,
                });
            }
        };
        let body = match props.remove("body") {
            Some(Value::String(s)) => s,
            Some(Value::Null) | None => String::new(),
            Some(other) => {
                return Err(ExecError::TypeMismatch {
                    expected: "String".into(),
                    got: other.type_name().into(),
                    span: pattern.span,
                });
            }
        };

        let mut storage_props = std::collections::HashMap::new();
        for (k, v) in props.iter() {
            let json = value_to_json(v).ok_or_else(|| {
                ExecError::InvalidCreate(format!(
                    "cannot store value of type {} as property `{}`",
                    v.type_name(),
                    k
                ))
            })?;
            storage_props.insert(k.clone(), json);
        }

        let new_node = NewNode {
            kind: label,
            title,
            body,
            body_html: String::new(),
            properties: Properties::from(storage_props),
        };
        let stored = self.drevo.create_node(new_node)?;
        self.stats.nodes_created += 1;
        let nv = node_to_value(&stored);
        if let Some(name) = &pattern.variable {
            row.insert(name.clone(), Value::Node(nv.clone()));
        }
        Ok(nv)
    }

    fn create_relationship(
        &mut self,
        from_node: &Arc<NodeValue>,
        rel: &RelationshipPattern,
        to_node: &Arc<NodeValue>,
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        if rel.length.is_some() {
            return Err(ExecError::Unsupported {
                feature: "variable-length CREATE".into(),
                task: "00069".into(),
                span: rel.span,
            });
        }
        if rel.types.len() != 1 {
            return Err(ExecError::InvalidCreate(
                "CREATE relationship requires exactly one type (use `[:TYPE]`)".into(),
            ));
        }
        let (from_id, to_id) = match rel.direction {
            AstDirection::Outgoing => (from_node.id, to_node.id),
            AstDirection::Incoming => (to_node.id, from_node.id),
            AstDirection::Undirected => {
                return Err(ExecError::InvalidCreate(
                    "CREATE relationship must be directed (use `->` or `<-`)".into(),
                ));
            }
        };
        let mut storage_props = std::collections::HashMap::new();
        let rel_props = self.eval_map(&rel.properties, row)?;
        for (k, v) in rel_props.iter() {
            let json = value_to_json(v).ok_or_else(|| {
                ExecError::InvalidCreate(format!(
                    "cannot store value of type {} as property `{}`",
                    v.type_name(),
                    k
                ))
            })?;
            storage_props.insert(k.clone(), json);
        }
        let new_edge = NewEdge {
            from_id,
            to_id,
            kind: rel.types[0].clone(),
            weight: 1.0,
            properties: Properties::from(storage_props),
        };
        let stored = self.drevo.create_edge(new_edge)?;
        self.stats.relationships_created += 1;
        if let Some(name) = &rel.variable {
            row.insert(name.clone(), Value::Relationship(edge_to_value(&stored)));
        }
        Ok(())
    }

    // ----- RETURN ----------------------------------------------------------

    fn run_return(&mut self, r: &ReturnClause) -> ExecResultT<()> {
        // Materialise rows by evaluating each projection over every
        // binding row. Keep a parallel vector of the originating
        // bindings so ORDER BY can reach `n.prop` even when only `prop`
        // is in the projection list.
        let (columns, projections) = self.resolve_projections(&r.items)?;
        let mut keyed: KeyedRows = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let mut row = Vec::with_capacity(projections.len());
            for proj in &projections {
                let value = self.eval(proj, binding)?;
                row.push(value);
            }
            keyed.push((row, binding.clone()));
        }

        if !r.order_by.is_empty() {
            self.sort_keyed(&mut keyed, &r.order_by, &columns)?;
        }

        let mut rows: Vec<Vec<Value>> = keyed.into_iter().map(|(r, _)| r).collect();

        if r.distinct {
            dedup_rows(&mut rows);
        }

        if let Some(skip_expr) = &r.skip {
            let n = self.eval_usize(skip_expr, &HashMap::new())?;
            if n >= rows.len() {
                rows.clear();
            } else {
                rows.drain(..n);
            }
        }
        if let Some(limit_expr) = &r.limit {
            let n = self.eval_usize(limit_expr, &HashMap::new())?;
            if rows.len() > n {
                rows.truncate(n);
            }
        }

        self.result_columns = columns;
        self.result_rows = rows;
        Ok(())
    }

    /// Expand `RETURN *` into the bound variable names; return
    /// (column_names, expression_per_column).
    fn resolve_projections(
        &self,
        items: &[ProjectionItem],
    ) -> ExecResultT<(Vec<String>, Vec<Expression>)> {
        let mut columns = Vec::new();
        let mut projections = Vec::new();
        for item in items {
            match item {
                ProjectionItem::Star => {
                    if let Some(row) = self.bindings.first() {
                        let mut names: Vec<&String> = row.keys().collect();
                        names.sort();
                        for name in names {
                            columns.push(name.clone());
                            projections.push(Expression::Variable(
                                name.clone(),
                                Span {
                                    start: 0,
                                    end: 0,
                                    line: 0,
                                    column: 0,
                                },
                            ));
                        }
                    }
                }
                ProjectionItem::Expression { expr, alias } => {
                    let column = match alias {
                        Some(a) => a.clone(),
                        None => default_column_name(expr),
                    };
                    columns.push(column);
                    projections.push(expr.clone());
                }
            }
        }
        Ok((columns, projections))
    }

    fn sort_keyed(
        &self,
        keyed: &mut KeyedRows,
        order_by: &[OrderItem],
        columns: &[String],
    ) -> ExecResultT<()> {
        // Pre-compute sort keys so the comparator doesn't re-evaluate
        // expressions on every swap. The order-by expression can reach
        // either the original pattern binding (`n.name`) or the
        // projected column (`name AS x ORDER BY x`); compose a single
        // scope that has both.
        let mut sortable: SortableRows = Vec::with_capacity(keyed.len());
        let take = std::mem::take(keyed);
        for (row, binding) in take.into_iter() {
            let mut scope = binding.clone();
            for (col, val) in columns.iter().zip(row.iter()) {
                scope.entry(col.clone()).or_insert_with(|| val.clone());
            }
            let mut key = Vec::with_capacity(order_by.len());
            for item in order_by {
                let value = self.eval(&item.expression, &scope)?;
                key.push((value, item.direction));
            }
            sortable.push((key, (row, binding)));
        }
        sortable.sort_by(|a, b| compare_keys(&a.0, &b.0));
        *keyed = sortable.into_iter().map(|(_, pair)| pair).collect();
        Ok(())
    }

    // ----- Expression evaluation ------------------------------------------

    fn eval(&self, expr: &Expression, row: &Bindings) -> ExecResultT<Value> {
        match expr {
            Expression::Integer(i, _) => Ok(Value::Integer(*i)),
            Expression::Float(f, _) => Ok(Value::Float(*f)),
            Expression::String(s, _) => Ok(Value::String(s.clone())),
            Expression::True(_) => Ok(Value::Bool(true)),
            Expression::False(_) => Ok(Value::Bool(false)),
            Expression::Null(_) => Ok(Value::Null),
            Expression::Variable(name, span) => {
                row.get(name)
                    .cloned()
                    .ok_or_else(|| ExecError::UnboundVariable {
                        name: name.clone(),
                        span: *span,
                    })
            }
            Expression::Parameter(name, _) => self
                .params
                .get(name)
                .cloned()
                .ok_or_else(|| ExecError::MissingParameter(name.clone())),
            Expression::Property { base, name, span } => {
                let base_value = self.eval(base, row)?;
                Ok(get_property(&base_value, name, *span))
            }
            Expression::List { items, .. } => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(item, row)?);
                }
                Ok(Value::List(out))
            }
            Expression::Map(map) => {
                let mut out = BTreeMap::new();
                for (k, v) in &map.entries {
                    out.insert(k.clone(), self.eval(v, row)?);
                }
                Ok(Value::Map(out))
            }
            Expression::Unary { op, expr, span } => {
                let inner = self.eval(expr, row)?;
                eval_unary(*op, inner, *span)
            }
            Expression::Binary { op, lhs, rhs, span } => {
                let l = self.eval(lhs, row)?;
                let r = self.eval(rhs, row)?;
                eval_binary(*op, l, r, *span)
            }
            Expression::IsNull { expr, negated, .. } => {
                let inner = self.eval(expr, row)?;
                let is_null = matches!(inner, Value::Null);
                Ok(Value::Bool(if *negated { !is_null } else { is_null }))
            }
            Expression::In { expr, list, span } => {
                let needle = self.eval(expr, row)?;
                let haystack = self.eval(list, row)?;
                match haystack {
                    Value::List(items) => {
                        let mut saw_null = false;
                        for item in items {
                            if matches!(item, Value::Null) {
                                saw_null = true;
                                continue;
                            }
                            if needle == item {
                                return Ok(Value::Bool(true));
                            }
                        }
                        if saw_null {
                            Ok(Value::Null)
                        } else {
                            Ok(Value::Bool(false))
                        }
                    }
                    Value::Null => Ok(Value::Null),
                    other => Err(ExecError::TypeMismatch {
                        expected: "List".into(),
                        got: other.type_name().into(),
                        span: *span,
                    }),
                }
            }
            Expression::FunctionCall { name, span, .. } => Err(ExecError::Unsupported {
                feature: format!("function call `{}`", name.join(".")),
                task: "00066".into(),
                span: *span,
            }),
            Expression::Case { span, .. } => Err(ExecError::Unsupported {
                feature: "CASE expression".into(),
                task: "future Phase 10 follow-up".into(),
                span: *span,
            }),
            Expression::Star(span) => Err(ExecError::Unsupported {
                feature: "`*` outside `count(*)`".into(),
                task: "00066".into(),
                span: *span,
            }),
            Expression::Index { span, .. } | Expression::Slice { span, .. } => {
                Err(ExecError::Unsupported {
                    feature: "list / map indexing".into(),
                    task: "future Phase 10 follow-up".into(),
                    span: *span,
                })
            }
        }
    }

    fn eval_usize(&self, expr: &Expression, row: &Bindings) -> ExecResultT<usize> {
        let value = self.eval(expr, row)?;
        match value {
            Value::Integer(i) if i >= 0 => Ok(i as usize),
            Value::Integer(_) => Err(ExecError::TypeMismatch {
                expected: "non-negative Integer".into(),
                got: "negative Integer".into(),
                span: expr.span(),
            }),
            other => Err(ExecError::TypeMismatch {
                expected: "Integer".into(),
                got: other.type_name().into(),
                span: expr.span(),
            }),
        }
    }

    fn eval_map(
        &self,
        map: &Option<MapLiteral>,
        row: &Bindings,
    ) -> ExecResultT<BTreeMap<String, Value>> {
        let mut out = BTreeMap::new();
        if let Some(map) = map {
            for (k, expr) in &map.entries {
                out.insert(k.clone(), self.eval(expr, row)?);
            }
        }
        Ok(out)
    }
}

// ===== Pure helpers =========================================================

fn synth_title(label: &str) -> String {
    // UUID-based suffix keeps drevo's title uniqueness invariant while
    // staying deterministic for the storage layer's title index.
    let uuid = uuid::Uuid::from_bytes(new_uuid_v7());
    format!("__cypher__:{}:{}", label, uuid.as_simple())
}

fn segment_index_for(path: &PathPattern, segment: &crate::cypher::ast::PathSegment) -> usize {
    for (i, s) in path.tail.iter().enumerate() {
        if std::ptr::eq(s, segment) {
            return i;
        }
    }
    0
}

fn last_bound_node(
    row: &Bindings,
    path: &PathPattern,
    segment_idx: usize,
) -> ExecResultT<Arc<NodeValue>> {
    // For segment_idx=0 the predecessor is `path.head`; otherwise it's
    // the destination of the previous segment.
    let target_pattern = if segment_idx == 0 {
        &path.head
    } else {
        &path.tail[segment_idx - 1].node
    };
    if let Some(name) = &target_pattern.variable {
        if let Some(Value::Node(nv)) = row.get(name) {
            return Ok(nv.clone());
        }
    }
    // Anonymous pattern — find the most recent node value we did bind.
    // This is fine for chains of size 1 since `match_head` always emits
    // at least one row for anonymous variables; longer chains require
    // a variable name and are guarded above.
    Err(ExecError::InvalidCreate(
        "internal: anonymous intermediate node in multi-hop path".into(),
    ))
}

fn node_matches_pattern(
    nv: &Arc<NodeValue>,
    pattern: &NodePattern,
    executor: &Executor<'_>,
) -> ExecResultT<bool> {
    // Labels: drevo nodes carry one label today; the pattern must
    // match it exactly or omit labels altogether.
    if let Some(label) = pattern.labels.first() {
        if !nv.labels.iter().any(|l| l == label) {
            return Ok(false);
        }
    }
    if pattern.labels.len() > 1 {
        return Err(ExecError::Unsupported {
            feature: "multi-label MATCH".into(),
            task: "00064".into(),
            span: pattern.span,
        });
    }
    if let Some(map) = &pattern.properties {
        for (k, expr) in &map.entries {
            let expected = executor.eval(expr, &HashMap::new())?;
            let actual = nv.properties.get(k).cloned().unwrap_or(Value::Null);
            if actual != expected {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn edge_matches_pattern(
    edge: &Edge,
    pattern: &RelationshipPattern,
    executor: &Executor<'_>,
) -> ExecResultT<bool> {
    if !pattern.types.is_empty() && !pattern.types.iter().any(|t| t == &edge.kind) {
        return Ok(false);
    }
    if let Some(map) = &pattern.properties {
        let rv = edge_to_value(edge);
        for (k, expr) in &map.entries {
            let expected = executor.eval(expr, &HashMap::new())?;
            let actual = rv.properties.get(k).cloned().unwrap_or(Value::Null);
            if actual != expected {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn get_property(base: &Value, name: &str, _span: Span) -> Value {
    match base {
        Value::Null => Value::Null,
        Value::Node(nv) => nv.properties.get(name).cloned().unwrap_or(Value::Null),
        Value::Relationship(rv) => rv.properties.get(name).cloned().unwrap_or(Value::Null),
        Value::Map(map) => map.get(name).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn default_column_name(expr: &Expression) -> String {
    match expr {
        Expression::Variable(name, _) => name.clone(),
        Expression::Property { base, name, .. } => match base.as_ref() {
            Expression::Variable(var, _) => format!("{}.{}", var, name),
            _ => format!("{}.{}", default_column_name(base), name),
        },
        Expression::Parameter(name, _) => format!("${}", name),
        Expression::Integer(i, _) => i.to_string(),
        Expression::Float(f, _) => f.to_string(),
        Expression::String(s, _) => format!("\"{}\"", s),
        Expression::True(_) => "true".into(),
        Expression::False(_) => "false".into(),
        Expression::Null(_) => "NULL".into(),
        _ => "expr".into(),
    }
}

fn compare_keys(
    a: &[(Value, OrderDirection)],
    b: &[(Value, OrderDirection)],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for ((av, dir), (bv, _)) in a.iter().zip(b.iter()) {
        let ord = compare_values(av, bv);
        if ord != Ordering::Equal {
            return if matches!(dir, OrderDirection::Asc) {
                ord
            } else {
                ord.reverse()
            };
        }
    }
    Ordering::Equal
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Integer(a), Value::Float(b)) => {
            (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal)
        }
        (Value::Float(a), Value::Integer(b)) => {
            a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal)
        }
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::List(a), Value::List(b)) => {
            for (x, y) in a.iter().zip(b.iter()) {
                let ord = compare_values(x, y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Node(a), Value::Node(b)) => a.id.cmp(&b.id),
        (Value::Relationship(a), Value::Relationship(b)) => a.id.cmp(&b.id),
        _ => Ordering::Equal,
    }
}

fn dedup_rows(rows: &mut Vec<Vec<Value>>) {
    let mut seen: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    let mut out = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !seen.iter().any(|s| s == &row) {
            seen.push(row.clone());
            out.push(row);
        }
    }
    *rows = out;
}

fn eval_unary(op: UnaryOp, value: Value, span: Span) -> ExecResultT<Value> {
    match op {
        UnaryOp::Neg => match value {
            Value::Integer(i) => Ok(Value::Integer(-i)),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Null => Ok(Value::Null),
            other => Err(ExecError::TypeMismatch {
                expected: "Integer or Float".into(),
                got: other.type_name().into(),
                span,
            }),
        },
        UnaryOp::Plus => match value {
            v @ Value::Integer(_) | v @ Value::Float(_) | v @ Value::Null => Ok(v),
            other => Err(ExecError::TypeMismatch {
                expected: "Integer or Float".into(),
                got: other.type_name().into(),
                span,
            }),
        },
        UnaryOp::Not => match value {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            Value::Null => Ok(Value::Null),
            other => Err(ExecError::TypeMismatch {
                expected: "Boolean".into(),
                got: other.type_name().into(),
                span,
            }),
        },
    }
}

fn eval_binary(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    // Three-valued logic: NULL propagates through every operator except
    // equality (where NULL = anything yields NULL, not FALSE) and the
    // boolean short-circuits AND/OR/XOR which have explicit truth tables.
    use BinaryOp::*;
    match op {
        Add | Sub | Mul | Div | Mod | Pow => arith(op, lhs, rhs, span),
        Eq | Ne | Lt | Le | Gt | Ge => compare(op, lhs, rhs, span),
        And | Or | Xor => boolean(op, lhs, rhs, span),
        StartsWith | EndsWith | Contains => string_test(op, lhs, rhs, span),
        RegexMatch => Err(ExecError::Unsupported {
            feature: "regex match (`=~`)".into(),
            task: "future Phase 10 follow-up".into(),
            span,
        }),
    }
}

fn arith(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }
    if op == BinaryOp::Add {
        if let (Value::String(a), Value::String(b)) = (&lhs, &rhs) {
            return Ok(Value::String(format!("{}{}", a, b)));
        }
        if let (Value::List(mut a), Value::List(b)) = (lhs.clone(), rhs.clone()) {
            a.extend(b);
            return Ok(Value::List(a));
        }
    }
    let a = lhs.as_number();
    let b = rhs.as_number();
    let (Some(a), Some(b)) = (a, b) else {
        return Err(ExecError::TypeMismatch {
            expected: "numeric operands".into(),
            got: format!("{} {}", lhs.type_name(), rhs.type_name()),
            span,
        });
    };
    let both_int = matches!(lhs, Value::Integer(_)) && matches!(rhs, Value::Integer(_));
    let result = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        BinaryOp::Div => {
            if both_int {
                // Cypher division on integers is integer division
                // when both operands are integers — matches Neo4j.
                if b == 0.0 {
                    return Err(ExecError::TypeMismatch {
                        expected: "non-zero divisor".into(),
                        got: "zero".into(),
                        span,
                    });
                }
                return Ok(Value::Integer((a as i64) / (b as i64)));
            } else {
                a / b
            }
        }
        BinaryOp::Mod => a.rem_euclid(b),
        BinaryOp::Pow => a.powf(b),
        _ => unreachable!(),
    };
    if both_int
        && matches!(
            op,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Mod
        )
    {
        Ok(Value::Integer(result as i64))
    } else {
        Ok(Value::Float(result))
    }
}

fn compare(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }
    let ord_opt = match (&lhs, &rhs) {
        (Value::Integer(a), Value::Integer(b)) => Some(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Integer(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Integer(b)) => a.partial_cmp(&(*b as f64)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Node(a), Value::Node(b)) => Some(a.id.cmp(&b.id)),
        (Value::Relationship(a), Value::Relationship(b)) => Some(a.id.cmp(&b.id)),
        _ => None,
    };
    if op == BinaryOp::Eq {
        return Ok(Value::Bool(lhs == rhs));
    }
    if op == BinaryOp::Ne {
        return Ok(Value::Bool(lhs != rhs));
    }
    let Some(ord) = ord_opt else {
        return Err(ExecError::TypeMismatch {
            expected: "comparable types".into(),
            got: format!("{} vs {}", lhs.type_name(), rhs.type_name()),
            span,
        });
    };
    use std::cmp::Ordering::*;
    let result = match op {
        BinaryOp::Lt => ord == Less,
        BinaryOp::Le => ord != Greater,
        BinaryOp::Gt => ord == Greater,
        BinaryOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(Value::Bool(result))
}

fn boolean(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    let l = match lhs {
        Value::Bool(b) => Some(b),
        Value::Null => None,
        other => {
            return Err(ExecError::TypeMismatch {
                expected: "Boolean".into(),
                got: other.type_name().into(),
                span,
            })
        }
    };
    let r = match rhs {
        Value::Bool(b) => Some(b),
        Value::Null => None,
        other => {
            return Err(ExecError::TypeMismatch {
                expected: "Boolean".into(),
                got: other.type_name().into(),
                span,
            })
        }
    };
    Ok(match op {
        BinaryOp::And => match (l, r) {
            (Some(true), Some(true)) => Value::Bool(true),
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        BinaryOp::Or => match (l, r) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        BinaryOp::Xor => match (l, r) {
            (Some(a), Some(b)) => Value::Bool(a != b),
            _ => Value::Null,
        },
        _ => unreachable!(),
    })
}

fn string_test(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }
    let needle = rhs.as_string(span)?.to_string();
    let haystack = lhs.as_string(span)?.to_string();
    Ok(Value::Bool(match op {
        BinaryOp::StartsWith => haystack.starts_with(&needle),
        BinaryOp::EndsWith => haystack.ends_with(&needle),
        BinaryOp::Contains => haystack.contains(&needle),
        _ => unreachable!(),
    }))
}

// ===== Unit tests ===========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;
    use crate::db::Drevo;

    fn drevo() -> Drevo {
        Drevo::open_in_memory().expect("open in-memory")
    }

    fn run(source: &str, db: &Drevo) -> ExecResult {
        run_with_params(source, db, HashMap::new())
    }

    fn run_with_params(source: &str, db: &Drevo, params: HashMap<String, Value>) -> ExecResult {
        let query = parse(source).expect("parse");
        execute(&query, db, params).expect("execute")
    }

    fn err(source: &str, db: &Drevo) -> ExecError {
        let query = parse(source).expect("parse");
        execute(&query, db, HashMap::new()).expect_err("expected execution error")
    }

    // ---- CREATE -----------------------------------------------------------

    #[test]
    fn create_single_node_persists_label_and_properties() {
        let db = drevo();
        let res = run("CREATE (n:Person {name: 'Alice', age: 30})", &db);
        assert_eq!(res.stats.nodes_created, 1);
        assert!(res.columns.is_empty());
        let persisted = db.list_nodes_by_kind("Person", 10, 0).unwrap();
        assert_eq!(persisted.len(), 1);
        let n = &persisted[0];
        assert_eq!(n.properties.get("name").unwrap().as_str(), Some("Alice"));
        assert_eq!(n.properties.get("age").unwrap().as_i64(), Some(30));
    }

    #[test]
    fn create_then_return_yields_node_value() {
        let db = drevo();
        let res = run(
            "CREATE (n:Note {title: 'Hello', body: 'World'}) RETURN n.title AS title, n.body AS body",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 1);
        assert_eq!(res.columns, vec!["title", "body"]);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Hello".into()));
        assert_eq!(res.rows[0][1], Value::String("World".into()));
    }

    #[test]
    fn create_multiple_unnamed_nodes_does_not_collide_on_title() {
        let db = drevo();
        let res = run("CREATE (:Person), (:Person), (:Person)", &db);
        assert_eq!(res.stats.nodes_created, 3);
        assert_eq!(db.list_nodes_by_kind("Person", 100, 0).unwrap().len(), 3);
    }

    #[test]
    fn create_relationship_between_two_fresh_nodes() {
        let db = drevo();
        let res = run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS {since: 2020}]->(b:Person {name: 'B'})",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 2);
        assert_eq!(res.stats.relationships_created, 1);
        let people = db.list_nodes_by_kind("Person", 10, 0).unwrap();
        assert_eq!(people.len(), 2);
    }

    #[test]
    fn create_relationship_requires_direction() {
        let db = drevo();
        let e = err(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]-(b:Person {name: 'B'})",
            &db,
        );
        assert!(matches!(e, ExecError::InvalidCreate(_)), "got {:?}", e);
    }

    // ---- MATCH ------------------------------------------------------------

    #[test]
    fn match_by_label_returns_all_nodes_of_that_kind() {
        let db = drevo();
        run(
            "CREATE (:Person {name: 'A'}), (:Person {name: 'B'}), (:Animal {name: 'C'})",
            &db,
        );
        let res = run("MATCH (n:Person) RETURN n.name AS name", &db);
        let mut names: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn match_with_inline_property_filters() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        run("CREATE (:Person {name: 'B', age: 40})", &db);
        let res = run("MATCH (n:Person {age: 30}) RETURN n.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn match_single_hop_relationship() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &db,
        );
        let res = run(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS src, b.name AS dst",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::String("B".into()));
    }

    #[test]
    fn match_then_create_extends_existing_node() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run(
            "MATCH (a:Person {name: 'A'}) CREATE (a)-[:HAS_PET]->(p:Animal {name: 'Rex'})",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 1);
        assert_eq!(res.stats.relationships_created, 1);
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 1);
        assert_eq!(db.list_nodes_by_kind("Animal", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn match_anonymous_target_label_filters_pairs() {
        let db = drevo();
        run(
            "CREATE (a:Thought {title: 'T1'})-[:HAS_DISTORTION]->(d:Distortion {kind: 'catastrophizing'})",
            &db,
        );
        run(
            "CREATE (a:Thought {title: 'T2'})-[:HAS_DISTORTION]->(d:Distortion {kind: 'mind_reading'})",
            &db,
        );
        let res = run(
            "MATCH (t:Thought)-[:HAS_DISTORTION]->(:Distortion {kind: 'mind_reading'}) RETURN t.title AS title",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("T2".into()));
    }

    // ---- RETURN -----------------------------------------------------------

    #[test]
    fn return_order_by_skip_limit() {
        let db = drevo();
        for name in ["a", "b", "c", "d", "e"] {
            run(&format!("CREATE (:Person {{name: '{}'}})", name), &db);
        }
        let res = run(
            "MATCH (n:Person) RETURN n.name AS name ORDER BY n.name DESC SKIP 1 LIMIT 2",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("d".into()));
        assert_eq!(res.rows[1][0], Value::String("c".into()));
    }

    #[test]
    fn return_distinct_dedupes_rows() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', team: 'red'})", &db);
        run("CREATE (:Person {name: 'B', team: 'red'})", &db);
        run("CREATE (:Person {name: 'C', team: 'blue'})", &db);
        let res = run("MATCH (n:Person) RETURN DISTINCT n.team AS team", &db);
        let mut teams: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        teams.sort();
        assert_eq!(teams, vec!["blue".to_string(), "red".to_string()]);
    }

    #[test]
    fn return_arithmetic_and_comparison() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        let res = run(
            "MATCH (n:Person) RETURN n.age + 5 AS older, n.age > 18 AS adult",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Integer(35));
        assert_eq!(res.rows[0][1], Value::Bool(true));
    }

    #[test]
    fn return_parameter_value() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let mut params = HashMap::new();
        params.insert("name".to_string(), Value::String("A".into()));
        let q = parse("MATCH (n:Person {name: $name}) RETURN n.name AS name").unwrap();
        let res = execute(&q, &db, params).unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn return_is_null_propagates_three_valued_logic() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run(
            "MATCH (n:Person) RETURN n.missing IS NULL AS missing, n.name IS NOT NULL AS named",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Bool(true));
        assert_eq!(res.rows[0][1], Value::Bool(true));
    }

    #[test]
    fn return_star_projects_all_bound_variables() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run("MATCH (n:Person) RETURN *", &db);
        assert_eq!(res.columns, vec!["n"]);
        assert_eq!(res.rows.len(), 1);
    }

    // ---- Errors -----------------------------------------------------------

    #[test]
    fn where_clause_is_rejected_until_00065() {
        let db = drevo();
        let e = err("MATCH (n:Person) WHERE n.age > 18 RETURN n", &db);
        match e {
            ExecError::Unsupported { feature, task, .. } => {
                assert!(feature.contains("WHERE"));
                assert_eq!(task, "00065");
            }
            other => panic!("expected Unsupported, got {:?}", other),
        }
    }

    #[test]
    fn merge_is_rejected_until_00064() {
        let db = drevo();
        let e = err("MERGE (n:Person {name: 'A'})", &db);
        assert!(
            matches!(e, ExecError::Unsupported { ref task, .. } if task == "00064"),
            "got {:?}",
            e
        );
    }

    #[test]
    fn unbound_variable_reports_name() {
        let db = drevo();
        let e = err("RETURN unknown", &db);
        match e {
            ExecError::UnboundVariable { name, .. } => assert_eq!(name, "unknown"),
            other => panic!("expected UnboundVariable, got {:?}", other),
        }
    }

    #[test]
    fn missing_parameter_is_reported() {
        let db = drevo();
        let q = parse("RETURN $missing AS m").unwrap();
        let e = execute(&q, &db, HashMap::new()).expect_err("expected error");
        assert!(matches!(e, ExecError::MissingParameter(name) if name == "missing"));
    }

    #[test]
    fn variable_length_path_is_rejected_until_00069() {
        let db = drevo();
        let e = err("MATCH (a)-[*1..3]->(b) RETURN a", &db);
        assert!(
            matches!(e, ExecError::Unsupported { ref task, .. } if task == "00069"),
            "got {:?}",
            e
        );
    }

    #[test]
    fn function_call_is_rejected_until_00066() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let e = err("MATCH (n:Person) RETURN count(n)", &db);
        assert!(
            matches!(e, ExecError::Unsupported { ref task, .. } if task == "00066"),
            "got {:?}",
            e
        );
    }
}
