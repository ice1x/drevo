"""Property-based / fuzz round-trip tests through the compiled extension.

Where ``tests/unit/test_uuid_shim_properties.py`` fuzzes the pure-Python
boundary, these fuzz the *serialization* boundary: arbitrary
JSON-shaped property dicts written through ``create_node`` /
``create_edge`` must come back equal from ``get_node`` / ``get_edge``.
This is the high-value place for fuzzing in the bindings — the property
bag crosses Python -> PyO3 -> serde_json -> redb and back, so hand-picked
examples easily miss an encoding edge case (Unicode, empty strings, large
integers, ``None``, deeply varied keys).

Floats are intentionally excluded from the value strategy: ``NaN`` is not
equal to itself, so a naive round-trip assertion on floats would be
ill-defined; that boundary deserves its own dedicated test rather than a
flaky property. Integers are bounded to the signed 64-bit range that the
Rust ``serde_json`` layer represents losslessly.
"""

from __future__ import annotations

import uuid

from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

import drevo


def _unique_title() -> str:
    """A title guaranteed not to collide across generated examples.

    ``create_node`` enforces title uniqueness, so each Hypothesis example
    (they all share one function-scoped ``drevo_db``) needs a fresh title;
    a random uuid string keeps the focus on the *property bag*, which is
    what this fuzz actually exercises.
    """
    return f"fuzz-{uuid.uuid4()}"


# The ``drevo_db`` fixture is function-scoped and is therefore shared
# across all generated examples within one test (Hypothesis does not
# re-run fixtures per example). That is harmless here — every example
# creates its own fresh nodes/edges with unique titles and asserts only
# on those — so we silence the health check that would otherwise flag the
# shared fixture.
_FUZZ = settings(
    max_examples=75,
    deadline=None,
    suppress_health_check=[HealthCheck.function_scoped_fixture],
)

# Keys: non-empty text without surrogate code points (which are not valid
# in UTF-8 and would fail at the encoding boundary for reasons unrelated
# to what we are testing).
_keys = st.text(
    alphabet=st.characters(blacklist_categories=("Cs",)),
    min_size=1,
    max_size=24,
)

# Values: the JSON-representable scalar types that round-trip losslessly.
_INT64_MIN = -(2**63)
_INT64_MAX = 2**63 - 1
_values = st.one_of(
    st.text(alphabet=st.characters(blacklist_categories=("Cs",)), max_size=64),
    st.integers(min_value=_INT64_MIN, max_value=_INT64_MAX),
    st.booleans(),
    st.none(),
)

_property_bags = st.dictionaries(_keys, _values, max_size=8)


class TestPropertyRoundTrip:
    @_FUZZ
    @given(props=_property_bags)
    def test_node_properties_round_trip(self, drevo_db: drevo.Drevo, props: dict) -> None:
        # A unique title per example keeps the duplicate-title guard out of
        # the way — the property bag is what we are actually fuzzing.
        node = drevo_db.create_node(
            drevo.NewNode(kind="fuzz_node", title=_unique_title(), properties=props)
        )
        fetched = drevo_db.get_node(node.id)
        assert fetched is not None
        assert fetched.properties == props

    @_FUZZ
    @given(props=_property_bags)
    def test_edge_properties_round_trip(self, drevo_db: drevo.Drevo, props: dict) -> None:
        a = drevo_db.create_node(drevo.NewNode(kind="fuzz_node", title=_unique_title()))
        b = drevo_db.create_node(drevo.NewNode(kind="fuzz_node", title=_unique_title()))
        edge = drevo_db.create_edge(
            drevo.NewEdge(from_id=a.id, to_id=b.id, kind="fuzz_edge", properties=props)
        )
        fetched = drevo_db.get_edge(edge.id)
        assert fetched is not None
        assert fetched.properties == props
