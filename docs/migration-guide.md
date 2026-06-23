# Migration Guide

How to move an existing **Neo4j** database into drevo, and what to expect from drevo's
Neo4j-compatibility surface afterwards.

drevo is deliberately ignorant of Neo4j: the database and its Python bindings know nothing
about it. Migration is handled by a **standalone tool** — [`tools/neo4j-to-drevo`](../tools/neo4j-to-drevo)
— that depends on `drevo`, never the reverse. It is a pure database → database data move,
independent of how you later query drevo (HTTP, Bolt, MCP, or embedded).

---

## 1. The three-phase migration

```bash
pip install "neo4j-to-drevo[live]"   # the [live] extra pulls the official neo4j driver
```

### Phase 1 — Dump

Stream a live Neo4j over Bolt to a local JSON-Lines file:

```bash
python -m neo4j_to_drevo dump \
    --neo4j-uri bolt://localhost:7687 \
    --neo4j-user neo4j \
    --neo4j-password "$DREVO_NEO4J_PASSWORD" \
    --out graph.json
```

(You can skip this and use an APOC export instead — see [source options](#source-options).)

### Phase 2 — Dry run

Read, map, and validate everything, writing **nothing** — the dry run uses a throwaway
in-memory drevo to exercise the *full* mapping, including drevo's globally-unique-title check
and dangling-edge detection, so it surfaces every conflict you'd hit for real:

```bash
python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb --dry-run
# → [dry run] would import: 1240 nodes (0 skipped), 5310 edges (0 skipped), 0 error(s)
```

Combine with `--on-error skip` to preview which edges reference unmigrated endpoints.

### Phase 3 — Import

Run it for real:

```bash
python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb
# → migration complete: 1240 nodes (0 skipped), 5310 edges (0 skipped), 0 error(s)
```

---

## 2. Source and target options

### Source options

- `--apoc-json <file>` — an **offline dump**. Either the file from phase 1, or one exported
  inside Neo4j with APOC:
  `CALL apoc.export.json.all('graph.json', {useTypes:true})`. Pure stdlib, no driver, no live
  connection — a graph can migrate long after the source Neo4j is gone.
- `--neo4j-uri … --neo4j-user … --neo4j-password …` — stream straight from a running Neo4j,
  skipping the dump file (needs the `[live]` extra). The password can also come from
  `DREVO_NEO4J_PASSWORD`.

Both sources feed the same transport-agnostic `migrate()` engine.

### Target options

- `--drevo-path <file>` — the destination redb file (created if missing).
- `--in-memory` — an ephemeral target for smoke tests.

### Programmatic use

```python
import drevo
from neo4j_to_drevo import migrate
from neo4j_to_drevo.apoc import ApocJsonSource

with drevo.Drevo.open("graph.redb") as db:
    report = migrate(ApocJsonSource("graph.json"), db)
    print(report.summary())
```

---

## 3. How the model maps

Neo4j and drevo are both labelled property graphs, but two model differences need translating:

### Labels → `kind`

Neo4j nodes carry a **set** of labels; drevo nodes have a single `kind` string. The migration
joins the label set with a separator (default `;`) into `kind`, and preserves the full set in a
reserved `_labels` property.

- `["Person", "Developer"]` → `kind = "Person;Developer"`, `_labels = ["Person", "Developer"]`.
- drevo's Cypher `MATCH (n:Person:Developer)` then matches such a node by any of its labels.

### Globally-unique titles

drevo enforces **globally-unique node titles**. The migration resolves a title from the
`title` / `name` / `id` fields (in that order) and disambiguates collisions by appending the
source node id — so a migration never fails on a `DuplicateTitleError` and is idempotent.

### Property coercion

Neo4j temporal and spatial values are coerced to JSON-storable forms (temporals →
ISO-8601 strings, spatials → arrays); everything else round-trips as JSON.

### Dangling edges

A relationship whose endpoint wasn't migrated is handled per `--on-error`:

- `raise` (default) — fail the migration.
- `skip` — record it in the report and continue.

---

## 4. After migrating: the Neo4j-compatibility surface

Once your data is in drevo you keep much of the Neo4j developer experience:

- **Bolt protocol** — drevo speaks Bolt on port `7687`, so the official Neo4j drivers (Python,
  JavaScript, Go, Java) connect unchanged. See
  [SDK Reference → Bolt](sdk-reference.md#bolt-protocol).
- **Cypher** — drevo implements a growing subset of openCypher: `MATCH` / `OPTIONAL MATCH` /
  `CREATE` / `MERGE` / `SET` / `REMOVE` / `DELETE`, `WHERE`, `WITH`, aggregation, variable-length
  paths, and parameters. Unimplemented constructs return a deterministic, named error rather
  than a wrong answer — see the [Cypher Reference](cypher-reference.md) and its
  [Not yet supported](cypher-reference.md#not-yet-supported) table.

### What to check before you switch a workload over

| Neo4j feature | drevo status |
|---------------|--------------|
| Bolt drivers | ✅ Supported (port 7687). |
| Core read/write Cypher | ✅ Supported subset (see reference). |
| `UNWIND` | ✅ Supported (list expansion; composes with `MATCH` / `WITH` / `CREATE`). |
| `FOREACH (x IN list \| …)` | ✅ Supported (bulk update; body restricted to update clauses, `null` list is a no-op). |
| `UNION` / `UNION ALL` | ✅ Supported (arms must share column names; no mixing the two). |
| `CASE … WHEN … THEN … END` | ✅ Supported (generic & simple forms; aggregations may appear inside an arm). |
| Scalar functions (`toLower`, `size`, `coalesce`, `range`, `keys`, …) | ✅ Supported (string / numeric / list library; see reference). |
| `CALL` / `YIELD` — built-in `db.*` introspection (`db.labels`, `db.relationshipTypes`, `db.propertyKeys`) | ✅ Supported (standalone or `YIELD … WHERE`). |
| User-defined / `apoc.*` / `gds.*` procedures | ⛔ Only the built-in `db.*` procedures exist. |
| Regex `=~` | ✅ Supported (full-string match; common Java/Neo4j subset incl. `(?i)`). |
| List / map indexing & slicing (`xs[0]`, `xs[1..3]`, `m['k']`) | ✅ Supported. |
| List comprehension (`[x IN list WHERE p \| proj]`) | ✅ Supported (filter / project / both; `null` list → `null`). |
| Map projection (`n {.title, .*, k: expr, var}`) | ✅ Supported (node / relationship / map base; `null` base → `null`). |
| List predicates (`all` / `any` / `none` / `single` `(x IN list WHERE p)`) | ✅ Supported (three-valued; `null` list → `null`). |
| Pattern comprehension (`[(a)-[:R]->(b) WHERE p \| proj]`) | ✅ Supported (anchored on bound vars; no match / `null` anchor → `[]`). |
| Pattern predicate (`WHERE (a)-[:R]->(b)`, `NOT (a)-[:R]->()`) | ✅ Supported (existence test; anchored on bound vars; `null` anchor → `null`). |
| Existential subquery (`EXISTS { [MATCH] pattern [WHERE pred] }`) | ✅ Supported (optional `MATCH` keyword + inner `WHERE`; bare node legal; `null` anchor → `null`). The deprecated `exists(n.prop)` function form is not — use `n.prop IS NOT NULL`. |
| Anonymous nodes in `MATCH` (`(:Label)-->(b)`, `()-->(b)`, `(a)-->()-->(c)`) | ✅ Supported (head & intermediate). |
| Multi-label nodes | ✅ Via `kind` + `_labels` (any-of match). |
| APOC export → import | ✅ Via this migration tool. |

When a query relies on an unsupported construct, express the intent through drevo's
[Rust](sdk-reference.md#rust-api) or [Python](sdk-reference.md#python-sdk) API, which expose the
traversal, FTS, and vector primitives directly.

---

## 5. Why a separate tool?

drevo bundles no Neo4j dependency by design: an adapter for an external system lives in its own
package that depends on the core, never inside it. This keeps the database embeddable and
WASM-safe, and lets the migration tool evolve (new source formats, new drivers) without
touching the database. The same principle governs drevo's PostgreSQL CDC support
(see the [Admin Guide](admin-guide.md#cdc-from-postgresql)) — a *decoder* compatible with an
external format is a core feature, but an *importer* that talks to a live external system is a
separate tool.
