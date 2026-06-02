"""Phase 10.5 task ``00127`` — MCP-level agentic workload (layer 4).

Drives the real ``drevo-mcp`` stdio server under a sustained, realistic
mix of read-only tool calls — the exact wire a live Claude Code / Cline
session hits — and measures p50/p95/p99 latency per tool class while
asserting the *process-health* invariants that only surface across a
long-running session:

  * the child process **survives** the whole run (no crash / panic);
  * no **stdio buffer overflow** — every request's response is drained,
    so the server never blocks on a full stdout pipe;
  * no **file-descriptor leak** — the server's own FD count stays
    bounded across thousands of calls (best-effort, Linux ``/proc``);
  * no **zombie process** after shutdown — stdin EOF makes the server
    exit cleanly and ``wait()`` reaps it with status 0.

## Layout mirrors the Rust 00123 / 00125 harnesses

Following the established Phase 10.5 shape:

  * a handful of **fast scaffolding tests** (the pure latency/RNG math
    plus one short ~300-call burst against a freshly-spawned server)
    run in seconds and validate the harness machinery itself; and
  * one **soak test** (``test_mcp_agentic_workload_soak``) that runs the
    full 30+ minute session. It is skipped unless ``DREVO_MCP_SOAK_SECS``
    is set (floored at 30 min) so the default run never spends half an
    hour — same env-overridable-duration trick as the Rust soaks, which
    lets the single test serve both the 30-min floor and the roadmap's
    8-hour nightly.

## Why every binary-driven test skips gracefully

The harness needs two things the wheel-only CI sandbox does not have:
the compiled Rust ``drevo-mcp`` binary, and an importable ``drevo``
extension module. When either is missing every binary-driven test
``pytest.skip``s rather than failing — a skip is not a regression, and
it keeps this file safe to collect from a bare ``pytest`` run.

The corpus is populated through the ``drevo`` Python module directly
(redb takes an exclusive file lock, so we **close** the handle before
spawning the server against the same path — the MCP baseline tools are
read-only and cannot mutate). This mirrors the Rust ``00091`` fixture.
"""

from __future__ import annotations

import json
import math
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import pytest

# ── Prerequisite discovery ─────────────────────────────────────────────


def _locate_mcp_binary() -> Optional[Path]:
    """Locate the compiled ``drevo-mcp`` binary, or ``None``.

    Resolution order:

      1. ``DREVO_MCP_BIN`` env var (explicit override, used by the
         nightly/on-demand runner that builds the binary out-of-tree);
      2. ``target/debug/drevo-mcp`` then ``target/release/drevo-mcp``
         under the repo root discovered by walking up from this file.

    Returns ``None`` when nothing is found so callers can ``skip``
    cleanly inside the wheel-only CI sandbox where ``target/`` is absent.
    """
    override = os.environ.get("DREVO_MCP_BIN")
    if override:
        p = Path(override)
        return p if p.is_file() and os.access(p, os.X_OK) else None

    exe = "drevo-mcp.exe" if sys.platform == "win32" else "drevo-mcp"
    here = Path(__file__).resolve()
    for ancestor in here.parents:
        for profile in ("debug", "release"):
            candidate = ancestor / "target" / profile / exe
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return candidate
        # Stop once we pass the workspace root (has the top Cargo.toml).
        if (ancestor / "Cargo.toml").is_file() and (ancestor / "target").is_dir():
            break
    return None


def _drevo_module() -> Optional[Any]:
    """Import and return the ``drevo`` extension module, or ``None``.

    The module is the wheel under test; absent in any environment where
    ``maturin develop`` / the wheel install has not run.
    """
    try:
        import drevo  # noqa: PLC0415 — intentional lazy, optional import
    except ImportError:
        return None
    return drevo


_MCP_BINARY = _locate_mcp_binary()
_DREVO = _drevo_module()

# A single reason string so the skip marker reads the same everywhere.
_SKIP_REASON = (
    "drevo-mcp binary and/or the `drevo` extension module is unavailable "
    "(build the binary with `cargo build --bin drevo-mcp` and the module "
    "with `maturin develop`); this workload runs on demand / nightly, not "
    "on the wheel-only PR path"
)

requires_mcp = pytest.mark.skipif(_MCP_BINARY is None or _DREVO is None, reason=_SKIP_REASON)


# ── Deterministic RNG (no external dependency) ─────────────────────────


class Xorshift64:
    """Tiny seed-reproducible xorshift64 PRNG.

    Mirrors the RNG in the Rust ``00123`` harness so the Python workload
    is reproducible without pulling ``random`` state into the picture (a
    failing soak reproduces from the same seed). Not cryptographic — it
    only needs a cheap, well-distributed, deterministic stream.
    """

    _MASK = (1 << 64) - 1

    def __init__(self, seed: int = 0x9E3779B97F4A7C15) -> None:
        # A zero seed is a fixed point of xorshift; nudge it off zero.
        self._state = seed & self._MASK or 0x2545F4914F6CDD1D

    def next_u64(self) -> int:
        x = self._state
        x ^= (x << 13) & self._MASK
        x ^= x >> 7
        x ^= (x << 17) & self._MASK
        self._state = x & self._MASK
        return self._state

    def below(self, n: int) -> int:
        """Uniform-ish integer in ``[0, n)``; ``n`` must be positive."""
        if n <= 0:
            raise ValueError("below() needs a positive bound")
        return self.next_u64() % n


# ── Latency percentile collector ───────────────────────────────────────


class LatencyStats:
    """Nearest-rank percentile collector over latency samples (ms).

    Nearest-rank (the same method the Rust harness uses) keeps the
    percentile equal to an *observed* sample, so ``p99`` of ``1..=100``
    is exactly ``99`` — no interpolation surprises in assertions.
    """

    def __init__(self) -> None:
        self._samples: list[float] = []

    def record(self, ms: float) -> None:
        self._samples.append(ms)

    @property
    def count(self) -> int:
        return len(self._samples)

    @property
    def max(self) -> float:
        return max(self._samples) if self._samples else 0.0

    def percentile(self, p: float) -> float:
        """Nearest-rank ``p``-th percentile (``0 < p <= 100``).

        Returns ``0.0`` for an empty collector. ``rank = ceil(p/100 * n)``
        clamped into ``[1, n]``; the sample at ``rank - 1`` of the sorted
        series is the answer.
        """
        if not self._samples:
            return 0.0
        if not 0.0 < p <= 100.0:
            raise ValueError("percentile p must be in (0, 100]")
        ordered = sorted(self._samples)
        rank = math.ceil(p / 100.0 * len(ordered))
        rank = min(max(rank, 1), len(ordered))
        return ordered[rank - 1]

    @property
    def p50(self) -> float:
        return self.percentile(50.0)

    @property
    def p95(self) -> float:
        return self.percentile(95.0)

    @property
    def p99(self) -> float:
        return self.percentile(99.0)


# ── Query-class model (read-only MCP surface) ──────────────────────────
#
# The MCP baseline registry is read-only (no create/update/delete tool),
# so every class is a reader. Weights bias toward the cheap point
# lookups a real agent issues most, with a heavier tail of traversals
# and FTS that stress the executor. The weights sum to 100 so the burst
# distribution is easy to reason about in a test.

READ_WEIGHTS: dict[str, int] = {
    "health_check": 4,
    "count_nodes": 6,
    "node_get": 28,
    "node_get_by_uuid": 12,
    "list_by_kind": 12,
    "bfs_2hop": 14,
    "bfs_3hop": 8,
    "fts_short": 10,
    "fts_phrase": 6,
}

# A small fixed vocabulary so FTS queries hit real titles. Each is >= 3
# chars so the trigram index returns matches.
_TOPICS = ("alpha", "bravo", "delta", "echo", "foxtrot", "gamma", "hotel", "india")
_KINDS = ("person", "project", "task", "note", "label")


@dataclass
class Corpus:
    """Identifiers the workload references, plus the on-disk DB path.

    ``path`` is the redb file both the populator (``drevo`` module) and
    the spawned ``drevo-mcp`` binary open — same string passed to both so
    they agree on the database.
    """

    path: str
    node_ids: list[int]
    node_uuids: list[str]
    kinds: list[str]
    topics: list[str]

    @property
    def size(self) -> int:
        return len(self.node_ids)


def build_corpus(drevo_mod: Any, path: str, size: int = 200, fake: Any = None) -> Corpus:
    """Populate a connected corpus then close the handle (release lock).

    The fabric adds ``+1``/``+2``/``+3 (mod N)`` forward edges so BFS at
    depth 2/3 returns rich frontiers — the same connectivity trick the
    Rust ``00123`` corpus uses. Titles embed a deterministic topic word
    (so FTS queries hit) plus optional faker text for incidental body
    content (per the project's Python-testing policy: faker default-on
    for values whose exact content the test does not pin).
    """
    node_ids: list[int] = []
    node_uuids: list[str] = []
    kinds: list[str] = []
    topics: list[str] = []

    with drevo_mod.Drevo.open(path) as db:
        for i in range(size):
            topic = _TOPICS[i % len(_TOPICS)]
            kind = _KINDS[i % len(_KINDS)]
            # Title carries the topic token (FTS-searchable) + a stable
            # index; faker fills the free-text body when available.
            body = fake.sentence(nb_words=8) if fake is not None else f"body for {i}"
            node = db.create_node(
                drevo_mod.NewNode(
                    kind=kind,
                    title=f"{topic} {kind} {i:05d}",
                    body=body,
                    properties={"idx": i, "topic": topic},
                )
            )
            node_ids.append(node.id)
            node_uuids.append(str(node.uuid))
            kinds.append(kind)
            topics.append(topic)

        # Connected forward-edge fabric (skip when too small to wrap).
        if size > 3:
            for i in range(size):
                for step in (1, 2, 3):
                    target = (i + step) % size
                    db.create_edge(
                        drevo_mod.NewEdge(
                            from_id=node_ids[i],
                            to_id=node_ids[target],
                            kind="links_to",
                            weight=1.0,
                        )
                    )
        # Context-manager exit calls close(), releasing the redb lock so
        # the spawned binary can open the same file.

    return Corpus(
        path=path,
        node_ids=node_ids,
        node_uuids=node_uuids,
        kinds=kinds,
        topics=topics,
    )


# ── MCP stdio client ───────────────────────────────────────────────────


class McpClientError(RuntimeError):
    """Raised when the server returns a JSON-RPC error or dies mid-call."""


class McpClient:
    """Minimal MCP JSON-RPC 2.0 client over a spawned ``drevo-mcp``.

    Newline-delimited JSON: one request line in, one response line out.
    The client owns the child process lifecycle — ``close()`` sends EOF
    (drops stdin) and waits, the same clean-shutdown path the binary
    documents ("stdin closed → exit cleanly").
    """

    def __init__(self, binary: Path, data_path: str) -> None:
        self._proc = subprocess.Popen(
            [str(binary), "--data-dir", data_path],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,  # line-buffered
        )
        self._next_id = 0

    @property
    def pid(self) -> int:
        return self._proc.pid

    def is_alive(self) -> bool:
        return self._proc.poll() is None

    @property
    def returncode(self) -> Optional[int]:
        return self._proc.returncode

    def _send(self, payload: dict[str, Any]) -> dict[str, Any]:
        if self._proc.stdin is None or self._proc.stdout is None:
            raise McpClientError("child stdio pipes are not open")
        self._proc.stdin.write(json.dumps(payload) + "\n")
        self._proc.stdin.flush()
        line = self._proc.stdout.readline()
        if line == "":
            raise McpClientError(f"drevo-mcp closed stdout unexpectedly (exit={self._proc.poll()})")
        return json.loads(line)

    def handshake(self) -> None:
        """One-shot MCP ``initialize`` — required before any tool call."""
        self._next_id += 1
        resp = self._send(
            {
                "jsonrpc": "2.0",
                "id": self._next_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "mcp-agentic-workload-00127",
                        "version": "0.0.0",
                    },
                },
            }
        )
        if resp.get("error") is not None:
            raise McpClientError(f"initialize failed: {resp['error']}")

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """Issue ``tools/call`` and return the parsed tool payload.

        Unwraps the MCP ``text`` content block (the tool result is itself
        JSON-serialised into a single text block — see ``src/mcp/tools.rs``
        wire-shape docs). Raises on a JSON-RPC error envelope or
        ``isError: true``.
        """
        self._next_id += 1
        resp = self._send(
            {
                "jsonrpc": "2.0",
                "id": self._next_id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        if resp.get("error") is not None:
            raise McpClientError(f"tools/call {name} error: {resp['error']}")
        result = resp.get("result", {})
        if result.get("isError"):
            raise McpClientError(f"tools/call {name} isError: {result}")
        text = result["content"][0]["text"]
        return json.loads(text)

    def close(self, timeout: float = 30.0) -> int:
        """EOF the server, wait, and return its exit code.

        Idempotent — safe to call after the process already exited.
        """
        if self._proc.stdin is not None:
            try:
                self._proc.stdin.close()
            except (BrokenPipeError, ValueError):
                pass
        try:
            return self._proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self._proc.kill()
            return self._proc.wait()

    def __enter__(self) -> "McpClient":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


# ── Workload driver ────────────────────────────────────────────────────


@dataclass
class WorkloadResult:
    """Per-class latency stats plus run totals."""

    per_class: dict[str, LatencyStats] = field(default_factory=dict)
    total_calls: int = 0
    errors: int = 0

    def stats(self, cls: str) -> LatencyStats:
        return self.per_class.setdefault(cls, LatencyStats())


def _pick_class(rng: Xorshift64) -> str:
    """Weighted pick over ``READ_WEIGHTS`` (weights sum to 100)."""
    roll = rng.below(sum(READ_WEIGHTS.values()))
    cursor = 0
    for cls, weight in READ_WEIGHTS.items():
        cursor += weight
        if roll < cursor:
            return cls
    # Unreachable given roll < total, but keeps the type-checker happy.
    return next(iter(READ_WEIGHTS))


def _arguments_for(cls: str, corpus: Corpus, rng: Xorshift64) -> tuple[str, dict[str, Any]]:
    """Map a query class to a concrete ``(tool_name, arguments)`` pair."""
    if cls == "health_check":
        return "drevo_health_check", {}
    if cls == "count_nodes":
        return "drevo_count_nodes", {}
    if cls == "node_get":
        node_id = corpus.node_ids[rng.below(corpus.size)]
        return "drevo_node_get", {"id": node_id}
    if cls == "node_get_by_uuid":
        node_uuid = corpus.node_uuids[rng.below(corpus.size)]
        return "drevo_node_get_by_uuid", {"uuid": node_uuid}
    if cls == "list_by_kind":
        kind = _KINDS[rng.below(len(_KINDS))]
        return "drevo_list_nodes_by_kind", {"kind": kind, "limit": 50}
    if cls == "bfs_2hop":
        start = corpus.node_ids[rng.below(corpus.size)]
        return "drevo_bfs", {"start_id": start, "max_depth": 2, "direction": "both"}
    if cls == "bfs_3hop":
        start = corpus.node_ids[rng.below(corpus.size)]
        return "drevo_bfs", {"start_id": start, "max_depth": 3, "direction": "both"}
    if cls == "fts_short":
        topic = _TOPICS[rng.below(len(_TOPICS))]
        return "drevo_search_fts", {"query": topic, "limit": 20}
    if cls == "fts_phrase":
        topic = _TOPICS[rng.below(len(_TOPICS))]
        return "drevo_search_fts", {"query": f"{topic} task", "limit": 20}
    raise ValueError(f"unknown query class: {cls}")


def run_workload(
    client: McpClient,
    corpus: Corpus,
    rng: Xorshift64,
    *,
    max_calls: Optional[int] = None,
    duration_secs: Optional[float] = None,
    target_qpm: int = 300,
) -> WorkloadResult:
    """Fire the weighted read mix and collect per-class latencies.

    Exactly one stop condition must be given:

      * ``max_calls`` — count-bounded, **unthrottled** (used by the fast
        scaffolding burst);
      * ``duration_secs`` — time-bounded, throttled to ``target_qpm`` so
        the soak genuinely spans wall-clock time rather than racing to
        completion in seconds.
    """
    if (max_calls is None) == (duration_secs is None):
        raise ValueError("pass exactly one of max_calls / duration_secs")

    result = WorkloadResult()
    interval = 60.0 / target_qpm if duration_secs is not None else 0.0
    start = time.monotonic()
    issued = 0

    while True:
        if max_calls is not None and issued >= max_calls:
            break
        if duration_secs is not None and time.monotonic() - start >= duration_secs:
            break

        cls = _pick_class(rng)
        tool, args = _arguments_for(cls, corpus, rng)
        t0 = time.monotonic()
        try:
            client.call_tool(tool, args)
        except McpClientError:
            result.errors += 1
        else:
            elapsed_ms = (time.monotonic() - t0) * 1000.0
            result.stats(cls).record(elapsed_ms)
        result.total_calls += 1
        issued += 1

        if interval:
            # Pace toward target_qpm without busy-spinning.
            target_time = start + issued * interval
            sleep_for = target_time - time.monotonic()
            if sleep_for > 0:
                time.sleep(sleep_for)

    return result


def _open_fd_count(pid: int) -> Optional[int]:
    """Best-effort open-FD count for ``pid`` (Linux ``/proc`` only).

    Returns ``None`` where ``/proc`` is unavailable (macOS, Windows) so
    the FD-leak assertion degrades to a skip rather than a false signal.
    """
    fd_dir = Path(f"/proc/{pid}/fd")
    try:
        return len(list(fd_dir.iterdir()))
    except OSError:
        return None


# ── Soak gating ────────────────────────────────────────────────────────

_SOAK_FLOOR_SECS = 30 * 60  # 30-minute minimum per the roadmap spec.


def _soak_duration() -> Optional[float]:
    """Soak duration from ``DREVO_MCP_SOAK_SECS``, floored at 30 min.

    Returns ``None`` when the env var is unset → the soak test skips.
    """
    raw = os.environ.get("DREVO_MCP_SOAK_SECS")
    if raw is None:
        return None
    try:
        requested = float(raw)
    except ValueError:
        return None
    return max(requested, float(_SOAK_FLOOR_SECS))


# ═══════════════════════════════════════════════════════════════════════
# Tests
# ═══════════════════════════════════════════════════════════════════════


# ── Pure harness machinery (no binary, always run) ─────────────────────


def test_latency_stats_nearest_rank_percentiles() -> None:
    stats = LatencyStats()
    for i in range(1, 101):  # 1..=100
        stats.record(float(i))
    assert stats.count == 100
    assert stats.p50 == 50.0
    assert stats.p95 == 95.0
    assert stats.p99 == 99.0
    assert stats.max == 100.0
    assert stats.percentile(100.0) == 100.0


def test_latency_stats_empty_is_zero() -> None:
    stats = LatencyStats()
    assert stats.count == 0
    assert stats.p50 == 0.0
    assert stats.p99 == 0.0
    assert stats.max == 0.0


def test_latency_stats_single_sample() -> None:
    stats = LatencyStats()
    stats.record(42.0)
    assert stats.p50 == 42.0
    assert stats.p99 == 42.0
    assert stats.max == 42.0


def test_percentile_rejects_out_of_range() -> None:
    stats = LatencyStats()
    stats.record(1.0)
    with pytest.raises(ValueError):
        stats.percentile(0.0)
    with pytest.raises(ValueError):
        stats.percentile(101.0)


def test_xorshift64_is_deterministic() -> None:
    a = Xorshift64(seed=12345)
    b = Xorshift64(seed=12345)
    seq_a = [a.next_u64() for _ in range(64)]
    seq_b = [b.next_u64() for _ in range(64)]
    assert seq_a == seq_b
    # A different seed yields a different stream.
    c = Xorshift64(seed=54321)
    assert [c.next_u64() for _ in range(64)] != seq_a


def test_xorshift64_zero_seed_does_not_lock_up() -> None:
    rng = Xorshift64(seed=0)
    # Zero is a fixed point of raw xorshift; the constructor must nudge
    # it off zero so the stream actually advances.
    assert rng.next_u64() != 0
    assert len({rng.next_u64() for _ in range(50)}) > 1


def test_below_is_bounded_and_rejects_nonpositive() -> None:
    rng = Xorshift64(seed=7)
    for _ in range(1000):
        assert 0 <= rng.below(13) < 13
    with pytest.raises(ValueError):
        rng.below(0)


def test_read_weights_sum_to_100_and_cover_every_class() -> None:
    assert sum(READ_WEIGHTS.values()) == 100
    # Every weighted class must map to a real tool + argument shape.
    dummy = Corpus(
        path="/dev/null",
        node_ids=[1, 2, 3, 4],
        node_uuids=[
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000004",
        ],
        kinds=["task"],
        topics=["alpha"],
    )
    rng = Xorshift64(seed=1)
    for cls in READ_WEIGHTS:
        tool, args = _arguments_for(cls, dummy, rng)
        assert tool.startswith("drevo_")
        assert isinstance(args, dict)


def test_pick_class_distribution_tracks_weights() -> None:
    rng = Xorshift64(seed=99)
    counts: dict[str, int] = {cls: 0 for cls in READ_WEIGHTS}
    draws = 20_000
    for _ in range(draws):
        counts[_pick_class(rng)] += 1
    # Every class is drawn, and the heaviest class (node_get @ 28) is
    # picked far more often than the lightest (health_check @ 4).
    assert all(n > 0 for n in counts.values())
    assert counts["node_get"] > counts["health_check"] * 3


def test_run_workload_requires_exactly_one_stop_condition() -> None:
    rng = Xorshift64(seed=1)
    corpus = Corpus(path="x", node_ids=[1], node_uuids=["x"], kinds=["task"], topics=["alpha"])
    # Passing both, or neither, is a programmer error.
    with pytest.raises(ValueError):
        run_workload(None, corpus, rng)  # type: ignore[arg-type]
    with pytest.raises(ValueError):
        run_workload(None, corpus, rng, max_calls=1, duration_secs=1.0)  # type: ignore[arg-type]


# ── Property-based check on the percentile invariant ───────────────────
#
# Justified per the project Python-testing policy (hypothesis case-by-
# case): ``percentile`` is a small pure function with a clear,
# wide-input invariant — monotone in p, and bounded by the sample
# range — exactly the shape property testing pays off on.

try:
    from hypothesis import given
    from hypothesis import strategies as st

    @given(
        st.lists(
            st.floats(min_value=0.0, max_value=1e6, allow_nan=False, allow_infinity=False),
            min_size=1,
            max_size=500,
        )
    )
    def test_percentile_is_monotonic_and_bounded(samples: list[float]) -> None:
        stats = LatencyStats()
        for s in samples:
            stats.record(s)
        p50, p95, p99 = stats.p50, stats.p95, stats.p99
        # Monotone non-decreasing across rising percentiles.
        assert p50 <= p95 <= p99
        # Every percentile is an observed sample, hence within range.
        lo, hi = min(samples), max(samples)
        for p in (p50, p95, p99):
            assert lo <= p <= hi

except ImportError:  # pragma: no cover — hypothesis is a dev-only dep
    pass


# ── Binary-driven scaffolding (fast; skip if prerequisites absent) ─────


@requires_mcp
def test_mcp_short_burst_records_every_class(tmp_path: Path, fake: Any) -> None:
    """~600-call burst exercises every read class and the full lifecycle.

    Asserts the harness machinery end-to-end: corpus → spawn → handshake
    → sustained tool calls → clean shutdown, with no zombie afterward.
    """
    assert _DREVO is not None and _MCP_BINARY is not None
    db_path = str(tmp_path / "burst.drevo")
    corpus = build_corpus(_DREVO, db_path, size=200, fake=fake)

    client = McpClient(_MCP_BINARY, db_path)
    try:
        client.handshake()
        rng = Xorshift64(seed=2026)
        result = run_workload(client, corpus, rng, max_calls=600)

        assert result.total_calls == 600
        assert result.errors == 0, "no read-only call should error on a valid corpus"
        # Every weighted class must have been issued at least once and
        # carry a real p99 sample.
        for cls in READ_WEIGHTS:
            stats = result.stats(cls)
            assert stats.count > 0, f"class {cls!r} never ran"
            assert stats.p99 >= 0.0
        # Server is still healthy mid-session.
        assert client.is_alive()
    finally:
        code = client.close()

    # Clean shutdown: EOF → exit 0, process reaped (no zombie).
    assert code == 0
    assert client.is_alive() is False
    assert client.returncode == 0


@requires_mcp
def test_mcp_corpus_is_connected(tmp_path: Path) -> None:
    """BFS from any node returns a rich frontier (corpus is connected)."""
    assert _DREVO is not None and _MCP_BINARY is not None
    db_path = str(tmp_path / "conn.drevo")
    corpus = build_corpus(_DREVO, db_path, size=60)

    with McpClient(_MCP_BINARY, db_path) as client:
        client.handshake()
        count = client.call_tool("drevo_count_nodes", {})
        assert count["count"] == 60
        payload = client.call_tool(
            "drevo_bfs",
            {"start_id": corpus.node_ids[0], "max_depth": 2, "direction": "both"},
        )
        # +1/+2/+3 forward fabric → depth-2 both-directions reaches a
        # double-digit neighbourhood, never just the start node.
        assert len(payload["nodes"]) >= 6


@requires_mcp
def test_mcp_node_get_missing_id_returns_null_not_error(tmp_path: Path) -> None:
    """A miss is a ``null`` node payload, not a transport-level error."""
    assert _DREVO is not None and _MCP_BINARY is not None
    db_path = str(tmp_path / "miss.drevo")
    build_corpus(_DREVO, db_path, size=10)
    with McpClient(_MCP_BINARY, db_path) as client:
        client.handshake()
        payload = client.call_tool("drevo_node_get", {"id": 9_999_999})
        assert payload["node"] is None


@requires_mcp
def test_mcp_invalid_params_raises(tmp_path: Path) -> None:
    """Malformed args surface as a JSON-RPC error the client raises on."""
    assert _DREVO is not None and _MCP_BINARY is not None
    db_path = str(tmp_path / "bad.drevo")
    build_corpus(_DREVO, db_path, size=10)
    with McpClient(_MCP_BINARY, db_path) as client:
        client.handshake()
        with pytest.raises(McpClientError):
            client.call_tool("drevo_bfs", {"start_id": 1, "direction": "sideways"})


# ── Soak (30+ min; skip unless DREVO_MCP_SOAK_SECS set) ────────────────


@requires_mcp
@pytest.mark.skipif(
    _soak_duration() is None,
    reason="set DREVO_MCP_SOAK_SECS to run the 30+ min MCP soak (nightly/on-demand)",
)
def test_mcp_agentic_workload_soak(tmp_path: Path, fake: Any) -> None:
    """Full 30+ minute MCP session under a throttled 300-qpm read mix.

    Proves the process-health invariants that only emerge over a long
    session: survives the whole duration, no FD leak (Linux best-effort),
    no zombie after shutdown, and every query class keeps producing
    latency samples throughout.
    """
    assert _DREVO is not None and _MCP_BINARY is not None
    duration = _soak_duration()
    assert duration is not None

    db_path = str(tmp_path / "soak.drevo")
    corpus = build_corpus(_DREVO, db_path, size=10_000, fake=fake)

    client = McpClient(_MCP_BINARY, db_path)
    try:
        client.handshake()
        fd_before = _open_fd_count(client.pid)

        rng = Xorshift64(seed=20270602)
        result = run_workload(client, corpus, rng, duration_secs=duration, target_qpm=300)

        # Survived the entire session.
        assert client.is_alive(), "drevo-mcp died during the soak"
        # Every class kept producing samples with a measured p99.
        for cls in READ_WEIGHTS:
            stats = result.stats(cls)
            assert stats.count > 0, f"class {cls!r} produced no samples"
            assert stats.p99 >= 0.0
        # No FD leak (only checkable where /proc exists).
        fd_after = _open_fd_count(client.pid)
        if fd_before is not None and fd_after is not None:
            assert fd_after <= fd_before + 8, f"file-descriptor leak: {fd_before} -> {fd_after}"
        # Sustained throughput stayed in the right ballpark for 300 qpm.
        assert result.total_calls >= (duration / 60.0) * 250
    finally:
        code = client.close()

    # No zombie: clean EOF exit, reaped.
    assert code == 0
    assert client.is_alive() is False
    assert client.returncode == 0
