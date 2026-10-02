"""Two-phase commit across a restart (#556 slice 3).

A prepared transaction is durable: closing and reopening the store (or a
crash) brings it back as prepared, still blocking writes, and it can be
resolved from the reopened handle, as a coordinator recovering in-doubt
transactions would.
"""

from __future__ import annotations

import pytest

import drevo
import drevo.dbapi as dbapi


def test_prepared_survives_reopen_then_commit(tmp_db_path: str) -> None:
    with drevo.Drevo.open(tmp_db_path) as db:
        tx = db.begin()
        tx.execute("CREATE (:Person {name: 'ada'})")
        tx.prepare("in-doubt")

    with drevo.Drevo.open(tmp_db_path) as db:
        assert [p.gid for p in db.list_prepared()] == ["in-doubt"]
        with pytest.raises(drevo.TransactionConflict):
            db.execute("CREATE (:X)")
        db.commit_prepared("in-doubt")

    with drevo.Drevo.open(tmp_db_path) as db:
        assert db.execute("MATCH (p:Person) RETURN p.name AS n").rows == [{"n": "ada"}]
        assert db.list_prepared() == []


def test_dbapi_recover_after_restart(tmp_db_path: str) -> None:
    conn = dbapi.connect(tmp_db_path)
    xid = conn.xid(3, "transfer-9", "drevo")
    conn.tpc_begin(xid)
    conn.cursor().execute("CREATE (:Transfer {id: 9})")
    conn.tpc_prepare()
    conn.close()  # a prepared branch outlives its connection

    conn = dbapi.connect(tmp_db_path)
    (recovered,) = conn.tpc_recover()
    assert recovered == xid
    conn.tpc_rollback(recovered)
    cur = conn.cursor()
    cur.execute("MATCH (t:Transfer) RETURN count(t)")
    assert cur.fetchone() == (0,)
    conn.close()
