"""Integration tests for `Drevo.execute` on the durable (WAL) store (#553).

Autocommit means a Cypher write is on disk when `execute` returns: it must
survive closing and reopening the database, and it must be visible to the
typed CRUD API on the same data. Execution releases the GIL like every other
binding call, so Python threads can run statements concurrently.
"""

from __future__ import annotations

import threading

from faker import Faker

import drevo


def test_cypher_writes_survive_reopen(tmp_db_path: str, fake: Faker) -> None:
    names = [fake.unique.first_name() for _ in range(5)]
    with drevo.Drevo.open(tmp_db_path) as db:
        db.execute("UNWIND $names AS name CREATE (:Person {name: name})", {"names": names})
        db.execute(
            "MATCH (a:Person {name: $a}), (b:Person {name: $b}) CREATE (a)-[:KNOWS]->(b)",
            {"a": names[0], "b": names[1]},
        )

    with drevo.Drevo.open(tmp_db_path) as db:
        got = db.execute("MATCH (p:Person) RETURN p.name AS name ORDER BY name")
        assert [row["name"] for row in got] == sorted(names)
        rel = db.execute("MATCH (a)-[r:KNOWS]->(b) RETURN a.name AS a, b.name AS b").rows
        assert rel == [{"a": names[0], "b": names[1]}]


def test_cypher_and_crud_share_one_graph(disk_db: drevo.Drevo, fake: Faker) -> None:
    title = fake.sentence()
    node = disk_db.create_node(drevo.NewNode(kind="note", title=title))
    disk_db.execute("MATCH (n:note {title: $t}) SET n.stars = 5", {"t": title})
    assert disk_db.get_node(node.id).properties["stars"] == 5

    disk_db.execute("CREATE (:note {title: 'from cypher'})")
    assert disk_db.get_node_by_title("from cypher").kind == "note"


def test_threads_execute_concurrently(disk_db: drevo.Drevo) -> None:
    errors: list[BaseException] = []

    def work(i: int) -> None:
        try:
            for j in range(10):
                disk_db.execute("CREATE (:T {i: $i, j: $j})", {"i": i, "j": j})
        except BaseException as exc:  # noqa: BLE001 — surfaced below
            errors.append(exc)

    threads = [threading.Thread(target=work, args=(i,)) for i in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert errors == []
    assert disk_db.execute("MATCH (n:T) RETURN count(n) AS c")[0]["c"] == 40
