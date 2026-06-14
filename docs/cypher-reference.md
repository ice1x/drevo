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

## Not yet supported

These constructs **parse** but the executor returns `ExecError::Unsupported` with a task
pointer, or are not yet in the grammar. They are tracked as follow-on Cypher tasks. Because
they are *deliberate, named* gaps rather than panics, a client always gets an actionable
error rather than a crash or a silent wrong answer.

| Construct | Status |
|-----------|--------|
| `UNION` / `UNION ALL` | parses, executor returns `Unsupported` |
| `CALL` / `YIELD` (procedures) | not in grammar |
| `FOREACH` | not in executor |
| Named path binding `p = (a)-[*]->(b)` | parses, executor returns `Unsupported` |
| Variable-length paths in `CREATE` | executor returns `Unsupported` |
| `CASE … WHEN … THEN … END` | parses, executor returns `Unsupported` |
| Regex match `=~` | executor returns `Unsupported` |
| List/map indexing `x[i]` and slicing `x[a..b]` | executor returns `Unsupported` |
| Standalone scalar functions other than `keywords` / `similar` (`length`, `size`, `toLower`, …) | executor returns `Unsupported` |

When you need one of these today, express the intent through the [Rust](sdk-reference.md#rust-api)
or [Python](sdk-reference.md#python-sdk) API, which expose the underlying traversal, FTS, and
vector primitives directly.
