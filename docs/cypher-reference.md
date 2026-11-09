# Cypher Reference

drevo speaks a **subset of [Cypher](https://opencypher.org/)**, the openCypher query
language popularised by Neo4j. The subset is implemented bottom-up by a hand-written
lexer ([`src/cypher/lexer.rs`](https://github.com/ice1x/drevo/tree/main/src/cypher/lexer.rs)), a recursive-descent + Pratt
parser ([`src/cypher/parser.rs`](https://github.com/ice1x/drevo/tree/main/src/cypher/parser.rs)), and a tree-walking executor
([`src/cypher/executor.rs`](https://github.com/ice1x/drevo/tree/main/src/cypher/executor.rs)) that runs against a live
[`Drevo`](https://github.com/ice1x/drevo/tree/main/src/db.rs) handle.

> **Every fenced `cypher` block in this document is executed as a test.**
> [`tests/docs_examples.rs`](https://github.com/ice1x/drevo/tree/main/tests/docs_examples.rs) extracts each block, parses it,
> and runs it through the executor on a fresh in-memory database, asserting it neither
> fails to parse nor returns an executor error. The reference therefore cannot drift
> from the implementation: if drevo stops supporting a construct shown here, CI goes red.

This is a *living subset* — clauses that are not yet implemented return a deterministic
[`ExecError::Unsupported`](https://github.com/ice1x/drevo/tree/main/src/cypher/executor.rs) error naming the construct, never a
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

### Shortest paths

`shortestPath((a)-[*]-(b))` finds **one** shortest path between two nodes;
`allShortestPaths((a)-[*]-(b))` returns **every** path of that minimum length (one row each).
Both wrap a single **variable-length** relationship and are usually bound to a path variable,
so `length`, `nodes`, and `relationships` read the result like any [named path](#named-paths).
The endpoints are normally bound by a preceding pattern; the search is breadth-first, so the
first path found is provably the shortest.

```cypher
MATCH (a:Person {name: 'Ann'})-[:KNOWS]->(b:Person {name: 'Bob'})
MATCH (b)-[:KNOWS]->(c:Person {name: 'Cara'})
MATCH p = shortestPath((a)-[:KNOWS*]-(c))
RETURN length(p) AS degrees_of_separation, [x IN nodes(p) | x.name] AS via
```

The wrapped relationship must be variable-length (`-[*]-`, `-[:KNOWS*..5]-`, …) and there
must be exactly one of it; anything else is a recoverable `ExecError::InvalidFunctionCall`.

`shortestPath` counts hops. For the path with the lowest summed cost (seconds,
metres, …) use [`drevo.path.weighted`](#weighted-paths); for every node within a
cost budget, [`drevo.path.reachable`](#reachable-nodes); for journeys on public
transport, [`drevo.path.timetable`](#timetable-routing). To find where a route
starts and ends, [`drevo.spatial.nearest`](#nearest-nodes-to-a-point) gives the
nodes closest to a latitude and longitude.

### Weighted paths

`CALL drevo.path.weighted(start, end, options) YIELD path, cost` finds the path
from `start` to `end` whose relationships cost the least in total, which is what
routing needs: the quickest drive or walk, not the fewest streets. It yields one
row with the `path` and its total `cost`, or no row when `end` can't be reached.
It runs once per input row, so both ends can come from a preceding `MATCH`; a
`null` end yields no row.

| Option | Default | Meaning |
|--------|---------|---------|
| `relTypes` | every type | list of relationship types the path may use |
| `direction` | `'OUTGOING'` | `'OUTGOING'`, `'INCOMING'` or `'BOTH'`; one-way streets are relationships in one direction |
| `cost` | built-in weight (`1.0` for relationships created in Cypher) | numeric relationship property holding the cost; a relationship without it (or with `null`) is not used |
| `heuristic` | none | `{lat: 'lat', lon: 'lon', maxSpeedMps: 13.9}`: node position properties in degrees and the fastest speed in metres per second, for an A* search |
| `maxCost` | none | paths costing more are not returned, and the search stops there |

Without `heuristic` the search is a bidirectional Dijkstra. With it, the search
is A*: the straight-line distance to `end` divided by `maxSpeedMps` is a lower
bound on the remaining cost, so the search heads for `end` and explores less.
The path is still the cheapest as long as `maxSpeedMps` is at least as fast as
any relationship allows (no relationship costs less than its straight-line
length at that speed). A node without a position gets a bound of zero. Costs
must be non-negative numbers: a negative cost, or a `cost` property that holds
something other than a number, fails the call.

Several travel modes can share one road graph, each with its own cost property:

```cypher
CREATE (a:RoadNode {name: 'A', lat: 42.698, lon: 23.320}),
       (b:RoadNode {name: 'B', lat: 42.698, lon: 23.321}),
       (c:RoadNode {name: 'C', lat: 42.697, lon: 23.321}),
       (a)-[:ROAD {car_s: 60, walk_s: 300}]->(b),
       (b)-[:ROAD {car_s: 60, walk_s: 300}]->(c),
       (a)-[:PATH {walk_s: 100}]->(c)
WITH a, c
CALL drevo.path.weighted(a, c, {relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s'})
YIELD path, cost
RETURN [n IN nodes(path) | n.name] AS route, cost
```

On foot the route is `['A', 'C']` over the footpath at cost `100.0`; with
`cost: 'car_s'` the footpath has no cost, so the car goes `['A', 'B', 'C']` at
`120.0`. With positions on the nodes, A* gives the same routes:

```cypher
MATCH (a:RoadNode {name: 'A'}), (c:RoadNode {name: 'C'})
CALL drevo.path.weighted(a, c, {
  relTypes: ['ROAD'], cost: 'car_s',
  heuristic: {lat: 'lat', lon: 'lon', maxSpeedMps: 30},
  maxCost: 600
})
YIELD path, cost
RETURN cost, [n IN nodes(path) | [n.lat, n.lon]] AS coords
```

### Reachable nodes

`CALL drevo.path.reachable(start, options) YIELD node, cost, path` returns every
node that can be reached from `start` within a cost budget, each with its
least cost, cheapest first: what is within a 10-minute walk, or which stops
are close enough to walk to. Nodes of equal cost come back in id order. It runs
once per input row; a `null` start yields no row. `start` itself is always
reached, at cost `0.0`.

The options `relTypes`, `direction` and `cost` work as for
[`drevo.path.weighted`](#weighted-paths), and so do the cost rules: a
relationship without the cost property is not used, and a negative cost fails
the call. With `direction: 'INCOMING'` the cost is that of getting from each
node *to* `start`, over one-way streets.

| Option | Default | Meaning |
|--------|---------|---------|
| `maxCost` | required | the budget; nodes that cost more are not reached |
| `targets` | every node | `{label: 'Stop'}`: only nodes with this label are returned, though the search goes through every node |
| `limit` | none | stop after this many nodes are returned |
| `path` | `false` | `true` also returns the cheapest `path` to each node; otherwise `path` is `null` |

The walking isochrone around a crossing, and the stops on it:

```cypher
CREATE (a:RoadNode {name: 'A'}), (b:RoadNode {name: 'B'}),
       (c:RoadNode:Stop {name: 'C'}), (d:RoadNode:Stop {name: 'D'}),
       (a)-[:ROAD {walk_s: 300}]->(b),
       (b)-[:ROAD {walk_s: 300}]->(c),
       (a)-[:PATH {walk_s: 100}]->(c),
       (c)-[:ROAD {walk_s: 600}]->(d)
WITH a
CALL drevo.path.reachable(a, {
  relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s',
  maxCost: 400, targets: {label: 'Stop'}, path: true
})
YIELD node, cost, path
RETURN node.name AS stop, cost, [n IN nodes(path) | n.name] AS route
```

The only stop within 400 seconds is `C`, at cost `100.0` over the footpath
(`route` is `['A', 'C']`); `D` costs `700.0`, over the budget.

### Timetable routing

`CALL drevo.path.timetable(starts, ends, options) YIELD arriveAt, departAt,
transfers, legs` plans a public-transport journey: leave at a given time from
near one place, arrive as early as possible near another, riding buses and
trams, changing between them and walking between stops. It runs once per
input row.

The timetable lives in the graph. Stops are nodes. A **hop** relationship runs
from a stop to the next stop of a trip pattern (one stop sequence of a line)
and holds one entry per trip in four parallel list properties: the departure
from its start stop, the arrival at its end stop, the trip id and the service
id. Times are seconds after midnight of the trip's service day and may pass
`86400` for trips that run past midnight. A **footpath** is a relationship
between two stops holding the walk in seconds.

```text
(:Stop)-[:HOP {dep: [29400, 30000], arr: [29520, 30120], trip: [8812, 8813], svc: [17, 17]}]->(:Stop)
(:Stop)-[:WALK {s: 120}]->(:Stop)
```

`starts` and `ends` are lists of `{node, cost}` maps: the stops near the
origin with the seconds it takes to walk to each, and the stops near the
destination with the seconds from each. `drevo.path.weighted` on a road graph
gives those costs.

| Option | Default | Meaning |
|--------|---------|---------|
| `departAt` | required | when the journey starts, in seconds after midnight of the query date |
| `days` | required | the service days whose trips may run, each `{offset, services}`: seconds added to its trips' times (`0` today, `-86400` yesterday, for its trips past midnight) and the service ids running that day |
| `hops` | `{relType: 'HOP', dep: 'dep', arr: 'arr', trip: 'trip', service: 'svc'}` | the hop relationship type and its list properties |
| `walk` | none (no walking between stops) | `{relTypes: ['WALK'], cost: 's'}`: footpath relationship types and their seconds property |
| `minTransfer` | `0` | seconds a change of vehicles takes at least, at one stop or over footpaths |
| `maxTransfers` | `4` | the most vehicle changes in a journey |
| `maxDuration` | none | journeys arriving later than `departAt` plus this are not returned |
| `maxWalk` | none | the most seconds of footpaths in a row — between two rides, or after the start cost; a longer chain is not followed |

Staying on the same trip from hop to hop is one ride. Boarding needs the
vehicle to leave no earlier than you are at the stop. After a ride, the next
vehicle can be boarded `minTransfer` seconds after arriving, at that stop or
at any stop reached over footpaths from it — a walk between rides takes its
seconds, or `minTransfer` when that is longer, so two poles of one stop joined
by a 0 s footpath still need the transfer time. Several footpaths may follow
each other, up to `maxWalk` seconds in a row when it is given. A trip runs on
a day when its service is in that day's `services`. Hops and footpaths are
followed in their direction only.

Each row is a journey that arrives earlier than every journey with fewer
transfers, so a slower direct ride and a faster ride with a change both come
back; rows are ordered by `arriveAt`, and there are at most `maxTransfers + 1`.
A journey that needs no ride at all (a start that is also an end) counts as no
transfers. Nothing arriving within `maxDuration` gives no row.

- `arriveAt` — when the destination is reached;
- `departAt` — when to leave the origin: the first ride's departure minus the
  walk to its stop;
- `transfers` — vehicle changes;
- `legs` — the journey in order, each a map `{mode, from, to, depart, arrive,
  trip, hops}`: `mode` is `'access'` (origin to the first stop, `from` is
  `null`), `'ride'`, `'walk'` or `'egress'` (last stop to the destination, `to`
  is `null`); `trip` is the trip id of a ride; `hops` lists the relationships
  travelled — the hops of a ride or the footpath of a walk.

```cypher
CREATE (a:Stop {name: 'Central'}), (b:Stop {name: 'Market'}),
       (c:Stop {name: 'Park'}), (d:Stop {name: 'Zoo'}),
       (a)-[:HOP {dep: [28800, 29400], arr: [29100, 29700], trip: [11, 12], svc: [1, 1]}]->(b),
       (b)-[:HOP {dep: [29160, 29760], arr: [29400, 30000], trip: [11, 12], svc: [1, 1]}]->(c),
       (c)-[:WALK {s: 180}]->(d)
WITH a, d
CALL drevo.path.timetable([{node: a, cost: 240}], [{node: d, cost: 60}], {
  departAt: 28500,
  days: [{offset: 0, services: [1]}],
  walk: {relTypes: ['WALK'], cost: 's'},
  minTransfer: 60
})
YIELD arriveAt, departAt, transfers, legs
RETURN arriveAt, departAt, transfers,
       [l IN legs | [l.mode, coalesce(l.from.name, '-'), coalesce(l.to.name, '-'), l.trip]] AS route
```

Leaving at 07:55 (`28500`), the journey walks 4 minutes to Central, rides trip
11 from 08:00 to Park at 08:10, walks 3 minutes to the Zoo stop and 1 more to
the destination: `arriveAt` is `29640`, `departAt` `28560`, no transfers.

The search runs in rounds, one more ride per round (RAPTOR). A query reads
only the stops it reaches, so its cost grows with the part of the network
within reach of `departAt + maxDuration`, not with the size of the graph.

### Nearest nodes to a point

`CALL drevo.spatial.nearest(label, {lat, lon}, options) YIELD node, meters`
returns the nodes with `label` closest to a point, nearest first, with the
great-circle (haversine) distance in metres. Nodes at the same distance come
back in id order. The point is a map of two numbers in degrees, the latitude
in −90..90 and the longitude in −180..180; `options` may be left out. It runs
once per input row; a `null` label or point yields no row.

A node's position is read from two number properties, `lat` and `lon` unless
the options name others. Nodes without both, or with a value that is not a
number in range, are never returned.

| Option | Default | Meaning |
|--------|---------|---------|
| `k` | `1` | at most this many nodes |
| `maxMeters` | none | only nodes at most this far away; none in range gives no rows |
| `where` | none | a map: only nodes whose properties equal its values (`{r: 1}`) |
| `lat`, `lon` | `'lat'`, `'lon'` | the properties holding the position |

A [point index](#point-indexes) on the label and the two properties answers
the call from a grid instead of reading every node with the label; the answer
is the same. On a database with no such index, the call reads every node with
the label.

The nearest drivable junction to a point, and the route from there:

```cypher
CREATE (o:Junction {name: 'Opera', r: 1, lat: 42.6935, lon: 23.3290}),
       (l:Junction {name: 'Lions Bridge', r: 1, lat: 42.7075, lon: 23.3216}),
       (:Junction {name: 'Footway', r: 0, lat: 42.6977, lon: 23.3219}),
       (o)-[:ROAD {car_s: 120}]->(l)
WITH 1 AS one
CALL drevo.spatial.nearest('Junction', {lat: 42.6977, lon: 23.3219}, {where: {r: 1}})
YIELD node AS start, meters
MATCH (goal:Junction {name: 'Lions Bridge'})
CALL drevo.path.weighted(start, goal, {relTypes: ['ROAD'], cost: 'car_s'})
YIELD cost
RETURN start.name AS start, round(meters) AS meters, cost
```

The footway crossing is closer, but `where` skips it: the route starts at
Opera, 745 m away, and costs `120.0`.

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

**Label predicates:** `n:Label` is a boolean expression, so it combines with `AND` / `OR` /
`NOT` and can be returned like any other value. `n:A:B` requires every listed label; on a
relationship, `r:TYPE` tests its type. A `null` operand yields `null`.

```cypher
MATCH (n)
WHERE (n:Bug OR n:Feature) AND NOT n:Archived
RETURN n.title AS item, n:Bug AS is_bug
```

**Label expressions** (Neo4j 5) work both in node patterns and in label predicates:
`|` (or), `&` (and), `!` (not), `%` (any label) and parentheses. Precedence, from loosest:
`|`, `&`, `!`. The legacy `:A:B` means `:A&B`.

```cypher
MATCH (n:(Bug|Feature)&!Archived)
RETURN n.title AS item, n:Bug|Feature AS tracked
```

- CREATE and MERGE write concrete labels, so they accept only `:A:B` / `:A&B`.
- Inside a list or pattern comprehension filter, a `|` after a label starts the projection:
  `[x IN xs WHERE x:Bug | x.title]`. To get a disjunction there, put it in parentheses:
  `x:(Bug|Feature)`.
- In relationship patterns, only `[r:A|B]` is supported. For other type expressions, filter
  with `WHERE r:!A`.

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

A variable or alias is an identifier, or one of the keywords that only ever
stand between operands or inside a clause: `starts`, `ends`, `contains`, `in`,
`is`, `on`, `by`, `asc`, `desc`, `and`, `or`, `xor`. They keep working as
keywords in their own position.

```cypher
UNWIND ['Central', 'North'] AS starts
RETURN starts, starts STARTS WITH 'C' AS central
ORDER BY starts DESC
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

An aggregation can be an operand of a larger expression: an argument of a function, an
indexed or sliced list, a map value, or the list a comprehension, list predicate or `reduce`
walks. The aggregation is computed over the group first, then the expression around it:

```cypher
MATCH (b:Bug)
WITH b ORDER BY b.severity DESC
RETURN b.component AS component,
       size(collect(b)) AS bugs,
       collect(b.title)[0..3] AS worst,
       {open: count(b), mean: round(avg(b.severity))} AS summary,
       [t IN collect(b.title) WHERE t STARTS WITH 'Crash'] AS crashes
```

An aggregation cannot appear inside another aggregation (`count(size(collect(x)))`) or in the
body of a comprehension, list predicate or `reduce`, which runs once per element.

### Statistical aggregations

Four further aggregations fold a numeric group to a statistic, exactly like
`avg`. Each null-skips its input and rejects a non-numeric value with a
recoverable error.

| Function | Returns |
|----------|---------|
| `stDev(expr)` | **sample** standard deviation (divides by `n − 1`; `0.0` for fewer than two values) |
| `stDevP(expr)` | **population** standard deviation (divides by `n`; `0.0` for an empty group) |
| `percentileCont(expr, p)` | the **continuous** percentile at fraction `p ∈ [0, 1]`, linearly interpolating between the two closest ranks (always a `Float`) |
| `percentileDisc(expr, p)` | the **discrete** percentile: the nearest actual value at fraction `p` (no interpolation, so the stored `Integer` / `Float` type is preserved) |

```cypher
MATCH (o:Order)
RETURN stDevP(o.total)              AS spread,
       percentileCont(o.total, 0.5) AS median,
       percentileDisc(o.total, 0.9) AS p90
```

`percentileCont` / `percentileDisc` over an empty (all-null) group are `null`;
the fraction must be a number in `[0.0, 1.0]` (anything else is an error). All
four are ordinary aggregations, so any non-aggregated projection item is the
`GROUP BY` key and they coexist with `count` / `sum` / `avg` / … in one
`RETURN`.

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
discover a graph's shape. Of APOC, the trigger procedures `apoc.trigger.*` are
built in (see [Triggers](#triggers)); there is no support for user-defined, other
`apoc.*`, or `gds.*` procedures.

| Procedure | Output column | Returns |
|-----------|---------------|---------|
| `db.labels()` | `label` | every distinct node label (the primary kind plus any secondary `:Extra` labels), sorted |
| `db.relationshipTypes()` | `relationshipType` | every distinct relationship type, sorted |
| `db.propertyKeys()` | `propertyKey` | every distinct property key across nodes and relationships, sorted (the reserved `_labels` key is never exposed) |
| `drevo.info()` | `version, git_sha, build_date, protocol` | the running build's version + metadata, so a Bolt client can assert a minimum-compatible drevo (issue #303) |
| `drevo.engine.status()` | `engine, mirror_fresh, native_hits, kv_fallbacks, kv_routed, rebuild_errors` | which engine serves this database's Cypher (`kv` or `native`) and the native read mirror's routing counters (RFC #307) |
| `drevo.pagerank([config])` | `node, score` | PageRank centrality over the whole graph, computed over an immutable adjacency snapshot; rows come back most-central-first, ties by node id. The optional config map takes the keys of `drevo.pagerank.personalized` except `sourceWeights` (`maxIterations` defaults to `100` here) — see [Personalized PageRank](#personalized-pagerank) |
| `drevo.pagerank.personalized(sourceNodes[, config])` | `node, score` | PageRank whose random walk jumps back to `sourceNodes` instead of to a random node: which nodes matter *for these seeds* — see [Personalized PageRank](#personalized-pagerank) |
| `drevo.louvain()` | `node, community` | Louvain community detection over the whole graph; each node is returned with its contiguously-numbered community id (RFC #307 Phase 8) |
| `drevo.wcc()` | `node, component` | Weakly connected components over the whole graph; each node is returned with its component id (edges treated as undirected), numbered by the minimum node id per component (RFC #307 Phase 8) |
| `drevo.scc()` | `node, component` | Strongly connected components over the whole graph (iterative Tarjan); two nodes share a component only if mutually reachable **following edge direction** — i.e. on a common directed cycle — numbered by the minimum node id per component (RFC #307 Phase 8) |
| `drevo.triangles()` | `node, triangles, coefficient` | Triangle count and local clustering coefficient per node over the undirected projection; `coefficient` is `2·triangles / (degree·(degree−1))` in `[0,1]`, `0` for degree `< 2` (RFC #307 Phase 8) |
| `drevo.betweenness()` | `node, score` | Betweenness centrality (Brandes' algorithm) over the directed, unweighted graph; each node's `score` is the number of shortest paths through it, rows most-central first (RFC #307 Phase 8) |
| `drevo.closeness()` | `node, score` | Harmonic closeness centrality over the directed, unweighted graph; `score` is `Σ 1/d` over all nodes reachable along edge direction (stays finite when disconnected), rows most-central first (RFC #307 Phase 8) |
| `drevo.ricciCurvature()` | `from, to, curvature` | Ollivier–Ricci curvature of every undirected edge over the unweighted projection (edge direction and weight ignored): `curvature ≤ 1` is negative on bridge-like edges between communities (good cut candidates) and positive inside dense clusters; each edge returned once with `from` the smaller-id endpoint, ascending by `(from, to)` (issue #526) |
| `drevo.stableMatching(proposerLabel, acceptorLabel, relType, rankProperty)` | `proposer, acceptor, proposerRank, acceptorRank` | Gale–Shapley stable matching between two sides that rank each other with `relType` edges (lower `rankProperty` = preferred); proposer-optimal, only mutually-ranked pairs match, one row per matched pair (issue #541) |
| `drevo.path.weighted(start, end, options)` | `path, cost` | The lowest-cost path from `start` to `end` over a numeric relationship property, chosen relationship types and direction, by A* or bidirectional Dijkstra — see [Weighted paths](#weighted-paths) |
| `drevo.search.hybrid(query, label, embeddingProperty, k[, config])`, `drevo.search.hybridRelationships(query, type, …)` | `node` or `rel`, `score`, `ftsRank`, `vectorRank`, `mode` | BM25 and embedding similarity fused with Reciprocal Rank Fusion, best first — see [Hybrid search](#hybrid-search-780) |
| `drevo.graph.expand(seeds[, config])` | `node, distance, via, truncated` | The seeds and every node within `hops` of them, each once at its shortest distance with the relationship that reached it, in a fixed order — see [Graph expansion](#graph-expansion) |
| `drevo.rag.retrieve(query[, config])` | `node, seedScore, distance, pprScore, rank, via, truncated` | The nodes that answer a question, best first: hybrid-search seeds, then their neighbourhood ranked by Personalized PageRank, distance or walk order — see [Graph-RAG retrieval](#graph-rag-retrieval) |
| `drevo.rag.context(query[, config])` | `text, tokens, format, nodeIds, relIds, truncated, mode, traceId` | The same nodes and the relationships between them written as LLM-ready text (markdown, triples or JSON) within a token budget — see [Graph-RAG retrieval](#graph-rag-retrieval) |
| `drevo.entity.candidates(label[, config])`, `drevo.entity.merge(keepId, dropIds[, config])`, `drevo.entity.aliases(name)` | `a, b, nameScore, vectorScore, score, reasons`; `kept, merged, relationshipsMoved, propertiesMerged`; `entity, aliases` | Pairs of nodes that probably name the same thing, with the reasons; folding dropped nodes into a kept one that remembers their names; the nodes known under a name — see [Entity resolution](#entity-resolution) |
| `drevo.rag.traces(session, limit)` | `trace` | The newest context traces of a session (`null`: of every session) — see [Context traces](#context-traces) |
| `drevo.rag.explain(traceId)` | `node, status, rank, distance, seedScore, pprScore, tokens, reason` | What one context was made of: the nodes written and the nodes left out, by rank |
| `drevo.rag.replay(traceId)` | `text, tokens, changed` | A traced context re-run on the graph as it is now, and whether its text changed; records nothing |
| `drevo.rag.pruneTraces(olderThan)` | `deleted` | Delete context traces created before a UTC date or timestamp; needs `EXECUTE ADMIN PROCEDURES` |
| `drevo.rag.evaluate(questions[, config])` | `question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds, recallRetrieved` | Each retrieval strategy (vector, BM25, hybrid, hybrid + expansion, the full pipeline) scored on a gold set within one token budget — see [Evaluate strategies](#evaluate-strategies) |
| `drevo.rag.evaluateSummary(questions[, config])` | `strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms, meanTokens` | The same scores averaged per strategy, with latency percentiles |
| `drevo.text.fuzzy(indexName, query, maxEdits)` | `node, distance` | Nodes covered by the text index `indexName` whose value is within `maxEdits` Levenshtein edits of `query`, nearest first, ties by node id — see [Fuzzy matching](#fuzzy-matching-593) (issue #593) |
| `drevo.indexes.secondary()`, `drevo.indexes.setSecondary(enabled)` | `enabled` | Report, or turn off / on, the database's property, value-cache and full-text indexes — see [Secondary indexes](#secondary-indexes) |
| `apoc.trigger.install(db, name, statement, selector[, config])` and the other `apoc.trigger.*` | `name, query, selector, params, installed, paused` | Install, list, stop, start and drop triggers — see [Triggers](#triggers) |
| `drevo.sequence.create(name[, {start, step}])`, `drevo.sequence.drop(name)`, `drevo.sequence.list()` | `name, start, step, current` | Create, drop and list sequences; draw values with `drevo.sequence.next(name)` — see [Sequences](#sequences) |

### Stable matching (#541)

`drevo.stableMatching` pairs two sides of the graph — mentees and mentors,
reviewers and papers, candidates and openings — so that no two would both
rather be with each other than with their assigned partners (a **stable**
matching, found by Gale–Shapley deferred acceptance). Preferences are ordinary
edges: each node ranks members of the other side with an outgoing `relType`
edge carrying a numeric `rankProperty`, lower meaning more preferred. The
proposing side gets the best partners it can have in any stable matching.

A pair can match only when each side ranks the other; an edge without a numeric
rank comes after every ranked one (ties by node id). Sides may differ in size —
unmatched nodes are simply absent. `proposerRank` / `acceptorRank` are the
ranks each side gave the other, as stored.

```cypher
CREATE (ann:Mentee {name: 'ann'}), (xia:Mentor {name: 'xia'}),
       (ann)-[:PREFERS {rank: 1}]->(xia), (xia)-[:PREFERS {rank: 1}]->(ann)
WITH ann
CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank')
YIELD proposer, acceptor, proposerRank, acceptorRank
RETURN proposer.name, acceptor.name, proposerRank, acceptorRank
```

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
| Edit distance | `apoc.text.levenshteinDistance(a, b)` — the fewest single-character insertions, deletions and substitutions turning `a` into `b` (an `Integer`); `apoc.text.levenshteinSimilarity(a, b)` — `1 − distance / max(length)`, `1.0` for two empty strings. Characters are Unicode code points. See [Fuzzy matching](#fuzzy-matching-593) |
| Name normalisation | `drevo.entity.normalise(name)` — `name` lower-cased, without accents or punctuation, words separated by single spaces (`'А. С. Пушкин'` → `'а с пушкин'`), the form entity resolution compares. See [Entity resolution](#entity-resolution) |
| Numeric | `abs`, `ceil`, `floor`, `round(x[, precision[, mode]])`, `sign`, `sqrt`, `toInteger`, `toFloat`, `toBoolean` — see [round()](#rounding-with-round) for the precision / mode overloads |
| List conversion | `toIntegerList(list)`, `toFloatList(list)`, `toBooleanList(list)`, `toStringList(list)` — element-wise conversion; an unconvertible (or `NULL`) element becomes `NULL`, preserving list length |
| Lenient conversion | `toIntegerOrNull(x)`, `toFloatOrNull(x)`, `toBooleanOrNull(x)`, `toStringOrNull(x)` — fully-lenient scalar conversion; any value that cannot be converted yields `NULL` instead of an error (unlike the strict `toString`, which errors on a non-stringifiable value) |
| Trigonometric / logarithmic | `e()`, `pi()`, `exp(x)`, `log(x)`, `log10(x)`, `sin`, `cos`, `tan`, `cot`, `asin`, `acos`, `atan`, `atan2(y, x)`, `degrees(x)`, `radians(x)`, `haversin(x)` — see [Trigonometric & logarithmic functions](#trigonometric--logarithmic-functions) |
| List / scalar | `size`, `length`, `head`, `last`, `tail`, `range(start, end[, step])`, `coalesce(a, b, …)`, `keys`, `labels`, `type`, `properties` |
| Identity | `id(x)` — a node's or relationship's id, an `Integer`; `elementId(x)` — the same id as a `String` (`"42"`), the `element_id` Bolt 5 drivers report on nodes and relationships. `MATCH (n) WHERE elementId(n) = $e` (or `IN $ids`) looks the node up by id, like `id(n) = $id`; the two do not mix types, so `elementId(n) = 42` matches nothing |
| Container predicate | `isEmpty(x)` — `true` when the container `x` holds no elements; accepts a String, a List, or a Map. Fills the gap left by `size`, which rejects a Map (so `size(m) = 0` cannot express it). A non-container argument errors |
| Numeric predicate | `isNaN(n)` — `true` when `n` is the IEEE-754 NaN value, `false` for any other number (an `Integer` is never NaN, and `±Infinity` are numbers, not NaN). The only way to test for NaN, since `NaN = NaN` is *false*. See [isNaN()](#testing-for-nan-with-isnan) |
| Non-deterministic | `rand()` — uniform `Float` in `[0.0, 1.0)`; `randomUUID()` — a fresh version-4 UUID string. Both take **no arguments** and re-draw on every evaluation (per row). See [Non-deterministic functions](#non-deterministic-functions) |
| Path | `length(p)` (hop count), `nodes(p)`, `relationships(p)` — see [Named paths](#named-paths) |
| Relationship | `startNode(r)`, `endNode(r)` — the source / target node of a relationship `r`. The companion to [`fts.searchRelationships`](#procedures): a `YIELD rel` endpoint is then directly reachable, no re-`MATCH`. A `NULL` argument yields `NULL`; a non-relationship argument errors |

**NULL handling.** Every function except `coalesce` is *NULL-propagating*: a `NULL`
argument yields `NULL`, never an error — so a function applied across a heterogeneous
scan quietly skips rows whose property is absent rather than aborting the query.
`coalesce(a, b, …)` is the exception — it returns its first non-`NULL` argument (or
`NULL` if every argument is `NULL`). `toInteger` / `toFloat` / `toBoolean` are lenient:
an unparseable string converts to `NULL` rather than erroring. The list-conversion
functions (`toIntegerList` / `toFloatList` / `toBooleanList` / `toStringList`) apply the
same lenient rules to every element, so an unconvertible element becomes a `NULL` slot
rather than aborting the whole list — the result always matches the input length. (Unlike
the scalar `toString`, which errors on a non-stringifiable value, `toStringList` is lenient
and yields `NULL` for such an element.) The `*OrNull` conversions
(`toIntegerOrNull` / `toFloatOrNull` / `toBooleanOrNull` / `toStringOrNull`) are
*fully lenient*: any value they cannot convert — an unparseable string, a List,
a Map, a node — yields `NULL` rather than an error. `toStringOrNull` is the one
whose behaviour differs from its strict sibling: scalar `toString` errors on a
non-stringifiable value, whereas `toStringOrNull` returns `NULL`. The idiomatic
use is `coalesce(toIntegerOrNull(x), default)` for a safe parse-with-fallback.

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

```cypher
CALL fts.searchRelationships('merger', 10) YIELD rel, score
RETURN startNode(rel).name AS source, endNode(rel).name AS target, score
```

### Trigonometric & logarithmic functions

Task `00156` completes the numeric family with Neo4j's trigonometric and
logarithmic functions. Each returns a `Float`; an integer argument widens
automatically. Angles are in **radians** (use `radians(x)` / `degrees(x)` to
convert).

| Family | Functions |
|--------|-----------|
| Constants | `e()`, `pi()` (zero-argument) |
| Exponential / logarithmic | `exp(x)` (eˣ), `log(x)` (natural log), `log10(x)` |
| Trigonometric | `sin(x)`, `cos(x)`, `tan(x)`, `cot(x)` |
| Inverse trigonometric | `asin(x)`, `acos(x)`, `atan(x)`, `atan2(y, x)` |
| Angle helpers | `degrees(x)`, `radians(x)`, `haversin(x)` |

Like the rest of the library they are **NULL-propagating** (a `NULL` argument
yields `NULL`) and reject a non-numeric argument with
`ExecError::InvalidFunctionCall`. **Domain edges follow IEEE-754, not errors:**
`log(-1)` and `asin(2)` return `NaN`, `log(0)` returns `-Infinity` — exactly as
Neo4j does, so a heterogeneous scan never aborts on an out-of-domain value.

```cypher
RETURN round(degrees(pi())) AS half_turn, exp(0) AS one, log10(1000) AS three
```

```cypher
RETURN sin(0) AS zero, cos(0) AS one, atan2(1, 1) AS quarter_pi
```

### Rounding with `round()`

Task `00160` extends `round` from its single-argument form to Neo4j's three
overloads. The result is always a `Float`.

| Form | Meaning |
|------|---------|
| `round(value)` | Nearest integer, ties away from zero (`HALF_UP`). |
| `round(value, precision)` | Round to `precision` decimal places, still `HALF_UP`. A **negative** `precision` rounds to the left of the decimal point (`round(1234.5, -2) = 1200.0`). |
| `round(value, precision, mode)` | Round to `precision` decimal places under `mode` — one of `UP`, `DOWN`, `CEILING`, `FLOOR`, `HALF_UP`, `HALF_DOWN`, `HALF_EVEN` (case-insensitive), matching Java's `RoundingMode`. |

The rounding modes are: `UP` (away from zero), `DOWN` (toward zero / truncate),
`CEILING` (toward +∞), `FLOOR` (toward −∞), and the three tie-breakers `HALF_UP`
(ties away from zero), `HALF_DOWN` (ties toward zero), and `HALF_EVEN` (ties to
the even digit — banker's rounding). Rounding is performed on the number's
**decimal** digits, so `round(1.255, 2)` is `1.26` (the intuitive decimal
answer) rather than the `1.25` a naive binary `value * 100` scaling would give.

Like the rest of the library `round` is **NULL-propagating** (a `NULL` in any
argument yields `NULL`); a non-numeric `value`, non-Integer `precision`,
non-String `mode`, an unknown `mode` keyword, or wrong arity is a recoverable
`ExecError::InvalidFunctionCall`. A non-finite `value` (`NaN` / ±`Infinity`) is
returned unchanged.

```cypher
RETURN round(2.71828, 2) AS pe,
       round(1234.5, -2) AS thousands,
       round(2.5, 0, 'HALF_EVEN') AS banker
```

### Non-deterministic functions

Task `00161` adds the two zero-argument value generators Neo4j exposes. Both
re-draw on **every evaluation**, so used inside `UNWIND` / a scan they yield a
fresh value per row.

| Function | Result | Notes |
|----------|--------|-------|
| `rand()` | Uniform `Float` in `[0.0, 1.0)` | Backed by a fast per-thread non-cryptographic generator, mirroring Neo4j's `ThreadLocalRandom`. The result never reaches `1.0`. |
| `randomUUID()` | A version-4 UUID as a 36-char `8-4-4-4-12` lowercase-hex `String` | Backed by the OS CSPRNG, mirroring Neo4j's `SecureRandom`. Practically unique across draws. |

Both take **no arguments** — passing any is a recoverable
`ExecError::InvalidFunctionCall`. They compose like any other expression:

```cypher
// Roll a six-sided die and mint an external id per row.
UNWIND range(1, 3) AS i
RETURN toInteger(rand() * 6) + 1 AS die, randomUUID() AS ref
```

```cypher
// Stamp a stable reference on each newly-created node.
CREATE (b:Bug {title: 'login crash', ref: randomUUID()})
RETURN b.ref
```

---

### Testing for NaN with `isNaN()`

Task `00162` adds `isNaN(n)`, the boolean predicate that tells the IEEE-754 NaN
value apart from every other number. It is the **only** way to test for NaN in
Cypher: per IEEE-754, `NaN = NaN` is *false*, so an equality comparison can
never catch it.

| Argument | Result |
|----------|--------|
| A `Float` that is NaN | `true` |
| Any other `Float`, including `±Infinity` | `false` |
| An `Integer` (never NaN) | `false` |
| `NULL` | `NULL` (NULL-propagating) |
| A non-numeric value (String, Bool, List, Map) | recoverable `ExecError::InvalidFunctionCall` |

NaN arises from the [trigonometric / logarithmic](#trigonometric--logarithmic-functions)
domain edges (`sqrt(-1)`, `log(-1)`, `asin(2)`, …) and from float division of
zero by zero (`0.0/0.0`). `isNaN` is the guard that keeps such an undefined
result from silently flowing into a report:

```cypher
// Guard a ratio whose denominator may be zero.
WITH 0.0 AS reopened, 0.0 AS resolved
RETURN isNaN(reopened / resolved) AS rate_undefined
```

```cypher
// Keep only the rows whose computed value is NaN.
UNWIND [sqrt(-1.0), sqrt(4.0), 0.0 / 0.0] AS x
WITH x WHERE isNaN(x)
RETURN count(*) AS nan_count
```

---

## drevo extension functions

drevo adds two scalar functions that bridge Cypher to its full-text and vector engines.

`keywords(text, k [, stem])` extracts the top-`k` salient terms from a string via BM25-IDF
(see [task `00132`](https://github.com/ice1x/drevo/tree/main/README.md)):

```cypher
RETURN keywords('the anxious thought spiraled into catastrophic predictions about work', 3) AS top_terms
```

`similar(vector, query, threshold)` returns whether two embeddings are within `threshold`
cosine similarity — the building block for hybrid graph + vector filters:

```cypher
RETURN similar([0.10, 0.20, 0.30], [0.11, 0.19, 0.31], 0.80) AS is_similar
```

`cosine_similarity(a, b)` returns the cosine similarity **score** (a Float in
`[-1, 1]`) of two numeric-list vectors. Unlike `similar()`, which only
threshold-filters, the score is usable in `RETURN` / `ORDER BY` for scored
retrieval. NULL-propagating; a non-list argument, dimension mismatch, or zero
vector is an error.

```cypher
MATCH (c:Chunk {book_id: $book_id})
WHERE c.embedding IS NOT NULL
RETURN c, cosine_similarity(c.embedding, $query) AS score
ORDER BY score DESC
LIMIT $k
```

`CALL drevo.vector.query(label, property, query, k) YIELD node, score` returns
the top-`k` nodes of `label` ranked by cosine similarity between their
`property` embedding and the `query` vector — the same contract Neo4j's
`db.index.vector.queryNodes` exposes. Nodes without the property (or with a
non-numeric / mismatched-dimension one) are skipped. It is a brute-force scan
(sub-millisecond at per-book scale); `k` is applied before any post-`YIELD`
`WHERE`, so for pre-filtered per-book retrieval prefer the
`cosine_similarity` form above.

```cypher
CALL drevo.vector.query('Chunk', 'embedding', [0.1, 0.2, 0.3], 5) YIELD node, score
RETURN node, score ORDER BY score DESC
```

### Declarative vector search — the `SEARCH` clause

The same cosine scan is also available as a declarative clause (issue #430),
mirroring Neo4j's `SEARCH … IN (VECTOR INDEX …)` form:

```
[OPTIONAL] MATCH pattern
  SEARCH binding_variable IN (
    VECTOR INDEX Label.property
    FOR query_vector
    [WHERE predicate]
    LIMIT top_k
  ) [SCORE AS score_alias]
```

`SEARCH` constrains `binding_variable` to the top-`top_k` nodes of `Label`
whose `property` embedding is closest to `query_vector`, and — with
`SCORE AS score_alias` — binds the cosine similarity (`0.0`–`1.0`, `1.0` most
similar) to `score_alias`. The optional inner `WHERE` post-filters the results
(`k`-then-filter, exactly like `drevo.vector.query`). Because the search
variable is overwritten by the results, `MATCH (m:Movie) SEARCH m IN (… LIMIT k)`
returns `k` rows, not `k × |Movie|`.

The `SEARCH` clause addresses the index as a dotted `Label.property`. A named
`CREATE VECTOR INDEX` registry — so a Neo4j-style *name* resolves to a declared
`(label, property)` — is documented just below (issue #532). `SEARCH`, `VECTOR`,
`INDEX`, `FOR`, and `SCORE` are soft keywords — a property or variable named,
say, `score` keeps working.

```cypher
MATCH (m:Movie)
SEARCH m IN (
  VECTOR INDEX Movie.plotEmbedding
  FOR [0.1, 0.2, 0.3]
  LIMIT 6
) SCORE AS score
RETURN m.title, score
```

### Neo4j-compatible index surface (#532)

drevo also accepts the Neo4j spelling, so the Neo4j GenAI stack
(`neo4j-graphrag`, `neo4j-agent-memory`, LangChain `Neo4jVector`, …) runs
against drevo unmodified. `CREATE VECTOR INDEX <name> FOR (n:Label) ON
(n.property)` registers `<name>` as an alias for that `(label, property)` pair —
the binding the dotted `SEARCH` form lacks. The `OPTIONS { … }` clause
(dimensions / similarity function) is accepted but ignored: the cosine scan
infers the dimension from the stored vectors and always scores by cosine.

```cypher
CREATE VECTOR INDEX moviePlots IF NOT EXISTS
FOR (m:Movie) ON (m.plotEmbedding)
```

`CALL db.index.vector.queryNodes(indexName, k, queryVector) YIELD node, score`
then resolves the name to its `(label, property)` and runs the same cosine scan
as `drevo.vector.query` — identical rows, Neo4j-compatible spelling:

```cypher
CALL db.index.vector.queryNodes('moviePlots', 5, [0.1, 0.2, 0.3])
YIELD node, score
RETURN node, score ORDER BY score DESC
```

`CALL db.index.fulltext.queryNodes(indexName, queryString) YIELD node, score` is
the full-text counterpart, aliasing `fts.search`. drevo keeps one global
full-text index over node title/body, so `indexName` is accepted but does not
select an index.

Relationship embeddings get the same treatment. `CREATE VECTOR INDEX <name> FOR
()-[r:TYPE]-() ON (r.property)` (either arrow direction) names a relationship
index, and `CALL db.index.vector.queryRelationships(indexName, k, queryVector)
YIELD relationship, score` cosine-ranks the edges of that type. A node index
cannot be queried with `queryRelationships` or the reverse; the error names the
right procedure.

```cypher
CREATE VECTOR INDEX similarEmbedding IF NOT EXISTS
FOR ()-[r:SIMILAR]-() ON (r.embedding)
```

```cypher
CALL db.index.vector.queryRelationships('similarEmbedding', 5, [0.1, 0.2, 0.3])
YIELD relationship, score
RETURN relationship, score ORDER BY score DESC
```

Schema DDL that drevo does not need is **accepted but a no-op**, so a driver's
bootstrap does not fail: `CREATE [RANGE|LOOKUP|FULLTEXT] INDEX …` on
top-level properties or relationships, `CREATE POINT INDEX` on one property or
on relationships, `CREATE TEXT INDEX` on relationships,
and the relationship and property-type `CREATE CONSTRAINT` forms parse and do
nothing (drevo auto-indexes every top-level node property).
`CREATE VECTOR INDEX`, indexes on nested property paths
([below](#indexes-on-nested-properties-578)), text indexes on node
properties ([below](#text-indexes-589)), point indexes on two position
properties ([below](#point-indexes)) and node constraints
([below](#constraints-604-606)) have an effect.

`SHOW [type] INDEXES` lists the named indexes in Neo4j's column layout
(`id`, `name`, `state`, `populationPercent`, `type`, `entityType`,
`labelsOrTypes`, `properties`, `indexProvider`, `owningConstraint`, `options`),
with the usual `YIELD …` / `WHERE …` / `RETURN …`, a bare `WHERE`, and
`YIELD *`. Vector indexes are listed as `VECTOR`, nested-path indexes as
`RANGE`, text indexes as `TEXT`, point indexes as `POINT`; `SHOW ALL INDEXES`
lists all of them. drevo's automatic indexes have no
names and the no-op DDL registers nothing. `SHOW CONSTRAINTS` lists the
declared UNIQUE constraints and nothing else. That is exactly what a client's
check-then-create bootstrap expects, e.g. neo4j-agent-memory's:

```cypher
SHOW INDEXES YIELD name WHERE name = $name RETURN count(*) AS count
```

```cypher
SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, properties
WHERE type = 'VECTOR'
RETURN name, entityType, labelsOrTypes, properties
```

As in Neo4j, a query that ends in `CALL … YIELD …` (or `SHOW …`) returns the
yielded columns without a `RETURN`.

`CALL fts.search(query, k) YIELD node, score` returns the top-`k` nodes
matching `query` in the BM25 full-text index (task `00131`), ranked by
relevance — the full-text counterpart of `drevo.vector.query`. The `score` is
the Okapi BM25 relevance. Post-filter with a `WHERE` on any node property and
`ORDER BY score DESC` as usual.

```cypher
CALL fts.search('anxious thoughts about work', 25) YIELD node, score
RETURN node, score ORDER BY score DESC
```

### Indexes on nested properties (#578)

Every top-level node property is indexed automatically. Values *inside* a map
property are not, so `WHERE n.meta.author = 'ann'` scans every node — unless
you declare an index on that path:

```cypher
CREATE INDEX doc_author IF NOT EXISTS FOR (n:Doc) ON (n.meta.author)
```

Index every path under a map, or the whole property map, with a wildcard. The
label is optional; without it the index covers every node:

```cypher
CREATE INDEX doc_meta FOR (n:Doc) ON (n.meta.*)
```

```cypher
CREATE INDEX everything FOR (n) ON (n.*)
```

Equality, `IN` and numeric ranges on a covered path then go through the index:

```cypher
MATCH (n:Doc)
WHERE n.meta.author = 'ann' AND n.meta.review.score >= 4
RETURN n.title
```

- **Coverage:** an index declared `FOR (n:Doc)` serves patterns that require
  `:Doc`, because only `Doc` nodes are indexed. An index without a label serves
  any pattern. A `WHERE` term under `OR` or `NOT` is not served, just as with
  top-level properties.
- **Values:** the same types the automatic index handles: strings, booleans
  and integers for equality, integers and floats for ranges. A list is a leaf;
  nothing below a list is indexed. A range over a path where some node stores
  a non-number falls back to a scan.
- **Each path stands alone:** `ON (n.a.b, n.c.*)` declares two paths. It is
  not a composite key.
- **Names:** an index without a name gets one from its label and paths
  (`index_Doc_meta_author`). Names are unique across vector, path and text
  indexes.
- **Lifecycle:** definitions are stored next to the WAL and survive a
  restart. The index data lives in memory, is rebuilt on open, and is rebuilt
  before the next statement whenever an index is created or dropped.

`DROP INDEX` removes a path, text or vector index:

```cypher
DROP INDEX doc_author IF EXISTS
```

### Text indexes (#589)

`CONTAINS`, `STARTS WITH` and `ENDS WITH` normally test every node the pattern
reaches. A text index on one property answers them from an index instead:

```cypher
CREATE TEXT INDEX ticket_summary IF NOT EXISTS FOR (t:Ticket) ON (t.summary)
```

```cypher
MATCH (t:Ticket)
WHERE t.summary CONTAINS 'timeout' AND t.summary STARTS WITH 'PROJ-'
RETURN t.title
```

The index splits each value into overlapping three-character pieces
(trigrams), case-sensitively, with markers for the start and end of the value.
A query looks up the needle's trigrams, keeps the nodes that have all of them,
and checks those nodes with the real predicate, so results are always the same
as without the index.

- **One property per index**, top-level (`t.summary`) or nested
  (`t.meta.author`); the label is optional, as with nested-path indexes, and a
  labelled index serves only patterns that require the label.
- **Needle length:** `STARTS WITH` and `ENDS WITH` use the index from one
  character; `CONTAINS` needs at least three. An empty needle matches every
  string and is answered by a scan.
- **Values:** strings are indexed; `null` and missing values never match. While
  any indexed node holds a number, list or map at that property, the index is
  not used, so the query raises the same type error it would without the
  index.
- **Common needles:** on an index of 1 024 values or more, a needle whose
  rarest trigram appears in more than half of them is answered by a scan, which
  is cheaper at that point.
- **Where it applies:** the predicate must be `property <op> needle` in a
  top-level `AND` of the `MATCH`'s `WHERE`; the needle can be a literal or a
  parameter. Under `OR` or `NOT`, or with the property as the needle
  (`'text' CONTAINS n.word`), the query scans.
- **Names and lifecycle:** an unnamed index is called
  `text_index_<label|all>_<path>` (`text_index_Ticket_summary`). Definitions
  survive a restart, the data is rebuilt in memory on open, and `DROP INDEX`
  removes it. `OPTIONS {…}` is accepted and ignored. Relationship text indexes
  are accepted as no-ops.

Full-text search (`fts.search`, `db.index.fulltext.queryNodes`) is a different
tool: it ranks whole nodes by relevance, ignores case and punctuation, and
needs no declaration.

### Point indexes

A point index keeps the position of every node with a label, read from a
latitude and a longitude property in degrees, so that
[`drevo.spatial.nearest`](#nearest-nodes-to-a-point) finds the nodes closest to
a point without reading every node:

```cypher
CREATE POINT INDEX junctions IF NOT EXISTS FOR (j:Junction) ON (j.lat, j.lon)
```

- **Two top-level properties**, the latitude first, and a label. Neo4j's form
  with one point-valued property (`ON (p.location)`) is accepted as a no-op.
- **Values:** a node is indexed when both properties hold numbers, the
  latitude in −90..90 and the longitude in −180..180. Other nodes are left
  out, as `drevo.spatial.nearest` leaves them out without the index.
- **Writes:** the index follows `CREATE`, `SET`, `REMOVE` and `DELETE`. A
  statement that has written answers from a scan, so it sees its own writes.
- **Lean databases:** a point index is not one of the
  [secondary indexes](#secondary-indexes); `setSecondary(false)` keeps it.
- **Names and lifecycle:** an unnamed index is called
  `point_index_<label>_<lat>_<lon>` (`point_index_Junction_lat_lon`).
  Definitions survive a restart, the positions are rebuilt in memory on open,
  and `DROP INDEX` removes it. `OPTIONS {…}` is accepted and ignored.

The index buckets positions into a grid of 0.002° cells (about 220 m north to
south) and reads the cells around the point ring by ring until no unread cell
can hold anything closer.

### Constraints (#604, #606)

A constraint is a rule every node with a label must follow. drevo enforces
Neo4j's three node constraints:

```cypher
CREATE CONSTRAINT customer_email IF NOT EXISTS FOR (c:Customer) REQUIRE c.email IS UNIQUE
```

```cypher
CREATE CONSTRAINT release_name FOR (r:Release) REQUIRE (r.product, r.name) IS UNIQUE
```

```cypher
CREATE CONSTRAINT ticket_status FOR (t:Ticket) REQUIRE t.status IS NOT NULL
```

```cypher
CREATE CONSTRAINT ticket_key FOR (t:Ticket) REQUIRE (t.project, t.number) IS NODE KEY
```

- **`IS UNIQUE`:** no two nodes share the value, or the combination of values
  for a list of properties. Nodes lacking the property (or any property of
  the list) are not constrained.
- **`IS NOT NULL`:** every node has the property. One property per
  constraint.
- **`IS NODE KEY`** (or `IS KEY`): every node has all the properties, and no
  two share their combination.

The Neo4j 4 spellings work too: `ON (t:Ticket) ASSERT t.status IS NOT NULL`,
`ASSERT exists(t.status)`, `ASSERT (t.project, t.number) IS NODE KEY`.
`OPTIONS {…}` is accepted and ignored.

- **When uniqueness is checked:** on every write: `CREATE`, `SET`, adding a
  label, `MERGE`'s create branch, HTTP, Python and batch writes. A violating
  write fails and changes nothing. `MERGE (c:Customer {email: …})` finds the
  existing node instead. A lookup on the values makes each check
  constant-time.
- **When presence is checked:** at the end of the statement, and at commit in
  an explicit transaction. A statement may create a node and set its required
  property later, so this works:

```cypher
CREATE CONSTRAINT ticket_status FOR (t:Ticket) REQUIRE t.status IS NOT NULL
```

```cypher
MERGE (t:Ticket {project: 'APP', number: 7})
ON CREATE SET t.status = 'new', t.title = 'Login fails'
```

  A statement that leaves a required property missing writes nothing. While
  an `IS NOT NULL` or `IS NODE KEY` constraint exists, every writing statement
  runs as one transaction; its `MATCH`es then read the graph without the
  property and label indexes. A single HTTP or Python write is checked at
  once.
- **Which nodes:** every node carrying the label, as its first label or any
  other. `null` counts as no value. `title` and `body` are covered like any
  property. Values compare by type and content, so `1` and `'1'` differ.
- **Existing data:** creating a constraint that the data already breaks fails,
  and nothing is stored.
- **Errors:** Bolt `Neo.ClientError.Schema.ConstraintValidationFailed`; HTTP 409
  with that string in the body's `code`; Python `drevo.ConstraintViolation`.
- **Names:** an unnamed constraint is called `constraint_<label>_<properties>`,
  with `_not_null` or `_node_key` appended for those kinds. Names are shared
  with indexes. Declaring the same rule again under another name is an error
  unless `IF NOT EXISTS` is given.
- **Lifecycle:** definitions are stored next to the WAL and survive a restart.
  `SHOW CONSTRAINTS` lists them with type `UNIQUENESS`,
  `NODE_PROPERTY_EXISTENCE` or `NODE_KEY`; `SHOW UNIQUE CONSTRAINTS`,
  `SHOW EXISTENCE CONSTRAINTS` and `SHOW NODE KEY CONSTRAINTS` filter them.
  `DROP CONSTRAINT` removes one:

```cypher
DROP CONSTRAINT customer_email IF EXISTS
```

Relationship constraints and property-type constraints (`IS :: INTEGER`) are
accepted and not enforced.

### Triggers

A trigger is a Cypher statement the database runs every time a transaction
commits a change. Triggers keep derived data in the database instead of in
every client: timestamps, counters, audit trails. They are installed with
APOC's `apoc.trigger` procedures:

```cypher
CALL apoc.trigger.install('drevo', 'stamp_updated_at',
  'UNWIND keys($assignedNodeProperties) AS k
   UNWIND $assignedNodeProperties[k] AS change
   WITH k, change WHERE k <> "updated_at"
   WITH DISTINCT change.node AS n WHERE n:Task
   SET n.updated_at = timestamp()',
  {phase: 'before'})
```

```cypher
CALL apoc.trigger.install('drevo', 'audit_new_tasks',
  'UNWIND $createdNodes AS n WITH n WHERE n:Task
   CREATE (:Audit {task: n.title, at: timestamp()})',
  {phase: 'after'})
```

The statement sees what the transaction changed as parameters, named and
shaped as in APOC:

| Parameter | Shape |
|-----------|-------|
| `$createdNodes`, `$deletedNodes` | list of nodes (a deleted node as it was) |
| `$createdRelationships`, `$deletedRelationships` | list of relationships |
| `$assignedLabels`, `$removedLabels` | `{label: [node, …]}` |
| `$assignedNodeProperties` | `{key: [{key, old, new, node}, …]}` |
| `$removedNodeProperties` | `{key: [{key, old, node}, …]}` |
| `$assignedRelationshipProperties` | `{key: [{key, old, new, relationship}, …]}` |
| `$removedRelationshipProperties` | `{key: [{key, old, relationship}, …]}` |

The labels and properties of a created node count as assigned, and those of a
deleted node as removed. Parameters given at install time,
`{params: {team: 'core'}}` as the fifth argument, are visible next to them.

The **phase** in the selector says when the trigger runs:

- `before` (the default) — inside the transaction, just before it commits.
  The trigger's writes commit with the transaction, and a failing trigger
  rolls the whole transaction back: the client gets
  `Neo.ClientError.Transaction.TransactionHookFailed`.
- `after` — right after the commit, in a transaction of its own. A failure
  is logged; the committed transaction stands.
- `afterAsync` — like `after`, on a background thread, so the commit does
  not wait for it.

A trigger's own writes do not fire triggers. Statements that change nothing
fire nothing. Triggers run for Cypher statements and transactions over Bolt,
HTTP, gRPC and the Python package, including two-phase commit: `before`
triggers run at prepare, `after` triggers once the transaction is committed.

| Procedure | Does |
|-----------|------|
| `apoc.trigger.install(db, name, statement, selector[, config])` | install or replace a trigger |
| `apoc.trigger.list()` | list the triggers |
| `apoc.trigger.stop(db, name)` / `apoc.trigger.start(db, name)` | pause / resume one |
| `apoc.trigger.drop(db, name)` / `apoc.trigger.dropAll(db)` | remove one / all |
| `apoc.trigger.add(name, statement, selector[, config])`, `remove(name)`, `removeAll()`, `pause(name)`, `resume(name)` | the APOC 4 forms, acting on the current database |

Each yields `name, query, selector, params, installed, paused`. `db` names
the database the statement runs on. Triggers are kept per database and
survive a restart. Changing them needs `EXECUTE ADMIN PROCEDURES`, is refused
inside an explicit transaction and on a read-only database; the trigger's
statement runs with the database's own rights, whoever committed.

```cypher
CALL apoc.trigger.list() YIELD name, selector, paused RETURN name, selector.phase, paused
```

### Sequences

A sequence hands out unique, increasing numbers for human-readable ids such
as `BUG-1042` or invoice numbers:

```cypher
CALL drevo.sequence.create('bug', {start: 1000, step: 1})
```

```cypher
CREATE (b:Bug {title: 'crash on save', key: 'BUG-' + toString(drevo.sequence.next('bug'))})
RETURN b.key
```

- `drevo.sequence.next(name)` returns the next value; `drevo.sequence.current(name)`
  the last one handed out, or `null` before the first.
- Values live outside transactions. Concurrent writers never wait for each
  other or conflict over a number, and a rolled-back transaction or a failed
  statement leaves a gap: its value is not handed out again.
- A sequence counts from `start` (default 1) by `step` (default 1; negative
  counts down).
- Sequences are kept in `sequences.json` next to the database's log. Values are
  reserved in blocks of 1000, and a restart resumes after the reserved block:
  it may skip numbers but never repeats one. Backups include the file.

| Procedure | Yields | Does |
|-----------|--------|------|
| `drevo.sequence.create(name[, {start, step}])` | `name, start, step, current` | create a sequence |
| `drevo.sequence.drop(name)` | `name, start, step, current` | remove it (no row when there was none) |
| `drevo.sequence.list()` | `name, start, step, current` | every sequence, by name |

Creating and dropping need `EXECUTE ADMIN PROCEDURES` and are refused inside an
explicit transaction; on a read-only database no value is handed out.

```cypher
CALL drevo.sequence.list() YIELD name, current RETURN name, current
```
### Secondary indexes

Besides adjacency, ids and labels, a database keeps three indexes of its own
for every node and relationship: a property-value index (fast `{key: value}`
and range lookups), a value cache, and full-text indexes over every string
(`fts.search`, `db.index.fulltext.*`). They can take more memory than the
graph itself. A database that does not need them — a road network that is
only routed over, say — can turn them off:

```cypher
CALL drevo.indexes.setSecondary(false) YIELD enabled RETURN enabled
```

Their memory is freed before the next statement. Queries stay correct:
lookups by property scan the label or the graph instead, and the full-text
procedures report that they are unavailable. Ids, adjacency, labels,
`drevo.path.weighted` and the indexes you declare (`CREATE INDEX`,
`CREATE TEXT INDEX`, `CREATE POINT INDEX`, vector indexes) keep working. The choice is kept across
restarts; `CALL drevo.indexes.setSecondary(true)` rebuilds the indexes from
the graph, and `CALL drevo.indexes.secondary()` reports the setting. Changing
it needs index management rights and is refused in an explicit transaction
and on a read-only database.

On the Sofia road graph (242,947 junctions, 281,421 roads with two numbers
each) the engine holds about 745 MB of heap with the secondary indexes and
about 256 MB without; the admin guide's *Memory footprint* section breaks
the numbers down.

### Fuzzy matching (#593)

Find values that are close to a string rather than equal to it — typos,
variant spellings, near-duplicate titles. The edit distance is the fewest
single-character insertions, deletions and substitutions between two strings:

```cypher
RETURN apoc.text.levenshteinDistance('recieve', 'receive') AS edits,
       apoc.text.levenshteinSimilarity('abcd', 'abed') AS similarity
```

The functions work anywhere, but filtering with them computes the distance for
every row. `drevo.text.fuzzy` searches a [text index](#text-indexes-589)
instead:

```cypher
CREATE TEXT INDEX bug_summary IF NOT EXISTS FOR (b:Bug) ON (b.summary)
```

```cypher
CALL drevo.text.fuzzy('bug_summary', 'login fails on Safary', 2)
YIELD node, distance
RETURN node.summary, distance
```

It returns every node the index covers whose value is within `maxEdits` edits
of `query`, nearest first. One edit changes at most three of a value's
trigrams, so a match must share all but `3 × maxEdits` of the query's
trigrams. The index counts shared trigrams to pick candidates, and only those
get the exact distance. When that bound rules nothing out (a query of a few
characters, or a large `maxEdits`), every indexed value is checked. Results
are exact either way, including after a write earlier in the same statement.
Arguments are literals or parameters; `maxEdits` must be a non-negative
integer.

### Server info (#303)

`CALL drevo.info() YIELD version, git_sha, build_date, protocol` returns one row
of build metadata so a Bolt client can learn which drevo it is connected to and
fail fast against an incompatible server. It is read-only, takes no arguments,
needs no auth, and has no side effects — safe to call on connect. `version` is
the running build's semver (bare, e.g. `0.0.16`, no leading `v`); `git_sha` and
`build_date` are `null` when the build did not capture them (a plain
`docker build` without the release build-args, for instance); `protocol` is a
coarse capability integer clients can gate on without parsing semver. The YIELD
contract is stable across versions, and a client that finds the procedure absent
(an older drevo) should fall back gracefully.

```cypher
CALL drevo.info() YIELD version, git_sha, build_date, protocol
RETURN version, git_sha, build_date, protocol
```

### Engine status (RFC #307)

`CALL drevo.engine.status()` reports which engine serves this database's
Cypher and, when the server runs with `DREVO_ENGINE=native`, the read
mirror's live routing counters — the soak signal for flipping the engine
default. `engine` is `"kv"` (every query on the storage engine; the five
counter columns are `null`) or `"native"`; `mirror_fresh` says whether the
mirror's snapshot matches the store's mutation epoch right now;
`native_hits` / `kv_fallbacks` / `kv_routed` count reads served natively,
reads that fell back to KV while the mirror was stale, and queries (writes,
non-mirrorable procedures — this call included) routed to KV by design;
`rebuild_errors` counts failed background rebuilds. Read-only, no
arguments, no side effects.

```cypher
CALL drevo.engine.status() YIELD engine, mirror_fresh, native_hits
RETURN engine, mirror_fresh, native_hits
```

### Agent memory (#533)

Server-side agent memory: a session's turns stored as
`:Message { id, session, seq, role, text, body, created_at }` nodes chained by
`:NEXT` (the schema the agent-memory MCP writes, with `body = text` so the
full-text index reaches them), plus `:ReasoningTrace` nodes recording why the
agent did something. These procedures put writing and recalling memory one
Bolt/HTTP call away for any client, not just the MCP.

`CALL drevo.memory.addMessage(session, role, text) YIELD node` appends a turn:
it gets `seq = latest + 1` (1 for a new session), `body = text`, an `id` and a
`created_at`, and a `:NEXT` edge from the session's previous newest message.

```cypher
CALL drevo.memory.addMessage('session-1', 'user', 'the deploy port is 7688')
YIELD node
RETURN node.seq, node.text
```

`CALL drevo.memory.recordReasoning(session, step, tool, outcome) YIELD node`
records a decision as `:ReasoningTrace { id, session, step, tool, outcome,
created_at }` linked `:INITIATED_BY` to the session's newest message (no edge
when the session has none). `tool` and `outcome` may be `null`.

```cypher
CALL drevo.memory.recordReasoning('session-1', 'chose port 7688 for Bolt', null, 'ok')
YIELD node
RETURN node.step
```

Both write through the regular `CREATE` path, so the usual mutation counters
are reported, and a registered `'auto'` `Message` semantic target (see below)
embeds each turn as it is written.

`CALL drevo.memory.recall(session, query, k) YIELD node, score` returns the
top-`k` messages of `session` whose text best matches `query` by BM25,
best-first — "what did *this* conversation say about X?". It reuses the
full-text index, then keeps only messages in `session`.

```cypher
CALL drevo.memory.recall('session-1', 'deploy port', 5)
YIELD node, score
RETURN node.text, score ORDER BY score DESC
```

`CALL drevo.memory.getConversation(session, limit) YIELD node` replays the most
recent `limit` messages of `session` oldest-first (by `seq`) — the transcript to
reload as context.

```cypher
CALL drevo.memory.getConversation('session-1', 50) YIELD node
RETURN node.seq, node.role, node.text ORDER BY node.seq
```

BM25 only finds messages that share a token with the query. To recall by
**meaning** — a paraphrase like "what did we deploy?" finding "we shipped the
release" — rank the session's messages by cosine similarity instead.
`CALL drevo.memory.recallVector(session, vector, k) YIELD node, score` takes a
query vector you embedded yourself:

```cypher
CALL drevo.memory.recallVector('session-1', [0.1, 0.9, 0.2], 5)
YIELD node, score
RETURN node.text, score ORDER BY score DESC
```

`CALL drevo.memory.recallSemantic(session, text, k) YIELD node, score` embeds the
query text on the server (the same embedder as `drevo.semantic.query`), so a
client needs no embedder of its own. Without one configured it fails with an
engine-capability error — fall back to `drevo.memory.recall`.

```cypher
CALL drevo.memory.recallSemantic('session-1', 'what did we deploy?', 5)
YIELD node, score
RETURN node.text, score ORDER BY score DESC
```

Both read each message's vector from the `embedding_property` of a registered
`Message` semantic target, else from `embedding`; messages without one are
skipped. Registering the target in `'auto'` mode on the `text` property embeds
every new message as it is written, so the memory chain becomes semantically
searchable with no client work:

```cypher
CALL drevo.semantic.register('Message', 'text', 'embedding', 'auto')
YIELD label, state RETURN label, state
```

The recall and replay procedures are read-only; only `addMessage` and
`recordReasoning` write.

#### Long-term memory: entities and facts over time

The long-term layer holds what the agent knows about the world, in the schema
neo4j-agent-memory uses: POLE+O entities (`PERSON`, `OBJECT`, `LOCATION`,
`EVENT`, `ORGANIZATION`) and facts between them that carry a validity window.

`CALL drevo.memory.rememberEntity(session, name, type, description) YIELD node`
upserts `:Entity:<Type> { id, name, type, description, created_at }` keyed on
`(name, type)`: a known entity is reused and its description refreshed when a
new one is given (a `null` description never erases one). When `session` has
messages, its newest one gets a `:MENTIONS` edge to the entity (never twice), so
a conversation's context is `MATCH (:Message {session: $s})-[:MENTIONS]->(e)`.
`session` may be `null`; `type` is case-insensitive and must be one of the five.

```cypher
CALL drevo.memory.rememberEntity('session-1', 'Dana', 'person', 'platform engineer')
YIELD node
RETURN node.name, node.type, labels(node)
```

`CALL drevo.memory.assertFact(subject, relation, object, exclusive) YIELD rel`
records that a fact holds from now, as
`(subject)-[:RELATED_TO { id, type: relation, valid_from, created_at }]->(object)`
between two remembered entities (by name). Re-asserting a fact that holds is a
no-op. With `exclusive = true` the subject's other current facts of the same
relation are closed first (`valid_until = now`) — "works at Globex" ends "works
at Acme" but keeps it as history. `CALL drevo.memory.retractFact(subject,
relation, object) YIELD rel` ends a fact the same way.

```cypher
CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null) YIELD node AS dana
CALL drevo.memory.rememberEntity(null, 'Acme', 'ORGANIZATION', null) YIELD node AS acme
CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', true) YIELD rel
RETURN rel.type, rel.valid_from, rel.valid_until
```

`CALL drevo.memory.factsAt(name, asOf) YIELD subject, relation, object,
valid_from, valid_until, source, quote` answers "what did memory hold about this entity at
that time?": every fact touching the entity, either direction, with
`valid_from <= asOf` and `valid_until` unset or later. `asOf` is an ISO-8601
UTC timestamp in the form `datetime()` returns, or `null` for now. `source`
and `quote` are the fact's citation (see Provenance below), `null` when it has
none.

```cypher
CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null) YIELD node
CALL drevo.memory.factsAt('Dana', null)
YIELD subject, relation, object, valid_from, valid_until
RETURN subject.name, relation, object.name, valid_from, valid_until
```

Unlike the reference library, where `valid_until` is written but never set,
drevo closes superseded facts and filters on the window, so "as of" questions
get real answers.

`rememberEntity` keys on the exact name by default. Its optional fifth
argument — the map that also carries provenance (below) — takes `resolve` to
reuse an entity known under another spelling: `'exact'` (the default) as
above; `'alias'` also reuses the entity of the same type an `:Alias` with
the same normalised name points at (see
[Entity resolution](#entity-resolution)); `'fuzzy'` then compares the name
with every entity of the type and reuses the one scoring at least
`threshold` (default 0.9). When several score that high, or none does, a
new entity is created with `-[:POSSIBLY_SAME {score}]->` the ones at or
above the threshold, or else the closest (up to five) within 0.1 below it,
for a later review. `threshold` is only accepted with `resolve: 'fuzzy'`.

```cypher
CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', 'poet') YIELD node AS poet
CALL drevo.memory.rememberEntity(null, 'Пушкину', 'PERSON', null, {resolve: 'fuzzy'})
YIELD node
RETURN node.name, id(node) = id(poet)
```

#### Provenance: sources, chunks and quotes

Long-term memory can record where it read something, so an answer can show
the passage behind a fact. A `:Source {id, uri, title, kind, created_at}` is a
document, URL, message or file; its passages are
`:Chunk {id, source_id, order, offset, length, text, body, created_at}` nodes,
each `-[:FROM]->` its source and chained `-[:NEXT]->` in reading order. `body`
repeats `text`, so `fts.search` finds chunks, and
`CALL drevo.semantic.register('Chunk', 'text', 'embedding', 'auto')` makes
them searchable by meaning.

`CALL drevo.memory.addSource(id, uri, title, kind) YIELD node` upserts a
source keyed on `id` (a `null` field keeps the stored value).
`CALL drevo.memory.addChunks(sourceId, chunks) YIELD node` upserts its chunks:
each entry is `{id, order, offset, length, text}`, where `order` and `text` are
required, `offset` / `length` place the passage in the source (in characters),
and a missing `id` becomes `<sourceId>#<order>`. Gaps in `order` are allowed,
the `NEXT` chain is rebuilt when a chunk lands between two others, and adding
the same chunks again changes nothing. The chunks are yielded in reading
order; an unknown source, or a chunk id that belongs to another source, is an
error.

```cypher
CALL drevo.memory.addSource('book-1', 'file:///books/one.md', 'Book One', 'document')
YIELD node AS source
CALL drevo.memory.addChunks('book-1', [
  {order: 0, offset: 0, text: 'Dana opened the manuscript at dawn.'},
  {order: 1, offset: 36, text: 'By noon Dana had joined Acme as an editor.'}
]) YIELD node
RETURN node.id, node.order, node.text
```

`rememberEntity` and `assertFact` take an optional fifth argument, a
`provenance` map `{source, chunk, quote, offset, length, surface}`; for
`rememberEntity` the same map also carries the resolution keys `resolve` and
`threshold` described above. `source`
and `chunk` are either an id, which must exist, or a map that is upserted by
its `id` (`{id, uri, title, kind}` and `{id, order, offset, length, text}`; a
chunk map needs a source). A chunk named by id brings its own source. When
the chunk's text is known, `quote` (and a given `surface`) must occur in it,
which is what makes the citation trustworthy; `offset` and `length` default
to where it occurs, in characters, so `substring(chunk.text, offset, length)`
is the quote. When an `offset` is given with a quote (or a given `surface`),
the text at that character offset must be exactly the quote (`length`, when
given, must be its length); a mismatch is an error naming the chunk and the
offset. A fact then carries `source_id, chunk_id, quote, offset,
length`; an entity gets `-[:MENTIONED_IN {offset, length, surface}]->` the
chunk, once per place (`surface` defaults to the quote, then the entity's
name). Re-asserting a fact that holds without a citation adds this one.

```cypher
CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null,
  {source: {id: 'book-1', title: 'Book One'},
   chunk: {id: 'book-1#1', order: 1, text: 'By noon Dana had joined Acme as an editor.'}})
YIELD node AS dana
CALL drevo.memory.rememberEntity(null, 'Acme', 'ORGANIZATION', null) YIELD node AS acme
CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false,
  {chunk: 'book-1#1', quote: 'joined Acme'})
YIELD rel
RETURN rel.source_id, rel.chunk_id, rel.quote, rel.offset, rel.length
```

`CALL drevo.memory.evidence(name, k) YIELD chunk, quote, offset, source, fact`
lists the passages behind an entity: chunks that back one of its facts (either
direction; `fact` is the relationship, `quote` / `offset` its citation) and
chunks that only mention it (`fact` is `null`, `quote` the surface at its
first place). Each chunk appears once per fact and a chunk backing a fact is
not repeated as a mention. Newest chunks come first; at most `k` rows.

```cypher
CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null,
  {source: {id: 'book-1', title: 'Book One'},
   chunk: {id: 'book-1#0', order: 0, text: 'Dana opened the manuscript at dawn.'}})
YIELD node
CALL drevo.memory.evidence('Dana', 5) YIELD chunk, quote, source, fact
RETURN chunk.id, quote, source.title, fact
```

### Named events

`CALL drevo.events.post(name, payload) [YIELD name]` posts a named event for
the subscribers of [`GET /events`](admin-guide.md#named-events-drevoeventspost),
once per row; the arguments may use the row's variables, and the payload is
any value but a node, relationship or path (`null` for none). The event is
published when the statement, or its explicit transaction, commits; a failed
statement or a rolled-back transaction posts nothing. The rows pass through
unchanged.

```cypher
CREATE (b:Bug {key: 'BUG-12', status: 'reopened'})
WITH b
CALL drevo.events.post('bug_reopened', {bug: b.key}) YIELD name
RETURN b.key, name
```

### Explicit transactions

An explicit transaction is a group of statements that commit or roll back
together: Bolt `BEGIN` … `COMMIT` (which is how the Neo4j drivers run
`session.execute_read` / `execute_write`) or a Python `Transaction`. Its
statements read a snapshot taken at `BEGIN` plus its own writes, and nobody
else sees those writes until `COMMIT`.

Inside one, everything a statement can do in auto-commit mode works,
including full-text, vector and fuzzy search, `keywords()`, `SHOW INDEXES`,
`SHOW CONSTRAINTS` and auto-embedding of new nodes. Two exceptions:

- **Full-text search covers committed data only.** `fts.search`,
  `fts.searchRelationships`, `db.index.fulltext.queryNodes`,
  `drevo.search.hybrid` and `drevo.memory.recall` work until the transaction writes. After that they
  fail with `Neo.ClientError.Transaction.ForbiddenDueToTransactionType`,
  because the index cannot rank uncommitted writes. Search before writing, or
  after `COMMIT`. A hit committed by someone else after `BEGIN` is left out,
  since it is not in the snapshot. The same rule applies to one auto-commit
  statement that writes and then searches.
- **Schema commands run in auto-commit mode only.** `CREATE` / `DROP INDEX`,
  `CREATE VECTOR INDEX`, `CREATE` / `DROP CONSTRAINT`,
  `drevo.semantic.register`, `registerRel`, `reindex` and `reindexRel` take
  effect at once and cannot be rolled back. Inside an explicit transaction they
  fail with the same code, and the transaction is rolled back.

A read in a managed transaction, for example with the Python driver:

```python
with driver.session() as session:
    rows = session.execute_read(
        lambda tx: tx.run(
            "CALL db.index.fulltext.queryNodes('docs', $q) YIELD node, score "
            "RETURN node.title AS title, score", q="graph"
        ).data()
    )
```

### Concurrency

Every statement, and every explicit transaction, works on one **snapshot** of
the graph: the state as of its start, plus its own writes. A traversal never
sees half of someone else's change, and the rows a statement returns are
consistent with each other even while other clients write.

Writers never wait for each other. There are no row locks, no lock queue and
therefore **no deadlocks**: the cross-order update that PostgreSQL has to break
with its deadlock detector (A updates row 1 then row 2, B updates row 2 then
row 1) ends in drevo with one transaction committed and the other told to
retry, in under a millisecond, with nothing blocked in between.

Instead, a conflict is detected **at commit**. An explicit transaction
commits unless a transaction that committed after its `BEGIN`

- wrote a node or relationship it also wrote (created, changed or deleted), or
- deleted a node that a relationship it wrote starts or ends at, or the other
  way round.

Two transactions that touched different records both commit, whatever order
they finish in. A conflicting one is rolled back, and the client gets a
retryable error:

- Bolt: `Neo.TransientError.Transaction.Outdated`, message *transaction
  conflict: the graph changed since the transaction began; retry*;
- Python (`drevo`): `TransactionConflict`.

This is **snapshot isolation**. No update is ever lost, and unique and
existence constraints are checked again on the combined result: when two
transactions each claim the same unique value, the second gets the retryable
conflict, and its retry gets the constraint error. What snapshot isolation
allows is *write skew*: two transactions that each read what the other writes
can both commit. If an application keeps a rule across several nodes — "at
least one on-call engineer per team", where each transaction reads the team
and takes one engineer off call — ask for **serializable** isolation, under
which a transaction commits only if nothing at all was committed since it
began:

```python
from neo4j import unit_of_work

@unit_of_work(metadata={"drevo.isolation": "serializable"})
def take_off_call(tx, name):
    ...

with driver.session() as session:
    session.execute_write(take_off_call, "alice")
```

The server-wide default is `DREVO_TX_ISOLATION` (`snapshot` or
`serializable`, see the [Admin Guide](admin-guide.md)); a transaction asks for
its own with `drevo.isolation` in its metadata, which the Neo4j drivers send
with `BEGIN`. Keep transactions short either way, and let the driver retry.

The Neo4j drivers retry `TransientError`s on their own inside a managed
transaction, with exponential back-off. The loser of a conflict therefore
commits on its second attempt without any code in the application:

```python
def bump(tx, first, second):
    tx.run("MATCH (t:Ticket {id: $id}) SET t.touches = t.touches + 1", id=first)
    tx.run("MATCH (t:Ticket {id: $id}) SET t.touches = t.touches + 1", id=second)

with driver.session() as session:
    session.execute_write(bump, 1, 2)   # retried for you on Transaction.Outdated
```

A hand-rolled `session.begin_transaction()` gets the error as a
`neo4j.exceptions.TransientError` and has to retry itself. The same error
inside a prepared two-phase transaction cannot happen: `drevo.tx.prepare` runs
the conflict check, and until the transaction is resolved the fence it raises
holds back every other commit that writes what it wrote or would break a
constraint once it commits. Commits on other records go through.

**Auto-commit statements** are each their own transaction, and the server
retries one that lost a conflict itself, up to eight times, before reporting
it. From the client's point of view they never fail with a conflict. HTTP
`POST /cypher` is always auto-commit. A counter updated by many clients at once
is therefore best bumped in one statement:

```cypher
MATCH (t:Ticket {id: 7})
SET t.touches = coalesce(t.touches, 0) + 1
RETURN t.touches
```

For unique increasing numbers under contention use a [sequence](#sequences):
`drevo.sequence.next` hands out a number per call without a transaction that
can be rolled back by a conflict.

**Timeouts and tools.** A statement or transaction that runs too long is
stopped by the server's `DREVO_QUERY_TIMEOUT_MS` or the client's own
`tx_timeout`, and a connection that opens a transaction and falls silent is
closed by `DREVO_IDLE_SESSION_TIMEOUT_MS` (see the
[Admin Guide](admin-guide.md#client-timeouts)). `SHOW TRANSACTIONS` and
`TERMINATE TRANSACTIONS`, below, show and stop what runs now.

**Coming from PostgreSQL**

| PostgreSQL | drevo |
|---|---|
| row locks; writers block each other | snapshots; writers never wait |
| `deadlock_timeout`, error `40P01 deadlock_detected` | no deadlocks; the later commit fails with `Neo.TransientError.Transaction.Outdated` |
| `40001 serialization_failure` under `REPEATABLE READ` / `SERIALIZABLE` | the same `Transaction.Outdated` |
| `REPEATABLE READ` (snapshot isolation) | the default, `DREVO_TX_ISOLATION=snapshot` |
| `SERIALIZABLE` | `DREVO_TX_ISOLATION=serializable`, or `drevo.isolation` in a transaction's metadata |
| `READ COMMITTED` default | every statement reads one snapshot |
| `statement_timeout` | `DREVO_QUERY_TIMEOUT_MS` (server) and the driver's `tx_timeout` (client) |
| `idle_in_transaction_session_timeout` | `DREVO_IDLE_SESSION_TIMEOUT_MS` |
| `pg_stat_activity` | `SHOW TRANSACTIONS`, `SHOW CONNECTIONS` |
| `pg_cancel_backend`, `pg_terminate_backend` | `TERMINATE TRANSACTIONS` |
| `PREPARE TRANSACTION` / `COMMIT PREPARED` | `drevo.tx.prepare` / `drevo.tx.commitPrepared` (next section) |
| `SAVEPOINT` | none; after a failed statement the transaction is rolled back, not resumed |
| `SELECT … FOR UPDATE`, advisory locks | none; use one auto-commit statement or a sequence for a hot spot |

### Two-phase commit (#556)

drevo can take part in a distributed transaction run by an external
coordinator (design: [RFC](rfc-two-phase-commit.md)). Inside an explicit Bolt
transaction, `drevo.tx.prepare` validates the transaction and durably records
it as *prepared*. It then has to be resolved by `commitPrepared` or
`rollbackPrepared`, from any session and even after a restart:

```cypher
CALL drevo.tx.prepare($gid)          // inside BEGIN … ; YIELD gid
```

The driver's closing `COMMIT` / `ROLLBACK` after a prepare is acknowledged and
changes nothing. To resolve the transaction:

```cypher
CALL drevo.tx.commitPrepared($gid)   // or drevo.tx.rollbackPrepared($gid); YIELD gid
```

To find in-doubt transactions:

```cypher
CALL drevo.tx.listPrepared() YIELD gid, preparedAt, opCount
```

An operator can override an abandoned transaction without its coordinator. The
outcome is remembered, so a late `commitPrepared` fails with a message saying
the transaction was rolled back heuristically, instead of the coordinator
believing it never existed:

```cypher
CALL drevo.tx.heuristicRollback($gid)   // YIELD gid
```

`preparedAt` is ISO-8601 UTC. While any transaction is prepared, **every other
write fails fast** with a retryable error:

- Bolt: `Neo.TransientError.Transaction.LockAcquisitionTimeout`;
- HTTP: 503;
- Python: `TransactionConflict`.

Reads keep working, in auto-commit and in explicit transactions: `BEGIN`,
`MATCH … RETURN …`, `COMMIT` succeeds, because a transaction that wrote nothing
has nothing to hold back. A prepared transaction is never resolved
automatically; see the [Admin Guide](admin-guide.md) for the alert and HTTP
routes.

`commitPrepared`, `rollbackPrepared`, `heuristicRollback` and `listPrepared`
run in auto-commit mode only. Resolving takes effect at once and cannot be
undone by the caller's own rollback, so inside an explicit transaction they
fail with `Neo.ClientError.Transaction.ForbiddenDueToTransactionType` (Python:
`CypherError`), and the transaction is rolled back.

### Running transactions

`SHOW TRANSACTIONS` lists what runs now on every database of the server: each
running statement, and each open explicit transaction, with Neo4j's columns:

- `database`, `transactionId` (`<database>-transaction-<n>`), `username`,
  `clientAddress`, `connectionId` (the Bolt connection, as `SHOW CONNECTIONS`
  lists it);
- `currentQuery` (null for an explicit transaction between statements);
- `startTime` (ISO 8601, UTC), `elapsedTime` (ISO 8601 duration), `status`
  (`Running`, `Idle` or `Terminated`);
- `protocol` (`bolt`, `http`, `grpc` or `flight`), `isExplicit`,
  `elapsedTimeMillis`, `currentQueryElapsedTimeMillis`;
- `bufferedWrites`: how many writes an explicit transaction holds as of its
  last statement (null for a single statement). One transaction may hold up
  to `DREVO_MAX_TX_WRITES` (1,000,000 by default); the statement that goes
  over fails with `Neo.ClientError.General.TransactionOutOfMemoryError` and
  the transaction is rolled back.

The statement itself is listed too.

```cypher
SHOW TRANSACTIONS
YIELD transactionId, username, bufferedWrites
WHERE bufferedWrites > 100000
RETURN transactionId, username, bufferedWrites
```

```cypher
SHOW TRANSACTIONS
YIELD transactionId, username, currentQuery, elapsedTimeMillis
WHERE elapsedTimeMillis > 10000
RETURN transactionId, username, currentQuery
```

`TERMINATE TRANSACTIONS` stops them by id: a string, several, or a list
parameter.

```cypher
TERMINATE TRANSACTIONS 'drevo-transaction-42', 'drevo-transaction-43'
```

It answers one row per id: `transactionId`, `username` and `message`
(`Transaction terminated.` or `Transaction not found.`).

- A running statement stops at its next cancellation check, like a statement
  over the timeout. Its client gets `Neo.ClientError.Transaction.Terminated`,
  which drivers do not retry.
- An explicit transaction refuses its next statement and its commit with the
  same code, and is rolled back: none of its writes stay.
- An auto-commit statement leaves none of its writes: like any failing
  statement, it is rolled back as a whole.

With logins required, users see and stop their own transactions. Someone
else's needs `TRANSACTION MANAGEMENT` for that user on that database, which
`admin` holds:

```
GRANT TRANSACTION MANAGEMENT (*) ON DATABASE * TO ops
```

### Connections

`SHOW CONNECTIONS` lists the server's open client connections, including idle
ones that run nothing and so do not show in `SHOW TRANSACTIONS`:

- every Bolt connection, from its handshake until it closes, as `bolt-<n>`;
- every signed-in Web UI session, as `http-<n>` (HTTP itself keeps no
  connection open between requests).

Columns: `connectionId`, `connectTime` (ISO 8601, UTC), `connector` (`bolt`
or `http`), `username`, `userAgent` (the driver's, from `HELLO`) and
`clientAddress`.

```cypher
SHOW CONNECTIONS
YIELD connectionId, userAgent, connectTime
WHERE userAgent STARTS WITH 'neo4j-python'
RETURN connectionId, connectTime
```

`TERMINATE CONNECTIONS` closes them by id. A Bolt connection is closed and its
open transaction rolled back (a running statement stops); a Web UI session is
logged out. It answers one row per id: `connectionId`, `username` and
`message` (`Connection found` or `No connection found`).

```cypher
TERMINATE CONNECTIONS 'bolt-7', 'http-3'
```

With logins required, users see and close their own connections; everyone's
needs `ALL DBMS PRIVILEGES`, which `admin` holds.

Without it, another user's transaction is not listed and reads as not found.

The Web UI's Storage panel (⛁) lists the running transactions, refreshed every
few seconds, with a Terminate button on each.

### Semantic-index control plane (#251, Phase 21)

`CALL drevo.semantic.register(label, text_property, embedding_property, mode)`
registers a **server-side auto-embedding** target: nodes of `label` have the
text in `text_property` embedded into `embedding_property`. `mode` is `'auto'`
or `'manual'`. `text_property` may be any string property or the node's own
`title` / `body` field; a placeholder title drevo generated for an untitled
node is never embedded. It yields the target's control-plane record — `label`,
`text_property`, `embedding_property`, `state`, `mode`:

```cypher
CALL drevo.semantic.register('Entity', 'summary', 'embedding', 'auto')
YIELD label, state RETURN label, state
```

`CALL drevo.semantic.status()` yields one row per registered target, so a
client can introspect the capability — detect that server-side auto-embedding is
available and branch, falling back to an external embedder when the procedure is
absent. Each row carries the control-plane columns plus live **health** signals
(`pending_count`, `failed_count`, `last_error`):

```cypher
CALL drevo.semantic.status()
YIELD label, embedding_property, state, pending_count, failed_count, last_error
RETURN label, embedding_property, state, pending_count, failed_count, last_error
```

Auto-embed is fail-open — a transient embedder outage never fails a write, so a
`CREATE` can succeed with no embedding written. These columns make that
observable so `drevo.semantic.query` doesn't silently under-return:

- `pending_count` — `'auto'` nodes of the label that still lack an embedding
  (a live backlog; drain it with `drevo.semantic.reindex`). Always `0` for
  `'manual'` targets, which drevo does not embed.
- `failed_count` / `last_error` — a cumulative tally of swallowed embed failures
  and the most recent reason (or `null`).
- `state` reads `'degraded'` whenever `pending_count > 0`, and returns to
  `'enabled'` once the backlog is drained.

So a client can distinguish "fully embedded" (`pending_count = 0`) from "writes
landed but embeddings are missing", and repair via `reindex`.

`CALL drevo.semantic.query(label, property, text, k) YIELD node, score` is
`drevo.vector.query` with the embedding step folded into the server: the third
argument is a **query string**, not a vector. drevo embeds it server-side (via
the configured embeddings upstream) and returns the top-`k` nodes of `label`
ranked by cosine similarity between their `property` embedding and that query
embedding — so a Bolt/Cypher client can run semantic retrieval without hosting
an embedder of its own:

```cypher
CALL drevo.semantic.query('Chunk', 'embedding', 'anxious thoughts about work', 5)
YIELD node, score RETURN node.title, score ORDER BY score DESC
```

It requires the server to be built with the `embeddings-proxy` feature and
started with `DREVO_EMBEDDINGS_UPSTREAM` set (the same configuration that powers
`POST /v1/embeddings`); otherwise the call fails with a clear "embeddings
backend not configured" error, so a client can catch it and fall back to an
external embedder plus `drevo.vector.query`.

**Filtered semantic retrieval — `drevo.semantic.embed` (#272).** `semantic.query`
fuses embed + scan but is **unfiltered**: it cannot scope results by tenant,
entity type, or edge endpoints. When you need a filter, use
`CALL drevo.semantic.embed(text) YIELD vector` — it embeds one string
server-side (same model/dimension as the index, per `drevo.semantic.info`) and
returns the vector, so you run your **own** Cypher, with any `WHERE`, against
`cosine_similarity` (see above):

```cypher
CALL drevo.semantic.embed('anxious thoughts about work') YIELD vector
WITH vector
MATCH (n:Chunk)
WHERE n.group_id IN ['g1'] AND n.embedding IS NOT NULL
WITH n, cosine_similarity(n.embedding, vector) AS score
WHERE score > 0.5
RETURN n.title, score ORDER BY score DESC LIMIT 5
```

So a Bolt/Cypher client gets filter/group-scoped semantic search without hosting
an embedder of its own. Same `embeddings-proxy` requirement and same clear "not
configured" error as `semantic.query`, so the same fallback applies.

**Auto-embedding on ingest.** When a target is registered in `'auto'` mode and
the server has an embedder configured, drevo embeds `text_property` into
`embedding_property` **automatically as matching nodes are created or updated** —
no client round-trip. So the full loop needs no client-side embedding at all.
First register the target:

```cypher
CALL drevo.semantic.register('Doc', 'text', 'embedding', 'auto')
YIELD label, state RETURN label, state
```

Then ordinary writes are embedded server-side — this `CREATE` populates
`embedding` on its own:

```cypher
CREATE (:Doc {title: 'note-1', text: 'anxious thoughts about work'})
```

and the node is immediately retrievable by query text:

```cypher
CALL drevo.semantic.query('Doc', 'embedding', 'work stress', 5)
YIELD node, score RETURN node.title, score ORDER BY score DESC
```

Auto-embedding is a no-op unless an embedder is configured and an `'auto'`
target matches, so ordinary writes are unaffected. It embeds `text_property` on
each create/update (an update re-embeds only when that text changes); a `'manual'`
target is never auto-embedded, and existing nodes created before registration
keep their current embeddings until next written. An upstream failure is logged
and swallowed — it never fails the write.

**Backfilling existing nodes.** Because auto-embed fires only on create/update,
registering a rule against an already-populated label leaves those rows
un-embedded. `CALL drevo.semantic.reindex(label, embedding_property, batch_size)`
backfills them in resumable batches:

```cypher
CALL drevo.semantic.reindex('Doc', 'embedding', 128)
YIELD scanned, embedded, skipped, remaining
RETURN scanned, embedded, skipped, remaining
```

It embeds up to `batch_size` of the still-un-embedded `label` nodes and returns
the counts; a client loops while `remaining > 0`. It is idempotent (nodes that
already carry `embedding_property` are `skipped`, so re-running is cheap) and
resumable across calls. Nodes without a non-empty `text_property` are skipped; a
`'manual'` target is a no-op (all-zero counts); and it errors if no target is
registered for `(label, embedding_property)`.

### Capability introspection (#267)

`CALL drevo.semantic.info() YIELD embedder_present, model, dimension, upstream`
reports the server-side embedder's capability so a client can verify
consistency before trusting — or mixing in its own — vectors:

```cypher
CALL drevo.semantic.info()
YIELD embedder_present, model, dimension, upstream
RETURN embedder_present, model, dimension, upstream
```

- `embedder_present` — whether an embedder is actually installed and configured
  (not merely whether the procedures exist); `false` on a build without
  `embeddings-proxy` or with `DREVO_EMBEDDINGS_UPSTREAM` unset.
- `model` — the configured embedding model id.
- `dimension` — the vector length (discovered by a one-off probe and cached), so
  a client can validate its own vectors / index config before mixing a native
  and a fallback embedder.
- `upstream` — the configured endpoint URL. **Never** includes the API key.

All fields but `embedder_present` are `null` when no embedder is configured.

### Relationships (#266)

Every semantic procedure has a **relationship** mirror that matches an edge's
type (`Edge::kind`) instead of a node label — the same split as `fts.search` vs
`fts.searchRelationships`. A rule registered with `registerRel` auto-embeds
matching relationships on create/update, and `queryRel` retrieves them:

```cypher
CALL drevo.semantic.registerRel('RELATES_TO', 'fact', 'fact_embedding', 'auto')
YIELD label, state RETURN label, state
```

```cypher
CALL drevo.semantic.queryRel('RELATES_TO', 'fact_embedding', 'who mentors whom', 5)
YIELD rel, score RETURN rel, score ORDER BY score DESC
```

```cypher
CALL drevo.semantic.reindexRel('RELATES_TO', 'fact_embedding', 128)
YIELD scanned, embedded, skipped, remaining
RETURN scanned, embedded, skipped, remaining
```

`registerRel` / `queryRel` / `reindexRel` behave exactly like their node
counterparts (`register` / `query` / `reindex`) — same modes, fail-open
auto-embed, skip-unchanged, and batched/idempotent backfill. `drevo.semantic.status`
lists node **and** relationship targets, with a `target_kind` column
(`node` / `relationship`) distinguishing them.

### Hybrid search (#780)

`drevo.search.hybrid` ranks nodes by keywords and by meaning in one call. It
runs the BM25 ranking of `fts.search` and the cosine ranking of
`drevo.semantic.query` (the query text embedded on the server), each a few
dozen candidates deep, and fuses them with Reciprocal Rank Fusion: a node
scores `Σ weight / (rrfK + rank)` over the rankings that returned it. A node
both rankings found beats one that tops only one of them, so an exact name
and a paraphrase both land near the top.

```text
CALL drevo.search.hybrid(query, label, embeddingProperty, k[, config])
YIELD node, score, ftsRank, vectorRank, mode
```

```cypher
CALL drevo.search.hybrid('anxious thoughts about work', 'Chunk', 'embedding', 5)
YIELD node, score, ftsRank, vectorRank, mode
RETURN node.title, score, ftsRank, vectorRank, mode
```

| Argument | Meaning |
|---|---|
| `query` | the text to search for; ranked by BM25 and embedded for the vector ranking |
| `label` | the label whose nodes carry the embedding; `null` ranks by BM25 only |
| `embeddingProperty` | the property holding each node's embedding, e.g. `'embedding'` |
| `k` | how many rows to return, at least 1 |
| `config` | optional map, below |

| Config key | Default | Meaning |
|---|---|---|
| `candidates` | `max(k, 20)` | how deep each ranking goes before fusion; never fewer than `k` |
| `rrfK` | `60` | the RRF constant, a number ≥ 0; a larger one narrows the gap between neighbouring ranks |
| `mode` | `'auto'` | `'auto'`, `'hybrid'`, `'fts'` or `'vector'`, see below |
| `ftsFilter` | `null` | a label the BM25 ranking is restricted to (BM25 otherwise covers every node) |
| `weights` | `{fts: 1.0, vector: 1.0}` | multiplies each ranking's reciprocal rank; finite numbers ≥ 0. A node only a `0`-weighted ranking found is left out |

An unknown key, a config that is not a map, `k < 1`, a negative `rrfK` or a
wrong-typed value fail with an error naming the key.

Each row carries the fused `score` (rows come best first), the node's 1-based
rank in each ranking — `null` when that ranking did not return it — and the
`mode` that ran, the same on every row:

| `mode` | Embedder installed | No embedder |
|---|---|---|
| `'auto'` | both rankings; rows say `'hybrid'` (BM25 only when `label` is `null`); embedder fails on the query → BM25 only, rows say `'fts'`, and the server logs a warning | BM25 only; rows say `'fts'` |
| `'hybrid'` | both rankings | error: `semantic embedding` is not available |
| `'fts'` | BM25 only | BM25 only |
| `'vector'` | the embedding ranking only | error: `semantic embedding` is not available |

`CALL drevo.semantic.info()` tells whether an embedder is installed.
`'hybrid'` and `'vector'` also need a `label`. Equal scores are ordered with the
nodes both rankings found first, then by node id, so the same graph and query
always give the same rows. As with `fts.search`, `k` is applied before a
`YIELD … WHERE`:

```cypher
CALL drevo.search.hybrid('graph databases', 'Doc', 'embedding', 10, {mode: 'fts', ftsFilter: 'Doc'})
YIELD node, score, ftsRank
WHERE node.group_id = 'g1'
RETURN node.title, score, ftsRank
```

`drevo.search.hybridRelationships(query, type, embeddingProperty, k[, config])
YIELD rel, score, ftsRank, vectorRank, mode` does the same over relationships:
`fts.searchRelationships` and the embeddings of the relationships of `type`
(`ftsFilter` names a relationship type):

```cypher
CALL drevo.search.hybridRelationships('who mentors whom', 'RELATES_TO', 'fact_embedding', 5)
YIELD rel, score, mode
RETURN rel, score, mode
```

### Personalized PageRank

`drevo.pagerank` says which nodes matter in the whole graph.
`drevo.pagerank.personalized` says which nodes matter **for a set of seed
nodes**: its random walk follows edges with probability `damping` and
otherwise jumps back to one of the seeds (classic PageRank jumps to any
node). Nodes close to and well connected with the seeds score high, a hub that
is merely popular elsewhere in the graph does not, and nodes the seeds cannot
reach are not returned at all. The scores of a call sum to 1.

```text
CALL drevo.pagerank.personalized(sourceNodes[, config])
YIELD node, score
```

```cypher
MATCH (seed:Page {title: 'Graph databases'})
CALL drevo.pagerank.personalized(seed, {limit: 10})
YIELD node, score
RETURN node.title, score
```

| Argument | Meaning |
|---|---|
| `sourceNodes` | the seeds: a node, a node id (integer), or a list mixing both; at least one |
| `config` | optional map, below |

| Config key | Default | Meaning |
|---|---|---|
| `damping` | `0.85` | the probability of following an edge rather than jumping back to the seeds; a number in the open interval (0, 1) |
| `maxIterations` | `20` | the most power-iteration steps, an integer ≥ 1 (`100` for `drevo.pagerank`) |
| `tolerance` | `1e-6` | stop once the scores change by at most this much in a step (L1 norm) |
| `direction` | `'outgoing'` | `'outgoing'` follows edges, `'incoming'` walks them backwards, `'both'` walks them either way |
| `relTypes` | `null` | walk only relationships of these types, e.g. `['CITES', 'LINKS']`; `null` or `[]` walks every type |
| `sourceWeights` | `null` | one number ≥ 0 per entry of `sourceNodes`, not all zero; a seed with twice the weight receives twice the jumps. Equal weights when `null` |
| `limit` | `null` | return only the best `limit` rows; scores are still computed over the whole graph |
| `minScore` | `0.0` | drop rows scoring below this (unreachable nodes are never returned) |

Edges are weighted by the relationship's weight (`1.0` unless set). A node
with no edge to follow sends its score back to the seeds. Rows come best
first, ties by node id, so the same graph and seeds always give the same rows;
`minScore` and then `limit` apply after ranking. An empty `sourceNodes`, an id
that is not a node, a `sourceWeights` list of the wrong length or all zeros, an
unknown key or a `direction` outside the three fail with an error naming the
problem.

The arguments are evaluated for each incoming row, so the seeds can come from
a search. A graph-RAG step that ranks the neighbourhood of the hybrid-search
hits, rather than returning every neighbour:

```cypher
CALL drevo.search.hybrid('anxious thoughts about work', 'Chunk', 'embedding', 5)
YIELD node
WITH collect(node) AS seeds
WHERE size(seeds) > 0
CALL drevo.pagerank.personalized(seeds, {limit: 50, direction: 'both'})
YIELD node, score
RETURN node.title, score
```

The `WHERE size(seeds) > 0` skips the call when the search found nothing,
since an empty `sourceNodes` is an error.

`drevo.pagerank` takes the same map (without `sourceWeights`), so the damping,
iterations, edge direction and types of global PageRank can be set too:

```cypher
CALL drevo.pagerank({damping: 0.9, relTypes: ['LINKS'], limit: 10})
YIELD node, score
RETURN node.title, score
```

### Graph expansion

`drevo.graph.expand` widens a handful of seed nodes into their neighbourhood in
one call: a breadth-first walk that returns every node within `hops`
relationships of a seed, each once, at its shortest distance, together with the
relationship that first reached it. It is the "find seeds, then widen" step of
graph retrieval — seeds from a search, then the facts and entities around them —
without a round-trip per node.

```text
CALL drevo.graph.expand(seeds[, config])
YIELD node, distance, via, truncated
```

```cypher
CREATE (deploy:Task {title: 'deploy'})-[:BLOCKS]->(migrate:Task {title: 'migrate'}),
       (migrate)-[:TAGGED]->(:Topic {title: 'postgres'}),
       (deploy)-[:OWNED_BY]->(:Person {title: 'ana'})
WITH deploy
CALL drevo.graph.expand(deploy, {hops: 2})
YIELD node, distance, via
RETURN node.title, distance, via.relType, via.direction
```

returns `deploy` at distance 0 (`via` is `null` for a seed), `migrate` and `ana`
at distance 1, and `postgres` at distance 2, reached over `TAGGED` from
`migrate`.

`seeds` is a node, a node id, or a list mixing both; `null` and `[]` give no
rows. The call runs once per input row, so seeds can come from an earlier
clause — for example the hits of a [hybrid search](#hybrid-search-780):

```cypher
CALL drevo.search.hybrid('database migration', null, 'embedding', 5, {mode: 'fts'})
YIELD node
WITH collect(node) AS seeds
CALL drevo.graph.expand(seeds, {hops: 2, excludeLabels: ['Message', 'ReasoningTrace']})
YIELD node, distance
RETURN node.title, distance
```

| Config key | Default | Meaning |
|---|---|---|
| `hops` | `1` | how many relationships away from a seed a node may be; `0` returns the seeds only. At most `DREVO_EXPAND_MAX_HOPS` (6 unless the server sets it) |
| `direction` | `'both'` | `'outgoing'`, `'incoming'` or `'both'` |
| `relTypes` | `null` | only relationships of these types are walked; `null` walks every type |
| `labels` | `null` | only nodes with one of these labels are returned and walked further; seeds are always returned |
| `excludeLabels` | `[]` | nodes with any of these labels are neither returned nor walked through |
| `perNodeLimit` | `25` | at most this many new nodes are taken from one node, its relationships taken by id, lowest first |
| `maxNodes` | `200` | at most this many rows, seeds included; at most `DREVO_EXPAND_MAX_NODES` (5000 unless the server sets it) |
| `asOf` | `null` | an ISO-8601 timestamp: a relationship with `valid_from` / `valid_until` (as `drevo.memory.assertFact` writes them) is walked only if `valid_from <= asOf < valid_until`; one without them always is |
| `includeSeeds` | `true` | whether the seeds come back as distance-0 rows |

Each row holds:

| Column | Meaning |
|---|---|
| `node` | the node |
| `distance` | how many relationships from the nearest seed; `0` for a seed |
| `via` | `{fromId, relId, relType, direction}` for the relationship that first reached the node — `direction` is `'out'` when it points away from `fromId`, `'in'` when it points at it; `null` for a seed |
| `truncated` | `true` on every row when the walk stopped at `maxNodes` with more nodes left |

Rows come seeds first, in the order given, then by distance and node id. Nodes
at one distance are walked in that order and each node's relationships by id,
so a node two others reach at the same distance is attributed to the one walked
first, and the same graph always gives the same rows. A node reached from two
seeds appears once; the relationships between returned nodes are not part of the
output. Nodes and relationships the user may not traverse are neither returned
nor walked through.

What a time-travel walk over long-term memory looks like:

```cypher
MATCH (alice {name: 'Alice'})
CALL drevo.graph.expand(alice, {relTypes: ['RELATED_TO'], asOf: '2024-03-01T00:00:00Z', includeSeeds: false})
YIELD node
RETURN node.name
```

A seed id that is not a node, a config that is not a map, an unknown key, a
negative or wrong-typed value, `hops` or `maxNodes` above the server's limit, and
`maxNodes` below the number of seeds fail with an error naming the id or the key.

### Graph-RAG retrieval

`drevo.rag.context` turns a question into the part of the graph that answers it,
written as text ready to paste into an LLM prompt, within a token budget — in one
call. `drevo.rag.retrieve` returns the same nodes as rows, for a client that does
its own formatting.

```text
CALL drevo.rag.retrieve(query[, config])
YIELD node, seedScore, distance, pprScore, rank, via, truncated, scope

CALL drevo.rag.context(query[, config])
YIELD text, tokens, format, nodeIds, relIds, truncated, mode, traceId, scope
```

```cypher
CALL drevo.rag.context('panic before the team meeting', {label: 'Entry', format: 'triples', tokenBudget: 800})
YIELD text, tokens, truncated, mode
RETURN text, tokens, truncated, mode
```

Over a CBT journal — a person writes entries, an entry contains automatic
thoughts, a thought is an instance of a cognitive distortion — the `text` is:

```text
Monday shift | Entry | Panic before the team meeting, heart racing.
Mila | Person | Keeps a mood journal
Mila | WROTE | Monday shift
Everyone will see I am incompetent | Thought
Monday shift | CONTAINS | Everyone will see I am incompetent
Mind reading | Distortion | Assuming what others think
Everyone will see I am incompetent | IS_A | Mind reading
```

The call runs five stages inside one statement, each the code of the procedure it
is named after:

1. **Seeds** — [`drevo.search.hybrid`](#hybrid-search-780)`(query, label,
   embeddingProperty, k, {candidates, rrfK, mode})`. Seeds with a label in
   `excludeLabels` are dropped.
2. **Expand** — [`drevo.graph.expand`](#graph-expansion)`(seeds, {hops,
   direction, relTypes, labels, excludeLabels, perNodeLimit, maxNodes, asOf})`.
3. **Edges** — the relationships whose two ends were both found, of `relTypes`
   and valid at `asOf`.
4. **Rank** — the seeds first, in search order; then the other nodes by `rank`:
   `'ppr'` runs [Personalized PageRank](#personalized-pagerank) over just the
   nodes and relationships found (so nothing outside them moves a score),
   teleporting to the seeds in proportion to their search score; `'distance'`
   orders by distance, then node id; `'none'` keeps the expansion's order.
5. **Write** (`context` only) — nodes are taken best-ranked first; each brings
   its block and the relationships to the nodes already taken. A node that would
   push the text over `tokenBudget` is skipped whole, never cut mid-sentence, and
   the next one is tried.

The statement timeout (`DREVO_QUERY_TIMEOUT_MS`) is checked between stages.

| Config key | Default | Meaning |
|---|---|---|
| `label` | `null` | the label whose embeddings rank the vector leg; `null` searches by BM25 only |
| `embeddingProperty` | `'embedding'` | the property holding those embeddings |
| `k` | `8` | how many seeds the search returns |
| `candidates`, `rrfK`, `mode` | `max(k, 20)`, `60`, `'auto'` | as in [`drevo.search.hybrid`](#hybrid-search-780) |
| `hops` | `2` | as in [`drevo.graph.expand`](#graph-expansion) |
| `direction`, `relTypes`, `labels`, `perNodeLimit`, `maxNodes`, `asOf` | `'both'`, `null`, `null`, `25`, `200`, `null` | as in [`drevo.graph.expand`](#graph-expansion); `direction` also orients PageRank |
| `excludeLabels` | `['Message', 'ReasoningTrace', 'ContextTrace', 'Community', 'IngestJob', 'IngestItem']` | labels kept out of the seeds and the expansion: conversation and bookkeeping, not knowledge, and [community summaries](#communities), which `scope: 'global'` reads instead. `[]` keeps everything |
| `rank` | `'ppr'` | `'ppr'`, `'distance'` or `'none'` |
| `source` | `null` | a list of source ids: nodes and relationships whose `source_id` property names another source are left out; those without a `source_id` are kept |
| `tokenBudget` | `2000` | the most tokens `text` may take; at least `50` |
| `tokenizer` | `'approx'` | how tokens are estimated: `'approx'` = ⌈characters / 4⌉, `'words'` = ⌈whitespace-separated words × 1.3⌉. Both are estimates, not a model's exact count |
| `format` | `'markdown'` | `'markdown'`, `'triples'` or `'json'` (below) |
| `textProperties` | `['name', 'title', 'description', 'body', 'text']` | the properties a node's block is built from, in order; the first one present names the node (a node with none is named `#id`) |
| `maxBodyChars` | `600` | each `body`, `text`, `description` or `quote` value longer than this many characters is cut to it, at a character boundary, with `…`. `name`, `title` and other properties are never cut, so a node keeps its full name |
| `includeProvenance` | `true` | write `valid_from`→`valid_until`, `«quote»` and `(source_id)` when an element carries them |
| `scope` | `'local'` | `'local'`: the five stages above. `'global'`: the `k` [communities](#communities) of `communityLayer` best matching the question (topped up with the largest others when fewer match) are the seeds, followed by their members, best in-community rank first, taken in turns across the communities up to `maxNodes`; a community is written as `community <id>` with its summary. `'auto'`: global when the local search finds no seed, when the question matches `globalHint`, or — when both search legs ran — when the best seed's search score is below `globalThreshold`; local otherwise, and local when there is no community. A heuristic: tune both to your corpus |
| `globalThreshold` | `0.02` | under `scope: 'auto'` with both search legs (an embedder and a `label`), a best seed score below this goes global — an RRF score: `2/61` ≈ 0.033 for a first place in both legs, `1/61` ≈ 0.016 in one. With BM25 alone the score says nothing about confidence, so the threshold is not used |
| `communityLayer` | `'default'` | the layer of [communities](#communities) `scope: 'global'` reads |
| `globalHint` | `'^(what\|что\|о чём\|overview\|summary)'` | under `scope: 'auto'`, a regular expression matched against the start of the question, ignoring case: a match goes global |
| `session` | `null` | the conversation the context is for, a string. `drevo.rag.context` then records a [context trace](#context-traces) linked to the session's newest message; `drevo.rag.retrieve` ignores it |
| `trace` | `false` | `true` records a [context trace](#context-traces) without a session (`drevo.rag.context` only) |

The three formats, for a node *Thought* that carries a quote and a fact
`IS_A` valid since 2025-01-01:

```text
markdown:
### Extraction

`drevo.extract.*` reads entities and facts out of a text with a language
model, and can write them into long-term memory with
[provenance](#provenance-sources-chunks-and-quotes). The model is any
OpenAI-compatible `chat/completions` endpoint the server is configured with
(`DREVO_EXTRACT_*`, falling back to `DREVO_TEXT2CYPHER_*`; see the
[Admin Guide](admin-guide.md#extraction-drevoextract-opt-in)).

```text
CALL drevo.extract.entities(text[, config])
YIELD name, type, description, offset, length, surface, confidence

CALL drevo.extract.facts(text[, config])
YIELD subject, subjectType, relation, object, objectType, quote, offset, length, confidence

CALL drevo.extract.ingest(text, config)
YIELD source, chunks, entities, facts, skipped

CALL drevo.extract.chunks(text[, config]) YIELD position, offset, length, text
CALL drevo.extract.status() YIELD configured, model, upstream, lastError, lastSkipped
```

The text is cut into passages of about `chunkChars` characters — at a
paragraph break, else a sentence end, else a space — each starting a tenth
of `chunkChars` before the previous one ended. Every passage is sent to the
model with instructions to answer with one JSON object
`{"entities": [...], "facts": [...]}` in which each mention quotes the
passage verbatim (`surface`) and each fact quotes the words that state it
(`quote`), both with their character offset. drevo then **checks every
quote against the passage**: one at the stated offset is kept; one standing
elsewhere is moved to its nearest place; one that differs only in whitespace
is matched to the passage's own words; one the passage does not contain is
dropped and counted in `skipped` (with items of a type or relation not
allowed, items below `minConfidence`, and malformed items). So nothing the
model made up reaches the results or the graph. An answer that is not JSON
is asked for once more ("return only the JSON object"); a second one fails
the call.

`entities` and `facts` read only. Their `offset` / `length` are characters
into the whole text, so `substring(text, offset, length)` is the surface or
quote; an item read twice where two passages overlap is returned once.
`type` is upper case and `relation` upper snake case (`works at` →
`WORKS_AT`).

```text
CALL drevo.extract.facts('Dana joined Acme in May. Acme is based in Oslo.')
YIELD subject, relation, object, quote, offset
RETURN subject, relation, object, quote, offset
```

```text
subject | relation   | object | quote                 | offset
Dana    | WORKS_AT   | Acme   | Dana joined Acme      | 0
Acme    | LOCATED_IN | Oslo   | Acme is based in Oslo | 25
```

`ingest` writes what it finds, in one transaction — a failure writes
nothing. Every passage is extracted before anything is written; then:

- the source is upserted (`source` as a map) or must exist (`source` as an id);
- each passage becomes a `:Chunk` with id `<sourceId>#<order>`, `-[:FROM]->`
  the source and chained `-[:NEXT]->` (as `drevo.memory.addChunks`);
- each entity is upserted as `drevo.memory.rememberEntity` does — keyed on
  name and type, labelled `:Entity` plus its type (`Person`, …, or the extra
  type in Pascal case: `SPACESHIP` → `:Spaceship`) — and gets
  `-[:MENTIONED_IN {offset, length, surface}]->` its chunk, the offset counted
  in the chunk;
- each fact is asserted as `drevo.memory.assertFact(subject, relation,
  object, false, …)` does, with `source_id`, `chunk_id`, `quote`, `offset` and
  `length`; facts are never exclusive.

The row counts the chunks, the entity mentions and the facts written (or, with
`dryRun: true`, that would be written; nothing is). Ingesting the same text
into the same source again finds the same chunks, entities and facts, and
changes nothing. Afterwards `drevo.memory.evidence` and `drevo.memory.factsAt`
show the passages behind each entity.

```text
CALL drevo.extract.ingest($chapter, {source: {id: 'book-1', title: 'Book One', kind: 'document'}})
YIELD source, chunks, entities, facts, skipped
RETURN source, chunks, entities, facts, skipped
```

| Config key | Default | Meaning |
|---|---|---|
| `types` | `['PERSON', 'OBJECT', 'LOCATION', 'EVENT', 'ORGANIZATION']` | entity types to look for, upper-snake-cased; extra types are allowed |
| `relations` | `null` | the relations facts may use, upper-snake-cased; `null` lets the model name them |
| `schemaHint` | `null` | domain vocabulary added to the instructions ("Characters are the crew of the Nostromo") |
| `language` | `'auto'` | the text's language, as a hint to the model |
| `model` | the server's | the model to ask instead of `DREVO_EXTRACT_MODEL` |
| `chunkChars` | `1500` | passage size in characters, from 20 to 100000 |
| `minConfidence` | `0.5` | items the model rates lower are dropped; an item without a confidence counts as 1 |
| `source` | — | `ingest` only, required: `{id, uri, title, kind}` (upserted) or an existing source's id |
| `session` | `null` | `ingest` only: the conversation whose newest message gets `MENTIONS` to each entity, as in `rememberEntity` |
| `resolve` | `'exact'` | `ingest` only: how a known entity is found, as the `resolve` option of `drevo.memory.rememberEntity`: `'exact'` = same name and type; `'fuzzy'` also reuses the one entity of the type whose name scores at least 0.9 (else the new entity gets `POSSIBLY_SAME` edges to its near-misses) |
| `dryRun` | `false` | `ingest` only: count without writing |

An unknown key, or an `ingest`-only key given to another procedure, is an
error naming it. Without a configured endpoint, `entities`, `facts` and
`ingest` fail with a capability error naming `drevo.extract.status()`; a bad
config is reported before any model call.

`drevo.extract.chunks` shows the passages a text is cut into, without a
model — what `ingest` would send and store:

```cypher
CALL drevo.extract.chunks('Dana joined Acme in May. Acme is based in Oslo, far from the sea.', {chunkChars: 40})
YIELD position, offset, length, text
RETURN position, offset, length, text
```

`drevo.extract.status()` reports whether an endpoint is configured, its model
and URL (never the key), the error of the last call (`null` when it
succeeded) and how many items the last call dropped:

```cypher
CALL drevo.extract.status() YIELD configured, model, upstream, lastError, lastSkipped
RETURN configured, model, upstream, lastError, lastSkipped
```

**Extract on write.** A trigger can feed every new document to `ingest`.
`afterAsync` runs it after the commit, off the request that created the
document, so the model call never holds up a write:

```text
CALL apoc.trigger.install('drevo', 'extract_documents',
  'UNWIND $createdNodes AS d WITH d WHERE d:Document
   CALL drevo.extract.ingest(d.body, {source: {id: d.id, title: d.title, kind: "document"}}) YIELD facts
   RETURN count(*)',
  {phase: 'afterAsync'})
```

The first argument is the database the trigger belongs to — `drevo`, the
default one, here.

A failed extraction is logged by the trigger and recorded in
`drevo.extract.status()`; the document itself stays committed. This path
suits short texts; the call's time is bounded by `DREVO_EXTRACT_TIMEOUT_MS`
per passage.

## Entities
- **Anna** (Person): Keeps a CBT journal
- **I will fail** (Thought) «I will fail at this» (journal-1)

## Relations
- (I will fail) -[IS_A]-> (Catastrophizing) valid 2025-01-01→ «always the worst» (journal-1)

triples:
Anna | Person | Keeps a CBT journal
I will fail | Thought | «I will fail at this» (journal-1)
I will fail | IS_A | Catastrophizing | valid 2025-01-01→ | «always the worst» (journal-1)

json:
{"nodes":[{"id":1,"labels":["Person"],"props":{"description":"Keeps a CBT journal","name":"Anna"}}],"rels":[{"from":3,"id":12,"props":{"valid_from":"2025-01-01"},"to":4,"type":"IS_A"}],"stats":{"nodes":1,"rels":1}}
```

`triples` takes the fewest tokens. A `RELATED_TO` fact written by
`drevo.memory.assertFact` is shown under its own relation name (its `type`
property). JSON keys are sorted; a node's `props` holds its text properties and,
with `includeProvenance`, `quote`, `source_id`, `valid_from` and `valid_until`.

`drevo.rag.retrieve` returns one row per node, in rank order:

| Column | Meaning |
|---|---|
| `node` | the node |
| `seedScore` | its fused search score when it is a seed, else `null` |
| `distance` | relationships from the nearest seed; `0` for a seed |
| `pprScore` | its Personalized PageRank score with `rank: 'ppr'` (the scores of an untruncated retrieval sum to 1), else `null` |
| `rank` | its position, from 1 |
| `via` | as in [`drevo.graph.expand`](#graph-expansion); `null` for a seed |
| `truncated` | whether the expansion stopped at `maxNodes` |
| `scope` | the scope that ran: `'local'` or `'global'`. In the global scope the communities are the seeds and their members have `distance` 1 and their in-community rank as `pprScore` |

```cypher
CALL drevo.rag.retrieve('team meeting', {rank: 'distance', hops: 1})
YIELD node, seedScore, distance, rank
RETURN rank, node.title, distance, seedScore
```

`drevo.rag.context` returns one row:

| Column | Meaning |
|---|---|
| `text` | the context; `''` when nothing matched or nothing fits |
| `tokens` | the size of `text` under `tokenizer` — never more than `tokenBudget` |
| `format` | the format written |
| `nodeIds`, `relIds` | the ids of the nodes and relationships in `text`, in the order written |
| `truncated` | `true` when a node did not fit the budget or the expansion hit `maxNodes` |
| `mode` | the search mode that ran: `'hybrid'`, `'fts'` or `'vector'` (`'auto'` without an embedder or a `label` runs `'fts'`) |
| `traceId` | the `id` of the [context trace](#context-traces) recorded with `session` or `trace: true`, else `null` |
| `scope` | the scope that ran: `'local'` or `'global'` (`'auto'` resolves to one of them); a context trace records it too |

The same graph, question and config give byte-identical `text`: ties are broken
by node id and properties are written in a fixed order. A question that matches
nothing gives no `retrieve` rows and a `context` row with empty `text`, `tokens`
`0` and empty id lists. Nodes and relationships the user may not read are left
out of every stage. A query that is not a string, a config that is not a map, an
unknown key (`includeSeeds` included: seeds are always returned) and a value of
the wrong type or range fail with an error naming the key; an explicit `mode:
'hybrid'` or `'vector'` without an embedder fails as in `drevo.search.hybrid`.
Over HTTP the same call is [`POST /rag/context`](admin-guide.md#graph-rag-context-post-ragcontext).

### Communities

`drevo.community.summarize` cuts the graph into communities — groups of nodes
more densely linked to each other than to the rest — and writes each one back
as a `:Community` node with a short summary of who and what is in it. A
corpus-level question ("what is this project about?") is then answered from a
handful of summaries with [`scope: 'global'`](#graph-rag-retrieval) instead of
from the neighbourhood of a few search hits.

```text
CALL drevo.community.summarize([config]) YIELD community, size, summary, keywords, mode
CALL drevo.community.query(query, k[, config]) YIELD community, score, summary
CALL drevo.community.of(node) YIELD community, level
CALL drevo.community.stale() YIELD community, size, changed
```

```cypher
CREATE (alice:Person {name: 'Alice', description: 'Backend lead'}),
       (bob:Person {name: 'Bob'}),
       (t1:Ticket {title: 'BE-142 Login token expires early'}),
       (t2:Ticket {title: 'BE-101 API rate limiter'}),
       (alice)-[:WORKS_ON]->(t1), (alice)-[:WORKS_ON]->(t2),
       (bob)-[:WORKS_ON]->(t1), (alice)-[:MENTORS]->(bob)
WITH alice
CALL drevo.community.summarize({mode: 'extractive'})
YIELD community, size, summary, keywords
RETURN community.id, size, summary, keywords
```

Over an IT task manager with a backend and a mobile team, one row per team:

```text
default-0 | 6 | 6 members. Person: Alice, Bob, Carol. Ticket: BE-101 API rate limiter, BE-142 Login token expires early, BE-150 Database migration. Keywords: api, backend, database, …. Relations: WORKS_ON (7), MENTORS (2), BLOCKS (1).
default-1 | 6 | 6 members. Person: Dave, Erin, Frank. Ticket: …
```

It writes

```text
(:Community {id, layer, size, modularity, summary, keywords, mode, computed_at, member_ids_hash, config})
(member)-[:MEMBER_OF {layer, rank}]->(:Community)
(:Community)-[:RELATED_COMMUNITY {weight}]->(:Community)
```

`id` is `<layer>-<n>`, communities numbered largest first; `rank` is the
member's PageRank inside its community (the ranks of a community sum to 1);
`modularity` is the partition's; `weight` counts the relationships between two
communities (one `RELATED_COMMUNITY` per linked pair, from the lower-numbered
one); `member_ids_hash` fingerprints the member set for `stale()`; `config` is
the JSON `{"labels": …, "relTypes": …}` the layer was computed with.

| Config key | Default | Meaning |
|---|---|---|
| `algorithm` | `'louvain'` | `'louvain'` (modularity) or `'wcc'` (weakly connected components) |
| `resolution` | `1.0` | Louvain's γ, a number ≥ 0: higher gives more, smaller communities |
| `labels` | `null` | partition only nodes with one of these labels; `null` takes every node but `Message`, `ReasoningTrace`, `ContextTrace`, `Community`, `Alias`, `Source`, `Chunk`, `IngestJob` and `IngestItem` |
| `relTypes` | `null` | partition along these relationship types only; `null` takes every type |
| `minSize` | `3` | smaller communities get no summary, keywords or mode (their members still get `MEMBER_OF`) |
| `maxMembers` | `40` | the most members a summary names, best-ranked first |
| `mode` | `'auto'` | `'extractive'`: written from the members, no model. `'llm'`: written by the chat model — an error while none is configured (see `drevo.extract.status()`). `'auto'`: the model when one is configured, else extractive; `mode` on the row and the node says which ran |
| `model` | `null` | the chat model for `'llm'`; `null` uses the server's |
| `tokenBudget` | `300` | the most tokens a summary takes (the `'approx'` estimate of [`drevo.rag.context`](#graph-rag-retrieval)); at least `20` |
| `replace` | `true` | delete the layer's previous communities first; `false` refuses a layer that already has some |
| `layer` | `'default'` | the name of the partition, so several can sit side by side (say, `'fine'` with `resolution: 2.0`); the global scope of `drevo.rag.context` reads the one its `communityLayer` names |
| `async` | `false` | `true` is refused until background jobs exist; summarise synchronously |

An extractive summary is `<size> members.`, then the members named best-ranked
first, grouped by label in the order of their best member (`Person: Alice,
Bob.`), then `Keywords:` — the ten most salient terms of the members' `name`,
`title`, `description`, `body` and `text`, ranked as by `keywords()` with every
partitioned node as a document — then `Relations:` with the three most frequent
relationship types inside the community and their counts. As many members as
fit `tokenBudget` are named. The same graph and config give the same text.

`drevo.community.summarize` writes, so it needs `CREATE` (and `DELETE` to
replace a layer); the other three only read. It runs over what the user may
see.

`drevo.community.query` searches the communities of one layer: BM25 over their
summaries (every trigram of the query must occur, as in
`fts.search`), fused by RRF with a vector leg over their
embeddings when an embedder is installed and the communities carry one. To
embed them as they are written, register the label once before summarising:

```text
CALL drevo.semantic.register('Community', 'summary', 'embedding', 'auto')
```

Its config takes `layer` (`'default'`), `embeddingProperty` (`'embedding'`)
and `candidates`, `rrfK`, `mode` as in
[`drevo.search.hybrid`](#hybrid-search-780). Rows come best first.

```cypher
CALL drevo.community.query('login token', 3)
YIELD community, score, summary
RETURN community.id, score, summary
```

`drevo.community.of(node)` — a node or a node id; `null` gives no rows —
returns the node's community in each layer, `level` being the layer's name.
`drevo.community.stale()` returns every community with its member count now
and whether its member set `changed` since it was computed: a member deleted or
unlinked, or a new node linked to members — it counts towards the community it
has the most links into. Only nodes with the layer's `labels` and links of its
`relTypes` (as stored in `config`; bookkeeping left out when it named none)
count. Summarising again, from a
scheduled task or a trigger, refreshes them.

```cypher
CREATE (t:Ticket {title: 'BE-160 Cache warm-up'})
WITH t
CALL drevo.community.of(t) YIELD community, level
RETURN community.id, level
```

```cypher
CALL drevo.community.stale() YIELD community, size, changed
WHERE changed
RETURN community.id, size
```

A config that is not a map, an unknown key and a value of the wrong type or
range fail with an error naming the key; `drevo.community.of` with a value that
is not a node or an id, or with an id no node has, fails naming it.

### Entity resolution

Two spellings of one thing — «Пушкин» and «А. С. Пушкин», `ACME Corp.` and
`Acme Corporation` — become two nodes, and the facts about the thing split
between them. Entity resolution finds such pairs, lets a person or an agent
review them, and folds each pair into one node that remembers the other
spelling:

```text
CALL drevo.entity.candidates(label[, config])
YIELD a, b, nameScore, vectorScore, score, reasons

CALL drevo.entity.merge(keepId, dropIds[, config])
YIELD kept, merged, relationshipsMoved, propertiesMerged

CALL drevo.entity.aliases(name) YIELD entity, aliases
```

Names are compared in a normalised form: lower case, Latin letters without
accents (`é` → `e`, `ß` → `ss`), `ё` → `е`, and every run of punctuation and
spaces one space, so initials become one-letter words. The function
`drevo.entity.normalise(name)` returns that form:

```cypher
RETURN drevo.entity.normalise('А. С. Пушкин') AS initials,
       drevo.entity.normalise('Émile  Zola!') AS accents
```

returns `'а с пушкин'` and `'emile zola'`.

#### Candidates

`drevo.entity.candidates` compares the names of the nodes with `label` and
returns the pairs scoring at least `threshold`, best first (then by the ids
of `a` and `b`; `a` is always the older node). Nodes whose types differ are
never paired, so the street «Пушкина» (a `LOCATION`) is never offered as the
poet «Пушкин»:

```cypher
CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', 'poet') YIELD node AS p1
CALL drevo.memory.rememberEntity(null, 'А. С. Пушкин', 'PERSON', null) YIELD node AS p2
CALL drevo.memory.rememberEntity(null, 'Пушкина', 'LOCATION', 'a street') YIELD node AS p3
WITH count(*) AS remembered
CALL drevo.entity.candidates('Entity', {})
YIELD a, b, nameScore, vectorScore, score, reasons
RETURN a.name, b.name, nameScore, vectorScore, score, reasons
```

returns one row: «Пушкин» and «А. С. Пушкин» with `nameScore` 1.0,
`vectorScore` `null`, `score` 1.0 and the reasons
`['name:1.00', 'shared_token:пушкин']`.

`nameScore` is the Jaro–Winkler similarity of the normalised names, or a
higher one that allows for initials: a name without initials is compared
with the other name's words without them («пушкин» and «а с пушкин» score
1.0), and a name with initials is compared with the other name cut to
initials where the words differ («а п смирнова» and «анна петровна смирнова»
score 1.0). Names whose initials disagree («а с пушкин», «л н пушкин») are
only as close as their spelling. `vectorScore` is the cosine similarity of
the two nodes' embeddings when `embeddingProperty` is set and both have one,
else `null`. `score` is the weighted mean of the scores present; when one is
missing the other counts alone. `reasons` lists, as strings, the name score,
the vector score (when present) and every shared word of at least three
characters, so a reviewer sees why a pair was offered.

| Config key | Default | Meaning |
|---|---|---|
| `nameProperty` | `'name'` | the property holding a node's name; nodes without a string there are skipped |
| `typeProperty` | `'type'` | pairs whose values here differ are never candidates (two missing values are equal) |
| `embeddingProperty` | `null` | the property holding a node's embedding; `null` compares names only |
| `threshold` | `0.9` | the `score` a pair needs, inclusive |
| `nameWeight` / `vectorWeight` | `0.5` / `0.5` | how much each score counts; not both `0` |
| `blocking` | `'prefix3'` | which pairs are compared at all: `'prefix3'` — names whose first three characters agree once initials are left out; `'token'` — names sharing a word of three or more characters; `'none'` — every pair |
| `maxPairs` | `100000` | the most pairs compared; more fails with an error suggesting blocking |
| `normalise` | `true` | `false` compares the names as they are written |

A label that is not a string, a config that is not a map, an unknown key and
a wrong-typed or out-of-range value fail with an error naming the key.

#### Merge

`drevo.entity.merge(keepId, dropIds[, config])` folds the dropped nodes into
the kept one, in one transaction:

- every relationship of a dropped node is re-created on the kept node with
  its type and properties; one the kept node would then have twice (same
  type, same other end, same direction and equal properties) is dropped
  instead. A relationship between the kept and a dropped node becomes a
  self-loop, kept once; a `POSSIBLY_SAME` between them is settled and
  dropped;
- the kept node's properties are combined with the dropped nodes' by
  `strategy`: `'keepFirst'` (the kept node's value wins, missing ones are
  filled from the dropped nodes in order), `'newest'` (the value of the
  most recently changed node — `updated_at`, else `created_at` — wins; the
  kept node wins a tie) or `'collect'` (differing scalar values become a
  list, the kept node's first). The name, the type, `id`, `created_at`,
  `updated_at`, `aliases` and `merged_ids` are never combined;
- the dropped nodes' names (and their own aliases) are appended to the kept
  node's `aliases` list, without repeats, and each dropped name gets an
  `(:Alias {name, normalised, created_at})-[:ALIAS_OF]->(kept)` node, so
  `drevo.entity.aliases` and `rememberEntity` with `resolve: 'alias'` find
  the old spelling;
- the dropped nodes are deleted and their ids appended to the kept node's
  `merged_ids`.

```cypher
CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', 'poet') YIELD node AS keep
CALL drevo.memory.rememberEntity(null, 'А. С. Пушкин', 'PERSON', 'author of Onegin') YIELD node AS dup
CALL drevo.memory.rememberEntity(null, 'Евгений Онегин', 'OBJECT', null) YIELD node AS novel
CALL drevo.memory.assertFact('А. С. Пушкин', 'WROTE', 'Евгений Онегин', false) YIELD rel
CALL drevo.entity.merge(keep, [dup], {strategy: 'keepFirst'})
YIELD kept, merged, relationshipsMoved, propertiesMerged
RETURN kept.name, kept.aliases, kept.description, merged, relationshipsMoved, propertiesMerged
```

returns «Пушкин» with aliases `['А. С. Пушкин']`, description `'poet'`, one
node merged and one relationship (the `WROTE` fact) moved;
`drevo.memory.factsAt('Пушкин', null)` now lists the fact.

`keepId` is a node or a node id; `dropIds` is a node, a node id or a list
mixing both (repeats count once; an empty list changes nothing). The call
runs once per input row, so both may come from an earlier clause. It fails,
writing nothing, when an id is not a node, the kept node is among the
dropped ones, or a dropped node's type differs from the kept node's — unless
the config says `force: true` (the kept node keeps its type). It needs the
privilege to delete the dropped nodes and to create relationships and
`:Alias` nodes, and goes through the regular write path: triggers fire,
change events are posted and auto-embedding applies. Two transactions
merging overlapping nodes conflict at commit; one of them fails.

| Config key | Default | Meaning |
|---|---|---|
| `strategy` | `'keepFirst'` | `'keepFirst'`, `'newest'` or `'collect'` |
| `force` | `false` | merge nodes of different types |
| `nameProperty` | `'name'` | the property holding a node's name |
| `typeProperty` | `'type'` | the property holding a node's type |

There is no undo: `merged_ids` records what was folded in.

#### Aliases

`drevo.entity.aliases(name)` returns the nodes known under `name`, compared
normalised: the nodes an `:Alias` of that name points at and the nodes
(other than `:Alias` nodes) whose `name` it is, each with its aliases — its
`aliases` list, then the names of `:Alias` nodes pointing at it that the list
lacks — by node id.

```cypher
CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', 'poet') YIELD node AS keep
CALL drevo.memory.rememberEntity(null, 'Pushkin', 'PERSON', null) YIELD node AS dup
CALL drevo.entity.merge(keep, dup) YIELD kept
CALL drevo.entity.aliases('PUSHKIN') YIELD entity, aliases
RETURN entity.name, aliases
```

returns «Пушкин» with `['Pushkin']`. A `name` that is not a string is an
error.

The workflow is `candidates`, then a review by a person or an agent, then
`merge`. `rememberEntity` can also resolve a new mention against known
entities as it writes — see `resolve` under
[Long-term memory](#long-term-memory-entities-and-facts-over-time).
#### Context traces

A context trace records what one `drevo.rag.context` call handed to the model, so
a wrong answer can be traced to a missing fact, a bad rank or a budget cut. It is
written when the config names a `session` (or sets `trace: true`), in the same
statement, through the regular `CREATE` path — so it needs the `CREATE`
privilege, fires triggers, shows on `/events` and lands in the log like any other
write:

```text
(:ContextTrace {id, session, query, created_at, format, tokenizer, tokens,
                tokenBudget, truncated, mode, hops, rank, seedCount, nodeCount,
                relCount, durationMs, config, text})
(:ContextTrace)-[:INCLUDED {rank, distance, seedScore, pprScore, tokens}]->(node)
(:ContextTrace)-[:SKIPPED {rank, reason}]->(node)
(:ContextTrace)-[:INITIATED_BY]->(:Message)
```

```cypher
CALL drevo.rag.context('panic before the team meeting', {session: 'chat-7', tokenBudget: 300})
YIELD text, traceId
RETURN text, traceId
```

| Property | Meaning |
|---|---|
| `id` | a UUID, returned as `traceId` |
| `session` | the config's `session`; absent with `trace: true` alone |
| `query`, `text` | the question and the text returned for it |
| `created_at` | when it was recorded, ISO-8601 UTC |
| `format`, `tokenizer`, `tokens`, `tokenBudget`, `truncated`, `mode`, `hops`, `rank` | as in the call and its row; `mode` is the search mode that ran |
| `seedCount`, `nodeCount`, `relCount` | the seeds found, and the nodes and relationships written into `text` |
| `durationMs` | how long retrieval and writing the text took, without recording the trace |
| `config` | the effective config as JSON — every key with its value or default, except `session` and `trace` |

`INCLUDED` goes to each node written into `text`, ranked from 1 in the order
written, with what ranked it (`seedScore` and `pprScore` are absent when they do
not apply) and the tokens it added: they sum to the trace's `tokens`. `SKIPPED`
goes to each node retrieved but left out, ranked after the written ones:
`reason: 'budget'` when it did not fit, `'maxNodes'` for the node the expansion
stopped at when it hit `maxNodes`. `INITIATED_BY` goes to the session's newest
`:Message` (none when the session has no messages yet). The default
`excludeLabels` keeps traces out of every `drevo.rag.*` retrieval; full-text
search indexes them like any node with string properties, so `fts.search` can
return a trace whose question or text matches.

```text
CALL drevo.rag.traces(session, limit) YIELD trace
CALL drevo.rag.explain(traceId)
YIELD node, status, rank, distance, seedScore, pprScore, tokens, reason
CALL drevo.rag.replay(traceId) YIELD text, tokens, changed
CALL drevo.rag.pruneTraces(olderThan) YIELD deleted
```

- `drevo.rag.traces` — the newest `limit` traces of `session`, newest first;
  `null` lists every session's.
- `drevo.rag.explain` — one row per node of the trace, by `rank`: `status` is
  `'included'` or `'skipped'`; `distance`, `seedScore`, `pprScore` and `tokens`
  are set for included nodes, `reason` for skipped ones.
- `drevo.rag.replay` — runs the trace's question with its stored config against
  the graph as it is now and returns the new text, its tokens, and whether it
  differs from the recorded text. It records no trace.
- `drevo.rag.pruneTraces` — deletes the traces created before `olderThan` (a UTC
  date `'2025-01-01'` or timestamp `'2025-01-01T00:00:00Z'`) with their
  relationships, and returns how many; the nodes they pointed at stay. It needs
  `EXECUTE ADMIN PROCEDURES`. `DREVO_RAG_TRACE_RETENTION_DAYS` runs it at
  startup and every 24 hours.

```cypher
CALL drevo.rag.traces(null, 10) YIELD trace
RETURN trace.id, trace.session, trace.query, trace.tokens, trace.created_at
```

```text
CALL drevo.rag.explain($traceId)
YIELD node, status, rank, tokens, reason
RETURN rank, status, coalesce(node.name, node.title) AS name, tokens, reason
```

```cypher
CALL drevo.rag.pruneTraces('2025-01-01') YIELD deleted
RETURN deleted
```

An unknown `traceId` fails with `no context trace with id …`; a trace whose
stored `config` no longer parses cannot be replayed and says why. A `traceId`,
`session`, `limit` or `olderThan` of the wrong type fails with an error naming
the argument.

#### Evaluate strategies

`drevo.rag.evaluate` scores how well each way of retrieving a context does on a
gold set — questions and the nodes their context should hold — so you can see
whether expanding through the graph helps on your data. `drevo.rag.evaluateSummary`
averages the scores per strategy. Both only read: the pipeline runs without a
session, so no trace is recorded.

```text
CALL drevo.rag.evaluate(questions[, config])
YIELD question, strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds,
      recallRetrieved

CALL drevo.rag.evaluateSummary(questions[, config])
YIELD strategy, questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms,
      meanTokens
```

`questions` is a list of maps, one per question:

- `{query, expected}` — `expected` lists the nodes the context should hold, each
  a node id or a `{name, label}` map naming the node whose `name` or `title`
  property is exactly `name` (and that carries `label`, when given);
- `{query, traceId}` — scored against the nodes that [context
  trace](#context-traces) `INCLUDED`; without `query` the trace's own question
  is asked.

```text
CALL drevo.rag.evaluateSummary([
  {query: 'panic before the team meeting',
   expected: [{name: 'Mind reading', label: 'Distortion'}, {name: 'Mila', label: 'Person'}]},
  {query: 'a calm walk', expected: [{name: 'Mila', label: 'Person'}]}
], {strategies: ['fts', 'hybrid+expand', 'rag'], tokenBudget: 300})
YIELD strategy, hitRate, meanRecall, meanPrecision, mrr, p95Ms, meanTokens
RETURN strategy, hitRate, meanRecall, meanPrecision, mrr, p95Ms, meanTokens
```

`config` takes every key of the [`drevo.rag.context` config](#graph-rag-retrieval)
— `k`, `hops`, `tokenBudget`, `label` and `embeddingProperty` are the usual
ones — shared by all strategies, plus `strategies`, a list of:

| Strategy | Context |
|---|---|
| `'vector'` | the `k` nodes of `label` closest to the question by `embeddingProperty`, as `drevo.semantic.query` ranks them — the baseline. Needs a `label` and a server-side embedder |
| `'fts'` | the `k` best BM25 matches |
| `'hybrid'` | the `k` hits of `drevo.search.hybrid` (in the config's `mode`) |
| `'hybrid+expand'` | those hits, then their neighbourhood within `hops`, nearest first (`rank: 'distance'`) |
| `'rag'` | the full `drevo.rag.context` pipeline, ranked by Personalized PageRank (`rank: 'ppr'`) |
| `'rag-global'` | retrieval over community summaries; this server does not build them, so naming it is an error |

The default is every strategy this server can run with the config: `'fts'`,
`'hybrid'`, `'hybrid+expand'` and `'rag'`, with `'vector'` first when there is
an embedder and a `label`. Every strategy writes its nodes as
`drevo.rag.context` would, within the same `tokenBudget`, and is scored on what
fits — what the model would see:

| Column | Meaning |
|---|---|
| `question` | the question asked |
| `strategy` | the strategy |
| `hit` | whether the context held any expected node |
| `recall` | the share of the expected nodes the context held |
| `precision` | the share of the context's nodes that were expected (`0.0` for an empty context) |
| `mrr` | `1 / rank` of the first expected node in the context, `0.0` when none is there |
| `tokens` | the size of the context — never more than `tokenBudget` |
| `durationMs` | how long retrieving and writing it took, measured as a trace's `durationMs` |
| `nodeIds` | the ids of the nodes in the context, in the order written |
| `recallRetrieved` | the share of the expected nodes retrieved at all, before the budget: `recallRetrieved − recall` is what the budget cut |

Rows come per question, in order, then per strategy, in the config's order.
`evaluateSummary` returns one row per strategy: `questions` counted,
`hitRate`, `meanRecall`, `meanPrecision` and `mrr` averaged over them, `p50Ms`
and `p95Ms` the percentiles of `durationMs` (interpolated between the two
nearest values), and `meanTokens`. No questions give no rows.

An expected node that does not resolve — an id that is not a node, a `{name,
label}` nothing matches — fails the call with every such entry listed
(`2 expected nodes not found: questions[0].expected[1] {name: 'Fix', label:
'Commit'}, …`); a `{name}` that matches several nodes — by `name` on one and
`title` on another, say — fails listing their ids. An
unknown `traceId`, an unknown strategy, `'rag-global'`, `'vector'` without a
`label` or an embedder, a `session` or `trace: true` (an evaluation records
nothing), and a malformed question or config key fail with an error naming it.

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

## EXISTS subquery

An **existential subquery** `EXISTS { [MATCH] pattern [WHERE predicate] }` is the
brace-delimited, richer sibling of the bare [pattern predicate](#pattern-predicate).
It tests whether **at least one** match of the enclosed pattern exists relative to
the current row, with three additions the braces make possible:

- an optional leading `MATCH` keyword (`EXISTS { MATCH (a)-->(b) }` is equivalent
  to `EXISTS { (a)-->(b) }`),
- an optional inner `WHERE` that filters the matches *before* the existence test,
- a **bare node** is legal — because the braces delimit the pattern, there is no
  grouping ambiguity to resolve, so `EXISTS { (n) }` is a subquery (a bare `(n)`
  in expression position would be grouping).

```cypher
MATCH (t:Task)
WHERE EXISTS { (t)-[:DEPENDS_ON]->(dep) WHERE dep.status = 'open' }
RETURN t.title
```

Like a pattern predicate it is **anchored** on the variables already bound, the
variables it introduces stay scoped to the subquery, it composes with `NOT` /
`AND` / `OR`, and it can be returned directly as a boolean:

```cypher
MATCH (c:Customer)
WHERE NOT EXISTS { (c)-[:PLACED]->(:Order) }
RETURN c.name
```

Semantics:

- **Existence test** — `true` as soon as one match survives the optional inner
  `WHERE`; the matches are otherwise discarded.
- **`null` anchor** — if the head variable is already bound to `null` (an
  unmatched [`OPTIONAL MATCH`](#optional-match) node), the subquery is `null`
  under three-valued logic rather than an error.
- **Function form not supported** — the deprecated `exists(n.prop)` *function*
  form is not accepted; use `n.prop IS NOT NULL` (see [WHERE](#filtering--where)).

---

## COUNT subquery

A **counting subquery** `COUNT { [MATCH] pattern [WHERE predicate] }` is the
integer-valued sibling of the [EXISTS subquery](#exists-subquery): where
`EXISTS { p }` tests whether **at least one** match exists, `COUNT { p }` returns
**how many** there are. `COUNT { p } > 0` is exactly `EXISTS { p }`.

It shares the EXISTS subquery's surface — the braces disambiguate the pattern
from a grouped expression, an optional leading `MATCH` keyword is accepted, an
optional inner `WHERE` filters the matches before they are counted, and a bare
node `COUNT { (n) }` is legal — but it can be used anywhere an integer is
expected: a `WHERE` comparison, a `RETURN` column, `ORDER BY`, or arithmetic.

```cypher
MATCH (p:Person)
RETURN p.name AS name, COUNT { (p)-[:KNOWS]->() } AS friends
ORDER BY friends DESC
```

```cypher
MATCH (t:Task)
WHERE COUNT { (t)-[:DEPENDS_ON]->(dep) WHERE dep.status = 'open' } >= 2
RETURN t.title
```

Semantics:

- **Match count** — the number of matches of the pattern that survive the
  optional inner `WHERE`, anchored on the variables already bound (each row
  counts only its own matches); the subquery-introduced variables stay scoped to
  the subquery. No match yields `0`, never `null`.
- **`null` anchor** — if the head variable is already bound to `null` (an
  unmatched [`OPTIONAL MATCH`](#optional-match) node), the subquery is `null`
  under three-valued logic rather than an error (so it is distinct from an
  honest `0`).
- **Not the `count(*)` aggregation** — a counting subquery is a per-row scalar (a
  group key), not a fold over the result rows. `count(*)` / `count(expr)` keep
  their [aggregation](#aggregations) meaning; the two can coexist in one query.

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
