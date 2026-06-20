//! Cypher abstract syntax tree — Phase 10 task `00062`.
//!
//! The AST is the output of [`crate::cypher::parser::parse`] and the input
//! of the upcoming executor (task `00063`). It is a faithful structural
//! representation of the Cypher source: no semantic checks, no name
//! resolution, no type inference. Every variant carries the source
//! [`crate::cypher::lexer::Span`] of the construct it represents so the
//! executor can produce diagnostics that point at the offending source.
//!
//! The shape mirrors the openCypher grammar to keep the parser simple:
//! `Query` is one or more `SingleQuery` joined by `UNION` / `UNION ALL`;
//! `SingleQuery` is an ordered list of `Clause`s; `Clause` is one of the
//! nine reading / writing / projecting forms; `Expression` is a single
//! recursive enum.

use crate::cypher::lexer::Span;

/// A complete Cypher query.
///
/// One or more [`SingleQuery`] parts joined by `UNION` / `UNION ALL`.
/// The first part never carries a `union` field; subsequent parts always
/// do (the parser enforces this invariant — `parts[i].union` for `i > 0`
/// is the union *kind that joins `parts[i-1]` to `parts[i]`*).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Constituent single-queries.
    pub parts: Vec<UnionPart>,
}

/// One arm of a UNION-joined [`Query`].
#[derive(Debug, Clone, PartialEq)]
pub struct UnionPart {
    /// How this arm is joined to its predecessor. `None` for the first
    /// arm; `Some(_)` otherwise.
    pub union: Option<UnionKind>,
    /// The single query.
    pub query: SingleQuery,
}

/// How two [`SingleQuery`]s are combined by a `UNION` operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnionKind {
    /// `UNION` — duplicates removed.
    Distinct,
    /// `UNION ALL` — duplicates preserved.
    All,
}

/// A single (UNION-free) Cypher query — a non-empty ordered sequence of
/// clauses.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleQuery {
    /// Clauses in source order.
    pub clauses: Vec<Clause>,
}

/// One Cypher clause.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// `[OPTIONAL] MATCH pattern [, pattern …] [WHERE expr]`.
    Match(MatchClause),
    /// `CREATE pattern [, pattern …]`.
    Create(CreateClause),
    /// `MERGE pattern [ON CREATE SET …] [ON MATCH SET …]`.
    Merge(MergeClause),
    /// `[DETACH] DELETE expr [, expr …]`.
    Delete(DeleteClause),
    /// `SET item [, item …]`.
    Set(SetClause),
    /// `REMOVE item [, item …]`.
    Remove(RemoveClause),
    /// `WITH projections [DISTINCT] [ORDER BY …] [SKIP …] [LIMIT …] [WHERE …]`.
    With(WithClause),
    /// `RETURN [DISTINCT] projections [ORDER BY …] [SKIP …] [LIMIT …]`.
    Return(ReturnClause),
    /// `UNWIND expr AS name`.
    Unwind(UnwindClause),
    /// `FOREACH (var IN list | update_clause …)`.
    Foreach(ForeachClause),
    /// `CALL proc.name(args) [YIELD col [AS alias] … [WHERE pred]]`.
    Call(CallClause),
}

/// `MATCH` / `OPTIONAL MATCH` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    /// `true` when the source read `OPTIONAL MATCH`.
    pub optional: bool,
    /// Patterns to match, separated by `,` in source.
    pub patterns: Vec<NamedPattern>,
    /// Optional `WHERE` predicate attached to this `MATCH`.
    pub where_clause: Option<Expression>,
    /// Source span of the `MATCH` keyword (or `OPTIONAL` if present).
    pub span: Span,
}

/// `CREATE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    /// Patterns to create.
    pub patterns: Vec<NamedPattern>,
    /// Source span of the `CREATE` keyword.
    pub span: Span,
}

/// `MERGE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    /// Pattern to merge (Cypher restricts MERGE to a single pattern).
    pub pattern: NamedPattern,
    /// `ON CREATE SET …` actions, in declaration order.
    pub on_create: Vec<SetItem>,
    /// `ON MATCH SET …` actions, in declaration order.
    pub on_match: Vec<SetItem>,
    /// Source span of the `MERGE` keyword.
    pub span: Span,
}

/// `DELETE` / `DETACH DELETE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    /// `true` when the source read `DETACH DELETE`.
    pub detach: bool,
    /// Targets — usually variables, but the spec allows arbitrary
    /// expressions evaluating to a node or relationship.
    pub targets: Vec<Expression>,
    /// Source span of the keyword that introduced the clause (`DELETE`
    /// or `DETACH`).
    pub span: Span,
}

/// `SET` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    /// `SET` items in declaration order.
    pub items: Vec<SetItem>,
    /// Source span of the `SET` keyword.
    pub span: Span,
}

/// One item in a `SET` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `target.prop = value` — set a single property.
    Property {
        /// Property access expression on the left of `=`.
        target: Expression,
        /// Value to assign.
        value: Expression,
    },
    /// `target = map` — replace the entire property map.
    Replace {
        /// Variable being replaced.
        target: Expression,
        /// New properties.
        value: Expression,
    },
    /// `target += map` — merge new properties over existing ones.
    Merge {
        /// Variable being merged into.
        target: Expression,
        /// Properties to merge in.
        value: Expression,
    },
    /// `target :Label1:Label2` — add labels to a node.
    Labels {
        /// Variable to label.
        target: Expression,
        /// Label names to add.
        labels: Vec<String>,
    },
}

/// `REMOVE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    /// `REMOVE` items in declaration order.
    pub items: Vec<RemoveItem>,
    /// Source span of the `REMOVE` keyword.
    pub span: Span,
}

/// One item in a `REMOVE` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `target.prop` — remove a property.
    Property(Expression),
    /// `target :Label1:Label2` — remove labels.
    Labels {
        /// Variable to unlabel.
        target: Expression,
        /// Label names to remove.
        labels: Vec<String>,
    },
}

/// `WITH` clause — query pipelining boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    /// `true` when `WITH DISTINCT` was written.
    pub distinct: bool,
    /// Projection items.
    pub items: Vec<ProjectionItem>,
    /// `ORDER BY` entries (empty if absent).
    pub order_by: Vec<OrderItem>,
    /// `SKIP` expression, if present.
    pub skip: Option<Expression>,
    /// `LIMIT` expression, if present.
    pub limit: Option<Expression>,
    /// Trailing `WHERE` predicate, if present.
    pub where_clause: Option<Expression>,
    /// Source span of the `WITH` keyword.
    pub span: Span,
}

/// `RETURN` clause — terminal projection.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    /// `true` when `RETURN DISTINCT` was written.
    pub distinct: bool,
    /// Projection items.
    pub items: Vec<ProjectionItem>,
    /// `ORDER BY` entries (empty if absent).
    pub order_by: Vec<OrderItem>,
    /// `SKIP` expression, if present.
    pub skip: Option<Expression>,
    /// `LIMIT` expression, if present.
    pub limit: Option<Expression>,
    /// Source span of the `RETURN` keyword.
    pub span: Span,
}

/// One projection in a `WITH` / `RETURN` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionItem {
    /// `*` — project all bound variables.
    Star,
    /// `expr [AS alias]` — project a single expression with an optional
    /// alias.
    Expression {
        /// Expression to project.
        expr: Expression,
        /// Optional alias name introduced with `AS`.
        alias: Option<String>,
    },
}

/// One `ORDER BY` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// Expression to order by.
    pub expression: Expression,
    /// Ordering direction (defaults to [`OrderDirection::Asc`] in source).
    pub direction: OrderDirection,
}

/// Sort direction for an `ORDER BY` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    /// Ascending order (the source-level default).
    Asc,
    /// Descending order.
    Desc,
}

/// `UNWIND expression AS variable`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    /// List expression to unwind.
    pub expression: Expression,
    /// Variable bound to each element.
    pub alias: String,
    /// Source span of the `UNWIND` keyword.
    pub span: Span,
}

/// `FOREACH (variable IN list | update_clause …)`.
///
/// An update clause that iterates `list`, binding `variable` to each
/// element in turn and running the nested update `clauses` for that
/// element. Cypher restricts the body to update clauses (`CREATE`,
/// `MERGE`, `SET`, `REMOVE`, `DELETE`, and nested `FOREACH`) — read
/// clauses such as `MATCH` / `RETURN` / `WITH` are not allowed inside.
/// `FOREACH` never changes the outer query's cardinality; the loop
/// variable is scoped to the body and is not visible afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachClause {
    /// Loop variable bound to each list element.
    pub variable: String,
    /// List expression iterated over.
    pub list: Expression,
    /// Update clauses run once per list element, in source order.
    pub clauses: Vec<Clause>,
    /// Source span of the `FOREACH` keyword.
    pub span: Span,
}

/// `CALL proc.name(args) [YIELD …]` — invoke a built-in procedure.
///
/// drevo ships a small set of read-only schema-introspection procedures
/// (`db.labels`, `db.relationshipTypes`, `db.propertyKeys`). A procedure
/// emits a fixed set of *output columns*; `YIELD` selects which of those
/// columns to bind into the row stream (optionally renamed with `AS` and
/// filtered with a trailing `WHERE`). A standalone `CALL` with no `YIELD`
/// projects every output column directly as the query result.
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// Dotted procedure name as segments (`["db", "labels"]`).
    pub name: Vec<String>,
    /// Argument expressions inside the parentheses (empty for the
    /// built-in introspection procedures).
    pub args: Vec<Expression>,
    /// `YIELD` items, or `None` when the source omitted `YIELD`
    /// (a standalone call projecting all output columns).
    pub yields: Option<Vec<YieldItem>>,
    /// Optional `WHERE` predicate after `YIELD` (only valid with
    /// `yields = Some(_)`; the parser enforces this).
    pub where_clause: Option<Expression>,
    /// Source span of the `CALL` keyword.
    pub span: Span,
}

/// One item in a `CALL … YIELD` list: `col [AS alias]`.
#[derive(Debug, Clone, PartialEq)]
pub struct YieldItem {
    /// Output column name produced by the procedure.
    pub name: String,
    /// Optional rename (`YIELD col AS alias`); the bound variable is
    /// `alias` when present, otherwise `name`.
    pub alias: Option<String>,
    /// Source span of the yielded column name.
    pub span: Span,
}

/// A pattern with an optional path-binding variable.
///
/// Source form: `[variable =] (a)-[r]->(b)-…`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedPattern {
    /// Optional path variable (`p = (a)-->(b)`).
    pub variable: Option<String>,
    /// The path itself.
    pub path: PathPattern,
}

/// A path pattern — head node plus zero or more `(rel, node)` legs.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// Leading node pattern.
    pub head: NodePattern,
    /// Subsequent relationship + node segments.
    pub tail: Vec<PathSegment>,
}

/// One leg of a path pattern: a relationship followed by a node.
#[derive(Debug, Clone, PartialEq)]
pub struct PathSegment {
    /// Relationship pattern joining the previous node to `node`.
    pub relationship: RelationshipPattern,
    /// Destination node pattern.
    pub node: NodePattern,
}

/// A node pattern — `(variable :Label1:Label2 {props})`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    /// Optional variable bound to the node.
    pub variable: Option<String>,
    /// Required labels (any-of for MATCH, all-of for CREATE).
    pub labels: Vec<String>,
    /// Optional inline property map literal.
    pub properties: Option<MapLiteral>,
    /// Span of the leading `(`.
    pub span: Span,
}

/// A relationship pattern — `-[variable :Type1|Type2 *1..3 {props}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    /// Direction encoded by the arrows.
    pub direction: Direction,
    /// Optional variable bound to the relationship.
    pub variable: Option<String>,
    /// Allowed relationship types (any-of for MATCH; CREATE / MERGE
    /// require exactly one and the parser does NOT enforce that — the
    /// executor does).
    pub types: Vec<String>,
    /// Variable-length-path range, if `*` appears inside the brackets.
    pub length: Option<RelLength>,
    /// Optional inline property map literal.
    pub properties: Option<MapLiteral>,
    /// Span of the leading `-` or `<-`.
    pub span: Span,
}

/// Direction of a [`RelationshipPattern`] as written in source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `-[…]->` — left-to-right.
    Outgoing,
    /// `<-[…]-` — right-to-left.
    Incoming,
    /// `-[…]-` — either direction.
    Undirected,
}

/// Variable-length-path range as written inside `[*…]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelLength {
    /// `[*]` — one or more hops (the executor decides the upper bound).
    Any,
    /// `[*N]` — exactly `N` hops.
    Exact(i64),
    /// `[*N..M]` / `[*..M]` / `[*N..]` — bounded range.
    Range {
        /// Inclusive lower bound (`None` = unbounded below, defaults to 1
        /// for the executor).
        from: Option<i64>,
        /// Inclusive upper bound (`None` = unbounded above).
        to: Option<i64>,
    },
}

/// A literal `{ key: expr, … }` map.
#[derive(Debug, Clone, PartialEq)]
pub struct MapLiteral {
    /// Entries in source order.
    pub entries: Vec<(String, Expression)>,
    /// Span of the leading `{`.
    pub span: Span,
}

/// An expression — the recursive heart of the AST.
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    /// Integer literal.
    Integer(i64, Span),
    /// Floating-point literal.
    Float(f64, Span),
    /// String literal.
    String(String, Span),
    /// `TRUE`.
    True(Span),
    /// `FALSE`.
    False(Span),
    /// `NULL`.
    Null(Span),
    /// Bare identifier — a variable reference.
    Variable(String, Span),
    /// `$name` or `$0` query parameter.
    Parameter(String, Span),
    /// `expr.name` property access.
    Property {
        /// Base expression.
        base: Box<Expression>,
        /// Property name. May be a soft keyword (Cypher allows keywords
        /// after `.`).
        name: String,
        /// Span of the property identifier.
        span: Span,
    },
    /// `[a, b, c]` list literal.
    List {
        /// Element expressions.
        items: Vec<Expression>,
        /// Span of the leading `[`.
        span: Span,
    },
    /// `{k: v, …}` map literal.
    Map(MapLiteral),
    /// `name(arg, …)` or `name(DISTINCT arg)` function / aggregation call.
    FunctionCall {
        /// Function name (possibly dotted: `apoc.coll.sum`).
        name: Vec<String>,
        /// `true` for aggregations written as `count(DISTINCT x)`.
        distinct: bool,
        /// Arguments in source order.
        args: Vec<Expression>,
        /// Span of the function name.
        span: Span,
    },
    /// `-expr` / `NOT expr` / `+expr`.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expression>,
        /// Span of the operator.
        span: Span,
    },
    /// `lhs op rhs`.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expression>,
        /// Right operand.
        rhs: Box<Expression>,
        /// Span of the operator.
        span: Span,
    },
    /// `expr IS NULL` / `expr IS NOT NULL`.
    IsNull {
        /// Operand.
        expr: Box<Expression>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
        /// Span of the `IS` keyword.
        span: Span,
    },
    /// `expr IN list_expr`.
    In {
        /// Element to test.
        expr: Box<Expression>,
        /// List to look in.
        list: Box<Expression>,
        /// Span of the `IN` keyword.
        span: Span,
    },
    /// `expr[index]` list / map element access.
    Index {
        /// Base expression.
        base: Box<Expression>,
        /// Index expression.
        index: Box<Expression>,
        /// Span of the `[`.
        span: Span,
    },
    /// `expr[from..to]` list slice.
    Slice {
        /// Base list.
        base: Box<Expression>,
        /// Lower bound (inclusive). `None` for unbounded below.
        from: Option<Box<Expression>>,
        /// Upper bound (exclusive). `None` for unbounded above.
        to: Option<Box<Expression>>,
        /// Span of the `[`.
        span: Span,
    },
    /// `CASE [scrutinee] WHEN cond THEN val [WHEN …] [ELSE val] END`.
    Case {
        /// Optional scrutinee (`CASE x WHEN 1 THEN …`). `None` for the
        /// generic form (`CASE WHEN cond THEN …`).
        scrutinee: Option<Box<Expression>>,
        /// `(when, then)` pairs in declaration order.
        arms: Vec<(Expression, Expression)>,
        /// Optional `ELSE` branch.
        else_branch: Option<Box<Expression>>,
        /// Span of the `CASE` keyword.
        span: Span,
    },
    /// `[var IN list WHERE predicate | projection]` — a list comprehension.
    ///
    /// Evaluates `list`, binds each element to `var` in a child scope, keeps
    /// the elements for which `predicate` holds (when present), and collects
    /// `projection` of each survivor (or the element itself when there is no
    /// `| projection`). At least one of `predicate` / `projection` is always
    /// present — without either the bracket form is an ordinary list literal.
    ListComprehension {
        /// The loop variable bound to each list element in turn.
        variable: String,
        /// The source list expression.
        list: Box<Expression>,
        /// Optional `WHERE` filter, evaluated in the child scope.
        predicate: Option<Box<Expression>>,
        /// Optional `| projection`, evaluated in the child scope. `None`
        /// keeps the element unchanged (filter-only comprehension).
        projection: Option<Box<Expression>>,
        /// Span of the leading `[`.
        span: Span,
    },
    /// `kind(var IN list WHERE predicate)` — a list predicate function
    /// (`all` / `any` / `none` / `single`).
    ///
    /// Evaluates `list`, binds each element to `var` in a child scope, and
    /// folds the three-valued `predicate` result across the elements per the
    /// [`ListPredicateKind`] quantifier. The `WHERE predicate` is mandatory
    /// (unlike a list comprehension's optional filter).
    ListPredicate {
        /// Which quantifier — `all` / `any` / `none` / `single`.
        kind: ListPredicateKind,
        /// The loop variable bound to each list element in turn.
        variable: String,
        /// The source list expression.
        list: Box<Expression>,
        /// The `WHERE` predicate, evaluated in the child scope.
        predicate: Box<Expression>,
        /// Span of the function-name keyword.
        span: Span,
    },
    /// `count(*)` — the only context in which `*` is a valid sub-expression.
    Star(Span),
}

/// The quantifier of a [`Expression::ListPredicate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPredicateKind {
    /// `all(x IN list WHERE pred)` — `true` iff `pred` holds for every element.
    All,
    /// `any(x IN list WHERE pred)` — `true` iff `pred` holds for some element.
    Any,
    /// `none(x IN list WHERE pred)` — `true` iff `pred` holds for no element.
    None,
    /// `single(x IN list WHERE pred)` — `true` iff `pred` holds for exactly one.
    Single,
}

impl ListPredicateKind {
    /// The lower-case Cypher keyword for this quantifier.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
            Self::None => "none",
            Self::Single => "single",
        }
    }
}

impl Expression {
    /// Return the source [`Span`] of this expression.
    pub fn span(&self) -> Span {
        match self {
            Self::Integer(_, s)
            | Self::Float(_, s)
            | Self::String(_, s)
            | Self::True(s)
            | Self::False(s)
            | Self::Null(s)
            | Self::Variable(_, s)
            | Self::Parameter(_, s)
            | Self::Star(s) => *s,
            Self::Property { span, .. }
            | Self::List { span, .. }
            | Self::FunctionCall { span, .. }
            | Self::Unary { span, .. }
            | Self::Binary { span, .. }
            | Self::IsNull { span, .. }
            | Self::In { span, .. }
            | Self::Index { span, .. }
            | Self::Slice { span, .. }
            | Self::Case { span, .. }
            | Self::ListComprehension { span, .. }
            | Self::ListPredicate { span, .. } => *span,
            Self::Map(m) => m.span,
        }
    }
}

/// Unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-expr` — arithmetic negation.
    Neg,
    /// `+expr` — arithmetic identity. Kept distinct from `Neg` for
    /// faithful round-tripping of source.
    Plus,
    /// `NOT expr`.
    Not,
}

/// Binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Mod,
    /// `^`.
    Pow,
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `=~` — regex match.
    RegexMatch,
    /// `AND`.
    And,
    /// `OR`.
    Or,
    /// `XOR`.
    Xor,
    /// `STARTS WITH`.
    StartsWith,
    /// `ENDS WITH`.
    EndsWith,
    /// `CONTAINS`.
    Contains,
}
