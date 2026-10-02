"""Unit tests for explicit transactions — `Drevo.begin` / `Drevo.transaction` (#554).

A `Transaction` groups Cypher statements atomically: its writes are visible
to its own statements (read-your-writes), invisible to everyone else until
`commit()`, and discarded by `rollback()`. Used as a context manager it
commits on a clean exit and rolls back on an exception. Commit is
optimistic: if the graph changed since `begin()` it raises the retryable
`TransactionConflict`. A statement that fails inside a transaction rolls the
whole transaction back (as in Neo4j), and any use of a closed transaction
raises `TransactionError`.
"""

from __future__ import annotations

import gc

import pytest
from faker import Faker

import drevo


def count(db: drevo.Drevo, label: str) -> int:
    return int(db.execute(f"MATCH (n:{label}) RETURN count(n) AS c")[0]["c"])


# ── Commit / rollback ───────────────────────────────────────────────


def test_commit_makes_writes_visible(drevo_db: drevo.Drevo, fake: Faker) -> None:
    name = fake.name()
    tx = drevo_db.begin()
    tx.execute("CREATE (:Person {name: $n})", {"n": name})
    assert count(drevo_db, "Person") == 0, "uncommitted writes are invisible outside"
    tx.commit()
    assert tx.closed
    assert drevo_db.execute("MATCH (p:Person) RETURN p.name AS n").rows == [{"n": name}]


def test_rollback_discards_writes(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:Person {name: 'gone'})")
    tx.rollback()
    assert tx.closed
    assert count(drevo_db, "Person") == 0


def test_read_your_own_writes(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:T {v: 1})")
    tx.execute("MATCH (t:T) SET t.v = t.v + 1")
    assert tx.execute("MATCH (t:T) RETURN t.v AS v").rows == [{"v": 2}]
    tx.commit()
    assert drevo_db.execute("MATCH (t:T) RETURN t.v AS v").rows == [{"v": 2}]


def test_a_transaction_sees_a_stable_snapshot(drevo_db: drevo.Drevo) -> None:
    drevo_db.execute("CREATE (:T {v: 1})")
    tx = drevo_db.begin()
    drevo_db.execute("CREATE (:T {v: 2})")  # committed after begin()
    assert tx.execute("MATCH (t:T) RETURN count(t) AS c")[0]["c"] == 1
    tx.rollback()


def test_execute_returns_a_cypher_result(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    result = tx.execute("CREATE (n:T {v: $v}) RETURN n.v AS v", {"v": 7})
    assert isinstance(result, drevo.CypherResult)
    assert result.rows == [{"v": 7}]
    assert result.stats["nodes_created"] == 1
    tx.rollback()


# ── Context manager ─────────────────────────────────────────────────


def test_with_block_commits_on_clean_exit(drevo_db: drevo.Drevo) -> None:
    with drevo_db.transaction() as tx:
        tx.execute("CREATE (:T)")
        tx.execute("CREATE (:T)")
    assert tx.closed
    assert count(drevo_db, "T") == 2


def test_with_block_rolls_back_on_exception(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(KeyError):
        with drevo_db.transaction() as tx:
            tx.execute("CREATE (:T)")
            raise KeyError("boom")
    assert tx.closed
    assert count(drevo_db, "T") == 0


def test_explicit_commit_inside_with_is_fine(drevo_db: drevo.Drevo) -> None:
    with drevo_db.transaction() as tx:
        tx.execute("CREATE (:T)")
        tx.commit()
    assert count(drevo_db, "T") == 1


# ── Conflicts and failures ──────────────────────────────────────────


def test_concurrent_write_makes_commit_conflict(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:T {who: 'tx'})")
    drevo_db.execute("CREATE (:T {who: 'autocommit'})")
    with pytest.raises(drevo.TransactionConflict):
        tx.commit()
    assert tx.closed
    assert drevo_db.execute("MATCH (t:T) RETURN t.who AS w").rows == [{"w": "autocommit"}]


def test_conflict_is_retryable(drevo_db: drevo.Drevo) -> None:
    def transfer() -> None:
        with drevo_db.transaction() as tx:
            tx.execute("CREATE (:T)")

    tx = drevo_db.begin()
    tx.execute("CREATE (:T)")
    drevo_db.execute("CREATE (:Other)")
    with pytest.raises(drevo.TransactionConflict):
        tx.commit()
    transfer()  # a fresh transaction on the new state commits
    assert count(drevo_db, "T") == 1


def test_a_failed_statement_rolls_the_transaction_back(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:T)")
    with pytest.raises(drevo.CypherError):
        tx.execute("RETURN undefined_variable AS v")
    assert tx.closed
    with pytest.raises(drevo.TransactionError):
        tx.commit()
    assert count(drevo_db, "T") == 0


def test_a_syntax_error_also_closes_the_transaction(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    with pytest.raises(drevo.CypherSyntaxError):
        tx.execute("CREATE (:T")
    assert tx.closed


@pytest.mark.parametrize("action", ["execute", "commit", "rollback"])
def test_a_closed_transaction_raises(drevo_db: drevo.Drevo, action: str) -> None:
    tx = drevo_db.begin()
    tx.commit()
    with pytest.raises(drevo.TransactionError):
        if action == "execute":
            tx.execute("RETURN 1 AS v")
        else:
            getattr(tx, action)()


def test_an_abandoned_transaction_is_rolled_back(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:T)")
    del tx
    gc.collect()
    assert count(drevo_db, "T") == 0
    drevo_db.execute("CREATE (:T)")  # the graph is not wedged by the leak
    assert count(drevo_db, "T") == 1


def test_begin_on_a_closed_handle_raises() -> None:
    db = drevo.Drevo.open_in_memory()
    db.close()
    with pytest.raises(RuntimeError):
        db.begin()


def test_exception_hierarchy() -> None:
    assert issubclass(drevo.TransactionError, drevo.DrevoError)
    assert issubclass(drevo.TransactionConflict, drevo.TransactionError)
    assert issubclass(drevo.ConstraintViolation, drevo.ConflictError)


def test_closing_the_database_with_an_open_transaction_does_not_hang() -> None:
    db = drevo.Drevo.open_in_memory()
    tx = db.begin()
    tx.execute("CREATE (:T)")
    db.close()  # must not wait for the transaction object to go away
    with pytest.raises(RuntimeError):
        tx.execute("RETURN 1 AS v")
