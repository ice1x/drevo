# KG schema audit — 2026-05-26 cleanup

Project: `drevo` knowledge graph (Neo4j, MCP server `knowledge-graph`).
Branch: `feature/kg-snake-case-schema-unify`.
Companion to the original 2026-05-26 morning migration that did the first mechanical pass.

## Problem

The original `kg_migration_snake_case_2026_05_26` was a pure lowercase + separator replacement (`replace([- :./]→_)` + collapse `__→_`). It preserved any name that already matched `^[a-z0-9_]+$`, even when the name was unreadable run-together text. Two failure modes:

1. **Squashed-compound duplicates.** Two entities both already conformed to the regex but differed only in underscore placement. The proper one (`task_00037_http_api_server`) had been created by a later session that didn't notice the squashed twin (`task00037_httpapiscaffold`). `search_knowledge` for the proper spelling missed the twin.
2. **Status-suffix names.** `task_00116_python_package_wheels_landed` baked the lifecycle status into the identifier. The same task in the "pending" stage was `task_00116_python_package_wheels`. Both alive at once.

## What was done

Two migrations recorded in the project's `kg.migrations` table (visible via `MATCH (m:Migration {project:'drevo'}) RETURN m.seq, m.description, m.applied`).

### Migration seq 1 — duplicate merges

26 pairs merged via `apoc.refactor.mergeNodes([target, source], {properties: 'discard', mergeRels: true, produceSelfRel: false})`. Before each merge, the source's `observations` list was pre-concatenated onto the target so neither side's content was lost.

| source (deleted) | target (kept) |
|---|---|
| `task00037_httpapiscaffold` | `task_00037_http_api_server` |
| `task00038_httpnodecrud` | `task_00038_http_node_crud` |
| `task00039_httpedgeendpoints` | `task_00039_http_edge_endpoints` |
| `task00040_httptraversalendpoints` | `task_00040_http_traversal` |
| `task00041_httpsearchendpoint` | `task_00041_http_search` |
| `task00042_httpadminendpoints` | `task_00042_http_admin_endpoints` |
| `task00043_jsonerrorhandling` | `task_00043_json_error_handling` |
| `task_00044` | `task_00044_http_integration_tests` |
| `task00045_dockerfile` | `task_00045_dockerfile` |
| `task00046_httpedgeupdate` | `task_00046_http_edge_update` |
| `task00047_dockercompose` | `task_00047_docker_compose` |
| `task00048_healthreadyshutdown` | `task_00048_health_readiness_shutdown` |
| `task_00049_kubernetesmanifests` | `task_00049_k8s_manifests` |
| `task_00051` | `task_00051_ci_publish_ghcr` |
| `task_00052` | `task_00052_container_integration_test` |
| `task_00055_jsonimportexport` | `task_00055_json_import_export` |
| `task_00057_propertybasedtests` | `task_00057_proptest_graph_invariants` |
| `task_00058_ftstokenizerfuzz` | `task_00058_fts_tokenizer_fuzz` |
| `task_00060_rustdocpublicapis` | `task_00060_rustdoc_public_apis` |
| `task_00103_storageaudit` | `task_00103_audit_storage` |
| `task_00104_erroraudit` | `task_00104_audit_error` |
| `task_00105_modelaudit` | `task_00105_audit_model` |
| `task_00112_serveropsaudit` | `task_00112_audit_server_ops` |
| `task_00114_python_api_rfc` | `task_00114_python_api_surface_rfc` |
| `task_00116_python_package_wheels` | `task_00116_python_package_wheels_landed` |
| `phase8_5_auditrefactor` | `phase_8_5_audit_plan` |

### Migration seq 2 — standalone renames

12 in-place `SET n.name = new_name` renames where no canonical twin existed:

| old | new |
|---|---|
| `task_00116_python_package_wheels_landed` | `task_00116_python_package_wheels` (re-shortened — status moved to `.status` property) |
| `task00046_dockerignore` | `task_00046_dockerignore` |
| `phase16_task_00115` | `phase_16_task_00115` |
| `dumpformat_v1` | `dump_format_v1` |
| `graphnote_db_spec` | `graph_note_db_spec` |
| `task_graphnote_db_lifecycle` | `task_graph_note_db_lifecycle` |
| `task_rename_graphnote_to_drevo` | `task_rename_graph_note_to_drevo` |
| `scenariotest_cypher_bug_tracker` | `scenario_test_cypher_bug_tracker` |
| `scenariotest_cypher_cbt_journal` | `scenario_test_cypher_cbt_journal` |
| `scenariotest_cypher_erp` | `scenario_test_cypher_erp` |
| `scenariotest_cypher_story_editor` | `scenario_test_cypher_story_editor` |
| `scenariotest_cypher_task_manager` | `scenario_test_cypher_task_manager` |

### Strengthened convention

The `kg_schema_naming_convention` entity (type `SchemaConvention`) gained six new observations:

1. **Word boundaries are mandatory.** Every distinct English word in the identifier MUST be separated by `_`. The regex `^[a-z0-9_]+$` is necessary but not sufficient.
2. **Concrete anti-patterns** from this cleanup (no `taskNNNNN_` without an underscore between `task` and the number; no `httpapi`/`propertybased`/`scenariotest` multi-word compounds; `dockerfile`/`dockerignore`/`graphnote` as standalones are fine but split once paired with another descriptor).
3. **Status suffixes belong in `.status` properties, not in names.** `_landed`, `_done`, `_shipped`, `_wip`, `_v2` are property values, not identifiers.
4. **Stub-vs-descriptive merge rule.** A placeholder name (e.g. `task_00044`) must be merged into its descriptive twin (`task_00044_http_integration_tests`) the moment the twin is created.
5. **Migration record cross-link.** The two migrations are findable via `MATCH (m:Migration {project:'drevo'}) RETURN m`.
6. **Drift-detection query** for periodic audits (see below).

### Cleanup migration entity

Added `kg_migration_snake_case_cleanup_2026_05_26` (type `Migration`) with three relationships:
- `STRENGTHENS → kg_schema_naming_convention`
- `FOLLOWS → kg_migration_snake_case_2026_05_26`
- `APPLIES_TO → drevo`

## Verification

```cypher
// Pre-cleanup: 254 entities. Post-cleanup: 228 entities (−26 merged duplicates).
MATCH (n:Entity {project:'drevo'}) RETURN count(n);   // 228

// Regex conformance: 100%.
MATCH (n:Entity {project:'drevo'}) WHERE NOT n.name =~ '^[a-z0-9_]+$' RETURN n.name;   // (empty)

// Word-boundary drift detection — pairs that differ only in underscore placement.
MATCH (n:Entity {project:'drevo'}) WITH collect(n.name) AS names
UNWIND names AS a UNWIND names AS b WITH a, b
WHERE a < b AND replace(a,'_','') = replace(b,'_','')
RETURN a, b;   // (empty)
```

All three queries pass. The migrations are immutable in `kg.migrations` (applied=true, seq 1 + 2).

## Going forward

The strengthened convention is enforced by:
- **Convention-as-documentation** inside the KG (`kg_schema_naming_convention`). Future contributors discoverable via `search_knowledge`.
- **Periodic audit query** (above). Run it before any PR that adds 5+ KG entities to catch fresh drift early.
- **Optional future hardening**: a Cypher constraint `CREATE CONSTRAINT entity_name_snake_case FOR (n:Entity) REQUIRE n.name =~ '^[a-z0-9_]+$'`. This only catches non-conforming characters, not squashed compounds, so it's a necessary-but-not-sufficient guardrail.
