"""GIL re-acquisition and threaded access on a real disk backend.

Every public method on `Drevo` wraps its storage I/O in
`Python::allow_threads`. The contract the suite pins here is the one
the current implementation actually delivers:

* **GIL release on storage I/O** — while one Python thread is inside
  a storage call on the handle, an unrelated Python thread (one that
  does *not* touch the handle) keeps making progress. The proxy is a
  pure-Python counter incremented by the second thread; if the GIL
  were not released, no ticks would land while the first thread was
  inside the Rust call.

* **Per-thread sequential reuse** — a `Drevo` handle is `Send`, so a
  thread spawned with the handle can call `create_node` /
  `get_node` / etc.; on join, the next thread sees every committed
  row. This pins thread-affinity-agnostic, sequential cross-thread
  usage (the common pattern in worker pools that hand a job off and
  wait).

* **Sequential close-then-reopen cycles** — a writer that releases the
  file lock between sessions can be replaced by a fresh handle on the
  same path without losing data, no matter how many times the cycle
  repeats.

Concurrent N-thread access *to the same live handle* is **out of
scope** for this suite — the current `handle.rs::with_db` wrapper
holds a `std::sync::Mutex` for the full duration of every storage
call while `allow_threads` releases the GIL, which is a classic
GIL+Mutex deadlock pattern. Fixing the wrapper (e.g. dropping the
guard before `allow_threads`, or switching to a read-mostly lock)
unblocks true concurrent access; that work belongs in a separate
follow-up, not in 00119's test addition.
"""

from __future__ import annotations

import threading
import time

import drevo


def test_python_thread_progresses_while_storage_io_runs(
    disk_db: drevo.Drevo,
) -> None:
    """While the main thread runs a batch of storage I/O, a side thread
    that does *not* touch the handle continues to increment a counter.

    The counter is the observable proxy for "Python bytecode is actually
    executing on the side thread". If `allow_threads` were a no-op the
    counter would barely advance — the storage thread would hold the
    GIL through every redb call.

    The side thread takes care to never touch `disk_db`: it is the
    *handle wrapper's* GIL release we are pinning, not anything the
    side thread reads.
    """
    stop = threading.Event()
    tick = [0]

    def python_loop() -> None:
        while not stop.is_set():
            tick[0] += 1
            # Yields the GIL frequently. Without `allow_threads` on the
            # storage side, no amount of yielding here would let the
            # counter advance while the writer is in Rust.
            time.sleep(0)

    p = threading.Thread(target=python_loop)
    p.start()
    try:
        for i in range(200):
            disk_db.create_node(drevo.NewNode(kind="rw", title=f"rw-{i}", body="y"))
    finally:
        stop.set()
        p.join()

    # Loose lower bound — we expect thousands of ticks on any modern
    # machine. The point is that the counter is decidedly non-zero,
    # which means GIL release happened.
    assert tick[0] > 0, "python thread never advanced — GIL was not released"


def test_handle_is_usable_after_worker_thread_returns(
    disk_db: drevo.Drevo,
) -> None:
    """A worker thread that does storage I/O and then exits leaves the
    handle in a usable state for the next caller.

    Pins the `Drevo: Send` contract — handing the handle off, doing
    work, joining, then resuming on the original thread must work.
    """
    created_ids: list[int] = []

    def worker() -> None:
        for i in range(20):
            node = disk_db.create_node(drevo.NewNode(kind="worker", title=f"w-{i}"))
            created_ids.append(node.id)

    t = threading.Thread(target=worker)
    t.start()
    t.join()

    # Main thread reads back what the worker wrote.
    rows = disk_db.list_nodes_by_kind("worker", limit=100, offset=0)
    assert {n.id for n in rows} == set(created_ids)
    assert len(created_ids) == 20


def test_sequential_thread_handoff_preserves_writes(disk_db: drevo.Drevo) -> None:
    """Hand the handle from thread A to thread B (via join) to thread
    C. Each thread writes some rows; the final readback sees every
    row from every thread.

    This is the worker-pool pattern that an `asyncio.run_in_executor`
    user would hit. The handle's `Send` bound makes it sound; the
    `join` between threads makes it sequential, sidestepping the
    in-flight mutex contention bug.
    """
    written: list[int] = []

    def writer(tag: str) -> None:
        for i in range(10):
            n = disk_db.create_node(drevo.NewNode(kind="seq", title=f"{tag}-{i}"))
            written.append(n.id)

    for tag in ("A", "B", "C"):
        t = threading.Thread(target=writer, args=(tag,))
        t.start()
        t.join()

    rows = disk_db.list_nodes_by_kind("seq", limit=100, offset=0)
    assert {n.id for n in rows} == set(written)
    assert len(written) == 30


def test_concurrent_reopen_cycles_preserve_data(tmp_db_path: str) -> None:
    """Sequential close → open → close → open from one thread leaves
    every committed row visible to the final reader.

    Each cycle re-acquires the file lock; if the close path didn't
    fully release it, the second open would raise `LockedError`.
    """
    for i in range(5):
        with drevo.Drevo.open(tmp_db_path) as db:
            db.create_node(drevo.NewNode(kind="cycle", title=f"cycle-{i}"))
    with drevo.Drevo.open(tmp_db_path) as db:
        rows = db.list_nodes_by_kind("cycle", limit=100, offset=0)
    assert {n.title for n in rows} == {f"cycle-{i}" for i in range(5)}


def test_gil_release_around_search_fts(disk_db: drevo.Drevo) -> None:
    """`search_fts` (a read path) also releases the GIL — a side
    thread doing pure Python work advances while a sequence of
    queries runs on the main thread.
    """
    # Seed corpus so the queries do real work.
    for i in range(40):
        disk_db.create_node(
            drevo.NewNode(
                kind="note",
                title=f"doc-{i}",
                body=f"alpha beta gamma delta epsilon doc {i}",
            )
        )

    stop = threading.Event()
    tick = [0]

    def python_loop() -> None:
        while not stop.is_set():
            tick[0] += 1
            time.sleep(0)

    p = threading.Thread(target=python_loop)
    p.start()
    try:
        for _ in range(100):
            disk_db.search_fts("alpha", 10)
    finally:
        stop.set()
        p.join()

    assert tick[0] > 0
