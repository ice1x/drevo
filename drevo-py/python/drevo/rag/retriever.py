"""`Retriever`, `Context`, and `ContextStats` — the headline graph-RAG API.

`Retriever` resolves a seed (FTS query / node id / UUID) to one or
more seed nodes, expands each seed `hops` deep via
`expand_neighborhood`, and bundles the result into a `Context` ready
for `to_text(...)` consumption by an LLM prompt template.

`Context.to_text` is deterministic given the same context (sorted by
`(kind, title, id)` per RFC §8.4) — this is what `00120` e2e tests
assert on.
"""

from __future__ import annotations

import json
import uuid as _uuid
from dataclasses import asdict, dataclass
from typing import TYPE_CHECKING, Any, Iterable

from .neighborhood import expand_neighborhood

if TYPE_CHECKING:
    from .. import Drevo, Edge, Node


_VALID_FORMATS = ("markdown", "json", "turtle")


@dataclass(frozen=True)
class ContextStats:
    """Telemetry for a retrieved Context.

    Read after a `retrieve(...)` call to confirm that the BFS actually
    reached the requested depth, that `max_nodes` did not silently
    truncate the result, etc.
    """

    seed_count: int
    neighbour_count: int
    edge_count: int
    hops_used: int


@dataclass(frozen=True)
class Context:
    """Retriever output — the slice of the graph relevant to a query.

    `seeds` are the nodes the retriever hit first (FTS matches, the
    looked-up id, etc.). `neighbours` is the expanded BFS frontier
    minus the seeds. `edges` connects any pair drawn from
    `seeds | neighbours` that the BFS observed.

    The dataclass is frozen so a `Context` can be cached / hashed by
    downstream pipelines.
    """

    seeds: list["Node"]
    neighbours: list["Node"]
    edges: list["Edge"]
    stats: ContextStats

    def to_text(self, *, format: str = "markdown") -> str:
        """Format the context as LLM-ready text.

        Supported formats:
          - `"markdown"` — headings + bullet lists (default)
          - `"json"`     — JSON object {seeds, neighbours, edges, stats}
          - `"turtle"`   — RDF Turtle (for SPARQL / RAG hybrid stacks)

        Output is deterministic: nodes are sorted by `(kind, title, id)`
        and edges by `(from_id, to_id, id)`. The 00120 e2e test asserts
        byte-equality across runs.
        """
        if format not in _VALID_FORMATS:
            raise ValueError(
                f"Context.to_text: unsupported format {format!r}; "
                f"choose one of {_VALID_FORMATS}"
            )

        sorted_seeds = _sort_nodes(self.seeds)
        sorted_neighbours = _sort_nodes(self.neighbours)
        sorted_edges = _sort_edges(self.edges)

        if format == "markdown":
            return _render_markdown(sorted_seeds, sorted_neighbours, sorted_edges)
        if format == "json":
            return _render_json(sorted_seeds, sorted_neighbours, sorted_edges, self.stats)
        return _render_turtle(sorted_seeds, sorted_neighbours, sorted_edges)


class Retriever:
    """Composable seed → expanded-context retriever for graph-RAG.

    Construct once per workload (`hops`, `kind_filter`, `max_nodes`
    are baked in) and call `retrieve(...)` for each query. The class
    is intentionally stateless apart from the constructor args so the
    same Retriever can be reused across threads — `drevo.Drevo`
    releases the GIL around every storage I/O.
    """

    def __init__(
        self,
        drevo: "Drevo",
        *,
        hops: int = 2,
        kind_filter: list[str] | None = None,
        max_nodes: int = 50,
    ) -> None:
        if hops < 0:
            raise ValueError(f"Retriever: hops must be non-negative (got {hops})")
        if max_nodes < 1:
            raise ValueError(f"Retriever: max_nodes must be ≥ 1 (got {max_nodes})")
        self._drevo = drevo
        self._hops = hops
        self._kind_filter = list(kind_filter) if kind_filter is not None else None
        self._max_nodes = max_nodes

    def retrieve(
        self,
        seed: "str | int | _uuid.UUID",
        *,
        limit: int = 10,
    ) -> Context:
        """Resolve `seed` to one or more seed nodes, expand `hops`
        deep, and return a `Context`.

        `seed` is dispatched by type:
          - `str` → `Drevo.search_fts(seed, limit=limit)`
          - `int` → `Drevo.get_node(seed)`
          - `uuid.UUID` → `Drevo.get_node_by_uuid(seed.bytes)`

        Other types raise `TypeError`.
        """
        seeds = self._resolve_seeds(seed, limit=limit)
        return self._expand_seeds(seeds)

    def retrieve_with_embedding(
        self,
        embedding: "list[float]",
        *,
        limit: int = 10,
    ) -> Context:
        """Vector-similarity variant. Raises `NotImplementedError`
        until Phase 12 (`00075`) ships the HNSW index — RFC §8.3.
        """
        raise NotImplementedError(
            "Retriever.retrieve_with_embedding: vector index not yet "
            "available — tracked under Phase 12 task 00075. Use "
            "Retriever.retrieve(<fts query>) until then."
        )

    # ── internals ───────────────────────────────────────────────────

    def _resolve_seeds(self, seed: Any, *, limit: int) -> list["Node"]:
        if isinstance(seed, _uuid.UUID):
            node = self._drevo.get_node_by_uuid(seed.bytes)
            return [node] if node is not None else []
        # bool is a subclass of int — handle it before int.
        if isinstance(seed, bool):
            raise TypeError(
                "Retriever.retrieve: seed cannot be bool — pass int, str, or UUID"
            )
        if isinstance(seed, int):
            node = self._drevo.get_node(seed)
            return [node] if node is not None else []
        if isinstance(seed, str):
            hits = self._drevo.search_fts(seed, limit)
            return [h.node for h in hits]
        raise TypeError(
            f"Retriever.retrieve: seed must be str / int / uuid.UUID "
            f"(got {type(seed).__name__})"
        )

    def _expand_seeds(self, seeds: list["Node"]) -> Context:
        seen_ids: set[int] = {n.id for n in seeds}
        all_nodes: dict[int, Node] = {n.id: n for n in seeds}
        all_edges: dict[int, Edge] = {}
        max_hops_used = 0

        for seed_node in seeds:
            nh = expand_neighborhood(
                self._drevo,
                seed_node.uuid,
                hops=self._hops,
                kind_filter=self._kind_filter,
                max_nodes=self._max_nodes,
            )
            if nh.hops_used > max_hops_used:
                max_hops_used = nh.hops_used
            for n in nh.nodes:
                all_nodes.setdefault(n.id, n)
            for e in nh.edges:
                all_edges.setdefault(e.id, e)

        neighbours = [n for nid, n in all_nodes.items() if nid not in seen_ids]
        edges = list(all_edges.values())
        stats = ContextStats(
            seed_count=len(seeds),
            neighbour_count=len(neighbours),
            edge_count=len(edges),
            hops_used=max_hops_used,
        )
        return Context(seeds=list(seeds), neighbours=neighbours, edges=edges, stats=stats)


# ── rendering helpers ──────────────────────────────────────────────────


def _node_sort_key(n: "Node") -> tuple[str, str, int]:
    return (n.kind, n.title, n.id)


def _edge_sort_key(e: "Edge") -> tuple[int, int, int]:
    return (e.from_id, e.to_id, e.id)


def _sort_nodes(nodes: Iterable["Node"]) -> list["Node"]:
    return sorted(nodes, key=_node_sort_key)


def _sort_edges(edges: Iterable["Edge"]) -> list["Edge"]:
    return sorted(edges, key=_edge_sort_key)


def _node_dict(n: "Node") -> dict[str, Any]:
    return {
        "id": n.id,
        "uuid": str(n.uuid),
        "kind": n.kind,
        "title": n.title,
    }


def _edge_dict(e: "Edge") -> dict[str, Any]:
    return {
        "id": e.id,
        "uuid": str(e.uuid),
        "from_id": e.from_id,
        "to_id": e.to_id,
        "kind": e.kind,
        "weight": e.weight,
    }


def _render_markdown(
    seeds: list["Node"], neighbours: list["Node"], edges: list["Edge"]
) -> str:
    lines: list[str] = []
    lines.append("## Seeds")
    if not seeds:
        lines.append("- (none)")
    else:
        for n in seeds:
            lines.append(f"- {n.title} (kind={n.kind}, id={n.id})")
    lines.append("")
    lines.append("## Neighbours")
    if not neighbours:
        lines.append("- (none)")
    else:
        for n in neighbours:
            lines.append(f"- {n.title} (kind={n.kind}, id={n.id})")
    lines.append("")
    lines.append("## Edges")
    if not edges:
        lines.append("- (none)")
    else:
        for e in edges:
            lines.append(
                f"- {e.from_id} -[{e.kind}]-> {e.to_id} (id={e.id}, weight={e.weight})"
            )
    return "\n".join(lines) + "\n"


def _render_json(
    seeds: list["Node"],
    neighbours: list["Node"],
    edges: list["Edge"],
    stats: ContextStats,
) -> str:
    payload = {
        "seeds": [_node_dict(n) for n in seeds],
        "neighbours": [_node_dict(n) for n in neighbours],
        "edges": [_edge_dict(e) for e in edges],
        "stats": asdict(stats),
    }
    return json.dumps(payload, sort_keys=True, indent=2)


def _render_turtle(
    seeds: list["Node"], neighbours: list["Node"], edges: list["Edge"]
) -> str:
    lines: list[str] = []
    lines.append("@prefix drevo: <https://drevo.local/> .")
    lines.append("@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .")
    lines.append("")
    for role, nodes in (("Seed", seeds), ("Neighbour", neighbours)):
        for n in nodes:
            lines.append(f"drevo:node-{n.id} rdf:type drevo:{role} ;")
            lines.append(f'    drevo:kind "{_escape(n.kind)}" ;')
            lines.append(f'    drevo:title "{_escape(n.title)}" .')
    for e in edges:
        lines.append(
            f"drevo:edge-{e.id} rdf:type drevo:Edge ;\n"
            f"    drevo:from drevo:node-{e.from_id} ;\n"
            f"    drevo:to drevo:node-{e.to_id} ;\n"
            f'    drevo:kind "{_escape(e.kind)}" .'
        )
    return "\n".join(lines) + "\n"


def _escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"')
