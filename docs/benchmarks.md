# Benchmarks — drevo vs. competitors

> Phase 15 task `00101`. This guide pairs with the executable harness
> [`benches/comparison_bench.rs`](https://github.com/ice1x/drevo/tree/main/benches/comparison_bench.rs). The whole point
> is **reproducibility**: drevo's numbers are measured by the harness on *your*
> machine, and the identical workload is specified below as runnable code against
> each competitor, so a comparison is something you *run* — never a figure copied
> from a vendor slide.

## Why this exists

The README's positioning table carries **approximate** competitor figures
derived from published benchmarks and vendor claims. Those are fine for a rough
"where does drevo sit" sketch, but they are not measured, not version-pinned, and
not run on the same hardware — so they cannot be trusted as a head-to-head. This
guide replaces "trust the slide" with "run the workload."

A fair cross-engine benchmark must hold three things constant: **the same
graph**, **the same operations**, and **the same machine**. This guide fixes the
first two precisely and tells you how to fix the third.

## The standard workload

A sparse social graph — the shape KuzuDB, Memgraph, and Neo4j all publish numbers
for:

| Property | Value |
|----------|-------|
| Nodes | 10,000 `Person` nodes |
| Edges | ~40,000 `KNOWS` edges (out-degree 4) |
| Indexed string | unique `title` (`person_00000000` …) |
| Indexed property | `city` ∈ 8 values (low cardinality), `age` ∈ [20, 80) |
| Full-text body | a per-node token (`identifier_00000123`) + a token shared by every node (`searchable`) |

The operations measured, and what each is meant to expose:

| Operation | What it probes |
|-----------|----------------|
| **Bulk load** | write throughput incl. index maintenance |
| **Point lookup by id** | primary-key access |
| **Point lookup by indexed title** | unique secondary-index access |
| **Lookup by indexed property** (`city = 'Berlin'`) | low-cardinality equality scan (≈1,250 rows) |
| **1-hop neighbours** | single-hop expansion |
| **2-hop BFS** | multi-hop traversal |
| **FTS — selective** | full-text query returning one row |
| **FTS — broad** | full-text query matching every node (worst case) |

## Running drevo's side

> **Note (epic #444):** the `comparison_bench` criterion harness drove the KV
> `Drevo` engine and was removed with it. The cross-database methodology below
> is retained for reference; re-run the drevo side through the server's Bolt
> endpoint (the same path the Memgraph comparison uses) rather than an
> in-process KV bench.

Criterion prints a confidence interval per operation and writes HTML reports to
`target/criterion/`. This bench is **not** in the per-PR test path — criterion
benches only run under `cargo bench`, never `cargo test` — so it never contends
for the shared CI runner. The scheduled / manual
[`benchmarks.yml`](https://github.com/ice1x/drevo/tree/main/.github/workflows/benchmarks.yml) workflow is the only place
it runs in CI.

### Reference run (in-memory backend)

Measured on the project's development machine (Apple Silicon, `--release`,
`MemoryBackend`). **These are illustrative — re-run on your hardware before
quoting them.** The competitor columns are deliberately left blank: fill them by
running the recipes below in the *same* environment rather than copying numbers
here.

| Operation | drevo (in-memory) | Neo4j | Memgraph | KuzuDB |
|-----------|------------------:|:-----:|:--------:|:------:|
| Bulk load (10k nodes + 40k edges) | ~700 ms | — | — | — |
| Point lookup by id | ~0.8 µs | — | — | — |
| Point lookup by indexed title | ~1.1 µs | — | — | — |
| Lookup by property (`city`, ~1,250 rows) | ~1.3 ms | — | — | — |
| 1-hop neighbours | ~5.8 µs | — | — | — |
| 2-hop BFS | ~18 µs | — | — | — |
| FTS — selective (top 10) | ~29 ms | — | — | — |
| FTS — broad (top 10, all match) | ~135 ms | — | — | — |

> The broad-FTS cost reflects the known trigram-scan hotspot flagged in
> [`audit/AUDIT-fts.md`](https://github.com/ice1x/drevo/tree/main/audit/AUDIT-fts.md) (Performance Watch List); the
> [SDK reference](sdk-reference.md) documents `search_fts` semantics.

The redb-backed (persistent) backend is intentionally **not** in this table:
redb opens one ACID transaction per write (~5 ms), so durable bulk-load latency
is a separate axis from the cross-engine *query* comparison this workload targets.
Benchmark persistence separately with batched transactions if that is the axis
you care about.

## Routing on a real road network

[`benches/sofia_routing_bench.rs`](https://github.com/ice1x/drevo/tree/main/benches/sofia_routing_bench.rs)
loads the streets of Sofia from OpenStreetMap and times
[`drevo.path.weighted`](cypher-reference.md#weighted-paths) between fixed pairs
of junctions 3 km or more apart. Every OSM node on a street or footway is a
`Junction` with `lat`/`lon`; every stretch between two of them is a `ROAD`
(with `car_s`, and `walk_s` unless it is a motorway, in each direction cars may
drive it) or a `PATH` (`walk_s` only). Driving routes follow `ROAD`s
`OUTGOING` by `car_s`; walking routes follow `ROAD`s and `PATH`s in `BOTH`
directions by `walk_s`. Download the extract, then run:

```sh
curl -A drevo-bench -o sofia-roads.json --data-urlencode \
  'data=[out:json][timeout:600];way["highway"](42.62,23.20,42.77,23.42);(._;>;);out body;' \
  https://overpass-api.de/api/interpreter
DREVO_OSM_JSON=sofia-roads.json cargo bench --bench sofia_routing_bench
```

Before timing, the bench routes every pair with A* and with Dijkstra and stops
if the costs differ. Map data © OpenStreetMap contributors, ODbL.

### Reference run

Apple Silicon, `--release`, in-memory `NativeService`, OSM data of 2026-10-20:
242,947 junctions, 396,551 relationships (143,781 junctions drivable), loaded
in 2.8 s. Times are per route, the mean over 8 routes of 243–589 junctions.
**Illustrative — re-run on your hardware before quoting them.**

| Workload | Time per route |
|----------|---------------:|
| Driving, A* (`maxSpeedMps` 38.9, the fastest road) | ~80 ms |
| Driving, bidirectional Dijkstra | ~46 ms |
| Driving, A* through `CALL drevo.path.weighted` | ~83 ms |
| Driving, `shortest_path` (one-directional Dijkstra over `weight`) | ~83 ms |
| Walking, A* (`maxSpeedMps` 1.4) | ~42 ms |
| Walking, bidirectional Dijkstra | ~105 ms |

| Adjacency read, every drivable junction once | Time |
|----------------------------------------------|-----:|
| Outgoing relationships copied as full records (`edges_of`) | ~68 ms |
| Outgoing relationships by reference (`for_each_edge_of`, what the search uses) | ~5 ms |
| Outgoing neighbour ids only (`neighbor_ids`) | ~21 ms |

The search reads each relationship by reference instead of copying its
record (#684): on the same data, per-route times dropped 11–23% (driving A*
90 → 80 ms, Dijkstra 53 → 46 ms; walking A* 51 → 42 ms, Dijkstra 137 →
105 ms).

What the numbers show:

- **A* pays off when the speed bound is tight.** On foot every relationship
  moves at the bound, the estimate is close to the real remaining cost, and A*
  takes half the time. For cars the bound has to be the fastest road in the city; most streets
  are much slower, the estimate is weak, and the bidirectional Dijkstra is
  faster. Leave `heuristic` out for networks whose speeds vary a lot.
- **Cypher adds nothing measurable** on top of the native call.
- **Reading relationships by reference is cheap.** Visiting every drivable
  junction's relationships in place costs a quarter of collecting neighbour
  ids and a fourteenth of copying the records; what a route costs now is
  mostly the search's own bookkeeping.

## Public-transport routing

[`benches/timetable_bench.rs`](https://github.com/ice1x/drevo/tree/main/benches/timetable_bench.rs)
times [`drevo.path.timetable`](cypher-reference.md#timetable-routing) on a
synthetic city the size of Sofia's weekday GTFS feed: a grid of 4,900 stops
250 m apart, 540 bus line patterns of 19 stops with 31 trips each (16,740
trips, 318,060 stop times, 9,720 `HOP` relationships) and 132,196 `WALK`
footpaths between stops up to 750 m apart. Each of 1,000 queries goes at
least 3 km, between 06:00 and 20:00, from the 20 stops nearest the origin to
the 20 nearest the destination (each within a 15-minute walk), with a 60 s
transfer, at most 4 transfers and 2 hours. No external data is needed:

```sh
cargo bench --bench timetable_bench
```

### Reference run

Apple M1 Max, `--release`, in-memory `NativeService`, on a shared machine
under load. **Illustrative — re-run on your hardware before quoting them.**

| Workload | p50 | p95 | p99 | max |
|----------|----:|----:|----:|----:|
| `NativeService::timetable` | 61 ms | 95 ms | 113 ms | 154 ms |
| `CALL drevo.path.timetable` (stops looked up by an indexed property) | 65 ms | 99 ms | 115 ms | 158 ms |

931 of the 1,000 queries find a journey within two hours; the fastest uses
0 / 1 / 2 / 3 / 4 transfers in 182 / 315 / 239 / 143 / 52 of them. Cypher and
the native call return the same arrivals for every query. A query keeps only
what it reads — the stops it reaches and their labels — and frees it when it
returns; nothing is kept between queries.

## Write transactions

A write transaction works on its own copy of the store. The copy shares
everything with the committed store — node and relationship records, the
adjacency lists (in chunks of 64 nodes), embeddings, and the kind and title
indexes — and a write copies only what it touches, so a small transaction
costs the same on a large graph as on a small one.

One `SET` on one node in an explicit transaction (`execute_write`), on
112,000 junctions and 210,000 roads, release build, counting allocator:

| | peak heap | time |
|---|---:|---:|
| before (whole adjacency, embeddings and indexes copied) | +22.4 MB | 16.7 ms |
| now | **+0.1 MB** | **0.27 ms** |

The store itself is the same size (102 MB vs 103 MB). The test
`drevo-core/tests/tx_copy_cost.rs` keeps the peak under 1 MB on 50,000
junctions with embeddings.

## Reproducing against competitors

Each competitor runs the **same** graph and the **same** eight operations. Use a
fresh, default-configured instance of a pinned version, on the same machine you
ran drevo on, and warm the cache before timing.

### Building the graph (Neo4j / Memgraph — Bolt + Cypher)

Both speak Bolt and openCypher, so the load is identical:

```cypher
UNWIND range(0, 9999) AS i
CREATE (p:Person {
  title: 'person_' + apoc.text.lpad(toString(i), 8, '0'),
  city: ['London','Paris','Berlin','Tokyo','Cairo','Lima','Oslo','Delhi'][i % 8],
  age: 20 + (i % 60),
  body: 'member profile graph node identifier_' + apoc.text.lpad(toString(i), 8, '0') + ' searchable'
});
```

```cypher
MATCH (p:Person) WITH p, toInteger(substring(p.title, 7)) AS i
UNWIND range(1, 4) AS j
MATCH (q:Person {title: 'person_' + apoc.text.lpad(toString((i + j) % 10000), 8, '0')})
CREATE (p)-[:KNOWS]->(q);
```

Create the indexes the workload assumes (drevo indexes `title` and properties
automatically):

```cypher
CREATE CONSTRAINT person_title IF NOT EXISTS FOR (p:Person) REQUIRE p.title IS UNIQUE;
CREATE INDEX person_city IF NOT EXISTS FOR (p:Person) ON (p.city);
CREATE FULLTEXT INDEX person_body IF NOT EXISTS FOR (p:Person) ON EACH [p.body];
```

The eight timed queries:

```cypher
// point lookup by id (use the engine's internal id captured at load)
MATCH (p) WHERE id(p) = $id RETURN p;
// point lookup by indexed title
MATCH (p:Person {title: 'person_00005000'}) RETURN p;
// lookup by indexed property
MATCH (p:Person {city: 'Berlin'}) RETURN p;
// 1-hop neighbours
MATCH (p:Person {title: 'person_00000000'})-[:KNOWS]->(n) RETURN n;
// 2-hop BFS
MATCH (p:Person {title: 'person_00000000'})-[:KNOWS*1..2]->(n) RETURN DISTINCT n;
// FTS selective / broad
CALL db.index.fulltext.queryNodes('person_body', 'identifier_00005000') YIELD node RETURN node LIMIT 10;
CALL db.index.fulltext.queryNodes('person_body', 'searchable') YIELD node RETURN node LIMIT 10;
```

> Memgraph's full-text support differs by version (label-property index +
> `text_search` module); consult its docs for the exact `CALL` and substitute it
> for the `db.index.fulltext` calls above. Everything else is identical.

### KuzuDB

KuzuDB is embedded (no server) and uses its own Cypher dialect with a typed
schema. Define the table, bulk-load from a generated CSV, and time the same eight
operations through the Python API:

```python
import kuzu
db = kuzu.Database("bench.kuzu")
conn = kuzu.Connection(db)
conn.execute("CREATE NODE TABLE Person(title STRING, city STRING, age INT64, body STRING, PRIMARY KEY(title))")
conn.execute("CREATE REL TABLE KNOWS(FROM Person TO Person)")
conn.execute('COPY Person FROM "person.csv"')   # generate with the same 10k rows
conn.execute('COPY KNOWS FROM "knows.csv"')
# then time: MATCH (p:Person {title:'person_00005000'}) RETURN p;  etc.
```

KuzuDB has no built-in full-text index comparable to drevo's trigram FTS, so the
two FTS rows are not directly comparable — note that asymmetry rather than forcing
a number.

## Honesty rules

- **Never copy a vendor number into the drevo column, or vice-versa.** A row is
  only filled by a run you did, on one machine, with pinned versions.
- **Pin and record versions** (drevo commit, Neo4j/Memgraph/KuzuDB release) next
  to any table you publish.
- **Report the backend.** drevo's in-memory and redb backends have very different
  write profiles; say which one a number came from.
- **Note capability gaps** (e.g. KuzuDB FTS) instead of fabricating parity.

See the [SDK reference](sdk-reference.md) for the drevo APIs the harness calls and
the [Cypher reference](cypher-reference.md) for drevo's query surface.
