"""Integration tests for explicit transactions on the durable store (#554).

A committed transaction is one fsynced WAL batch: it survives closing and
reopening the database, while a rolled-back one leaves no trace. Concurrent
readers on other threads never observe a half-applied transaction.
"""

from __future__ import annotations

import threading

from faker import Faker

import drevo


def test_committed_transaction_survives_reopen(tmp_db_path: str, fake: Faker) -> None:
    names = [fake.unique.first_name() for _ in range(3)]
    with drevo.Drevo.open(tmp_db_path) as db:
        with db.transaction() as tx:
            for name in names:
                tx.execute("CREATE (:Person {name: $n})", {"n": name})
        rolled = db.begin()
        rolled.execute("CREATE (:Person {name: 'rolled back'})")
        rolled.rollback()

    with drevo.Drevo.open(tmp_db_path) as db:
        got = db.execute("MATCH (p:Person) RETURN p.name AS n ORDER BY n")
        assert [row["n"] for row in got] == sorted(names)


def test_readers_never_see_a_partial_transaction(disk_db: drevo.Drevo) -> None:
    seen: list[int] = []
    stop = threading.Event()

    def reader() -> None:
        while not stop.is_set():
            seen.append(int(disk_db.execute("MATCH (n:T) RETURN count(n) AS c")[0]["c"]))

    thread = threading.Thread(target=reader)
    thread.start()
    try:
        for _ in range(5):
            with disk_db.transaction() as tx:
                for _ in range(10):
                    tx.execute("CREATE (:T)")
    finally:
        stop.set()
        thread.join()

    assert seen, "the reader ran"
    assert all(c % 10 == 0 for c in seen), f"saw a partial transaction: {sorted(set(seen))}"
    assert disk_db.execute("MATCH (n:T) RETURN count(n) AS c")[0]["c"] == 50
