"""Unit tests for READ access and client time limits from Python (#853).

`Drevo.execute(..., access="read")` and `Drevo.begin(access="read")` run
statements that may not change the graph: a write, a schema change or a
procedure with effects outside the graph (`drevo.extract.*`,
`drevo.events.post`, ...) raises `AccessModeError` before it runs, and
nothing is written. `timeout_ms` bounds a statement (autocommit) or the
whole transaction; past it the statement stops with `QueryTimeoutError` and
a transaction is rolled back.

The scenario is an AI agent reading a CBT journal: it may look entries up,
but nothing it runs may change the journal or call a paid model, and a
heavy question must not run forever.
"""

from __future__ import annotations

import time

import pytest
from faker import Faker

import drevo


def entries(db: drevo.Drevo) -> int:
    return int(db.execute("MATCH (e:Entry) RETURN count(e) AS c")[0]["c"])


@pytest.fixture
def journal(drevo_db: drevo.Drevo, fake: Faker) -> drevo.Drevo:
    """Three journal entries with a mood vector each, one with `"bridges"`."""
    drevo_db.execute("CREATE VECTOR INDEX entry_vec FOR (e:Entry) ON (e.vec)")
    for i, (text, vec) in enumerate(
        [
            ("afraid of crossing bridges", [1.0, 0.0, 0.0]),
            (fake.sentence(), [0.0, 1.0, 0.0]),
            (fake.sentence(), [0.0, 0.0, 1.0]),
        ]
    ):
        drevo_db.execute(
            "CREATE (:Entry {title: $title, text: $text, vec: $vec})",
            {"title": f"entry-{i}", "text": text, "vec": vec},
        )
    return drevo_db


# ── READ access ─────────────────────────────────────────────────────


def test_access_mode_error_is_a_cypher_error() -> None:
    assert issubclass(drevo.AccessModeError, drevo.CypherError)


def test_a_read_transaction_refuses_a_write_and_writes_nothing(journal: drevo.Drevo) -> None:
    with pytest.raises(drevo.AccessModeError, match="READ"):
        with journal.transaction(access="read") as tx:
            tx.execute("CREATE (:Entry {title: 'sneaked in'})")
    assert tx.closed
    assert entries(journal) == 3


@pytest.mark.parametrize(
    "query",
    [
        "MATCH (e:Entry) SET e.mood = 'calm'",
        "MATCH (e:Entry) DETACH DELETE e",
        "MERGE (:Entry {title: 'entry-9'})",
        "CREATE INDEX entry_title FOR (e:Entry) ON (e.title)",
    ],
)
def test_an_autocommit_read_refuses_every_kind_of_change(journal: drevo.Drevo, query: str) -> None:
    before = journal.execute("MATCH (e:Entry) RETURN e.title AS t, e.mood AS m ORDER BY t").rows
    with pytest.raises(drevo.AccessModeError):
        journal.execute(query, access="read")
    after = journal.execute("MATCH (e:Entry) RETURN e.title AS t, e.mood AS m ORDER BY t").rows
    assert after == before


def test_a_read_never_calls_the_model(journal: drevo.Drevo, fake: Faker) -> None:
    # Without READ the embedded handle reaches the procedure, which reports
    # the missing chat endpoint; in READ it is refused before it runs.
    text = fake.paragraph()
    for query in [
        "CALL drevo.extract.entities($text) YIELD name RETURN name",
        "CALL drevo.extract.ingest($text, {source: 'journal'}) YIELD facts RETURN facts",
    ]:
        with pytest.raises(drevo.AccessModeError, match="READ"):
            journal.execute(query, {"text": text}, access="read")
        with pytest.raises(drevo.AccessModeError, match="READ"):
            journal.begin(access="read").execute(query, {"text": text})


def test_a_read_refuses_a_write_inside_a_subquery(journal: drevo.Drevo) -> None:
    """A `CALL { ... }` subquery does not hide a write from READ access (#865)."""
    hidden = "MATCH (e:Entry) CALL { WITH e SET e.mood = 'calm' }"
    with pytest.raises(drevo.AccessModeError, match="READ"):
        journal.execute(hidden, access="read")
    with pytest.raises(drevo.AccessModeError, match="READ"):
        journal.begin(access="read").execute(hidden)
    assert journal.execute(
        "MATCH (e:Entry) WHERE e.mood IS NOT NULL RETURN count(e) AS c"
    ).rows == [{"c": 0}]
    per_entry = journal.execute(
        "MATCH (e:Entry) CALL (e) { RETURN size(e.title) AS n } RETURN sum(n) AS total",
        access="read",
    ).rows
    assert per_entry == [{"total": 21}]


def test_matching_full_text_and_vector_search_work_in_a_read_transaction(
    journal: drevo.Drevo,
) -> None:
    with journal.transaction(access="read") as tx:
        titles = tx.execute("MATCH (e:Entry) RETURN e.title AS t ORDER BY t").rows
        assert titles == [{"t": "entry-0"}, {"t": "entry-1"}, {"t": "entry-2"}]
        found = tx.execute("CALL fts.search('bridges', 5) YIELD node RETURN node.title AS t").rows
        assert found == [{"t": "entry-0"}]
        nearest = tx.execute(
            "CALL db.index.vector.queryNodes('entry_vec', 1, $v) YIELD node "
            "RETURN node.title AS t",
            {"v": [0.0, 0.9, 0.1]},
        ).rows
        assert nearest == [{"t": "entry-1"}]
    assert tx.closed


def test_an_autocommit_read_returns_rows(journal: drevo.Drevo) -> None:
    rows = journal.execute(
        "MATCH (e:Entry) WHERE e.text CONTAINS $w RETURN e.title AS t",
        {"w": "bridges"},
        access="read",
    ).rows
    assert rows == [{"t": "entry-0"}]


def test_write_access_stays_the_default(drevo_db: drevo.Drevo) -> None:
    drevo_db.execute("CREATE (:Entry {title: 'a'})", access="write")
    with drevo_db.transaction(access="write") as tx:
        tx.execute("CREATE (:Entry {title: 'b'})")
    with drevo_db.transaction() as tx:
        tx.execute("CREATE (:Entry {title: 'c'})")
    assert entries(drevo_db) == 3


@pytest.mark.parametrize("access", ["r", "readonly", "", "READ ONLY"])
def test_an_unknown_access_is_refused(drevo_db: drevo.Drevo, access: str) -> None:
    with pytest.raises(ValueError, match="access"):
        drevo_db.execute("RETURN 1", access=access)
    with pytest.raises(ValueError, match="access"):
        drevo_db.begin(access=access)


def test_access_is_case_insensitive(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.AccessModeError):
        drevo_db.execute("CREATE (:Entry)", access="READ")
    assert drevo_db.execute("RETURN 1 AS one", access="Write").rows == [{"one": 1}]


# ── Time limits ─────────────────────────────────────────────────────


def slow_question(n: int) -> str:
    """CPU-bound, constant-memory: `n²` additions via nested `reduce`."""
    return (
        f"RETURN reduce(s = 0, i IN range(1, {n}) | "
        f"s + reduce(t = 0, j IN range(1, {n}) | t + j)) AS effort"
    )


@pytest.fixture(scope="module")
def slow_n() -> int:
    """Grow `n` until the question takes at least 1.5 s here without a limit."""
    with drevo.Drevo.open_in_memory() as db:
        n = 400
        while True:
            start = time.monotonic()
            db.execute(slow_question(n))
            if time.monotonic() - start >= 1.5 or n >= 40_000:
                return n
            n *= 2


def test_a_slow_autocommit_read_stops_at_its_timeout(drevo_db: drevo.Drevo, slow_n: int) -> None:
    start = time.monotonic()
    with pytest.raises(drevo.QueryTimeoutError) as caught:
        drevo_db.execute(slow_question(slow_n), access="read", timeout_ms=100)
    assert time.monotonic() - start < 1.0, "stopped well before it would finish"
    assert caught.value.args[1] == 100


def test_a_slow_statement_stops_at_its_transactions_timeout(
    drevo_db: drevo.Drevo, slow_n: int
) -> None:
    tx = drevo_db.begin(access="read", timeout_ms=100)
    start = time.monotonic()
    with pytest.raises(drevo.QueryTimeoutError):
        tx.execute(slow_question(slow_n))
    assert time.monotonic() - start < 1.0
    assert tx.closed, "a transaction past its timeout is rolled back"


def test_a_transaction_past_its_timeout_runs_nothing_more(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin(timeout_ms=50)
    tx.execute("CREATE (:Entry {title: 'half-written'})")
    time.sleep(0.1)
    with pytest.raises(drevo.QueryTimeoutError):
        tx.execute("RETURN 1")
    assert tx.closed
    assert entries(drevo_db) == 0


def test_a_transaction_past_its_timeout_does_not_commit(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin(timeout_ms=50)
    tx.execute("CREATE (:Entry {title: 'too late'})")
    time.sleep(0.1)
    with pytest.raises(drevo.QueryTimeoutError):
        tx.commit()
    assert tx.closed
    assert entries(drevo_db) == 0


def test_a_transaction_past_its_timeout_is_not_prepared(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin(timeout_ms=50)
    tx.execute("CREATE (:Entry {title: 'in doubt'})")
    time.sleep(0.1)
    with pytest.raises(drevo.QueryTimeoutError):
        tx.prepare("journal-1")
    assert tx.closed
    assert drevo_db.list_prepared() == []
    assert entries(drevo_db) == 0


def test_a_transaction_within_its_timeout_commits(drevo_db: drevo.Drevo) -> None:
    with drevo_db.transaction(timeout_ms=60_000) as tx:
        tx.execute("CREATE (:Entry {title: 'in time'})")
    assert entries(drevo_db) == 1


def test_a_zero_timeout_is_no_limit(drevo_db: drevo.Drevo) -> None:
    assert drevo_db.execute("RETURN 1 AS one", timeout_ms=0).rows == [{"one": 1}]
    with drevo_db.transaction(timeout_ms=0) as tx:
        time.sleep(0.01)
        tx.execute("CREATE (:Entry)")
    assert entries(drevo_db) == 1


def test_a_negative_timeout_is_refused(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(ValueError, match="timeout_ms"):
        drevo_db.execute("RETURN 1", timeout_ms=-1)
    with pytest.raises(ValueError, match="timeout_ms"):
        drevo_db.begin(timeout_ms=-1)
