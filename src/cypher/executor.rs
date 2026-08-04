//! Cypher executor — Phase 10 tasks `00063` (CREATE / MATCH / RETURN),
//! `00064` (mutations: SET, DELETE, MERGE, REMOVE), `00065` (WHERE on
//! `MATCH`), `00066` (aggregations), `00067` (`OPTIONAL MATCH`),
//! `00068` (`WITH` query pipelining), and `00069` (variable-length
//! paths). Phase 10 is fully complete at this point.
//!
//! The executor consumes the [`crate::cypher::ast::Query`] produced by
//! [`crate::cypher::parser::parse`] and runs it against the underlying
//! [`crate::db::Drevo`] handle. The current cut covers the README
//! "critical path" prefix — `CREATE`, `MATCH`, `RETURN`, the full
//! mutation surface, `WHERE` predicates on `MATCH`, and aggregation
//! functions in `RETURN` — with enough expression evaluation to make
//! `RETURN`, `ORDER BY`, `SKIP`, `LIMIT`, `DISTINCT`, inline property
//! filters, `SET` / `REMOVE` / `DELETE` (incl. `DETACH DELETE`),
//! `MERGE` (with `ON CREATE` / `ON MATCH` actions), `WHERE` boolean /
//! comparison / `IN` / `IS NULL` predicates, and `COUNT` / `SUM` /
//! `AVG` / `MIN` / `MAX` / `COLLECT` (incl. `DISTINCT`) with implicit
//! `GROUP BY` semantics, all useful.
//!
//! # Aggregations (`00066`)
//!
//! `GROUP BY` is implicit: every projection in `RETURN` that does *not*
//! contain an aggregation function call forms a group key. Rows with
//! equal group keys are folded into one output row, with the
//! aggregation columns reduced across the group. A pure-aggregation
//! query (no group keys) always yields exactly one row, even on zero
//! input rows — `COUNT(*)` returns `0`, `MIN` / `MAX` / `AVG` return
//! `NULL`. Aggregations skip `NULL` per Cypher semantics; the
//! `DISTINCT` modifier inside an aggregation deduplicates per-row
//! argument values before folding.
//!
//! # OPTIONAL MATCH (`00067`)
//!
//! `OPTIONAL MATCH` is the Cypher analogue of SQL's LEFT OUTER JOIN.
//! For every input binding row, the pattern is matched as usual; if
//! it produces zero rows (either because the pattern doesn't match
//! or because the optional `WHERE` rejects every candidate), exactly
//! one synthetic row is emitted with every variable the pattern
//! *would* have introduced set to `NULL`. Variables already bound by
//! an upstream `MATCH` flow through unchanged. WHERE attached to an
//! `OPTIONAL MATCH` is part of the pattern (not a post-join filter):
//! rows that fail it are treated as "no match" and trigger the
//! null-row synthesis, so the upstream row is *never* dropped.
//!
//! # WITH (`00068`)
//!
//! `WITH` is the query-pipelining boundary. Like `RETURN` it projects
//! a set of expressions with optional aliases, applies `DISTINCT` /
//! `ORDER BY` / `SKIP` / `LIMIT`, and supports aggregation. Unlike
//! `RETURN` it isn't terminal — it converts the projected rows back
//! into bindings keyed by the column names, so the *next* clause
//! sees only the projected aliases (pattern variables that weren't
//! projected are dropped — `WITH` is the only point at which the
//! variable scope can be reshaped).
//!
//! `WITH` accepts a trailing `WHERE` that filters *after* projection
//! (and after aggregation). This is the canonical
//! aggregation-before-filter pattern: `MATCH (n) WITH n.team AS team,
//! count(*) AS c WHERE c >= 2 RETURN team, c`.
//!
//! Cypher requires every non-variable projection in `WITH` to have an
//! `AS alias` — otherwise downstream clauses have no name to
//! reference. We surface that as `ExecError::InvalidMutation` with a
//! "use `expr AS name`" pointer.
//!
//! # Variable-length paths (`00069`)
//!
//! `MATCH (a)-[*N..M]->(b)` performs a breadth-first expansion from
//! `a` through edges matching the relationship pattern at depths
//! `N..=M` inclusive. Forms supported:
//!
//! * `[*]` — one or more hops (capped at `VARLEN_DEFAULT_UPPER`
//!   when unbounded above).
//! * `[*N]` — exactly `N` hops.
//! * `[*N..M]` / `[*..M]` / `[*N..]` — bounded range.
//! * `[*0..M]` — includes the zero-hop "source = target" case.
//!
//! **Cypher trail uniqueness** is enforced: within a single path no
//! relationship is traversed twice (nodes may repeat). If the
//! relationship pattern has a variable (`[r*1..3]`), `r` is bound to
//! a [`Value::List`](crate::cypher::executor::Value::List) of the
//! traversed relationships in source order.
//!
//! Variable-length paths in `CREATE` are rejected — they have no
//! semantic meaning there (how many edges to create?).
//!
//! `UNION` / `UNION ALL` combine the result rows of two or more arms as
//! of task `00136`: `UNION ALL` concatenates every arm's rows in arm
//! order, `UNION` additionally removes duplicate rows across the combined
//! set. Every arm must project the same column names in the same order,
//! and a query may not mix the two operators — both cases surface as
//! [`ExecError::UnionMismatch`](crate::cypher::executor::ExecError::UnionMismatch).
//!
//! `EXISTS { [MATCH] pattern [WHERE predicate] }` is an existential
//! subquery as of task `00152` — `true` iff at least one match of the
//! enclosed pattern survives the optional inner `WHERE`, relative to the
//! current row. The brace-delimited, richer sibling of the bare pattern
//! predicate (`00151`): the braces let a single node (`EXISTS { (n) }`)
//! be a subquery rather than grouping, an optional leading `MATCH`
//! keyword is accepted, and the inner `WHERE` filters matches before the
//! existence test. (The deprecated `exists(n.prop)` *function* form is
//! not supported — `n.prop IS NOT NULL`, shipped in `00065`, replaces it.)
//!
//! Anything still unsupported surfaces as
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

/// Safety cap for `[*]` / `[*N..]` unbounded variable-length paths.
///
/// Cypher's "trail" uniqueness (no relationship traversed twice within
/// a single path) already prevents infinite walks, but on a dense graph
/// with cycles the frontier can still grow exponentially before the
/// invariant kicks in. drevo is an in-memory store run from CLI / MCP /
/// agentic workloads, so we cap the search depth at this value when the
/// pattern is unbounded above — the cap is deliberately small relative
/// to Neo4j's default (which is effectively unlimited) so the
/// `agentic_workload_*` suites stay fast. Bounded patterns
/// (`[*1..N]` / `[*N]`) honour the user's stated upper bound verbatim.
const VARLEN_DEFAULT_UPPER: usize = 25;

use crate::cypher::ast::{
    BinaryOp, CallClause, Clause, CreateClause, Direction as AstDirection, Expression,
    ForeachClause, ListPredicateKind, MapLiteral, MapProjectionSelector, MatchClause, NamedPattern,
    NodePattern, OrderDirection, OrderItem, PathPattern, ProjectionItem, Query, RelLength,
    RelationshipPattern, ReturnClause, ShortestKind, SingleQuery, UnaryOp, UnionKind, UnwindClause,
};
use crate::cypher::lexer::Span;
use crate::db::Drevo;
use crate::error::DrevoError;
use crate::model::{
    new_uuid_v7, Direction as ModelDirection, Edge, NewEdge, NewNode, Node, Properties,
};
use crate::semantic_index::{IndexMode, SemanticIndex};
use crate::vector::cosine_similarity;

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
    /// A bound path — an alternating node / relationship sequence produced by
    /// a named pattern (`MATCH p = (a)-->(b)`). See [`PathValue`].
    Path(Arc<PathValue>),
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

/// A path as seen by the Cypher runtime — the value bound by a named pattern
/// such as `MATCH p = (a)-[:R]->(b)`.
///
/// A path is an alternating sequence of nodes and relationships captured in
/// traversal order: `nodes[0]`, `relationships[0]`, `nodes[1]`, …,
/// `relationships[k-1]`, `nodes[k]`. The invariant
/// `nodes.len() == relationships.len() + 1` always holds, and `nodes` is
/// never empty (a single-node pattern `MATCH p = (a)` yields a length-0 path).
/// Endpoints that carry no Cypher variable are still recorded, so `nodes(p)`
/// surfaces anonymous intermediate hops.
#[derive(Debug, Clone, PartialEq)]
pub struct PathValue {
    /// Nodes in traversal order; `relationships.len() + 1` entries.
    pub nodes: Vec<Arc<NodeValue>>,
    /// Relationships in traversal order; one per hop.
    pub relationships: Vec<Arc<RelationshipValue>>,
}

impl PathValue {
    /// The number of relationships (hops) in the path — Cypher's `length(p)`.
    pub fn length(&self) -> usize {
        self.relationships.len()
    }
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
    /// A scalar function call was structurally invalid — wrong arity, an
    /// argument of the wrong type, or (for `similar`) an underlying vector
    /// math error such as a dimension mismatch or zero-magnitude operand.
    #[error("invalid call to `{name}`: {message}")]
    InvalidFunctionCall {
        /// Function name as written (`"similar"`).
        name: String,
        /// Explanation of what was wrong.
        message: String,
        /// Source span of the offending call.
        span: Span,
    },
    /// A `CALL` clause named a procedure that does not exist, passed the
    /// wrong number of arguments, or `YIELD`ed a column the procedure does
    /// not produce. drevo ships only a small set of read-only built-in
    /// procedures (`db.labels`, `db.relationshipTypes`, `db.propertyKeys`),
    /// so the error names the offending procedure and what was wrong.
    #[error("invalid procedure call `{name}`: {message}")]
    InvalidProcedureCall {
        /// Procedure name as written (`"db.labels"`).
        name: String,
        /// Explanation of what was wrong (unknown name, wrong arity,
        /// unknown yield column).
        message: String,
        /// Source span of the offending `CALL`.
        span: Span,
    },
    /// The arms of a `UNION` are incompatible. Either the projected
    /// column names differ between arms (Neo4j requires every arm of a
    /// `UNION` to return the same column names in the same order), or a
    /// single query mixed `UNION` and `UNION ALL` (forbidden — a query
    /// must pick one or the other).
    #[error("invalid UNION: {message}")]
    UnionMismatch {
        /// Explanation of the incompatibility.
        message: String,
        /// Source span of the offending `UNION` arm.
        span: Span,
    },
    /// The right-hand side of a `=~` regex match was not a valid regular
    /// expression, or matching exceeded the engine's complexity budget.
    #[error("invalid regular expression: {message}")]
    InvalidRegex {
        /// Explanation of why the regex was rejected.
        message: String,
        /// Source span of the offending `=~` expression.
        span: Span,
    },
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
            | Self::TypeMismatch { span, .. }
            | Self::InvalidFunctionCall { span, .. }
            | Self::InvalidProcedureCall { span, .. }
            | Self::UnionMismatch { span, .. }
            | Self::InvalidRegex { span, .. } => Some(*span),
            Self::MissingParameter(_)
            | Self::InvalidCreate(_)
            | Self::InvalidMutation(_)
            | Self::Storage(_) => None,
        }
    }
}

/// Convenience alias for executor results.
pub type ExecResultT<T> = Result<T, ExecError>;

/// One discovered path as `(relationships, nodes-after-source)`, both in
/// traversal order — the shape [`Executor::bfs_shortest_paths`] returns.
type ShortestPath = (Vec<Arc<RelationshipValue>>, Vec<Arc<NodeValue>>);

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
            // Two paths are equal when they traverse the same relationships in
            // the same order — the relationship ids fix the node sequence too.
            (Self::Path(a), Self::Path(b)) => {
                a.relationships.len() == b.relationships.len()
                    && a.nodes.first().map(|n| n.id) == b.nodes.first().map(|n| n.id)
                    && a.relationships
                        .iter()
                        .zip(b.relationships.iter())
                        .all(|(x, y)| x.id == y.id)
            }
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
            Self::Path(_) => "Path",
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
        // Nodes, relationships, and paths round-trip as opaque structures —
        // storing a bound graph value into a property map would be a
        // programming error and is rejected by InvalidCreate at the call site.
        Value::Node(_) | Value::Relationship(_) | Value::Path(_) => return None,
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

// ===== Named-path accumulation ==============================================
//
// A named pattern (`MATCH p = (a)-->(b)`) binds its variable to an alternating
// node/relationship [`PathValue`]. The matcher records the path incrementally
// under this reserved binding key: [`Executor::match_head`] seeds it with the
// head node, and each matched segment appends the traversed relationship and
// target node. [`Executor::match_named_pattern`] then renames the entry to the
// user's variable. The leading double space makes the key impossible to
// collide with a user-written Cypher identifier.

const PATH_ACCUM_KEY: &str = "  path";

/// Seed the path accumulator in `row` with a single starting node.
fn seed_path(row: &mut Bindings, head: Arc<NodeValue>) {
    row.insert(
        PATH_ACCUM_KEY.to_string(),
        Value::Path(Arc::new(PathValue {
            nodes: vec![head],
            relationships: Vec::new(),
        })),
    );
}

/// Extend the path accumulator in `row` by one hop — relationship `rel`
/// reaching `node`. A no-op when no accumulator is present.
fn extend_path(row: &mut Bindings, rel: Arc<RelationshipValue>, node: Arc<NodeValue>) {
    if let Some(Value::Path(p)) = row.get(PATH_ACCUM_KEY) {
        let mut nodes = p.nodes.clone();
        let mut relationships = p.relationships.clone();
        nodes.push(node);
        relationships.push(rel);
        row.insert(
            PATH_ACCUM_KEY.to_string(),
            Value::Path(Arc::new(PathValue {
                nodes,
                relationships,
            })),
        );
    }
}

/// Extend the path accumulator in `row` by several hops at once — used by the
/// variable-length expander, where `rels[i]` reaches `nodes[i]`. The two
/// slices must be the same length. A no-op when no accumulator is present.
fn extend_path_multi(
    row: &mut Bindings,
    rels: &[Arc<RelationshipValue>],
    nodes: &[Arc<NodeValue>],
) {
    if let Some(Value::Path(p)) = row.get(PATH_ACCUM_KEY) {
        let mut all_nodes = p.nodes.clone();
        let mut all_rels = p.relationships.clone();
        all_nodes.extend(nodes.iter().cloned());
        all_rels.extend(rels.iter().cloned());
        row.insert(
            PATH_ACCUM_KEY.to_string(),
            Value::Path(Arc::new(PathValue {
                nodes: all_nodes,
                relationships: all_rels,
            })),
        );
    }
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
    // Fast path — a query with no `UNION` is a single arm, executed
    // directly with no row combination.
    if query.parts.len() == 1 {
        return execute_single(&query.parts[0].query, drevo, params);
    }

    // Multi-arm `UNION`. The parser guarantees `parts[i].union` is
    // `Some` for every `i > 0` and the *kind that joins arm `i-1` to arm
    // `i`*. A query must not mix `UNION` and `UNION ALL`, so collapse the
    // joining kinds into a single agreed kind (or reject the mix).
    let mut kind: Option<UnionKind> = None;
    for part in &query.parts[1..] {
        // The parser guarantees `union` is `Some` for every arm after the
        // first; a `None` would be a parser bug, so skip it rather than
        // panic (no `unwrap`/`expect` in library code).
        let Some(this) = part.union else { continue };
        match kind {
            None => kind = Some(this),
            Some(prev) if prev != this => {
                return Err(ExecError::UnionMismatch {
                    message: "a query cannot mix UNION and UNION ALL — \
                              use one or the other for every arm"
                        .into(),
                    span: first_clause_span(&part.query.clauses),
                });
            }
            Some(_) => {}
        }
    }
    let distinct = matches!(kind, Some(UnionKind::Distinct));

    // Execute every arm independently and concatenate the rows in arm
    // order. Every arm must project the same column names in the same
    // order; stats accumulate across all arms.
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut stats = ExecStats::default();
    for part in &query.parts {
        let arm = execute_single(&part.query, drevo, params.clone())?;
        match &columns {
            None => columns = Some(arm.columns),
            Some(expected) if *expected != arm.columns => {
                return Err(ExecError::UnionMismatch {
                    message: format!(
                        "all arms of a UNION must return the same columns — \
                         expected {expected:?}, this arm returns {:?}",
                        arm.columns
                    ),
                    span: first_clause_span(&part.query.clauses),
                });
            }
            Some(_) => {}
        }
        rows.extend(arm.rows);
        stats = add_stats(stats, arm.stats);
    }

    if distinct {
        dedup_rows(&mut rows);
    }

    Ok(ExecResult {
        columns: columns.unwrap_or_default(),
        rows,
        stats,
    })
}

/// Execute one `UNION`-free arm against a fresh executor.
fn execute_single(
    single: &SingleQuery,
    drevo: &Drevo,
    params: HashMap<String, Value>,
) -> ExecResultT<ExecResult> {
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

/// Sum two [`ExecStats`] field-by-field — used to aggregate the mutation
/// counters of every arm of a `UNION` into one summary.
fn add_stats(a: ExecStats, b: ExecStats) -> ExecStats {
    ExecStats {
        nodes_created: a.nodes_created + b.nodes_created,
        relationships_created: a.relationships_created + b.relationships_created,
        properties_set: a.properties_set + b.properties_set,
        nodes_deleted: a.nodes_deleted + b.nodes_deleted,
        relationships_deleted: a.relationships_deleted + b.relationships_deleted,
        labels_added: a.labels_added + b.labels_added,
        labels_removed: a.labels_removed + b.labels_removed,
    }
}

fn validate_clause_supported(clause: &Clause) -> ExecResultT<()> {
    match clause {
        Clause::Match(m) => {
            if let Some(expr) = &m.where_clause {
                validate_expr_supported(expr)?;
            }
            for pattern in &m.patterns {
                validate_shortest_supported(pattern)?;
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
                    validate_expr_supported_in_projection(expr)?;
                }
            }
            for item in &r.order_by {
                // ORDER BY after aggregation typically references the
                // aliased projection columns; sub-expressions still go
                // through the strict validator (no function calls in
                // ORDER BY for 00066 — use an alias instead).
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
            for item in &w.items {
                match item {
                    ProjectionItem::Star => {}
                    ProjectionItem::Expression { expr, alias } => {
                        // Cypher requires an alias for every projection
                        // in WITH that isn't a bare variable — otherwise
                        // the projected column has no name downstream
                        // clauses can reference.
                        if alias.is_none() && !matches!(expr, Expression::Variable(..)) {
                            return Err(ExecError::InvalidMutation(
                                "WITH projection requires an alias for non-variable expression (use `expr AS name`)".into(),
                            ));
                        }
                        validate_expr_supported_in_projection(expr)?;
                    }
                }
            }
            for item in &w.order_by {
                validate_expr_supported(&item.expression)?;
            }
            if let Some(e) = &w.skip {
                validate_expr_supported(e)?;
            }
            if let Some(e) = &w.limit {
                validate_expr_supported(e)?;
            }
            if let Some(e) = &w.where_clause {
                validate_expr_supported(e)?;
            }
        }
        Clause::Unwind(u) => {
            validate_expr_supported(&u.expression)?;
        }
        Clause::Foreach(f) => {
            validate_expr_supported(&f.list)?;
            for inner in &f.clauses {
                validate_clause_supported(inner)?;
            }
        }
        Clause::Call(c) => {
            for arg in &c.args {
                validate_expr_supported(arg)?;
            }
            // Resolve the procedure upfront so an unknown name, wrong
            // arity, or a `YIELD` of a non-existent column is reported
            // deterministically before any side effects run — even on an
            // empty graph.
            let name = c.name.join(".");
            let columns =
                procedure_columns(&name).ok_or_else(|| ExecError::InvalidProcedureCall {
                    name: name.clone(),
                    message: "no such procedure — built-in procedures are \
                          db.labels, db.relationshipTypes, db.propertyKeys, \
                          drevo.vector.query, drevo.semantic.register, \
                          drevo.semantic.status, fts.search, \
                          fts.searchRelationships"
                        .into(),
                    span: c.span,
                })?;
            let expected_args = procedure_arity(&name);
            if c.args.len() != expected_args {
                return Err(ExecError::InvalidProcedureCall {
                    name,
                    message: format!("expected {expected_args} arguments, got {}", c.args.len()),
                    span: c.span,
                });
            }
            if let Some(items) = &c.yields {
                for item in items {
                    if !columns.contains(&item.name.as_str()) {
                        return Err(ExecError::InvalidProcedureCall {
                            name,
                            message: format!(
                                "procedure does not yield a column `{}` \
                                 (available: {})",
                                item.name,
                                columns.join(", ")
                            ),
                            span: item.span,
                        });
                    }
                }
            }
            if let Some(pred) = &c.where_clause {
                validate_expr_supported(pred)?;
            }
        }
    }
    Ok(())
}

fn validate_path_supported(path: &PathPattern, creating: bool) -> ExecResultT<()> {
    if let Some(map) = &path.head.properties {
        for (_, expr) in &map.entries {
            validate_expr_supported(expr)?;
        }
    }
    for segment in &path.tail {
        let rel = &segment.relationship;
        // Variable-length paths are MATCH-only; in CREATE they make
        // no semantic sense (how many edges should be created?) so
        // we keep the rejection there.
        if creating && rel.length.is_some() && !matches!(rel.length, Some(RelLength::Exact(1))) {
            return Err(ExecError::Unsupported {
                feature: "variable-length CREATE".into(),
                task: "future Phase 10 follow-up".into(),
                span: rel.span,
            });
        }
        // For MATCH, validate range bounds here so the upfront sweep
        // catches `[*5..2]` (lo > hi) before any side effects run.
        if !creating {
            if let Some(len) = &rel.length {
                validate_varlen_bounds(len)?;
            }
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

/// The source spelling of a [`ShortestKind`], for error messages.
fn shortest_fn_name(kind: ShortestKind) -> &'static str {
    match kind {
        ShortestKind::Single => "shortestPath",
        ShortestKind::All => "allShortestPaths",
    }
}

/// Validate the shape of a `shortestPath(...)` / `allShortestPaths(...)`
/// pattern: Neo4j requires exactly one relationship between two nodes, and
/// that relationship must be variable-length (`-[*]-`, `-[*..n]-`, …). A
/// no-op for an ordinary (non-shortest) pattern.
fn validate_shortest_supported(pattern: &NamedPattern) -> ExecResultT<()> {
    let Some(kind) = pattern.shortest else {
        return Ok(());
    };
    let name = shortest_fn_name(kind);
    let path = &pattern.path;
    let segment = match path.tail.as_slice() {
        [seg] => seg,
        _ => {
            return Err(ExecError::InvalidFunctionCall {
                name: name.into(),
                message: "requires exactly one relationship between two nodes".into(),
                span: path.head.span,
            });
        }
    };
    if segment.relationship.length.is_none() {
        return Err(ExecError::InvalidFunctionCall {
            name: name.into(),
            message: "requires a variable-length relationship, e.g. -[*]-".into(),
            span: segment.relationship.span,
        });
    }
    Ok(())
}

fn validate_varlen_bounds(len: &RelLength) -> ExecResultT<()> {
    match len {
        RelLength::Any => Ok(()),
        RelLength::Exact(n) => {
            if *n < 0 {
                return Err(ExecError::InvalidMutation(format!(
                    "variable-length range [*{}] must be non-negative",
                    n
                )));
            }
            Ok(())
        }
        RelLength::Range { from, to } => {
            let lo = from.unwrap_or(1);
            if lo < 0 {
                return Err(ExecError::InvalidMutation(format!(
                    "variable-length range [*{}..] must have non-negative lower bound",
                    lo
                )));
            }
            if let Some(hi) = to {
                if *hi < lo {
                    return Err(ExecError::InvalidMutation(format!(
                        "variable-length range [*{}..{}] is invalid: lower bound exceeds upper",
                        lo, hi
                    )));
                }
                if *hi < 0 {
                    return Err(ExecError::InvalidMutation(format!(
                        "variable-length range [*..{}] must have non-negative upper bound",
                        hi
                    )));
                }
            }
            Ok(())
        }
    }
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
        Expression::FunctionCall {
            name, args, span, ..
        } => {
            if is_scalar_function_name(name) {
                for arg in args {
                    validate_expr_supported(arg)?;
                }
                Ok(())
            } else {
                Err(ExecError::Unsupported {
                    feature: format!("function call `{}`", name.join(".")),
                    task: "future Phase 10 follow-up".into(),
                    span: *span,
                })
            }
        }
        Expression::Case {
            scrutinee,
            arms,
            else_branch,
            ..
        } => {
            if let Some(s) = scrutinee {
                validate_expr_supported(s)?;
            }
            for (when, then) in arms {
                validate_expr_supported(when)?;
                validate_expr_supported(then)?;
            }
            if let Some(e) = else_branch {
                validate_expr_supported(e)?;
            }
            Ok(())
        }
        Expression::Star(span) => Err(ExecError::Unsupported {
            feature: "`*` outside `count(*)`".into(),
            task: "future Phase 10 follow-up".into(),
            span: *span,
        }),
        Expression::Index { base, index, .. } => {
            validate_expr_supported(base)?;
            validate_expr_supported(index)
        }
        Expression::Slice { base, from, to, .. } => {
            validate_expr_supported(base)?;
            if let Some(f) = from {
                validate_expr_supported(f)?;
            }
            if let Some(t) = to {
                validate_expr_supported(t)?;
            }
            Ok(())
        }
        Expression::Binary { lhs, rhs, .. } => {
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
        // A list comprehension is supported as long as its sub-expressions
        // are. Aggregations are *not* allowed inside a comprehension (the
        // loop variable is per-element, not per-group), so every part is
        // checked with the non-aggregation validator — `collect(x)` inside a
        // comprehension stays `Unsupported` rather than silently misfolding.
        Expression::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            validate_expr_supported(list)?;
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            if let Some(proj) = projection {
                validate_expr_supported(proj)?;
            }
            Ok(())
        }
        // A list predicate (`all`/`any`/`none`/`single`) is supported when its
        // sub-expressions are; like a comprehension it loops a per-element
        // variable, so both parts use the non-aggregation validator.
        Expression::ListPredicate {
            list, predicate, ..
        } => {
            validate_expr_supported(list)?;
            validate_expr_supported(predicate)
        }
        // `reduce` loops a per-element variable plus a per-fold accumulator, so
        // (like a comprehension) aggregations are not meaningful inside it; all
        // three sub-expressions use the non-aggregation validator.
        Expression::Reduce {
            init, list, expr, ..
        } => {
            validate_expr_supported(init)?;
            validate_expr_supported(list)?;
            validate_expr_supported(expr)
        }
        // A map projection is supported when its base and any literal-entry
        // sub-expressions are. Like the comprehension family, an aggregation
        // is not meaningful inside a projection selector, so every part uses
        // the non-aggregation validator.
        Expression::MapProjection {
            base, selectors, ..
        } => {
            validate_expr_supported(base)?;
            for selector in selectors {
                if let MapProjectionSelector::Literal(_, expr) = selector {
                    validate_expr_supported(expr)?;
                }
            }
            Ok(())
        }
        // A pattern comprehension loops a per-match binding scope, so (like the
        // comprehension family) an aggregation inside it is not meaningful — its
        // optional predicate and mandatory projection use the non-aggregation
        // validator. The pattern itself is matched at runtime, exactly like a
        // `MATCH` pattern, so it needs no expression-level validation here.
        Expression::PatternComprehension {
            predicate,
            projection,
            ..
        } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            validate_expr_supported(projection)
        }
        // A pattern predicate is matched at runtime exactly like a `MATCH`
        // pattern (see `eval_pattern_predicate`), so it carries no
        // expression-level sub-parts to validate here.
        Expression::PatternPredicate { .. } => Ok(()),
        // An existential subquery matches its pattern at runtime like a `MATCH`;
        // its optional inner `WHERE` is evaluated per match (no aggregation),
        // so it uses the non-aggregation validator, mirroring the comprehension
        // family.
        Expression::ExistsSubquery { predicate, .. } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            Ok(())
        }
        // A counting subquery matches its pattern at runtime like a `MATCH` and
        // its optional inner `WHERE` is evaluated per match (no aggregation), so
        // it uses the non-aggregation validator, mirroring the existential
        // subquery.
        Expression::CountSubquery { predicate, .. } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
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

/// Walk the patterns of a `MATCH` clause and return every node /
/// relationship variable name that appears, in source order with
/// duplicates removed.
///
/// Used by `OPTIONAL MATCH` to figure out which variables a missing
/// pattern would have bound, so the synthesised "no match" row can
/// fill them with `NULL` (Cypher's left-join semantics).
fn collect_pattern_variables(patterns: &[NamedPattern]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push_unique = |name: &str, sink: &mut Vec<String>| {
        if !sink.iter().any(|n| n == name) {
            sink.push(name.to_string());
        }
    };
    for pattern in patterns {
        if let Some(name) = &pattern.path.head.variable {
            push_unique(name, &mut out);
        }
        for segment in &pattern.path.tail {
            if let Some(name) = &segment.relationship.variable {
                push_unique(name, &mut out);
            }
            if let Some(name) = &segment.node.variable {
                push_unique(name, &mut out);
            }
        }
    }
    out
}

/// Build the `OPTIONAL MATCH` "no match" row: the existing binding
/// plus a `NULL` entry for every variable the pattern *would* have
/// introduced (skipping any already bound by upstream clauses).
fn synthesise_null_row(existing: &Bindings, new_variables: &[String]) -> Bindings {
    let mut row = existing.clone();
    for name in new_variables {
        if !row.contains_key(name) {
            row.insert(name.clone(), Value::Null);
        }
    }
    row
}

fn is_aggregation_name(name: &[String]) -> bool {
    if name.len() != 1 {
        return false;
    }
    matches!(
        name[0].to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "collect"
            | "stdev"
            | "stdevp"
            | "percentilecont"
            | "percentiledisc"
    )
}

/// `true` when `lower` (an already-lowercased aggregation name) is one of the
/// two-argument percentile aggregations — `percentileCont` / `percentileDisc`,
/// which take `(value, fraction)`. Every other aggregation takes exactly one
/// argument (or `count(*)`).
fn is_percentile_aggregation(lower: &str) -> bool {
    matches!(lower, "percentilecont" | "percentiledisc")
}

/// The supported scalar (non-aggregation) functions: the built-in
/// string / numeric / list library ([`is_builtin_scalar_function`],
/// task `00138`), plus drevo's two domain extensions — `similar(...)`,
/// the joint graph+vector predicate (`00077`), and `keywords(...)`,
/// BM25-IDF keyword extraction (`00132`).
/// Which end of a relationship [`Executor::eval_endpoint_node`] resolves —
/// the tail (`startNode`) or the head (`endNode`). Issue #232.
#[derive(Clone, Copy)]
enum Endpoint {
    Start,
    End,
}

fn is_scalar_function_name(name: &[String]) -> bool {
    if name.len() != 1 {
        return false;
    }
    let lower = name[0].to_ascii_lowercase();
    // `startnode` / `endnode` (issue #232) are DB-aware like `similar` /
    // `keywords` — they resolve a relationship's endpoint *id* to the actual
    // node — so they are evaluated in the executor, not `call_scalar`, but
    // must still be recognised here as valid scalar function names.
    lower == "similar"
        || lower == "keywords"
        || lower == "startnode"
        || lower == "endnode"
        || is_builtin_scalar_function(&lower)
}

/// `true` when `lower` (an already-lowercased function name) is one of the
/// built-in scalar functions the executor evaluates in [`Executor::call_scalar`].
///
/// Kept deliberately in lock-step with the `call_scalar` dispatch: a name
/// accepted here that `call_scalar` does not handle would surface a confusing
/// "unsupported" error *after* the upfront validation sweep passed, and a name
/// `call_scalar` handles but this rejects would be blocked before it ever runs.
fn is_builtin_scalar_function(lower: &str) -> bool {
    matches!(
        lower,
        // String functions.
        "tolower"
            | "toupper"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "substring"
            | "replace"
            | "split"
            | "left"
            | "right"
            | "reverse"
            | "tostring"
            // Numeric functions.
            | "abs"
            | "ceil"
            | "floor"
            | "round"
            | "sign"
            | "sqrt"
            | "tointeger"
            | "tofloat"
            | "toboolean"
            // List value-conversion functions (task `00157`) — element-wise
            // siblings of the scalar conversions above.
            | "tointegerlist"
            | "tofloatlist"
            | "tobooleanlist"
            | "tostringlist"
            // Fully-lenient scalar conversions (task `00158`) — the Neo4j 5
            // `*OrNull` siblings of `toInteger` / `toFloat` / `toBoolean` /
            // `toString` that yield `NULL` for any unconvertible value.
            | "tointegerornull"
            | "tofloatornull"
            | "tobooleanornull"
            | "tostringornull"
            // Trigonometric / logarithmic functions (task `00156`).
            | "e"
            | "exp"
            | "log"
            | "log10"
            | "sin"
            | "cos"
            | "tan"
            | "cot"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "degrees"
            | "radians"
            | "pi"
            | "haversin"
            // List / scalar functions.
            | "size"
            | "length"
            | "head"
            | "last"
            | "tail"
            | "coalesce"
            | "range"
            | "keys"
            | "labels"
            | "type"
            | "id"
            | "properties"
            // Container predicate (task `00159`) — empty-test over the three
            // container types (String / List / Map). Fills the gap `size`
            // leaves: `size` rejects a Map, so `size(m) = 0` cannot express it.
            | "isempty"
            // Numeric predicate (task `00162`) — `isNaN(n)` tells the IEEE-754
            // NaN value apart from every other number. The only way to test
            // for NaN in Cypher, since `NaN = NaN` is false.
            | "isnan"
            // Path functions.
            | "nodes"
            | "relationships"
            // Non-deterministic value functions (task `00161`) — the two
            // zero-argument generators Neo4j exposes: `rand()` (uniform Float
            // in `[0,1)`) and `randomUUID()` (a fresh version-4 UUID string).
            | "rand"
            | "randomuuid"
            // Temporal value functions (task `00163`) — `timestamp()` (epoch
            // milliseconds as an Integer) and `datetime()` (the current UTC
            // instant as an ISO-8601 String; drevo's Cypher has no dedicated
            // temporal value type). Zero-argument and non-deterministic, like
            // `rand`/`randomUUID`. Needed by Neo4j-compatible clients that
            // stamp `created_at` / `updated_at` on writes (e.g. the Bolt
            // drop-in of the knowledge-graph MCP).
            | "timestamp"
            | "datetime"
            // Vector similarity (issue #202) — `cosine_similarity(a, b)`
            // returns the cosine SCORE of two numeric-list vectors (in
            // `[-1, 1]`) for `RETURN` / `ORDER BY`, complementing the
            // `similar()` threshold predicate.
            | "cosine_similarity"
    )
}

fn contains_aggregation(expr: &Expression) -> bool {
    match expr {
        Expression::FunctionCall { name, args, .. } => {
            is_aggregation_name(name) || args.iter().any(contains_aggregation)
        }
        Expression::Binary { lhs, rhs, .. } => {
            contains_aggregation(lhs) || contains_aggregation(rhs)
        }
        Expression::Unary { expr, .. } => contains_aggregation(expr),
        Expression::IsNull { expr, .. } => contains_aggregation(expr),
        Expression::In { expr, list, .. } => {
            contains_aggregation(expr) || contains_aggregation(list)
        }
        Expression::Property { base, .. } => contains_aggregation(base),
        Expression::List { items, .. } => items.iter().any(contains_aggregation),
        Expression::Map(m) => m.entries.iter().any(|(_, e)| contains_aggregation(e)),
        Expression::Case {
            scrutinee,
            arms,
            else_branch,
            ..
        } => {
            scrutinee
                .as_deref()
                .map(contains_aggregation)
                .unwrap_or(false)
                || arms
                    .iter()
                    .any(|(w, t)| contains_aggregation(w) || contains_aggregation(t))
                || else_branch
                    .as_deref()
                    .map(contains_aggregation)
                    .unwrap_or(false)
        }
        Expression::Index { base, index, .. } => {
            contains_aggregation(base) || contains_aggregation(index)
        }
        Expression::Slice { base, from, to, .. } => {
            contains_aggregation(base)
                || from.as_deref().map(contains_aggregation).unwrap_or(false)
                || to.as_deref().map(contains_aggregation).unwrap_or(false)
        }
        _ => false,
    }
}

/// Validator for expressions written in a `RETURN` projection.
///
/// Identical to [`validate_expr_supported`] except that **aggregation**
/// function calls (`count` / `sum` / `avg` / `min` / `max` / `collect`)
/// are allowed, and `*` is allowed only as the sole argument of `count`.
/// Non-aggregation function calls are accepted when they name a built-in
/// scalar function (`size`, `toLower`, … — `00138`) or a drevo extension
/// (`similar` / `keywords`); every other name is rejected with a pointer to
/// the future scalar-function task.
fn validate_expr_supported_in_projection(expr: &Expression) -> ExecResultT<()> {
    match expr {
        Expression::FunctionCall {
            name,
            distinct,
            args,
            span,
        } => {
            if !is_aggregation_name(name) {
                // Scalar functions (the `00138` built-ins plus `similar` /
                // `keywords`) are allowed in a projection; validate their
                // arguments and accept.
                if is_scalar_function_name(name) {
                    for arg in args {
                        validate_expr_supported(arg)?;
                    }
                    return Ok(());
                }
                return Err(ExecError::Unsupported {
                    feature: format!("function call `{}`", name.join(".")),
                    task: "future Phase 10 follow-up".into(),
                    span: *span,
                });
            }
            let lower = name[0].to_ascii_lowercase();
            // `count(*)` is the only context in which `Expression::Star`
            // is a legal sub-expression. Validate that special case
            // before the generic arg loop so the inner `Star` doesn't
            // hit the standalone-`*` reject branch.
            if lower == "count" && args.len() == 1 && matches!(args[0], Expression::Star(_)) {
                if *distinct {
                    return Err(ExecError::InvalidMutation(
                        "DISTINCT is not allowed with count(*)".into(),
                    ));
                }
                return Ok(());
            }
            // The percentile aggregations take `(value, fraction)`; every
            // other aggregation takes exactly one argument.
            let expected_arity = if is_percentile_aggregation(&lower) {
                2
            } else {
                1
            };
            if args.len() != expected_arity {
                return Err(ExecError::InvalidMutation(format!(
                    "aggregate `{}` takes exactly {} argument{}",
                    lower,
                    expected_arity,
                    if expected_arity == 1 { "" } else { "s" }
                )));
            }
            for arg in args {
                if contains_aggregation(arg) {
                    return Err(ExecError::InvalidMutation(format!(
                        "nested aggregations are not allowed inside `{}`",
                        lower
                    )));
                }
            }
            // Every argument must be a plain expression — no bare `*`.
            for arg in args {
                validate_expr_supported(arg)?;
            }
            Ok(())
        }
        Expression::Star(span) => Err(ExecError::Unsupported {
            feature: "`*` outside `count(*)`".into(),
            task: "future Phase 10 follow-up".into(),
            span: *span,
        }),
        Expression::Binary { lhs, rhs, .. } => {
            validate_expr_supported_in_projection(lhs)?;
            validate_expr_supported_in_projection(rhs)?;
            Ok(())
        }
        Expression::Unary { expr, .. } => validate_expr_supported_in_projection(expr),
        Expression::IsNull { expr, .. } => validate_expr_supported_in_projection(expr),
        Expression::In { expr, list, .. } => {
            validate_expr_supported_in_projection(expr)?;
            validate_expr_supported_in_projection(list)
        }
        Expression::Property { base, .. } => validate_expr_supported_in_projection(base),
        Expression::List { items, .. } => {
            for item in items {
                validate_expr_supported_in_projection(item)?;
            }
            Ok(())
        }
        Expression::Map(m) => {
            for (_, expr) in &m.entries {
                validate_expr_supported_in_projection(expr)?;
            }
            Ok(())
        }
        Expression::Case {
            scrutinee,
            arms,
            else_branch,
            ..
        } => {
            // CASE sub-expressions are validated with the *projection*
            // validator so that an aggregation nested inside any arm
            // (scrutinee, WHEN, THEN, or ELSE) is accepted — `eval_with_agg`
            // folds it over the group (`00142`). The projection validator
            // still rejects an aggregation nested *inside another*
            // aggregation, matching Neo4j.
            if let Some(s) = scrutinee {
                validate_expr_supported_in_projection(s)?;
            }
            for (when, then) in arms {
                validate_expr_supported_in_projection(when)?;
                validate_expr_supported_in_projection(then)?;
            }
            if let Some(e) = else_branch {
                validate_expr_supported_in_projection(e)?;
            }
            Ok(())
        }
        // Index / slice sub-expressions are validated with the
        // non-aggregation validator: `eval_with_agg` does not fold an
        // aggregation nested inside an index (`collect(x)[0]`), so such a
        // form stays `Unsupported` rather than silently producing a wrong
        // answer. (`CASE` arms, by contrast, *are* folded — see `00142`.)
        Expression::Index { base, index, .. } => {
            validate_expr_supported(base)?;
            validate_expr_supported(index)
        }
        Expression::Slice { base, from, to, .. } => {
            validate_expr_supported(base)?;
            if let Some(f) = from {
                validate_expr_supported(f)?;
            }
            if let Some(t) = to {
                validate_expr_supported(t)?;
            }
            Ok(())
        }
        // A list comprehension is a group key (it never *contains* an
        // aggregation — see `validate_expr_supported`), so its sub-expressions
        // are validated with the non-aggregation validator, mirroring the
        // Index / Slice handling above.
        Expression::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            validate_expr_supported(list)?;
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            if let Some(proj) = projection {
                validate_expr_supported(proj)?;
            }
            Ok(())
        }
        // A list predicate is likewise a group key — see the comprehension arm
        // above; both parts use the non-aggregation validator.
        Expression::ListPredicate {
            list, predicate, ..
        } => {
            validate_expr_supported(list)?;
            validate_expr_supported(predicate)
        }
        // `reduce` is likewise a group key (it never contains an aggregation);
        // its sub-expressions use the non-aggregation validator.
        Expression::Reduce {
            init, list, expr, ..
        } => {
            validate_expr_supported(init)?;
            validate_expr_supported(list)?;
            validate_expr_supported(expr)
        }
        // A map projection is a group key (it never *contains* an aggregation —
        // see `validate_expr_supported`); its base and literal-entry
        // sub-expressions use the non-aggregation validator, mirroring the
        // comprehension / Index / Slice handling above.
        Expression::MapProjection {
            base, selectors, ..
        } => {
            validate_expr_supported(base)?;
            for selector in selectors {
                if let MapProjectionSelector::Literal(_, expr) = selector {
                    validate_expr_supported(expr)?;
                }
            }
            Ok(())
        }
        // A pattern comprehension is a group key (it never *contains* an
        // aggregation — see `validate_expr_supported`); its predicate and
        // projection use the non-aggregation validator, mirroring the
        // comprehension / map-projection handling above.
        Expression::PatternComprehension {
            predicate,
            projection,
            ..
        } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            validate_expr_supported(projection)
        }
        // A pattern predicate is a group key (it never *contains* an
        // aggregation — its match is a runtime existence test), so it needs no
        // recursion, mirroring the comprehension / map-projection handling.
        Expression::PatternPredicate { .. } => Ok(()),
        // An existential subquery is likewise a group key (it never *contains*
        // an aggregation); its optional inner `WHERE` uses the non-aggregation
        // validator, mirroring the comprehension / map-projection handling.
        Expression::ExistsSubquery { predicate, .. } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
            }
            Ok(())
        }
        // A counting subquery is likewise a group key (it never *contains* an
        // aggregation — its `count` is a per-row match tally, not a fold over
        // the outer rows); its optional inner `WHERE` uses the non-aggregation
        // validator, mirroring the existential subquery.
        Expression::CountSubquery { predicate, .. } => {
            if let Some(pred) = predicate {
                validate_expr_supported(pred)?;
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
            Clause::Foreach(f) => f.span,
            Clause::Call(c) => c.span,
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
            Clause::With(w) => self.run_with(w),
            Clause::Unwind(u) => self.run_unwind(u),
            Clause::Foreach(f) => self.run_foreach(f),
            Clause::Call(c) => self.run_call(c),
        }
    }

    // ----- MATCH -----------------------------------------------------------

    fn run_match(&mut self, m: &MatchClause) -> ExecResultT<()> {
        // Process each prior binding independently so OPTIONAL MATCH
        // can fall back to a single all-null row per input when no
        // pattern matches (Cypher's left-join semantics — see
        // `synthesise_null_row` below).
        let mut new_bindings: Vec<Bindings> = Vec::new();
        let prior = std::mem::take(&mut self.bindings);
        let new_variables: Vec<String> = if m.optional {
            collect_pattern_variables(&m.patterns)
        } else {
            Vec::new()
        };

        for existing in prior.into_iter() {
            // The MATCH may contain multiple comma-separated patterns;
            // each one further multiplies the current binding row.
            let mut current = vec![existing.clone()];
            for pattern in &m.patterns {
                let mut next = Vec::new();
                for row in current.drain(..) {
                    for produced in self.match_named_pattern(pattern, &row)? {
                        next.push(produced);
                    }
                }
                current = next;
            }

            // WHERE is applied *per input row* — for OPTIONAL MATCH
            // a row whose pattern matched but failed WHERE is treated
            // as "no match", which then triggers the null-row
            // synthesis below (Cypher spec: WHERE on OPTIONAL MATCH
            // is part of the pattern, not a post-join filter).
            if let Some(expr) = &m.where_clause {
                let mut filtered: Vec<Bindings> = Vec::with_capacity(current.len());
                for row in current.drain(..) {
                    let value = self.eval(expr, &row)?;
                    match value {
                        Value::Bool(true) => filtered.push(row),
                        Value::Bool(false) | Value::Null => {}
                        other => {
                            return Err(ExecError::TypeMismatch {
                                expected: "Boolean".into(),
                                got: other.type_name().into(),
                                span: expr.span(),
                            });
                        }
                    }
                }
                current = filtered;
            }

            if m.optional && current.is_empty() {
                current.push(synthesise_null_row(&existing, &new_variables));
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
        // A `shortestPath(...)` / `allShortestPaths(...)` wrapper searches for
        // the shortest connecting path(s) rather than enumerating every match.
        if let Some(kind) = pattern.shortest {
            return self.match_shortest_pattern(pattern, kind, existing);
        }
        // A path variable triggers accumulation: `match_head` seeds the
        // reserved [`PATH_ACCUM_KEY`] entry and each segment extends it, so by
        // the time the rows return the full alternating node/relationship
        // sequence is recorded. We then rename that entry to the user's
        // variable. When there is no variable we skip the bookkeeping entirely.
        let build_path = pattern.variable.is_some();
        let mut rows = self.match_path(&pattern.path, existing, build_path)?;
        if let Some(var) = &pattern.variable {
            for row in rows.iter_mut() {
                if let Some(path) = row.remove(PATH_ACCUM_KEY) {
                    row.insert(var.clone(), path);
                }
            }
        }
        Ok(rows)
    }

    /// Match a `shortestPath(...)` / `allShortestPaths(...)` pattern.
    ///
    /// The wrapped pattern is a single variable-length leg `(a)-[*]-(b)`
    /// (validated upfront by [`validate_shortest_supported`], re-checked here
    /// defensively). Both endpoints are resolved exactly like an ordinary
    /// MATCH — a bound variable is reused, an unbound one is enumerated — then
    /// a breadth-first search finds the shortest connecting path(s). For
    /// `shortestPath` the first path at the minimum length is returned; for
    /// `allShortestPaths` every path of that minimum length yields a row.
    fn match_shortest_pattern(
        &self,
        pattern: &NamedPattern,
        kind: ShortestKind,
        existing: &Bindings,
    ) -> ExecResultT<Vec<Bindings>> {
        let path = &pattern.path;
        let segment = match path.tail.as_slice() {
            [seg] => seg,
            _ => {
                return Err(ExecError::InvalidFunctionCall {
                    name: shortest_fn_name(kind).into(),
                    message: "requires exactly one relationship between two nodes".into(),
                    span: path.head.span,
                });
            }
        };
        let rel = &segment.relationship;
        let (lo, hi) = match &rel.length {
            Some(RelLength::Exact(n)) => (*n, Some(*n)),
            Some(RelLength::Any) => (1, None),
            Some(RelLength::Range { from, to }) => (from.unwrap_or(1), *to),
            None => {
                return Err(ExecError::InvalidFunctionCall {
                    name: shortest_fn_name(kind).into(),
                    message: "requires a variable-length relationship, e.g. -[*]-".into(),
                    span: rel.span,
                });
            }
        };
        let lower = lo.max(0) as usize;
        let upper = hi.map(|h| h as usize).unwrap_or(VARLEN_DEFAULT_UPPER);

        // Resolve the source endpoints. `match_head` verifies a bound head
        // variable and enumerates an unbound one, binding it into the row;
        // the search builds its own path so we skip path accumulation here.
        let head_rows = self.match_head(&path.head, existing, /*build_path=*/ false)?;
        let path_var = pattern.variable.as_ref();
        let mut out: Vec<Bindings> = Vec::new();

        for (row, source) in head_rows {
            // Resolve the target endpoint(s): a bound tail variable searches
            // to exactly that node, an unbound one to every matching node.
            let targets: Vec<Arc<NodeValue>> = match &segment.node.variable {
                Some(name) => match row.get(name) {
                    Some(Value::Node(nv)) => {
                        if !node_matches_pattern(nv, &segment.node, &row, self)? {
                            continue;
                        }
                        vec![nv.clone()]
                    }
                    Some(other) => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Node".into(),
                            got: other.type_name().into(),
                            span: segment.node.span,
                        });
                    }
                    None => self.enumerate_nodes(&segment.node, &row)?,
                },
                None => self.enumerate_nodes(&segment.node, &row)?,
            };

            for target in targets {
                let found =
                    self.bfs_shortest_paths(&source, &target, rel, &row, lower, upper, kind)?;
                for (rels, nodes) in found {
                    let mut bindings = row.clone();
                    if let Some(name) = &segment.node.variable {
                        bindings.insert(name.clone(), Value::Node(target.clone()));
                    }
                    if let Some(name) = &rel.variable {
                        let list: Vec<Value> = rels
                            .iter()
                            .map(|e| Value::Relationship(e.clone()))
                            .collect();
                        bindings.insert(name.clone(), Value::List(list));
                    }
                    if let Some(var) = path_var {
                        let mut all_nodes = Vec::with_capacity(nodes.len() + 1);
                        all_nodes.push(source.clone());
                        all_nodes.extend(nodes.iter().cloned());
                        bindings.insert(
                            var.clone(),
                            Value::Path(Arc::new(PathValue {
                                nodes: all_nodes,
                                relationships: rels,
                            })),
                        );
                    }
                    out.push(bindings);
                }
            }
        }
        Ok(out)
    }

    /// Breadth-first search for the shortest path(s) from `source` to
    /// `target` over a variable-length relationship pattern `rel`.
    ///
    /// Returns the traversed `(relationships, intermediate-and-target nodes)`
    /// of each shortest path (the source node is *not* included in the node
    /// list — the caller prepends it). Cypher "trail" uniqueness applies (no
    /// relationship is reused within one path). BFS expands one hop per
    /// level, so the first level (at or above `lower`) that reaches `target`
    /// is the minimum length: [`ShortestKind::Single`] returns the first such
    /// path, [`ShortestKind::All`] every path at that level.
    #[allow(clippy::too_many_arguments)]
    fn bfs_shortest_paths(
        &self,
        source: &Arc<NodeValue>,
        target: &Arc<NodeValue>,
        rel: &RelationshipPattern,
        existing: &Bindings,
        lower: usize,
        upper: usize,
        kind: ShortestKind,
    ) -> ExecResultT<Vec<ShortestPath>> {
        let dir = rel.direction;

        struct State {
            node: Arc<NodeValue>,
            rels: Vec<Arc<RelationshipValue>>,
            nodes: Vec<Arc<NodeValue>>,
            used_ids: Vec<u64>,
        }

        let mut frontier: Vec<State> = vec![State {
            node: source.clone(),
            rels: Vec::new(),
            nodes: Vec::new(),
            used_ids: Vec::new(),
        }];

        for depth in 0..=upper {
            if depth >= lower {
                let mut found: Vec<ShortestPath> = Vec::new();
                for state in &frontier {
                    if state.node.id == target.id {
                        found.push((state.rels.clone(), state.nodes.clone()));
                        if matches!(kind, ShortestKind::Single) {
                            return Ok(found);
                        }
                    }
                }
                // BFS reaches the minimum length first, so the first level
                // with any hit holds *all* shortest paths.
                if !found.is_empty() {
                    return Ok(found);
                }
            }
            if depth == upper {
                break;
            }
            let mut next: Vec<State> = Vec::new();
            for state in &frontier {
                let edges = match dir {
                    AstDirection::Outgoing => self
                        .drevo
                        .edges_of(state.node.id, ModelDirection::Outgoing)?,
                    AstDirection::Incoming => self
                        .drevo
                        .edges_of(state.node.id, ModelDirection::Incoming)?,
                    AstDirection::Undirected => {
                        self.drevo.edges_of(state.node.id, ModelDirection::Both)?
                    }
                };
                for edge in edges {
                    if state.used_ids.contains(&edge.id) {
                        continue;
                    }
                    if !edge_matches_pattern(&edge, rel, existing, self)? {
                        continue;
                    }
                    match dir {
                        AstDirection::Outgoing if edge.from_id != state.node.id => continue,
                        AstDirection::Incoming if edge.to_id != state.node.id => continue,
                        _ => {}
                    }
                    let other_id = if edge.from_id == state.node.id {
                        edge.to_id
                    } else {
                        edge.from_id
                    };
                    let next_node = match self.drevo.get_node(other_id)? {
                        Some(n) => node_to_value(&n),
                        None => continue,
                    };
                    let mut rels = state.rels.clone();
                    let mut nodes = state.nodes.clone();
                    let mut used_ids = state.used_ids.clone();
                    rels.push(edge_to_value(&edge));
                    nodes.push(next_node.clone());
                    used_ids.push(edge.id);
                    next.push(State {
                        node: next_node,
                        rels,
                        nodes,
                        used_ids,
                    });
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok(Vec::new())
    }

    fn match_path(
        &self,
        path: &PathPattern,
        existing: &Bindings,
        build_path: bool,
    ) -> ExecResultT<Vec<Bindings>> {
        // Each in-progress row carries the *actual* endpoint node reached so
        // far, threaded forward directly rather than re-derived from the
        // bindings. This is what lets an **anonymous** head or intermediate
        // node — which binds no variable to look up later — still chain into
        // the next segment: `match_head` hands back the head node it just
        // matched, and every segment returns its target node as the new
        // endpoint. (Pattern lengths are short, so the extra `Arc` clone per
        // row is negligible.)
        let mut rows = self.match_head(&path.head, existing, build_path)?;
        for segment in &path.tail {
            let mut next: Vec<(Bindings, Arc<NodeValue>)> = Vec::new();
            for (row, prev_node) in rows.drain(..) {
                next.extend(self.match_segment(&prev_node, segment, &row, build_path)?);
            }
            rows = next;
        }
        Ok(rows.into_iter().map(|(bindings, _)| bindings).collect())
    }

    fn match_head(
        &self,
        head: &NodePattern,
        existing: &Bindings,
        build_path: bool,
    ) -> ExecResultT<Vec<(Bindings, Arc<NodeValue>)>> {
        // If the head's variable is already bound, just verify it
        // matches the requested label/properties — otherwise enumerate.
        // Each returned row is paired with the head node it matched so the
        // caller can thread it into the next segment.
        if let Some(name) = &head.variable {
            if let Some(value) = existing.get(name) {
                if let Value::Node(nv) = value {
                    if !node_matches_pattern(nv, head, existing, self)? {
                        return Ok(vec![]);
                    }
                    let mut row = existing.clone();
                    if build_path {
                        seed_path(&mut row, nv.clone());
                    }
                    return Ok(vec![(row, nv.clone())]);
                } else {
                    return Err(ExecError::TypeMismatch {
                        expected: "Node".into(),
                        got: value.type_name().into(),
                        span: head.span,
                    });
                }
            }
        }

        let candidates = self.enumerate_nodes(head, existing)?;
        let mut out = Vec::with_capacity(candidates.len());
        for nv in candidates {
            let mut bindings = existing.clone();
            if build_path {
                seed_path(&mut bindings, nv.clone());
            }
            if let Some(name) = &head.variable {
                bindings.insert(name.clone(), Value::Node(nv.clone()));
            }
            out.push((bindings, nv));
        }
        Ok(out)
    }

    fn enumerate_nodes(
        &self,
        pattern: &NodePattern,
        row: &Bindings,
    ) -> ExecResultT<Vec<Arc<NodeValue>>> {
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
            if !node_matches_pattern(&nv, pattern, row, self)? {
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
        build_path: bool,
    ) -> ExecResultT<Vec<(Bindings, Arc<NodeValue>)>> {
        let rel_pattern = &segment.relationship;
        // Variable-length path? Hand off to the BFS expander. The
        // single-hop fast path below still handles the common
        // `(a)-[:R]->(b)` and the explicit `[*1]` / `[*1..1]` cases.
        let (varlen_lo, varlen_hi) = match &rel_pattern.length {
            None => (None, None),
            Some(RelLength::Exact(n)) => (Some(*n), Some(*n)),
            Some(RelLength::Any) => (Some(1), None),
            Some(RelLength::Range { from, to }) => (Some(from.unwrap_or(1)), *to),
        };
        let is_varlen = match (varlen_lo, varlen_hi) {
            (None, _) => false, // no [*…] at all
            (Some(lo), Some(hi)) => lo != 1 || hi != 1,
            _ => true,
        };
        if is_varlen {
            return self.match_varlen_segment(
                prev_node,
                segment,
                existing,
                varlen_lo.unwrap_or(1),
                varlen_hi,
                build_path,
            );
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
            if !edge_matches_pattern(&edge, rel_pattern, existing, self)? {
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
            if !node_matches_pattern(&target, &segment.node, existing, self)? {
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
            if build_path {
                extend_path(&mut bindings, edge_to_value(&edge), target.clone());
            }
            out.push((bindings, target));
        }
        Ok(out)
    }

    /// BFS expansion for a variable-length path segment `[*lo..hi]`.
    ///
    /// Cypher "trail" uniqueness applies — no relationship is traversed
    /// twice within a single path; nodes may repeat. If the rel pattern
    /// has a variable, it's bound to a [`Value::List`] of the
    /// traversed relationships (one element per hop, source-order).
    ///
    /// When `hi == None` (unbounded above), we cap expansion at
    /// [`VARLEN_DEFAULT_UPPER`] to keep runaway cycles bounded even
    /// after trail-uniqueness — Neo4j's actual cap is much higher but
    /// drevo runs in-memory and the agentic-workload suite must stay
    /// fast. Tests can lower this with the dedicated unit covering
    /// trail uniqueness.
    fn match_varlen_segment(
        &self,
        src: &Arc<NodeValue>,
        segment: &crate::cypher::ast::PathSegment,
        existing: &Bindings,
        lo: i64,
        hi: Option<i64>,
        build_path: bool,
    ) -> ExecResultT<Vec<(Bindings, Arc<NodeValue>)>> {
        let rel_pattern = &segment.relationship;
        let dir = rel_pattern.direction;
        let upper = hi.map(|h| h as usize).unwrap_or(VARLEN_DEFAULT_UPPER);
        let lower = lo.max(0) as usize;

        // BFS frontier entries — each represents one in-progress path
        // ending at `node`, with the relationships already traversed
        // recorded for trail-uniqueness and for the optional rel
        // variable binding. `used_nodes` mirrors `used_edges` (the node
        // reached by each hop, excluding `src`) so a named path can record
        // every intermediate endpoint.
        struct VarlenState {
            node: Arc<NodeValue>,
            used_edges: Vec<Arc<RelationshipValue>>,
            used_nodes: Vec<Arc<NodeValue>>,
            used_ids: Vec<u64>,
        }

        let mut frontier: Vec<VarlenState> = vec![VarlenState {
            node: src.clone(),
            used_edges: Vec::new(),
            used_nodes: Vec::new(),
            used_ids: Vec::new(),
        }];
        let mut results: Vec<(Bindings, Arc<NodeValue>)> = Vec::new();

        for depth in 0..=upper {
            if depth >= lower {
                for state in &frontier {
                    if !node_matches_pattern(&state.node, &segment.node, existing, self)? {
                        continue;
                    }
                    let mut bindings = existing.clone();
                    if let Some(name) = &rel_pattern.variable {
                        let list: Vec<Value> = state
                            .used_edges
                            .iter()
                            .map(|e| Value::Relationship(e.clone()))
                            .collect();
                        if let Some(existing_val) = existing.get(name) {
                            // Pre-bound varlen rel variable would be
                            // exotic — keep behaviour deterministic by
                            // rejecting (mirrors single-hop logic
                            // returning `continue` on mismatch).
                            if let Value::List(prev) = existing_val {
                                if prev.len() != list.len() {
                                    continue;
                                }
                            } else {
                                return Err(ExecError::TypeMismatch {
                                    expected: "List".into(),
                                    got: existing_val.type_name().into(),
                                    span: rel_pattern.span,
                                });
                            }
                        }
                        bindings.insert(name.clone(), Value::List(list));
                    }
                    if let Some(name) = &segment.node.variable {
                        if let Some(existing_val) = existing.get(name) {
                            if let Value::Node(nv) = existing_val {
                                if nv.id != state.node.id {
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
                        bindings.insert(name.clone(), Value::Node(state.node.clone()));
                    }
                    if build_path {
                        // Append this segment's hops to the path accumulated up
                        // to `src` by `match_head` / earlier segments.
                        extend_path_multi(&mut bindings, &state.used_edges, &state.used_nodes);
                    }
                    results.push((bindings, state.node.clone()));
                }
            }
            if depth == upper {
                break;
            }
            // Expand the frontier by one hop.
            let mut next_frontier: Vec<VarlenState> = Vec::new();
            for state in &frontier {
                let edges = match dir {
                    AstDirection::Outgoing => self
                        .drevo
                        .edges_of(state.node.id, ModelDirection::Outgoing)?,
                    AstDirection::Incoming => self
                        .drevo
                        .edges_of(state.node.id, ModelDirection::Incoming)?,
                    AstDirection::Undirected => {
                        self.drevo.edges_of(state.node.id, ModelDirection::Both)?
                    }
                };
                for edge in edges {
                    if state.used_ids.contains(&edge.id) {
                        continue;
                    }
                    if !edge_matches_pattern(&edge, rel_pattern, existing, self)? {
                        continue;
                    }
                    match dir {
                        AstDirection::Outgoing if edge.from_id != state.node.id => continue,
                        AstDirection::Incoming if edge.to_id != state.node.id => continue,
                        _ => {}
                    }
                    let other_id = if edge.from_id == state.node.id {
                        edge.to_id
                    } else {
                        edge.from_id
                    };
                    let next_node = match self.drevo.get_node(other_id)? {
                        Some(n) => node_to_value(&n),
                        None => continue,
                    };
                    let mut next_used_edges = state.used_edges.clone();
                    let mut next_used_nodes = state.used_nodes.clone();
                    let mut next_used_ids = state.used_ids.clone();
                    next_used_edges.push(edge_to_value(&edge));
                    next_used_nodes.push(next_node.clone());
                    next_used_ids.push(edge.id);
                    next_frontier.push(VarlenState {
                        node: next_node,
                        used_edges: next_used_edges,
                        used_nodes: next_used_nodes,
                        used_ids: next_used_ids,
                    });
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }

        Ok(results)
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
                let path_value = self.create_path(&pattern.path, &mut row)?;
                if let Some(var) = &pattern.variable {
                    row.insert(var.clone(), Value::Path(Arc::new(path_value)));
                }
            }
            new_bindings.push(row);
        }
        self.bindings = new_bindings;
        Ok(())
    }

    /// Create the nodes and relationships of a pattern, returning the
    /// resulting [`PathValue`] so a named `CREATE p = …` can bind it.
    fn create_path(&mut self, path: &PathPattern, row: &mut Bindings) -> ExecResultT<PathValue> {
        let head_value = self.ensure_node_for_create(&path.head, row)?;
        let mut nodes = vec![head_value.clone()];
        let mut relationships = Vec::new();
        let mut prev_node = head_value;
        for segment in &path.tail {
            let target_value = self.ensure_node_for_create(&segment.node, row)?;
            let rel =
                self.create_relationship(&prev_node, &segment.relationship, &target_value, row)?;
            relationships.push(rel);
            nodes.push(target_value.clone());
            prev_node = target_value;
        }
        Ok(PathValue {
            nodes,
            relationships,
        })
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
    ) -> ExecResultT<Arc<RelationshipValue>> {
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
        let rv = edge_to_value(&stored);
        if let Some(name) = &rel.variable {
            row.insert(name.clone(), Value::Relationship(rv.clone()));
        }
        Ok(rv)
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
        let path_var = m.pattern.variable.as_ref();
        for existing in prior.into_iter() {
            // Try to MATCH the pattern first.
            let matched = self.match_path(&m.pattern.path, &existing, path_var.is_some())?;
            if !matched.is_empty() {
                for mut row in matched {
                    if let Some(var) = path_var {
                        if let Some(path) = row.remove(PATH_ACCUM_KEY) {
                            row.insert(var.clone(), path);
                        }
                    }
                    self.apply_set_items(&m.on_match, &mut row)?;
                    new_bindings.push(row);
                }
            } else {
                // No match — CREATE the pattern and run ON CREATE actions.
                let mut row = existing.clone();
                let created = self.create_path(&m.pattern.path, &mut row)?;
                if let Some(var) = path_var {
                    row.insert(var.clone(), Value::Path(Arc::new(created)));
                }
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
        let has_aggregation = projections.iter().any(contains_aggregation);
        let mut keyed: KeyedRows = if has_aggregation {
            self.aggregate_rows(&columns, &projections)?
        } else {
            let mut keyed = Vec::with_capacity(self.bindings.len());
            for binding in &self.bindings {
                let mut row = Vec::with_capacity(projections.len());
                for proj in &projections {
                    let value = self.eval(proj, binding)?;
                    row.push(value);
                }
                keyed.push((row, binding.clone()));
            }
            keyed
        };

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

    /// Run a `WITH` projection — the query-pipelining boundary.
    ///
    /// Mirrors `run_return` for the projection / ORDER BY / SKIP /
    /// LIMIT / DISTINCT pipeline, then:
    ///
    /// 1. Applies the trailing `WHERE` (post-projection filter — this
    ///    is how aggregation-before-filter works: `WITH count(*) AS c
    ///    WHERE c >= 2 …`).
    /// 2. Converts each surviving projected row back into a binding
    ///    keyed by the projection column names, so downstream clauses
    ///    see only the projected aliases — pattern variables that
    ///    weren't projected are dropped, matching Cypher's "WITH is
    ///    the only point at which the variable scope can be reshaped"
    ///    rule.
    fn run_with(&mut self, w: &crate::cypher::ast::WithClause) -> ExecResultT<()> {
        let (columns, projections) = self.resolve_projections(&w.items)?;
        let has_aggregation = projections.iter().any(contains_aggregation);
        let mut keyed: KeyedRows = if has_aggregation {
            self.aggregate_rows(&columns, &projections)?
        } else {
            let mut keyed = Vec::with_capacity(self.bindings.len());
            for binding in &self.bindings {
                let mut row = Vec::with_capacity(projections.len());
                for proj in &projections {
                    let value = self.eval(proj, binding)?;
                    row.push(value);
                }
                keyed.push((row, binding.clone()));
            }
            keyed
        };

        if !w.order_by.is_empty() {
            self.sort_keyed(&mut keyed, &w.order_by, &columns)?;
        }

        let mut rows: Vec<Vec<Value>> = keyed.into_iter().map(|(r, _)| r).collect();

        if w.distinct {
            dedup_rows(&mut rows);
        }

        if let Some(skip_expr) = &w.skip {
            let n = self.eval_usize(skip_expr, &HashMap::new())?;
            if n >= rows.len() {
                rows.clear();
            } else {
                rows.drain(..n);
            }
        }
        if let Some(limit_expr) = &w.limit {
            let n = self.eval_usize(limit_expr, &HashMap::new())?;
            if rows.len() > n {
                rows.truncate(n);
            }
        }

        // Convert projected rows back into bindings keyed by column
        // names so downstream clauses see exactly the projected scope.
        let mut new_bindings: Vec<Bindings> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut scope: Bindings = HashMap::new();
            for (col, val) in columns.iter().zip(row) {
                scope.insert(col.clone(), val);
            }
            new_bindings.push(scope);
        }

        // Post-projection WHERE — references aliased columns, applied
        // *after* aggregation so `WITH count(*) AS c WHERE c >= 2` is
        // the canonical aggregation-then-filter.
        if let Some(expr) = &w.where_clause {
            let mut filtered: Vec<Bindings> = Vec::with_capacity(new_bindings.len());
            for row in new_bindings.into_iter() {
                let value = self.eval(expr, &row)?;
                match value {
                    Value::Bool(true) => filtered.push(row),
                    Value::Bool(false) | Value::Null => {}
                    other => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Boolean".into(),
                            got: other.type_name().into(),
                            span: expr.span(),
                        });
                    }
                }
            }
            new_bindings = filtered;
        }

        self.bindings = new_bindings;
        Ok(())
    }

    // ----- UNWIND ----------------------------------------------------------

    /// `UNWIND list AS x` — expand a list expression into one binding
    /// row per element, carrying every existing binding forward and
    /// adding `x` bound to the element.
    ///
    /// Semantics mirror Neo4j:
    /// - `UNWIND [1, 2, 3] AS x` multiplies each input row by the list,
    ///   preserving element order.
    /// - `UNWIND [] AS x` drops the input row (zero elements → zero
    ///   rows).
    /// - `UNWIND null AS x` likewise yields zero rows — `null` is the
    ///   empty expansion, not a type error.
    /// - A non-list, non-null value is an [`ExecError::TypeMismatch`].
    ///
    /// A leading `UNWIND` works because [`execute`] seeds `bindings`
    /// with a single empty row; an `UNWIND` after a `MATCH` that
    /// produced no rows correctly yields nothing because there is no
    /// input row to expand (the seed row was already consumed).
    fn run_unwind(&mut self, u: &UnwindClause) -> ExecResultT<()> {
        let prior = std::mem::take(&mut self.bindings);
        let mut new_bindings: Vec<Bindings> = Vec::new();
        for row in &prior {
            let value = self.eval(&u.expression, row)?;
            match value {
                Value::List(items) => {
                    for item in items {
                        let mut next = row.clone();
                        next.insert(u.alias.clone(), item);
                        new_bindings.push(next);
                    }
                }
                // `UNWIND null` expands to zero rows (Neo4j semantics).
                Value::Null => {}
                other => {
                    return Err(ExecError::TypeMismatch {
                        expected: "List".into(),
                        got: other.type_name().into(),
                        span: u.expression.span(),
                    });
                }
            }
        }
        self.bindings = new_bindings;
        Ok(())
    }

    /// `CALL proc.name(args) [YIELD col [AS alias] … [WHERE pred]]`.
    ///
    /// Invokes a built-in read-only procedure and folds its output rows
    /// into the binding stream. With `YIELD`, each yielded column is bound
    /// (under its `AS` alias when present) as the cross-product of the
    /// prior rows with the procedure output — exactly like `UNWIND` — and
    /// an optional `WHERE` filters the result. Without `YIELD`, the call
    /// is a standalone query whose result columns are the procedure's full
    /// output signature.
    fn run_call(&mut self, c: &CallClause) -> ExecResultT<()> {
        let name = c.name.join(".");
        // `procedure_columns` / arity / yield-column validity were all
        // checked in the upfront sweep, so resolution here cannot fail on
        // a known procedure; fall back to the same error for safety.
        let columns = procedure_columns(&name).ok_or_else(|| ExecError::InvalidProcedureCall {
            name: name.clone(),
            message: "no such procedure".into(),
            span: c.span,
        })?;
        let output = self.invoke_procedure(&name, &c.args, c.span)?;

        match &c.yields {
            Some(items) => {
                // Map each yielded column name to its position in the
                // procedure's output signature.
                let prior = std::mem::take(&mut self.bindings);
                let mut new_bindings: Vec<Bindings> = Vec::new();
                for row in &prior {
                    for out_row in &output {
                        let mut next = row.clone();
                        for item in items {
                            let idx = columns
                                .iter()
                                .position(|col| *col == item.name)
                                .unwrap_or(0);
                            let key = item.alias.clone().unwrap_or_else(|| item.name.clone());
                            next.insert(key, out_row[idx].clone());
                        }
                        match &c.where_clause {
                            None => new_bindings.push(next),
                            Some(pred) => match self.eval(pred, &next)? {
                                Value::Bool(true) => new_bindings.push(next),
                                Value::Bool(false) | Value::Null => {}
                                other => {
                                    return Err(ExecError::TypeMismatch {
                                        expected: "Boolean".into(),
                                        got: other.type_name().into(),
                                        span: pred.span(),
                                    });
                                }
                            },
                        }
                    }
                }
                self.bindings = new_bindings;
            }
            None => {
                // Standalone call — project every output column directly
                // as the query result, mirroring Neo4j's bare `CALL`.
                self.result_columns = columns.iter().map(|c| c.to_string()).collect();
                self.result_rows = output;
            }
        }
        Ok(())
    }

    /// `CALL drevo.vector.query(label, property, query, k) YIELD node, score`
    /// (issue #202) — the top-`k` nodes of `label` ranked by cosine
    /// similarity between their `property` embedding and the `query` vector.
    ///
    /// Emits `(node, score)` rows (a `Value::Node` and a `Value::Float` in
    /// `[-1, 1]`), so the caller can `YIELD node, score`, post-filter with a
    /// `WHERE` on any node property (e.g. `node.book_id = $b`), and
    /// `RETURN … ORDER BY score DESC`.
    ///
    /// Semantics: this is a brute-force scan (score every `label` node that
    /// carries the `property`) — correct and sub-millisecond at per-book
    /// scale, per the issue. Nodes without the property, whose property is
    /// not a numeric list, or whose dimensionality does not match the query
    /// are skipped (like a `WHERE c.embedding IS NOT NULL` guard). The
    /// arguments are evaluated once against an empty binding, so they must
    /// be literals or parameters (`$query`), not references to variables
    /// bound by a preceding `MATCH`.
    ///
    /// NOTE: `k` is applied *before* any post-`YIELD` `WHERE`, so
    /// `… query(…, $k) YIELD node, score WHERE node.book_id = $b` returns the
    /// global top-`k` then filters. For pre-filtered per-book retrieval use
    /// the `cosine_similarity` scalar over a `MATCH (c:Chunk {book_id:$b})`.
    fn proc_vector_query(&self, args: &[Expression], span: Span) -> ExecResultT<Vec<Vec<Value>>> {
        // Arity (4) is already enforced by the upfront validation sweep.
        let empty = Bindings::new();
        let label = self.eval(&args[0], &empty)?;
        let label = label.as_string(span)?.to_string();
        let property = self.eval(&args[1], &empty)?;
        let property = property.as_string(span)?.to_string();
        let query_val = self.eval(&args[2], &empty)?;
        let query = similar_operand(&query_val, "drevo.vector.query", "query", span)?;
        let k = self.eval_usize(&args[3], &empty)?;

        let mut scored: Vec<(f32, Arc<NodeValue>)> = Vec::new();
        for node in self.drevo.collect_all_nodes()? {
            if !node_labels_from_storage(&node).iter().any(|l| l == &label) {
                continue;
            }
            let nv = node_to_value(&node);
            let Some(embedding_val) = nv.properties.get(&property) else {
                continue; // no embedding on this node — skip
            };
            let Ok(embedding) =
                similar_operand(embedding_val, "drevo.vector.query", "embedding", span)
            else {
                continue; // property is not a numeric list — skip
            };
            let Ok(score) = cosine_similarity(&embedding, &query) else {
                continue; // dimension mismatch / zero vector — skip
            };
            scored.push((score, nv));
        }

        // Highest cosine similarity first; ties broken by node id for a
        // deterministic order.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        scored.truncate(k);
        Ok(scored
            .into_iter()
            .map(|(score, nv)| vec![Value::Node(nv), Value::Float(f64::from(score))])
            .collect())
    }

    /// `CALL drevo.semantic.register(label, text_property, embedding_property,
    /// mode) YIELD label, text_property, embedding_property, state, mode`
    /// (#251 Phase 21 control plane).
    ///
    /// Registers (or re-enables) a server-side auto-embedding target: for nodes
    /// of `label`, the text in `text_property` should be embedded into
    /// `embedding_property`. `mode` is `'auto'` or `'manual'`. Returns the
    /// target's current control-plane record. The actual embedding is produced
    /// by a follow-up slice; this call records the intent and lets a client see
    /// the target via [`Self::proc_semantic_status`].
    fn proc_semantic_register(
        &self,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        // Arity (4) is enforced by the upfront validation sweep.
        let empty = Bindings::new();
        let label = self.eval(&args[0], &empty)?.as_string(span)?.to_string();
        let text_property = self.eval(&args[1], &empty)?.as_string(span)?.to_string();
        let embedding_property = self.eval(&args[2], &empty)?.as_string(span)?.to_string();
        let mode_str = self.eval(&args[3], &empty)?.as_string(span)?.to_string();
        let mode = IndexMode::parse(&mode_str).map_err(|e| ExecError::InvalidProcedureCall {
            name: "drevo.semantic.register".to_string(),
            message: e.to_string(),
            span,
        })?;
        let target = self
            .drevo
            .semantic_register(&label, &text_property, &embedding_property, mode, None)
            .map_err(|e| ExecError::InvalidProcedureCall {
                name: "drevo.semantic.register".to_string(),
                message: e.to_string(),
                span,
            })?;
        Ok(vec![semantic_index_row(&target)])
    }

    /// `CALL drevo.semantic.status() YIELD label, text_property,
    /// embedding_property, state, mode` (#251 Phase 21 control plane).
    ///
    /// One row per registered semantic-index target, so a client can introspect
    /// the control plane — detect that server-side auto-embedding is available
    /// and branch (fall back to an external embedder when the procedure is
    /// absent).
    fn proc_semantic_status(
        &self,
        _args: &[Expression],
        _span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        Ok(self
            .drevo
            .semantic_status()
            .iter()
            .map(semantic_index_row)
            .collect())
    }

    /// `CALL fts.search(query, k) YIELD node, score` (issue #208) — the top-`k`
    /// nodes matching `query` in the BM25 full-text index (task `00131`),
    /// ranked by relevance.
    ///
    /// Emits `(node, score)` rows so a Bolt/Cypher client gets scored
    /// full-text hits without client-side re-ranking, mirroring
    /// `drevo.vector.query` for the vector side:
    ///
    /// ```text
    /// CALL fts.search('anxious thoughts about work', 25) YIELD node, score
    /// WHERE node.group_id = $group_id
    /// RETURN node, score ORDER BY score DESC
    /// ```
    ///
    /// The score is the Okapi BM25 relevance already implemented in
    /// `search_fts`. As with `drevo.vector.query`, `k` is applied before any
    /// post-`YIELD WHERE`, and the arguments are evaluated once against an
    /// empty binding (so they must be literals or parameters).
    fn proc_fts_search(&self, args: &[Expression], span: Span) -> ExecResultT<Vec<Vec<Value>>> {
        // Arity (2) is already enforced by the upfront validation sweep.
        let empty = Bindings::new();
        let query = self.eval(&args[0], &empty)?;
        let query = query.as_string(span)?.to_string();
        let k = self.eval_usize(&args[1], &empty)?;

        // `search_fts` already returns nodes ranked by descending BM25 score.
        let hits = self.drevo.search_fts(&query, k)?;
        Ok(hits
            .into_iter()
            .map(|scored| {
                vec![
                    Value::Node(node_to_value(&scored.node)),
                    Value::Float(f64::from(scored.score)),
                ]
            })
            .collect())
    }

    /// `CALL fts.searchRelationships(query, k) YIELD rel, score` (#227-B) — the
    /// top-`k` relationships whose string properties best match `query`, ranked
    /// by BM25. The edge companion of [`Self::proc_fts_search`].
    fn proc_fts_search_relationships(
        &self,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        let empty = Bindings::new();
        let query = self.eval(&args[0], &empty)?;
        let query = query.as_string(span)?.to_string();
        let k = self.eval_usize(&args[1], &empty)?;

        let hits = self.drevo.search_fts_relationships(&query, k)?;
        Ok(hits
            .into_iter()
            .map(|scored| {
                vec![
                    Value::Relationship(edge_to_value(&scored.edge)),
                    Value::Float(f64::from(scored.score)),
                ]
            })
            .collect())
    }

    /// Run a built-in procedure and return its output rows, each a
    /// positional vector aligned with [`procedure_columns`].
    fn invoke_procedure(
        &self,
        name: &str,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        match name {
            "drevo.vector.query" => self.proc_vector_query(args, span),
            "drevo.semantic.register" => self.proc_semantic_register(args, span),
            "drevo.semantic.status" => self.proc_semantic_status(args, span),
            "fts.search" => self.proc_fts_search(args, span),
            "fts.searchRelationships" => self.proc_fts_search_relationships(args, span),
            "db.labels" => {
                let mut labels: Vec<String> = Vec::new();
                for node in self.drevo.collect_all_nodes()? {
                    for label in node_labels_from_storage(&node) {
                        if !labels.contains(&label) {
                            labels.push(label);
                        }
                    }
                }
                labels.sort();
                Ok(labels.into_iter().map(|l| vec![Value::String(l)]).collect())
            }
            "db.relationshipTypes" => {
                let mut kinds: Vec<String> = Vec::new();
                for edge in self.drevo.collect_all_edges()? {
                    if !kinds.contains(&edge.kind) {
                        kinds.push(edge.kind);
                    }
                }
                kinds.sort();
                Ok(kinds.into_iter().map(|k| vec![Value::String(k)]).collect())
            }
            "db.propertyKeys" => {
                let mut keys: Vec<String> = Vec::new();
                for node in self.drevo.collect_all_nodes()? {
                    for key in node_to_value(&node).properties.keys() {
                        if !keys.contains(key) {
                            keys.push(key.clone());
                        }
                    }
                }
                for edge in self.drevo.collect_all_edges()? {
                    for key in edge_to_value(&edge).properties.keys() {
                        if !keys.contains(key) {
                            keys.push(key.clone());
                        }
                    }
                }
                keys.sort();
                Ok(keys.into_iter().map(|k| vec![Value::String(k)]).collect())
            }
            // Unreachable for a procedure that passed the upfront sweep,
            // but keep a deterministic error rather than a panic.
            _ => Err(ExecError::InvalidProcedureCall {
                name: name.to_string(),
                message: "no such procedure".into(),
                span,
            }),
        }
    }

    /// `FOREACH (var IN list | update_clause …)` — for every current
    /// binding row, evaluate `list` and run the body update clauses once
    /// per element with `var` bound to it.
    ///
    /// `FOREACH` is a pure side-effecting clause: it never changes the
    /// outer cardinality. Each outer row passes through unchanged, and any
    /// variables the body introduces (e.g. `CREATE (n …)`) are scoped to
    /// the iteration and discarded afterwards — only the graph mutations
    /// persist. A `null` list is a no-op (zero iterations), mirroring
    /// `UNWIND null`; any other non-list value is a type error.
    fn run_foreach(&mut self, f: &ForeachClause) -> ExecResultT<()> {
        let outer = std::mem::take(&mut self.bindings);
        let mut result = Vec::with_capacity(outer.len());
        for row in outer {
            let value = self.eval(&f.list, &row)?;
            let items = match value {
                Value::List(items) => items,
                // `FOREACH (x IN null | …)` iterates zero times.
                Value::Null => Vec::new(),
                other => {
                    return Err(ExecError::TypeMismatch {
                        expected: "List".into(),
                        got: other.type_name().into(),
                        span: f.list.span(),
                    });
                }
            };
            for item in items {
                // Build a single-row context for this element: the outer
                // bindings plus the loop variable. Body clauses run
                // against it; whatever they leave in `self.bindings` is
                // thrown away (variable scoping), graph writes persist.
                let mut sub_row = row.clone();
                sub_row.insert(f.variable.clone(), item);
                self.bindings = vec![sub_row];
                for inner in &f.clauses {
                    self.run_clause(inner)?;
                }
            }
            result.push(row);
        }
        self.bindings = result;
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

    // ----- Aggregations (00066) -------------------------------------------

    /// Build the result rows for a `RETURN` clause that contains at
    /// least one aggregation function call.
    ///
    /// Grouping is implicit: each projection that does *not* contain an
    /// aggregation forms a group key. Rows with equal group keys are
    /// folded into one output row whose aggregation columns are the
    /// fold of the corresponding values across the group.
    ///
    /// Special case — pure aggregation with no group keys and zero
    /// input bindings still emits exactly one row (matching Neo4j),
    /// so e.g. `MATCH (n) RETURN count(*)` on an empty database
    /// returns `[0]`, not `[]`.
    fn aggregate_rows(
        &self,
        columns: &[String],
        projections: &[Expression],
    ) -> ExecResultT<KeyedRows> {
        let mut group_indices: Vec<usize> = Vec::new();
        let mut agg_indices: Vec<usize> = Vec::new();
        for (i, p) in projections.iter().enumerate() {
            if contains_aggregation(p) {
                agg_indices.push(i);
            } else {
                group_indices.push(i);
            }
        }

        // Group buckets, kept in input order so the result is
        // deterministic before any explicit ORDER BY.
        let mut groups: Vec<(Vec<Value>, Vec<Bindings>)> = Vec::new();
        for binding in &self.bindings {
            let mut key = Vec::with_capacity(group_indices.len());
            for &gi in &group_indices {
                key.push(self.eval(&projections[gi], binding)?);
            }
            if let Some(pos) = groups.iter().position(|(k, _)| k == &key) {
                groups[pos].1.push(binding.clone());
            } else {
                groups.push((key, vec![binding.clone()]));
            }
        }

        if group_indices.is_empty() && groups.is_empty() {
            // Pure aggregation over zero input rows — still emit one
            // synthetic group so e.g. `count(*)` returns 0.
            groups.push((Vec::new(), Vec::new()));
        }

        let mut out: KeyedRows = Vec::with_capacity(groups.len());
        for (key_values, rows_in_group) in groups {
            let mut row = vec![Value::Null; projections.len()];
            for (slot_idx, &gi) in group_indices.iter().enumerate() {
                row[gi] = key_values[slot_idx].clone();
            }
            for &ai in &agg_indices {
                row[ai] = self.eval_with_agg(&projections[ai], &rows_in_group)?;
            }
            // Build a synthetic per-row scope so the subsequent
            // ORDER BY can resolve column aliases. Raw pattern
            // variables (`n`, `r`) are *not* in scope after
            // aggregation — users must reference the projected alias.
            let mut scope: Bindings = HashMap::new();
            for (col, val) in columns.iter().zip(row.iter()) {
                scope.insert(col.clone(), val.clone());
            }
            out.push((row, scope));
        }
        Ok(out)
    }

    /// Evaluate a projection expression that contains an aggregation.
    ///
    /// Aggregation function calls fold across `group_rows`; everything
    /// else falls back to ordinary [`Self::eval`] against the first
    /// binding in the group (any binding works — non-aggregation
    /// variable references must be invariant across the group, which
    /// is guaranteed by treating non-aggregation projections as group
    /// keys).
    fn eval_with_agg(&self, expr: &Expression, group_rows: &[Bindings]) -> ExecResultT<Value> {
        match expr {
            Expression::FunctionCall {
                name,
                distinct,
                args,
                span,
            } if is_aggregation_name(name) => {
                self.eval_aggregate(name, *distinct, args, group_rows, *span)
            }
            Expression::Binary { op, lhs, rhs, span } => {
                let l = self.eval_with_agg(lhs, group_rows)?;
                let r = self.eval_with_agg(rhs, group_rows)?;
                eval_binary(*op, l, r, *span)
            }
            Expression::Unary { op, expr, span } => {
                let v = self.eval_with_agg(expr, group_rows)?;
                eval_unary(*op, v, *span)
            }
            Expression::IsNull { expr, negated, .. } => {
                let v = self.eval_with_agg(expr, group_rows)?;
                let is_null = matches!(v, Value::Null);
                Ok(Value::Bool(if *negated { !is_null } else { is_null }))
            }
            Expression::In { expr, list, span } => {
                let needle = self.eval_with_agg(expr, group_rows)?;
                let haystack = self.eval_with_agg(list, group_rows)?;
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
            Expression::Property { base, name, span } => {
                let base_value = self.eval_with_agg(base, group_rows)?;
                Ok(get_property(&base_value, name, *span))
            }
            Expression::List { items, .. } => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval_with_agg(item, group_rows)?);
                }
                Ok(Value::List(out))
            }
            // A `CASE` whose arms contain an aggregation (`00142`). Each
            // sub-expression is folded over the group via `eval_with_agg`,
            // so `CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END`
            // chooses on the aggregated count. A `CASE` with no aggregation
            // anywhere is a group key and never reaches this path (it is
            // evaluated by `eval` instead).
            Expression::Case {
                scrutinee,
                arms,
                else_branch,
                ..
            } => self.eval_case_with_agg(
                scrutinee.as_deref(),
                arms,
                else_branch.as_deref(),
                group_rows,
            ),
            // Leaf / non-aggregation forms — fall back to a
            // representative binding. We pick the first one in the
            // group; for pure-aggregation queries the group may be
            // empty, in which case bare variable references will
            // simply raise UnboundVariable, matching ordinary
            // ordering of error reporting.
            _ => {
                let empty: Bindings = HashMap::new();
                let scope = group_rows.first().unwrap_or(&empty);
                self.eval(expr, scope)
            }
        }
    }

    /// Fold one aggregation function over the bindings of one group.
    fn eval_aggregate(
        &self,
        name: &[String],
        distinct: bool,
        args: &[Expression],
        group_rows: &[Bindings],
        span: Span,
    ) -> ExecResultT<Value> {
        let func = name[0].to_ascii_lowercase();
        // `count(*)` short-circuits: count rows, ignore values.
        if func == "count" && args.len() == 1 && matches!(args[0], Expression::Star(_)) {
            return Ok(Value::Integer(group_rows.len() as i64));
        }
        // Every other aggregation takes exactly one argument and
        // null-skips. Collect the non-null per-row values up front.
        let arg = &args[0];
        let mut values: Vec<Value> = Vec::with_capacity(group_rows.len());
        for binding in group_rows {
            let v = self.eval(arg, binding)?;
            if !matches!(v, Value::Null) {
                values.push(v);
            }
        }
        if distinct {
            dedup_values(&mut values);
        }
        match func.as_str() {
            "count" => Ok(Value::Integer(values.len() as i64)),
            "sum" => {
                if values.is_empty() {
                    return Ok(Value::Integer(0));
                }
                let mut all_int = true;
                let mut sum_f = 0.0f64;
                for v in &values {
                    match v {
                        Value::Integer(i) => sum_f += *i as f64,
                        Value::Float(f) => {
                            sum_f += *f;
                            all_int = false;
                        }
                        other => {
                            return Err(ExecError::TypeMismatch {
                                expected: "Integer or Float".into(),
                                got: other.type_name().into(),
                                span,
                            });
                        }
                    }
                }
                if all_int {
                    Ok(Value::Integer(sum_f as i64))
                } else {
                    Ok(Value::Float(sum_f))
                }
            }
            "avg" => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                let mut sum_f = 0.0f64;
                for v in &values {
                    let n = v.as_number().ok_or_else(|| ExecError::TypeMismatch {
                        expected: "Integer or Float".into(),
                        got: v.type_name().into(),
                        span,
                    })?;
                    sum_f += n;
                }
                Ok(Value::Float(sum_f / values.len() as f64))
            }
            "min" => {
                let mut best: Option<Value> = None;
                for v in values {
                    best = Some(match best {
                        None => v,
                        Some(prev) => {
                            if matches!(compare_values(&prev, &v), std::cmp::Ordering::Greater) {
                                v
                            } else {
                                prev
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(Value::Null))
            }
            "max" => {
                let mut best: Option<Value> = None;
                for v in values {
                    best = Some(match best {
                        None => v,
                        Some(prev) => {
                            if matches!(compare_values(&prev, &v), std::cmp::Ordering::Less) {
                                v
                            } else {
                                prev
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(Value::Null))
            }
            "collect" => Ok(Value::List(values)),
            "stdev" | "stdevp" => {
                let nums = numeric_fold_values(&values, span)?;
                let n = nums.len();
                // `stDev` (sample) divides by `n - 1`, which is special-cased
                // to `0.0` for `n < 2`; `stDevP` (population) divides by `n`,
                // `0.0` for the empty group. Both match Neo4j.
                if n == 0 || (func == "stdev" && n < 2) {
                    return Ok(Value::Float(0.0));
                }
                let mean = nums.iter().sum::<f64>() / n as f64;
                let ss: f64 = nums.iter().map(|x| (x - mean) * (x - mean)).sum();
                let divisor = if func == "stdev" {
                    (n - 1) as f64
                } else {
                    n as f64
                };
                Ok(Value::Float((ss / divisor).sqrt()))
            }
            "percentilecont" | "percentiledisc" => {
                let fraction = self.eval_percentile_fraction(&func, args, group_rows, span)?;
                // Sort the (numeric) values ascending; an empty group is `null`.
                let mut sorted = values;
                for v in &sorted {
                    if v.as_number().is_none() {
                        return Err(ExecError::TypeMismatch {
                            expected: "Integer or Float".into(),
                            got: v.type_name().into(),
                            span,
                        });
                    }
                }
                if sorted.is_empty() {
                    return Ok(Value::Null);
                }
                sorted.sort_by(|a, b| {
                    a.as_number()
                        .partial_cmp(&b.as_number())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                if func == "percentiledisc" {
                    // Pick the actual stored value, preserving its type.
                    Ok(sorted[percentile_disc_index(fraction, sorted.len())].clone())
                } else {
                    let nums: Vec<f64> = sorted
                        .iter()
                        .map(|v| v.as_number().unwrap_or(0.0))
                        .collect();
                    Ok(Value::Float(percentile_cont(&nums, fraction)))
                }
            }
            _ => unreachable!("is_aggregation_name gated this call"),
        }
    }

    /// Evaluate and validate the percentile fraction (`args[1]`) of a
    /// `percentileCont` / `percentileDisc` call. It is a per-aggregation
    /// constant evaluated once (against any group row — it is row-invariant in
    /// practice, a literal or `$param`), and must be a number in `[0.0, 1.0]`;
    /// anything else is a recoverable [`ExecError::InvalidFunctionCall`].
    fn eval_percentile_fraction(
        &self,
        func: &str,
        args: &[Expression],
        group_rows: &[Bindings],
        span: Span,
    ) -> ExecResultT<f64> {
        if args.len() != 2 {
            return Err(ExecError::InvalidFunctionCall {
                name: func.to_string(),
                message: format!("`{func}` takes exactly two arguments (value, fraction)"),
                span,
            });
        }
        let empty: Bindings = HashMap::new();
        let scope = group_rows.first().unwrap_or(&empty);
        let raw = self.eval(&args[1], scope)?;
        let fraction = raw
            .as_number()
            .ok_or_else(|| ExecError::InvalidFunctionCall {
                name: func.to_string(),
                message: format!(
                    "percentile fraction must be a number between 0.0 and 1.0, got {}",
                    raw.type_name()
                ),
                span,
            })?;
        if !(0.0..=1.0).contains(&fraction) {
            return Err(ExecError::InvalidFunctionCall {
                name: func.to_string(),
                message: format!("percentile fraction must be between 0.0 and 1.0, got {fraction}"),
                span,
            });
        }
        Ok(fraction)
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
            Expression::FunctionCall {
                name, args, span, ..
            } => self.eval_scalar_function(name, args, row, *span),
            Expression::Case {
                scrutinee,
                arms,
                else_branch,
                ..
            } => self.eval_case(scrutinee.as_deref(), arms, else_branch.as_deref(), row),
            Expression::Star(span) => Err(ExecError::Unsupported {
                feature: "`*` outside `count(*)`".into(),
                task: "future Phase 10 follow-up".into(),
                span: *span,
            }),
            Expression::Index { base, index, span } => {
                let base_value = self.eval(base, row)?;
                let index_value = self.eval(index, row)?;
                eval_index(base_value, index_value, *span)
            }
            Expression::Slice {
                base,
                from,
                to,
                span,
            } => {
                let base_value = self.eval(base, row)?;
                let from_value = from.as_deref().map(|e| self.eval(e, row)).transpose()?;
                let to_value = to.as_deref().map(|e| self.eval(e, row)).transpose()?;
                eval_slice(base_value, from_value, to_value, *span)
            }
            Expression::ListComprehension {
                variable,
                list,
                predicate,
                projection,
                span,
            } => self.eval_list_comprehension(
                variable,
                list,
                predicate.as_deref(),
                projection.as_deref(),
                row,
                *span,
            ),
            Expression::ListPredicate {
                kind,
                variable,
                list,
                predicate,
                span,
            } => self.eval_list_predicate(*kind, variable, list, predicate, row, *span),
            Expression::Reduce {
                accumulator,
                init,
                variable,
                list,
                expr,
                span,
            } => self.eval_reduce(accumulator, init, variable, list, expr, row, *span),
            Expression::MapProjection {
                base,
                selectors,
                span,
            } => self.eval_map_projection(base, selectors, row, *span),
            Expression::PatternComprehension {
                pattern,
                predicate,
                projection,
                span,
            } => self.eval_pattern_comprehension(
                pattern,
                predicate.as_deref(),
                projection,
                row,
                *span,
            ),
            Expression::PatternPredicate { pattern, .. } => {
                self.eval_pattern_predicate(pattern, row)
            }
            Expression::ExistsSubquery {
                pattern,
                predicate,
                span,
            } => self.eval_exists_subquery(pattern, predicate.as_deref(), row, *span),
            Expression::CountSubquery {
                pattern,
                predicate,
                span,
            } => self.eval_count_subquery(pattern, predicate.as_deref(), row, *span),
        }
    }

    /// Evaluate a map projection `base { .key, .*, key: expr, var }`.
    ///
    /// `base` is evaluated in the current `row`. A `Null` base propagates to
    /// `Null` (so projecting an unmatched `OPTIONAL MATCH` variable yields
    /// `null`, not an error); a non-map / non-entity base is a recoverable
    /// [`ExecError::TypeMismatch`]. Selectors are applied in source order into
    /// a [`BTreeMap`], so a later selector overwrites an earlier key:
    ///
    /// * `.key` copies property `key` off the base (absent → `Null`),
    /// * `.*` copies every property of the base,
    /// * `key: expr` adds a computed entry (`expr` evaluated in `row`),
    /// * `var` is shorthand for `var: var` — the in-scope variable `var`
    ///   (unbound → [`ExecError::UnboundVariable`]).
    fn eval_map_projection(
        &self,
        base: &Expression,
        selectors: &[MapProjectionSelector],
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        let base_value = self.eval(base, row)?;
        // A `Null` base projects to `Null` (mirrors property access on null).
        if matches!(base_value, Value::Null) {
            return Ok(Value::Null);
        }
        let mut out: BTreeMap<String, Value> = BTreeMap::new();
        for selector in selectors {
            match selector {
                MapProjectionSelector::Property(key) => {
                    out.insert(key.clone(), get_property(&base_value, key, span));
                }
                MapProjectionSelector::AllProperties => {
                    let props =
                        base_properties(&base_value).ok_or_else(|| ExecError::TypeMismatch {
                            expected: "Node, Relationship, or Map".into(),
                            got: base_value.type_name().into(),
                            span,
                        })?;
                    for (k, v) in props {
                        out.insert(k.clone(), v.clone());
                    }
                }
                MapProjectionSelector::Literal(key, expr) => {
                    out.insert(key.clone(), self.eval(expr, row)?);
                }
                MapProjectionSelector::Variable(name) => {
                    let value =
                        row.get(name)
                            .cloned()
                            .ok_or_else(|| ExecError::UnboundVariable {
                                name: name.clone(),
                                span,
                            })?;
                    out.insert(name.clone(), value);
                }
            }
        }
        // A `.key` / `key: expr` / `var` projection does not require the base to
        // be a map (`var` ignores it entirely); only `.*` does, and it has been
        // type-checked above. But a scalar base with *only* such selectors is
        // still a misuse — Neo4j requires a map-like base — so reject it.
        if !matches!(
            base_value,
            Value::Map(_) | Value::Node(_) | Value::Relationship(_)
        ) {
            return Err(ExecError::TypeMismatch {
                expected: "Node, Relationship, or Map".into(),
                got: base_value.type_name().into(),
                span,
            });
        }
        Ok(Value::Map(out))
    }

    /// Evaluate a pattern comprehension `[ pattern WHERE pred | proj ]`.
    ///
    /// The `pattern` is matched relative to the current `row` via the same
    /// [`match_path`](Self::match_path) primitive that drives `MATCH`, so it is
    /// anchored on any variables already bound in `row` (the typical use:
    /// `(p)-[:WROTE]->(c)` where `p` comes from the surrounding query). Each
    /// match extends `row` with the pattern's freshly bound variables; the
    /// optional `predicate` filters those binding rows under `WHERE`'s
    /// three-valued logic (`true` keeps, `false`/`null` drops, a non-boolean is
    /// a recoverable [`ExecError::TypeMismatch`]) and `projection` is collected
    /// over each survivor into the result list. No match yields an empty list,
    /// and a head variable already bound to `null` — an unmatched
    /// `OPTIONAL MATCH` node — also yields an empty list rather than the
    /// `TypeMismatch` that anchoring a `MATCH` on a non-node would raise.
    fn eval_pattern_comprehension(
        &self,
        pattern: &PathPattern,
        predicate: Option<&Expression>,
        projection: &Expression,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        // A `null` anchor (e.g. an unmatched OPTIONAL MATCH head) → empty list,
        // matching Neo4j, instead of letting `match_head` raise a TypeMismatch.
        if let Some(name) = &pattern.head.variable {
            if matches!(row.get(name), Some(Value::Null)) {
                return Ok(Value::List(Vec::new()));
            }
        }
        let mut out = Vec::new();
        for binding in self.match_path(pattern, row, false)? {
            if let Some(pred) = predicate {
                match self.eval(pred, &binding)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) | Value::Null => continue,
                    other => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Bool".into(),
                            got: other.type_name().into(),
                            span,
                        })
                    }
                }
            }
            out.push(self.eval(projection, &binding)?);
        }
        Ok(Value::List(out))
    }

    /// Evaluate a pattern predicate `(a)-[:R]->(b)` — `true` iff at least one
    /// match of the path pattern exists relative to `row`.
    ///
    /// Existence is decided with the same [`match_path`](Self::match_path)
    /// primitive that drives `MATCH` and pattern comprehensions: the pattern is
    /// anchored on already-bound variables, extended into the graph, and the
    /// predicate is `true` as soon as one extended binding row survives (the
    /// matches themselves are discarded — only their existence matters, and the
    /// variables the pattern introduces stay scoped to the predicate). A head
    /// variable already bound to `null` — an unmatched `OPTIONAL MATCH` node —
    /// yields `null` under three-valued logic (matching Neo4j) rather than the
    /// `TypeMismatch` that anchoring a `MATCH` on a non-node would raise.
    fn eval_pattern_predicate(&self, pattern: &PathPattern, row: &Bindings) -> ExecResultT<Value> {
        if let Some(name) = &pattern.head.variable {
            if matches!(row.get(name), Some(Value::Null)) {
                return Ok(Value::Null);
            }
        }
        let exists = !self.match_path(pattern, row, false)?.is_empty();
        Ok(Value::Bool(exists))
    }

    /// Evaluate an existential subquery `EXISTS { [MATCH] pattern [WHERE pred] }`
    /// — `true` iff at least one match of `pattern` survives the optional inner
    /// `predicate`, relative to `row`.
    ///
    /// Like [`eval_pattern_predicate`](Self::eval_pattern_predicate), existence
    /// is decided with the same [`match_path`](Self::match_path) primitive that
    /// drives `MATCH`: the pattern is anchored on already-bound variables and
    /// extended into the graph, with the variables it introduces staying scoped
    /// to the subquery. The richer surface over a bare pattern predicate is the
    /// optional inner `WHERE`, applied per match in that match's binding scope
    /// under `WHERE`'s three-valued logic (`true` keeps, `false`/`null` drops, a
    /// non-boolean is a recoverable [`ExecError::TypeMismatch`]); the subquery is
    /// `true` as soon as one extended binding row survives it. A head variable
    /// already bound to `null` — an unmatched `OPTIONAL MATCH` node — yields
    /// `null` (matching Neo4j) rather than the `TypeMismatch` that anchoring a
    /// `MATCH` on a non-node would raise.
    fn eval_exists_subquery(
        &self,
        pattern: &PathPattern,
        predicate: Option<&Expression>,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        if let Some(name) = &pattern.head.variable {
            if matches!(row.get(name), Some(Value::Null)) {
                return Ok(Value::Null);
            }
        }
        for binding in self.match_path(pattern, row, false)? {
            match predicate {
                None => return Ok(Value::Bool(true)),
                Some(pred) => match self.eval(pred, &binding)? {
                    Value::Bool(true) => return Ok(Value::Bool(true)),
                    Value::Bool(false) | Value::Null => continue,
                    other => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Bool".into(),
                            got: other.type_name().into(),
                            span,
                        })
                    }
                },
            }
        }
        Ok(Value::Bool(false))
    }

    /// Evaluate a counting subquery `COUNT { [MATCH] pattern [WHERE pred] }`
    /// — the **number** of matches of `pattern` that survive the optional inner
    /// `predicate`, relative to `row`.
    ///
    /// The integer-valued sibling of
    /// [`eval_exists_subquery`](Self::eval_exists_subquery): it uses the same
    /// [`match_path`](Self::match_path) primitive that drives `MATCH`, anchoring
    /// the pattern on already-bound variables, with the variables it introduces
    /// scoped to the subquery. Rather than short-circuiting on the first match,
    /// it counts every extended binding row for which the optional inner `WHERE`
    /// holds (three-valued logic: `true` counts, `false`/`null` does not, a
    /// non-boolean is a recoverable [`ExecError::TypeMismatch`]) and returns the
    /// total as an [`Value::Integer`]. A head variable already bound to `null` —
    /// an unmatched `OPTIONAL MATCH` node — yields `null` (matching Neo4j) rather
    /// than the `TypeMismatch` that anchoring a `MATCH` on a non-node would
    /// raise; no match yields `0`.
    fn eval_count_subquery(
        &self,
        pattern: &PathPattern,
        predicate: Option<&Expression>,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        if let Some(name) = &pattern.head.variable {
            if matches!(row.get(name), Some(Value::Null)) {
                return Ok(Value::Null);
            }
        }
        let mut count: i64 = 0;
        for binding in self.match_path(pattern, row, false)? {
            match predicate {
                None => count += 1,
                Some(pred) => match self.eval(pred, &binding)? {
                    Value::Bool(true) => count += 1,
                    Value::Bool(false) | Value::Null => continue,
                    other => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Bool".into(),
                            got: other.type_name().into(),
                            span,
                        })
                    }
                },
            }
        }
        Ok(Value::Integer(count))
    }

    /// Evaluate a list comprehension `[var IN list WHERE pred | proj]`.
    ///
    /// The `list` expression is evaluated in the current `row`; a `Null` list
    /// propagates to `Null` (matching `UNWIND` / `IN` null handling) and a
    /// non-list is a recoverable [`ExecError::TypeMismatch`]. Each element is
    /// bound to `variable` in a *child* scope (a clone of `row` with the loop
    /// variable inserted, so it shadows any outer binding only for the duration
    /// of the comprehension), the optional `predicate` filters elements under
    /// `WHERE`'s three-valued logic (`true` keeps, `false`/`null` drops, a
    /// non-boolean is a type error), and `projection` (or the element itself
    /// when absent) is collected into the result list.
    #[allow(clippy::too_many_arguments)]
    fn eval_list_comprehension(
        &self,
        variable: &str,
        list: &Expression,
        predicate: Option<&Expression>,
        projection: Option<&Expression>,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        let items = match self.eval(list, row)? {
            Value::List(items) => items,
            Value::Null => return Ok(Value::Null),
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "List".into(),
                    got: other.type_name().into(),
                    span,
                })
            }
        };
        let mut out = Vec::new();
        for item in items {
            let mut scope = row.clone();
            scope.insert(variable.to_string(), item.clone());
            if let Some(pred) = predicate {
                match self.eval(pred, &scope)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) | Value::Null => continue,
                    other => {
                        return Err(ExecError::TypeMismatch {
                            expected: "Bool".into(),
                            got: other.type_name().into(),
                            span,
                        })
                    }
                }
            }
            match projection {
                Some(proj) => out.push(self.eval(proj, &scope)?),
                None => out.push(item),
            }
        }
        Ok(Value::List(out))
    }

    /// Evaluate a list predicate `kind(var IN list WHERE pred)`
    /// (`all` / `any` / `none` / `single`).
    ///
    /// The `list` is evaluated in the current `row`; a `Null` list propagates
    /// to `Null` (mirroring `UNWIND` / `IN` / the list comprehension) and a
    /// non-list is a recoverable [`ExecError::TypeMismatch`]. Each element is
    /// bound to `variable` in a child scope and `pred` is evaluated under
    /// `WHERE`'s three-valued logic — `true`, `false`, or `Null` (unknown);
    /// a non-boolean is a type error. The per-element results are folded with
    /// three-valued quantifier semantics so an unknown can make the whole
    /// predicate `Null`:
    ///
    /// * `all`    — `false` if any element is `false`, else `Null` if any is
    ///   unknown, else `true` (empty list → `true`).
    /// * `any`    — `true` if any element is `true`, else `Null` if any is
    ///   unknown, else `false` (empty list → `false`).
    /// * `none`   — the negation of `any` (empty list → `true`).
    /// * `single` — `true` iff exactly one element is `true` *and* no element
    ///   is unknown; more than one `true` is `false`; an unknown that could
    ///   tip the count yields `Null` (empty list → `false`).
    fn eval_list_predicate(
        &self,
        kind: ListPredicateKind,
        variable: &str,
        list: &Expression,
        predicate: &Expression,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        let items = match self.eval(list, row)? {
            Value::List(items) => items,
            Value::Null => return Ok(Value::Null),
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "List".into(),
                    got: other.type_name().into(),
                    span,
                })
            }
        };
        let mut trues = 0usize;
        let mut falses = 0usize;
        let mut nulls = 0usize;
        for item in items {
            let mut scope = row.clone();
            scope.insert(variable.to_string(), item);
            match self.eval(predicate, &scope)? {
                Value::Bool(true) => trues += 1,
                Value::Bool(false) => falses += 1,
                Value::Null => nulls += 1,
                other => {
                    return Err(ExecError::TypeMismatch {
                        expected: "Bool".into(),
                        got: other.type_name().into(),
                        span,
                    })
                }
            }
        }
        let result = match kind {
            ListPredicateKind::All => {
                if falses > 0 {
                    Value::Bool(false)
                } else if nulls > 0 {
                    Value::Null
                } else {
                    Value::Bool(true)
                }
            }
            ListPredicateKind::Any => {
                if trues > 0 {
                    Value::Bool(true)
                } else if nulls > 0 {
                    Value::Null
                } else {
                    Value::Bool(false)
                }
            }
            ListPredicateKind::None => {
                if trues > 0 {
                    Value::Bool(false)
                } else if nulls > 0 {
                    Value::Null
                } else {
                    Value::Bool(true)
                }
            }
            ListPredicateKind::Single => {
                if trues > 1 {
                    Value::Bool(false)
                } else if nulls > 0 {
                    // An unknown could change the true-count, so the exact-one
                    // verdict is itself unknown.
                    Value::Null
                } else {
                    Value::Bool(trues == 1)
                }
            }
        };
        Ok(result)
    }

    /// Evaluate a `reduce(acc = init, var IN list | expr)` left fold.
    ///
    /// The seed `init` is evaluated once in the current `row` to prime the
    /// accumulator. The `list` is then evaluated in `row`; a `Null` list
    /// propagates to `Null` (mirroring `UNWIND` / `IN` / the comprehension and
    /// predicate forms) and a non-list is a recoverable
    /// [`ExecError::TypeMismatch`]. Each element is bound to `variable` and the
    /// running accumulator to `accumulator` in a *child* scope (a clone of
    /// `row`, so both names shadow any outer binding only for the duration of
    /// the fold), and `expr` computes the next accumulator value. The final
    /// accumulator is returned; an empty list yields the seed unchanged.
    #[allow(clippy::too_many_arguments)]
    fn eval_reduce(
        &self,
        accumulator: &str,
        init: &Expression,
        variable: &str,
        list: &Expression,
        expr: &Expression,
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        let mut acc = self.eval(init, row)?;
        let items = match self.eval(list, row)? {
            Value::List(items) => items,
            Value::Null => return Ok(Value::Null),
            other => {
                return Err(ExecError::TypeMismatch {
                    expected: "List".into(),
                    got: other.type_name().into(),
                    span,
                })
            }
        };
        for item in items {
            let mut scope = row.clone();
            scope.insert(accumulator.to_string(), acc);
            scope.insert(variable.to_string(), item);
            acc = self.eval(expr, &scope)?;
        }
        Ok(acc)
    }

    /// Evaluate a `CASE` expression against a single binding row.
    ///
    /// * **Generic** form (`scrutinee` is `None`) — each `WHEN` condition is
    ///   a boolean predicate; the first arm whose condition is `true` wins.
    ///   `NULL` / `false` conditions are skipped, and a non-boolean condition
    ///   is an [`ExecError::TypeMismatch`] (matching `WHERE` semantics).
    /// * **Simple** form (`scrutinee` is `Some`) — the scrutinee is compared
    ///   for equality against each `WHEN` value; the first equal arm wins.
    ///   Equality uses Cypher three-valued logic ([`compare`]), so a `NULL`
    ///   scrutinee (or `NULL` arm value) never matches and falls through.
    ///
    /// If no arm matches, the `ELSE` value is returned, or `NULL` when there
    /// is no `ELSE`.
    fn eval_case(
        &self,
        scrutinee: Option<&Expression>,
        arms: &[(Expression, Expression)],
        else_branch: Option<&Expression>,
        row: &Bindings,
    ) -> ExecResultT<Value> {
        match scrutinee {
            Some(scrut_expr) => {
                let scrut = self.eval(scrut_expr, row)?;
                for (when, then) in arms {
                    let when_val = self.eval(when, row)?;
                    if matches!(
                        compare(BinaryOp::Eq, scrut.clone(), when_val, when.span())?,
                        Value::Bool(true)
                    ) {
                        return self.eval(then, row);
                    }
                }
            }
            None => {
                for (when, then) in arms {
                    match self.eval(when, row)? {
                        Value::Bool(true) => return self.eval(then, row),
                        Value::Bool(false) | Value::Null => {}
                        other => {
                            return Err(ExecError::TypeMismatch {
                                expected: "Boolean".into(),
                                got: other.type_name().into(),
                                span: when.span(),
                            });
                        }
                    }
                }
            }
        }
        match else_branch {
            Some(e) => self.eval(e, row),
            None => Ok(Value::Null),
        }
    }

    /// Evaluate a `CASE` expression whose arms may contain aggregations
    /// (`00142`), folding every sub-expression over the bindings of one
    /// group via [`eval_with_agg`](Self::eval_with_agg).
    ///
    /// Semantics are identical to [`eval_case`](Self::eval_case) — generic
    /// (boolean `WHEN`) and simple (scrutinee-equality) forms, three-valued
    /// `WHEN` handling, `NULL` on no-match-without-`ELSE` — the only
    /// difference is that each scrutinee / `WHEN` / `THEN` / `ELSE` is
    /// reduced across the group rather than read from a single row, so a
    /// nested `count(*)` / `sum(x)` / … contributes its aggregated value.
    fn eval_case_with_agg(
        &self,
        scrutinee: Option<&Expression>,
        arms: &[(Expression, Expression)],
        else_branch: Option<&Expression>,
        group_rows: &[Bindings],
    ) -> ExecResultT<Value> {
        match scrutinee {
            Some(scrut_expr) => {
                let scrut = self.eval_with_agg(scrut_expr, group_rows)?;
                for (when, then) in arms {
                    let when_val = self.eval_with_agg(when, group_rows)?;
                    if matches!(
                        compare(BinaryOp::Eq, scrut.clone(), when_val, when.span())?,
                        Value::Bool(true)
                    ) {
                        return self.eval_with_agg(then, group_rows);
                    }
                }
            }
            None => {
                for (when, then) in arms {
                    match self.eval_with_agg(when, group_rows)? {
                        Value::Bool(true) => return self.eval_with_agg(then, group_rows),
                        Value::Bool(false) | Value::Null => {}
                        other => {
                            return Err(ExecError::TypeMismatch {
                                expected: "Boolean".into(),
                                got: other.type_name().into(),
                                span: when.span(),
                            });
                        }
                    }
                }
            }
        }
        match else_branch {
            Some(e) => self.eval_with_agg(e, group_rows),
            None => Ok(Value::Null),
        }
    }

    /// Dispatch a non-aggregation (scalar) function call.
    ///
    /// Recognises `similar(...)` — drevo's joint graph+vector predicate
    /// (`00077`) — `keywords(...)` — BM25-IDF keyword extraction (`00132`) —
    /// and the built-in string / numeric / list library ([`call_scalar`],
    /// `00138`). Every other name stays [`ExecError::Unsupported`] so callers
    /// get a deterministic "not yet" rather than a silent wrong answer.
    fn eval_scalar_function(
        &self,
        name: &[String],
        args: &[Expression],
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        if name.len() == 1 && name[0].eq_ignore_ascii_case("similar") {
            return self.eval_similar(args, row, span);
        }
        if name.len() == 1 && name[0].eq_ignore_ascii_case("keywords") {
            return self.eval_keywords(args, row, span);
        }
        if name.len() == 1 && name[0].eq_ignore_ascii_case("startNode") {
            return self.eval_endpoint_node("startNode", Endpoint::Start, args, row, span);
        }
        if name.len() == 1 && name[0].eq_ignore_ascii_case("endNode") {
            return self.eval_endpoint_node("endNode", Endpoint::End, args, row, span);
        }
        if name.len() == 1 {
            let lower = name[0].to_ascii_lowercase();
            if is_builtin_scalar_function(&lower) {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(arg, row)?);
                }
                return call_scalar(&lower, values, span);
            }
        }
        Err(ExecError::Unsupported {
            feature: format!("function call `{}`", name.join(".")),
            task: "future Phase 10 follow-up".into(),
            span,
        })
    }

    /// Evaluate `startNode(rel)` / `endNode(rel)` (issue #232) — the source /
    /// target node of a relationship.
    ///
    /// A [`RelationshipValue`] carries only the endpoint *ids*, so the node is
    /// resolved through the graph ([`crate::db::Drevo::get_node`]); this is why
    /// the two functions live here (DB access) rather than in the pure
    /// [`call_scalar`] library. Semantics mirror Neo4j:
    ///
    /// * `NULL` argument → `NULL` (so it composes with `OPTIONAL MATCH`);
    /// * a non-relationship argument is a recoverable
    ///   [`ExecError::InvalidFunctionCall`];
    /// * a dangling endpoint (the node was deleted out from under the edge)
    ///   yields `NULL` rather than erroring.
    fn eval_endpoint_node(
        &self,
        fn_name: &str,
        which: Endpoint,
        args: &[Expression],
        row: &Bindings,
        span: Span,
    ) -> ExecResultT<Value> {
        if args.len() != 1 {
            return Err(ExecError::InvalidFunctionCall {
                name: fn_name.into(),
                message: format!("expected 1 argument (relationship), got {}", args.len()),
                span,
            });
        }
        match self.eval(&args[0], row)? {
            Value::Null => Ok(Value::Null),
            Value::Relationship(rv) => {
                let node_id = match which {
                    Endpoint::Start => rv.from_id,
                    Endpoint::End => rv.to_id,
                };
                match self.drevo.get_node(node_id)? {
                    Some(node) => Ok(Value::Node(node_to_value(&node))),
                    None => Ok(Value::Null),
                }
            }
            other => Err(ExecError::InvalidFunctionCall {
                name: fn_name.into(),
                message: format!("argument must be a Relationship, got {}", other.type_name()),
                span,
            }),
        }
    }

    /// Evaluate `keywords(text, k [, stem])` — the top-`k` salient terms of
    /// `text`, ranked by term-frequency × BM25 IDF over the indexed corpus.
    ///
    /// Returns a `Value::List` of `Value::String`. An optional third boolean
    /// argument enables Porter stemming (collapsing morphological variants);
    /// it defaults to `false`.
    ///
    /// `NULL` propagation mirrors `similar(...)`: a `NULL` `text` (most
    /// commonly a node that lacks the property) or a `NULL`/zero `k` yields
    /// an **empty list**, not an error — so scanning a heterogeneous label
    /// quietly skips rows with no text instead of aborting. This is also what
    /// lets the intended faceted idiom
    /// `MATCH (n) UNWIND keywords(n.body, 5) AS kw RETURN kw, count(*)`
    /// behave well once the `UNWIND` clause is implemented (a separate
    /// executor feature; `UNWIND` parses but is not yet executable). Wrong
    /// argument *types* (non-string text, non-integer `k`, non-boolean stem
    /// flag) are genuine errors ([`ExecError::InvalidFunctionCall`]).
    fn eval_keywords(&self, args: &[Expression], row: &Bindings, span: Span) -> ExecResultT<Value> {
        if args.len() != 2 && args.len() != 3 {
            return Err(ExecError::InvalidFunctionCall {
                name: "keywords".into(),
                message: format!(
                    "expected 2 or 3 arguments (text, k[, stem]), got {}",
                    args.len()
                ),
                span,
            });
        }

        let text = self.eval(&args[0], row)?;
        let k = self.eval(&args[1], row)?;

        // NULL text / k => no keywords (never an error).
        let text = match text {
            Value::Null => return Ok(Value::List(Vec::new())),
            Value::String(s) => s,
            other => {
                return Err(ExecError::InvalidFunctionCall {
                    name: "keywords".into(),
                    message: format!("text argument must be a String, got {}", other.type_name()),
                    span,
                })
            }
        };
        let k = match k {
            Value::Null => 0,
            // A negative count is meaningless; treat it as zero (empty list).
            Value::Integer(i) => i.max(0) as usize,
            other => {
                return Err(ExecError::InvalidFunctionCall {
                    name: "keywords".into(),
                    message: format!("k argument must be an Integer, got {}", other.type_name()),
                    span,
                })
            }
        };

        let stem = match args.get(2) {
            None => false,
            Some(expr) => match self.eval(expr, row)? {
                Value::Null => false,
                Value::Bool(b) => b,
                other => {
                    return Err(ExecError::InvalidFunctionCall {
                        name: "keywords".into(),
                        message: format!(
                            "stem argument must be a Boolean, got {}",
                            other.type_name()
                        ),
                        span,
                    })
                }
            },
        };

        let terms = crate::fts::keywords::extract_keywords(self.drevo.backend(), &text, k, stem)?;
        Ok(Value::List(terms.into_iter().map(Value::String).collect()))
    }

    /// Evaluate `similar(vector, query, threshold)` — `true` when the
    /// cosine similarity between the first two embeddings is at least
    /// `threshold`.
    ///
    /// Embeddings reach the executor as `Value::List` of numbers (a JSON
    /// array node property). A `NULL` in any argument — most commonly a
    /// node that simply lacks the embedding property — propagates to
    /// `NULL`, which `WHERE` treats as falsy; this lets a similarity
    /// filter scan a heterogeneous label without erroring on the nodes
    /// that have no vector. Genuine data errors (a non-list argument, a
    /// non-numeric element, a dimension mismatch, a zero-magnitude
    /// operand) surface as [`ExecError::InvalidFunctionCall`].
    fn eval_similar(&self, args: &[Expression], row: &Bindings, span: Span) -> ExecResultT<Value> {
        if args.len() != 3 {
            return Err(ExecError::InvalidFunctionCall {
                name: "similar".into(),
                message: format!(
                    "expected 3 arguments (vector, query, threshold), got {}",
                    args.len()
                ),
                span,
            });
        }
        let lhs = self.eval(&args[0], row)?;
        let rhs = self.eval(&args[1], row)?;
        let threshold = self.eval(&args[2], row)?;

        // NULL propagation: a missing embedding / query / threshold makes
        // the whole predicate NULL (falsy under WHERE), never an error.
        if matches!(lhs, Value::Null)
            || matches!(rhs, Value::Null)
            || matches!(threshold, Value::Null)
        {
            return Ok(Value::Null);
        }

        let a = similar_operand(&lhs, "similar", "vector", span)?;
        let b = similar_operand(&rhs, "similar", "query", span)?;
        let threshold = threshold
            .as_number()
            .ok_or_else(|| ExecError::InvalidFunctionCall {
                name: "similar".into(),
                message: format!("threshold must be a number, got {}", threshold.type_name()),
                span,
            })?;

        let score = cosine_similarity(&a, &b).map_err(|e| ExecError::InvalidFunctionCall {
            name: "similar".into(),
            message: e.to_string(),
            span,
        })?;
        Ok(Value::Bool(f64::from(score) >= threshold))
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

/// The output column signature of a built-in `CALL` procedure, or `None`
/// if `name` is not a known procedure.
///
/// drevo ships only read-only schema-introspection procedures. Each
/// returns a single column; the slice order defines the positional layout
/// [`Executor::invoke_procedure`] produces and the standalone (`YIELD`-less)
/// result columns.
fn procedure_columns(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "db.labels" => Some(&["label"]),
        "db.relationshipTypes" => Some(&["relationshipType"]),
        "db.propertyKeys" => Some(&["propertyKey"]),
        // Vector search (issue #202): top-k nodes by cosine similarity.
        "drevo.vector.query" => Some(&["node", "score"]),
        // Semantic-index control plane (#251 Phase 21): register / introspect
        // auto-embedding targets. Same column layout for both.
        "drevo.semantic.register" | "drevo.semantic.status" => Some(&[
            "label",
            "text_property",
            "embedding_property",
            "state",
            "mode",
        ]),
        // Full-text search (issue #208): BM25-ranked matching nodes.
        "fts.search" => Some(&["node", "score"]),
        // Relationship full-text search (issue #227-B): BM25-ranked edges.
        "fts.searchRelationships" => Some(&["rel", "score"]),
        _ => None,
    }
}

/// Number of positional arguments each built-in procedure takes. Used by
/// the upfront validation sweep to reject a mis-arity `CALL` before any
/// side effects run.
fn procedure_arity(name: &str) -> usize {
    match name {
        // db.vector.query(label, property, query, k)
        "drevo.vector.query" => 4,
        // drevo.semantic.register(label, text_property, embedding_property, mode)
        "drevo.semantic.register" => 4,
        // drevo.semantic.status() — no arguments.
        "drevo.semantic.status" => 0,
        // fts.search(query, k) / fts.searchRelationships(query, k)
        "fts.search" | "fts.searchRelationships" => 2,
        // The db.* introspection procedures take no arguments.
        _ => 0,
    }
}

/// Render one [`SemanticIndex`] target as a `drevo.semantic.*` output row,
/// matching the `["label","text_property","embedding_property","state","mode"]`
/// column layout in [`procedure_columns`].
fn semantic_index_row(target: &SemanticIndex) -> Vec<Value> {
    vec![
        Value::String(target.label.clone()),
        Value::String(target.text_property.clone()),
        Value::String(target.embedding_property.clone()),
        Value::String(target.state.as_str().to_string()),
        Value::String(target.mode.as_str().to_string()),
    ]
}

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

fn node_matches_pattern(
    nv: &Arc<NodeValue>,
    pattern: &NodePattern,
    row: &Bindings,
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
            // Property filters evaluate against the current binding row so a
            // filter may reference an already-bound variable (e.g. a FOREACH
            // loop variable in `MERGE (l:Label {title: lbl})`, or an outer
            // node in `MATCH (b {ref: a.id})`). Literals and `$params` are
            // row-independent, so this is a strict superset of the prior
            // empty-row behaviour.
            let expected = executor.eval(expr, row)?;
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
    row: &Bindings,
    executor: &Executor<'_>,
) -> ExecResultT<bool> {
    if !pattern.types.is_empty() && !pattern.types.iter().any(|t| t == &edge.kind) {
        return Ok(false);
    }
    if let Some(map) = &pattern.properties {
        let rv = edge_to_value(edge);
        for (k, expr) in &map.entries {
            // See `node_matches_pattern`: evaluate against the binding row so
            // a relationship-property filter can reference bound variables.
            let expected = executor.eval(expr, row)?;
            let actual = rv.properties.get(k).cloned().unwrap_or(Value::Null);
            if actual != expected {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Convert a `similar(...)` operand into a dense `Vec<f32>`.
///
/// The operand must be a `Value::List` whose every element is a number;
/// integer and float elements are both accepted (a JSON embedding array
/// may carry either). `which` names the argument (`"vector"` / `"query"`)
/// for the error message.
fn similar_operand(value: &Value, func: &str, which: &str, span: Span) -> ExecResultT<Vec<f32>> {
    let Value::List(items) = value else {
        return Err(ExecError::InvalidFunctionCall {
            name: func.into(),
            message: format!(
                "{which} argument must be a list of numbers, got {}",
                value.type_name()
            ),
            span,
        });
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let n = item
            .as_number()
            .ok_or_else(|| ExecError::InvalidFunctionCall {
                name: func.into(),
                message: format!(
                    "{which} argument element at index {index} is not a number (got {})",
                    item.type_name()
                ),
                span,
            })?;
        out.push(n as f32);
    }
    Ok(out)
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

/// The full property map of a map-like value — a node, relationship, or map.
///
/// Returns `None` for any other value, which the `.*` map-projection selector
/// turns into a [`ExecError::TypeMismatch`].
fn base_properties(base: &Value) -> Option<&BTreeMap<String, Value>> {
    match base {
        Value::Node(nv) => Some(&nv.properties),
        Value::Relationship(rv) => Some(&rv.properties),
        Value::Map(map) => Some(map),
        _ => None,
    }
}

// ===== List / map indexing & slicing (`00139`) ==============================
//
// `expr[index]` and `expr[from..to]` mirror Neo4j's element-access semantics:
//
// * **List index** — zero-based; a **negative** index counts from the end
//   (`xs[-1]` is the last element). An index **out of range** yields `NULL`
//   (never an error), so a speculative `xs[10]` over a short list is quietly
//   absent rather than fatal. The index must be an `Integer`.
// * **Map / node / relationship index** — `m[key]` with a `String` key is
//   exactly property access (it reuses [`get_property`]), returning `NULL` for
//   an absent key.
// * **Slice** — `xs[from..to]` is `from`-inclusive / `to`-exclusive,
//   zero-based, with negative bounds counting from the end and every bound
//   **clamped** into range (so `xs[-100..100]` is the whole list and
//   `from >= to` is the empty list). Either bound may be omitted
//   (`xs[..n]` / `xs[n..]` / `xs[..]`).
//
// `NULL` propagates: a `NULL` base, a `NULL` index, or a `NULL` slice bound all
// make the whole expression `NULL`. Genuine misuse — a non-integer list index,
// a non-string map key, or indexing/slicing a scalar — is a recoverable
// [`ExecError::TypeMismatch`].

/// Evaluate `base[index]` — a single list element or a map/entity field.
fn eval_index(base: Value, index: Value, span: Span) -> ExecResultT<Value> {
    // NULL on either side propagates (matches arithmetic / comparison).
    if matches!(base, Value::Null) || matches!(index, Value::Null) {
        return Ok(Value::Null);
    }
    match base {
        Value::List(items) => {
            let Value::Integer(i) = index else {
                return Err(ExecError::TypeMismatch {
                    expected: "Integer (list index)".into(),
                    got: index.type_name().into(),
                    span,
                });
            };
            Ok(list_element(&items, i))
        }
        Value::Map(_) | Value::Node(_) | Value::Relationship(_) => {
            let Value::String(key) = index else {
                return Err(ExecError::TypeMismatch {
                    expected: "String (map key)".into(),
                    got: index.type_name().into(),
                    span,
                });
            };
            Ok(get_property(&base, &key, span))
        }
        other => Err(ExecError::TypeMismatch {
            expected: "List or Map".into(),
            got: other.type_name().into(),
            span,
        }),
    }
}

/// Resolve a possibly-negative list index to an element, yielding `NULL` when
/// the (normalised) index falls outside the list.
fn list_element(items: &[Value], i: i64) -> Value {
    let len = items.len() as i64;
    let idx = if i < 0 { i + len } else { i };
    if idx < 0 || idx >= len {
        Value::Null
    } else {
        items[idx as usize].clone()
    }
}

/// Evaluate `base[from..to]`. `from` / `to` are already-evaluated bound values
/// (or `None` for an omitted bound).
fn eval_slice(
    base: Value,
    from: Option<Value>,
    to: Option<Value>,
    span: Span,
) -> ExecResultT<Value> {
    if matches!(base, Value::Null) {
        return Ok(Value::Null);
    }
    // A NULL bound makes the whole slice NULL (Neo4j semantics).
    if matches!(from, Some(Value::Null)) || matches!(to, Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let Value::List(items) = base else {
        return Err(ExecError::TypeMismatch {
            expected: "List".into(),
            got: base.type_name().into(),
            span,
        });
    };
    let len = items.len() as i64;
    let lo = match from {
        Some(v) => clamp_slice_bound(slice_bound(v, span)?, len),
        None => 0,
    };
    let hi = match to {
        Some(v) => clamp_slice_bound(slice_bound(v, span)?, len),
        None => len,
    };
    if lo >= hi {
        return Ok(Value::List(Vec::new()));
    }
    Ok(Value::List(items[lo as usize..hi as usize].to_vec()))
}

/// Extract an integer slice bound, rejecting any non-integer value.
fn slice_bound(v: Value, span: Span) -> ExecResultT<i64> {
    match v {
        Value::Integer(i) => Ok(i),
        other => Err(ExecError::TypeMismatch {
            expected: "Integer (slice bound)".into(),
            got: other.type_name().into(),
            span,
        }),
    }
}

/// Normalise a (possibly negative) slice bound and clamp it into `[0, len]`.
fn clamp_slice_bound(i: i64, len: i64) -> i64 {
    let idx = if i < 0 { i + len } else { i };
    idx.clamp(0, len)
}

// ===== Built-in scalar functions (`00138`) ==================================

/// Maximum number of elements [`scalar_range`] will materialise. `range` is
/// the one built-in that can request an unbounded allocation from small
/// inputs (`range(0, 1e18)`); the cap turns that into a recoverable error
/// instead of an out-of-memory abort.
const RANGE_MAX_ELEMENTS: i64 = 10_000_000;

/// Dispatch a built-in scalar function by its already-lowercased `name`.
///
/// `args` are the fully-evaluated argument [`Value`]s. With the sole
/// exception of `coalesce` (whose whole purpose is to skip `NULL`s), every
/// built-in is **NULL-propagating**: if any argument is `NULL` the result is
/// `NULL`, never an error — so a function applied across a heterogeneous scan
/// quietly yields `NULL` for the rows whose property is absent rather than
/// aborting the query. Genuine misuse (wrong arity, an argument of a type the
/// function cannot accept) is a recoverable [`ExecError::InvalidFunctionCall`].
fn call_scalar(name: &str, args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    // `coalesce` is the one non-propagating built-in: it returns its first
    // non-NULL argument, so it must see the raw NULLs.
    if name == "coalesce" {
        return scalar_coalesce(args, span);
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    match name {
        // ---- String ----
        "tolower" => str_map(name, args, span, |s| s.to_lowercase()),
        "toupper" => str_map(name, args, span, |s| s.to_uppercase()),
        "trim" => str_map(name, args, span, |s| s.trim().to_string()),
        "ltrim" => str_map(name, args, span, |s| s.trim_start().to_string()),
        "rtrim" => str_map(name, args, span, |s| s.trim_end().to_string()),
        "reverse" => scalar_reverse(args, span),
        "cosine_similarity" => scalar_cosine_similarity(args, span),
        "substring" => scalar_substring(args, span),
        "replace" => scalar_replace(args, span),
        "split" => scalar_split(args, span),
        "left" => scalar_left_right(name, args, span, true),
        "right" => scalar_left_right(name, args, span, false),
        "tostring" => scalar_tostring(args, span),
        // ---- Numeric ----
        "abs" => scalar_abs(args, span),
        "ceil" => float_map(name, args, span, f64::ceil),
        "floor" => float_map(name, args, span, f64::floor),
        "round" => scalar_round(args, span),
        "sqrt" => float_map(name, args, span, f64::sqrt),
        "sign" => scalar_sign(args, span),
        "tointeger" => scalar_to_integer(args, span),
        "tofloat" => scalar_to_float(args, span),
        "toboolean" => scalar_to_boolean(args, span),
        // ---- List value-conversion (task `00157`) ----
        "tointegerlist" => scalar_to_list(name, args, span, convert_to_integer_value),
        "tofloatlist" => scalar_to_list(name, args, span, convert_to_float_value),
        "tobooleanlist" => scalar_to_list(name, args, span, convert_to_boolean_value),
        "tostringlist" => scalar_to_list(name, args, span, convert_to_string_value),
        // ---- Fully-lenient scalar conversions (`*OrNull`, task `00158`) ----
        // Each applies the same lenient per-value conversion the list variants
        // use, returning `NULL` for any value it cannot convert. `toStringOrNull`
        // is the one with behaviour distinct from its strict sibling: scalar
        // `toString` errors on a non-stringifiable type, this yields `NULL`.
        "tointegerornull" => {
            scalar_or_null("toIntegerOrNull", args, span, convert_to_integer_value)
        }
        "tofloatornull" => scalar_or_null("toFloatOrNull", args, span, convert_to_float_value),
        "tobooleanornull" => {
            scalar_or_null("toBooleanOrNull", args, span, convert_to_boolean_value)
        }
        "tostringornull" => scalar_or_null("toStringOrNull", args, span, convert_to_string_value),
        // ---- Trigonometric / logarithmic (task `00156`) ----
        // Each folds a number to a `Float`; integer arguments widen and domain
        // edges (`log(-1)`, `asin(2)`, …) follow IEEE-754 (`NaN` / ±`Infinity`),
        // matching Neo4j, which returns the float rather than erroring.
        "e" => scalar_const("e", args, span, std::f64::consts::E),
        "pi" => scalar_const("pi", args, span, std::f64::consts::PI),
        "exp" => float_map(name, args, span, f64::exp),
        "log" => float_map(name, args, span, f64::ln),
        "log10" => float_map(name, args, span, f64::log10),
        "sin" => float_map(name, args, span, f64::sin),
        "cos" => float_map(name, args, span, f64::cos),
        "tan" => float_map(name, args, span, f64::tan),
        "cot" => float_map(name, args, span, |x| 1.0 / x.tan()),
        "asin" => float_map(name, args, span, f64::asin),
        "acos" => float_map(name, args, span, f64::acos),
        "atan" => float_map(name, args, span, f64::atan),
        "atan2" => scalar_atan2(args, span),
        "degrees" => float_map(name, args, span, f64::to_degrees),
        "radians" => float_map(name, args, span, f64::to_radians),
        "haversin" => float_map(name, args, span, |x| (1.0 - x.cos()) / 2.0),
        // ---- List / scalar ----
        "size" | "length" => scalar_size(name, args, span),
        "head" => scalar_head(args, span),
        "last" => scalar_last(args, span),
        "tail" => scalar_tail(args, span),
        "range" => scalar_range(args, span),
        "keys" => scalar_keys(args, span),
        "labels" => scalar_labels(args, span),
        "type" => scalar_type(args, span),
        "id" => scalar_id(args, span),
        "properties" => scalar_properties(args, span),
        "isempty" => scalar_is_empty(args, span),
        "isnan" => scalar_is_nan(args, span),
        "nodes" => scalar_nodes(args, span),
        "relationships" => scalar_relationships(args, span),
        // ---- Non-deterministic value functions (task `00161`) ----
        // Both take zero arguments (so the NULL-propagation guard above is a
        // no-op) and re-draw on every evaluation, matching Neo4j's per-row
        // semantics.
        "rand" => scalar_rand(args, span),
        "randomuuid" => scalar_random_uuid(args, span),
        "timestamp" => scalar_timestamp(args, span),
        "datetime" => scalar_datetime(args, span),
        // Unreachable: `is_builtin_scalar_function` gates entry, so any name
        // reaching here is a missing arm rather than user input.
        other => Err(ExecError::Unsupported {
            feature: format!("function call `{other}`"),
            task: "future Phase 10 follow-up".into(),
            span,
        }),
    }
}

/// Build an `invalid call` error for a built-in scalar function.
fn fn_err(name: &str, message: impl Into<String>, span: Span) -> ExecError {
    ExecError::InvalidFunctionCall {
        name: name.into(),
        message: message.into(),
        span,
    }
}

/// Assert a built-in received exactly `want` arguments.
fn expect_arity(name: &str, args: &[Value], want: usize, span: Span) -> ExecResultT<()> {
    if args.len() != want {
        return Err(fn_err(
            name,
            format!("expected {want} argument(s), got {}", args.len()),
            span,
        ));
    }
    Ok(())
}

/// Validate arity == 1 and return the single owned argument. Avoids the
/// `unwrap`/`expect` the crosscut audit forbids in library code while still
/// surfacing a recoverable arity error.
fn take_one(name: &str, args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    let mut iter = args.into_iter();
    match (iter.next(), iter.next()) {
        (Some(v), None) => Ok(v),
        (None, _) => Err(fn_err(name, "expected 1 argument, got 0", span)),
        (Some(_), Some(_)) => Err(fn_err(name, "expected 1 argument, got 2 or more", span)),
    }
}

/// Pull the single string argument of a one-arg string function.
fn one_string(name: &str, args: Vec<Value>, span: Span) -> ExecResultT<String> {
    match take_one(name, args, span)? {
        Value::String(s) => Ok(s),
        other => Err(fn_err(
            name,
            format!("argument must be a String, got {}", other.type_name()),
            span,
        )),
    }
}

/// One-argument `String -> String` mapping (`toLower`, `trim`, …).
fn str_map(
    name: &str,
    args: Vec<Value>,
    span: Span,
    f: impl Fn(&str) -> String,
) -> ExecResultT<Value> {
    let s = one_string(name, args, span)?;
    Ok(Value::String(f(&s)))
}

/// One-argument `Number -> Float` mapping (`ceil`, `floor`, `round`, `sqrt`).
fn float_map(
    name: &str,
    args: Vec<Value>,
    span: Span,
    f: impl Fn(f64) -> f64,
) -> ExecResultT<Value> {
    expect_arity(name, &args, 1, span)?;
    let n = args[0].as_number().ok_or_else(|| {
        fn_err(
            name,
            format!("argument must be a number, got {}", args[0].type_name()),
            span,
        )
    })?;
    Ok(Value::Float(f(n)))
}

/// Zero-argument numeric constant (`pi()`, `e()`). Validates the empty arity
/// and returns the constant as a `Float`.
fn scalar_const(name: &str, args: Vec<Value>, span: Span, value: f64) -> ExecResultT<Value> {
    expect_arity(name, &args, 0, span)?;
    Ok(Value::Float(value))
}

thread_local! {
    /// Per-thread PRNG state for `rand()`. Seeded lazily from the wall clock on
    /// first use, then advanced by [`next_rand_u64`] on every draw so successive
    /// `rand()` evaluations within one query yield independent values. A
    /// per-thread, non-cryptographic generator mirrors Neo4j's `rand()`, which
    /// is backed by `ThreadLocalRandom` rather than a secure source.
    static RAND_STATE: std::cell::Cell<u64> = std::cell::Cell::new(rand_seed());
}

/// Derive an initial PRNG seed from the wall clock, OR-ing in `1` so the state
/// is never zero (splitmix64 tolerates any seed, but a non-zero start avoids a
/// trivially-predictable first run). Falls back to the splitmix64 golden-ratio
/// constant if the clock predates the Unix epoch.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos | 1
}

/// Advance the thread-local [splitmix64](https://prng.di.unimi.it/splitmix64.c)
/// state and return the next 64-bit draw. The same well-distributed mixer the
/// HNSW index uses for level sampling.
fn next_rand_u64() -> u64 {
    RAND_STATE.with(|state| {
        let z = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        state.set(z);
        let mut z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// `rand()` — a uniformly-distributed random `Float` in the half-open interval
/// `[0.0, 1.0)`. Takes no arguments and re-draws on every evaluation (per row).
fn scalar_rand(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("rand", &args, 0, span)?;
    // Use the top 53 bits — the f64 mantissa width — so every representable
    // double in `[0,1)` is reachable and the quotient never rounds up to `1.0`.
    let bits = next_rand_u64() >> 11;
    Ok(Value::Float(bits as f64 / (1u64 << 53) as f64))
}

/// `randomUUID()` — a randomly-generated version-4 UUID rendered as the
/// canonical 36-character `8-4-4-4-12` lowercase-hex string. Takes no arguments
/// and mints a fresh, practically-unique value on every evaluation. Backed by
/// the OS CSPRNG (via the `uuid` crate's `getrandom` source), mirroring Neo4j's
/// `randomUUID()`, which draws from `SecureRandom`.
fn scalar_random_uuid(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("randomUUID", &args, 0, span)?;
    Ok(Value::String(uuid::Uuid::new_v4().to_string()))
}

/// `timestamp()` — the current wall-clock time as milliseconds since the Unix
/// epoch (an `Integer`). Zero-argument and non-deterministic (re-read on every
/// evaluation), mirroring Neo4j's `timestamp()`.
fn scalar_timestamp(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("timestamp", &args, 0, span)?;
    Ok(Value::Integer(unix_millis_now()))
}

/// `datetime()` — the current UTC instant as an ISO-8601 string
/// (`YYYY-MM-DDThh:mm:ss.sssZ`). drevo's Cypher has no dedicated temporal value
/// type, so the zero-argument `datetime()` constructor returns the canonical
/// ISO-8601 *string*: storable, Bolt-serialisable, and lexicographically
/// ordering like the instant itself — enough for the common "stamp
/// `created_at` / `updated_at`" use. Non-deterministic, like `timestamp()`.
fn scalar_datetime(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("datetime", &args, 0, span)?;
    Ok(Value::String(iso8601_utc(unix_millis_now())))
}

/// Current wall-clock time in milliseconds since the Unix epoch. A clock set
/// before 1970 clamps to `0` rather than panicking.
fn unix_millis_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Format a Unix-epoch millisecond instant as an ISO-8601 UTC string
/// (`YYYY-MM-DDThh:mm:ss.sssZ`) with no external date dependency — Howard
/// Hinnant's `civil_from_days` algorithm over the proleptic Gregorian calendar.
fn iso8601_utc(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let millis = epoch_ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // civil_from_days: days since 1970-01-01 -> (year, month, day).
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // day-of-era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year, [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// `atan2(y, x)` — the two-argument arctangent, returning the angle (in
/// radians) of the point `(x, y)` from the positive x-axis. Both arguments
/// must be numbers; the result is always a `Float`.
fn scalar_atan2(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("atan2", &args, 2, span)?;
    let y = args[0].as_number().ok_or_else(|| {
        fn_err(
            "atan2",
            format!("argument must be a number, got {}", args[0].type_name()),
            span,
        )
    })?;
    let x = args[1].as_number().ok_or_else(|| {
        fn_err(
            "atan2",
            format!("argument must be a number, got {}", args[1].type_name()),
            span,
        )
    })?;
    Ok(Value::Float(y.atan2(x)))
}

/// The rounding strategy of the three-argument [`scalar_round`] — the Cypher /
/// Java `RoundingMode` set. The `HALF_*` variants differ only in how an exact
/// halfway value is broken; the others ignore the magnitude of the dropped
/// remainder entirely.
#[derive(Clone, Copy)]
enum RoundingMode {
    /// Away from zero whenever any digit is dropped.
    Up,
    /// Toward zero — truncate the dropped digits.
    Down,
    /// Toward positive infinity.
    Ceiling,
    /// Toward negative infinity.
    Floor,
    /// Nearest neighbour; ties away from zero (the default).
    HalfUp,
    /// Nearest neighbour; ties toward zero.
    HalfDown,
    /// Nearest neighbour; ties to the even digit (banker's rounding).
    HalfEven,
}

impl RoundingMode {
    /// Parse the Cypher mode keyword, case-insensitively. Returns `None` for an
    /// unrecognised mode so the caller can raise a precise error.
    fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "UP" => Some(Self::Up),
            "DOWN" => Some(Self::Down),
            "CEILING" => Some(Self::Ceiling),
            "FLOOR" => Some(Self::Floor),
            "HALF_UP" => Some(Self::HalfUp),
            "HALF_DOWN" => Some(Self::HalfDown),
            "HALF_EVEN" => Some(Self::HalfEven),
            _ => None,
        }
    }

    /// Decide whether the retained digits must be incremented, given the first
    /// dropped digit (`round_digit`), whether any non-zero digit follows it
    /// (`any_after`), the sign of the value (`neg`), and the parity of the last
    /// retained digit (`last_kept_odd`, for `HALF_EVEN`).
    fn should_increment(
        self,
        neg: bool,
        round_digit: u8,
        any_after: bool,
        last_kept_odd: bool,
    ) -> bool {
        let any_dropped = round_digit != 0 || any_after;
        match self {
            Self::Up => any_dropped,
            Self::Down => false,
            Self::Ceiling => !neg && any_dropped,
            Self::Floor => neg && any_dropped,
            Self::HalfUp => round_digit >= 5,
            Self::HalfDown => round_digit > 5 || (round_digit == 5 && any_after),
            Self::HalfEven => round_digit > 5 || (round_digit == 5 && (any_after || last_kept_odd)),
        }
    }
}

/// `round(value [, precision [, mode]])` — round a number to a chosen number of
/// decimal places using a selectable rounding mode (task `00160`).
///
/// * `round(value)` rounds to the nearest integer, ties away from zero
///   (`HALF_UP`) — the long-standing one-argument behaviour.
/// * `round(value, precision)` rounds to `precision` decimal places, still
///   `HALF_UP`. A negative `precision` rounds to the left of the decimal point
///   (`round(1234.5, -2) = 1200.0`).
/// * `round(value, precision, mode)` selects the rounding mode — one of `UP`,
///   `DOWN`, `CEILING`, `FLOOR`, `HALF_UP`, `HALF_DOWN`, `HALF_EVEN`
///   (case-insensitive), matching Java's `RoundingMode` / Neo4j.
///
/// The result is always a `Float`. A non-finite `value` (`NaN` / ±`Infinity`)
/// is returned unchanged. NULL propagation is handled by [`call_scalar`].
fn scalar_round(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    if args.is_empty() || args.len() > 3 {
        return Err(fn_err(
            "round",
            format!("expected 1 to 3 arguments, got {}", args.len()),
            span,
        ));
    }
    let value = args[0].as_number().ok_or_else(|| {
        fn_err(
            "round",
            format!("argument must be a number, got {}", args[0].type_name()),
            span,
        )
    })?;
    let precision = match args.get(1) {
        None => 0,
        Some(v) => int_arg("round", v, "precision", span)?,
    };
    let mode = match args.get(2) {
        None => RoundingMode::HalfUp,
        Some(v) => {
            let name = string_arg("round", v, "mode", span)?;
            RoundingMode::from_name(name).ok_or_else(|| {
                fn_err(
                    "round",
                    format!(
                        "unknown rounding mode `{name}` (expected one of UP, DOWN, \
                         CEILING, FLOOR, HALF_UP, HALF_DOWN, HALF_EVEN)"
                    ),
                    span,
                )
            })?
        }
    };
    Ok(Value::Float(round_decimal(value, precision, mode)))
}

/// Round `value` to `precision` decimal places under `mode`, operating on the
/// *decimal* digits of the number rather than scaling the binary float.
///
/// Scaling by `10^precision` and rounding the product is the obvious approach,
/// but it inherits binary representation error: `1.255` is stored as the double
/// `1.2549999…`, so `(1.255 * 100).round() / 100` yields `1.25`, not the `1.26`
/// a user (and Neo4j's `BigDecimal`) expects. Instead we take the shortest
/// decimal string that round-trips to this double (Rust's `Display`, which is
/// always plain — never scientific — notation), round those digits exactly, and
/// reparse — recovering the intended decimal result.
fn round_decimal(value: f64, precision: i64, mode: RoundingMode) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let neg = value.is_sign_negative();
    // Magnitude as a plain decimal string, e.g. "1.255" or "1250".
    let s = format!("{}", value.abs());
    let (int_str, frac_str) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s.as_str(), ""),
    };
    let mut digits: Vec<u8> = int_str
        .bytes()
        .chain(frac_str.bytes())
        .map(|b| b - b'0')
        .collect();
    let int_len = int_str.len() as i64;

    // Number of leading digits to retain: the digit at the `precision`-th place
    // after the point is the last kept one, so we keep `int_len + precision`.
    let keep = int_len + precision;
    // Left-pad with zeros so the cut index is always at least 1 (gives carry a
    // digit to land in when rounding e.g. 0.6 up at the units place).
    let pad = if keep < 1 { (1 - keep) as usize } else { 0 };
    if pad > 0 {
        let mut padded = vec![0u8; pad];
        padded.append(&mut digits);
        digits = padded;
    }
    let cut = (keep + pad as i64) as usize;
    if cut >= digits.len() {
        // The value has no digit finer than `precision`; nothing to round off.
        return value;
    }

    let round_digit = digits[cut];
    let any_after = digits[cut + 1..].iter().any(|&d| d != 0);
    let mut kept = digits[..cut].to_vec();
    let last_kept_odd = kept[cut - 1] % 2 == 1;
    if mode.should_increment(neg, round_digit, any_after, last_kept_odd) {
        increment_digits(&mut kept);
    }

    // value = sign * kept * 10^(-precision); build that as an exponent string and
    // let the float parser do the (correctly-rounded) decimal→binary conversion.
    let mut out = String::with_capacity(kept.len() + 6);
    if neg {
        out.push('-');
    }
    for &d in &kept {
        out.push((b'0' + d) as char);
    }
    out.push('e');
    out.push_str(&(-precision).to_string());
    out.parse::<f64>().unwrap_or(value)
}

/// Add one to a big-endian decimal digit string in place, propagating the carry
/// and prepending a new leading `1` if the most-significant digit overflows
/// (`[9, 9] → [1, 0, 0]`).
fn increment_digits(digits: &mut Vec<u8>) {
    for d in digits.iter_mut().rev() {
        if *d == 9 {
            *d = 0;
        } else {
            *d += 1;
            return;
        }
    }
    digits.insert(0, 1);
}

/// `coalesce(a, b, …)` — the first non-`NULL` argument, or `NULL` if every
/// argument (and there must be at least one) is `NULL`.
fn scalar_coalesce(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    if args.is_empty() {
        return Err(fn_err("coalesce", "expected at least one argument", span));
    }
    Ok(args
        .into_iter()
        .find(|v| !matches!(v, Value::Null))
        .unwrap_or(Value::Null))
}

/// `reverse(x)` — reverses a String (by Unicode scalar value) or a List.
fn scalar_reverse(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("reverse", args, span)? {
        Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
        Value::List(mut items) => {
            items.reverse();
            Ok(Value::List(items))
        }
        other => Err(fn_err(
            "reverse",
            format!(
                "argument must be a String or List, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// `substring(original, start[, length])` — a 0-based, codepoint-indexed
/// slice. A `start` past the end yields the empty string; an omitted `length`
/// runs to the end. Negative `start` / `length` are rejected (Neo4j parity).
fn scalar_substring(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    if args.len() != 2 && args.len() != 3 {
        return Err(fn_err(
            "substring",
            format!("expected 2 or 3 arguments, got {}", args.len()),
            span,
        ));
    }
    let s = match &args[0] {
        Value::String(s) => s,
        other => {
            return Err(fn_err(
                "substring",
                format!("first argument must be a String, got {}", other.type_name()),
                span,
            ))
        }
    };
    let start = int_arg("substring", &args[1], "start", span)?;
    if start < 0 {
        return Err(fn_err("substring", "start must be non-negative", span));
    }
    let chars: Vec<char> = s.chars().collect();
    let start = (start as usize).min(chars.len());
    let end = match args.get(2) {
        None => chars.len(),
        Some(len_val) => {
            let len = int_arg("substring", len_val, "length", span)?;
            if len < 0 {
                return Err(fn_err("substring", "length must be non-negative", span));
            }
            start.saturating_add(len as usize).min(chars.len())
        }
    };
    Ok(Value::String(chars[start..end].iter().collect()))
}

/// `replace(original, search, replacement)` — replaces every non-overlapping
/// occurrence of `search`. An empty `search` leaves the string unchanged
/// (Neo4j parity), avoiding the surprising insert-between-every-char outcome.
fn scalar_replace(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("replace", &args, 3, span)?;
    let original = string_arg("replace", &args[0], "original", span)?;
    let search = string_arg("replace", &args[1], "search", span)?;
    let replacement = string_arg("replace", &args[2], "replacement", span)?;
    if search.is_empty() {
        return Ok(Value::String(original.to_string()));
    }
    Ok(Value::String(original.replace(search, replacement)))
}

/// `split(original, delimiter)` — splits into a List of String. An empty
/// delimiter splits into individual characters (Neo4j parity).
fn scalar_split(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("split", &args, 2, span)?;
    let original = string_arg("split", &args[0], "original", span)?;
    let delimiter = string_arg("split", &args[1], "delimiter", span)?;
    let parts: Vec<Value> = if delimiter.is_empty() {
        original
            .chars()
            .map(|c| Value::String(c.to_string()))
            .collect()
    } else {
        original
            .split(delimiter)
            .map(|p| Value::String(p.to_string()))
            .collect()
    };
    Ok(Value::List(parts))
}

/// `left(s, n)` / `right(s, n)` — the first / last `n` codepoints, clamped to
/// the string length. Negative `n` is rejected (Neo4j parity).
fn scalar_left_right(
    name: &str,
    args: Vec<Value>,
    span: Span,
    from_left: bool,
) -> ExecResultT<Value> {
    expect_arity(name, &args, 2, span)?;
    let s = string_arg(name, &args[0], "string", span)?;
    let n = int_arg(name, &args[1], "length", span)?;
    if n < 0 {
        return Err(fn_err(name, "length must be non-negative", span));
    }
    let chars: Vec<char> = s.chars().collect();
    let n = (n as usize).min(chars.len());
    let slice = if from_left {
        &chars[..n]
    } else {
        &chars[chars.len() - n..]
    };
    Ok(Value::String(slice.iter().collect()))
}

/// `toString(x)` — the string form of a Boolean, Integer, Float, or String.
/// Integral floats render with a trailing `.0` (Neo4j parity: `1.0`, not `1`).
fn scalar_tostring(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    let out = match take_one("toString", args, span)? {
        Value::Bool(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format_float(f),
        Value::String(s) => s,
        other => {
            return Err(fn_err(
                "toString",
                format!("cannot convert {} to a String", other.type_name()),
                span,
            ))
        }
    };
    Ok(Value::String(out))
}

/// `abs(n)` — preserves Integer vs Float. `abs(i64::MIN)` overflows and is a
/// recoverable error rather than a panic.
/// `cosine_similarity(a, b)` — cosine similarity of two numeric-list vectors,
/// as a Float in `[-1, 1]` (issue #202). Complements the `similar()` threshold
/// predicate by returning the SCORE, so scored retrieval can
/// `RETURN cosine_similarity(c.embedding, $q) AS score ORDER BY score DESC`.
/// NULL propagation is handled by the guard in [`call_scalar`]; a non-list
/// argument, dimension mismatch, or zero vector is a recoverable
/// [`ExecError::InvalidFunctionCall`].
fn scalar_cosine_similarity(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("cosine_similarity", &args, 2, span)?;
    let a = similar_operand(&args[0], "cosine_similarity", "first", span)?;
    let b = similar_operand(&args[1], "cosine_similarity", "second", span)?;
    let score = cosine_similarity(&a, &b).map_err(|e| ExecError::InvalidFunctionCall {
        name: "cosine_similarity".into(),
        message: e.to_string(),
        span,
    })?;
    Ok(Value::Float(f64::from(score)))
}

fn scalar_abs(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("abs", &args, 1, span)?;
    match &args[0] {
        Value::Integer(i) => i
            .checked_abs()
            .map(Value::Integer)
            .ok_or_else(|| fn_err("abs", "integer overflow", span)),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        other => Err(fn_err(
            "abs",
            format!("argument must be a number, got {}", other.type_name()),
            span,
        )),
    }
}

/// `sign(n)` — `-1`, `0`, or `1` as an Integer.
fn scalar_sign(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("sign", &args, 1, span)?;
    let n = args[0].as_number().ok_or_else(|| {
        fn_err(
            "sign",
            format!("argument must be a number, got {}", args[0].type_name()),
            span,
        )
    })?;
    let sign = if n > 0.0 {
        1
    } else if n < 0.0 {
        -1
    } else {
        0
    };
    Ok(Value::Integer(sign))
}

/// `isNaN(n)` — `true` when `n` is the IEEE-754 NaN value, `false` for any
/// other number. An `Integer` is never NaN, and `±Infinity` are numbers (not
/// NaN), so both yield `false`. A non-numeric argument is a recoverable error.
///
/// This is the only way to test for NaN in Cypher: per IEEE-754 `NaN = NaN`
/// is *false*, so an equality comparison can never catch it. It pairs with the
/// trigonometric / logarithmic library (task `00156`), whose domain edges
/// (`sqrt(-1)`, `log(-1)`, `asin(2)`, …) and float division (`0.0/0.0`)
/// produce exactly the NaN this detects. `NULL` propagates in `call_scalar`.
fn scalar_is_nan(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("isNaN", &args, 1, span)?;
    match &args[0] {
        Value::Integer(_) => Ok(Value::Bool(false)),
        Value::Float(f) => Ok(Value::Bool(f.is_nan())),
        other => Err(fn_err(
            "isNaN",
            format!("argument must be a number, got {}", other.type_name()),
            span,
        )),
    }
}

/// `toInteger(x)` — converts a number or numeric string to an Integer.
/// A Float truncates toward zero; an unparseable String or a Boolean yields
/// `NULL` (Neo4j parity — conversion is lenient, not an error).
fn scalar_to_integer(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    Ok(convert_to_integer_value(&take_one(
        "toInteger",
        args,
        span,
    )?))
}

/// Lenient single-value Integer conversion shared by `toInteger` and
/// `toIntegerList`: a Float truncates toward zero, a numeric String parses,
/// and anything else (Boolean, List, Map, `NULL`, …) yields `NULL`.
fn convert_to_integer_value(v: &Value) -> Value {
    match v {
        Value::Integer(i) => Value::Integer(*i),
        Value::Float(f) if f.is_finite() => Value::Integer(f.trunc() as i64),
        Value::Float(_) => Value::Null,
        Value::String(s) => parse_integer(s),
        _ => Value::Null,
    }
}

/// Parse a string into an Integer `Value`, falling back to truncating a
/// float-formatted string. Unparseable input yields `NULL`.
fn parse_integer(s: &str) -> Value {
    let trimmed = s.trim();
    if let Ok(i) = trimmed.parse::<i64>() {
        return Value::Integer(i);
    }
    match trimmed.parse::<f64>() {
        Ok(f) if f.is_finite() => Value::Integer(f.trunc() as i64),
        _ => Value::Null,
    }
}

/// `toFloat(x)` — converts a number or numeric string to a Float. An
/// unparseable String or a Boolean yields `NULL` (Neo4j parity).
fn scalar_to_float(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    Ok(convert_to_float_value(&take_one("toFloat", args, span)?))
}

/// Lenient single-value Float conversion shared by `toFloat` and `toFloatList`.
fn convert_to_float_value(v: &Value) -> Value {
    match v {
        Value::Integer(i) => Value::Float(*i as f64),
        Value::Float(f) => Value::Float(*f),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(f) => Value::Float(f),
            Err(_) => Value::Null,
        },
        _ => Value::Null,
    }
}

/// `toBoolean(x)` — Boolean passthrough; `"true"`/`"false"` (case-insensitive,
/// trimmed) from a String; `0`/`1` from an Integer; anything else yields
/// `NULL` (Neo4j parity).
fn scalar_to_boolean(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    Ok(convert_to_boolean_value(&take_one(
        "toBoolean",
        args,
        span,
    )?))
}

/// Lenient single-value Boolean conversion shared by `toBoolean` and
/// `toBooleanList`.
fn convert_to_boolean_value(v: &Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(*b),
        Value::Integer(0) => Value::Bool(false),
        Value::Integer(1) => Value::Bool(true),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

/// Lenient single-value String conversion used by `toStringList`. Unlike the
/// scalar `toString` (which *errors* on a non-stringifiable type), the list
/// variant follows Neo4j and yields `NULL` for an unconvertible element so the
/// returned list keeps its length.
fn convert_to_string_value(v: &Value) -> Value {
    match v {
        Value::Bool(b) => Value::String(b.to_string()),
        Value::Integer(i) => Value::String(i.to_string()),
        Value::Float(f) => Value::String(format_float(*f)),
        Value::String(s) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

/// `toIntegerList` / `toFloatList` / `toBooleanList` / `toStringList` — apply a
/// lenient single-value conversion to every element of a List. An element that
/// cannot be converted (including a `NULL` element) becomes `NULL`, so the
/// result always has the same length as the input. A non-`List` argument is a
/// recoverable error (a `NULL` argument is already short-circuited to `NULL`
/// by [`call_scalar`]).
fn scalar_to_list(
    name: &str,
    args: Vec<Value>,
    span: Span,
    convert: fn(&Value) -> Value,
) -> ExecResultT<Value> {
    match take_one(name, args, span)? {
        Value::List(items) => Ok(Value::List(items.iter().map(convert).collect())),
        other => Err(fn_err(
            name,
            format!("argument must be a List, got {}", other.type_name()),
            span,
        )),
    }
}

/// `toIntegerOrNull` / `toFloatOrNull` / `toBooleanOrNull` / `toStringOrNull`
/// (task `00158`) — the fully-lenient Neo4j 5 siblings of the scalar
/// conversions. Each applies the same per-value converter the list-conversion
/// functions use, so any value that cannot be converted yields `NULL` rather
/// than an error. A `NULL` argument is already short-circuited to `NULL` by
/// [`call_scalar`]; only the arity check remains here.
fn scalar_or_null(
    name: &str,
    args: Vec<Value>,
    span: Span,
    convert: fn(&Value) -> Value,
) -> ExecResultT<Value> {
    Ok(convert(&take_one(name, args, span)?))
}

/// `size(x)` / `length(x)` — the element count of a List or the codepoint
/// count of a String, as an Integer.
fn scalar_size(name: &str, args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity(name, &args, 1, span)?;
    match &args[0] {
        Value::List(items) => Ok(Value::Integer(items.len() as i64)),
        Value::String(s) => Ok(Value::Integer(s.chars().count() as i64)),
        // `length(path)` is the number of relationships (hops) — Neo4j's
        // canonical use of `length`. `size(path)` resolves the same way here.
        Value::Path(p) => Ok(Value::Integer(p.length() as i64)),
        other => Err(fn_err(
            name,
            format!(
                "argument must be a List, String, or Path, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// `isEmpty(x)` — `true` when the container `x` holds no elements, for the
/// three Neo4j container types: a String (no characters), a List (no items),
/// or a Map (no entries). A `NULL` argument propagates to `NULL` before this
/// helper is reached (see [`call_scalar`]); any non-container value is a
/// recoverable `InvalidFunctionCall`. This fills the gap left by [`scalar_size`],
/// which rejects a Map and so cannot express `size(m) = 0`.
fn scalar_is_empty(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("isEmpty", args, span)? {
        Value::String(s) => Ok(Value::Bool(s.is_empty())),
        Value::List(items) => Ok(Value::Bool(items.is_empty())),
        Value::Map(entries) => Ok(Value::Bool(entries.is_empty())),
        other => Err(fn_err(
            "isEmpty",
            format!(
                "argument must be a String, List, or Map, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// `nodes(path)` — the nodes of a path as a List, in traversal order.
fn scalar_nodes(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("nodes", args, span)? {
        Value::Path(p) => Ok(Value::List(
            p.nodes.iter().cloned().map(Value::Node).collect(),
        )),
        other => Err(fn_err(
            "nodes",
            format!("argument must be a Path, got {}", other.type_name()),
            span,
        )),
    }
}

/// `relationships(path)` — the relationships of a path as a List, in
/// traversal order.
fn scalar_relationships(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("relationships", args, span)? {
        Value::Path(p) => Ok(Value::List(
            p.relationships
                .iter()
                .cloned()
                .map(Value::Relationship)
                .collect(),
        )),
        other => Err(fn_err(
            "relationships",
            format!("argument must be a Path, got {}", other.type_name()),
            span,
        )),
    }
}

/// `head(list)` — the first element, or `NULL` for an empty list.
fn scalar_head(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("head", args, span)? {
        Value::List(items) => Ok(items.into_iter().next().unwrap_or(Value::Null)),
        other => Err(fn_err(
            "head",
            format!("argument must be a List, got {}", other.type_name()),
            span,
        )),
    }
}

/// `last(list)` — the final element, or `NULL` for an empty list.
fn scalar_last(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("last", args, span)? {
        Value::List(items) => Ok(items.into_iter().last().unwrap_or(Value::Null)),
        other => Err(fn_err(
            "last",
            format!("argument must be a List, got {}", other.type_name()),
            span,
        )),
    }
}

/// `tail(list)` — every element except the first (empty list for a 0/1-element
/// input).
fn scalar_tail(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("tail", args, span)? {
        Value::List(mut items) => {
            if !items.is_empty() {
                items.remove(0);
            }
            Ok(Value::List(items))
        }
        other => Err(fn_err(
            "tail",
            format!("argument must be a List, got {}", other.type_name()),
            span,
        )),
    }
}

/// `range(start, end[, step])` — an inclusive arithmetic sequence of Integers.
/// `step` defaults to `1` and may not be `0`. The element count is capped at
/// [`RANGE_MAX_ELEMENTS`] to bound memory.
fn scalar_range(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    if args.len() != 2 && args.len() != 3 {
        return Err(fn_err(
            "range",
            format!("expected 2 or 3 arguments, got {}", args.len()),
            span,
        ));
    }
    let start = int_arg("range", &args[0], "start", span)?;
    let end = int_arg("range", &args[1], "end", span)?;
    let step = match args.get(2) {
        None => 1,
        Some(v) => int_arg("range", v, "step", span)?,
    };
    if step == 0 {
        return Err(fn_err("range", "step must not be zero", span));
    }
    let count = range_len(start, end, step);
    if count > RANGE_MAX_ELEMENTS {
        return Err(fn_err(
            "range",
            format!("range of {count} elements exceeds the {RANGE_MAX_ELEMENTS} limit"),
            span,
        ));
    }
    let mut out = Vec::with_capacity(count as usize);
    let mut current = start;
    for _ in 0..count {
        out.push(Value::Integer(current));
        current = current.saturating_add(step);
    }
    Ok(Value::List(out))
}

/// Number of elements `range(start, end, step)` produces (inclusive of `end`
/// when it lands on a step boundary), or `0` when the step points away from
/// `end`.
fn range_len(start: i64, end: i64, step: i64) -> i64 {
    if (step > 0 && start > end) || (step < 0 && start < end) {
        return 0;
    }
    let span = (end - start) as i128;
    let stride = step as i128;
    (span / stride) as i64 + 1
}

/// `keys(x)` — the property names of a Node, Relationship, or Map, as a List
/// of String in sorted order (the property maps are themselves sorted).
fn scalar_keys(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("keys", &args, 1, span)?;
    let keys: Vec<Value> = match &args[0] {
        Value::Node(nv) => nv.properties.keys().cloned().map(Value::String).collect(),
        Value::Relationship(rv) => rv.properties.keys().cloned().map(Value::String).collect(),
        Value::Map(m) => m.keys().cloned().map(Value::String).collect(),
        other => {
            return Err(fn_err(
                "keys",
                format!(
                    "argument must be a Node, Relationship, or Map, got {}",
                    other.type_name()
                ),
                span,
            ))
        }
    };
    Ok(Value::List(keys))
}

/// `labels(node)` — the node's labels as a List of String.
fn scalar_labels(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("labels", &args, 1, span)?;
    match &args[0] {
        Value::Node(nv) => Ok(Value::List(
            nv.labels.iter().cloned().map(Value::String).collect(),
        )),
        other => Err(fn_err(
            "labels",
            format!("argument must be a Node, got {}", other.type_name()),
            span,
        )),
    }
}

/// `type(rel)` — the relationship's type as a String.
fn scalar_type(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("type", &args, 1, span)?;
    match &args[0] {
        Value::Relationship(rv) => Ok(Value::String(rv.kind.clone())),
        other => Err(fn_err(
            "type",
            format!("argument must be a Relationship, got {}", other.type_name()),
            span,
        )),
    }
}

/// `id(x)` — the internal storage id of a Node or Relationship, as an Integer.
fn scalar_id(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    expect_arity("id", &args, 1, span)?;
    match &args[0] {
        Value::Node(nv) => Ok(Value::Integer(nv.id as i64)),
        Value::Relationship(rv) => Ok(Value::Integer(rv.id as i64)),
        other => Err(fn_err(
            "id",
            format!(
                "argument must be a Node or Relationship, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// `properties(x)` — the property Map of a Node or Relationship (a Map argument
/// is returned unchanged).
fn scalar_properties(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    match take_one("properties", args, span)? {
        Value::Node(nv) => Ok(Value::Map(nv.properties.clone())),
        Value::Relationship(rv) => Ok(Value::Map(rv.properties.clone())),
        Value::Map(m) => Ok(Value::Map(m)),
        other => Err(fn_err(
            "properties",
            format!(
                "argument must be a Node, Relationship, or Map, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Coerce a [`Value`] to a borrowed `&str` for a named function argument, or a
/// recoverable error naming the offending position.
fn string_arg<'a>(func: &str, value: &'a Value, which: &str, span: Span) -> ExecResultT<&'a str> {
    match value {
        Value::String(s) => Ok(s.as_str()),
        other => Err(fn_err(
            func,
            format!(
                "{which} argument must be a String, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Coerce a [`Value`] to an `i64` for a named function argument, or a
/// recoverable error naming the offending position.
fn int_arg(func: &str, value: &Value, which: &str, span: Span) -> ExecResultT<i64> {
    match value {
        Value::Integer(i) => Ok(*i),
        other => Err(fn_err(
            func,
            format!(
                "{which} argument must be an Integer, got {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Render a Float the way Cypher's `toString` does: an integral value keeps a
/// trailing `.0` (`1.0`, not `1`); non-finite values use Rust's `inf`/`NaN`.
fn format_float(f: f64) -> String {
    if f.is_finite() && f == f.trunc() {
        format!("{f:.1}")
    } else {
        format!("{f}")
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

fn dedup_values(values: &mut Vec<Value>) {
    let mut seen: Vec<Value> = Vec::with_capacity(values.len());
    let mut out = Vec::with_capacity(values.len());
    for v in values.drain(..) {
        if !seen.iter().any(|s| s == &v) {
            seen.push(v.clone());
            out.push(v);
        }
    }
    *values = out;
}

/// Coerce a folded aggregation group's values to `f64`, rejecting any
/// non-numeric element with a recoverable [`ExecError::TypeMismatch`] (the
/// same discipline `avg` / `sum` apply).
fn numeric_fold_values(values: &[Value], span: Span) -> ExecResultT<Vec<f64>> {
    values
        .iter()
        .map(|v| {
            v.as_number().ok_or_else(|| ExecError::TypeMismatch {
                expected: "Integer or Float".into(),
                got: v.type_name().into(),
                span,
            })
        })
        .collect()
}

/// The index into an ascending-sorted, non-empty group for a discrete
/// percentile at `fraction ∈ [0, 1]`. Mirrors Neo4j's `percentileDisc`: at an
/// exact rank boundary the lower value wins (subtract one unless the index is
/// already zero), and `fraction == 1.0` selects the last element.
fn percentile_disc_index(fraction: f64, count: usize) -> usize {
    debug_assert!(count > 0);
    if fraction >= 1.0 {
        return count - 1;
    }
    let float_idx = fraction * count as f64;
    let mut idx = float_idx as usize;
    // On an exact integer boundary (and not the first slot) the lower value
    // is the percentile, so step back one.
    if float_idx == idx as f64 && idx != 0 {
        idx -= 1;
    }
    idx.min(count - 1)
}

/// The continuous percentile at `fraction ∈ [0, 1]` over an ascending-sorted,
/// non-empty slice, linearly interpolating between the two nearest ranks.
/// Mirrors Neo4j's `percentileCont`.
fn percentile_cont(sorted: &[f64], fraction: f64) -> f64 {
    let count = sorted.len();
    debug_assert!(count > 0);
    if fraction >= 1.0 {
        return sorted[count - 1];
    }
    let float_idx = fraction * (count - 1) as f64;
    let lo = float_idx.floor() as usize;
    let lo_val = sorted[lo];
    let hi = lo + 1;
    if hi < count {
        let frac = float_idx - lo as f64;
        lo_val * (1.0 - frac) + sorted[hi] * frac
    } else {
        lo_val
    }
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
        RegexMatch => regex_match(lhs, rhs, span),
    }
}

/// Evaluate the Cypher `=~` regex-match operator.
///
/// Semantics mirror Neo4j: `NULL =~ x` and `x =~ NULL` yield `NULL`
/// (three-valued logic); both operands must otherwise be strings; the
/// right-hand side is compiled as a regular expression and the **entire**
/// left-hand string must match (Java `Matcher::matches` semantics). An
/// invalid pattern or a pathological match raises [`ExecError::InvalidRegex`].
fn regex_match(lhs: Value, rhs: Value, span: Span) -> ExecResultT<Value> {
    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::String(text), Value::String(pattern)) = (&lhs, &rhs) else {
        return Err(ExecError::TypeMismatch {
            expected: "String =~ String".into(),
            got: format!("{} =~ {}", lhs.type_name(), rhs.type_name()),
            span,
        });
    };
    match crate::cypher::regex::full_match(pattern, text) {
        Ok(matched) => Ok(Value::Bool(matched)),
        Err(e) => Err(ExecError::InvalidRegex {
            message: e.to_string(),
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
    fn keyword_rel_type_round_trips_through_storage() {
        // `CONTAINS` is a reserved Cypher keyword (string predicate). Used
        // as a relationship type it must be stored verbatim, not lowercased
        // (regression: consume_name used to .to_lowercase() keyword names).
        // The read side uses a type-less `-[r]->` pattern so no consume_name
        // lowercasing can mask the bug; type(r) returns the stored kind.
        let db = drevo();
        run("CREATE (a:N)-[:CONTAINS]->(b:N)", &db);
        let res = run("MATCH (a)-[r]->(b) RETURN type(r) AS t", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("CONTAINS".into()));
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

    // ---- anonymous head / intermediate nodes in MATCH (task 00143) --------

    #[test]
    fn match_anonymous_labeled_head_single_hop() {
        // `MATCH (:Person)-->(b)` — the head binds no variable but is still
        // a real node that must be threaded into the relationship segment.
        let db = drevo();
        run(
            "CREATE (:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
            &db,
        );
        let res = run("MATCH (:Person)-[:KNOWS]->(b) RETURN b.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Bob".into()));
    }

    #[test]
    fn match_bare_anonymous_head_single_hop() {
        // `MATCH ()-->(b)` — a totally bare anonymous head (no label) is the
        // most permissive form; every relationship's target should surface.
        let db = drevo();
        run(
            "CREATE (:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
            &db,
        );
        let res = run("MATCH ()-[:KNOWS]->(b) RETURN b.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Bob".into()));
    }

    #[test]
    fn match_anonymous_intermediate_in_unnamed_multi_hop() {
        // `MATCH (a)-->()-->(c)` — the middle node is anonymous and the path
        // is unnamed (no accumulator), so the endpoint must be threaded
        // directly hop-to-hop.
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})-[:KNOWS]->(c:Person {name: 'Carol'})",
            &db,
        );
        let res = run(
            "MATCH (a:Person {name: 'Alice'})-[:KNOWS]->()-[:KNOWS]->(c) RETURN c.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Carol".into()));
    }

    #[test]
    fn match_anonymous_head_with_anonymous_intermediate() {
        // Both head and intermediate anonymous: `MATCH (:Person)-->()-->(c)`.
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})-[:KNOWS]->(c:Person {name: 'Carol'})",
            &db,
        );
        let res = run(
            "MATCH (:Person)-[:KNOWS]->()-[:KNOWS]->(c) RETURN c.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Carol".into()));
    }

    #[test]
    fn match_anonymous_head_varlen() {
        // Anonymous head feeding a variable-length segment.
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})-[:KNOWS]->(c:Person {name: 'Carol'})",
            &db,
        );
        let res = run(
            "MATCH (:Person {name: 'Alice'})-[:KNOWS*1..2]->(reached) RETURN reached.name AS name ORDER BY name",
            &db,
        );
        let names: Vec<Value> = res.rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(
            names,
            vec![Value::String("Bob".into()), Value::String("Carol".into())]
        );
    }

    #[test]
    fn match_anonymous_head_no_match_yields_empty_not_error() {
        // No relationship of the requested type — empty result, never the
        // old spurious `InvalidCreate`.
        let db = drevo();
        run("CREATE (:Person {name: 'Alice'})", &db);
        let res = run("MATCH (:Person)-[:KNOWS]->(b) RETURN b.name AS name", &db);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn match_named_path_through_anonymous_head_still_binds_path() {
        // The named-path accumulator (00141) keeps working with the new
        // threading: a named path over an anonymous head binds the full
        // alternating node/relationship sequence.
        let db = drevo();
        run(
            "CREATE (:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
            &db,
        );
        let res = run(
            "MATCH p = (:Person)-[:KNOWS]->(b) RETURN length(p) AS hops",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Integer(1));
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

    // ---- WHERE ------------------------------------------------------------

    #[test]
    fn where_filters_by_simple_equality() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        run("CREATE (:Person {name: 'B', age: 25})", &db);
        let res = run(
            "MATCH (n:Person) WHERE n.name = 'A' RETURN n.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn where_filters_by_numeric_comparison() {
        let db = drevo();
        for (name, age) in [("A", 17), ("B", 18), ("C", 25), ("D", 40)] {
            run(
                &format!("CREATE (:Person {{name: '{}', age: {}}})", name, age),
                &db,
            );
        }
        let res = run(
            "MATCH (n:Person) WHERE n.age >= 18 RETURN n.name AS name ORDER BY n.name",
            &db,
        );
        let names: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            names,
            vec!["B".to_string(), "C".to_string(), "D".to_string()]
        );
    }

    #[test]
    fn where_combines_predicates_with_and_or_not() {
        let db = drevo();
        for (n, age, team) in [
            ("A", 25, "red"),
            ("B", 35, "red"),
            ("C", 30, "blue"),
            ("D", 45, "blue"),
        ] {
            run(
                &format!(
                    "CREATE (:Person {{name: '{}', age: {}, team: '{}'}})",
                    n, age, team
                ),
                &db,
            );
        }
        let res = run(
            "MATCH (n:Person) WHERE n.age > 30 AND n.team = 'blue' RETURN n.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("D".into()));

        let res = run(
            "MATCH (n:Person) WHERE n.team = 'red' OR n.age >= 45 RETURN n.name AS name ORDER BY n.name",
            &db,
        );
        let names: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            names,
            vec!["A".to_string(), "B".to_string(), "D".to_string()]
        );

        let res = run(
            "MATCH (n:Person) WHERE NOT n.team = 'red' RETURN n.name AS name ORDER BY n.name",
            &db,
        );
        let names: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, vec!["C".to_string(), "D".to_string()]);
    }

    #[test]
    fn where_in_list_membership() {
        let db = drevo();
        for n in ["A", "B", "C", "D"] {
            run(&format!("CREATE (:Person {{name: '{}'}})", n), &db);
        }
        let res = run(
            "MATCH (n:Person) WHERE n.name IN ['A', 'C', 'Z'] RETURN n.name AS name ORDER BY n.name",
            &db,
        );
        let names: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, vec!["A".to_string(), "C".to_string()]);
    }

    #[test]
    fn where_is_null_and_is_not_null() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', email: 'a@b.com'})", &db);
        run("CREATE (:Person {name: 'B'})", &db);
        let res = run(
            "MATCH (n:Person) WHERE n.email IS NULL RETURN n.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("B".into()));

        let res = run(
            "MATCH (n:Person) WHERE n.email IS NOT NULL RETURN n.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn where_string_predicates_starts_with_ends_with_contains() {
        let db = drevo();
        for s in ["alpha", "beta", "gamma", "alphabet"] {
            run(&format!("CREATE (:Word {{title: '{}'}})", s), &db);
        }
        let res = run(
            "MATCH (n:Word) WHERE n.title STARTS WITH 'alpha' RETURN n.title AS title ORDER BY n.title",
            &db,
        );
        let titles: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(titles, vec!["alpha".to_string(), "alphabet".to_string()]);

        let res = run(
            "MATCH (n:Word) WHERE n.title ENDS WITH 'a' RETURN n.title AS title ORDER BY n.title",
            &db,
        );
        let titles: Vec<String> = res
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            titles,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );

        let res = run(
            "MATCH (n:Word) WHERE n.title CONTAINS 'amma' RETURN n.title AS title",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("gamma".into()));
    }

    #[test]
    fn where_null_predicate_drops_row() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // Comparing against a missing property yields NULL; WHERE keeps
        // only TRUE rows, so this should return zero results.
        let res = run(
            "MATCH (n:Person) WHERE n.missing = 'x' RETURN n.name AS name",
            &db,
        );
        assert!(res.rows.is_empty());
    }

    #[test]
    fn where_on_relationship_pattern() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'A'})-[:KNOWS {since: 2020}]->(b:Person {name: 'B'})",
            &db,
        );
        run(
            "CREATE (a:Person {name: 'C'})-[:KNOWS {since: 2024}]->(b:Person {name: 'D'})",
            &db,
        );
        let res = run(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) WHERE r.since >= 2024 RETURN a.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("C".into()));
    }

    #[test]
    fn where_with_parameter() {
        let db = drevo();
        run("CREATE (:Person {name: 'Alice'})", &db);
        run("CREATE (:Person {name: 'Bob'})", &db);
        let mut params = HashMap::new();
        params.insert("who".to_string(), Value::String("Alice".into()));
        let q = parse("MATCH (n:Person) WHERE n.name = $who RETURN n.name AS name").unwrap();
        let res = execute(&q, &db, params).unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("Alice".into()));
    }

    #[test]
    fn where_arithmetic_in_predicate() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        run("CREATE (:Person {name: 'B', age: 17})", &db);
        let res = run(
            "MATCH (n:Person) WHERE n.age + 1 > 18 RETURN n.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn where_with_unbound_variable_errors() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let e = err("MATCH (n:Person) WHERE unknown_var.x > 1 RETURN n", &db);
        assert!(
            matches!(e, ExecError::UnboundVariable { .. }),
            "got {:?}",
            e
        );
    }

    // ---- Aggregations (00066) --------------------------------------------

    #[test]
    fn count_star_counts_all_rows() {
        let db = drevo();
        for n in ["A", "B", "C"] {
            run(&format!("CREATE (:Person {{name: '{}'}})", n), &db);
        }
        let res = run("MATCH (n:Person) RETURN count(*) AS total", &db);
        assert_eq!(res.columns, vec!["total".to_string()]);
        assert_eq!(res.rows, vec![vec![Value::Integer(3)]]);
    }

    #[test]
    fn count_star_on_empty_match_returns_zero_row() {
        let db = drevo();
        let res = run("MATCH (n:Person) RETURN count(*) AS total", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn count_expression_skips_nulls() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', score: 10})", &db);
        run("CREATE (:Person {name: 'B'})", &db);
        run("CREATE (:Person {name: 'C', score: 20})", &db);
        let res = run("MATCH (n:Person) RETURN count(n.score) AS c", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn count_distinct_dedupes_argument_values() {
        let db = drevo();
        for v in [10, 10, 20, 30, 30] {
            run(&format!("CREATE (:Person {{score: {}}})", v), &db);
        }
        let res = run("MATCH (n:Person) RETURN count(DISTINCT n.score) AS c", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(3)]]);
    }

    #[test]
    fn sum_integer_returns_integer() {
        let db = drevo();
        for v in [1, 2, 3, 4] {
            run(&format!("CREATE (:Person {{score: {}}})", v), &db);
        }
        let res = run("MATCH (n:Person) RETURN sum(n.score) AS s", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(10)]]);
    }

    #[test]
    fn sum_with_float_promotes_to_float() {
        let db = drevo();
        run("CREATE (:Person {score: 1.5})", &db);
        run("CREATE (:Person {score: 2})", &db);
        let res = run("MATCH (n:Person) RETURN sum(n.score) AS s", &db);
        assert_eq!(res.rows, vec![vec![Value::Float(3.5)]]);
    }

    #[test]
    fn sum_skips_nulls() {
        let db = drevo();
        run("CREATE (:Person {score: 5})", &db);
        run("CREATE (:Person {})", &db);
        run("CREATE (:Person {score: 7})", &db);
        let res = run("MATCH (n:Person) RETURN sum(n.score) AS s", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(12)]]);
    }

    #[test]
    fn sum_on_empty_returns_zero() {
        let db = drevo();
        let res = run("MATCH (n:Person) RETURN sum(n.score) AS s", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(0)]]);
    }

    #[test]
    fn avg_returns_float_mean_of_non_nulls() {
        let db = drevo();
        for v in [2, 4, 6] {
            run(&format!("CREATE (:Person {{score: {}}})", v), &db);
        }
        let res = run("MATCH (n:Person) RETURN avg(n.score) AS a", &db);
        assert_eq!(res.rows, vec![vec![Value::Float(4.0)]]);
    }

    #[test]
    fn avg_on_empty_returns_null() {
        let db = drevo();
        let res = run("MATCH (n:Person) RETURN avg(n.score) AS a", &db);
        assert_eq!(res.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn min_and_max_skip_nulls() {
        let db = drevo();
        run("CREATE (:Person {score: 10})", &db);
        run("CREATE (:Person {})", &db);
        run("CREATE (:Person {score: 3})", &db);
        run("CREATE (:Person {score: 27})", &db);
        let res = run(
            "MATCH (n:Person) RETURN min(n.score) AS lo, max(n.score) AS hi",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::Integer(3), Value::Integer(27)]]);
    }

    #[test]
    fn min_max_on_empty_returns_null() {
        let db = drevo();
        let res = run(
            "MATCH (n:Person) RETURN min(n.score) AS lo, max(n.score) AS hi",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::Null, Value::Null]]);
    }

    #[test]
    fn collect_returns_list_of_non_null_values_preserving_input_order() {
        let db = drevo();
        for name in ["A", "B", "C"] {
            run(&format!("CREATE (:Person {{name: '{}'}})", name), &db);
        }
        let res = run(
            "MATCH (n:Person) RETURN collect(n.name) AS names ORDER BY names",
            &db,
        );
        let row = &res.rows[0];
        let list = match &row[0] {
            Value::List(items) => items.clone(),
            other => panic!("expected list, got {:?}", other),
        };
        let mut names: Vec<String> = list
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => panic!("expected string, got {:?}", other),
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn collect_distinct_dedupes() {
        let db = drevo();
        for v in ["red", "red", "blue", "blue", "red"] {
            run(&format!("CREATE (:Person {{team: '{}'}})", v), &db);
        }
        let res = run(
            "MATCH (n:Person) RETURN collect(DISTINCT n.team) AS teams",
            &db,
        );
        let list = match &res.rows[0][0] {
            Value::List(items) => items.clone(),
            other => panic!("expected list, got {:?}", other),
        };
        let mut teams: Vec<String> = list
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                other => panic!("expected string, got {:?}", other),
            })
            .collect();
        teams.sort();
        assert_eq!(teams, vec!["blue".to_string(), "red".to_string()]);
    }

    #[test]
    fn group_by_implicit_via_non_agg_projection() {
        let db = drevo();
        for (team, score) in [
            ("red", 10),
            ("red", 20),
            ("blue", 5),
            ("blue", 15),
            ("blue", 100),
        ] {
            run(
                &format!("CREATE (:Person {{team: '{}', score: {}}})", team, score),
                &db,
            );
        }
        let res = run(
            "MATCH (n:Person) RETURN n.team AS team, sum(n.score) AS total ORDER BY team",
            &db,
        );
        assert_eq!(res.columns, vec!["team".to_string(), "total".to_string()]);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("blue".into()), Value::Integer(120)],
                vec![Value::String("red".into()), Value::Integer(30)],
            ]
        );
    }

    #[test]
    fn group_by_with_count_per_group() {
        let db = drevo();
        for kind in ["a", "a", "a", "b", "b", "c"] {
            run(&format!("CREATE (:Item {{kind: '{}'}})", kind), &db);
        }
        let res = run(
            "MATCH (n:Item) RETURN n.kind AS k, count(*) AS c ORDER BY k",
            &db,
        );
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("a".into()), Value::Integer(3)],
                vec![Value::String("b".into()), Value::Integer(2)],
                vec![Value::String("c".into()), Value::Integer(1)],
            ]
        );
    }

    #[test]
    fn aggregation_combined_with_arithmetic() {
        let db = drevo();
        for v in [1, 2, 3, 4] {
            run(&format!("CREATE (:Person {{score: {}}})", v), &db);
        }
        let res = run("MATCH (n:Person) RETURN sum(n.score) * 2 AS doubled", &db);
        assert_eq!(res.rows, vec![vec![Value::Integer(20)]]);
    }

    #[test]
    fn aggregation_order_by_alias_then_limit() {
        let db = drevo();
        for kind in ["a", "a", "b", "b", "b", "c"] {
            run(&format!("CREATE (:Item {{kind: '{}'}})", kind), &db);
        }
        let res = run(
            "MATCH (n:Item) RETURN n.kind AS k, count(*) AS c ORDER BY c DESC LIMIT 2",
            &db,
        );
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("b".into()), Value::Integer(3)],
                vec![Value::String("a".into()), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn nested_aggregations_are_rejected() {
        let db = drevo();
        run("CREATE (:Person {score: 10})", &db);
        let e = err("MATCH (n:Person) RETURN count(sum(n.score)) AS c", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(ref s) if s.contains("nested aggregations")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn stdevp_and_stdev_fold_a_group() {
        let db = drevo();
        for v in [2, 4, 4, 4, 5, 5, 7, 9] {
            run(&format!("CREATE (:M {{v: {}}})", v), &db);
        }
        // Population stdev of the textbook sample is exactly 2.0.
        let res = run("MATCH (n:M) RETURN stDevP(n.v) AS sd", &db);
        match res.rows[0][0] {
            Value::Float(f) => assert!((f - 2.0).abs() < 1e-9, "got {f}"),
            ref other => panic!("expected Float, got {other:?}"),
        }
        // Sample stdev uses the n-1 divisor: sqrt(32/7).
        let res = run("MATCH (n:M) RETURN stDev(n.v) AS sd", &db);
        match res.rows[0][0] {
            Value::Float(f) => assert!((f - (32.0f64 / 7.0).sqrt()).abs() < 1e-9, "got {f}"),
            ref other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn percentile_disc_preserves_integer_and_cont_interpolates() {
        let db = drevo();
        for v in [1, 2, 3, 4] {
            run(&format!("CREATE (:M {{v: {}}})", v), &db);
        }
        // Discrete median keeps the Integer type.
        let res = run("MATCH (n:M) RETURN percentileDisc(n.v, 0.5) AS p", &db);
        assert_eq!(res.rows[0][0], Value::Integer(2));
        // Continuous median interpolates to a Float halfway between 2 and 3.
        let res = run("MATCH (n:M) RETURN percentileCont(n.v, 0.5) AS p", &db);
        assert_eq!(res.rows[0][0], Value::Float(2.5));
    }

    #[test]
    fn percentile_fraction_out_of_range_is_invalid() {
        let db = drevo();
        run("CREATE (:M {v: 1})", &db);
        let e = err("MATCH (n:M) RETURN percentileCont(n.v, 2.0) AS p", &db);
        assert!(
            matches!(e, ExecError::InvalidFunctionCall { ref message, .. } if message.contains("between 0.0 and 1.0")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn percentile_wrong_arity_is_rejected() {
        let db = drevo();
        run("CREATE (:M {v: 1})", &db);
        // The one-arg form is missing the mandatory fraction.
        let e = err("MATCH (n:M) RETURN percentileDisc(n.v) AS p", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(ref s) if s.contains("takes exactly 2 arguments")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn percentile_disc_index_matches_neo4j_boundaries() {
        // Over four values: exact rank boundaries step back to the lower value.
        assert_eq!(percentile_disc_index(0.0, 4), 0);
        assert_eq!(percentile_disc_index(0.5, 4), 1);
        assert_eq!(percentile_disc_index(0.75, 4), 2);
        assert_eq!(percentile_disc_index(1.0, 4), 3);
        // Single-element group always resolves to index 0.
        assert_eq!(percentile_disc_index(0.5, 1), 0);
    }

    #[test]
    fn percentile_cont_interpolation_is_linear() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile_cont(&xs, 0.0), 1.0);
        assert_eq!(percentile_cont(&xs, 0.5), 2.5);
        assert_eq!(percentile_cont(&xs, 0.75), 3.25);
        assert_eq!(percentile_cont(&xs, 1.0), 4.0);
        // A single element is its own percentile at any fraction.
        assert_eq!(percentile_cont(&[7.0], 0.3), 7.0);
    }

    #[test]
    fn unknown_function_still_unsupported() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // `nosuchfn` is not a built-in scalar, aggregation, or drevo
        // extension, so it must stay a deterministic `Unsupported`.
        let e = err("MATCH (n:Person) RETURN nosuchfn(n.name) AS s", &db);
        assert!(
            matches!(e, ExecError::Unsupported { ref feature, .. } if feature.contains("function call")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn distinct_on_count_star_is_rejected() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let e = err("MATCH (n:Person) RETURN count(DISTINCT *) AS c", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(ref s) if s.contains("DISTINCT")),
            "got {:?}",
            e
        );
    }

    // ---- OPTIONAL MATCH (00067) -----------------------------------------

    #[test]
    fn optional_match_on_empty_db_yields_single_null_row() {
        let db = drevo();
        let res = run("OPTIONAL MATCH (n:Person) RETURN n", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Null);
    }

    #[test]
    fn optional_match_with_results_returns_those_rows() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("CREATE (:Person {name: 'B'})", &db);
        let res = run(
            "OPTIONAL MATCH (n:Person) RETURN n.name AS name ORDER BY name",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[1][0], Value::String("B".into()));
    }

    #[test]
    fn match_then_optional_match_preserves_left_rows_with_null() {
        let db = drevo();
        // Two people; only one has a KNOWS edge.
        run(
            "CREATE (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'X'})",
            &db,
        );
        run("CREATE (:Person {name: 'B'})", &db);
        // B has no outgoing KNOWS — its friend column should be NULL.
        let res = run(
            "MATCH (n:Person) WHERE n.name IN ['A', 'B'] OPTIONAL MATCH (n)-[:KNOWS]->(f:Person) RETURN n.name AS who, f.name AS friend ORDER BY who",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::String("X".into()));
        assert_eq!(res.rows[1][0], Value::String("B".into()));
        assert_eq!(res.rows[1][1], Value::Null);
    }

    #[test]
    fn optional_match_relationship_introduces_null_rel_variable() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run(
            "MATCH (n:Person) OPTIONAL MATCH (n)-[r:KNOWS]->(f) RETURN n.name AS who, r, f",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::Null);
        assert_eq!(res.rows[0][2], Value::Null);
    }

    #[test]
    fn optional_match_with_where_falling_to_no_rows_yields_null_row() {
        let db = drevo();
        run(
            "CREATE (:Person {name: 'A'})-[:KNOWS {since: 2020}]->(:Person {name: 'X'})",
            &db,
        );
        let res = run(
            "MATCH (n:Person {name: 'A'}) OPTIONAL MATCH (n)-[r:KNOWS]->(f) WHERE r.since > 2024 RETURN n.name AS who, f.name AS friend",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::Null);
    }

    #[test]
    fn optional_match_does_not_drop_input_row_when_pattern_matches_in_another() {
        let db = drevo();
        run(
            "CREATE (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'X'})",
            &db,
        );
        run("CREATE (:Person {name: 'B'})", &db);
        run(
            "CREATE (:Person {name: 'C'})-[:KNOWS]->(:Person {name: 'Y'})",
            &db,
        );
        // A and C have friends; B does not — but it must still appear.
        let res = run(
            "MATCH (n:Person) WHERE n.name IN ['A', 'B', 'C'] OPTIONAL MATCH (n)-[:KNOWS]->(f) RETURN n.name AS who, f.name AS friend ORDER BY who",
            &db,
        );
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::String("X".into()));
        assert_eq!(res.rows[1][0], Value::String("B".into()));
        assert_eq!(res.rows[1][1], Value::Null);
        assert_eq!(res.rows[2][0], Value::String("C".into()));
        assert_eq!(res.rows[2][1], Value::String("Y".into()));
    }

    #[test]
    fn optional_match_chained_independently() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // Two independent OPTIONAL MATCHes that both miss → single all-null row.
        let res = run(
            "MATCH (n:Person {name: 'A'}) OPTIONAL MATCH (n)-[:KNOWS]->(f) OPTIONAL MATCH (n)-[:LIKES]->(t) RETURN n.name AS who, f, t",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::Null);
        assert_eq!(res.rows[0][2], Value::Null);
    }

    #[test]
    fn optional_match_with_aggregation_counts_zero_for_unmatched() {
        let db = drevo();
        run(
            "CREATE (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'X'})",
            &db,
        );
        run(
            "CREATE (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'Y'})",
            &db,
        );
        run("CREATE (:Person {name: 'B'})", &db);
        let res = run(
            "MATCH (n:Person) WHERE n.name IN ['A', 'B'] OPTIONAL MATCH (n)-[:KNOWS]->(f) RETURN n.name AS who, count(f) AS friends ORDER BY who",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        // A has 2 friends; the (n=A, f=X) and (n=A, f=Y) rows both
        // contribute to the same group key 'A'.
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::Integer(2));
        // B has none; the synthesized (n=B, f=Null) row contributes 0
        // because count(f) skips nulls.
        assert_eq!(res.rows[1][0], Value::String("B".into()));
        assert_eq!(res.rows[1][1], Value::Integer(0));
    }

    // ---- WITH (00068) ---------------------------------------------------

    #[test]
    fn with_renames_variable_for_downstream_use() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        let res = run(
            "MATCH (n:Person) WITH n AS person RETURN person.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn with_distinct_dedupes_intermediate_rows() {
        let db = drevo();
        for team in ["red", "red", "blue", "red", "blue"] {
            run(&format!("CREATE (:Person {{team: '{}'}})", team), &db);
        }
        let res = run(
            "MATCH (n:Person) WITH DISTINCT n.team AS team RETURN team ORDER BY team",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("blue".into()));
        assert_eq!(res.rows[1][0], Value::String("red".into()));
    }

    #[test]
    fn with_where_filters_after_projection() {
        let db = drevo();
        for (n, age) in [("A", 10), ("B", 20), ("C", 30), ("D", 40)] {
            run(
                &format!("CREATE (:Person {{name: '{}', age: {}}})", n, age),
                &db,
            );
        }
        let res = run(
            "MATCH (p:Person) WITH p.name AS name, p.age AS age WHERE age > 20 RETURN name ORDER BY name",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("C".into()));
        assert_eq!(res.rows[1][0], Value::String("D".into()));
    }

    #[test]
    fn with_aggregation_then_where_filter_on_aggregate() {
        let db = drevo();
        // Pre-aggregate count(*) per team, then keep only teams with >= 2.
        for team in ["a", "a", "b", "b", "b", "c"] {
            run(&format!("CREATE (:Item {{team: '{}'}})", team), &db);
        }
        let res = run(
            "MATCH (n:Item) WITH n.team AS team, count(*) AS c WHERE c >= 2 RETURN team, c ORDER BY team",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("a".into()));
        assert_eq!(res.rows[0][1], Value::Integer(2));
        assert_eq!(res.rows[1][0], Value::String("b".into()));
        assert_eq!(res.rows[1][1], Value::Integer(3));
    }

    #[test]
    fn with_order_by_skip_limit_pipeline_to_return() {
        let db = drevo();
        for (n, age) in [("A", 10), ("B", 20), ("C", 30), ("D", 40), ("E", 50)] {
            run(
                &format!("CREATE (:Person {{name: '{}', age: {}}})", n, age),
                &db,
            );
        }
        let res = run(
            "MATCH (p:Person) WITH p.name AS name, p.age AS age ORDER BY age DESC SKIP 1 LIMIT 2 RETURN name ORDER BY name",
            &db,
        );
        // Sort by age DESC: E(50), D(40), C(30), B(20), A(10).
        // SKIP 1 LIMIT 2 → D, C. Then RETURN sorted by name → C, D.
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::String("C".into()));
        assert_eq!(res.rows[1][0], Value::String("D".into()));
    }

    #[test]
    fn with_chained_pipelines_through_multiple_stages() {
        let db = drevo();
        for v in 1..=10 {
            run(&format!("CREATE (:N {{v: {}}})", v), &db);
        }
        // Stage 1: keep v >= 4. Stage 2: keep v <= 8. Stage 3: sum v.
        let res = run(
            "MATCH (n:N) WITH n.v AS v WHERE v >= 4 WITH v WHERE v <= 8 RETURN sum(v) AS total",
            &db,
        );
        // 4+5+6+7+8 = 30
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Integer(30));
    }

    #[test]
    fn with_projection_without_alias_for_complex_expression_is_rejected() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // `WITH n.name` without AS is not legal in Cypher — must be either
        // a bare variable or an aliased expression.
        let e = err("MATCH (n:Person) WITH n.name RETURN n.name", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(ref s) if s.contains("alias")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn with_bare_variable_is_allowed_without_alias() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        // `WITH n` without alias is fine — `n` becomes the column name.
        let res = run("MATCH (n:Person) WITH n RETURN n.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn with_star_passes_all_bound_variables() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        let res = run("MATCH (n:Person) WITH * RETURN n.name AS name", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
    }

    #[test]
    fn with_drops_unprojected_variables() {
        let db = drevo();
        run("CREATE (:Person {name: 'A', age: 30})", &db);
        // After WITH p.name AS name, the variable `p` is no longer bound.
        let e = err("MATCH (p:Person) WITH p.name AS name RETURN p.age", &db);
        assert!(
            matches!(e, ExecError::UnboundVariable { ref name, .. } if name == "p"),
            "got {:?}",
            e
        );
    }

    // ---- UNWIND (00135) -------------------------------------------------

    #[test]
    fn unwind_list_literal_expands_into_one_row_per_element() {
        let db = drevo();
        let res = run("UNWIND [1, 2, 3] AS x RETURN x", &db);
        assert_eq!(res.columns, vec!["x"]);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ]
        );
    }

    #[test]
    fn unwind_preserves_element_order() {
        let db = drevo();
        let res = run("UNWIND [3, 1, 2] AS x RETURN x", &db);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::Integer(3)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn unwind_empty_list_yields_no_rows() {
        let db = drevo();
        let res = run("UNWIND [] AS x RETURN x", &db);
        assert_eq!(res.columns, vec!["x"]);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn unwind_null_yields_no_rows() {
        let db = drevo();
        let res = run("UNWIND null AS x RETURN x", &db);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn unwind_from_parameter_list() {
        let db = drevo();
        let mut params = HashMap::new();
        params.insert(
            "xs".into(),
            Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
        );
        let res = run_with_params("UNWIND $xs AS x RETURN x", &db, params);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("a".into())],
                vec![Value::String("b".into())],
            ]
        );
    }

    #[test]
    fn unwind_multiplies_each_prior_binding_row() {
        let db = drevo();
        run("CREATE (:Person {name: 'A'})", &db);
        run("CREATE (:Person {name: 'B'})", &db);
        // Two matched people × two list elements = four rows.
        let res = run(
            "MATCH (p:Person) UNWIND [1, 2] AS n RETURN p.name AS name, n ORDER BY name, n",
            &db,
        );
        assert_eq!(res.columns, vec!["name", "n"]);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("A".into()), Value::Integer(1)],
                vec![Value::String("A".into()), Value::Integer(2)],
                vec![Value::String("B".into()), Value::Integer(1)],
                vec![Value::String("B".into()), Value::Integer(2)],
            ]
        );
    }

    #[test]
    fn unwind_after_empty_match_yields_no_rows() {
        let db = drevo();
        // No Person nodes exist → MATCH yields zero rows → UNWIND has
        // nothing to expand, so the leading-row seed must NOT resurrect.
        let res = run("MATCH (p:Person) UNWIND [1, 2, 3] AS n RETURN n", &db);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn unwind_alias_is_visible_to_downstream_with_and_aggregation() {
        let db = drevo();
        let res = run(
            "UNWIND [1, 2, 2, 3, 3, 3] AS x WITH x, count(*) AS c RETURN x, c ORDER BY x",
            &db,
        );
        assert_eq!(res.columns, vec!["x", "c"]);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(2)],
                vec![Value::Integer(3), Value::Integer(3)],
            ]
        );
    }

    #[test]
    fn unwind_feeds_create_one_node_per_element() {
        let db = drevo();
        run(
            "UNWIND ['Alice', 'Bob', 'Carol'] AS nm CREATE (:Person {name: nm})",
            &db,
        );
        let people = db.list_nodes_by_kind("Person", 100, 0).unwrap();
        assert_eq!(people.len(), 3);
    }

    #[test]
    fn unwind_non_list_scalar_is_type_mismatch() {
        let db = drevo();
        let e = err("UNWIND 42 AS x RETURN x", &db);
        assert!(
            matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "List"),
            "got {:?}",
            e
        );
    }

    #[test]
    fn unwind_nested_list_elements_are_preserved() {
        let db = drevo();
        let res = run("UNWIND [[1, 2], [3]] AS pair RETURN pair", &db);
        assert_eq!(
            res.rows,
            vec![
                vec![Value::List(vec![Value::Integer(1), Value::Integer(2)])],
                vec![Value::List(vec![Value::Integer(3)])],
            ]
        );
    }

    // ---- FOREACH (00144) ------------------------------------------------

    #[test]
    fn foreach_over_literal_list_creates_one_node_per_element() {
        let db = drevo();
        let res = run(
            "FOREACH (x IN [1, 2, 3] | CREATE (:Task {title: 'task-' + toString(x)}))",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 3);
        let tasks = db.list_nodes_by_kind("Task", 100, 0).unwrap();
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn foreach_over_empty_list_is_a_noop() {
        let db = drevo();
        let res = run(
            "FOREACH (x IN [] | CREATE (:Task {title: toString(x)}))",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 0);
        assert!(db.list_nodes_by_kind("Task", 100, 0).unwrap().is_empty());
    }

    #[test]
    fn foreach_over_null_is_a_noop() {
        let db = drevo();
        // Mirrors `UNWIND null` — a null list iterates zero times rather
        // than raising a type error.
        let res = run(
            "FOREACH (x IN null | CREATE (:Task {title: toString(x)}))",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 0);
        assert!(db.list_nodes_by_kind("Task", 100, 0).unwrap().is_empty());
    }

    #[test]
    fn foreach_over_non_list_scalar_is_type_mismatch() {
        let db = drevo();
        let e = err("FOREACH (x IN 42 | CREATE (:Task {title: 'x'}))", &db);
        assert!(
            matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "List"),
            "got {:?}",
            e
        );
    }

    #[test]
    fn foreach_sets_property_on_every_matched_node() {
        let db = drevo();
        run(
            "CREATE (:Task {title: 'A'}) CREATE (:Task {title: 'B'})",
            &db,
        );
        // Collect the matched nodes into a list, then SET a property on
        // each via FOREACH.
        run(
            "MATCH (t:Task) WITH collect(t) AS ts FOREACH (n IN ts | SET n.done = true)",
            &db,
        );
        let tasks = db.list_nodes_by_kind("Task", 100, 0).unwrap();
        assert_eq!(tasks.len(), 2);
        for t in tasks {
            assert_eq!(
                t.properties.get("done").and_then(|v| v.as_bool()),
                Some(true)
            );
        }
    }

    #[test]
    fn foreach_runs_multiple_update_clauses_per_element() {
        let db = drevo();
        let res = run(
            "FOREACH (x IN [1, 2] | CREATE (n:Task {title: 'item-' + toString(x)}) SET n.flag = true)",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 2);
        assert_eq!(res.stats.properties_set, 2);
        let tasks = db.list_nodes_by_kind("Task", 100, 0).unwrap();
        assert!(tasks
            .iter()
            .all(|t| t.properties.get("flag").and_then(|v| v.as_bool()) == Some(true)));
    }

    #[test]
    fn foreach_loop_variable_is_not_visible_after_the_clause() {
        let db = drevo();
        // `x` is scoped to the FOREACH body; referencing it in a trailing
        // RETURN must fail as an unbound variable rather than leaking the
        // last element of the list.
        let e = err(
            "FOREACH (x IN [1, 2, 3] | CREATE (:Task {title: toString(x)})) RETURN x",
            &db,
        );
        assert!(
            matches!(e, ExecError::UnboundVariable { ref name, .. } if name == "x"),
            "got {:?}",
            e
        );
    }

    #[test]
    fn foreach_references_outer_bound_variable() {
        let db = drevo();
        run("CREATE (:Project {title: 'Launch'})", &db);
        // For the matched project, create a subtask per title and link it.
        run(
            "MATCH (p:Project {title: 'Launch'}) \
             FOREACH (name IN ['design', 'build', 'ship'] | \
               CREATE (p)-[:HAS_SUBTASK]->(:Task {title: name}))",
            &db,
        );
        let subtasks = db.list_nodes_by_kind("Task", 100, 0).unwrap();
        assert_eq!(subtasks.len(), 3);
        let res = run(
            "MATCH (:Project {title: 'Launch'})-[:HAS_SUBTASK]->(t:Task) RETURN count(t)",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::Integer(3)]]);
    }

    #[test]
    fn foreach_nested_iterates_the_cross_product() {
        let db = drevo();
        run(
            "FOREACH (row IN [[1, 2], [3, 4]] | \
               FOREACH (cell IN row | CREATE (:Cell {title: 'c' + toString(cell)})))",
            &db,
        );
        let cells = db.list_nodes_by_kind("Cell", 100, 0).unwrap();
        assert_eq!(cells.len(), 4);
    }

    #[test]
    fn foreach_preserves_outer_cardinality() {
        let db = drevo();
        run(
            "CREATE (:Task {title: 'A'}) CREATE (:Task {title: 'B'})",
            &db,
        );
        // Two matched rows; FOREACH creates a child each but must not
        // multiply or collapse the outer rows — RETURN still sees both.
        let res = run(
            "MATCH (t:Task) FOREACH (n IN [1] | SET t.touched = true) RETURN t.title ORDER BY t.title",
            &db,
        );
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("A".into())],
                vec![Value::String("B".into())],
            ]
        );
    }

    #[test]
    fn foreach_over_an_empty_match_runs_zero_iterations() {
        let db = drevo();
        // No Project nodes → MATCH yields zero rows → FOREACH body never
        // runs, even though its list is non-empty.
        run(
            "MATCH (p:Project) FOREACH (name IN ['a', 'b'] | CREATE (p)-[:HAS_SUBTASK]->(:Task {title: name}))",
            &db,
        );
        assert!(db.list_nodes_by_kind("Task", 100, 0).unwrap().is_empty());
    }

    #[test]
    fn foreach_merge_keyed_by_loop_variable_is_idempotent() {
        let db = drevo();
        run("CREATE (:Bug {title: 'crash'})", &db);
        // MERGE inside FOREACH, keyed on the loop variable — the canonical
        // "tag each from a list" idiom. Running it twice must not duplicate.
        let q = "MATCH (b:Bug {title: 'crash'}) \
                 FOREACH (lbl IN ['regression', 'p1'] | \
                   MERGE (l:Label {title: lbl}) \
                   MERGE (b)-[:TAGGED]->(l))";
        run(q, &db);
        run(q, &db);
        let res = run(
            "MATCH (:Bug {title: 'crash'})-[:TAGGED]->(l:Label) RETURN count(l)",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn match_node_property_filter_resolves_a_bound_variable() {
        let db = drevo();
        run(
            "CREATE (:Tag {title: 'urgent'}) CREATE (:Item {title: 'I1', tag: 'urgent'}) \
             CREATE (:Item {title: 'I2', tag: 'low'})",
            &db,
        );
        // The second pattern's property filter references `t.title`, a value
        // bound by the first MATCH — previously this raised UnboundVariable
        // because filters evaluated against an empty row.
        let res = run(
            "MATCH (t:Tag {title: 'urgent'}) MATCH (i:Item {tag: t.title}) RETURN i.title",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::String("I1".into())]]);
    }

    #[test]
    fn foreach_body_rejects_read_clause_at_parse_time() {
        // A read clause inside FOREACH is a grammar error — the body is
        // restricted to update clauses.
        let e = parse("FOREACH (x IN [1] | MATCH (n:Task) SET n.done = true)")
            .expect_err("expected parse error");
        assert!(
            matches!(e, crate::cypher::parser::ParseError::Expected { .. }),
            "got {:?}",
            e
        );
    }

    // ---- Variable-length paths (00069) ----------------------------------

    fn varlen_chain(db: &Drevo, names: &[&str]) {
        // CREATE (:N {name: names[0]})-[:NEXT]->(:N {name: names[1]})-[:NEXT]->...
        if names.is_empty() {
            return;
        }
        let mut q = format!("CREATE (:N {{name: '{}'}})", names[0]);
        for next in &names[1..] {
            q = format!("{} CREATE (:N {{name: '{}'}})", q, next);
        }
        run(&q, db);
        for pair in names.windows(2) {
            run(
                &format!(
                    "MATCH (a:N {{name: '{}'}}), (b:N {{name: '{}'}}) CREATE (a)-[:NEXT]->(b)",
                    pair[0], pair[1]
                ),
                db,
            );
        }
    }

    #[test]
    fn varlen_exact_two_hops_matches_only_two_step_paths() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C", "D"]);
        // A→B→C→D : exactly 2 hops from A reaches C.
        let res = run(
            "MATCH (a:N {name: 'A'})-[:NEXT*2]->(b:N) RETURN b.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("C".into()));
    }

    #[test]
    fn varlen_range_one_to_three_returns_all_reachable() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C", "D"]);
        let res = run(
            "MATCH (a:N {name: 'A'})-[:NEXT*1..3]->(b:N) RETURN b.name AS name ORDER BY name",
            &db,
        );
        // 1: B, 2: C, 3: D
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0][0], Value::String("B".into()));
        assert_eq!(res.rows[1][0], Value::String("C".into()));
        assert_eq!(res.rows[2][0], Value::String("D".into()));
    }

    #[test]
    fn varlen_unbounded_star_returns_all_reachable() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C", "D"]);
        let res = run(
            "MATCH (a:N {name: 'A'})-[:NEXT*]->(b:N) RETURN b.name AS name ORDER BY name",
            &db,
        );
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0][0], Value::String("B".into()));
        assert_eq!(res.rows[1][0], Value::String("C".into()));
        assert_eq!(res.rows[2][0], Value::String("D".into()));
    }

    #[test]
    fn varlen_zero_hop_lower_includes_source() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C"]);
        let res = run(
            "MATCH (a:N {name: 'A'})-[:NEXT*0..2]->(b:N) RETURN b.name AS name ORDER BY name",
            &db,
        );
        // 0: A, 1: B, 2: C
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[1][0], Value::String("B".into()));
        assert_eq!(res.rows[2][0], Value::String("C".into()));
    }

    #[test]
    fn varlen_no_relationship_is_repeated_within_a_single_path() {
        let db = drevo();
        // A cycle: A → B → A. With *2 we'd reach A again only by reusing
        // an edge, which Cypher's "trail" uniqueness forbids — so the
        // result must be empty even though the cycle exists.
        run("CREATE (:N {name: 'A'})-[:R]->(:N {name: 'B'})", &db);
        run(
            "MATCH (a:N {name: 'B'}), (b:N {name: 'A'}) CREATE (a)-[:R]->(b)",
            &db,
        );
        // Two-hop with strict relationship-uniqueness: A→B→A traverses
        // two distinct edges → that's allowed.
        let res = run(
            "MATCH (a:N {name: 'A'})-[:R*2]->(b:N) RETURN b.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        // Three-hop would require reusing one of the two edges → no result.
        let res = run(
            "MATCH (a:N {name: 'A'})-[:R*3]->(b:N) RETURN b.name AS name",
            &db,
        );
        assert!(res.rows.is_empty());
    }

    #[test]
    fn varlen_relationship_variable_yields_list_of_relationships() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C"]);
        let res = run("MATCH (a:N {name: 'A'})-[r:NEXT*2]->(b:N) RETURN r", &db);
        assert_eq!(res.rows.len(), 1);
        match &res.rows[0][0] {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                for item in items {
                    assert!(
                        matches!(item, Value::Relationship(_)),
                        "expected relationship, got {:?}",
                        item
                    );
                }
            }
            other => panic!("expected list, got {:?}", other),
        }
    }

    #[test]
    fn varlen_with_target_node_filter() {
        let db = drevo();
        varlen_chain(&db, &["A", "B", "C", "D"]);
        let res = run(
            "MATCH (a:N {name: 'A'})-[:NEXT*1..3]->(b:N {name: 'C'}) RETURN b.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("C".into()));
    }

    #[test]
    fn varlen_with_optional_match_yields_null_on_no_path() {
        let db = drevo();
        run("CREATE (:N {name: 'A'})", &db);
        // No edges out of A — OPTIONAL MATCH with varlen should still
        // emit one row with `b = NULL`.
        let res = run(
            "MATCH (a:N {name: 'A'}) OPTIONAL MATCH (a)-[:NEXT*]->(b:N) RETURN a.name AS who, b.name AS friend",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("A".into()));
        assert_eq!(res.rows[0][1], Value::Null);
    }

    #[test]
    fn varlen_invalid_range_errors_cleanly() {
        let db = drevo();
        run("CREATE (:N {name: 'A'})", &db);
        let e = err("MATCH (a:N)-[:NEXT*5..2]->(b:N) RETURN b", &db);
        assert!(
            matches!(e, ExecError::InvalidMutation(ref s) if s.contains("variable-length range")),
            "got {:?}",
            e
        );
    }

    #[test]
    fn varlen_create_still_unsupported() {
        let db = drevo();
        // CREATE with varlen path makes no sense — must remain rejected.
        let e = err("CREATE (a:N)-[:NEXT*1..3]->(b:N)", &db);
        assert!(
            matches!(e, ExecError::Unsupported { ref feature, .. } if feature.contains("variable-length CREATE")),
            "got {:?}",
            e
        );
    }

    // ---- CALL / YIELD procedures (00145) ----------------------------------

    /// Collect a single-column result's rows as `String`s, for terse
    /// assertions on the introspection procedures.
    fn string_column(res: &ExecResult) -> Vec<String> {
        res.rows
            .iter()
            .map(|r| match &r[0] {
                Value::String(s) => s.clone(),
                other => panic!("expected String, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn call_db_labels_standalone_lists_sorted_distinct_labels() {
        let db = drevo();
        run("CREATE (:Person), (:Person), (:Company)", &db);
        let res = run("CALL db.labels()", &db);
        assert_eq!(res.columns, vec!["label"]);
        assert_eq!(string_column(&res), vec!["Company", "Person"]);
    }

    #[test]
    fn call_db_labels_includes_secondary_labels() {
        let db = drevo();
        run("CREATE (n:Person:Employee)", &db);
        let res = run("CALL db.labels()", &db);
        assert_eq!(string_column(&res), vec!["Employee", "Person"]);
    }

    #[test]
    fn call_db_labels_on_empty_graph_yields_no_rows() {
        let db = drevo();
        let res = run("CALL db.labels()", &db);
        assert_eq!(res.columns, vec!["label"]);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn call_db_relationship_types_lists_sorted_distinct_kinds() {
        let db = drevo();
        run("CREATE (a:N)-[:KNOWS]->(b:N)-[:LIKES]->(c:N)", &db);
        run("CREATE (d:N)-[:KNOWS]->(e:N)", &db);
        let res = run("CALL db.relationshipTypes()", &db);
        assert_eq!(res.columns, vec!["relationshipType"]);
        assert_eq!(string_column(&res), vec!["KNOWS", "LIKES"]);
    }

    #[test]
    fn call_db_property_keys_unions_node_and_edge_keys() {
        let db = drevo();
        run(
            "CREATE (a:N {name: 'x', age: 1})-[:R {since: 2020}]->(b:N {name: 'y'})",
            &db,
        );
        let res = run("CALL db.propertyKeys()", &db);
        assert_eq!(res.columns, vec!["propertyKey"]);
        // Sorted, distinct across nodes + edges. drevo auto-assigns a
        // unique `title` to every node (to keep the title-uniqueness
        // invariant), so `title` is always a real, queryable property key.
        // The reserved `_labels` key is never surfaced.
        let keys = string_column(&res);
        assert_eq!(keys, vec!["age", "name", "since", "title"]);
        assert!(!keys.contains(&"_labels".to_string()));
    }

    #[test]
    fn call_yield_binds_column_for_downstream_return() {
        let db = drevo();
        run("CREATE (:Person), (:Company)", &db);
        let res = run("CALL db.labels() YIELD label RETURN label", &db);
        assert_eq!(res.columns, vec!["label"]);
        assert_eq!(string_column(&res), vec!["Company", "Person"]);
    }

    #[test]
    fn call_yield_alias_renames_bound_variable() {
        let db = drevo();
        run("CREATE (:Person)", &db);
        let res = run("CALL db.labels() YIELD label AS l RETURN l", &db);
        assert_eq!(res.columns, vec!["l"]);
        assert_eq!(string_column(&res), vec!["Person"]);
    }

    #[test]
    fn call_yield_where_filters_rows() {
        let db = drevo();
        run("CREATE (:Person), (:Company), (:Project)", &db);
        let res = run(
            "CALL db.labels() YIELD label WHERE label = 'Person' RETURN label",
            &db,
        );
        assert_eq!(string_column(&res), vec!["Person"]);
    }

    #[test]
    fn call_yield_feeds_aggregation() {
        let db = drevo();
        run("CREATE (:Person), (:Company), (:Project)", &db);
        let res = run("CALL db.labels() YIELD label RETURN count(label) AS n", &db);
        assert_eq!(res.rows[0][0], Value::Integer(3));
    }

    #[test]
    fn call_unknown_procedure_is_invalid_procedure_call() {
        let db = drevo();
        let e = err("CALL db.bogus()", &db);
        assert!(
            matches!(e, ExecError::InvalidProcedureCall { ref name, .. } if name == "db.bogus"),
            "got {e:?}"
        );
    }

    #[test]
    fn call_with_arguments_is_rejected() {
        let db = drevo();
        let e = err("CALL db.labels('extra')", &db);
        assert!(
            matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("0 arguments")),
            "got {e:?}"
        );
    }

    #[test]
    fn call_yield_unknown_column_is_rejected() {
        let db = drevo();
        let e = err("CALL db.labels() YIELD nope RETURN nope", &db);
        assert!(
            matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("does not yield")),
            "got {e:?}"
        );
    }

    #[test]
    fn call_unknown_procedure_rejected_on_empty_graph_before_side_effects() {
        // The upfront sweep must surface the error deterministically even
        // when the graph is empty (no rows to produce).
        let db = drevo();
        let e = err("CALL db.bogus() YIELD x RETURN x", &db);
        assert!(
            matches!(e, ExecError::InvalidProcedureCall { .. }),
            "got {e:?}"
        );
    }

    // ---- similar() scalar function (00077) --------------------------------

    /// A zero span for unit-testing the pure helpers (`Span` carries no
    /// `Default`, so we spell one out).
    fn zero_span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 0,
            column: 0,
        }
    }

    #[test]
    fn scalar_function_name_recognises_similar_case_insensitively() {
        assert!(is_scalar_function_name(&["similar".to_string()]));
        assert!(is_scalar_function_name(&["SIMILAR".to_string()]));
        assert!(is_scalar_function_name(&["Similar".to_string()]));
        assert!(is_scalar_function_name(&["keywords".to_string()]));
        assert!(is_scalar_function_name(&["KEYWORDS".to_string()]));
        // The `00138` built-ins are recognised (case-insensitively) too.
        assert!(is_scalar_function_name(&["size".to_string()]));
        assert!(is_scalar_function_name(&["toUpper".to_string()]));
        assert!(is_scalar_function_name(&["COALESCE".to_string()]));
        // Aggregations and unknown names are not scalar functions.
        assert!(!is_scalar_function_name(&["count".to_string()]));
        assert!(!is_scalar_function_name(&["nosuchfn".to_string()]));
        // A dotted name is never a built-in scalar function.
        assert!(!is_scalar_function_name(&[
            "apoc".to_string(),
            "similar".to_string()
        ]));
    }

    #[test]
    fn similar_operand_accepts_mixed_int_and_float_elements() {
        let span = zero_span();
        let v = Value::List(vec![
            Value::Integer(1),
            Value::Float(2.5),
            Value::Integer(0),
        ]);
        assert_eq!(
            similar_operand(&v, "similar", "vector", span).unwrap(),
            vec![1.0_f32, 2.5, 0.0]
        );
    }

    #[test]
    fn similar_operand_rejects_non_list() {
        let span = zero_span();
        let e =
            similar_operand(&Value::String("nope".into()), "similar", "query", span).unwrap_err();
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    }

    #[test]
    fn similar_operand_rejects_non_numeric_element() {
        let span = zero_span();
        let v = Value::List(vec![Value::Float(1.0), Value::Bool(true)]);
        let e = similar_operand(&v, "similar", "vector", span).unwrap_err();
        match e {
            ExecError::InvalidFunctionCall { message, .. } => {
                assert!(message.contains("index 1"), "message was {message:?}");
            }
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn zero_magnitude_embedding_is_an_error() {
        let db = drevo();
        run("CREATE (:Doc {title: 'zero', embedding: [0.0, 0.0]})", &db);
        let mut params = HashMap::new();
        params.insert(
            "q".to_string(),
            Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
        );
        let query =
            parse("MATCH (d:Doc) WHERE similar(d.embedding, $q, 0.5) RETURN d.title").unwrap();
        let e = execute(&query, &db, params).expect_err("zero vector must error");
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    }

    // ---- UNION (00136) ----------------------------------------------------

    #[test]
    fn union_all_concatenates_arm_rows() {
        let db = drevo();
        let res = run("RETURN 1 AS n UNION ALL RETURN 2 AS n", &db);
        assert_eq!(res.columns, vec!["n"]);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::Integer(1));
        assert_eq!(res.rows[1][0], Value::Integer(2));
    }

    #[test]
    fn union_all_keeps_duplicates() {
        let db = drevo();
        let res = run("RETURN 7 AS n UNION ALL RETURN 7 AS n", &db);
        assert_eq!(res.rows.len(), 2);
    }

    #[test]
    fn union_distinct_dedups_across_arms() {
        let db = drevo();
        let res = run("RETURN 7 AS n UNION RETURN 7 AS n", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Integer(7));
    }

    #[test]
    fn union_distinct_preserves_first_seen_order() {
        let db = drevo();
        let res = run("RETURN 2 AS n UNION RETURN 1 AS n UNION RETURN 2 AS n", &db);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0][0], Value::Integer(2));
        assert_eq!(res.rows[1][0], Value::Integer(1));
    }

    #[test]
    fn union_mismatched_column_names_errors() {
        let db = drevo();
        let e = err("RETURN 1 AS a UNION RETURN 2 AS b", &db);
        assert!(matches!(e, ExecError::UnionMismatch { .. }), "{e:?}");
    }

    #[test]
    fn union_swapped_column_order_errors() {
        let db = drevo();
        let e = err("RETURN 1 AS a, 2 AS b UNION RETURN 3 AS b, 4 AS a", &db);
        assert!(matches!(e, ExecError::UnionMismatch { .. }), "{e:?}");
    }

    #[test]
    fn union_different_column_count_errors() {
        let db = drevo();
        let e = err("RETURN 1 AS a UNION RETURN 2 AS a, 3 AS b", &db);
        assert!(matches!(e, ExecError::UnionMismatch { .. }), "{e:?}");
    }

    #[test]
    fn mixing_union_and_union_all_errors() {
        let db = drevo();
        let e = err(
            "RETURN 1 AS n UNION RETURN 2 AS n UNION ALL RETURN 3 AS n",
            &db,
        );
        match e {
            ExecError::UnionMismatch { message, .. } => {
                assert!(message.contains("mix"), "message was {message:?}");
            }
            other => panic!("expected UnionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn union_mismatch_carries_a_span() {
        let db = drevo();
        let e = err("RETURN 1 AS a UNION RETURN 2 AS b", &db);
        assert!(e.span().is_some());
    }

    #[test]
    fn union_arm_unsupported_construct_surfaces() {
        let db = drevo();
        // A function with no executor implementation (an unknown name, since
        // the `00138` built-ins plus `similar` / `keywords` are now all
        // recognised) stays `Unsupported` when it appears inside a UNION arm.
        let e = err("RETURN 1 AS n UNION RETURN nosuchfn('x') AS n", &db);
        assert!(matches!(e, ExecError::Unsupported { .. }), "{e:?}");
    }

    #[test]
    fn union_accumulates_stats_across_arms() {
        let db = drevo();
        let res = run(
            "CREATE (:Note {title: 'A'}) RETURN 1 AS k \
             UNION ALL \
             CREATE (:Note {title: 'B'}) RETURN 2 AS k",
            &db,
        );
        assert_eq!(res.stats.nodes_created, 2);
        assert_eq!(res.rows.len(), 2);
    }

    // ---- CASE expression (00137) ------------------------------------------

    #[test]
    fn case_generic_selects_first_true_arm() {
        let db = drevo();
        let res = run(
            "RETURN CASE WHEN false THEN 'a' WHEN true THEN 'b' ELSE 'c' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("b".into()));
    }

    #[test]
    fn case_generic_else_when_all_false() {
        let db = drevo();
        let res = run("RETURN CASE WHEN false THEN 1 ELSE 2 END AS r", &db);
        assert_eq!(res.rows[0][0], Value::Integer(2));
    }

    #[test]
    fn case_generic_no_else_and_no_match_is_null() {
        let db = drevo();
        let res = run("RETURN CASE WHEN false THEN 1 END AS r", &db);
        assert_eq!(res.rows[0][0], Value::Null);
    }

    #[test]
    fn case_generic_null_condition_skipped() {
        let db = drevo();
        let res = run(
            "RETURN CASE WHEN null THEN 'a' WHEN true THEN 'b' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("b".into()));
    }

    #[test]
    fn case_generic_non_boolean_condition_is_type_error() {
        let db = drevo();
        let e = err("RETURN CASE WHEN 1 THEN 'a' END AS r", &db);
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn case_simple_matches_scrutinee_by_equality() {
        let db = drevo();
        let res = run(
            "WITH 2 AS x RETURN CASE x WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'n' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("two".into()));
    }

    #[test]
    fn case_simple_null_scrutinee_falls_to_else() {
        let db = drevo();
        // `null = null` is `null`, so the scrutinee never matches a WHEN arm.
        let res = run(
            "WITH null AS x RETURN CASE x WHEN null THEN 'isnull' ELSE 'other' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("other".into()));
    }

    #[test]
    fn case_simple_no_else_and_no_match_is_null() {
        let db = drevo();
        let res = run("WITH 9 AS x RETURN CASE x WHEN 1 THEN 'one' END AS r", &db);
        assert_eq!(res.rows[0][0], Value::Null);
    }

    #[test]
    fn case_usable_in_where_clause() {
        let db = drevo();
        run("CREATE (:N {title: 'keep', v: 5})", &db);
        run("CREATE (:N {title: 'drop', v: 1})", &db);
        let res = run(
            "MATCH (n:N) WHERE (CASE WHEN n.v > 3 THEN true ELSE false END) RETURN n.title AS t",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("keep".into()));
    }

    // ---- Aggregations nested inside a CASE arm (00142) -------------------

    #[test]
    fn case_generic_then_aggregation_folds_over_group() {
        let db = drevo();
        run("CREATE (:N), (:N), (:N)", &db);
        // count(*) inside a THEN folds over the (single) group of 3 rows.
        let res = run(
            "MATCH (n:N) RETURN CASE WHEN true THEN count(*) ELSE 0 END AS r",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Integer(3));
    }

    #[test]
    fn case_generic_aggregation_in_when_condition() {
        let db = drevo();
        run("CREATE (:N), (:N), (:N)", &db);
        let res = run(
            "MATCH (n:N) RETURN CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("many".into()));
    }

    #[test]
    fn case_aggregation_picks_else_on_empty_group() {
        let db = drevo();
        // No N nodes exist — the pure-aggregation group is synthetic and
        // count(*) is 0, so the CASE selects the ELSE branch.
        let res = run(
            "MATCH (n:N) RETURN CASE WHEN count(*) > 0 THEN 'some' ELSE 'none' END AS r",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("none".into()));
    }

    #[test]
    fn case_aggregation_in_else_branch() {
        let db = drevo();
        run("CREATE (:N {v: 10}), (:N {v: 20})", &db);
        let res = run(
            "MATCH (n:N) RETURN CASE WHEN false THEN 0 ELSE sum(n.v) END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Integer(30));
    }

    #[test]
    fn case_with_group_key_evaluates_aggregation_per_group() {
        let db = drevo();
        run(
            "CREATE (:T {status: 'open'}), (:T {status: 'open'}), (:T {status: 'done'})",
            &db,
        );
        // Group by status; the CASE column folds count(*) per group.
        let mut res = run(
            "MATCH (t:T) RETURN t.status AS s, \
             CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END AS label \
             ORDER BY s",
            &db,
        );
        let rows = std::mem::take(&mut res.rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::String("done".into()));
        assert_eq!(rows[0][1], Value::String("one".into()));
        assert_eq!(rows[1][0], Value::String("open".into()));
        assert_eq!(rows[1][1], Value::String("many".into()));
    }

    #[test]
    fn case_simple_form_with_aggregation_scrutinee() {
        let db = drevo();
        run("CREATE (:N), (:N)", &db);
        // Simple form: scrutinee count(*) compared by equality to WHEN values.
        let res = run(
            "MATCH (n:N) RETURN CASE count(*) WHEN 2 THEN 'pair' WHEN 1 THEN 'single' ELSE 'other' END AS r",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::String("pair".into()));
    }

    #[test]
    fn case_nested_aggregation_inside_aggregation_is_rejected() {
        let db = drevo();
        // An aggregation directly nested inside another aggregation stays
        // rejected even when reached through a CASE arm, matching Neo4j.
        let e = err(
            "MATCH (n) RETURN sum(CASE WHEN true THEN count(*) ELSE 0 END) AS r",
            &db,
        );
        assert!(matches!(e, ExecError::InvalidMutation(_)), "{e:?}");
    }

    #[test]
    fn case_aggregation_in_arm_usable_in_with_filter() {
        let db = drevo();
        run(
            "CREATE (:T {status: 'open'}), (:T {status: 'open'}), (:T {status: 'done'})",
            &db,
        );
        // The CASE-with-aggregation column survives a post-aggregation WHERE.
        let res = run(
            "MATCH (t:T) \
             WITH t.status AS s, CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END AS label \
             WHERE label = 'many' RETURN s",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("open".into()));
    }

    // ---- Scalar functions (00138) ---------------------------------------

    /// Evaluate a single scalar expression via `RETURN <expr> AS v` and
    /// return the one projected value.
    fn scalar(source: &str, db: &Drevo) -> Value {
        let res = run(source, db);
        assert_eq!(res.rows.len(), 1, "expected one row from {source:?}");
        res.rows[0][0].clone()
    }

    #[test]
    fn rand_helper_stays_in_unit_interval_and_advances() {
        // `scalar_rand` must always land in [0,1) and the thread-local state
        // must advance, so two consecutive draws (almost surely) differ.
        let mut prev = None;
        let mut distinct = 0usize;
        for _ in 0..1000 {
            match super::scalar_rand(Vec::new(), zero_span()).unwrap() {
                Value::Float(f) => {
                    assert!((0.0..1.0).contains(&f), "rand() out of [0,1): {f}");
                    if Some(f.to_bits()) != prev {
                        distinct += 1;
                    }
                    prev = Some(f.to_bits());
                }
                other => panic!("expected Float, got {other:?}"),
            }
        }
        // A stuck generator would yield `distinct == 1`; splitmix64 advances.
        assert!(distinct > 900, "rand() barely advanced: {distinct} changes");
    }

    #[test]
    fn rand_and_randomuuid_reject_arguments() {
        assert!(matches!(
            super::scalar_rand(vec![Value::Integer(1)], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
        assert!(matches!(
            super::scalar_random_uuid(vec![Value::Integer(1)], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
    }

    #[test]
    fn datetime_and_timestamp_are_builtins() {
        assert!(super::is_builtin_scalar_function("datetime"));
        assert!(super::is_builtin_scalar_function("timestamp"));
    }

    #[test]
    fn timestamp_and_datetime_reject_arguments() {
        assert!(matches!(
            super::scalar_timestamp(vec![Value::Integer(1)], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
        assert!(matches!(
            super::scalar_datetime(vec![Value::Integer(1)], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
    }

    #[test]
    fn timestamp_returns_recent_epoch_millis() {
        match super::scalar_timestamp(Vec::new(), zero_span()).unwrap() {
            Value::Integer(ms) => {
                assert!(ms > 1_600_000_000_000, "expected recent epoch ms, got {ms}")
            }
            other => panic!("expected Integer, got {other:?}"),
        }
    }

    #[test]
    fn datetime_returns_iso8601_string() {
        match super::scalar_datetime(Vec::new(), zero_span()).unwrap() {
            Value::String(s) => {
                assert_eq!(s.len(), 24, "iso8601 len: {s:?}");
                assert!(s.ends_with('Z'), "{s:?}");
                assert_eq!(&s[4..5], "-");
                assert_eq!(&s[10..11], "T");
            }
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn iso8601_utc_known_instants() {
        assert_eq!(super::iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            super::iso8601_utc(1_609_459_200_000),
            "2021-01-01T00:00:00.000Z"
        );
        assert_eq!(
            super::iso8601_utc(1_609_459_200_000 + 3_661_001),
            "2021-01-01T01:01:01.001Z"
        );
        // Leap day.
        assert_eq!(
            super::iso8601_utc(1_456_704_000_000),
            "2016-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn random_uuid_helper_emits_canonical_v4() {
        for _ in 0..256 {
            match super::scalar_random_uuid(Vec::new(), zero_span()).unwrap() {
                Value::String(s) => {
                    assert_eq!(s.len(), 36, "uuid len: {s:?}");
                    let b = s.as_bytes();
                    assert_eq!(b[14], b'4', "version nibble must be 4: {s:?}");
                    assert!(
                        matches!(b[19], b'8' | b'9' | b'a' | b'b'),
                        "variant nibble must be 8/9/a/b: {s:?}"
                    );
                    assert!(
                        s.chars()
                            .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                        "uuid must be lowercase hex/hyphen: {s:?}"
                    );
                }
                other => panic!("expected String, got {other:?}"),
            }
        }
    }

    #[test]
    fn is_nan_helper_classifies_values() {
        // NaN floats -> true; ordinary floats, infinities, and integers -> false.
        assert_eq!(
            super::scalar_is_nan(vec![Value::Float(f64::NAN)], zero_span()).unwrap(),
            Value::Bool(true)
        );
        for v in [
            Value::Float(0.0),
            Value::Float(2.5),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Integer(0),
            Value::Integer(42),
        ] {
            assert_eq!(
                super::scalar_is_nan(vec![v.clone()], zero_span()).unwrap(),
                Value::Bool(false),
                "isNaN({v:?}) should be false"
            );
        }
    }

    #[test]
    fn is_nan_helper_rejects_non_numeric_and_bad_arity() {
        assert!(matches!(
            super::scalar_is_nan(vec![Value::String("x".into())], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
        assert!(matches!(
            super::scalar_is_nan(vec![], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
        assert!(matches!(
            super::scalar_is_nan(vec![Value::Float(1.0), Value::Float(2.0)], zero_span()),
            Err(ExecError::InvalidFunctionCall { .. })
        ));
    }

    #[test]
    fn string_case_and_trim_functions() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN toLower('HeLLo') AS v", &db),
            Value::String("hello".into())
        );
        assert_eq!(
            scalar("RETURN toUpper('HeLLo') AS v", &db),
            Value::String("HELLO".into())
        );
        assert_eq!(
            scalar("RETURN trim('  hi  ') AS v", &db),
            Value::String("hi".into())
        );
        assert_eq!(
            scalar("RETURN ltrim('  hi  ') AS v", &db),
            Value::String("hi  ".into())
        );
        assert_eq!(
            scalar("RETURN rtrim('  hi  ') AS v", &db),
            Value::String("  hi".into())
        );
    }

    #[test]
    fn substring_two_and_three_arg() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN substring('hello', 1) AS v", &db),
            Value::String("ello".into())
        );
        assert_eq!(
            scalar("RETURN substring('hello', 1, 3) AS v", &db),
            Value::String("ell".into())
        );
        // start past the end yields the empty string, length over-runs clamp.
        assert_eq!(
            scalar("RETURN substring('hi', 9) AS v", &db),
            Value::String("".into())
        );
        assert_eq!(
            scalar("RETURN substring('hi', 1, 99) AS v", &db),
            Value::String("i".into())
        );
    }

    #[test]
    fn substring_negative_start_is_error() {
        let db = drevo();
        let e = err("RETURN substring('hi', -1) AS v", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    }

    #[test]
    fn replace_split_left_right_reverse() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN replace('a.b.c', '.', '-') AS v", &db),
            Value::String("a-b-c".into())
        );
        // empty search leaves the string unchanged.
        assert_eq!(
            scalar("RETURN replace('abc', '', 'X') AS v", &db),
            Value::String("abc".into())
        );
        assert_eq!(
            scalar("RETURN split('a,b,c', ',') AS v", &db),
            Value::List(vec![
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into())
            ])
        );
        assert_eq!(
            scalar("RETURN left('hello', 2) AS v", &db),
            Value::String("he".into())
        );
        assert_eq!(
            scalar("RETURN right('hello', 2) AS v", &db),
            Value::String("lo".into())
        );
        assert_eq!(
            scalar("RETURN reverse('abc') AS v", &db),
            Value::String("cba".into())
        );
        assert_eq!(
            scalar("RETURN reverse([1, 2, 3]) AS v", &db),
            Value::List(vec![
                Value::Integer(3),
                Value::Integer(2),
                Value::Integer(1)
            ])
        );
    }

    #[test]
    fn tostring_renders_each_scalar_type() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN toString(42) AS v", &db),
            Value::String("42".into())
        );
        assert_eq!(
            scalar("RETURN toString(true) AS v", &db),
            Value::String("true".into())
        );
        // an integral float keeps a trailing .0
        assert_eq!(
            scalar("RETURN toString(2.0) AS v", &db),
            Value::String("2.0".into())
        );
        assert_eq!(
            scalar("RETURN toString(1.5) AS v", &db),
            Value::String("1.5".into())
        );
    }

    #[test]
    fn numeric_functions_preserve_or_widen_type() {
        let db = drevo();
        // abs preserves Integer vs Float.
        assert_eq!(scalar("RETURN abs(-3) AS v", &db), Value::Integer(3));
        assert_eq!(scalar("RETURN abs(-2.5) AS v", &db), Value::Float(2.5));
        // ceil / floor / round / sqrt widen to Float.
        assert_eq!(scalar("RETURN ceil(1.1) AS v", &db), Value::Float(2.0));
        assert_eq!(scalar("RETURN floor(1.9) AS v", &db), Value::Float(1.0));
        assert_eq!(scalar("RETURN round(1.5) AS v", &db), Value::Float(2.0));
        assert_eq!(scalar("RETURN sqrt(9) AS v", &db), Value::Float(3.0));
        // sign returns an Integer in {-1, 0, 1}.
        assert_eq!(scalar("RETURN sign(-7) AS v", &db), Value::Integer(-1));
        assert_eq!(scalar("RETURN sign(0) AS v", &db), Value::Integer(0));
        assert_eq!(scalar("RETURN sign(7) AS v", &db), Value::Integer(1));
    }

    #[test]
    fn to_integer_float_boolean_conversions() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN toInteger('42') AS v", &db),
            Value::Integer(42)
        );
        assert_eq!(scalar("RETURN toInteger(3.9) AS v", &db), Value::Integer(3));
        // unparseable string => NULL, never an error.
        assert_eq!(scalar("RETURN toInteger('abc') AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN toFloat('1.5') AS v", &db), Value::Float(1.5));
        assert_eq!(scalar("RETURN toFloat(2) AS v", &db), Value::Float(2.0));
        assert_eq!(scalar("RETURN toFloat('x') AS v", &db), Value::Null);
        assert_eq!(
            scalar("RETURN toBoolean('TRUE') AS v", &db),
            Value::Bool(true)
        );
        assert_eq!(scalar("RETURN toBoolean(0) AS v", &db), Value::Bool(false));
        assert_eq!(scalar("RETURN toBoolean('maybe') AS v", &db), Value::Null);
    }

    #[test]
    fn list_value_conversions_are_elementwise_and_lenient() {
        let db = drevo();
        // Each element converts via the scalar rules; unconvertible elements
        // (and NULL elements) become NULL while preserving list length.
        assert_eq!(
            scalar(
                r#"RETURN toIntegerList([1, 2.9, "3", "x", null]) AS v"#,
                &db
            ),
            Value::List(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Null,
                Value::Null,
            ])
        );
        assert_eq!(
            scalar(r#"RETURN toFloatList([1, "2.5", true]) AS v"#, &db),
            Value::List(vec![Value::Float(1.0), Value::Float(2.5), Value::Null])
        );
        assert_eq!(
            scalar(r#"RETURN toBooleanList(["true", 0, "no"]) AS v"#, &db),
            Value::List(vec![Value::Bool(true), Value::Bool(false), Value::Null])
        );
        assert_eq!(
            scalar(r#"RETURN toStringList([1, 2.5, [3]]) AS v"#, &db),
            Value::List(vec![
                Value::String("1".into()),
                Value::String("2.5".into()),
                Value::Null,
            ])
        );
        // NULL argument => NULL; non-list argument => recoverable error.
        assert_eq!(scalar("RETURN toIntegerList(null) AS v", &db), Value::Null);
        match err("RETURN toFloatList(42) AS v", &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn or_null_conversions_are_fully_lenient() {
        let db = drevo();
        // Happy paths mirror the strict scalar conversions.
        assert_eq!(
            scalar("RETURN toIntegerOrNull('42') AS v", &db),
            Value::Integer(42)
        );
        assert_eq!(
            scalar("RETURN toFloatOrNull(2) AS v", &db),
            Value::Float(2.0)
        );
        assert_eq!(
            scalar("RETURN toBooleanOrNull('TRUE') AS v", &db),
            Value::Bool(true)
        );
        assert_eq!(
            scalar("RETURN toStringOrNull(42) AS v", &db),
            Value::String("42".into())
        );
        // Unconvertible inputs yield NULL, never an error — including the cases
        // where the strict scalar `toString` would error (a List / Map).
        assert_eq!(
            scalar("RETURN toIntegerOrNull('abc') AS v", &db),
            Value::Null
        );
        assert_eq!(scalar("RETURN toFloatOrNull('x') AS v", &db), Value::Null);
        assert_eq!(
            scalar("RETURN toBooleanOrNull('maybe') AS v", &db),
            Value::Null
        );
        assert_eq!(
            scalar("RETURN toStringOrNull([1, 2]) AS v", &db),
            Value::Null
        );
        // NULL argument propagates to NULL.
        assert_eq!(
            scalar("RETURN toIntegerOrNull(null) AS v", &db),
            Value::Null
        );
        // Wrong arity is still a recoverable error.
        match err("RETURN toStringOrNull(1, 2) AS v", &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn is_empty_over_the_three_container_types() {
        let db = drevo();
        // Empty containers => true; non-empty => false; across String/List/Map.
        assert_eq!(scalar("RETURN isEmpty('') AS v", &db), Value::Bool(true));
        assert_eq!(scalar("RETURN isEmpty('x') AS v", &db), Value::Bool(false));
        assert_eq!(scalar("RETURN isEmpty([]) AS v", &db), Value::Bool(true));
        assert_eq!(scalar("RETURN isEmpty([1]) AS v", &db), Value::Bool(false));
        assert_eq!(scalar("RETURN isEmpty({}) AS v", &db), Value::Bool(true));
        assert_eq!(
            scalar("RETURN isEmpty({a: 1}) AS v", &db),
            Value::Bool(false)
        );
        // NULL argument propagates to NULL.
        assert_eq!(scalar("RETURN isEmpty(null) AS v", &db), Value::Null);
        // A non-container argument is a recoverable error, as is wrong arity.
        match err("RETURN isEmpty(5) AS v", &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
        match err("RETURN isEmpty('a', 'b') AS v", &db) {
            ExecError::InvalidFunctionCall { .. } => {}
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn round_decimal_unit_modes_and_precision() {
        use super::{round_decimal, RoundingMode};
        // Decimal-faithful HALF_UP at a precision the binary float can't hold
        // exactly: a naive `(x * 100).round() / 100` would yield 1.25.
        assert!((round_decimal(1.255, 2, RoundingMode::HalfUp) - 1.26).abs() < 1e-9);
        // Each mode breaking the same 1.25 tie at 1 decimal place.
        assert!((round_decimal(1.25, 1, RoundingMode::HalfDown) - 1.2).abs() < 1e-9);
        assert!((round_decimal(2.5, 0, RoundingMode::HalfEven) - 2.0).abs() < 1e-9);
        assert!((round_decimal(3.5, 0, RoundingMode::HalfEven) - 4.0).abs() < 1e-9);
        // Directed modes ignore the remainder's magnitude.
        assert!((round_decimal(1.21, 1, RoundingMode::Up) - 1.3).abs() < 1e-9);
        assert!((round_decimal(1.29, 1, RoundingMode::Down) - 1.2).abs() < 1e-9);
        assert!((round_decimal(-1.21, 1, RoundingMode::Floor) - -1.3).abs() < 1e-9);
        assert!((round_decimal(-1.29, 1, RoundingMode::Ceiling) - -1.2).abs() < 1e-9);
        // Negative precision rounds to the left of the point; carry can grow the
        // digit string (9.99 → 10.0).
        assert!((round_decimal(1234.5, -2, RoundingMode::HalfUp) - 1200.0).abs() < 1e-9);
        assert!((round_decimal(9.99, 1, RoundingMode::Up) - 10.0).abs() < 1e-9);
        // Non-finite values pass through untouched.
        assert!(round_decimal(f64::NAN, 2, RoundingMode::HalfUp).is_nan());
        assert!(round_decimal(f64::INFINITY, 2, RoundingMode::HalfUp).is_infinite());
    }

    #[test]
    fn round_mode_keyword_parsing_is_case_insensitive() {
        use super::RoundingMode;
        assert!(RoundingMode::from_name("half_even").is_some());
        assert!(RoundingMode::from_name("CEILING").is_some());
        assert!(RoundingMode::from_name("Floor").is_some());
        assert!(RoundingMode::from_name("sideways").is_none());
    }

    #[test]
    fn size_length_head_last_tail() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN size([1, 2, 3]) AS v", &db),
            Value::Integer(3)
        );
        assert_eq!(scalar("RETURN size('hello') AS v", &db), Value::Integer(5));
        assert_eq!(
            scalar("RETURN length('hello') AS v", &db),
            Value::Integer(5)
        );
        assert_eq!(
            scalar("RETURN head([7, 8, 9]) AS v", &db),
            Value::Integer(7)
        );
        assert_eq!(
            scalar("RETURN last([7, 8, 9]) AS v", &db),
            Value::Integer(9)
        );
        assert_eq!(scalar("RETURN head([]) AS v", &db), Value::Null);
        assert_eq!(
            scalar("RETURN tail([7, 8, 9]) AS v", &db),
            Value::List(vec![Value::Integer(8), Value::Integer(9)])
        );
        assert_eq!(scalar("RETURN tail([]) AS v", &db), Value::List(vec![]));
    }

    #[test]
    fn range_inclusive_with_step() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN range(1, 4) AS v", &db),
            Value::List(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4)
            ])
        );
        assert_eq!(
            scalar("RETURN range(0, 10, 5) AS v", &db),
            Value::List(vec![
                Value::Integer(0),
                Value::Integer(5),
                Value::Integer(10)
            ])
        );
        // descending range with a negative step.
        assert_eq!(
            scalar("RETURN range(3, 1, -1) AS v", &db),
            Value::List(vec![
                Value::Integer(3),
                Value::Integer(2),
                Value::Integer(1)
            ])
        );
        // step pointing away from end yields the empty list.
        assert_eq!(
            scalar("RETURN range(1, 5, -1) AS v", &db),
            Value::List(vec![])
        );
    }

    #[test]
    fn range_zero_step_is_error() {
        let db = drevo();
        let e = err("RETURN range(1, 5, 0) AS v", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    }

    #[test]
    fn coalesce_returns_first_non_null() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN coalesce(null, null, 'third') AS v", &db),
            Value::String("third".into())
        );
        assert_eq!(scalar("RETURN coalesce(1, 2) AS v", &db), Value::Integer(1));
        // all-null => NULL (not an error).
        assert_eq!(scalar("RETURN coalesce(null, null) AS v", &db), Value::Null);
    }

    #[test]
    fn null_propagates_through_scalar_functions() {
        let db = drevo();
        assert_eq!(scalar("RETURN toUpper(null) AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN size(null) AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN abs(null) AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN substring(null, 0) AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN head(null) AS v", &db), Value::Null);
    }

    #[test]
    fn graph_scalar_functions_over_node_and_relationship() {
        let db = drevo();
        run(
            "CREATE (:Person {name: 'Ada', age: 36})-[:KNOWS {since: 2020}]->(:Person {name: 'Bo'})",
            &db,
        );
        // keys() over a node — sorted property names. Every node carries the
        // synthesised `title` alias (see `node_to_value`), so `keys()`
        // surfaces it alongside the user-supplied `name` / `age`, exactly the
        // property set `n.<prop>` access would see.
        assert_eq!(
            scalar("MATCH (n:Person {name: 'Ada'}) RETURN keys(n) AS v", &db),
            Value::List(vec![
                Value::String("age".into()),
                Value::String("name".into()),
                Value::String("title".into())
            ])
        );
        // labels() over a node.
        assert_eq!(
            scalar("MATCH (n:Person {name: 'Ada'}) RETURN labels(n) AS v", &db),
            Value::List(vec![Value::String("Person".into())])
        );
        // type() over a relationship. The path head must be a named node —
        // the executor looks up each segment's predecessor by variable.
        assert_eq!(
            scalar(
                "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN type(r) AS v",
                &db
            ),
            Value::String("KNOWS".into())
        );
        // properties() over a relationship returns its property map.
        let props = scalar(
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN properties(r) AS v",
            &db,
        );
        match props {
            Value::Map(m) => assert_eq!(m.get("since"), Some(&Value::Integer(2020))),
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn id_returns_node_storage_id() {
        let db = drevo();
        run("CREATE (:Doc {title: 'only'})", &db);
        // id() is an Integer >= 0; equal to the stored node id.
        let v = scalar("MATCH (n:Doc) RETURN id(n) AS v", &db);
        assert!(matches!(v, Value::Integer(_)), "{v:?}");
    }

    #[test]
    fn scalar_function_wrong_type_is_invalid_call() {
        let db = drevo();
        // size() of an Integer is a type error, not Unsupported.
        let e = err("RETURN size(5) AS v", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
        // labels() of a non-node.
        let e2 = err("RETURN labels('x') AS v", &db);
        assert!(
            matches!(e2, ExecError::InvalidFunctionCall { .. }),
            "{e2:?}"
        );
    }

    #[test]
    fn scalar_functions_compose_and_nest() {
        let db = drevo();
        run("CREATE (:Person {name: '  Ada Lovelace  '})", &db);
        // size(split(trim(toUpper(...)), ' ')) == number of words.
        assert_eq!(
            scalar(
                "MATCH (n:Person) RETURN size(split(trim(toUpper(n.name)), ' ')) AS v",
                &db
            ),
            Value::Integer(2)
        );
    }

    #[test]
    fn scalar_function_alongside_aggregation_groups_by_it() {
        let db = drevo();
        run("CREATE (:Tag {label: 'urgent'})", &db);
        run("CREATE (:Tag {label: 'URGENT'})", &db);
        run("CREATE (:Tag {label: 'later'})", &db);
        // toLower(label) becomes the grouping key; count per group.
        let res = run(
            "MATCH (t:Tag) RETURN toLower(t.label) AS k, count(*) AS c ORDER BY k",
            &db,
        );
        assert_eq!(
            res.rows,
            vec![
                vec![Value::String("later".into()), Value::Integer(1)],
                vec![Value::String("urgent".into()), Value::Integer(2)],
            ]
        );
    }

    // ---- Trigonometric / logarithmic functions (00156) -------------------

    /// Pull the `f64` out of a scalar `Float` projection.
    fn scalar_float(source: &str, db: &Drevo) -> f64 {
        match scalar(source, db) {
            Value::Float(f) => f,
            other => panic!("expected Float from {source:?}, got {other:?}"),
        }
    }

    #[test]
    fn math_constants_and_one_arg_folds() {
        let db = drevo();
        assert!((scalar_float("RETURN pi() AS v", &db) - std::f64::consts::PI).abs() < 1e-12);
        assert!((scalar_float("RETURN e() AS v", &db) - std::f64::consts::E).abs() < 1e-12);
        // exp/log are inverses; integer args widen to Float.
        assert!((scalar_float("RETURN exp(0) AS v", &db) - 1.0).abs() < 1e-12);
        assert!((scalar_float("RETURN log10(1000) AS v", &db) - 3.0).abs() < 1e-9);
        assert!((scalar_float("RETURN sin(0) AS v", &db)).abs() < 1e-12);
        assert!((scalar_float("RETURN cos(0) AS v", &db) - 1.0).abs() < 1e-12);
        assert!((scalar_float("RETURN cot(pi() / 4) AS v", &db) - 1.0).abs() < 1e-9);
        assert!((scalar_float("RETURN degrees(pi()) AS v", &db) - 180.0).abs() < 1e-9);
        assert!((scalar_float("RETURN haversin(pi()) AS v", &db) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn atan2_takes_two_arguments() {
        let db = drevo();
        assert!(
            (scalar_float("RETURN atan2(1, 1) AS v", &db) - std::f64::consts::FRAC_PI_4).abs()
                < 1e-12
        );
    }

    #[test]
    fn math_domain_edges_are_floats_not_errors() {
        let db = drevo();
        // log of a negative / asin out of range are NaN, never an error.
        assert!(scalar_float("RETURN log(-1) AS v", &db).is_nan());
        assert!(scalar_float("RETURN asin(2) AS v", &db).is_nan());
    }

    #[test]
    fn math_function_null_argument_propagates() {
        let db = drevo();
        assert_eq!(scalar("RETURN sin(null) AS v", &db), Value::Null);
        assert_eq!(scalar("RETURN atan2(null, 1) AS v", &db), Value::Null);
    }

    #[test]
    fn math_function_non_numeric_is_invalid_call() {
        let db = drevo();
        let e = err("RETURN cos('x') AS v", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
        let e2 = err("RETURN atan2(1, 's') AS v", &db);
        assert!(
            matches!(e2, ExecError::InvalidFunctionCall { .. }),
            "{e2:?}"
        );
    }

    #[test]
    fn math_function_wrong_arity_is_invalid_call() {
        let db = drevo();
        // pi/e take zero args; sin one; atan2 two.
        let e = err("RETURN pi(1) AS v", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
        let e2 = err("RETURN atan2(1) AS v", &db);
        assert!(
            matches!(e2, ExecError::InvalidFunctionCall { .. }),
            "{e2:?}"
        );
    }

    // ---- List / map indexing & slicing (00139) ----------------------------

    fn list(values: &[i64]) -> Value {
        Value::List(values.iter().map(|i| Value::Integer(*i)).collect())
    }

    #[test]
    fn list_element_zero_based_and_negative() {
        let xs = vec![Value::Integer(10), Value::Integer(20), Value::Integer(30)];
        assert_eq!(list_element(&xs, 0), Value::Integer(10));
        assert_eq!(list_element(&xs, 2), Value::Integer(30));
        // Negative counts from the end.
        assert_eq!(list_element(&xs, -1), Value::Integer(30));
        assert_eq!(list_element(&xs, -3), Value::Integer(10));
    }

    #[test]
    fn list_element_out_of_range_is_null() {
        let xs = vec![Value::Integer(1), Value::Integer(2)];
        assert_eq!(list_element(&xs, 2), Value::Null);
        assert_eq!(list_element(&xs, 99), Value::Null);
        assert_eq!(list_element(&xs, -3), Value::Null);
        assert_eq!(list_element(&[], 0), Value::Null);
    }

    #[test]
    fn eval_index_propagates_null() {
        let span = zero_span();
        assert_eq!(
            eval_index(Value::Null, Value::Integer(0), span).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_index(list(&[1, 2, 3]), Value::Null, span).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn eval_index_non_integer_list_index_is_type_error() {
        let span = zero_span();
        let e = eval_index(list(&[1, 2, 3]), Value::String("x".into()), span).unwrap_err();
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn eval_index_map_by_string_key() {
        let span = zero_span();
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), Value::Integer(1));
        let map = Value::Map(m);
        assert_eq!(
            eval_index(map.clone(), Value::String("a".into()), span).unwrap(),
            Value::Integer(1)
        );
        // Absent key -> NULL (no error).
        assert_eq!(
            eval_index(map, Value::String("missing".into()), span).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn eval_index_map_by_non_string_key_is_type_error() {
        let span = zero_span();
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), Value::Integer(1));
        let e = eval_index(Value::Map(m), Value::Integer(0), span).unwrap_err();
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn eval_index_scalar_base_is_type_error() {
        let span = zero_span();
        let e = eval_index(Value::Integer(7), Value::Integer(0), span).unwrap_err();
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn clamp_slice_bound_normalises_and_clamps() {
        assert_eq!(clamp_slice_bound(0, 5), 0);
        assert_eq!(clamp_slice_bound(3, 5), 3);
        assert_eq!(clamp_slice_bound(10, 5), 5); // past the end clamps to len
        assert_eq!(clamp_slice_bound(-1, 5), 4); // negative counts from end
        assert_eq!(clamp_slice_bound(-100, 5), 0); // far negative clamps to 0
    }

    #[test]
    fn eval_slice_basic_inclusive_exclusive() {
        let span = zero_span();
        let v = eval_slice(
            list(&[1, 2, 3, 4, 5]),
            Some(Value::Integer(1)),
            Some(Value::Integer(3)),
            span,
        )
        .unwrap();
        assert_eq!(v, list(&[2, 3]));
    }

    #[test]
    fn eval_slice_open_bounds() {
        let span = zero_span();
        // [..2]
        assert_eq!(
            eval_slice(list(&[1, 2, 3, 4]), None, Some(Value::Integer(2)), span).unwrap(),
            list(&[1, 2])
        );
        // [2..]
        assert_eq!(
            eval_slice(list(&[1, 2, 3, 4]), Some(Value::Integer(2)), None, span).unwrap(),
            list(&[3, 4])
        );
        // [..] is the whole list
        assert_eq!(
            eval_slice(list(&[1, 2, 3, 4]), None, None, span).unwrap(),
            list(&[1, 2, 3, 4])
        );
    }

    #[test]
    fn eval_slice_negative_and_clamped_bounds() {
        let span = zero_span();
        // [-3..-1] -> elements 3,4
        assert_eq!(
            eval_slice(
                list(&[1, 2, 3, 4, 5]),
                Some(Value::Integer(-3)),
                Some(Value::Integer(-1)),
                span
            )
            .unwrap(),
            list(&[3, 4])
        );
        // Out-of-range bounds clamp rather than panic.
        assert_eq!(
            eval_slice(
                list(&[1, 2, 3]),
                Some(Value::Integer(-100)),
                Some(Value::Integer(100)),
                span
            )
            .unwrap(),
            list(&[1, 2, 3])
        );
    }

    #[test]
    fn eval_slice_empty_when_from_ge_to() {
        let span = zero_span();
        assert_eq!(
            eval_slice(
                list(&[1, 2, 3]),
                Some(Value::Integer(2)),
                Some(Value::Integer(2)),
                span
            )
            .unwrap(),
            Value::List(Vec::new())
        );
        assert_eq!(
            eval_slice(
                list(&[1, 2, 3]),
                Some(Value::Integer(3)),
                Some(Value::Integer(1)),
                span
            )
            .unwrap(),
            Value::List(Vec::new())
        );
    }

    #[test]
    fn eval_slice_null_base_or_bound_is_null() {
        let span = zero_span();
        assert_eq!(
            eval_slice(Value::Null, None, None, span).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_slice(list(&[1, 2, 3]), Some(Value::Null), None, span).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_slice(list(&[1, 2, 3]), None, Some(Value::Null), span).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn eval_slice_non_list_base_is_type_error() {
        let span = zero_span();
        let e = eval_slice(Value::Integer(5), None, None, span).unwrap_err();
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn eval_slice_non_integer_bound_is_type_error() {
        let span = zero_span();
        let e = eval_slice(
            list(&[1, 2, 3]),
            Some(Value::String("x".into())),
            None,
            span,
        )
        .unwrap_err();
        assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
    }

    #[test]
    fn index_literal_list_through_return() {
        let db = drevo();
        let res = run("RETURN [10, 20, 30][1] AS x", &db);
        assert_eq!(res.rows[0][0], Value::Integer(20));
    }

    #[test]
    fn index_property_list_with_parameter() {
        let db = drevo();
        run("CREATE (:Doc {title: 'd', tags: ['a', 'b', 'c']})", &db);
        let mut params = HashMap::new();
        params.insert("i".to_string(), Value::Integer(2));
        let res = run_with_params("MATCH (d:Doc) RETURN d.tags[$i] AS t", &db, params);
        assert_eq!(res.rows[0][0], Value::String("c".into()));
    }

    #[test]
    fn slice_property_list_through_return() {
        let db = drevo();
        run(
            "CREATE (:Doc {title: 'd', tags: ['a', 'b', 'c', 'd']})",
            &db,
        );
        let res = run("MATCH (d:Doc) RETURN d.tags[1..3] AS t", &db);
        assert_eq!(
            res.rows[0][0],
            Value::List(vec![Value::String("b".into()), Value::String("c".into())])
        );
    }

    #[test]
    fn index_in_where_filters_rows() {
        let db = drevo();
        run("CREATE (:Doc {title: 'keep', tags: ['x', 'y']})", &db);
        run("CREATE (:Doc {title: 'drop', tags: ['z', 'y']})", &db);
        let res = run(
            "MATCH (d:Doc) WHERE d.tags[0] = 'x' RETURN d.title AS title",
            &db,
        );
        assert_eq!(res.rows, vec![vec![Value::String("keep".into())]]);
    }

    // ---- Named paths (`00141`) -------------------------------------------

    fn single_path(res: &ExecResult) -> Arc<PathValue> {
        assert_eq!(res.rows.len(), 1, "expected one row");
        match &res.rows[0][0] {
            Value::Path(p) => p.clone(),
            other => panic!("expected a Path, got {other:?}"),
        }
    }

    #[test]
    fn named_path_is_a_path_value_with_correct_arity() {
        let db = drevo();
        run("CREATE (:A {title: 'x'})-[:R]->(:B {title: 'y'})", &db);
        let res = run("MATCH p = (:A)-[:R]->(:B) RETURN p", &db);
        let p = single_path(&res);
        assert_eq!(p.nodes.len(), 2);
        assert_eq!(p.relationships.len(), 1);
        assert_eq!(p.length(), 1);
        // The node/relationship endpoints are internally consistent.
        assert_eq!(p.relationships[0].from_id, p.nodes[0].id);
        assert_eq!(p.relationships[0].to_id, p.nodes[1].id);
    }

    #[test]
    fn named_path_type_name_is_path() {
        let db = drevo();
        run("CREATE (:A {title: 'x'})-[:R]->(:B {title: 'y'})", &db);
        let res = run("MATCH p = (:A)-[:R]->(:B) RETURN p", &db);
        assert_eq!(res.rows[0][0].type_name(), "Path");
    }

    #[test]
    fn length_of_named_path_counts_relationships() {
        let db = drevo();
        run(
            "CREATE (a:N {title: 'a'})-[:R]->(b:N {title: 'b'})-[:R]->(c:N {title: 'c'})",
            &db,
        );
        let res = run(
            "MATCH p = (:N)-[:R]->(:N)-[:R]->(:N) RETURN length(p) AS len",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Integer(2));
    }

    #[test]
    fn nodes_and_relationships_functions_return_lists() {
        let db = drevo();
        run("CREATE (a:N {title: 'a'})-[:R]->(b:N {title: 'b'})", &db);
        let res = run(
            "MATCH p = (:N)-[:R]->(:N) RETURN nodes(p) AS ns, relationships(p) AS rs",
            &db,
        );
        match (&res.rows[0][0], &res.rows[0][1]) {
            (Value::List(ns), Value::List(rs)) => {
                assert_eq!(ns.len(), 2);
                assert_eq!(rs.len(), 1);
                assert!(matches!(ns[0], Value::Node(_)));
                assert!(matches!(rs[0], Value::Relationship(_)));
            }
            other => panic!("expected two Lists, got {other:?}"),
        }
    }

    #[test]
    fn create_named_path_binds_and_persists() {
        let db = drevo();
        let res = run(
            "CREATE p = (:Step {title: 'a'})-[:THEN]->(:Step {title: 'b'}) RETURN length(p) AS len",
            &db,
        );
        assert_eq!(res.rows[0][0], Value::Integer(1));
        assert_eq!(res.stats.nodes_created, 2);
        assert_eq!(res.stats.relationships_created, 1);
    }

    #[test]
    fn path_functions_return_null_on_null() {
        let db = drevo();
        let res = run(
            "RETURN nodes(NULL) AS a, relationships(NULL) AS b, length(NULL) AS c",
            &db,
        );
        assert_eq!(res.rows[0], vec![Value::Null, Value::Null, Value::Null]);
    }

    #[test]
    fn nodes_of_scalar_is_invalid_call() {
        let db = drevo();
        let e = err("RETURN nodes(1)", &db);
        assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
    }

    #[test]
    fn two_paths_are_equal_when_same_relationships() {
        // Path equality fixes DISTINCT collapsing of identical paths.
        let a = Value::Path(Arc::new(PathValue {
            nodes: vec![
                Arc::new(NodeValue {
                    id: 1,
                    uuid: [0; 16],
                    labels: vec![],
                    properties: BTreeMap::new(),
                }),
                Arc::new(NodeValue {
                    id: 2,
                    uuid: [0; 16],
                    labels: vec![],
                    properties: BTreeMap::new(),
                }),
            ],
            relationships: vec![Arc::new(RelationshipValue {
                id: 9,
                uuid: [0; 16],
                from_id: 1,
                to_id: 2,
                kind: "R".into(),
                properties: BTreeMap::new(),
            })],
        }));
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ---- list predicate functions (all/any/none/single) -------------------

    #[test]
    fn list_predicate_all_true_when_every_element_satisfies() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN all(x IN [1, 2, 3] WHERE x > 0)", &db),
            Value::Bool(true)
        );
    }

    #[test]
    fn list_predicate_all_false_when_one_element_fails() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN all(x IN [1, 2, -3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_any_true_when_some_element_satisfies() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN any(x IN [-1, -2, 3] WHERE x > 0)", &db),
            Value::Bool(true)
        );
    }

    #[test]
    fn list_predicate_any_false_when_no_element_satisfies() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN any(x IN [-1, -2, -3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_none_true_when_no_element_satisfies() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN none(x IN [-1, -2, -3] WHERE x > 0)", &db),
            Value::Bool(true)
        );
        assert_eq!(
            scalar("RETURN none(x IN [-1, 2, -3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_single_true_only_for_exactly_one_match() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN single(x IN [-1, 2, -3] WHERE x > 0)", &db),
            Value::Bool(true)
        );
        assert_eq!(
            scalar("RETURN single(x IN [-1, 2, 3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
        assert_eq!(
            scalar("RETURN single(x IN [-1, -2, -3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_empty_list_uses_identity_values() {
        let db = drevo();
        assert_eq!(
            scalar("RETURN all(x IN [] WHERE x > 0)", &db),
            Value::Bool(true)
        );
        assert_eq!(
            scalar("RETURN any(x IN [] WHERE x > 0)", &db),
            Value::Bool(false)
        );
        assert_eq!(
            scalar("RETURN none(x IN [] WHERE x > 0)", &db),
            Value::Bool(true)
        );
        assert_eq!(
            scalar("RETURN single(x IN [] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_null_list_propagates_null() {
        let db = drevo();
        for kw in ["all", "any", "none", "single"] {
            assert_eq!(
                scalar(&format!("RETURN {kw}(x IN null WHERE x > 0)"), &db),
                Value::Null,
                "{kw} over a null list should be null"
            );
        }
    }

    #[test]
    fn list_predicate_three_valued_logic_with_null_element() {
        let db = drevo();
        // No false, but an unknown → all is unknown.
        assert_eq!(
            scalar("RETURN all(x IN [1, null, 3] WHERE x > 0)", &db),
            Value::Null
        );
        // No true, but an unknown → any is unknown.
        assert_eq!(
            scalar("RETURN any(x IN [-1, null, -3] WHERE x > 0)", &db),
            Value::Null
        );
        // none is the negation of any.
        assert_eq!(
            scalar("RETURN none(x IN [-1, null, -3] WHERE x > 0)", &db),
            Value::Null
        );
        // A definite false short-circuits all regardless of the unknown.
        assert_eq!(
            scalar("RETURN all(x IN [1, null, -3] WHERE x > 0)", &db),
            Value::Bool(false)
        );
        // A definite true short-circuits any regardless of the unknown.
        assert_eq!(
            scalar("RETURN any(x IN [-1, null, 3] WHERE x > 0)", &db),
            Value::Bool(true)
        );
    }

    #[test]
    fn list_predicate_single_is_null_when_unknown_could_tip_the_count() {
        let db = drevo();
        // Exactly one true so far, but an unknown could make it two.
        assert_eq!(
            scalar("RETURN single(x IN [1, null, -3] WHERE x > 0)", &db),
            Value::Null
        );
        // Two definite trues already → false, the unknown cannot rescue it.
        assert_eq!(
            scalar("RETURN single(x IN [1, 2, null] WHERE x > 0)", &db),
            Value::Bool(false)
        );
    }

    #[test]
    fn list_predicate_non_list_is_type_mismatch() {
        let db = drevo();
        assert!(matches!(
            err("RETURN all(x IN 5 WHERE x > 0)", &db),
            ExecError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn list_predicate_non_boolean_predicate_is_type_mismatch() {
        let db = drevo();
        assert!(matches!(
            err("RETURN any(x IN [1, 2] WHERE x)", &db),
            ExecError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn list_predicate_reads_outer_scope_and_filters_in_where() {
        let db = drevo();
        run("CREATE (:Sprint {name: 'S1', points: [1, 3, 5]})", &db);
        run("CREATE (:Sprint {name: 'S2', points: [2, 4, 8]})", &db);
        // Only sprints all of whose point estimates are odd.
        let res = run(
            "MATCH (s:Sprint) WHERE all(p IN s.points WHERE p % 2 = 1) RETURN s.name AS name",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::String("S1".into()));
    }

    // ---- Map projection (`00149`) ----------------------------------------

    /// The single `Map` value of a one-row, one-column result.
    fn one_map(res: &ExecResult) -> BTreeMap<String, Value> {
        assert_eq!(res.rows.len(), 1, "expected one row");
        assert_eq!(res.rows[0].len(), 1, "expected one column");
        match &res.rows[0][0] {
            Value::Map(m) => m.clone(),
            other => panic!("expected a Map, got {other:?}"),
        }
    }

    #[test]
    fn map_projection_property_selectors_copy_named_properties() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann', age: 30, city: 'NYC'})", &db);
        let m = one_map(&run("MATCH (p:Person) RETURN p {.name, .age} AS m", &db));
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(m.get("age"), Some(&Value::Integer(30)));
        assert!(!m.contains_key("city"));
    }

    #[test]
    fn map_projection_absent_property_is_null() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann'})", &db);
        let m = one_map(&run(
            "MATCH (p:Person) RETURN p {.name, .nickname} AS m",
            &db,
        ));
        assert_eq!(m.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(m.get("nickname"), Some(&Value::Null));
    }

    #[test]
    fn map_projection_all_properties_copies_every_property() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann', age: 30})", &db);
        let m = one_map(&run("MATCH (p:Person) RETURN p {.*} AS m", &db));
        assert_eq!(m.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(m.get("age"), Some(&Value::Integer(30)));
    }

    #[test]
    fn map_projection_literal_entry_is_evaluated_in_scope() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann', age: 30})", &db);
        // A literal entry's expression sees the current row — `p.age * 2`.
        let m = one_map(&run(
            "MATCH (p:Person) RETURN p {.name, doubled: p.age * 2, role: 'admin'} AS m",
            &db,
        ));
        assert_eq!(m.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(m.get("doubled"), Some(&Value::Integer(60)));
        assert_eq!(m.get("role"), Some(&Value::String("admin".into())));
    }

    #[test]
    fn map_projection_variable_selector_is_shorthand() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann'})", &db);
        let m = one_map(&run(
            "MATCH (p:Person) WITH p, 99 AS extra RETURN p {.name, extra} AS m",
            &db,
        ));
        assert_eq!(m.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(m.get("extra"), Some(&Value::Integer(99)));
    }

    #[test]
    fn map_projection_unbound_variable_selector_errors() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann'})", &db);
        assert!(matches!(
            err("MATCH (p:Person) RETURN p {.name, missing} AS m", &db),
            ExecError::UnboundVariable { .. }
        ));
    }

    #[test]
    fn map_projection_later_selector_overwrites_earlier_key() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann'})", &db);
        // `.name` then `name: 'override'` — the literal wins.
        let m = one_map(&run(
            "MATCH (p:Person) RETURN p {.name, name: 'override'} AS m",
            &db,
        ));
        assert_eq!(m.get("name"), Some(&Value::String("override".into())));
    }

    #[test]
    fn map_projection_over_a_map_literal_base() {
        let db = drevo();
        let m = one_map(&run("RETURN {a: 1, b: 2, c: 3} {.a, .c} AS m", &db));
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("a"), Some(&Value::Integer(1)));
        assert_eq!(m.get("c"), Some(&Value::Integer(3)));
    }

    #[test]
    fn map_projection_null_base_propagates_to_null() {
        let db = drevo();
        // An unmatched OPTIONAL MATCH binds `z` to null; projecting yields null.
        let res = run("OPTIONAL MATCH (z:Nope) RETURN z {.name} AS m", &db);
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Null);
    }

    #[test]
    fn map_projection_scalar_base_is_type_mismatch() {
        let db = drevo();
        assert!(matches!(
            err("RETURN 7 {.name} AS m", &db),
            ExecError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn map_projection_all_properties_on_scalar_base_is_type_mismatch() {
        let db = drevo();
        assert!(matches!(
            err("WITH 7 AS n RETURN n {.*} AS m", &db),
            ExecError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn map_projection_empty_selectors_yields_empty_map() {
        let db = drevo();
        run("CREATE (:Person {name: 'Ann'})", &db);
        let m = one_map(&run("MATCH (p:Person) RETURN p {} AS m", &db));
        assert!(m.is_empty());
    }

    #[test]
    fn map_projection_is_a_group_key_alongside_aggregation() {
        let db = drevo();
        run("CREATE (:Item {cat: 'a', n: 1})", &db);
        run("CREATE (:Item {cat: 'a', n: 2})", &db);
        run("CREATE (:Item {cat: 'b', n: 5})", &db);
        // `i {.cat}` is a non-aggregating projection, so it forms the GROUP BY
        // key while `sum(i.n)` aggregates within each group.
        let res = run(
            "MATCH (i:Item) RETURN i {.cat} AS key, sum(i.n) AS total",
            &db,
        );
        assert_eq!(res.rows.len(), 2, "two distinct cat groups");
        // Collect (cat → total) by reaching into the projected key map.
        let mut totals: Vec<(String, i64)> = res
            .rows
            .iter()
            .map(|row| {
                let cat = match &row[0] {
                    Value::Map(m) => match m.get("cat") {
                        Some(Value::String(s)) => s.clone(),
                        other => panic!("expected cat string, got {other:?}"),
                    },
                    other => panic!("expected map key, got {other:?}"),
                };
                let total = match &row[1] {
                    Value::Integer(i) => *i,
                    other => panic!("expected integer total, got {other:?}"),
                };
                (cat, total)
            })
            .collect();
        totals.sort();
        assert_eq!(totals, vec![("a".to_string(), 3), ("b".to_string(), 5)]);
    }

    // ===== Pattern comprehension (task 00150) ==============================

    /// The single `List` value of a one-row, one-column result.
    fn one_list(res: &ExecResult) -> Vec<Value> {
        assert_eq!(res.rows.len(), 1, "expected one row");
        assert_eq!(res.rows[0].len(), 1, "expected one column");
        match &res.rows[0][0] {
            Value::List(items) => items.clone(),
            other => panic!("expected a List, got {other:?}"),
        }
    }

    #[test]
    fn pattern_comprehension_collects_projection_off_each_match() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob'})
             CREATE (a)-[:KNOWS]->(b)",
            &db,
        );
        let list = one_list(&run(
            "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[:KNOWS]->(f) | f.name] AS r",
            &db,
        ));
        assert_eq!(list, vec![Value::String("Bob".into())]);
    }

    #[test]
    fn pattern_comprehension_where_filters_matches() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob', age: 40})
             CREATE (c:Person {name: 'Cal', age: 20})
             CREATE (a)-[:KNOWS]->(b)
             CREATE (a)-[:KNOWS]->(c)",
            &db,
        );
        let list = one_list(&run(
            "MATCH (a:Person {name: 'Ann'})
             RETURN [(a)-[:KNOWS]->(f) WHERE f.age > 30 | f.name] AS r",
            &db,
        ));
        assert_eq!(list, vec![Value::String("Bob".into())]);
    }

    #[test]
    fn pattern_comprehension_no_match_is_empty_list() {
        let db = drevo();
        run("CREATE (:Person {name: 'Loner'})", &db);
        let list = one_list(&run(
            "MATCH (a:Person) RETURN [(a)-[:KNOWS]->(f) | f.name] AS r",
            &db,
        ));
        assert!(list.is_empty());
    }

    #[test]
    fn pattern_comprehension_null_head_is_empty_list() {
        let db = drevo();
        // No match for the OPTIONAL node → `m` is null → comprehension is [].
        let res = run(
            "OPTIONAL MATCH (m:Nope) RETURN [(m)-[:KNOWS]->(f) | f.name] AS r",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::List(vec![]));
    }

    #[test]
    fn pattern_comprehension_non_bool_predicate_is_type_mismatch() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob', age: 40})
             CREATE (a)-[:KNOWS]->(b)",
            &db,
        );
        assert!(matches!(
            err(
                "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[:KNOWS]->(f) WHERE f.age | f.name] AS r",
                &db
            ),
            ExecError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn pattern_comprehension_preserves_duplicate_matches() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob'})
             CREATE (a)-[:KNOWS]->(b)
             CREATE (a)-[:KNOWS]->(b)",
            &db,
        );
        let list = one_list(&run(
            "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[:KNOWS]->(f) | f.name] AS r",
            &db,
        ));
        assert_eq!(
            list,
            vec![Value::String("Bob".into()), Value::String("Bob".into())]
        );
    }

    #[test]
    fn pattern_comprehension_anchors_per_row() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob'})
             CREATE (a)-[:KNOWS]->(b)",
            &db,
        );
        let res = run(
            "MATCH (p:Person)
             RETURN p.name AS who, [(p)-[:KNOWS]->(f) | f.name] AS r
             ORDER BY who",
            &db,
        );
        assert_eq!(res.rows.len(), 2);
        assert_eq!(
            res.rows[0][1],
            Value::List(vec![Value::String("Bob".into())])
        );
        assert_eq!(res.rows[1][1], Value::List(vec![]));
    }

    #[test]
    fn pattern_comprehension_projection_may_be_a_map_projection() {
        let db = drevo();
        run(
            "CREATE (a:Person {name: 'Ann'})
             CREATE (b:Person {name: 'Bob', age: 40})
             CREATE (a)-[:KNOWS]->(b)",
            &db,
        );
        let list = one_list(&run(
            "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[:KNOWS]->(f) | f {.name}] AS r",
            &db,
        ));
        assert_eq!(list.len(), 1);
        match &list[0] {
            Value::Map(m) => assert_eq!(m.get("name"), Some(&Value::String("Bob".into()))),
            other => panic!("expected map element, got {other:?}"),
        }
    }

    // ---- shortestPath / allShortestPaths (00155) ------------------------

    /// A 4-node diamond `a → b → d`, `a → c → d` plus tail `d → e`.
    fn shortest_diamond() -> Drevo {
        let db = drevo();
        run(
            "CREATE (a:N {name:'a'}) CREATE (b:N {name:'b'}) CREATE (c:N {name:'c'})
             CREATE (d:N {name:'d'}) CREATE (e:N {name:'e'})
             CREATE (a)-[:R]->(b) CREATE (b)-[:R]->(d)
             CREATE (a)-[:R]->(c) CREATE (c)-[:R]->(d) CREATE (d)-[:R]->(e)",
            &db,
        );
        db
    }

    #[test]
    fn shortest_path_picks_minimum_length() {
        let db = shortest_diamond();
        let res = run(
            "MATCH (a:N {name:'a'}), (e:N {name:'e'})
             MATCH p = shortestPath((a)-[*]-(e))
             RETURN length(p)",
            &db,
        );
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0][0], Value::Integer(3));
    }

    #[test]
    fn shortest_path_yields_one_row_all_shortest_yields_all_ties() {
        let db = shortest_diamond();
        let single = run(
            "MATCH (a:N {name:'a'}), (d:N {name:'d'})
             MATCH p = shortestPath((a)-[*]-(d)) RETURN length(p)",
            &db,
        );
        assert_eq!(
            single.rows.len(),
            1,
            "shortestPath collapses ties to one row"
        );

        let all = run(
            "MATCH (a:N {name:'a'}), (d:N {name:'d'})
             MATCH p = allShortestPaths((a)-[*]-(d)) RETURN length(p)",
            &db,
        );
        assert_eq!(all.rows.len(), 2, "two equally-short paths a-b-d and a-c-d");
        assert!(all.rows.iter().all(|r| r[0] == Value::Integer(2)));
    }

    #[test]
    fn shortest_path_fixed_length_relationship_is_invalid_function_call() {
        let db = shortest_diamond();
        match err(
            "MATCH (a:N {name:'a'}), (b:N {name:'b'})
             MATCH p = shortestPath((a)-[:R]->(b)) RETURN p",
            &db,
        ) {
            ExecError::InvalidFunctionCall { name, message, .. } => {
                assert_eq!(name, "shortestPath");
                assert!(message.contains("variable-length"), "got: {message}");
            }
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn shortest_path_validate_rejects_multiple_relationships() {
        let db = shortest_diamond();
        match err(
            "MATCH (a:N {name:'a'}), (d:N {name:'d'})
             MATCH p = allShortestPaths((a)-[*]-(x)-[*]-(d)) RETURN p",
            &db,
        ) {
            ExecError::InvalidFunctionCall { name, message, .. } => {
                assert_eq!(name, "allShortestPaths");
                assert!(
                    message.contains("exactly one relationship"),
                    "got: {message}"
                );
            }
            other => panic!("expected InvalidFunctionCall, got {other:?}"),
        }
    }
}
