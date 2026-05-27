//! Cypher executor — Phase 10 tasks `00063` (CREATE / MATCH / RETURN)
//! and `00064` (mutations: SET, DELETE, MERGE, REMOVE).
//!
//! The executor consumes the [`crate::cypher::ast::Query`] produced by
//! [`crate::cypher::parser::parse`] and runs it against the underlying
//! [`crate::db::Drevo`] handle. The current cut covers the README
//! "critical path" prefix — `CREATE`, `MATCH`, `RETURN`, plus the full
//! mutation surface — with enough expression evaluation to make
//! `RETURN`, `ORDER BY`, `SKIP`, `LIMIT`, `DISTINCT`, inline property
//! filters, `SET` / `REMOVE` / `DELETE` (incl. `DETACH DELETE`) and
//! `MERGE` (with `ON CREATE` / `ON MATCH` actions) useful.
//!
//! Out of scope (tracked under follow-on Phase 10 tasks):
//!
//! * `WHERE` on `MATCH` (`00065`).
//! * Aggregations (`COUNT`, `SUM`, `COLLECT`, …) (`00066`).
//! * `OPTIONAL MATCH` (`00067`).
//! * `WITH` query pipelining (`00068`).
//! * Variable-length paths (`*1..3`) (`00069`).
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
//!   The primary label (drevo `kind`) is the one used by the kind
//!   index. Additional labels added via `SET n:Label` live in a
//!   reserved `_labels` property (a JSON array of strings); they are
//!   surfaced as part of the node's `labels` set but are not part of
//!   drevo's primary-kind index, so `MATCH (n:SecondaryLabel)` falls
//!   back to a full scan filtered by label. An indexed fast path lands
//!   with the planner work in task `00086`.
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
//! * **`DELETE`** of a node with connected relationships errors with
//!   [`ExecError::InvalidMutation`](crate::cypher::executor::ExecError::InvalidMutation) unless the user wrote
//!   `DETACH DELETE`. `DETACH DELETE` reuses the cascade behaviour of
//!   [`crate::db::Drevo::delete_node`] (which removes every adjacency
//!   for the node).
//! * **`MERGE`** runs as MATCH-or-CREATE: the pattern is matched
//!   against existing data first; if no row matches, the pattern is
//!   created exactly once. `ON CREATE SET` runs only on the create
//!   branch, `ON MATCH SET` only on the match branch.

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
    /// Total number of property assignments performed by `SET` /
    /// `REMOVE` / `MERGE`. A single `SET n = {a:1, b:2}` counts as two
    /// assignments. Label adds / removes are tracked separately by
    /// [`labels_added`](Self::labels_added) / [`labels_removed`](Self::labels_removed).
    pub properties_set: usize,
    /// Total number of nodes removed by `DELETE` / `DETACH DELETE`.
    pub nodes_deleted: usize,
    /// Total number of relationships removed by `DELETE` / `DETACH DELETE`
    /// (including the cascade-removed edges of `DETACH DELETE`).
    pub relationships_deleted: usize,
    /// Total number of labels added by `SET n:Label`.
    pub labels_added: usize,
    /// Total number of labels removed by `REMOVE n:Label`.
    pub labels_removed: usize,
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
    /// A mutation clause (`SET` / `REMOVE` / `DELETE` / `MERGE`) saw a
    /// structurally invalid target — e.g. `SET` on a literal, `DELETE`
    /// of a connected node without `DETACH`.
    #[error("invalid mutation: {0}")]
    InvalidMutation(String),
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
            Self::MissingParameter(_)
            | Self::InvalidCreate(_)
            | Self::InvalidMutation(_)
            | Self::Storage(_) => None,
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

/// Reserved property key holding a node's secondary Cypher labels.
///
/// drevo storage has a single primary `kind` per node. Cypher allows a
/// node to carry any number of labels; the extras live in this property
/// as a JSON array of strings, never visible to user-level Cypher
/// `n.<prop>` access (the executor filters it out when surfacing the
/// property map).
const SECONDARY_LABELS_KEY: &str = "_labels";

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
    let mut secondary: Vec<String> = Vec::new();
    for (k, v) in node.properties.iter() {
        if k == SECONDARY_LABELS_KEY {
            if let serde_json::Value::Array(arr) = v {
                for item in arr {
                    if let serde_json::Value::String(s) = item {
                        secondary.push(s.clone());
                    }
                }
            }
            continue;
        }
        properties.insert(k.clone(), json_to_value(v));
    }
    let mut labels = vec![node.kind.clone()];
    labels.extend(secondary);
    Arc::new(NodeValue {
        id: node.id,
        uuid: node.uuid,
        labels,
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
            validate_path_supported(&m.pattern.path, /*creating=*/ true)?;
            for item in m.on_create.iter().chain(m.on_match.iter()) {
                validate_set_item_supported(item)?;
            }
        }
        Clause::Delete(d) => {
            for target in &d.targets {
                validate_expr_supported(target)?;
            }
        }
        Clause::Set(s) => {
            for item in &s.items {
                validate_set_item_supported(item)?;
            }
        }
        Clause::Remove(r) => {
            for item in &r.items {
                match item {
                    crate::cypher::ast::RemoveItem::Property(expr) => {
                        validate_expr_supported(expr)?
                    }
                    crate::cypher::ast::RemoveItem::Labels { target, .. } => {
                        validate_expr_supported(target)?;
                    }
                }
            }
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
        if let Some(map) = &segment.node.properties {
            for (_, expr) in &map.entries {
                validate_expr_supported(expr)?;
            }
        }
    }
    Ok(())
}

fn validate_set_item_supported(item: &crate::cypher::ast::SetItem) -> ExecResultT<()> {
    use crate::cypher::ast::SetItem;
    match item {
        SetItem::Property { target, value }
        | SetItem::Replace { target, value }
        | SetItem::Merge { target, value } => {
            validate_expr_supported(target)?;
            validate_expr_supported(value)?;
        }
        SetItem::Labels { target, .. } => validate_expr_supported(target)?,
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
            Clause::Merge(m) => self.run_merge(m),
            Clause::Delete(d) => self.run_delete(d),
            Clause::Set(s) => self.run_set(s),
            Clause::Remove(r) => self.run_remove(r),
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
        // For a single label we can prefer `list_nodes_by_kind` (drevo's
        // primary-kind index) as a fast path, but the pattern label may
        // match a secondary label — added via `SET n:Label` and stored
        // in the reserved `_labels` property. Secondary labels have no
        // dedicated index, so we always fall back to `list_recent` to
        // catch them. Picking the union is more work than necessary; in
        // practice list_recent on the in-memory backend is O(n) which
        // matches list_nodes_by_kind, and an optimised index lands with
        // task `00086` (cost-based planner).
        let mut nodes: Vec<Node> = self.drevo.list_recent(usize::MAX)?;
        if let Some(label) = pattern.labels.first() {
            // Merge in primary-kind hits in case `list_recent` is bounded
            // by some future backend implementation.
            let primary = self.drevo.list_nodes_by_kind(label, usize::MAX, 0)?;
            for node in primary {
                if !nodes.iter().any(|n| n.id == node.id) {
                    nodes.push(node);
                }
            }
        }
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
        let extra_labels: Vec<String> = pattern.labels.iter().skip(1).cloned().collect();

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
            if k == SECONDARY_LABELS_KEY {
                continue;
            }
            let json = value_to_json(v).ok_or_else(|| {
                ExecError::InvalidCreate(format!(
                    "cannot store value of type {} as property `{}`",
                    v.type_name(),
                    k
                ))
            })?;
            storage_props.insert(k.clone(), json);
        }
        if !extra_labels.is_empty() {
            storage_props.insert(
                SECONDARY_LABELS_KEY.to_string(),
                serde_json::Value::Array(
                    extra_labels
                        .iter()
                        .map(|l| serde_json::Value::String(l.clone()))
                        .collect(),
                ),
            );
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

    // ----- SET / REMOVE / DELETE / MERGE -----------------------------------

    fn run_set(&mut self, s: &crate::cypher::ast::SetClause) -> ExecResultT<()> {
        use crate::cypher::ast::SetItem;
        // If no MATCH preceded the SET, the binding set is empty — Cypher
        // semantics say SET is a per-row mutation, so it does nothing.
        let bindings = std::mem::take(&mut self.bindings);
        let mut updated_bindings = Vec::with_capacity(bindings.len());
        for mut row in bindings.into_iter() {
            for item in &s.items {
                match item {
                    SetItem::Property { target, value } => {
                        self.apply_set_property(target, value, &mut row)?;
                    }
                    SetItem::Replace { target, value } => {
                        self.apply_set_replace(target, value, &mut row, /*merge=*/ false)?;
                    }
                    SetItem::Merge { target, value } => {
                        self.apply_set_replace(target, value, &mut row, /*merge=*/ true)?;
                    }
                    SetItem::Labels { target, labels } => {
                        self.apply_set_labels(target, labels, &mut row)?;
                    }
                }
            }
            updated_bindings.push(row);
        }
        self.bindings = updated_bindings;
        Ok(())
    }

    fn run_remove(&mut self, r: &crate::cypher::ast::RemoveClause) -> ExecResultT<()> {
        use crate::cypher::ast::RemoveItem;
        let bindings = std::mem::take(&mut self.bindings);
        let mut updated_bindings = Vec::with_capacity(bindings.len());
        for mut row in bindings.into_iter() {
            for item in &r.items {
                match item {
                    RemoveItem::Property(target) => {
                        self.apply_remove_property(target, &mut row)?;
                    }
                    RemoveItem::Labels { target, labels } => {
                        self.apply_remove_labels(target, labels, &mut row)?;
                    }
                }
            }
            updated_bindings.push(row);
        }
        self.bindings = updated_bindings;
        Ok(())
    }

    fn run_delete(&mut self, d: &crate::cypher::ast::DeleteClause) -> ExecResultT<()> {
        // Collect ids first, then delete — this avoids issues if the
        // same node is bound under different variable names across rows.
        let mut node_ids: Vec<u64> = Vec::new();
        let mut rel_ids: Vec<u64> = Vec::new();
        for row in &self.bindings {
            for target in &d.targets {
                let v = self.eval(target, row)?;
                match v {
                    Value::Node(nv) => {
                        if !node_ids.contains(&nv.id) {
                            node_ids.push(nv.id);
                        }
                    }
                    Value::Relationship(rv) => {
                        if !rel_ids.contains(&rv.id) {
                            rel_ids.push(rv.id);
                        }
                    }
                    Value::Null => {}
                    other => {
                        return Err(ExecError::InvalidMutation(format!(
                            "DELETE expects a Node or Relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        // Relationships first so they're not cascade-deleted by node removal.
        for id in &rel_ids {
            if self.drevo.get_edge(*id)?.is_some() {
                self.drevo.delete_edge(*id)?;
                self.stats.relationships_deleted += 1;
            }
        }
        for id in &node_ids {
            if let Some(_node) = self.drevo.get_node(*id)? {
                let connected = self.drevo.edges_of(*id, ModelDirection::Both)?;
                if !d.detach && !connected.is_empty() {
                    return Err(ExecError::InvalidMutation(format!(
                        "cannot DELETE node {} — it has {} connected relationship(s); use DETACH DELETE",
                        id, connected.len()
                    )));
                }
                let edge_count = connected.len();
                self.drevo.delete_node(*id)?;
                self.stats.nodes_deleted += 1;
                if d.detach {
                    self.stats.relationships_deleted += edge_count;
                }
            }
        }
        Ok(())
    }

    fn run_merge(&mut self, m: &crate::cypher::ast::MergeClause) -> ExecResultT<()> {
        if self.bindings.is_empty() {
            self.bindings.push(HashMap::new());
        }
        let prior = std::mem::take(&mut self.bindings);
        let mut new_bindings: Vec<Bindings> = Vec::new();
        for existing in prior.into_iter() {
            // Try to MATCH the pattern first.
            let matched = self.match_path(&m.pattern.path, &existing)?;
            if !matched.is_empty() {
                for mut row in matched {
                    self.apply_set_items(&m.on_match, &mut row)?;
                    new_bindings.push(row);
                }
            } else {
                // No match — CREATE the pattern and run ON CREATE actions.
                let mut row = existing.clone();
                self.create_path(&m.pattern.path, &mut row)?;
                self.apply_set_items(&m.on_create, &mut row)?;
                new_bindings.push(row);
            }
        }
        self.bindings = new_bindings;
        Ok(())
    }

    fn apply_set_items(
        &mut self,
        items: &[crate::cypher::ast::SetItem],
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        use crate::cypher::ast::SetItem;
        for item in items {
            match item {
                SetItem::Property { target, value } => {
                    self.apply_set_property(target, value, row)?;
                }
                SetItem::Replace { target, value } => {
                    self.apply_set_replace(target, value, row, /*merge=*/ false)?;
                }
                SetItem::Merge { target, value } => {
                    self.apply_set_replace(target, value, row, /*merge=*/ true)?;
                }
                SetItem::Labels { target, labels } => {
                    self.apply_set_labels(target, labels, row)?;
                }
            }
        }
        Ok(())
    }

    fn apply_set_property(
        &mut self,
        target: &Expression,
        value: &Expression,
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        let Expression::Property { base, name, span } = target else {
            return Err(ExecError::InvalidMutation(
                "SET target must be a property access (`var.prop`)".into(),
            ));
        };
        let var_name = match base.as_ref() {
            Expression::Variable(n, _) => n.clone(),
            _ => {
                return Err(ExecError::InvalidMutation(
                    "SET target must be of the form `variable.property`".into(),
                ));
            }
        };
        let new_value = self.eval(value, row)?;
        let entity = row
            .get(&var_name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundVariable {
                name: var_name.clone(),
                span: *span,
            })?;
        match entity {
            Value::Node(nv) => {
                self.write_node_property(&nv, name, new_value.clone(), row, &var_name)?;
            }
            Value::Relationship(rv) => {
                self.write_edge_property(&rv, name, new_value.clone(), row, &var_name)?;
            }
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "Node or Relationship".into(),
                    got: other.type_name().into(),
                    span: *span,
                });
            }
        }
        self.stats.properties_set += 1;
        Ok(())
    }

    fn apply_set_replace(
        &mut self,
        target: &Expression,
        value: &Expression,
        row: &mut Bindings,
        merge: bool,
    ) -> ExecResultT<()> {
        let Expression::Variable(var_name, span) = target else {
            return Err(ExecError::InvalidMutation(
                "SET = / SET += target must be a variable".into(),
            ));
        };
        let entity = row
            .get(var_name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundVariable {
                name: var_name.clone(),
                span: *span,
            })?;
        let new_map = match self.eval(value, row)? {
            Value::Map(m) => m,
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "Map".into(),
                    got: other.type_name().into(),
                    span: *span,
                });
            }
        };
        let count = new_map.len();
        match entity {
            Value::Node(nv) => {
                self.replace_node_properties(&nv, new_map, merge, row, var_name)?;
            }
            Value::Relationship(rv) => {
                self.replace_edge_properties(&rv, new_map, merge, row, var_name)?;
            }
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "Node or Relationship".into(),
                    got: other.type_name().into(),
                    span: *span,
                });
            }
        }
        self.stats.properties_set += count;
        Ok(())
    }

    fn apply_set_labels(
        &mut self,
        target: &Expression,
        labels: &[String],
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        let Expression::Variable(var_name, span) = target else {
            return Err(ExecError::InvalidMutation(
                "SET :Label target must be a variable".into(),
            ));
        };
        let entity = row
            .get(var_name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundVariable {
                name: var_name.clone(),
                span: *span,
            })?;
        let Value::Node(nv) = entity else {
            return Err(ExecError::TypeMismatch {
                expected: "Node".into(),
                got: entity.type_name().into(),
                span: *span,
            });
        };
        let stored = self
            .drevo
            .get_node(nv.id)?
            .ok_or_else(|| ExecError::InvalidMutation(format!("node {} not found", nv.id)))?;
        let mut current = node_labels_from_storage(&stored);
        let mut changed = 0usize;
        for label in labels {
            if !current.iter().any(|l| l == label) {
                current.push(label.clone());
                changed += 1;
            }
        }
        if changed > 0 {
            self.persist_node_labels(&stored, &current)?;
            self.stats.labels_added += changed;
            // Refresh binding so subsequent clauses see the new labels.
            if let Some(refreshed) = self.drevo.get_node(nv.id)? {
                row.insert(var_name.clone(), Value::Node(node_to_value(&refreshed)));
            }
        }
        Ok(())
    }

    fn apply_remove_property(
        &mut self,
        target: &Expression,
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        let Expression::Property { base, name, span } = target else {
            return Err(ExecError::InvalidMutation(
                "REMOVE target must be a property access (`var.prop`)".into(),
            ));
        };
        let var_name = match base.as_ref() {
            Expression::Variable(n, _) => n.clone(),
            _ => {
                return Err(ExecError::InvalidMutation(
                    "REMOVE target must be of the form `variable.property`".into(),
                ));
            }
        };
        let entity = row
            .get(&var_name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundVariable {
                name: var_name.clone(),
                span: *span,
            })?;
        match entity {
            Value::Node(nv) => {
                self.remove_node_property(&nv, name, row, &var_name)?;
            }
            Value::Relationship(rv) => {
                self.remove_edge_property(&rv, name, row, &var_name)?;
            }
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "Node or Relationship".into(),
                    got: other.type_name().into(),
                    span: *span,
                });
            }
        }
        self.stats.properties_set += 1;
        Ok(())
    }

    fn apply_remove_labels(
        &mut self,
        target: &Expression,
        labels: &[String],
        row: &mut Bindings,
    ) -> ExecResultT<()> {
        let Expression::Variable(var_name, span) = target else {
            return Err(ExecError::InvalidMutation(
                "REMOVE :Label target must be a variable".into(),
            ));
        };
        let entity = row
            .get(var_name)
            .cloned()
            .ok_or_else(|| ExecError::UnboundVariable {
                name: var_name.clone(),
                span: *span,
            })?;
        let Value::Node(nv) = entity else {
            return Err(ExecError::TypeMismatch {
                expected: "Node".into(),
                got: entity.type_name().into(),
                span: *span,
            });
        };
        let stored = self
            .drevo
            .get_node(nv.id)?
            .ok_or_else(|| ExecError::InvalidMutation(format!("node {} not found", nv.id)))?;
        // Cannot remove the primary label (drevo `kind`).
        for label in labels {
            if &stored.kind == label {
                return Err(ExecError::InvalidMutation(format!(
                    "cannot REMOVE primary label `{}` — drevo nodes always carry their `kind`",
                    label
                )));
            }
        }
        let mut current = node_labels_from_storage(&stored);
        let before = current.len();
        current.retain(|l| !labels.iter().any(|rm| rm == l));
        let removed = before - current.len();
        if removed > 0 {
            self.persist_node_labels(&stored, &current)?;
            self.stats.labels_removed += removed;
            if let Some(refreshed) = self.drevo.get_node(nv.id)? {
                row.insert(var_name.clone(), Value::Node(node_to_value(&refreshed)));
            }
        }
        Ok(())
    }

    fn write_node_property(
        &mut self,
        nv: &Arc<NodeValue>,
        prop_name: &str,
        new_value: Value,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self
            .drevo
            .get_node(nv.id)?
            .ok_or_else(|| ExecError::InvalidMutation(format!("node {} not found", nv.id)))?;
        let mut patch = crate::model::NodePatch::default();
        match prop_name {
            "title" => {
                patch.title = Some(value_as_string_for_alias(&new_value, "title")?);
            }
            "body" => {
                patch.body = Some(value_as_string_for_alias(&new_value, "body")?);
            }
            _ => {
                let mut props = stored.properties.clone();
                if matches!(new_value, Value::Null) {
                    props.remove(prop_name);
                } else {
                    let json = value_to_json(&new_value).ok_or_else(|| {
                        ExecError::InvalidMutation(format!(
                            "cannot store value of type {} as property `{}`",
                            new_value.type_name(),
                            prop_name
                        ))
                    })?;
                    props.insert(prop_name.to_string(), json);
                }
                patch.properties = Some(props);
            }
        }
        self.drevo.update_node(nv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_node(nv.id)? {
            row.insert(var_name.to_string(), Value::Node(node_to_value(&refreshed)));
        }
        Ok(())
    }

    fn write_edge_property(
        &mut self,
        rv: &Arc<RelationshipValue>,
        prop_name: &str,
        new_value: Value,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self.drevo.get_edge(rv.id)?.ok_or_else(|| {
            ExecError::InvalidMutation(format!("relationship {} not found", rv.id))
        })?;
        let mut props = stored.properties.clone();
        if matches!(new_value, Value::Null) {
            props.remove(prop_name);
        } else {
            let json = value_to_json(&new_value).ok_or_else(|| {
                ExecError::InvalidMutation(format!(
                    "cannot store value of type {} as property `{}`",
                    new_value.type_name(),
                    prop_name
                ))
            })?;
            props.insert(prop_name.to_string(), json);
        }
        let patch = crate::model::EdgePatch {
            properties: Some(props),
            ..Default::default()
        };
        self.drevo.update_edge(rv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_edge(rv.id)? {
            row.insert(
                var_name.to_string(),
                Value::Relationship(edge_to_value(&refreshed)),
            );
        }
        Ok(())
    }

    fn replace_node_properties(
        &mut self,
        nv: &Arc<NodeValue>,
        new_map: BTreeMap<String, Value>,
        merge: bool,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self
            .drevo
            .get_node(nv.id)?
            .ok_or_else(|| ExecError::InvalidMutation(format!("node {} not found", nv.id)))?;
        // Start either from existing storage props (merge) or empty (replace).
        // Always preserve the reserved `_labels` key — it carries Cypher
        // labels orthogonal to user properties.
        let mut next_props = if merge {
            stored.properties.clone()
        } else {
            let mut p = crate::model::Properties::default();
            if let Some(v) = stored.properties.get(SECONDARY_LABELS_KEY) {
                p.insert(SECONDARY_LABELS_KEY.to_string(), v.clone());
            }
            p
        };
        let mut patch = crate::model::NodePatch::default();
        for (k, v) in &new_map {
            match k.as_str() {
                "title" => {
                    patch.title = Some(value_as_string_for_alias(v, "title")?);
                }
                "body" => {
                    patch.body = Some(value_as_string_for_alias(v, "body")?);
                }
                _ => {
                    if matches!(v, Value::Null) {
                        next_props.remove(k);
                    } else {
                        let json = value_to_json(v).ok_or_else(|| {
                            ExecError::InvalidMutation(format!(
                                "cannot store value of type {} as property `{}`",
                                v.type_name(),
                                k
                            ))
                        })?;
                        next_props.insert(k.clone(), json);
                    }
                }
            }
        }
        // On replace, clear title/body if not provided in the new map.
        if !merge {
            if !new_map.contains_key("title") {
                patch.title = Some(synth_title(&stored.kind));
            }
            if !new_map.contains_key("body") {
                patch.body = Some(String::new());
            }
        }
        patch.properties = Some(next_props);
        self.drevo.update_node(nv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_node(nv.id)? {
            row.insert(var_name.to_string(), Value::Node(node_to_value(&refreshed)));
        }
        Ok(())
    }

    fn replace_edge_properties(
        &mut self,
        rv: &Arc<RelationshipValue>,
        new_map: BTreeMap<String, Value>,
        merge: bool,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self.drevo.get_edge(rv.id)?.ok_or_else(|| {
            ExecError::InvalidMutation(format!("relationship {} not found", rv.id))
        })?;
        let mut next = if merge {
            stored.properties.clone()
        } else {
            crate::model::Properties::default()
        };
        for (k, v) in &new_map {
            if matches!(v, Value::Null) {
                next.remove(k);
            } else {
                let json = value_to_json(v).ok_or_else(|| {
                    ExecError::InvalidMutation(format!(
                        "cannot store value of type {} as property `{}`",
                        v.type_name(),
                        k
                    ))
                })?;
                next.insert(k.clone(), json);
            }
        }
        let patch = crate::model::EdgePatch {
            properties: Some(next),
            ..Default::default()
        };
        self.drevo.update_edge(rv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_edge(rv.id)? {
            row.insert(
                var_name.to_string(),
                Value::Relationship(edge_to_value(&refreshed)),
            );
        }
        Ok(())
    }

    fn remove_node_property(
        &mut self,
        nv: &Arc<NodeValue>,
        prop_name: &str,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self
            .drevo
            .get_node(nv.id)?
            .ok_or_else(|| ExecError::InvalidMutation(format!("node {} not found", nv.id)))?;
        let mut patch = crate::model::NodePatch::default();
        match prop_name {
            "title" => patch.title = Some(synth_title(&stored.kind)),
            "body" => patch.body = Some(String::new()),
            _ => {
                let mut props = stored.properties.clone();
                props.remove(prop_name);
                patch.properties = Some(props);
            }
        }
        self.drevo.update_node(nv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_node(nv.id)? {
            row.insert(var_name.to_string(), Value::Node(node_to_value(&refreshed)));
        }
        Ok(())
    }

    fn remove_edge_property(
        &mut self,
        rv: &Arc<RelationshipValue>,
        prop_name: &str,
        row: &mut Bindings,
        var_name: &str,
    ) -> ExecResultT<()> {
        let stored = self.drevo.get_edge(rv.id)?.ok_or_else(|| {
            ExecError::InvalidMutation(format!("relationship {} not found", rv.id))
        })?;
        let mut props = stored.properties.clone();
        props.remove(prop_name);
        let patch = crate::model::EdgePatch {
            properties: Some(props),
            ..Default::default()
        };
        self.drevo.update_edge(rv.id, patch)?;
        if let Some(refreshed) = self.drevo.get_edge(rv.id)? {
            row.insert(
                var_name.to_string(),
                Value::Relationship(edge_to_value(&refreshed)),
            );
        }
        Ok(())
    }

    fn persist_node_labels(&mut self, stored: &Node, labels: &[String]) -> ExecResultT<()> {
        let mut props = stored.properties.clone();
        let secondary: Vec<&String> = labels.iter().skip(1).collect();
        if secondary.is_empty() {
            props.remove(SECONDARY_LABELS_KEY);
        } else {
            props.insert(
                SECONDARY_LABELS_KEY.to_string(),
                serde_json::Value::Array(
                    secondary
                        .iter()
                        .map(|s| serde_json::Value::String((*s).clone()))
                        .collect(),
                ),
            );
        }
        let patch = crate::model::NodePatch {
            properties: Some(props),
            ..Default::default()
        };
        self.drevo.update_node(stored.id, patch)?;
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

fn node_labels_from_storage(node: &Node) -> Vec<String> {
    let mut labels = vec![node.kind.clone()];
    if let Some(serde_json::Value::Array(arr)) = node.properties.get(SECONDARY_LABELS_KEY) {
        for item in arr {
            if let serde_json::Value::String(s) = item {
                if !labels.iter().any(|l| l == s) {
                    labels.push(s.clone());
                }
            }
        }
    }
    labels
}

fn value_as_string_for_alias(value: &Value, alias: &str) -> ExecResultT<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Null => Ok(String::new()),
        other => Err(ExecError::InvalidMutation(format!(
            "alias `{}` must be a String, got {}",
            alias,
            other.type_name()
        ))),
    }
}

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
    // All requested labels must be present (Cypher MATCH semantics).
    for label in &pattern.labels {
        if !nv.labels.iter().any(|l| l == label) {
            return Ok(false);
        }
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

    // ---- SET --------------------------------------------------------------

    #[test]
    fn set_single_property_assigns_value() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        let res = run("MATCH (n:Person {name: 'A'}) SET n.age = 31", &db);
        assert_eq!(res.stats.properties_set, 1);
        let persisted = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert_eq!(persisted.properties.get("age").unwrap().as_i64(), Some(31));
    }

    #[test]
    fn set_property_creates_new_one_if_missing() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("MATCH (n:Person) SET n.email = 'a@b.com'", &db);
        let persisted = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert_eq!(
            persisted.properties.get("email").unwrap().as_str(),
            Some("a@b.com")
        );
    }

    #[test]
    fn set_title_alias_updates_node_title() {
        let db = drevo();
        run("CREATE (:Note {title: 'Old'})", &db);
        run("MATCH (n:Note) SET n.title = 'New'", &db);
        let persisted = &db.list_nodes_by_kind("Note", 10, 0).unwrap()[0];
        assert_eq!(persisted.title, "New");
    }

    #[test]
    fn set_body_alias_updates_node_body() {
        let db = drevo();
        run("CREATE (:Note {title: 'T', body: 'Old body'})", &db);
        run("MATCH (n:Note) SET n.body = 'New body'", &db);
        let persisted = &db.list_nodes_by_kind("Note", 10, 0).unwrap()[0];
        assert_eq!(persisted.body, "New body");
    }

    #[test]
    fn set_replace_overwrites_entire_property_map() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30, team: 'red'})", &db);
        run(
            "MATCH (n:Person {name: 'A'}) SET n = {name: 'A', score: 99}",
            &db,
        );
        let persisted = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert!(persisted.properties.get("age").is_none());
        assert!(persisted.properties.get("team").is_none());
        assert_eq!(
            persisted.properties.get("score").unwrap().as_i64(),
            Some(99)
        );
    }

    #[test]
    fn set_merge_keeps_old_and_adds_new() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        run(
            "MATCH (n:Person {name: 'A'}) SET n += {age: 31, team: 'blue'}",
            &db,
        );
        let persisted = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert_eq!(persisted.properties.get("age").unwrap().as_i64(), Some(31));
        assert_eq!(
            persisted.properties.get("team").unwrap().as_str(),
            Some("blue")
        );
    }

    #[test]
    fn set_label_adds_secondary_label_to_node() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("MATCH (n:Person) SET n:Employee", &db);
        let res = run("MATCH (n:Employee) RETURN n.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn set_multiple_labels_in_one_clause() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("MATCH (n:Person) SET n:Employee:Manager", &db);
        let r1 = run("MATCH (n:Employee) RETURN n.name AS name", &db);
        let r2 = run("MATCH (n:Manager) RETURN n.name AS name", &db);
        assert_eq!(r1.rows.len(), 1);
        assert_eq!(r2.rows.len(), 1);
    }

    #[test]
    fn set_property_on_relationship() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS {since: 2020}]->(b:Person {name: 'B'})",
            &db,
        );
        let res = run(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) SET r.since = 2024 RETURN r.since AS since",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Integer(2024));
    }

    // ---- REMOVE -----------------------------------------------------------

    #[test]
    fn remove_property_drops_key_from_map() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30, team: 'red'})", &db);
        run("MATCH (n:Person) REMOVE n.team", &db);
        let persisted = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert!(persisted.properties.get("team").is_none());
        assert_eq!(persisted.properties.get("age").unwrap().as_i64(), Some(30));
    }

    #[test]
    fn remove_label_drops_secondary_label() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("MATCH (n:Person) SET n:Employee", &db);
        run("MATCH (n:Employee) REMOVE n:Employee", &db);
        let res = run("MATCH (n:Employee) RETURN n", &db);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn remove_primary_label_is_rejected() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let e = err("MATCH (n:Person) REMOVE n:Person", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(_)),
            "expected InvalidMutation, got {:?}",
            e
        );
    }

    // ---- DELETE -----------------------------------------------------------

    #[test]
    fn delete_unconnected_node_removes_it() {
        let db = drevo();
        run("CREATE (:Person {name: 'Solo'})", &db);
        let res = run("MATCH (n:Person {name: 'Solo'}) DELETE n", &db);
        assert_eq!(res.stats.nodes_deleted, 1);
        assert!(db.list_nodes_by_kind("Person", 10, 0).unwrap().is_empty());
    }

    #[test]
    fn delete_connected_node_without_detach_errors() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &db,
        );
        let e = err("MATCH (a:Person {name: 'A'}) DELETE a", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(_)),
            "expected InvalidMutation, got {:?}",
            e
        );
        // The error is fail-fast — node should still exist.
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 2);
    }

    #[test]
    fn detach_delete_removes_node_and_its_edges() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &db,
        );
        let res = run("MATCH (a:Person {name: 'A'}) DETACH DELETE a", &db);
        assert_eq!(res.stats.nodes_deleted, 1);
        assert_eq!(res.stats.relationships_deleted, 1);
        let people = db.list_nodes_by_kind("Person", 10, 0).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(
            people[0].properties.get("name").and_then(|v| v.as_str()),
            Some("B")
        );
    }

    #[test]
    fn delete_relationship_only() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
            &db,
        );
        let res = run("MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r", &db);
        assert_eq!(res.stats.relationships_deleted, 1);
        assert_eq!(res.stats.nodes_deleted, 0);
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 2);
    }

    #[test]
    fn delete_same_node_twice_is_idempotent_per_run() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // Two MATCH rows would target the same node only if we double-bind,
        // but DELETE on an already-removed id should not panic.
        let res = run("MATCH (n:Person) DELETE n", &db);
        assert_eq!(res.stats.nodes_deleted, 1);
        let res2 = run("MATCH (n:Person) DELETE n", &db);
        assert_eq!(res2.stats.nodes_deleted, 0);
    }

    // ---- MERGE ------------------------------------------------------------

    #[test]
    fn merge_creates_when_missing() {
        let db = drevo();
        let res = run("MERGE (n:Person {name: 'A'})", &db);
        assert_eq!(res.stats.nodes_created, 1);
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn merge_matches_existing_and_does_not_recreate() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run("MERGE (n:Person {name: 'A'})", &db);
        assert_eq!(res.stats.nodes_created, 0);
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn merge_is_idempotent_when_repeated() {
        let db = drevo();
        for _ in 0..3 {
            run("MERGE (n:Person {name: 'A'})", &db);
        }
        assert_eq!(db.list_nodes_by_kind("Person", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn merge_on_create_set_runs_only_on_create() {
        let db = drevo();
        run(
            "MERGE (n:Person {name: 'A'}) ON CREATE SET n.created = true ON MATCH SET n.matched = true",
            &db,
        );
        let p = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert_eq!(p.properties.get("created").unwrap().as_bool(), Some(true));
        assert!(p.properties.get("matched").is_none());
    }

    #[test]
    fn merge_on_match_set_runs_only_on_match() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run(
            "MERGE (n:Person {name: 'A'}) ON CREATE SET n.created = true ON MATCH SET n.matched = true",
            &db,
        );
        let p = &db.list_nodes_by_kind("Person", 10, 0).unwrap()[0];
        assert_eq!(p.properties.get("matched").unwrap().as_bool(), Some(true));
        assert!(p.properties.get("created").is_none());
    }

    #[test]
    fn merge_relationship_between_bound_vars() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("CREATE (:Person {name: 'B'})", &db);
        let res = run(
            "MATCH (a:Person {name: 'A'}), (b:Person {name: 'B'}) MERGE (a)-[:KNOWS]->(b)",
            &db,
        );
        assert_eq!(res.stats.relationships_created, 1);
        // Second MERGE must not double-create.
        let res2 = run(
            "MATCH (a:Person {name: 'A'}), (b:Person {name: 'B'}) MERGE (a)-[:KNOWS]->(b)",
            &db,
        );
        assert_eq!(res2.stats.relationships_created, 0);
    }
}
