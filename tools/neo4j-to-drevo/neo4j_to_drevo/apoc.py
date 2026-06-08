"""APOC-JSON dump source — the offline dump → load path.

Reads a file produced by Neo4j's APOC export —

    // in neo4j (APOC installed):
    CALL apoc.export.json.all('graph.json', {useTypes:true})

— and presents it as the engine's source-agnostic `GraphSource`. The dump
is **JSON Lines**: one JSON object per line, each tagged with
`"type": "node"` or `"type": "relationship"`. A node carries `labels` +
`properties`; a relationship carries `label` (its type), `start`/`end`
endpoint objects, and `properties`.

Pure standard-library `json` — no `neo4j` driver and no database
connection needed at load time, so a graph can be migrated long after the
source Neo4j is gone, on a machine that never had Bolt access.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from typing import TYPE_CHECKING, Any

from ._engine import SourceNode, SourceRelationship, _coerce_properties

if TYPE_CHECKING:
    from ._engine import GraphSource


def _element_id(obj: dict[str, Any]) -> str:
    """Stable identifier for a node / endpoint / relationship.

    APOC emits both the modern `elementId` (a string) and the legacy
    numeric `id`. We prefer `elementId` when present and fall back to
    `id` — applied identically to nodes and to relationship `start`/`end`
    endpoints, so the id used to *wire* an edge always matches the id the
    node was *recorded* under.
    """
    raw = obj.get("elementId")
    if raw is None:
        raw = obj.get("id")
    return str(raw)


class ApocJsonSource:
    """A `GraphSource` backed by an `apoc.export.json.all` dump file.

    Re-iterable: `nodes()` and `relationships()` each reopen and rescan
    the file, so the engine's two passes never exhaust a shared cursor.
    Blank lines are skipped; every non-blank line must be a JSON object.
    """

    def __init__(self, path: str) -> None:
        self._path = path

    def _records(self) -> Iterator[dict[str, Any]]:
        with open(self._path, encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if line:
                    yield json.loads(line)

    def nodes(self) -> Iterator[SourceNode]:
        for obj in self._records():
            if obj.get("type") == "node":
                yield SourceNode(
                    id=_element_id(obj),
                    labels=list(obj.get("labels") or []),
                    properties=dict(obj.get("properties") or {}),
                )

    def relationships(self) -> Iterator[SourceRelationship]:
        for obj in self._records():
            if obj.get("type") == "relationship":
                yield SourceRelationship(
                    id=_element_id(obj),
                    # APOC puts the relationship *type* in `label`; the
                    # `type` key is the node/rel discriminator.
                    type=str(obj.get("label") or ""),
                    start=_element_id(obj["start"]),
                    end=_element_id(obj["end"]),
                    properties=dict(obj.get("properties") or {}),
                )


def write_apoc_json(source: "GraphSource", path: str) -> tuple[int, int]:
    """Serialise any `GraphSource` to an APOC-compatible JSON-Lines dump.

    Writes the same shape `ApocJsonSource` reads, so a `dump` produced here
    round-trips through a later `--apoc-json` import. Properties are run
    through the engine's JSON coercion, so a live-Bolt source carrying
    Neo4j temporal/spatial values still serialises cleanly. Returns the
    `(nodes, relationships)` count written.
    """
    nodes = edges = 0
    with open(path, "w", encoding="utf-8") as handle:
        for node in source.nodes():
            handle.write(
                json.dumps(
                    {
                        "type": "node",
                        "id": node.id,
                        "labels": list(node.labels),
                        "properties": _coerce_properties(node.properties),
                    }
                )
                + "\n"
            )
            nodes += 1
        for rel in source.relationships():
            handle.write(
                json.dumps(
                    {
                        "type": "relationship",
                        "id": rel.id,
                        "label": rel.type,
                        "start": {"id": rel.start},
                        "end": {"id": rel.end},
                        "properties": _coerce_properties(rel.properties),
                    }
                )
                + "\n"
            )
            edges += 1
    return nodes, edges
