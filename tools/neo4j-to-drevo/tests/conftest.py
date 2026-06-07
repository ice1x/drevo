"""Shared fixtures for the neo4j-to-drevo test suite."""

from __future__ import annotations

import pytest
from faker import Faker

_FAKE_SEED = 20260607


@pytest.fixture
def fake() -> Faker:
    """Deterministically-seeded faker for incidental test data."""
    instance = Faker()
    instance.seed_instance(_FAKE_SEED)
    return instance


@pytest.fixture
def drevo_db():
    """Yield a fresh in-memory drevo handle; close on scope exit."""
    import drevo

    with drevo.Drevo.open_in_memory() as db:
        yield db
