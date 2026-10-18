"""Unit tests for triggers (`apoc.trigger.*`) through the Python handle:
statements the database runs when a transaction commits a change.
"""

from __future__ import annotations

import time

import pytest
from faker import Faker

import drevo

_AUDIT = "UNWIND $createdNodes AS n WITH n WHERE n:Task " "CREATE (:Audit {task: n.title})"
_REFUSE_UNOWNED = "UNWIND $createdNodes AS n WITH n WHERE n:Task AND n.owner IS NULL RETURN 1 / 0"


def _install(db: drevo.Drevo, name: str, statement: str, phase: str) -> None:
    db.execute(
        "CALL apoc.trigger.install('drevo', $name, $statement, {phase: $phase})",
        {"name": name, "statement": statement, "phase": phase},
    )


def _audited(db: drevo.Drevo) -> list[str]:
    rows = db.execute("MATCH (a:Audit) RETURN a.task AS task ORDER BY task").rows
    return [row["task"] for row in rows]


def test_an_after_trigger_audits_created_tasks(drevo_db: drevo.Drevo, fake: Faker) -> None:
    title = fake.catch_phrase()
    _install(drevo_db, "audit", _AUDIT, "after")
    drevo_db.execute("CREATE (:Task {title: $title})", {"title": title})
    assert _audited(drevo_db) == [title]


def test_a_before_trigger_writes_with_the_transaction(drevo_db: drevo.Drevo, fake: Faker) -> None:
    owner = fake.user_name()
    drevo_db.execute(
        "CALL apoc.trigger.install('drevo', 'default_owner', "
        "'UNWIND $createdNodes AS n WITH n WHERE n:Task AND n.owner IS NULL "
        "SET n.owner = $owner', {phase: 'before'}, {params: {owner: $owner}})",
        {"owner": owner},
    )
    with drevo_db.transaction() as tx:
        tx.execute("CREATE (:Task {title: 'a'})")
        tx.execute("CREATE (:Task {title: 'b'})")
    rows = drevo_db.execute("MATCH (t:Task) RETURN t.owner AS owner").rows
    assert [row["owner"] for row in rows] == [owner, owner]


def test_a_failing_before_trigger_rejects_the_commit(drevo_db: drevo.Drevo) -> None:
    _install(drevo_db, "require_owner", _REFUSE_UNOWNED, "before")
    tx = drevo_db.begin()
    tx.execute("CREATE (:Task {title: 'orphan'})")
    with pytest.raises(drevo.TransactionError, match="require_owner"):
        tx.commit()
    count = drevo_db.execute("MATCH (t:Task) RETURN count(t) AS n").rows[0]["n"]
    assert count == 0


def test_a_failing_before_trigger_fails_an_autocommit_statement(
    drevo_db: drevo.Drevo,
) -> None:
    _install(drevo_db, "require_owner", _REFUSE_UNOWNED, "before")
    with pytest.raises(drevo.CypherError, match="require_owner"):
        drevo_db.execute("CREATE (:Task {title: 'orphan'})")
    drevo_db.execute("CREATE (:Task {title: 'owned', owner: 'ann'})")
    count = drevo_db.execute("MATCH (t:Task) RETURN count(t) AS n").rows[0]["n"]
    assert count == 1


def test_after_async_triggers_run_off_the_commit(drevo_db: drevo.Drevo, fake: Faker) -> None:
    title = fake.catch_phrase()
    _install(drevo_db, "audit_async", _AUDIT, "afterAsync")
    drevo_db.execute("CREATE (:Task {title: $title})", {"title": title})
    deadline = time.monotonic() + 10
    while not _audited(drevo_db):
        assert time.monotonic() < deadline, "the afterAsync trigger never ran"
        time.sleep(0.01)
    assert _audited(drevo_db) == [title]


def test_triggers_are_listed_stopped_and_dropped(drevo_db: drevo.Drevo) -> None:
    _install(drevo_db, "audit", _AUDIT, "after")
    listed = drevo_db.execute(
        "CALL apoc.trigger.list() YIELD name, selector, paused "
        "RETURN name, selector.phase AS phase, paused"
    ).rows
    assert listed == [{"name": "audit", "phase": "after", "paused": False}]

    drevo_db.execute("CALL apoc.trigger.stop('drevo', 'audit')")
    drevo_db.execute("CREATE (:Task {title: 'quiet'})")
    assert _audited(drevo_db) == []

    drevo_db.execute("CALL apoc.trigger.drop('drevo', 'audit')")
    assert drevo_db.execute("CALL apoc.trigger.list()").rows == []
