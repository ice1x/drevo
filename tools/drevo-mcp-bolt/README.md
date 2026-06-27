# drevo-mcp-bolt

A **Bolt drop-in of the Neo4j knowledge-graph MCP**, pointed at drevo.

Same tools, same Cypher — just connected to drevo's Neo4j-compatible Bolt
endpoint instead of Neo4j. drevo's `drevo-server` speaks Bolt (with a `Neo4j/`
agent so the official driver accepts it) and the Cypher subset these tools use
(`MERGE` / `datetime()` / `SET +=` / map projection / `labels()` / `type()` /
`properties()` / `OPTIONAL MATCH` / `collect`), so it is a genuine copy-and-swap.

```
MCP client ──stdio(MCP)──▶ drevo-mcp-bolt (this) ──Bolt(neo4j driver)──▶ drevo-server :7687 ──▶ drevo.redb
```

The only drevo difference: `CREATE INDEX` schema DDL is unsupported (drevo
auto-indexes), so `_ensure_indexes` is best-effort (a no-op on drevo).

## Bring up drevo with Bolt

```bash
DREVO_UID=$(id -u) DREVO_GID=$(id -g) DREVO_DATA_DIR=~/drevo_data docker compose up -d
# the image sets DREVO_BOLT_PORT=7687 and publishes it
```

## Install + run

```bash
pip install -e tools/drevo-mcp-bolt
export DREVO_BOLT_URL=bolt://localhost:7687   # default; auth is accepted+ignored by drevo
python -m drevo_mcp_bolt                       # MCP over stdio
```

## Connect an MCP client

```json
"drevo-kg": {
  "command": "python",
  "args": ["-m", "drevo_mcp_bolt"],
  "env": { "DREVO_BOLT_URL": "bolt://localhost:7687" }
}
```

## Tools

`create_entity`, `add_observations`, `delete_entity`, `create_relationship`,
`delete_relationship`, `get_entity`, `search_knowledge`, `get_project_graph`,
`list_projects`, `add_migration`, `get_migrations`, `apply_migration`,
`run_cypher` — identical to the Neo4j MCP.

## Develop / test

```bash
cd tools/drevo-mcp-bolt
pip install -e ".[dev]"
pytest          # integration test spawns a local drevo-server if the binary is built
mypy --strict drevo_mcp_bolt/
ruff check . && black --check .
```
