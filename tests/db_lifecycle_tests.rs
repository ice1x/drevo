//! Integration tests for GraphNoteDb lifecycle: open, open_in_memory, close, compact.

use graphnote_db::db::GraphNoteDb;
use tempfile::TempDir;

// --- open_in_memory ---

#[test]
fn in_memory_db_opens_and_closes() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    db.close().unwrap();
}

#[test]
fn in_memory_db_compact_is_noop() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    db.compact().unwrap();
    db.close().unwrap();
}

// --- open (disk-backed) ---

#[test]
fn disk_db_opens_new_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graphnote.db");
    let db = GraphNoteDb::open(&path).unwrap();
    assert!(path.exists());
    db.close().unwrap();
}

#[test]
fn disk_db_reopens_existing_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graphnote.db");

    {
        let db = GraphNoteDb::open(&path).unwrap();
        db.close().unwrap();
    }

    // Reopen the same file
    let db = GraphNoteDb::open(&path).unwrap();
    db.close().unwrap();
}

#[test]
fn disk_db_counters_persist_through_close_reopen_cycle() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graphnote.db");

    // First session: allocate IDs 1..5 for nodes, 1..3 for edges
    {
        let db = GraphNoteDb::open(&path).unwrap();
        for _ in 0..5 {
            db.alloc_node_id();
        }
        for _ in 0..3 {
            db.alloc_edge_id();
        }
        db.close().unwrap();
    }

    // Second session: counters should continue from 6 and 4
    {
        let db = GraphNoteDb::open(&path).unwrap();
        assert_eq!(db.alloc_node_id(), 6);
        assert_eq!(db.alloc_edge_id(), 4);
        db.close().unwrap();
    }

    // Third session: counters should continue from 7 and 5
    {
        let db = GraphNoteDb::open(&path).unwrap();
        assert_eq!(db.alloc_node_id(), 7);
        assert_eq!(db.alloc_edge_id(), 5);
        db.close().unwrap();
    }
}

#[test]
fn disk_db_compact_flushes_backend() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graphnote.db");
    let db = GraphNoteDb::open(&path).unwrap();
    db.compact().unwrap();
    db.close().unwrap();
}

// --- concurrent ID allocation ---

#[test]
fn concurrent_id_allocation_no_duplicates() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    let db = Arc::new(GraphNoteDb::open_in_memory().unwrap());
    let num_threads = 4;
    let ids_per_thread = 100;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(ids_per_thread);
                for _ in 0..ids_per_thread {
                    ids.push(db.alloc_node_id());
                }
                ids
            })
        })
        .collect();

    let mut all_ids = HashSet::new();
    for handle in handles {
        let ids = handle.join().unwrap();
        for id in ids {
            assert!(all_ids.insert(id), "duplicate node id: {id}");
        }
    }

    assert_eq!(all_ids.len(), num_threads * ids_per_thread);
}

// --- Debug ---

#[test]
fn debug_display_shows_counters() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let _ = db.alloc_node_id(); // now next=2
    let debug = format!("{:?}", db);
    assert!(debug.contains("next_node_id: 2"));
}
