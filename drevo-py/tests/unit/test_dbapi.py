"""Unit tests for `drevo.dbapi` — the PEP 249 (DB-API 2.0) module (#555).

The query language is Cypher. Parameters are named and written in Cypher's
own `$name` syntax (PEP 249's `:name` would collide with `:Label`). A
connection runs an implicit transaction (DB-API semantics): it begins on the
first statement, `commit()` applies it, `rollback()` and `close()` discard
it. Every PEP 249 module attribute, exception, type object, constructor,
connection and cursor method has a test here.
"""

from __future__ import annotations

import datetime as dt
from typing import Any

import pytest
from faker import Faker

import drevo
import drevo.dbapi as dbapi

# ── Module interface ────────────────────────────────────────────────


def test_module_globals() -> None:
    assert dbapi.apilevel == "2.0"
    assert dbapi.threadsafety == 1
    assert dbapi.paramstyle == "named"


@pytest.mark.parametrize(
    "cls,parent",
    [
        (dbapi.Warning, Exception),
        (dbapi.Error, Exception),
        (dbapi.InterfaceError, dbapi.Error),
        (dbapi.DatabaseError, dbapi.Error),
        (dbapi.DataError, dbapi.DatabaseError),
        (dbapi.OperationalError, dbapi.DatabaseError),
        (dbapi.IntegrityError, dbapi.DatabaseError),
        (dbapi.InternalError, dbapi.DatabaseError),
        (dbapi.ProgrammingError, dbapi.DatabaseError),
        (dbapi.NotSupportedError, dbapi.DatabaseError),
    ],
)
def test_exception_hierarchy(cls: type, parent: type) -> None:
    assert issubclass(cls, parent)


def test_exceptions_are_also_connection_attributes() -> None:
    with dbapi.connect(":memory:") as conn:
        assert conn.ProgrammingError is dbapi.ProgrammingError
        assert conn.IntegrityError is dbapi.IntegrityError


def test_type_objects_and_constructors() -> None:
    assert dbapi.Date(2026, 10, 2) == dt.date(2026, 10, 2)
    assert dbapi.Time(1, 2, 3) == dt.time(1, 2, 3)
    assert dbapi.Timestamp(2026, 10, 2, 1, 2, 3) == dt.datetime(2026, 10, 2, 1, 2, 3)
    assert isinstance(dbapi.DateFromTicks(0), dt.date)
    assert isinstance(dbapi.TimeFromTicks(0), dt.time)
    assert isinstance(dbapi.TimestampFromTicks(0), dt.datetime)
    assert dbapi.Binary(b"x") == b"x"
    # PEP 249 type objects are compared with `==` against type codes.
    assert dbapi.STRING == str  # noqa: E721
    assert dbapi.NUMBER == int and dbapi.NUMBER == float  # noqa: E721
    assert dbapi.STRING != int  # noqa: E721
    for type_object in (dbapi.BINARY, dbapi.DATETIME, dbapi.ROWID):
        assert type_object is not None


# ── Connection ──────────────────────────────────────────────────────


@pytest.fixture
def conn() -> Any:
    connection = dbapi.connect(":memory:")
    yield connection
    connection.close()


def test_commit_applies_the_implicit_transaction(conn: Any, fake: Faker) -> None:
    name = fake.name()
    cur = conn.cursor()
    cur.execute("CREATE (:Person {name: $n})", {"n": name})
    conn.commit()
    cur.execute("MATCH (p:Person) RETURN p.name AS name")
    assert cur.fetchall() == [(name,)]


def test_rollback_discards_the_implicit_transaction(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("CREATE (:Person {name: 'gone'})")
    conn.rollback()
    cur.execute("MATCH (p:Person) RETURN count(p) AS c")
    assert cur.fetchone() == (0,)


def test_uncommitted_work_is_invisible_to_another_connection(tmp_path: Any) -> None:
    path = str(tmp_path / "graph.drevo")
    writer = dbapi.connect(path)
    writer.cursor().execute("CREATE (:T)")
    writer.close()  # closing without commit rolls back
    reader = dbapi.connect(path)
    cur = reader.cursor()
    cur.execute("MATCH (n:T) RETURN count(n) AS c")
    assert cur.fetchone() == (0,)
    reader.close()


def test_with_block_commits_or_rolls_back(tmp_path: Any) -> None:
    path = str(tmp_path / "graph.drevo")
    with dbapi.connect(path) as conn:
        conn.cursor().execute("CREATE (:T {v: 1})")
    with pytest.raises(KeyError):
        with dbapi.connect(path) as conn:
            conn.cursor().execute("CREATE (:T {v: 2})")
            raise KeyError("boom")
    with dbapi.connect(path) as conn:
        cur = conn.cursor()
        cur.execute("MATCH (t:T) RETURN t.v AS v")
        assert cur.fetchall() == [(1,)]


def test_commit_and_rollback_without_work_are_no_ops(conn: Any) -> None:
    conn.commit()
    conn.rollback()


def test_closed_connection_raises(conn: Any) -> None:
    conn.close()
    conn.close()  # idempotent
    with pytest.raises(dbapi.InterfaceError):
        conn.cursor()
    with pytest.raises(dbapi.InterfaceError):
        conn.commit()


def test_conflicting_commit_is_an_operational_error(tmp_path: Any) -> None:
    db = drevo.Drevo.open_in_memory()
    conn = dbapi.connect(db)
    conn.cursor().execute("CREATE (:T)")
    db.execute("CREATE (:Other)")  # a write after the implicit begin
    with pytest.raises(dbapi.OperationalError):
        conn.commit()
    conn.close()
    assert db.execute("MATCH (n) RETURN count(n) AS c")[0]["c"] == 1, "the handle stays open"
    db.close()


# ── Cursor ──────────────────────────────────────────────────────────


def test_description_and_fetch_family(conn: Any) -> None:
    cur = conn.cursor()
    assert cur.description is None
    assert cur.rowcount == -1
    assert cur.arraysize == 1
    cur.execute("UNWIND range(1, 5) AS i RETURN i AS n, toString(i) AS s")
    names = [d[0] for d in cur.description]
    assert names == ["n", "s"]
    assert all(len(d) == 7 for d in cur.description)
    assert cur.description[0][1] == dbapi.NUMBER
    assert cur.description[1][1] == dbapi.STRING
    assert cur.rowcount == 5
    assert cur.fetchone() == (1, "1")
    assert cur.fetchmany() == [(2, "2")]
    assert cur.fetchmany(2) == [(3, "3"), (4, "4")]
    assert cur.fetchall() == [(5, "5")]
    assert cur.fetchone() is None
    assert cur.fetchall() == []


def test_arraysize_drives_fetchmany(conn: Any) -> None:
    cur = conn.cursor()
    cur.arraysize = 3
    cur.execute("UNWIND range(1, 4) AS i RETURN i")
    assert len(cur.fetchmany()) == 3


def test_iteration(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("UNWIND [1, 2, 3] AS i RETURN i")
    assert [row[0] for row in cur] == [1, 2, 3]


def test_write_rowcount_and_description(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("UNWIND range(1, 3) AS i CREATE (:T {i: i})")
    assert cur.description is None
    assert cur.rowcount == 3


def test_execute_returns_the_cursor(conn: Any) -> None:
    cur = conn.cursor()
    assert cur.execute("RETURN 1 AS v") is cur


def test_executemany(conn: Any, fake: Faker) -> None:
    names = [fake.unique.first_name() for _ in range(4)]
    cur = conn.cursor()
    cur.executemany("CREATE (:Person {name: $n})", [{"n": n} for n in names])
    assert cur.rowcount == 4
    cur.execute("MATCH (p:Person) RETURN p.name ORDER BY p.name")
    assert cur.fetchall() == [(n,) for n in sorted(names)]


def test_date_and_time_parameters_become_iso_strings(conn: Any) -> None:
    cur = conn.cursor()
    when = dt.datetime(2026, 10, 2, 12, 30, 0)
    cur.execute("RETURN $d AS d, $t AS t", {"d": dt.date(2026, 10, 2), "t": when})
    assert cur.fetchone() == ("2026-10-02", "2026-10-02T12:30:00")


def test_graph_values_come_back_as_drevo_objects(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("CREATE (n:T {v: 1}) RETURN n")
    (node,) = cur.fetchone()
    assert isinstance(node, drevo.CypherNode)
    assert node.properties == {"v": 1}


def test_setinputsizes_and_setoutputsize_are_accepted(conn: Any) -> None:
    cur = conn.cursor()
    cur.setinputsizes([None])
    cur.setoutputsize(10)
    cur.setoutputsize(10, 0)


def test_fetch_before_execute_is_a_programming_error(conn: Any) -> None:
    with pytest.raises(dbapi.ProgrammingError):
        conn.cursor().fetchone()


def test_closed_cursor_raises(conn: Any) -> None:
    cur = conn.cursor()
    cur.close()
    with pytest.raises(dbapi.InterfaceError):
        cur.execute("RETURN 1 AS v")
    with pytest.raises(dbapi.InterfaceError):
        cur.fetchall()


def test_cursor_reports_its_connection(conn: Any) -> None:
    assert conn.cursor().connection is conn


# ── Error mapping ───────────────────────────────────────────────────


@pytest.mark.parametrize(
    "query,params,error",
    [
        ("MATCH (n RETURN n", None, dbapi.ProgrammingError),
        ("RETURN $missing AS v", None, dbapi.ProgrammingError),
        ("RETURN undefined_variable AS v", None, dbapi.ProgrammingError),
        ("RETURN $v AS v", {"v": 2**70}, dbapi.DataError),
        ("RETURN $v AS v", {"v": object()}, dbapi.ProgrammingError),
        ("RETURN $v AS v", {"v": b"raw"}, dbapi.NotSupportedError),
    ],
    ids=["syntax", "missing-param", "semantic", "overflow", "bad-type", "binary"],
)
def test_errors_map_to_pep249_classes(
    conn: Any, query: str, params: dict[str, Any] | None, error: type
) -> None:
    with pytest.raises(error) as info:
        conn.cursor().execute(query, params)
    assert info.value.__cause__ is not None or error is dbapi.NotSupportedError


def test_duplicate_title_is_an_integrity_error(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("CREATE (:note {title: 'dup'})")
    with pytest.raises(dbapi.IntegrityError):
        cur.execute("CREATE (:note {title: 'dup'})")


def test_a_failed_statement_aborts_the_implicit_transaction(conn: Any) -> None:
    cur = conn.cursor()
    cur.execute("CREATE (:T)")
    with pytest.raises(dbapi.ProgrammingError):
        cur.execute("RETURN undefined_variable AS v")
    conn.commit()  # nothing left to commit: the failure rolled it back
    cur.execute("MATCH (n:T) RETURN count(n)")
    assert cur.fetchone() == (0,)


def test_params_must_be_a_mapping(conn: Any) -> None:
    with pytest.raises(dbapi.ProgrammingError):
        conn.cursor().execute("RETURN 1 AS v", [1])  # type: ignore[arg-type]
