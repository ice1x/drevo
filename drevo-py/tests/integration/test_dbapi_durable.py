"""Integration tests for `drevo.dbapi` on the durable store (#555).

DB-API work is one implicit transaction per connection: committed work
survives reopening the store, uncommitted work never reaches disk, and a
batch (`executemany`) inside one transaction is all-or-nothing.
"""

from __future__ import annotations

from faker import Faker

import drevo.dbapi as dbapi


def count(conn: dbapi.Connection, label: str) -> int:
    cur = conn.cursor()
    cur.execute(f"MATCH (n:{label}) RETURN count(n)")
    row = cur.fetchone()
    assert row is not None
    return int(row[0])


def test_commit_survives_reopen(tmp_db_path: str, fake: Faker) -> None:
    names = [fake.unique.first_name() for _ in range(5)]
    conn = dbapi.connect(tmp_db_path)
    conn.cursor().executemany("CREATE (:Person {name: $n})", [{"n": n} for n in names])
    conn.commit()
    conn.close()

    conn = dbapi.connect(tmp_db_path)
    cur = conn.cursor()
    cur.execute("MATCH (p:Person) RETURN p.name ORDER BY p.name")
    assert [row[0] for row in cur.fetchall()] == sorted(names)
    conn.close()


def test_uncommitted_work_never_reaches_disk(tmp_db_path: str) -> None:
    conn = dbapi.connect(tmp_db_path)
    conn.cursor().execute("CREATE (:T)")
    conn.close()
    with dbapi.connect(tmp_db_path) as conn:
        assert count(conn, "T") == 0


def test_a_batch_is_all_or_nothing(tmp_db_path: str) -> None:
    with dbapi.connect(tmp_db_path) as conn:
        conn.cursor().execute("CREATE (:note {title: 'taken'})")

    conn = dbapi.connect(tmp_db_path)
    cur = conn.cursor()
    try:
        cur.executemany(
            "CREATE (:note {title: $t})",
            [{"t": "first"}, {"t": "second"}, {"t": "taken"}],  # the last one collides
        )
    except dbapi.IntegrityError:
        pass
    else:
        raise AssertionError("the duplicate title should have failed the batch")
    conn.commit()  # the failure already rolled the transaction back
    conn.close()

    with dbapi.connect(tmp_db_path) as conn:
        cur = conn.cursor()
        cur.execute("MATCH (n:note) RETURN n.title ORDER BY n.title")
        assert cur.fetchall() == [("taken",)]
