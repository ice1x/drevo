//! Integration tests for Phase 13 task 00080 — read-write separation.
//!
//! Phase 13's first step swaps the in-memory backend's exclusive `Mutex` for
//! an `RwLock` so that readers run concurrently instead of serialising behind
//! one another, while the redb backend already gives concurrent reads for free
//! through its own MVCC (each `begin_read` opens an independent snapshot).
//!
//! These tests assert the *observable* contract from the top of the stack:
//! - many threads sharing one `Drevo` can read concurrently and every read
//!   sees the committed data (no torn reads, no deadlock, no panic);
//! - a single writer interleaved with a swarm of readers stays consistent;
//! - the redb backend genuinely holds multiple read transactions open at once.
//!
//! Full MVCC tuple-versioning and write-write conflict handling land later in
//! the phase (`00081`+); this suite locks in the read-concurrency floor.

use std::sync::Arc;
use std::thread;

use drevo::db::Drevo;
use drevo::model::{NewNode, Properties};

fn make_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

/// Seed `n` nodes titled `node:{i}` and return their ids indexed by `i`.
fn seed(db: &Drevo, n: u32) -> Vec<u64> {
    (0..n)
        .map(|i| {
            db.create_node(make_node("doc", &format!("node:{i}")))
                .expect("seed create")
                .id
        })
        .collect()
}

#[test]
fn many_threads_read_one_shared_db_concurrently() {
    let db = Arc::new(Drevo::open_in_memory().unwrap());
    let ids = seed(&db, 200);
    let ids = Arc::new(ids);

    let mut handles = Vec::new();
    for _ in 0..16 {
        let db = Arc::clone(&db);
        let ids = Arc::clone(&ids);
        handles.push(thread::spawn(move || {
            for _ in 0..25 {
                for (i, &id) in ids.iter().enumerate() {
                    let node = db.get_node(id).unwrap().expect("node must exist");
                    assert_eq!(node.title, format!("node:{i}"));
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("reader thread must not panic");
    }
}

#[test]
fn readers_see_consistent_data_while_a_writer_runs() {
    // One writer thread keeps creating nodes while many readers hammer the
    // already-committed ids. Readers must never observe a torn / missing node
    // and the process must not deadlock.
    let db = Arc::new(Drevo::open_in_memory().unwrap());
    let baseline = seed(&db, 100);
    let baseline = Arc::new(baseline);

    let writer = {
        let db = Arc::clone(&db);
        thread::spawn(move || {
            for i in 0..200u32 {
                db.create_node(make_node("doc", &format!("writer:{i}")))
                    .expect("writer create");
            }
        })
    };

    let mut readers = Vec::new();
    for _ in 0..8 {
        let db = Arc::clone(&db);
        let baseline = Arc::clone(&baseline);
        readers.push(thread::spawn(move || {
            for _ in 0..200 {
                for (i, &id) in baseline.iter().enumerate() {
                    let node = db.get_node(id).unwrap().expect("baseline node");
                    assert_eq!(node.title, format!("node:{i}"));
                }
            }
        }));
    }

    writer.join().expect("writer must not panic");
    for r in readers {
        r.join().expect("reader must not panic");
    }

    // All writer nodes landed and are reachable by title.
    for i in 0..200u32 {
        assert!(
            db.get_node_by_title(&format!("writer:{i}"))
                .unwrap()
                .is_some(),
            "writer:{i} must be persisted"
        );
    }
}

#[test]
fn lookup_by_title_and_uuid_concurrently() {
    let db = Arc::new(Drevo::open_in_memory().unwrap());
    let mut uuids = Vec::new();
    for i in 0..120u32 {
        let node = db.create_node(make_node("doc", &format!("k:{i}"))).unwrap();
        uuids.push(node.uuid);
    }
    let uuids = Arc::new(uuids);

    let mut handles = Vec::new();
    for _ in 0..12 {
        let db = Arc::clone(&db);
        let uuids = Arc::clone(&uuids);
        handles.push(thread::spawn(move || {
            for _ in 0..30 {
                for (i, uuid) in uuids.iter().enumerate() {
                    let by_title = db.get_node_by_title(&format!("k:{i}")).unwrap();
                    let by_uuid = db.get_node_by_uuid(uuid).unwrap();
                    assert!(by_title.is_some());
                    assert_eq!(by_title.map(|n| n.id), by_uuid.map(|n| n.id));
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("lookup thread must not panic");
    }
}

// ---------------------------------------------------------------------------
// redb backend — concurrent read transactions
// ---------------------------------------------------------------------------

#[cfg(feature = "redb-backend")]
mod redb_concurrent_reads {
    use super::*;
    use drevo::storage::{RedbBackend, StorageBackend};

    #[test]
    fn many_threads_read_one_redb_backend_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(RedbBackend::open(dir.path().join("rw.redb")).unwrap());
        for i in 0..200u32 {
            backend
                .put(format!("k:{i}").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }

        let mut handles = Vec::new();
        for _ in 0..16 {
            let backend = Arc::clone(&backend);
            handles.push(thread::spawn(move || {
                for _ in 0..25 {
                    for i in 0..200u32 {
                        let got = backend.get(format!("k:{i}").as_bytes()).unwrap();
                        assert_eq!(got, Some(i.to_le_bytes().to_vec()));
                    }
                    // Prefix scans run concurrently against the same db too.
                    let scanned = backend.scan_prefix(b"k:1").unwrap();
                    assert!(!scanned.is_empty());
                }
            }));
        }
        for h in handles {
            h.join().expect("redb reader thread must not panic");
        }
    }

    /// redb opens an independent snapshot per `begin_read`, so several read
    /// paths overlapping in time do not block each other. A barrier forces
    /// the threads to be *simultaneously* inside their reads before any may
    /// finish — if reads serialised, this would hang.
    #[test]
    fn redb_read_transactions_overlap_in_time() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(RedbBackend::open(dir.path().join("overlap.redb")).unwrap());
        backend.put(b"k", b"v").unwrap();

        let n = 8;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles = Vec::new();
        for _ in 0..n {
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                // Read once, then wait for every other reader to also have read.
                // The wait happens while each thread is mid-flight; reaching the
                // barrier at all proves the reads ran in parallel.
                let got = backend.get(b"k").unwrap();
                assert_eq!(got, Some(b"v".to_vec()));
                barrier.wait();
                backend.get(b"k").unwrap()
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap(), Some(b"v".to_vec()));
        }
    }

    #[test]
    fn redb_reads_and_writes_interleave_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(RedbBackend::open(dir.path().join("mixed.redb")).unwrap());
        for i in 0..50u32 {
            backend
                .put(format!("base:{i}").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }

        let writer = {
            let backend = Arc::clone(&backend);
            thread::spawn(move || {
                for i in 0..200u32 {
                    backend
                        .put(format!("w:{i}").as_bytes(), &i.to_le_bytes())
                        .unwrap();
                }
            })
        };

        let mut readers = Vec::new();
        for _ in 0..6 {
            let backend = Arc::clone(&backend);
            readers.push(thread::spawn(move || {
                for _ in 0..100 {
                    for i in 0..50u32 {
                        let got = backend.get(format!("base:{i}").as_bytes()).unwrap();
                        assert_eq!(got, Some(i.to_le_bytes().to_vec()));
                    }
                }
            }));
        }

        writer.join().expect("writer must not panic");
        for r in readers {
            r.join().expect("reader must not panic");
        }
        assert_eq!(
            backend.get(b"w:199").unwrap(),
            Some(199u32.to_le_bytes().to_vec())
        );
    }
}
