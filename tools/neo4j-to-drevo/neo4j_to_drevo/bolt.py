"""Live Neo4j (Bolt) source — streams a running database into the engine.

`Neo4jSource` wraps a live `neo4j` driver and presents it as the engine's
source-agnostic `GraphSource`: it streams every node (`MATCH (n) RETURN
n`) and every relationship (`MATCH ()-[r]->() RETURN r`), mapping the
driver's `graph.Node` / `graph.Relationship` records to `SourceNode` /
`SourceRelationship`.

The `neo4j` package is an **optional** dependency of this tool
(`pip install neo4j-to-drevo[live]`). It is imported lazily inside
`connect()`, so importing the package — and using the offline
`ApocJsonSource` path — never requires the driver. `Neo4jSource(driver)`
itself takes an already-constructed driver (real or mock), so the
adapter's mapping logic is testable with no driver installed at all.
"""

from __future__ import annotations

from collections.abc import Iterator
from types import TracebackType
from typing import Any, Literal

from ._engine import SourceNode, SourceRelationship

# Full scans. Streaming is the driver's job (`session.run` returns a
# lazy result cursor); we iterate it without ever materialising the
# whole graph in Python.
_NODE_QUERY = "MATCH (n) RETURN n"
_REL_QUERY = "MATCH ()-[r]->() RETURN r"


class Neo4jSource:
    """A `GraphSource` backed by a `neo4j` driver.

    Construct with an existing driver (`Neo4jSource(driver)`), or use
    `Neo4jSource.connect(uri, user, password)` to build the driver from
    connection parameters (requires the `neo4j` package). Usable as a
    context manager — `with Neo4jSource.connect(...) as src:` closes the
    driver on exit.
    """

    def __init__(self, driver: Any, *, database: str | None = None) -> None:
        self._driver = driver
        self._database = database

    @classmethod
    def connect(
        cls,
        uri: str,
        user: str,
        password: str,
        *,
        database: str | None = None,
    ) -> "Neo4jSource":
        """Open a driver to `uri` with basic auth and wrap it.

        Raises a clear `ImportError` (rather than a bare
        `ModuleNotFoundError`) if the optional `neo4j` dependency is
        missing, so the operator knows exactly what to install.
        """
        try:
            import neo4j  # type: ignore[import-not-found]  # optional extra
        except ImportError as exc:  # pragma: no cover - exercised via message
            raise ImportError(
                "the `neo4j` driver is required for live migration; "
                "install it with `pip install neo4j-to-drevo[live]`"
            ) from exc
        driver = neo4j.GraphDatabase.driver(uri, auth=(user, password))
        return cls(driver, database=database)

    def _session(self) -> Any:
        if self._database is not None:
            return self._driver.session(database=self._database)
        return self._driver.session()

    def nodes(self) -> Iterator[SourceNode]:
        with self._session() as session:
            for record in session.run(_NODE_QUERY):
                node = record["n"]
                yield SourceNode(
                    # `node.labels` is an unordered frozenset; sort it so
                    # `kind` (the joined label string) is stable across runs.
                    id=str(node.element_id),
                    labels=sorted(node.labels),
                    properties=dict(node.items()),
                )

    def relationships(self) -> Iterator[SourceRelationship]:
        with self._session() as session:
            for record in session.run(_REL_QUERY):
                rel = record["r"]
                yield SourceRelationship(
                    id=str(rel.element_id),
                    type=rel.type,
                    start=str(rel.start_node.element_id),
                    end=str(rel.end_node.element_id),
                    properties=dict(rel.items()),
                )

    def close(self) -> None:
        self._driver.close()

    def __enter__(self) -> "Neo4jSource":
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> Literal[False]:
        self.close()
        return False
