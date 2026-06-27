"""Source-agnostic graph-import engine.

The engine consumes any `GraphSource` (a `nodes()` / `relationships()`
iterator pair yielding `SourceNode` / `SourceRelationship` records) and
loads it into a `drevo.Drevo` handle. It depends on `drevo` (it writes
through the public bindings) but `drevo` knows nothing about this tool —
the dependency points one way only.

Two structural mismatches between a labelled property graph (Neo4j) and
drevo are reconciled here:

* **Labels → kind.** A source node carries a *set* of labels; a drevo
  node has a single `kind` string. We join the label set with
  `config.label_join` for `kind` and preserve the full ordered set in a
  reserved `config.labels_property` (`_labels` by default) so the
  mapping is lossless and round-trips with drevo's Cypher multi-label
  convention.
* **Unique titles.** drevo enforces *globally unique* node titles
  (`DuplicateTitleError`); a property graph has no such concept. The
  engine resolves a title from `config.title_properties` (first present
  wins) and, on any collision within the run, disambiguates
  deterministically with the source id — which is unique — so a graph
  with ten "Alice" nodes migrates without a single title clash.
"""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Protocol, Sequence, runtime_checkable

from drevo import NewEdge, NewNode

if TYPE_CHECKING:
    from drevo import Drevo


# ── source records ───────────────────────────────────────────────────


@dataclass(frozen=True)
class SourceNode:
    """A graph node as presented by a source (Neo4j et al.).

    `id` is the source's own stable identifier (Neo4j `element_id`); it is
    used only to wire relationships and to disambiguate titles — it is not
    a drevo id. `labels` is the (possibly empty) ordered label set.
    """

    id: str
    labels: list[str]
    properties: dict[str, Any]


@dataclass(frozen=True)
class SourceRelationship:
    """A directed, typed relationship between two `SourceNode.id`s."""

    id: str
    type: str
    start: str
    end: str
    properties: dict[str, Any]


@runtime_checkable
class GraphSource(Protocol):
    """Anything the engine can migrate from.

    Both methods return a *fresh* iterator each call (the engine walks
    `nodes()` to completion before `relationships()`).
    """

    def nodes(self) -> Iterator[SourceNode]: ...

    def relationships(self) -> Iterator[SourceRelationship]: ...


# ── configuration ────────────────────────────────────────────────────


@dataclass(frozen=True)
class MigrationConfig:
    """Knobs controlling the source → drevo mapping.

    `title_properties` / `body_properties` are tried in order; the first
    present, non-null value becomes the node's title / body. `default_kind`
    is used when a node has no labels. `label_join` folds a multi-label
    set into the single `kind` string. `labels_property` is the reserved
    key under which the full label list is preserved. `weight_property`
    supplies an edge's weight when present (else `default_weight`).
    `on_error` is `"raise"` (default — fail loud on a dangling edge or a
    title clash that survives disambiguation) or `"skip"` (record the
    problem in `report.errors` and continue).
    """

    title_properties: Sequence[str] = ("title", "name", "id")
    body_properties: Sequence[str] = ("body", "text", "content", "description")
    default_kind: str = "node"
    label_join: str = ":"
    labels_property: str = "_labels"
    weight_property: str = "weight"
    default_weight: float = 1.0
    on_error: str = "raise"


# ── report ───────────────────────────────────────────────────────────


@dataclass
class MigrationReport:
    """Outcome of a migration run.

    `id_map` maps every successfully-migrated source id to its new drevo
    node id — the same map the engine uses internally to wire edges, and
    useful to a caller that wants to cross-reference afterwards.
    """

    nodes_created: int = 0
    nodes_skipped: int = 0
    edges_created: int = 0
    edges_skipped: int = 0
    errors: list[str] = field(default_factory=list)
    id_map: dict[str, int] = field(default_factory=dict)

    def summary(self) -> str:
        """One-line human summary for CLI / log output."""
        return (
            f"{self.nodes_created} nodes ({self.nodes_skipped} skipped), "
            f"{self.edges_created} edges ({self.edges_skipped} skipped), "
            f"{len(self.errors)} error(s)"
        )


# ── property coercion ────────────────────────────────────────────────


def _coerce_value(value: Any) -> Any:
    """Reduce a source property value to something drevo can store.

    drevo properties are JSON values. Neo4j temporal / spatial types
    (`DateTime`, `Date`, `Time`, `Duration`, `Point`) are not
    JSON-serialisable, so anything with an `isoformat()` is converted to
    its ISO string, containers are coerced element-wise, JSON primitives
    pass through untouched, and any remaining object falls back to
    `str(...)` (lossy but never raises). The function is `neo4j`-free so
    the engine has no hard dependency on the driver.
    """
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_coerce_value(v) for v in value]
    if isinstance(value, dict):
        return {str(k): _coerce_value(v) for k, v in value.items()}
    if isinstance(value, (bytes, bytearray)):
        return list(value)
    iso = getattr(value, "isoformat", None)
    if callable(iso):
        return iso()
    return str(value)


def _coerce_properties(props: dict[str, Any]) -> dict[str, Any]:
    return {str(k): _coerce_value(v) for k, v in props.items()}


# ── mapping helpers ──────────────────────────────────────────────────


def _resolve_kind(node: SourceNode, config: MigrationConfig) -> str:
    if not node.labels:
        return config.default_kind
    return config.label_join.join(node.labels)


def _resolve_field(props: dict[str, Any], keys: Sequence[str]) -> str | None:
    for key in keys:
        if key in props and props[key] is not None:
            return str(props[key])
    return None


def _resolve_title(node: SourceNode, config: MigrationConfig, used: set[str]) -> str:
    """Resolve a globally-unique title for `node`.

    Preference order: first present `title_properties` value; else a
    synthesized `<kind>#<source-id>`. On any collision with a title
    already used this run, disambiguate by appending the source id (which
    is unique), guaranteeing no `DuplicateTitleError`.
    """
    base = _resolve_field(node.properties, config.title_properties)
    if base is None or base == "":
        base = f"{_resolve_kind(node, config)}#{node.id}"
    title = base
    if title in used:
        title = f"{base} ({node.id})"
    # Pathological: even the id-disambiguated form was used (e.g. a source
    # property literally equal to it). Spin a numeric suffix until unique.
    suffix = 1
    while title in used:
        title = f"{base} ({node.id}#{suffix})"
        suffix += 1
    used.add(title)
    return title


def _resolve_weight(rel: SourceRelationship, config: MigrationConfig) -> float:
    raw = rel.properties.get(config.weight_property)
    if isinstance(raw, (int, float)) and not isinstance(raw, bool):
        return float(raw)
    return config.default_weight


# ── engine ───────────────────────────────────────────────────────────

#: How many nodes/edges to fold into one batched write (a single redb
#: transaction). Bounds memory and transaction size for large imports while
#: still collapsing per-item fsyncs (N fsyncs -> ceil(N / _BATCH_SIZE)).
_BATCH_SIZE = 1000


def migrate(
    source: GraphSource,
    db: "Drevo",
    *,
    config: MigrationConfig | None = None,
) -> MigrationReport:
    """Migrate every node and relationship from `source` into `db`.

    Nodes are created first (building the source-id → drevo-id map), then
    relationships are wired through that map. Returns a `MigrationReport`
    with counts, the id map, and any recorded errors. With
    `config.on_error == "raise"` a dangling relationship (an endpoint that
    was never migrated) raises `KeyError`; with `"skip"` it is counted in
    `edges_skipped` and noted in `errors`.
    """
    cfg = config or MigrationConfig()
    report = MigrationReport()
    used_titles: set[str] = set()

    # Nodes first, in batches, so each chunk commits in one redb transaction
    # and the source-id -> drevo-id map is fully built before edges are wired.
    node_batch: list[tuple[str, NewNode]] = []

    def _flush_nodes() -> None:
        if not node_batch:
            return
        created = db.create_nodes([new_node for _src, new_node in node_batch])
        for (src_id, _new_node), node in zip(node_batch, created):
            report.id_map[src_id] = node.id
            report.nodes_created += 1
        node_batch.clear()

    for node in source.nodes():
        kind = _resolve_kind(node, cfg)
        title = _resolve_title(node, cfg, used_titles)
        body = _resolve_field(node.properties, cfg.body_properties) or ""
        properties = _coerce_properties(node.properties)
        properties[cfg.labels_property] = list(node.labels)
        node_batch.append(
            (node.id, NewNode(kind=kind, title=title, body=body, properties=properties))
        )
        if len(node_batch) >= _BATCH_SIZE:
            _flush_nodes()
    _flush_nodes()

    # Edges next, in batches. Dangling endpoints are filtered here (skip/raise)
    # so each batch handed to create_edges has only valid, existing endpoints.
    edge_batch: list[NewEdge] = []

    def _flush_edges() -> None:
        if not edge_batch:
            return
        db.create_edges(edge_batch)
        report.edges_created += len(edge_batch)
        edge_batch.clear()

    for rel in source.relationships():
        from_id = report.id_map.get(rel.start)
        to_id = report.id_map.get(rel.end)
        if from_id is None or to_id is None:
            missing = rel.start if from_id is None else rel.end
            message = f"relationship {rel.id!r} ({rel.type}) references unmigrated node {missing!r}"
            if cfg.on_error == "skip":
                report.edges_skipped += 1
                report.errors.append(message)
                continue
            raise KeyError(message)
        edge_batch.append(
            NewEdge(
                from_id=from_id,
                to_id=to_id,
                kind=rel.type,
                weight=_resolve_weight(rel, cfg),
                properties=_coerce_properties(rel.properties),
            )
        )
        if len(edge_batch) >= _BATCH_SIZE:
            _flush_edges()
    _flush_edges()

    return report
