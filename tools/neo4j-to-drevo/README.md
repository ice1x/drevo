# neo4j-to-drevo

Migrate a Neo4j graph into a [drevo](../../README.md) database.

This is a **standalone tool that depends on `drevo`** — the database and
its Python bindings (`drevo`) know nothing about Neo4j. The dependency
points one way only: `neo4j-to-drevo` imports `drevo`, never the reverse.

It is a pure **database → database** data move (dump + load), independent
of how you later *query* drevo (HTTP, MCP, embedded).

## Three phases, three commands

```bash
# 1. DUMP a live Neo4j (over Bolt) to a local JSON-Lines file
pip install "neo4j-to-drevo[live]"     # pulls the official neo4j driver
python -m neo4j_to_drevo dump \
    --neo4j-uri bolt://localhost:7687 --neo4j-user neo4j \
    --neo4j-password "$DREVO_NEO4J_PASSWORD" \
    --out graph.json

# 2. DRY RUN — read + map + validate, report what WOULD happen, write nothing
python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb --dry-run
# → [dry run] would import: 1240 nodes (0 skipped), 5310 edges (0 skipped), 0 error(s)
# → [dry run] nothing written to graph.redb

# 3. IMPORT for real
python -m neo4j_to_drevo import --apoc-json graph.json --drevo-path graph.redb
# → migration complete: 1240 nodes (0 skipped), 5310 edges (0 skipped), 0 error(s)
```

The dry run uses a throwaway in-memory drevo to exercise the *full*
mapping — including drevo's globally-unique-title check and dangling-edge
detection — so it surfaces every conflict you'd hit for real, then
discards the result. Combine with `--on-error skip` to preview which edges
reference unmigrated endpoints.

### Source options for `import`

- `--apoc-json <file>` — an offline dump. Either the file produced by
  phase 1 (`dump`), or one exported inside Neo4j with APOC:
  `CALL apoc.export.json.all('graph.json', {useTypes:true})`. Pure stdlib,
  no driver, no live connection — a graph migrates long after the source
  Neo4j is gone.
- `--neo4j-uri bolt://… --neo4j-user … --neo4j-password …` — stream
  straight from a running Neo4j, skipping the dump file entirely (needs
  the `[live]` extra).

Target: `--drevo-path <file>` (or `--in-memory` for a smoke test).

## Programmatic use

```python
import drevo
from neo4j_to_drevo import migrate
from neo4j_to_drevo.apoc import ApocJsonSource

with drevo.Drevo.open("graph.redb") as db:
    report = migrate(ApocJsonSource("graph.json"), db)
    print(report.summary())
```

## Model mapping

- **Labels → kind.** Neo4j label *sets* fold into drevo's single `kind`
  (joined with `MigrationConfig.label_join`); the full ordered set is
  preserved under the reserved `_labels` property.
- **Unique titles.** drevo enforces globally-unique node titles; the
  title is resolved from `title` / `name` / `id` and any collision is
  disambiguated with the unique source id (so N identically-named nodes
  import with zero `DuplicateTitleError`).
- **Property types.** Neo4j temporal/spatial values coerce to
  JSON-storable forms (`isoformat()` / element-wise / `str`).
- **Dangling edges.** `--on-error skip` records edges to unmigrated
  endpoints in the report instead of failing.

The `migrate()` engine is source-agnostic — it consumes any `GraphSource`
yielding `SourceNode` / `SourceRelationship`, so the same path backs both
the APOC-dump and live-Bolt adapters (and any future source).

## Develop / test

```bash
cd tools/neo4j-to-drevo
pip install -e ".[dev]"        # needs `drevo` importable (build drevo-py first)
pytest
mypy --strict neo4j_to_drevo
ruff check . && black --check .
```
