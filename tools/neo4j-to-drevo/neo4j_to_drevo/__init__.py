"""`neo4j_to_drevo` — import a Neo4j graph into drevo.

A standalone migration tool that **depends on `drevo`** (it writes through
the public bindings) while keeping `drevo` itself completely unaware of
Neo4j — the dependency points one way only.

    # one-shot CLI — offline dump → load (no live connection)
    python -m neo4j_to_drevo --apoc-json graph.json --drevo-path graph.redb

    # or programmatically
    import drevo
    from neo4j_to_drevo import migrate
    from neo4j_to_drevo.apoc import ApocJsonSource

    with drevo.Drevo.open("graph.redb") as db:
        report = migrate(ApocJsonSource("graph.json"), db)
        print(report.summary())

The engine (`migrate`) is source-agnostic — it consumes any `GraphSource`
yielding `SourceNode` / `SourceRelationship` records. Two adapters feed
it: `neo4j_to_drevo.apoc.ApocJsonSource` (offline APOC dump, stdlib-only)
and `neo4j_to_drevo.bolt.Neo4jSource` (live Bolt; the `neo4j` driver is an
optional extra, `pip install neo4j-to-drevo[live]`).
"""

from __future__ import annotations

from ._engine import (
    GraphSource,
    MigrationConfig,
    MigrationReport,
    SourceNode,
    SourceRelationship,
    migrate,
)

__all__ = [
    "GraphSource",
    "MigrationConfig",
    "MigrationReport",
    "SourceNode",
    "SourceRelationship",
    "migrate",
]
