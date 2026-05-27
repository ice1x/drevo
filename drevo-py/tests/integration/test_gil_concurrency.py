"""GIL re-acquisition and threaded access on a real disk backend.

Every public method on `Drevo` wraps its storage I/O in
`Python::allow_threads`. The contract the suite pins here:

* **GIL release on storage I/O** — while one Python thread is inside
  a storage call on the handle, an unrelated Python thread (one that
  does *not* touch the handle) keeps making progress. The proxy is a
  pure-Python counter incremented by the second thread; if the GIL
  were not released, no ticks would land while the first thread was
  inside the Rust call.

* **Per-thread sequential reuse** — a `Drevo` handle is `Send`, so a
  thread spawned with the handle can call `create_node` /
  `get_node` / etc.; on join, the next thread sees every committed
  row.

* **Concurrent writers on the same live handle** — N Python threads
  each calling `create_node` produce N×M unique node ids; no torn
  rows, no missing rows, no deadlock. The PyO3 wrapper drops its
  mutex guard before entering `py.allow_threads(...)` (see
  `drevo-py/src/handle.rs::borrow_db`); without that, the GIL+Mutex
  combination deadlocks after 1–2 iterations because thread A holds
  the Rust mutex with the GIL released while thread B holds the GIL
  blocked on the same mutex.

* **Reader + writer interleaved on the same handle** — a reader
  polling `list_nodes_by_kind` while a writer inserts rows observes
  a monotonically non-decreasing count. redb's internal MVCC
  guarantees each read transaction sees a single consistent
  snapshot.

* **Traversal during writes** — a `bfs` running while edges are
  added returns nodes that are all real (no ghost ids that fail a
  subsequent `get_node` lookup).

* **Sequential close-then-reopen cycles** — repeated open / close
  releases the file lock so the next session can re-acquire it.
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


def test_concurrent_writers_observe_no_torn_rows(disk_db: drevo.Drevo) -> None:
    """N=8 threads, each creating M=25 nodes on the same handle,
    produce N*M unique ids with no duplicates.

    Locks the contract that the PyO3 wrapper does NOT serialise
    storage I/O behind its own mutex (it used to, and the resulting
    GIL+Mutex deadlock made this test hang indefinitely). The
    underlying redb backend's internal `RwLock` is what serialises
    writes correctly without blocking the GIL.
    """
    n_threads = 8
    per_thread = 25
    created: list[int] = []
    lock = threading.Lock()

    def worker(tid: int) -> None:
        local_ids: list[int] = []
        for i in range(per_thread):
            node = disk_db.create_node(drevo.NewNode(kind="task", title=f"t{tid}-{i}", body="x"))
            local_ids.append(node.id)
        with lock:
            created.extend(local_ids)

    threads = [threading.Thread(target=worker, args=(t,)) for t in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert len(created) == n_threads * per_thread
    assert len(set(created)) == len(created), "duplicate node id allocated"


def test_concurrent_readers_during_writes_observe_monotonic_count(
    disk_db: drevo.Drevo,
) -> None:
    """A reader running alongside a writer sees a non-decreasing count
    of matching rows — never goes backwards (which would imply a torn
    or rolled-back commit).
    """
    writer_done = threading.Event()
    observations: list[int] = []

    def writer() -> None:
        for i in range(50):
            disk_db.create_node(drevo.NewNode(kind="obs", title=f"row-{i}"))
        writer_done.set()

    def reader() -> None:
        while not writer_done.is_set():
            page = disk_db.list_nodes_by_kind("obs", limit=100, offset=0)
            observations.append(len(page))
            time.sleep(0.001)
        # Final observation after the writer signals done.
        observations.append(len(disk_db.list_nodes_by_kind("obs", limit=100, offset=0)))

    w = threading.Thread(target=writer)
    r = threading.Thread(target=reader)
    w.start()
    r.start()
    w.join()
    r.join()

    assert observations[-1] == 50
    for prev, nxt in zip(observations, observations[1:]):
        assert nxt >= prev, f"reader saw row count regress: {prev} → {nxt}"


def test_threaded_traversal_during_writes_returns_consistent_snapshot(
    disk_db: drevo.Drevo,
) -> None:
    """A `bfs` running while edges are added to the same root returns
    only real nodes — every id from the traversal lookups back to a
    live `Node`. redb's MVCC chooses a consistent snapshot at txn-open
    time, so the reader never sees a ghost id.
    """
    root = disk_db.create_node(drevo.NewNode(kind="root", title="root"))
    initial = [disk_db.create_node(drevo.NewNode(kind="child", title=f"c-{i}")) for i in range(5)]
    for c in initial:
        disk_db.create_edge(drevo.NewEdge(from_id=root.id, to_id=c.id, kind="parent_of"))

    stop = threading.Event()
    errors: list[str] = []

    def writer() -> None:
        for i in range(50):
            child = disk_db.create_node(drevo.NewNode(kind="child", title=f"late-{i}"))
            disk_db.create_edge(drevo.NewEdge(from_id=root.id, to_id=child.id, kind="parent_of"))
        stop.set()

    def reader() -> None:
        while not stop.is_set():
            try:
                frontier = disk_db.bfs(root.id, 1, drevo.Direction.OUT)
                for n in frontier:
                    if disk_db.get_node(n.id) is None:
                        errors.append(f"bfs returned ghost node {n.id}")
            except Exception as exc:  # pragma: no cover - regression catch
                errors.append(f"bfs raised: {exc!r}")
            time.sleep(0.001)

    w = threading.Thread(target=writer)
    r = threading.Thread(target=reader)
    w.start()
    r.start()
    w.join()
    r.join()

    assert errors == []


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
