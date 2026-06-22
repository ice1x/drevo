# Cypher Reference

drevo speaks a **subset of [Cypher](https://opencypher.org/)**, the openCypher query
language popularised by Neo4j. The subset is implemented bottom-up by a hand-written
lexer ([`src/cypher/lexer.rs`](../src/cypher/lexer.rs)), a recursive-descent + Pratt
parser ([`src/cypher/parser.rs`](../src/cypher/parser.rs)), and a tree-walking executor
([`src/cypher/executor.rs`](../src/cypher/executor.rs)) that runs against a live
[`Drevo`](../src/db.rs) handle.

> **Every fenced `cypher` block in this document is executed as a test.**
> [`tests/docs_examples.rs`](../tests/docs_examples.rs) extracts each block, parses it,
> and runs it through the executor on a fresh in-memory database, asserting it neither
> fails to parse nor returns an executor error. The reference therefore cannot drift
> from the implementation: if drevo stops supporting a construct shown here, CI goes red.

This is a *living subset* — clauses that are not yet implemented return a deterministic
[`ExecError::Unsupported`](../src/cypher/executor.rs) error naming the construct, never a
panic or a wrong answer. See [Not yet supported](#not-yet-supported) for the current edge.

---

## Running a query

A query is `parse`d into an AST and then `execute`d with a parameter map:

```rust
use std::collections::HashMap;
use drevo::cypher::{parser::parse, executor::execute};
use drevo::db::Drevo;

let db = Drevo::open_in_memory()?;
let query = parse("MATCH (n:Person) RETURN n.title AS name LIMIT 10")?;
let result = execute(&query, &db, HashMap::new())?;
for row in &result.rows {
    println!("{:?}", row);
}
```

Over the wire the same queries arrive through the [Bolt protocol](sdk-reference.md#bolt-protocol)
(`RUN` / `PULL`) or — for read paths — the [HTTP API](sdk-reference.md#http-api).

---

## Reading data

### MATCH

`MATCH` finds patterns. A node pattern is `(variable:Label {property: value})`; every part
is optional. Inline properties act as equality filters.

```cypher
MATCH (t:Task)
RETURN t.title AS task, t.priority AS priority
```

Relationships are written `-[:TYPE]->` (outgoing), `<-[:TYPE]-` (incoming) or `-[:TYPE]-`
(either direction). A variable and inline properties are allowed: `-[r:KNOWS {since: 2020}]->`.

```cypher
MATCH (person:Person)-[:ASSIGNED_TO]->(task:Task {status: 'pending'})
RETURN person.title AS person, task.title AS task
```

A label-free pattern matches nodes of any kind, and multiple labels match a node carrying
*any* of them:

```cypher
MATCH (n:Person:Developer)
WHERE n.active = true OR n.role = 'lead'
RETURN n.title AS name
```

### Anonymous nodes

Any node in a pattern may omit its variable — including the **head** (first) node and
intermediate nodes in a multi-hop path (task `00143`). An anonymous node still matches and
filters by its label and inline properties; it just binds nothing for later use. This is the
idiomatic way to say "from *any* such node, reach…":

```cypher
MATCH (:Task)-[:ASSIGNED_TO]->(person:Person)
RETURN DISTINCT person.title AS assignee
```

```cypher
MATCH (:Book {title: 'Dune'})-[:HAS_CHAPTER]->()-[:NEXT]->(c:Chapter)
RETURN c.title AS second_chapter
```

A bare `()` head with no label or properties matches every node, so `MATCH ()-[:KNOWS]->(b)`
returns the target of every `:KNOWS` relationship.

### OPTIONAL MATCH

`OPTIONAL MATCH` is a left-outer join: when the pattern does not match, its variables bind
to `NULL` instead of dropping the row.

```cypher
MATCH (issue:Issue)
OPTIONAL MATCH (issue)-[:ASSIGNED_TO]->(dev:Developer)
RETURN issue.title AS issue, dev.title AS assignee
```

### Variable-length paths

`[:TYPE*MIN..MAX]` expands a relationship between 1 and many hops using breadth-first search
with **trail uniqueness** (no edge is traversed twice in a single path). The bounds are
optional: `[*]`, `[*2]`, `[*1..3]`, `[*..4]`, `[*2..]` are all valid. Unbounded upper limits
are capped for safety.

```cypher
MATCH (start:Chapter)-[:FLOWS_TO*1..3]->(later:Chapter)
RETURN start.title AS from_chapter, later.title AS to_chapter
```

### Named paths

Prefixing a pattern with `variable =` binds the whole pattern to a **path** value — an
alternating sequence of nodes and relationships in traversal order (task `00141`). The path
captures **every** endpoint it traverses, including anonymous intermediate nodes that carry
no variable of their own. Named paths work for both fixed- and variable-length patterns, and
for `CREATE` and `MERGE`.

Three functions consume a path:

- `length(p)` — the number of relationships (hops).
- `nodes(p)` — the nodes as a list, in path order (`length(p) + 1` of them).
- `relationships(p)` — the relationships as a list, in path order.

```cypher
MATCH p = (a:Task)-[:DEPENDS_ON*1..3]->(b:Task)
RETURN length(p) AS hops, size(nodes(p)) AS visited
ORDER BY hops
```

```cypher
CREATE p = (:Step {title: 'draft'})-[:THEN]->(:Step {title: 'review'})
RETURN length(p) AS steps
```

---

## Filtering — WHERE

`WHERE` attaches a predicate to `MATCH`, `OPTIONAL MATCH`, or `WITH`.

**Comparisons:** `=`, `<>`, `<`, `<=`, `>`, `>=`. Comparison with `NULL` yields `NULL`
(three-valued logic).

```cypher
MATCH (t:Task)
WHERE t.priority >= 3 AND t.status <> 'done'
RETURN t.title AS task, t.priority AS priority
ORDER BY priority DESC
LIMIT 10
```

**Boolean operators:** `AND`, `OR`, `XOR`, `NOT`, with three-valued truth tables.

**String predicates:** `STARTS WITH`, `ENDS WITH`, `CONTAINS` (all case-sensitive).

```cypher
MATCH (n:Article)
WHERE n.title STARTS WITH 'Intro' AND n.body CONTAINS 'graph'
RETURN n.title AS title
```

**Regex match:** `=~` tests a string against a regular expression. Like Neo4j (Java
`Matcher::matches`), the pattern must match the **entire** string — anchor-free patterns are
implicitly anchored at both ends, so `'hello world' =~ 'hello'` is `false` (use `'hello.*'`).
`NULL` on either side yields `NULL`; a non-string operand is a `TypeMismatch`; an invalid
pattern is an `InvalidRegex` error.

```cypher
MATCH (b:Bug)
WHERE b.reporter =~ '[\\w.]+@[\\w.]+'        // looks like an email address
RETURN b.title AS bug
```

The engine supports the common Java/Neo4j subset: literals, `.`, the quantifiers
`* + ? {n} {n,} {n,m}` (greedy, or lazy with a trailing `?`), character classes `[...]`
with ranges and negation, the shortcuts `\d \D \w \W \s \S`, anchors `^ $`, alternation
`|`, grouping `(...)` / non-capturing `(?:...)`, and the inline case-insensitive flag
`(?i)` (applied to the whole pattern).

```cypher
MATCH (t:Thought)
WHERE t.text =~ '(?i).*should.*'             // case-insensitive "should" statements
RETURN t.text AS thought
```

**List membership and null tests:** `IN`, `IS NULL`, `IS NOT NULL`.

```cypher
MATCH (b:Bug)
WHERE b.severity IN ['high', 'critical'] AND b.assignee IS NOT NULL
RETURN b.title AS bug, b.severity AS severity
```

---

## Projecting — RETURN and WITH

### RETURN

`RETURN` shapes the output. It supports `AS` aliases, `DISTINCT`, `ORDER BY … [ASC|DESC]`,
`SKIP`, and `LIMIT`.

```cypher
MATCH (a:Author)-[:WROTE]->(post:Post)
RETURN DISTINCT a.title AS author
ORDER BY author ASC
SKIP 0
LIMIT 25
```

### WITH

`WITH` is `RETURN` mid-query: it projects intermediate results and pipes them into the next
clause. Crucially, a `WHERE` after `WITH` filters *after* aggregation, which is how you
express "groups having …":

```cypher
MATCH (dept:Department)<-[:MEMBER_OF]-(e:Employee)
WITH dept, count(e) AS headcount
WHERE headcount > 5
RETURN dept.title AS department, headcount
ORDER BY headcount DESC
```

### UNWIND

`UNWIND` expands a list into one row per element, binding each element to a new variable.
Every existing row is multiplied by the list, so it composes with `MATCH`, `WITH`,
aggregation, and `CREATE`:

```cypher
UNWIND [1, 2, 3] AS x
RETURN x
```

An empty list — and `null` — expand to **zero** rows (so a heterogeneous scan does not
abort). Combined with `keywords()`, `UNWIND` powers "group by extracted keyword" faceting:

```cypher
UNWIND ['alice', 'bob', 'carol'] AS name
CREATE (:Person {name: name})
```

### UNION

`UNION` combines the result rows of two or more queries into one result set. `UNION ALL`
concatenates every arm's rows in order, keeping duplicates; plain `UNION` additionally
removes duplicate rows across the combined set:

```cypher
RETURN 'open' AS state
UNION ALL
RETURN 'closed' AS state
```

Every arm must project the **same column names in the same order**, and a single query may
not mix `UNION` and `UNION ALL` — either constraint surfaces as `ExecError::UnionMismatch`.
A common use is gathering rows of the same shape from different labels:

```cypher
MATCH (p:Person)
RETURN p.title AS name
UNION
MATCH (c:Company)
RETURN c.title AS name
```

---

## Aggregation

The aggregating functions are `count`, `sum`, `avg`, `min`, `max`, and `collect`. Any
non-aggregated projection item becomes an implicit `GROUP BY` key. `count(*)` counts rows;
`count(expr)` skips `NULL`s.

```cypher
MATCH (p:Person)-[:ASSIGNED_TO]->(t:Task)
RETURN p.title AS person, count(*) AS task_count
ORDER BY task_count DESC
```

`collect` gathers values into a list; the numeric aggregates fold a group to a scalar:

```cypher
MATCH (o:Order)
RETURN count(*) AS orders,
       sum(o.total) AS revenue,
       avg(o.total) AS avg_order,
       max(o.total) AS biggest
```

```cypher
MATCH (a:Author)-[:WROTE]->(post:Post)
RETURN a.title AS author, collect(post.title) AS posts
```

`count(DISTINCT …)` deduplicates before counting:

```cypher
MATCH (e:Employee)
RETURN count(DISTINCT e.team) AS distinct_teams
```

---

## Conditional expressions — CASE

`CASE` returns one of several values depending on a condition. It is an *expression*, so it
may appear anywhere a value is expected: in `RETURN` / `WITH` projections, in `WHERE`, or
nested inside another expression. There are two forms.

The **generic** form evaluates each boolean `WHEN` condition in order and returns the `THEN`
value of the first that is `true`. A `NULL` or `false` condition is skipped:

```cypher
MATCH (o:Order)
RETURN o.title AS po,
       CASE WHEN o.total >= 10000 THEN 'gold'
            WHEN o.total >= 1000 THEN 'silver'
            ELSE 'standard' END AS tier
```

The **simple** form compares a scrutinee against each `WHEN` value for equality and returns
the matching `THEN`. Because `NULL = NULL` is `NULL` (not `true`), a `NULL` scrutinee never
matches and falls through to `ELSE`:

```cypher
MATCH (t:Task)
RETURN t.title AS task,
       CASE t.priority WHEN 'P1' THEN 1
                       WHEN 'P2' THEN 8
                       ELSE 72 END AS sla_hours
```

When no arm matches and there is no `ELSE`, the result is `NULL`. A generic-form condition
that is neither boolean nor `NULL` raises `ExecError::TypeMismatch`.

A `CASE` arm may contain an **aggregation** (`count`, `sum`, `avg`, `min`, `max`, `collect`).
The aggregation folds over the current group, exactly like a bare aggregating column, so the
whole `CASE` column is an aggregating projection (the other, non-aggregating projection items
form the group key):

```cypher
MATCH (t:Task)
RETURN t.status AS status,
       CASE WHEN count(*) > 5 THEN 'overloaded'
            ELSE 'normal' END AS load
```

Here `count(*)` is folded per `status` group and the `CASE` chooses a label from the
aggregated count. An aggregation may appear in the scrutinee, any `WHEN`, any `THEN`, or the
`ELSE`. As everywhere else in Cypher, an aggregation nested *directly inside another*
aggregation (e.g. `sum(CASE WHEN … THEN count(*) END)`) is rejected.

---

## Writing data

### CREATE

`CREATE` makes nodes and relationships. Inline properties are assigned verbatim; for
relationships the direction is required.

```cypher
CREATE (t:Thought {title: 'Morning entry', body: 'I felt anxious before the standup', valence: -0.4})
RETURN t.title AS title, t.valence AS valence
```

```cypher
CREATE (e:Entry {title: 'Session 1'})-[:HAS_DISTORTION]->(d:Distortion {kind: 'catastrophizing'})
RETURN e.title AS entry, d.kind AS distortion
```

### SET and REMOVE

`SET` assigns a property (`n.p = v`), replaces the whole property map (`n = {…}`), merges
into it (`n += {…}`), or adds labels (`n:Label`). `REMOVE` deletes a property or a label.

```cypher
MATCH (r:Record {status: 'draft'})
SET r.status = 'approved', r:Audited
RETURN r.title AS record
```

```cypher
MATCH (p:Product {sku: 'X-1'})
SET p += {price: 19.99, in_stock: true}
RETURN p.sku AS sku
```

```cypher
MATCH (n:Draft)
REMOVE n.tmp_flag, n:Stale
RETURN n.title AS title
```

### DELETE and DETACH DELETE

`DELETE` removes a node or relationship. A node with relationships must be removed with
`DETACH DELETE`, which cascades to its incident edges.

```cypher
MATCH (obsolete:Ticket {status: 'closed'})
DETACH DELETE obsolete
```

### MERGE

`MERGE` is match-or-create: it finds the pattern or creates it, and runs `ON CREATE SET` /
`ON MATCH SET` actions depending on which branch fired.

```cypher
MERGE (s:Stage {name: 'Awareness'})
ON CREATE SET s.created = true
ON MATCH SET s.touched = true
RETURN s.name AS stage
```

### FOREACH

`FOREACH (var IN list | …)` runs one or more update clauses once per element of `list`,
binding each element to `var`. It is a bulk-update clause: it never changes the outer
query's cardinality, and `var` is scoped to the body — it is not visible afterwards. The
body is restricted to update clauses (`CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, and
nested `FOREACH`); read clauses such as `MATCH` are not permitted inside.

```cypher
CREATE (p:Project {title: 'Launch'})
FOREACH (name IN ['design', 'build', 'ship'] |
  CREATE (p)-[:HAS_SUBTASK]->(:Task {title: name}))
```

A common idiom collects matched nodes with `WITH … collect(…)` and updates each in one
pass. A `null` list iterates zero times (mirroring `UNWIND null`); any other non-list value
is a type error.

```cypher
MATCH (t:Task)
WITH collect(t) AS tasks
FOREACH (n IN tasks | SET n.status = 'done')
```

---

## Procedures

drevo ships a small set of read-only, built-in procedures invoked with the `CALL` clause.
These mirror Neo4j's schema-introspection procedures so existing tooling and drivers can
discover a graph's shape. There is no support for user-defined, `apoc.*`, or `gds.*`
procedures.

| Procedure | Output column | Returns |
|-----------|---------------|---------|
| `db.labels()` | `label` | every distinct node label (the primary kind plus any secondary `:Extra` labels), sorted |
| `db.relationshipTypes()` | `relationshipType` | every distinct relationship type, sorted |
| `db.propertyKeys()` | `propertyKey` | every distinct property key across nodes and relationships, sorted (the reserved `_labels` key is never exposed) |

### CALL

A standalone `CALL` projects the procedure's output column(s) directly as the query result:

```cypher
CALL db.labels()
```

`YIELD` brings the named output columns into scope for the rest of the query, optionally
renamed with `AS` and filtered with a trailing `WHERE`:

```cypher
CALL db.labels() YIELD label
WHERE label <> 'Internal'
RETURN label
```

Because `YIELD` produces ordinary rows, downstream clauses — including aggregation — work as
usual:

```cypher
CALL db.propertyKeys() YIELD propertyKey AS key
RETURN count(key) AS distinct_keys
```

A `CALL` to an unknown procedure, with the wrong number of arguments, or a `YIELD` of a column
the procedure does not produce, fails with `ExecError::InvalidProcedureCall` — a deterministic,
named error rather than a panic or a wrong answer.

---

## Parameters

Parameters are written `$name` (or `$0`, `$1`, …) and supplied as a `HashMap<String, Value>`
to `execute`. They keep query text constant and untrusted values out of the query string.

```cypher
MATCH (p:Person {name: $name})
RETURN p.title AS person
LIMIT $limit
```

---

## Scalar functions

The executor ships a built-in library of standalone scalar functions (task `00138`).
They are usable anywhere an expression is — `RETURN`, `WHERE`, `WITH`, `CASE`, `ORDER BY`,
inside `UNWIND`, and as a grouping key alongside an aggregation.

| Family | Functions |
|--------|-----------|
| String | `toLower`, `toUpper`, `trim`, `ltrim`, `rtrim`, `substring(s, start[, len])`, `replace(s, search, repl)`, `split(s, delim)`, `left(s, n)`, `right(s, n)`, `reverse`, `toString` |
| Numeric | `abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`, `toInteger`, `toFloat`, `toBoolean` |
| List / scalar | `size`, `length`, `head`, `last`, `tail`, `range(start, end[, step])`, `coalesce(a, b, …)`, `keys`, `labels`, `type`, `id`, `properties` |
| Path | `length(p)` (hop count), `nodes(p)`, `relationships(p)` — see [Named paths](#named-paths) |

**NULL handling.** Every function except `coalesce` is *NULL-propagating*: a `NULL`
argument yields `NULL`, never an error — so a function applied across a heterogeneous
scan quietly skips rows whose property is absent rather than aborting the query.
`coalesce(a, b, …)` is the exception — it returns its first non-`NULL` argument (or
`NULL` if every argument is `NULL`). `toInteger` / `toFloat` / `toBoolean` are lenient:
an unparseable string converts to `NULL` rather than erroring.

**Errors.** Wrong arity or an argument of a type the function cannot accept is a
recoverable `ExecError::InvalidFunctionCall`; an unknown function name stays
`ExecError::Unsupported`.

```cypher
RETURN toUpper(trim('  ready  ')) AS shout, size([1, 2, 3]) AS n, coalesce(null, 'fallback') AS pick
```

```cypher
UNWIND range(1, 5) AS x
RETURN x, x * x AS squared
```

```cypher
MATCH (n)
RETURN labels(n) AS kinds, keys(n) AS props
LIMIT 5
```

---

## drevo extension functions

drevo adds two scalar functions that bridge Cypher to its full-text and vector engines.

`keywords(text, k [, stem])` extracts the top-`k` salient terms from a string via BM25-IDF
(see [task `00132`](../README.md)):

```cypher
RETURN keywords('the anxious thought spiraled into catastrophic predictions about work', 3) AS top_terms
```

`similar(vector, query, threshold)` returns whether two embeddings are within `threshold`
cosine similarity — the building block for hybrid graph + vector filters:

```cypher
RETURN similar([0.10, 0.20, 0.30], [0.11, 0.19, 0.31], 0.80) AS is_similar
```

---

## Literals

| Kind | Examples |
|------|----------|
| Integer | `42`, `0`, `-17`, `0xff`, `0o17` |
| Float | `1.5`, `.5`, `5.`, `1.5e10` |
| String | `'single'`, `"double"`, with `\n \t \\ \u{1F600}` escapes |
| Boolean | `true`, `false` (case-insensitive) |
| Null | `null` (case-insensitive) |
| List | `[1, 2, 3]`, `[1, 'a', null]` |
| Map | `{name: 'Alice', age: 30}` |

---

## Indexing and slicing

Lists and maps support element access with `[]`.

- **List index** `xs[i]` — zero-based. A **negative** index counts from the end
  (`xs[-1]` is the last element). An **out-of-range** index yields `null` rather
  than an error, so a speculative lookup over a short list is simply absent. The
  index must be an integer.
- **Map / node / relationship index** `m['key']` — equivalent to property access
  (`m.key`); an absent key yields `null`. The key must be a string.
- **List slice** `xs[from..to]` — `from`-inclusive, `to`-exclusive, zero-based,
  with negative bounds counting from the end and every bound clamped into range.
  Either bound may be omitted: `xs[..n]`, `xs[n..]`, `xs[..]`.

`null` propagates: a `null` base, a `null` index, or a `null` slice bound makes
the whole expression `null`. Misuse — a non-integer list index, a non-string map
key, or indexing/slicing a scalar — is an `ExecError::TypeMismatch`.

```cypher
RETURN [10, 20, 30][1] AS second, [10, 20, 30][-1] AS last, range(1, 5)[1..3] AS middle
```

---

## List comprehensions

A **list comprehension** `[var IN list WHERE predicate | projection]` transforms a
list into a list without leaving the row. It is the in-expression counterpart to
[`UNWIND`](#unwind) + [`collect`](#aggregation): where `UNWIND` flattens a list
into rows and `collect` folds rows back, a comprehension maps and filters a list
in place.

Each element of `list` is bound to `var` in a child scope, the optional `WHERE`
`predicate` keeps the elements for which it holds, and the optional `| projection`
is collected for each survivor (when there is no `| projection`, the element
itself is kept). At least one of `WHERE` / `|` must be present — without either,
`[…]` is an ordinary [list literal](#literals).

```cypher
RETURN [x IN [1, 2, 3, 4, 5] WHERE x % 2 = 0 | x * x] AS even_squares
```

Semantics:

- **Scope** — `var` shadows any outer binding of the same name *only inside* the
  comprehension; the projection may freely reference outer bindings alongside
  `var` (`[x IN xs | x + base]`).
- **`null` list** — a `null` source (most often a node missing the property)
  makes the whole comprehension `null`, so a heterogeneous scan does not abort
  (mirrors `UNWIND` / `IN`).
- **Three-valued filter** — a `predicate` that is `false` *or* `null` drops the
  element; a non-boolean predicate is an `ExecError::TypeMismatch`.
- **Type** — a non-list source is an `ExecError::TypeMismatch`; element order is
  preserved.
- **Aggregations** are not allowed inside a comprehension (the loop variable is
  per-element, not per-group); use `collect` / `UNWIND` for that.

Comprehensions compose everywhere an expression does — over a node's list
property, feeding `size(...)`, inside `WHERE … IN […]`, nested, or alongside an
aggregation in the same `RETURN`:

```cypher
RETURN size([x IN range(1, 10) WHERE x % 3 = 0]) AS multiples_of_three
```

---

## List predicate functions

The **list predicate functions** `all`, `any`, `none`, and `single` collapse a
list into a single boolean by folding a predicate across its elements. They take
the same `var IN list WHERE predicate` form as a [list comprehension](#list-comprehensions)
— but the `WHERE` is **mandatory** and there is no `| projection`, because the
result is a truth value rather than a list. They are the idiomatic way to filter
rows by a *collection* property:

```cypher
RETURN all(x IN [2, 4, 6] WHERE x % 2 = 0) AS every_even
```

| Function | `true` when … | Empty list |
|----------|---------------|------------|
| `all(x IN list WHERE p)`    | `p` holds for **every** element  | `true`  |
| `any(x IN list WHERE p)`    | `p` holds for **some** element   | `false` |
| `none(x IN list WHERE p)`   | `p` holds for **no** element     | `true`  |
| `single(x IN list WHERE p)` | `p` holds for **exactly one** element | `false` |

Semantics:

- **Scope** — each element is bound to `var` in a child scope; the predicate may
  reference outer bindings alongside `var` (`all(x IN xs WHERE x > base)`).
- **`null` list** — a `null` source (most often a node missing the property)
  makes the whole predicate `null` (mirrors `UNWIND` / `IN` / list comprehension),
  so a heterogeneous scan does not abort.
- **Three-valued logic** — a `null` predicate result is *unknown*, not `false`,
  and folds accordingly: `all` is `null` when no element is `false` but some is
  unknown; `any` is `null` when no element is `true` but some is unknown; `none`
  is the negation of `any`; `single` is `null` when an unknown could change the
  match count. A definite `false` (for `all`) or `true` (for `any`) short-circuits
  regardless of any unknown.
- **Type** — a non-list source, or a non-boolean predicate, is an
  `ExecError::TypeMismatch`.
- **Aggregations** are not allowed inside the predicate (the loop variable is
  per-element, not per-group).

They compose anywhere an expression does — most often a `WHERE` over a node's
list property:

```cypher
RETURN single(x IN [1, 2, 3, 4] WHERE x % 2 = 0) AS exactly_one_even
```

---

## reduce

`reduce(accumulator = init, var IN list | expr)` folds a list into a single
value. It is the third member of the list-expression family: where a
[list comprehension](#list-comprehensions) maps a list to a list and a
[list predicate](#list-predicate-functions) collapses a list to a boolean,
`reduce` collapses a list to an *arbitrary* value — a sum, a product, a running
maximum, a concatenated string.

The seed `init` is evaluated once in the current scope to prime `accumulator`.
Each element of `list` is then bound to `var`, and the running total to
`accumulator`, in a child scope; `expr` computes the next accumulator value.
The final accumulator is the result.

```cypher
RETURN reduce(total = 0, n IN [1, 2, 3, 4] | total + n) AS sum
```

Semantics:

- **Left fold** — elements are folded left to right, so a non-commutative `expr`
  (e.g. string concatenation) sees them in list order.
- **Empty list** — yields the seed unchanged.
- **Scope** — both `accumulator` and `var` shadow any outer binding of the same
  name *only inside* the fold; `expr` may freely reference outer bindings
  alongside them.
- **`null` list** — a `null` source makes the whole `reduce` `null` (mirrors
  `UNWIND` / `IN` / the comprehension and predicate forms). A `null` produced by
  `expr` simply becomes the accumulator and folds onward.
- **Type** — a non-list source is an `ExecError::TypeMismatch`.
- **Aggregations** are not allowed inside `reduce` (the loop variable is
  per-element, not per-group); use `collect` / `UNWIND` for that.

A common shape is folding a `collect`ed property list — for example summing
line-item subtotals into an order total:

```cypher
RETURN reduce(toc = 'Chapters:', t IN ['Dawn', 'Noon', 'Dusk'] | toc + ' ' + t) AS toc
```

---

## Map projection

A map projection `base { selector, … }` builds a new map by projecting selected
entries off `base` — a node, a relationship, or a map value. It is the shaping
idiom for returning a tailored record per row without naming every property by
hand:

```cypher
MATCH (t:Task)
RETURN t {.title, .priority, kind: 'work-item'} AS card
```

Four selector forms compose in any mix:

- **`.key`** — copy property `key` from the base. An absent property projects to
  `null` (never an error).
- **`.*`** — copy *every* property of the base.
- **`key: expr`** — a computed entry; `expr` is evaluated in the current row, so
  it can reference any in-scope variable (`subtotal: l.qty * l.unit_price`).
- **`var`** — shorthand for `var: var`, the in-scope variable `var`.

```cypher
MATCH (l:Line)
RETURN l {.sku, .qty, subtotal: l.qty * l.unit_price, currency: 'USD'} AS line
```

Semantics:

- **Source order** — selectors apply left to right into a sorted map, so a later
  selector *overwrites* an earlier entry with the same key.
- **`null` base** — a `null` base makes the whole projection `null`, so projecting
  an unmatched [`OPTIONAL MATCH`](#optional-match) variable is `null` rather than
  an error.
- **Type** — a scalar (non-map, non-entity) base is an `ExecError::TypeMismatch`.
- **As a group key** — a map projection that contains no aggregation is an
  ordinary grouping expression, so `RETURN n {.category} AS k, count(*)` groups by
  the projected map.

A map projection composes anywhere an expression is allowed — including inside
`collect`, which gathers one projected record per row:

```cypher
MATCH (t:Tag)
RETURN collect(t {.label, .weight}) AS tags
```

---

## Pattern comprehension

A pattern comprehension `[ pattern WHERE predicate | projection ]` builds a list
by matching a graph `pattern` relative to the current row and projecting an
expression over each match. Where a [list comprehension](#list-comprehensions)
shapes a list you already have, a pattern comprehension shapes a list straight
off the graph — gathering a per-row collection without a second `MATCH`:

```cypher
MATCH (p:Person)
RETURN p.name AS name, [(p)-[:KNOWS]->(f) | f.name] AS friends
```

The `pattern` is an ordinary path with at least one relationship. It is
**anchored** on whatever variables the surrounding query has already bound — `p`
above — so each row only sees its own matches. Both the optional `WHERE` and the
mandatory `| projection` are evaluated in each match's binding scope, so they can
reference the freshly bound pattern variables (`f`, and a relationship variable
when present):

```cypher
MATCH (a:Account)
RETURN [(a)-[t:TRANSFER]->(b) WHERE t.amount > 100 | b.id] AS large_payees
```

Semantics:

- **Anchored & per-row** — the pattern extends the current row, exactly like the
  same pattern in a `MATCH` would; it never reaches rows from other groups.
- **No match → empty list** — a pattern that matches nothing yields `[]`, never
  `null`. Match order (and duplicates, e.g. parallel edges) is preserved.
- **`WHERE`** — filters matches under three-valued logic (`true` keeps,
  `false`/`null` drops); a non-boolean predicate is an `ExecError::TypeMismatch`.
- **`null` anchor** — if the head variable is already bound to `null` (an
  unmatched [`OPTIONAL MATCH`](#optional-match) node), the comprehension is `[]`
  rather than an error.
- **As a group key** — a pattern comprehension contains no aggregation, so it is
  an ordinary grouping expression alongside `count(*)` / `sum(...)`.

The `projection` is any expression, so a pattern comprehension composes with
[map projection](#map-projection) to gather tailored records per match:

```cypher
MATCH (o:Order)
RETURN [(o)-[c:CONTAINS]->(i) | i {.sku, qty: c.qty}] AS lines
```

---

## Pattern predicate

A **pattern predicate** is a path pattern `(a)-[:R]->(b)` used in a boolean
position — a `WHERE` filter, a `RETURN` column, or any expression slot. It tests
whether **at least one** match of the pattern exists relative to the current row.
Where a [pattern comprehension](#pattern-comprehension) *shapes a list* off the
graph, a pattern predicate *tests existence* of one:

```cypher
MATCH (p:Person)
WHERE (p)-[:KNOWS]->()
RETURN p.name
```

Like a comprehension's pattern, it is **anchored** on the variables already bound
(`p` above), so each row only tests its own neighbourhood. Variables the pattern
introduces are scoped to the predicate — they are not exported to the row. The
pattern can constrain by relationship type, by the target's label, and span
multiple hops:

```cypher
MATCH (s:Supplier)
WHERE (s)-[:SUPPLIES]->(:Part)
RETURN s.name
```

It composes with `NOT`, `AND` / `OR`, and other predicates, and can be returned
directly as a boolean:

```cypher
MATCH (b:Bug)
RETURN b.id AS id, NOT (b)-[:ASSIGNED_TO]->() AS unassigned
```

Semantics:

- **Existence test** — `true` as soon as one match exists; the matches are
  discarded (only their existence matters).
- **Anchored & per-row** — the pattern extends the current row, exactly like the
  same pattern in a `MATCH` would.
- **`null` anchor** — if the head variable is already bound to `null` (an
  unmatched [`OPTIONAL MATCH`](#optional-match) node), the predicate is `null`
  under three-valued logic — so a `WHERE` drops the row — rather than an error.
- **Grouping is unaffected** — only a path with at least one relationship is a
  predicate; a bare parenthesised expression (`(a.age + 1)`, `(1 + 2) * 3`,
  `(a).name`) is still ordinary grouping.

---

## Not yet supported

These constructs **parse** but the executor returns `ExecError::Unsupported` with a task
pointer, or are not yet in the grammar. They are tracked as follow-on Cypher tasks. Because
they are *deliberate, named* gaps rather than panics, a client always gets an actionable
error rather than a crash or a silent wrong answer.

| Construct | Status |
|-----------|--------|
| User-defined / `apoc.*` / `gds.*` procedures | only the built-in `db.*` introspection procedures exist — see [CALL](#call) |
| Variable-length paths in `CREATE` | executor returns `Unsupported` (semantically meaningless — how many edges?) |

When you need one of these today, express the intent through the [Rust](sdk-reference.md#rust-api)
or [Python](sdk-reference.md#python-sdk) API, which expose the underlying traversal, FTS, and
vector primitives directly.
