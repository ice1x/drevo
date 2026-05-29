"""Property-based / fuzz tests for the UUID conversion boundary.

These complement the example-based cases in ``tests/test_shim.py`` by
fuzzing the pure-Python ``drevo._wrap_uuid`` helper in ``drevo.__init__``
with Hypothesis. The boundary is a strong fuzz target: it is pure Python
(no compiled extension needed), it has clear round-trip invariants, and
it guards on length — exactly the kind of logic where hand-picked
examples miss edge cases.

Invariants exercised:

* ``_wrap_uuid(raw).bytes == raw`` for *every* 16-byte input.
* ``_wrap_uuid(u) is u`` for *every* ``uuid.UUID`` (pass-through).
* ``_wrap_uuid(u.bytes) == u`` for *every* ``uuid.UUID`` (round-trip).
* ``_wrap_uuid`` rejects *every* byte string whose length is not 16.
"""

from __future__ import annotations

import uuid

import pytest
from hypothesis import given
from hypothesis import strategies as st

from drevo import _wrap_uuid  # type: ignore[attr-defined]


class TestWrapUuidProperties:
    @given(st.binary(min_size=16, max_size=16))
    def test_any_16_bytes_round_trip(self, raw: bytes) -> None:
        result = _wrap_uuid(raw)
        assert isinstance(result, uuid.UUID)
        assert result.bytes == raw

    @given(st.uuids())
    def test_uuid_passed_through_unchanged(self, u: uuid.UUID) -> None:
        # A `uuid.UUID` input is returned by identity — no re-wrap.
        assert _wrap_uuid(u) is u

    @given(st.uuids())
    def test_bytes_of_uuid_round_trip(self, u: uuid.UUID) -> None:
        assert _wrap_uuid(u.bytes) == u

    @given(st.binary(max_size=64).filter(lambda b: len(b) != 16))
    def test_rejects_any_wrong_length(self, raw: bytes) -> None:
        # `uuid.UUID(bytes=...)` raises ValueError for any non-16-byte
        # input — the shim forwards that without masking it.
        with pytest.raises(ValueError):
            _wrap_uuid(raw)
