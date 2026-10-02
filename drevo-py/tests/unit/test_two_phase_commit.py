"""Unit tests for two-phase commit from Python (#556 slice 3).

`Transaction.prepare(gid)` validates the transaction and records it as
prepared; it is then resolved with `Drevo.commit_prepared(gid)` or
`Drevo.rollback_prepared(gid)`, and `Drevo.list_prepared()` lists in-doubt
transactions. While anything is prepared, every other write raises the
retryable `TransactionConflict`; reads keep working. `drevo.dbapi` exposes
the same through the optional PEP 249 two-phase-commit extension (`xid`,
`tpc_begin`, `tpc_prepare`, `tpc_commit`, `tpc_rollback`, `tpc_recover`).
"""

from __future__ import annotations

import pytest

import drevo
import drevo.dbapi as dbapi


def count(db: drevo.Drevo, label: str) -> int:
    return int(db.execute(f"MATCH (n:{label}) RETURN count(n) AS c")[0]["c"])


def prepared(db: drevo.Drevo, gid: str, n: int = 1) -> None:
    tx = db.begin()
    for _ in range(n):
        tx.execute("CREATE (:T)")
    tx.prepare(gid)
    assert tx.closed


def test_prepare_then_commit(drevo_db: drevo.Drevo) -> None:
    prepared(drevo_db, "g1", 2)
    assert count(drevo_db, "T") == 0
    (info,) = drevo_db.list_prepared()
    assert isinstance(info, drevo.PreparedTransaction)
    assert (info.gid, info.op_count) == ("g1", 2)
    assert info.prepared_at_ms > 0
    drevo_db.commit_prepared("g1")
    assert count(drevo_db, "T") == 2
    assert drevo_db.list_prepared() == []


def test_prepare_then_rollback(drevo_db: drevo.Drevo) -> None:
    prepared(drevo_db, "g1")
    drevo_db.rollback_prepared("g1")
    assert count(drevo_db, "T") == 0
    drevo_db.execute("CREATE (:T)")  # the fence is lifted


def test_writes_while_prepared_raise_transaction_conflict(drevo_db: drevo.Drevo) -> None:
    other = drevo_db.begin()
    other.execute("CREATE (:Other)")
    prepared(drevo_db, "g1")
    with pytest.raises(drevo.TransactionConflict, match="g1"):
        drevo_db.execute("CREATE (:X)")
    with pytest.raises(drevo.TransactionConflict):
        drevo_db.create_node(drevo.NewNode(kind="note", title="blocked"))
    with pytest.raises(drevo.TransactionConflict):
        other.commit()
    assert count(drevo_db, "X") == 0, "reads still work"
    drevo_db.commit_prepared("g1")


def test_unknown_gid(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.UnknownGidError) as info:
        drevo_db.commit_prepared("missing")
    assert isinstance(info.value, drevo.PreparedTransactionError)
    assert isinstance(info.value, drevo.TransactionError)
    with pytest.raises(drevo.UnknownGidError):
        drevo_db.rollback_prepared("missing")


def test_prepare_conflict_closes_the_transaction(drevo_db: drevo.Drevo) -> None:
    tx = drevo_db.begin()
    tx.execute("CREATE (:T)")
    drevo_db.execute("CREATE (:Concurrent)")
    with pytest.raises(drevo.TransactionConflict):
        tx.prepare("late")
    assert tx.closed
    assert drevo_db.list_prepared() == []


def test_duplicate_gid_and_closed_transaction(drevo_db: drevo.Drevo) -> None:
    prepared(drevo_db, "dup")
    tx = drevo_db.begin()
    with pytest.raises(drevo.PreparedTransactionError):
        tx.prepare("dup")
    with pytest.raises(drevo.TransactionError):
        tx.prepare("again")  # the failed prepare closed it
    drevo_db.rollback_prepared("dup")


def test_prepare_via_with_block(drevo_db: drevo.Drevo) -> None:
    with drevo_db.transaction() as tx:
        tx.execute("CREATE (:T)")
        tx.prepare("w1")
    # Leaving the block does not commit a prepared transaction.
    assert count(drevo_db, "T") == 0
    drevo_db.commit_prepared("w1")
    assert count(drevo_db, "T") == 1


def test_exception_hierarchy() -> None:
    assert issubclass(drevo.PreparedTransactionError, drevo.TransactionError)
    assert issubclass(drevo.UnknownGidError, drevo.PreparedTransactionError)


# ── PEP 249 two-phase-commit extension ──────────────────────────────────


def test_dbapi_xid_is_a_triple() -> None:
    with dbapi.connect(":memory:") as conn:
        xid = conn.xid(7, "global-1", "branch-a")
        assert (xid[0], xid[1], xid[2]) == (7, "global-1", "branch-a")
        assert xid.format_id == 7 and xid.gtrid == "global-1" and xid.bqual == "branch-a"


def test_dbapi_tpc_prepare_recover_commit() -> None:
    db = drevo.Drevo.open_in_memory()
    conn = dbapi.connect(db)
    xid = conn.xid(1, "order-42", "drevo")
    conn.tpc_begin(xid)
    conn.cursor().execute("CREATE (:Order {id: 42})")
    conn.tpc_prepare()
    assert count(db, "Order") == 0
    assert conn.tpc_recover() == [xid]

    other = dbapi.connect(db)  # any connection can resolve it
    other.tpc_commit(xid)
    assert count(db, "Order") == 1
    assert other.tpc_recover() == []
    conn.close()
    other.close()
    db.close()


def test_dbapi_tpc_one_phase_and_rollback() -> None:
    db = drevo.Drevo.open_in_memory()
    conn = dbapi.connect(db)
    xid = conn.xid(1, "single", "drevo")
    conn.tpc_begin(xid)
    conn.cursor().execute("CREATE (:T)")
    conn.tpc_commit()  # no prepare: one-phase commit
    assert count(db, "T") == 1

    xid2 = conn.xid(1, "undo", "drevo")
    conn.tpc_begin(xid2)
    conn.cursor().execute("CREATE (:T)")
    conn.tpc_prepare()
    conn.tpc_rollback()
    assert count(db, "T") == 1
    assert conn.tpc_recover() == []
    conn.close()
    db.close()


def test_dbapi_tpc_misuse_is_a_programming_error() -> None:
    with dbapi.connect(":memory:") as conn:
        with pytest.raises(dbapi.ProgrammingError):
            conn.tpc_prepare()  # no tpc_begin
        xid = conn.xid(1, "g", "b")
        conn.tpc_begin(xid)
        with pytest.raises(dbapi.ProgrammingError):
            conn.commit()  # PEP 249: commit() is invalid inside a TPC transaction
        with pytest.raises(dbapi.ProgrammingError):
            conn.tpc_begin(xid)  # already in one
        conn.tpc_rollback()
        with pytest.raises(dbapi.ProgrammingError):
            conn.tpc_commit(conn.xid(1, "unknown", "b"))


# ── Heuristic rollback (#556 slice 4) ───────────────────────────────────


def test_heuristic_rollback_is_reported_to_a_late_commit(drevo_db: drevo.Drevo) -> None:
    prepared(drevo_db, "abandoned")
    drevo_db.heuristic_rollback_prepared("abandoned")
    assert drevo_db.list_prepared() == []
    drevo_db.execute("CREATE (:Free)")  # the fence is lifted
    with pytest.raises(drevo.HeuristicRollbackError) as info:
        drevo_db.commit_prepared("abandoned")
    assert isinstance(info.value, drevo.PreparedTransactionError)
    drevo_db.rollback_prepared("abandoned")  # agrees with the outcome


def test_dbapi_late_tpc_commit_after_heuristic_rollback() -> None:
    db = drevo.Drevo.open_in_memory()
    conn = dbapi.connect(db)
    xid = conn.xid(1, "late", "drevo")
    conn.tpc_begin(xid)
    conn.cursor().execute("CREATE (:T)")
    conn.tpc_prepare()
    (info,) = db.list_prepared()
    db.heuristic_rollback_prepared(info.gid)
    with pytest.raises(dbapi.OperationalError, match="heuristically"):
        conn.tpc_commit()
    conn.close()
    db.close()
