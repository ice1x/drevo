"""Shared pytest fixtures for the drevo-py runtime test suite.

These tests run inside the cibuildwheel `CIBW_TEST_COMMAND` step
(`.github/workflows/python-wheels.yml`) after each wheel is built and
installed into the isolation venv. Locally they run via
`maturin develop && pytest drevo-py/tests/`.

The suite stays minimal — full unit / integration / e2e coverage is the
scope of Phase 16 tasks `00118` / `00119` / `00120`. What lives here is
the *runtime contract for the 00116 shim layer* (UUID wrapping,
`InvalidWeightError` subclass, `__all__` consistency, re-export
correctness) — the gaps the text-level scaffolding tests in
`tests/python_package_wheels_tests.rs` cannot catch.
"""

from __future__ import annotations

import pytest


@pytest.fixture
def drevo_db():
    """Yield an in-memory drevo handle, close on exit.

    Uses the context-manager protocol of `Drevo`, so even if a test
    raises the handle is closed (releasing the in-memory backend).
    """
    import drevo

    with drevo.Drevo.open_in_memory() as db:
        yield db
