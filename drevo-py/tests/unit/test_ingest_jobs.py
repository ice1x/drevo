"""Unit tests for ingest jobs from Python: `drevo.ingest.submit`, `status`,
`list`, `pause`, `resume`, `cancel`, `retryFailed`, `errors` and `purge`,
through the Cypher API.

The embedded handle stores jobs but runs no scheduler and has no chat
endpoint, so a submitted job stays queued: these tests check what is stored
— a faker-written book cut into passages with exact character offsets — and
how the procedures steer a job. Running jobs is tested in Rust
(`tests/ingest_jobs_tests.rs`) against a scripted chat upstream.
"""

from __future__ import annotations

import time
from typing import Any

import pytest
from faker import Faker

import drevo

_SUBMIT = "CALL drevo.ingest.submit($items, $config) YIELD jobId, total RETURN jobId, total"
_STATUS = (
    "CALL drevo.ingest.status($job) YIELD job, items "
    "RETURN job.state AS state, job.total AS total, job.done AS done, items"
)


def _book(fake: Faker) -> str:
    return "\n\n".join(fake.paragraph(nb_sentences=8) for _ in range(12))


def _submit(db: drevo.Drevo, items: Any, config: dict[str, Any]) -> tuple[str, int]:
    [row] = db.execute(_SUBMIT, {"items": items, "config": config}).rows
    return row["jobId"], row["total"]


def _status(db: drevo.Drevo, job: str) -> dict[str, Any]:
    [row] = db.execute(_STATUS, {"job": job}).rows
    return row


def _wait_for_state(db: drevo.Drevo, job: str, state: str, limit: float = 5.0) -> dict[str, Any]:
    deadline = time.monotonic() + limit
    while True:
        status = _status(db, job)
        if status["state"] == state:
            return status
        assert time.monotonic() < deadline, f"job stayed {status['state']}, not {state}"
        time.sleep(0.02)


def test_a_book_is_stored_as_passages_with_exact_offsets(
    drevo_db: drevo.Drevo, fake: Faker
) -> None:
    book = _book(fake)
    job, total = _submit(
        drevo_db,
        {"text": book},
        {"source": {"id": "book-1", "title": fake.catch_phrase()}, "chunkChars": 300},
    )
    assert total > 3
    status = _wait_for_state(drevo_db, job, "queued")
    assert status["total"] == total
    assert status["done"] == 0
    assert status["items"] == {"queued": total, "running": 0, "done": 0, "failed": 0, "dead": 0}
    rows = drevo_db.execute(
        "MATCH (:IngestJob {id: $job})-[:HAS_ITEM]->(i:IngestItem) "
        "RETURN i.seq AS seq, i.offset AS offset, i.length AS length, i.text AS text "
        "ORDER BY i.seq",
        {"job": job},
    ).rows
    assert [r["seq"] for r in rows] == list(range(total))
    for r in rows:
        # Python indexes code points, as drevo counts characters.
        assert book[r["offset"] : r["offset"] + r["length"]] == r["text"]
        assert 1 <= r["length"] <= 300
    assert rows[-1]["offset"] + rows[-1]["length"] == len(book)


def test_list_items_follow_each_other_and_keep_their_meta(
    drevo_db: drevo.Drevo, fake: Faker
) -> None:
    chapters = [fake.sentence() for _ in range(3)]
    job, total = _submit(
        drevo_db,
        [chapters[0], {"text": chapters[1], "meta": {"chapter": 2}}, chapters[2]],
        {"source": {"id": "chapters"}},
    )
    assert total == 3
    rows = drevo_db.execute(
        "MATCH (i:IngestItem {job_id: $job}) RETURN i.offset AS offset, i.meta AS meta "
        "ORDER BY i.seq",
        {"job": job},
    ).rows
    assert [r["offset"] for r in rows] == [
        0,
        len(chapters[0]),
        len(chapters[0]) + len(chapters[1]),
    ]
    assert rows[1]["meta"] == '{"chapter":2}'
    assert rows[0]["meta"] is None


def test_pause_resume_cancel_and_purge(drevo_db: drevo.Drevo, fake: Faker) -> None:
    job, total = _submit(drevo_db, [fake.sentence() for _ in range(4)], {"source": {"id": "s"}})
    step = "CALL drevo.ingest.{}($job) YIELD job RETURN job.state AS state"
    assert drevo_db.execute(step.format("pause"), {"job": job}).rows == [{"state": "paused"}]
    with pytest.raises(drevo.CypherError, match="the job is already paused"):
        drevo_db.execute(step.format("pause"), {"job": job})
    assert drevo_db.execute(step.format("resume"), {"job": job}).rows == [{"state": "queued"}]
    with pytest.raises(drevo.CypherError, match="cancel it or let it finish"):
        drevo_db.execute("CALL drevo.ingest.purge($job)", {"job": job})
    assert drevo_db.execute(step.format("cancel"), {"job": job}).rows == [{"state": "cancelled"}]
    with pytest.raises(drevo.CypherError, match="cannot retry a job that is cancelled"):
        drevo_db.execute("CALL drevo.ingest.retryFailed($job)", {"job": job})
    [purged] = drevo_db.execute(
        "CALL drevo.ingest.purge($job) YIELD deleted RETURN deleted", {"job": job}
    ).rows
    assert purged == {"deleted": total}
    status = _status(drevo_db, job)
    assert (status["state"], status["total"]) == ("cancelled", total)
    assert (
        drevo_db.execute(
            "CALL drevo.ingest.errors($job, 10) YIELD seq RETURN seq", {"job": job}
        ).rows
        == []
    )


def test_list_filters_by_state(drevo_db: drevo.Drevo, fake: Faker) -> None:
    first, _ = _submit(drevo_db, [fake.sentence()], {"source": {"id": "a"}})
    second, _ = _submit(drevo_db, [fake.sentence()], {"source": {"id": "b"}})
    drevo_db.execute("CALL drevo.ingest.cancel($job)", {"job": first})
    listed = drevo_db.execute(
        "CALL drevo.ingest.list($state, 10) YIELD job RETURN job.id AS id", {"state": "queued"}
    ).rows
    assert listed == [{"id": second}]
    everything = drevo_db.execute(
        "CALL drevo.ingest.list(null, 10) YIELD job RETURN job.id AS id"
    ).rows
    assert {r["id"] for r in everything} == {first, second}


def test_bad_submissions_say_why(drevo_db: drevo.Drevo, fake: Faker) -> None:
    with pytest.raises(drevo.CypherError, match="needs a config with a `source`"):
        drevo_db.execute(_SUBMIT, {"items": [fake.sentence()], "config": {}})
    with pytest.raises(drevo.CypherError, match="`dryRun` does not apply to ingest jobs"):
        drevo_db.execute(
            _SUBMIT,
            {"items": [fake.sentence()], "config": {"source": {"id": "x"}, "dryRun": True}},
        )
    with pytest.raises(drevo.CypherError, match="there is no text to ingest"):
        drevo_db.execute(_SUBMIT, {"items": [], "config": {"source": {"id": "x"}}})
    with pytest.raises(drevo.CypherError, match="no ingest job `nope`"):
        drevo_db.execute("CALL drevo.ingest.status('nope')")
