# Cypher ↔ Neo4j parity diff (task `00124`, Phase 10.5 layer 2)

The **hard gate** between Phase 10 (Cypher executor) and Phase 11 (Bolt wire
protocol): proves drevo's Cypher *answers* match Neo4j's on a curated,
feature-tagged corpus before official Neo4j drivers ever talk to drevo.

Harness: [`tests/cypher_neo4j_parity.rs`](../cypher_neo4j_parity.rs).

## Two halves

| Half | When | Needs | What it checks |
|------|------|-------|----------------|
| **drevo baseline** (always-on tests) | every PR | nothing | corpus runs through `parse → execute`, normalised, diffed against `golden/baseline.jsonl` — a fast drevo-regression guard |
| **live Neo4j parity** (`#[ignore]`) | on demand | Docker + `cypher-shell` | the same corpus run through real Neo4j 5.x, diffed against the golden, ≥ 95 % match, tagged by feature class |

Neither half runs Docker on CI — the live half is `#[ignore]` and gated behind
a Bolt-port reachability probe, so it skips cleanly without infra.

## Golden baseline

`golden/baseline.jsonl` holds one JSON line per corpus query: `{id, tags,
schema, columns, rows}`. The rows are drevo's **normalised** output — floats
rounded to 6 dp, unordered results canonically sorted, nodes/relationships
reduced to content (labels/type + properties, never storage id/uuid).

Regenerate after an intentional Cypher behaviour change:

```sh
DREVO_UPDATE_GOLDEN=1 cargo test --test cypher_neo4j_parity
```

`SCHEMA_VERSION` in the harness is asserted against every golden line, so a
dataset change with a stale golden fails loudly instead of silently passing.

## Running the live parity diff

```sh
# 1. start Neo4j 5.x
docker compose -f tests/cypher_neo4j_parity/docker-compose.yml up -d

# 2. wait until Bolt is up (port 7687 accepts / logs show "Bolt enabled"),
#    then run the ignored test
cargo test --test cypher_neo4j_parity -- --ignored --nocapture

# 3. tear down
docker compose -f tests/cypher_neo4j_parity/docker-compose.yml down -v
```

Credentials (`neo4j` / `drevoparity`) are shared between the compose file and
the test's `cypher_shell()` helper.

## Extending the corpus

The corpus is the seed the roadmap grows toward ~100 entries. To add a query:

1. Append a `ParityQuery { id, tags, cypher }` to `corpus()`. Use only
   features the Phase 10 executor supports (`00063`–`00069`).
2. Tag it by feature class so the diff report can attribute drift.
3. Regenerate the golden (`DREVO_UPDATE_GOLDEN=1 …`).
4. Inspect the new golden line before committing.
