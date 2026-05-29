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
from faker import Faker

# Deterministic seed so every run draws the same incidental data. Pinned
# to the date this fixture landed (2026-05-29) — an arbitrary but stable
# constant. A flake that reproduces locally reproduces in CI too.
_FAKE_SEED = 20260529


@pytest.fixture
def fake() -> Faker:
    """A deterministically-seeded `faker.Faker` for incidental test data.

    Use it for values whose *exact* content is irrelevant to what a test
    asserts — a node body, a free-text property leaf, a sentence. The
    discipline is **capture-and-round-trip**: generate the value once,
    bind it to a local variable, pass that variable in, then assert the
    stored value equals the captured variable. Never assert against a
    fresh `fake.*()` call (it would draw a *different* value) and never
    use faker for values a test pins exactly (kinds, weights, ids).

    Seeded with a fixed constant so the suite stays reproducible: the
    same example is drawn on every machine and every CI run.
    """
    instance = Faker()
    instance.seed_instance(_FAKE_SEED)
    return instance


@pytest.fixture
def drevo_db():
    """Yield an in-memory drevo handle, close on exit.

    Uses the context-manager protocol of `Drevo`, so even if a test
    raises the handle is closed (releasing the in-memory backend).
    """
    import drevo

    with drevo.Drevo.open_in_memory() as db:
        yield db
