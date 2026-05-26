"""Bounded BFS over the drevo graph — `expand_neighborhood`.

Pure Python on top of `Drevo.edges_of` + `Drevo.get_node`. The depth +
node-count caps live here (instead of pushing them into a Rust
traversal kernel) so callers can change the traversal policy from
Python without recompiling the cdylib.

This is one of two BFS surfaces in `drevo.rag`:
- `expand_neighborhood` (this module): free function, returns a
  `Neighborhood` dataclass with `hops_used` telemetry. Used by
  `Retriever` and by users who want raw context expansion.
- `Retriever.retrieve` (`retriever.py`): wraps `expand_neighborhood`
  per-seed and bundles the results into a `Context` ready for
  `to_text(...)`.
"""

from __future__ import annotations

import uuid as _uuid
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .. import Direction, Drevo, Edge, Node
else:
    # Runtime import via the parent package — same rationale as
    # `drevo.rag.ingest`: opt-in subpackage, no circular import risk.
    from .. import Direction


@dataclass(frozen=True)
class Neighborhood:
    """Bounded BFS result.

    `nodes` includes the root. `edges` is the set of edges connecting
    any pair of nodes in `nodes` that the BFS traversal saw.
    `hops_used` is the deepest BFS level at which a *new* node was
    discovered — useful for telemetry ("did my hops cap actually bite,
    or did the graph just run out of neighbours?").
    """

    nodes: list["Node"]
    edges: list["Edge"]
    hops_used: int


def _resolve_root(drevo: "Drevo", node_uuid: "_uuid.UUID | bytes | int") -> "Node":
    """Resolve the BFS root from a UUID, raw bytes, or a node id.

    Free functions in `drevo.rag` accept the same union of seed types
    as `Retriever.retrieve` so the two layers can be mixed-and-matched
    without an awkward "convert to UUID first" dance.
    """
    if isinstance(node_uuid, _uuid.UUID):
        node = drevo.get_node_by_uuid(node_uuid.bytes)
    elif isinstance(node_uuid, (bytes, bytearray)):
        node = drevo.get_node_by_uuid(bytes(node_uuid))
    elif isinstance(node_uuid, int):
        node = drevo.get_node(node_uuid)
    else:
        raise TypeError(
            f"expand_neighborhood: node_uuid must be uuid.UUID, bytes, or int "
            f"(got {type(node_uuid).__name__})"
        )
    if node is None:
        raise ValueError(f"expand_neighborhood: root not found ({node_uuid!r})")
    return node


def expand_neighborhood(
    drevo: "Drevo",
    node_uuid: "_uuid.UUID | bytes | int",
    *,
    hops: int = 2,
    kind_filter: list[str] | None = None,
    max_nodes: int = 50,
) -> Neighborhood:
    """Bounded BFS starting at the node identified by `node_uuid`.

    Walks `hops` levels deep, dropping nodes whose `kind` is not in
    `kind_filter` (if provided) and stopping early once `max_nodes`
    total nodes have been collected. The root is always included even
    if it would be excluded by `kind_filter` — the filter constrains
    the *frontier*, not the seed.

    Returns a `Neighborhood` with the visited nodes (BFS order, root
    first), the edges connecting them, and the actual depth reached.
    """

    if hops < 0:
        raise ValueError(f"hops must be non-negative (got {hops})")
    if max_nodes < 1:
        raise ValueError(f"max_nodes must be ≥ 1 (got {max_nodes})")

    kind_set = set(kind_filter) if kind_filter is not None else None

    root = _resolve_root(drevo, node_uuid)
    visited: dict[int, Node] = {root.id: root}
    edges_seen: list[Edge] = []
    edge_ids: set[int] = set()
    frontier: list[int] = [root.id]
    hops_used = 0

    for depth in range(hops):
        if not frontier or len(visited) >= max_nodes:
            break
        next_frontier: list[int] = []
        discovered_this_depth = False
        for nid in frontier:
            for edge in drevo.edges_of(nid, Direction.BOTH):
                other_id = edge.to_id if edge.from_id == nid else edge.from_id

                # Record the edge if we kept (or will keep) both endpoints.
                # When we're at the max_nodes cap and `other_id` is new,
                # we won't keep it, so skip the edge to keep the result
                # internally consistent (every edge endpoint is in `nodes`).
                if edge.id not in edge_ids and (other_id in visited or len(visited) < max_nodes):
                    edges_seen.append(edge)
                    edge_ids.add(edge.id)

                if other_id in visited:
                    continue
                if len(visited) >= max_nodes:
                    break

                node = drevo.get_node(other_id)
                if node is None:
                    continue
                if kind_set is not None and node.kind not in kind_set:
                    continue

                visited[other_id] = node
                next_frontier.append(other_id)
                discovered_this_depth = True
            if len(visited) >= max_nodes:
                break
        if discovered_this_depth:
            hops_used = depth + 1
        frontier = next_frontier

    return Neighborhood(
        nodes=list(visited.values()),
        edges=edges_seen,
        hops_used=hops_used,
    )
