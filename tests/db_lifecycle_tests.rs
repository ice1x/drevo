//! Integration tests for Drevo lifecycle: open, open_in_memory, close, compact.

use drevo::db::Drevo;

// --- open_in_memory ---

#[test]
fn in_memory_db_opens_and_closes() {
    let db = Drevo::open_in_memory().unwrap();
    db.close().unwrap();
}

#[test]
fn in_memory_db_compact_is_noop() {
    let mut db = Drevo::open_in_memory().unwrap();
    db.compact().unwrap();
    db.close().unwrap();
}
// --- concurrent ID allocation ---

#[test]
fn concurrent_id_allocation_no_duplicates() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    let db = Arc::new(Drevo::open_in_memory().unwrap());
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
    let db = Drevo::open_in_memory().unwrap();
    let _ = db.alloc_node_id(); // now next=2
    let debug = format!("{:?}", db);
    assert!(debug.contains("next_node_id: 2"));
}
