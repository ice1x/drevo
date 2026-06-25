"""Locks Deliverable 1: docker-compose.yml bind-mounts a host folder for /data.

Kept in this package (not a Rust test) so the whole PR's test surface runs on
the fast Python path without a cargo build. Reads the repo-root compose file.
"""

from __future__ import annotations

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
COMPOSE = REPO_ROOT / "docker-compose.yml"


def test_compose_file_exists() -> None:
    assert COMPOSE.is_file(), f"expected {COMPOSE} to exist"


def test_compose_bind_mounts_host_folder() -> None:
    text = COMPOSE.read_text(encoding="utf-8")
    assert "${DREVO_DATA_DIR:-./data}:/data" in text, "compose must bind-mount the host data dir"


def test_compose_runs_as_host_user() -> None:
    text = COMPOSE.read_text(encoding="utf-8")
    assert "${DREVO_UID:-1000}:${DREVO_GID:-1000}" in text, "compose must run as the host user"


def test_compose_has_no_named_volume() -> None:
    text = COMPOSE.read_text(encoding="utf-8")
    assert (
        "drevo-data:" not in text
    ), "the named-volume declaration must be gone (bind-mount instead)"
