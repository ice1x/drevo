"""Runtime tests for the pure-Python shim shipped by Phase 16 task `00116`.

The text-level scaffolding tests in `tests/python_package_wheels_tests.rs`
verify the shim's *structure* (UUID wrap mentioned, `InvalidWeightError`
subclass declared, `__all__` lists the right symbols). These tests
verify the shim's *behaviour* at runtime — that `node.uuid` is really
a `uuid.UUID`, that every symbol in `__all__` is really importable, that
`InvalidWeightError` is really a `ValueError`.

Runs inside cibuildwheel's `CIBW_TEST_COMMAND` after the wheel is
installed into an isolated venv. Locally: `maturin develop` then
`pytest drevo-py/tests/test_shim.py`.

Full unit / integration / e2e coverage is the scope of tasks `00118` /
`00119` / `00120`; this file closes the runtime gaps specific to the
`00116` shim layer.
"""

from __future__ import annotations

import inspect
import uuid

import pytest

import drevo


# ── Top-level package surface ─────────────────────────────────────────


def test_version_is_nonempty_string():
    """`drevo.__version__` must surface the cdylib's CARGO_PKG_VERSION."""
    assert isinstance(drevo.__version__, str)
    assert drevo.__version__  # non-empty
    # Cargo's default semver scheme — at least two dots OR a major-only.
    # `0.1.0` parses today; this assertion catches a future shim that
    # accidentally re-exports `None` or an empty string.
    assert any(ch.isdigit() for ch in drevo.__version__)


def test_all_lists_every_public_symbol():
    """Every name in `__all__` must be importable from `drevo`."""
    for name in drevo.__all__:
        assert hasattr(drevo, name), (
            f"`drevo.__all__` lists `{name}` but `drevo.{name}` does "
            f"not resolve — shim re-export missing"
        )


@pytest.mark.parametrize(
    "name",
    [
        # Handle.
        "Drevo",
        # Plain-data wrappers.
        "CompactReport",
        "Direction",
        "Edge",
        "EdgePatch",
        "NewEdge",
        "NewNode",
        "Node",
        "NodePatch",
        "ScoredNode",
        "SubGraph",
        # Exception hierarchy.
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
    ],
)
def test_public_symbol_is_importable(name):
    """Every documented public symbol must actually exist on `drevo`.

    Matches RFC §3.3 + the `__init__.pyi` stub block — catches a shim
    re-export typo that the text-level scaffolding tests would miss
    (those check for the literal string but not that the symbol
    actually resolves).
    """
    assert hasattr(drevo, name)


# ── Exception hierarchy ───────────────────────────────────────────────


def test_invalid_weight_error_subclasses_value_error():
    """`drevo.InvalidWeightError` MUST inherit `ValueError` (RFC §5.3).

    Without this guarantee, callers cannot rely on
    `except ValueError:` catching a bad-weight failure — which is the
    canonical Python idiom this RFC amendment preserves.
    """
    assert issubclass(drevo.InvalidWeightError, ValueError)


def test_invalid_weight_error_constructible():
    """Constructing the subclass directly must not raise."""
    err = drevo.InvalidWeightError("bad weight: nan")
    assert isinstance(err, drevo.InvalidWeightError)
    assert isinstance(err, ValueError)
    assert "bad weight" in str(err)


def test_exception_hierarchy_roots_at_drevo_error():
    """Every typed exception (except `InvalidWeightError`) must root at
    `DrevoError` so `except drevo.DrevoError:` catches them all.

    `InvalidWeightError` is the deliberate exception per RFC §5.3 — it
    extends `ValueError`, not `DrevoError`, because "bad numeric input"
    is the canonical Python idiom for `ValueError`.
    """
    drevo_rooted = [
        drevo.NotFoundError,
        drevo.NodeNotFoundError,
        drevo.EdgeNotFoundError,
        drevo.ConflictError,
        drevo.DuplicateTitleError,
        drevo.StorageError,
        drevo.SerializationError,
        drevo.LockedError,
        drevo.PanicError,
    ]
    for cls in drevo_rooted:
        assert issubclass(cls, drevo.DrevoError), (
            f"{cls.__name__} must subclass drevo.DrevoError per RFC §5.1 "
            f"(typed exception hierarchy rooted at DrevoError)"
        )


def test_not_found_subclasses_are_node_and_edge_specific():
    """The `NotFoundError` subclass tree mirrors RFC §5.1."""
    assert issubclass(drevo.NodeNotFoundError, drevo.NotFoundError)
    assert issubclass(drevo.EdgeNotFoundError, drevo.NotFoundError)


def test_duplicate_title_subclasses_conflict():
    """`DuplicateTitleError` ⊂ `ConflictError` per RFC §5.1."""
    assert issubclass(drevo.DuplicateTitleError, drevo.ConflictError)


# ── UUID wrapping (the centrepiece of the 00116 shim) ─────────────────


def test_node_uuid_is_uuid_uuid(drevo_db):
    """`Node.uuid` must surface a native `uuid.UUID`, not raw `bytes`.

    RFC §3.2 default position; §12.2 amendment that defers the
    conversion from the cdylib to this shim. Without this test the
    shim could silently regress to returning `bytes` (e.g. someone
    removes the `property()` monkey-patch in `__init__.py`) and the
    text-level scaffolding test would still pass (the string
    `uuid.UUID` is still in the file).
    """
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="hello"))
    assert isinstance(node.uuid, uuid.UUID), (
        f"node.uuid is {type(node.uuid).__name__}, expected uuid.UUID — "
        f"the shim in drevo/__init__.py must wrap the cdylib's bytes "
        f"return as uuid.UUID(bytes=...). See RFC §12.2."
    )
    # The UUID v7 prefix should round-trip through bytes → uuid.UUID →
    # bytes without mutation.
    assert len(node.uuid.bytes) == 16


def test_edge_uuid_is_uuid_uuid(drevo_db):
    """Same contract for `Edge.uuid` (mirror RFC §3.2)."""
    src = drevo_db.create_node(drevo.NewNode(kind="note", title="src"))
    dst = drevo_db.create_node(drevo.NewNode(kind="note", title="dst"))
    edge = drevo_db.create_edge(
        drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to")
    )
    assert isinstance(edge.uuid, uuid.UUID)
    assert len(edge.uuid.bytes) == 16


def test_node_uuid_is_idempotent(drevo_db):
    """Reading `node.uuid` twice yields equal UUID objects (no stale
    cache, no mutation, no per-call re-conversion artefact).
    """
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t"))
    assert node.uuid == node.uuid
    assert node.uuid.bytes == node.uuid.bytes


def test_get_node_by_uuid_accepts_bytes_on_input(drevo_db):
    """The cdylib still takes `bytes` on the INPUT side — only the
    return side is wrapped. This contract lets `get_node_by_uuid` keep
    its zero-copy path for callers that already hold `bytes`.

    Asserts the input contract has NOT silently changed alongside the
    output wrap.
    """
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="t"))
    fetched = drevo_db.get_node_by_uuid(node.uuid.bytes)
    assert fetched is not None
    assert fetched.id == node.id


# ── Handle smoke test (covers the re-export chain end-to-end) ─────────


def test_open_in_memory_round_trip(drevo_db):
    """Smoke test: create + fetch round-trips via the shim's re-exports.

    Exercises `Drevo.open_in_memory` (classmethod via shim), `NewNode`
    constructor (kwargs), `create_node` (handle method), `get_node`
    (returns Optional[Node]), and `Node.title` (getter through the
    frozen pyclass). If any link in this chain regresses, this test
    fails.
    """
    node = drevo_db.create_node(drevo.NewNode(kind="note", title="hello"))
    assert node.kind == "note"
    assert node.title == "hello"

    fetched = drevo_db.get_node(node.id)
    assert fetched is not None
    assert fetched.title == "hello"
    assert fetched.uuid == node.uuid


def test_context_manager_closes_handle():
    """`with Drevo.open_in_memory()` must close on exit; subsequent
    method calls on the closed handle raise `RuntimeError`.
    """
    db = drevo.Drevo.open_in_memory()
    with db as opened:
        assert opened is db
        node = db.create_node(drevo.NewNode(kind="note", title="t"))
        assert node.id >= 1
    # After exit, the handle is closed — every method must raise.
    with pytest.raises(RuntimeError):
        db.create_node(drevo.NewNode(kind="note", title="post-close"))


def test_direction_enum_values():
    """`Direction` is an `IntEnum` with stable integer values matching
    the Rust `model::Direction` ordering (`OUT`, `IN`, `BOTH`).
    """
    assert drevo.Direction.OUT == 0
    assert drevo.Direction.IN == 1
    assert drevo.Direction.BOTH == 2


# ── Type-stub vs runtime sanity ───────────────────────────────────────


def test_drevo_class_has_every_rfc_method():
    """Every method enumerated in RFC §3.3 is actually present on the
    runtime `Drevo` class.

    The `.pyi` stub claims these exist — if the cdylib ever drops one
    without updating the stub, mypy on the user's code would lie. This
    test catches the drift at runtime.
    """
    required = [
        "open",
        "open_in_memory",
        "close",
        "compact",
        "health_check",
        "create_node",
        "get_node",
        "get_node_by_uuid",
        "get_node_by_title",
        "update_node",
        "delete_node",
        "create_edge",
        "get_edge",
        "get_edge_by_uuid",
        "update_edge",
        "delete_edge",
        "edges_of",
        "list_nodes_by_kind",
        "list_edges_by_kind",
        "list_recent",
        "bfs",
        "dfs",
        "shortest_path",
        "subgraph",
        "neighbors",
        "search_fts",
    ]
    for name in required:
        assert callable(getattr(drevo.Drevo, name, None)), (
            f"drevo.Drevo.{name} is missing or not callable — RFC §3.3 "
            f"lists this method as part of the handle's contract"
        )


def test_node_class_exposes_documented_attributes(drevo_db):
    """Every getter the `.pyi` stub promises on `Node` must resolve on
    a real instance.
    """
    node = drevo_db.create_node(
        drevo.NewNode(kind="note", title="t", body="b", body_html="<p>b</p>")
    )
    for attr in [
        "id",
        "uuid",
        "kind",
        "title",
        "body",
        "body_html",
        "created_at",
        "updated_at",
        "properties",
    ]:
        # Use a sentinel because some attributes may legitimately be 0
        # / None / "".
        sentinel = object()
        assert getattr(node, attr, sentinel) is not sentinel, (
            f"Node.{attr} is missing at runtime — drift between the "
            f".pyi stub and the cdylib"
        )


def test_edge_class_exposes_documented_attributes(drevo_db):
    """Mirror of `test_node_class_exposes_documented_attributes` for `Edge`."""
    src = drevo_db.create_node(drevo.NewNode(kind="note", title="s"))
    dst = drevo_db.create_node(drevo.NewNode(kind="note", title="d"))
    edge = drevo_db.create_edge(
        drevo.NewEdge(from_id=src.id, to_id=dst.id, kind="links_to", weight=2.5)
    )
    for attr in [
        "id",
        "uuid",
        "from_id",
        "to_id",
        "kind",
        "weight",
        "created_at",
        "properties",
    ]:
        sentinel = object()
        assert getattr(edge, attr, sentinel) is not sentinel, (
            f"Edge.{attr} is missing at runtime — drift between the "
            f".pyi stub and the cdylib"
        )
    assert edge.weight == pytest.approx(2.5)


def test_errors_module_is_importable():
    """`from drevo.errors import InvalidWeightError` is a documented
    path; assert it resolves and is the same class as
    `drevo.InvalidWeightError`.
    """
    from drevo import errors  # noqa: F401 — import side-effect is the test

    assert inspect.isclass(errors.InvalidWeightError)
    assert errors.InvalidWeightError is drevo.InvalidWeightError
