"""Public Python API for drevo — embedded graph database.

This module is the pure-Python shim that ships in the
`drevo-py` wheel alongside the compiled `_drevo` extension. End users
import this package; the underscore-prefixed `_drevo` cdylib is an
implementation detail.

The shim's job is small but load-bearing:

1. Re-export every public symbol from `_drevo` so
   ``from drevo import Drevo`` resolves at the top level (the cdylib
   itself lives at ``drevo._drevo``).
2. Wrap the raw 16-byte ``bytes`` UUIDs returned by ``_drevo.Node.uuid``
   and ``_drevo.Edge.uuid`` as native :class:`uuid.UUID` objects — per
   RFC §3.2 (default position) and the §12.2 amendment that defers the
   conversion from the PyO3 layer to this shim, keeping the Rust ↔ Python
   boundary free of ``import uuid`` calls on the hot path.
3. Attach :class:`drevo.errors.InvalidWeightError` (a ``ValueError``
   subclass) so callers can catch the typed exception even though the
   PyO3 layer raises a plain ``PyValueError`` for
   ``DrevoError::InvalidWeight`` (RFC §12.3).

Implements the contract in ``audit/RFC-python-api.md`` — Phase 16 task
``00116``.
"""

from __future__ import annotations

import uuid as _uuid
from typing import Any

# Compiled cdylib (Rust + PyO3). Imported as a sub-module via the dotted
# `module-name = "drevo._drevo"` declaration in `pyproject.toml` so the
# relative import below resolves on every supported platform.
from . import _drevo

# Re-export every public class the PyO3 module ships. Source of truth:
# `drevo-py/src/lib.rs` — every `m.add_class::<...>()` and
# `m.add("...", py.get_type::<...>())` call has a matching name here.
# Handle. Imported separately so the UUID-wrapping decorator below can
# patch its `__init_subclass__`-style hooks without confusing the
# attribute re-exports above.
from ._drevo import (  # type: ignore[attr-defined]
    # Plain-data wrappers.
    CompactReport,
    # Exception hierarchy.
    ConflictError,
    Direction,
    Drevo,  # type: ignore[attr-defined]
    DrevoError,
    DuplicateTitleError,
    Edge,
    EdgeNotFoundError,
    EdgePatch,
    ImportReport,
    LockedError,
    NewEdge,
    NewNode,
    Node,
    NodeNotFoundError,
    NodePatch,
    NotFoundError,
    PanicError,
    ScoredNode,
    SerializationError,
    StorageError,
    SubGraph,
)

# Pure-Python `InvalidWeightError(ValueError)` — see RFC §12.3. The
# PyO3 layer raises `PyValueError` directly for `DrevoError::InvalidWeight`,
# and this subclass lets callers narrow the catch to the drevo-specific
# variant: `except drevo.InvalidWeightError` works after `00116`, while
# `except ValueError` continues to work.
from .errors import InvalidWeightError

# `__version__` is set in Rust (`drevo-py/src/lib.rs` —
# `m.add("__version__", env!("CARGO_PKG_VERSION"))?`). Surfacing it here
# means `import drevo; drevo.__version__` works without depending on
# `_drevo` internals.
__version__: str = _drevo.__version__

__all__ = [
    # Handle.
    "Drevo",
    # Plain-data wrappers.
    "CompactReport",
    "Direction",
    "Edge",
    "EdgePatch",
    "ImportReport",
    "NewEdge",
    "NewNode",
    "Node",
    "NodePatch",
    "ScoredNode",
    "SubGraph",
    # Exceptions.
    "ConflictError",
    "DrevoError",
    "DuplicateTitleError",
    "EdgeNotFoundError",
    "InvalidWeightError",
    "LockedError",
    "NodeNotFoundError",
    "NotFoundError",
    "PanicError",
    "SerializationError",
    "StorageError",
    # Version.
    "__version__",
]


# ── UUID wrapping ──────────────────────────────────────────────────────
#
# The PyO3 surface returns raw `bytes` (length 16) for `Node.uuid` and
# `Edge.uuid`. RFC §3.2 promises `uuid.UUID` on the Python side; the
# §12.2 amendment defers the conversion to this shim layer so the
# Rust ↔ Python boundary stays free of `import uuid` calls on the
# hot path (`Node.uuid` is read once per object — by the time a caller
# touches it, they have already paid for one Python ↔ Rust round-trip).
#
# We monkey-patch the `_drevo.Node.uuid` and `_drevo.Edge.uuid`
# properties with wrappers that call `uuid.UUID(bytes=...)` on the raw
# bytes returned by the underlying frozen `#[pyclass]`. Frozen pyclasses
# expose their fields via getters but PyO3 still publishes them as
# regular Python descriptors — `property()` overrides resolve before
# the original descriptor at attribute lookup time.

_node_uuid_raw = Node.uuid  # type: ignore[attr-defined]
_edge_uuid_raw = Edge.uuid  # type: ignore[attr-defined]


def _wrap_uuid(raw: Any) -> _uuid.UUID:
    """Convert a 16-byte ``bytes`` from `_drevo` into a `uuid.UUID`.

    Passthrough if `raw` is already a `uuid.UUID` (defensive — covers
    forward-compat with a future PyO3 layer that surfaces `uuid.UUID`
    natively without breaking callers of this shim).
    """
    if isinstance(raw, _uuid.UUID):
        return raw
    return _uuid.UUID(bytes=bytes(raw))


def _node_uuid(self: Node) -> _uuid.UUID:  # type: ignore[valid-type]
    return _wrap_uuid(_node_uuid_raw.__get__(self, type(self)))


def _edge_uuid(self: Edge) -> _uuid.UUID:  # type: ignore[valid-type]
    return _wrap_uuid(_edge_uuid_raw.__get__(self, type(self)))


# Replace the `uuid` descriptor on the PyO3 classes with our wrapper.
# `Node` / `Edge` are PyO3 `#[pyclass(frozen)]`s, but Python attribute
# lookup still consults the type's `__dict__` first — setting a new
# property here shadows the original getter without touching the
# underlying Rust storage.
Node.uuid = property(_node_uuid)  # type: ignore[assignment,misc]
Edge.uuid = property(_edge_uuid)  # type: ignore[assignment,misc]
