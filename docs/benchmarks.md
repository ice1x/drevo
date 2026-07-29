# Benchmarks — drevo vs. competitors

> Phase 15 task `00101`. This guide pairs with the executable harness
> [`benches/comparison_bench.rs`](../benches/comparison_bench.rs). The whole point
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

```sh
cargo bench --bench comparison_bench
```

Criterion prints a confidence interval per operation and writes HTML reports to
`target/criterion/`. This bench is **not** in the per-PR test path — criterion
benches only run under `cargo bench`, never `cargo test` — so it never contends
for the shared CI runner. The scheduled / manual
[`benchmarks.yml`](../.github/workflows/benchmarks.yml) workflow is the only place
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
> [`audit/AUDIT-fts.md`](../audit/AUDIT-fts.md) (Performance Watch List); the
> [SDK reference](sdk-reference.md) documents `search_fts` semantics.

The redb-backed (persistent) backend is intentionally **not** in this table:
redb opens one ACID transaction per write (~5 ms), so durable bulk-load latency
is a separate axis from the cross-engine *query* comparison this workload targets.
Benchmark persistence separately with batched transactions if that is the axis
you care about.

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
