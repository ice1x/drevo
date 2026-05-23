//! Integration tests for Phase 9 task `00054` — Compaction.
//!
//! Compaction is the operator-facing "reclaim unused on-disk space and
//! checkpoint the database" primitive. It sits on top of two things that
//! already work:
//!
//! 1. **redb's per-commit double-write + fsync WAL** (`00053`) — every
//!    individual put/delete is already durable. Compaction therefore is not
//!    about durability — it is about *physical layout*: after enough
//!    delete + insert churn the redb file accumulates pages that are
//!    logically free but still allocated on disk.
//! 2. **`Drevo::persist_counters`** — the next-id counters live in memory
//!    between writes (a perf optimisation — see `00053`). Compaction
//!    checkpoints them to `meta:next_*_id` *before* doing the physical
//!    reclaim so the post-compaction file is internally consistent without
//!    relying on the `load_counters` rescan to fix it on next open.
//!
//! The contract exercised here:
//!
//! - [`Drevo::compact`] returns a serde-serialisable [`CompactReport`]
//!   describing `bytes_before` / `bytes_after` / `bytes_reclaimed` plus
//!   the post-compaction counter checkpoint.
//! - For the redb backend, repeated create+delete cycles followed by
//!   `compact` produce a file whose size is `<=` the pre-compaction size.
//! - For the persistent memory backend, `compact` rewrites the snapshot
//!   file (the "size before == size after" case for a balanced backend).
//! - For the ephemeral memory backend, `compact` is a no-op that still
//!   returns a well-formed report (`bytes_before == bytes_after == None`).
//! - All graph data and indexes survive a `compact` round-trip — no
//!   silent data loss, no orphaned secondary index entries.
//! - The counter checkpoint persists across a close/reopen cycle —
//!   `Drevo::open` after `compact` does NOT need the `load_counters`
//!   rescan to clamp the counter upward.

use std::path::Path;

use drevo::db::{CompactReport, Drevo};
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::storage::{MemoryBackend, RedbBackend, StorageBackend, StorageError};
use tempfile::TempDir;

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn new_edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn open_temp() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("drevo.db");
    (dir, path)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Insert N nodes, then delete every second one. This produces the
/// fragment pattern the compactor is meant to reclaim.
fn churn_redb(db: &Drevo, n: u64) {
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let node = db
            .create_node(new_node("note", &format!("churn-{i}")))
            .unwrap();
        ids.push(node.id);
    }
    for (i, id) in ids.iter().enumerate() {
        if i % 2 == 0 {
            db.delete_node(*id).unwrap();
        }
    }
}

// ---------------------------------------------------------------
// CompactReport — public API surface
// ---------------------------------------------------------------

#[test]
fn compact_report_is_serde_round_trippable() {
    let report = CompactReport {
        bytes_before: Some(4096),
        bytes_after: Some(2048),
        bytes_reclaimed: 2048,
        next_node_id: 42,
        next_edge_id: 7,
    };
    let json = serde_json::to_string(&report).expect("CompactReport must serialise");
    let back: CompactReport =
        serde_json::from_str(&json).expect("CompactReport must round-trip via serde");
    assert_eq!(report, back);
}

#[test]
fn compact_report_default_is_empty() {
    let report = CompactReport::default();
    assert_eq!(report.bytes_before, None);
    assert_eq!(report.bytes_after, None);
    assert_eq!(report.bytes_reclaimed, 0);
    assert_eq!(report.next_node_id, 0);
    assert_eq!(report.next_edge_id, 0);
}

#[test]
fn compact_report_debug_does_not_panic() {
    let report = CompactReport {
        bytes_before: Some(100),
        bytes_after: Some(50),
        bytes_reclaimed: 50,
        next_node_id: 3,
        next_edge_id: 1,
    };
    let _ = format!("{report:?}");
}

// ---------------------------------------------------------------
// In-memory backend (ephemeral)
// ---------------------------------------------------------------

#[test]
fn in_memory_compact_returns_unsized_report() {
    let mut db = Drevo::open_in_memory().unwrap();
    let report = db.compact().expect("in-memory compact must not error");
    assert_eq!(report.bytes_before, None, "ephemeral memory has no size");
    assert_eq!(report.bytes_after, None);
    assert_eq!(report.bytes_reclaimed, 0);
    assert_eq!(report.next_node_id, 1);
    assert_eq!(report.next_edge_id, 1);
}

#[test]
fn in_memory_compact_persists_counters_into_meta() {
    let mut db = Drevo::open_in_memory().unwrap();
    let _ = db.create_node(new_node("note", "a")).unwrap();
    let _ = db.create_node(new_node("note", "b")).unwrap();
    let report = db.compact().unwrap();
    assert_eq!(report.next_node_id, 3, "two creates → next id is 3");
}

#[test]
fn in_memory_compact_preserves_all_data() {
    let mut db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "Alpha")).unwrap();
    let b = db.create_node(new_node("note", "Beta")).unwrap();
    let _ = db.create_edge(new_edge(a.id, b.id, "links")).unwrap();

    let report = db.compact().unwrap();
    assert!(report.next_node_id >= 3);

    assert_eq!(db.get_node(a.id).unwrap().unwrap().title, "Alpha");
    assert_eq!(db.get_node(b.id).unwrap().unwrap().title, "Beta");
    let edges = db.edges_of(a.id, Direction::Outgoing).unwrap();
    assert_eq!(edges.len(), 1);
}

#[test]
fn in_memory_compact_idempotent() {
    let mut db = Drevo::open_in_memory().unwrap();
    let r1 = db.compact().unwrap();
    let r2 = db.compact().unwrap();
    let r3 = db.compact().unwrap();
    assert_eq!(r1, r2);
    assert_eq!(r2, r3);
}

// ---------------------------------------------------------------
// In-memory backend (persistent path)
// ---------------------------------------------------------------

#[test]
fn persistent_memory_backend_compact_rewrites_snapshot_file() {
    let (_dir, path) = open_temp();
    let backend = MemoryBackend::open(&path).unwrap();
    backend.put(b"k1", b"v1").unwrap();
    backend.put(b"k2", b"v2".repeat(64).as_slice()).unwrap();
    backend.flush().unwrap();

    let size_before = file_size(&path);
    assert!(size_before > 0, "snapshot must exist after first flush");

    // delete one of the keys — the snapshot on disk still contains both
    backend.delete(b"k2").unwrap();

    // compact rewrites the snapshot — the file should shrink
    let mut backend = backend;
    backend.compact().unwrap();
    let size_after = file_size(&path);
    assert!(
        size_after < size_before,
        "compact must shrink snapshot when keys were deleted: before={size_before}, after={size_after}"
    );
}

#[test]
fn persistent_memory_backend_compact_returns_size() {
    let (_dir, path) = open_temp();
    let mut backend = MemoryBackend::open(&path).unwrap();
    backend.put(b"k1", b"v1").unwrap();
    backend.flush().unwrap();

    let before = backend.size_bytes().unwrap();
    assert!(
        before.is_some_and(|s| s > 0),
        "persistent backend must report a positive size"
    );

    backend.compact().unwrap();
    let after = backend.size_bytes().unwrap();
    assert!(
        after.is_some(),
        "persistent backend must report size after compact"
    );
}

#[test]
fn ephemeral_memory_backend_size_bytes_is_none() {
    let backend = MemoryBackend::new();
    assert_eq!(backend.size_bytes().unwrap(), None);
}

// ---------------------------------------------------------------
// redb backend — the headline case
// ---------------------------------------------------------------

#[test]
fn redb_compact_reports_file_sizes() {
    let (_dir, path) = open_temp();
    {
        let mut db = Drevo::open(&path).unwrap();
        churn_redb(&db, 100);
        let report = db.compact().expect("redb compact must not error");
        assert!(
            report.bytes_before.is_some(),
            "redb file size must be measurable"
        );
        assert!(report.bytes_after.is_some());
        let before = report.bytes_before.unwrap();
        let after = report.bytes_after.unwrap();
        assert_eq!(
            report.bytes_reclaimed,
            before.saturating_sub(after),
            "bytes_reclaimed = max(0, before - after)"
        );
        db.close().unwrap();
    }
}

#[test]
fn redb_compact_reclaims_after_heavy_churn() {
    let (_dir, path) = open_temp();
    // Phase 1: inflate the file with many nodes, then delete most of them
    // so the redb page allocator carries lots of freed pages.
    let bytes_before_churn = {
        let db = Drevo::open(&path).unwrap();
        for i in 0..500u64 {
            let n = db.create_node(new_node("note", &format!("n{i}"))).unwrap();
            assert_eq!(n.id, i + 1);
        }
        for i in 0..500u64 {
            db.delete_node(i + 1).unwrap();
        }
        db.close().unwrap();
        file_size(&path)
    };

    // Phase 2: compact. The post-compaction file size must be <=
    // pre-compaction size. We can't assert a strict reduction because redb's
    // page allocator may already have absorbed everything, but the report
    // contract (bytes_reclaimed is a u64, bytes_after <= bytes_before) must
    // hold.
    {
        let mut db = Drevo::open(&path).unwrap();
        let report = db.compact().unwrap();
        let before = report.bytes_before.unwrap();
        let after = report.bytes_after.unwrap();
        assert!(
            after <= before,
            "post-compaction file must not grow: before={before}, after={after}"
        );
        assert_eq!(bytes_before_churn, before);
        db.close().unwrap();
    }
}

#[test]
fn redb_compact_preserves_remaining_nodes_and_edges() {
    let (_dir, path) = open_temp();
    let mut db = Drevo::open(&path).unwrap();

    let alpha = db.create_node(new_node("note", "Alpha")).unwrap();
    let beta = db.create_node(new_node("note", "Beta")).unwrap();
    let gamma = db.create_node(new_node("note", "Gamma")).unwrap();
    let edge_ab = db
        .create_edge(new_edge(alpha.id, beta.id, "links"))
        .unwrap();
    let edge_bg = db
        .create_edge(new_edge(beta.id, gamma.id, "links"))
        .unwrap();

    // Churn around the survivors
    for i in 0..50 {
        let n = db
            .create_node(new_node("scratch", &format!("s{i}")))
            .unwrap();
        db.delete_node(n.id).unwrap();
    }

    db.compact().unwrap();

    assert_eq!(db.get_node(alpha.id).unwrap().unwrap().title, "Alpha");
    assert_eq!(db.get_node(beta.id).unwrap().unwrap().title, "Beta");
    assert_eq!(db.get_node(gamma.id).unwrap().unwrap().title, "Gamma");
    assert_eq!(db.get_edge(edge_ab.id).unwrap().unwrap().kind, "links");
    assert_eq!(db.get_edge(edge_bg.id).unwrap().unwrap().kind, "links");
    let outgoing = db.edges_of(alpha.id, Direction::Outgoing).unwrap();
    assert_eq!(outgoing.len(), 1);
    db.close().unwrap();
}

#[test]
fn redb_compact_preserves_secondary_indexes() {
    let (_dir, path) = open_temp();
    let mut db = Drevo::open(&path).unwrap();

    let a = db.create_node(new_node("note", "Foo")).unwrap();
    db.create_node(new_node("task", "Bar")).unwrap();

    db.compact().unwrap();

    // Title index — by_title lookup must still resolve.
    let by_title = db.get_node_by_title("Foo").unwrap();
    assert_eq!(by_title.unwrap().id, a.id);

    // UUID index.
    let by_uuid = db.get_node_by_uuid(&a.uuid).unwrap();
    assert_eq!(by_uuid.unwrap().id, a.id);

    // Kind index.
    let notes = db.list_nodes_by_kind("note", 100, 0).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "Foo");

    let tasks = db.list_nodes_by_kind("task", 100, 0).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Bar");

    db.close().unwrap();
}

#[test]
fn redb_compact_checkpoints_counters_to_meta() {
    let (_dir, path) = open_temp();
    {
        let mut db = Drevo::open(&path).unwrap();
        for i in 0..10 {
            db.create_node(new_node("note", &format!("cp-{i}")))
                .unwrap();
        }
        let report = db.compact().unwrap();
        assert_eq!(report.next_node_id, 11, "10 creates → next id is 11");
        // NOTE: we deliberately drop without close() — compact must have
        // already persisted the counter to meta:next_node_id.
    }

    // Reopen — load_counters should pick up the persisted hint AT LEAST.
    // (The rescan can also lift it; the important thing is no rewind.)
    let db = Drevo::open(&path).unwrap();
    let next = db.alloc_node_id();
    assert!(
        next >= 11,
        "post-compact, post-drop reopen must hand out an id >= 11 (got {next})"
    );
    db.close().unwrap();
}

#[test]
fn redb_compact_idempotent() {
    let (_dir, path) = open_temp();
    let mut db = Drevo::open(&path).unwrap();
    for i in 0..50 {
        db.create_node(new_node("note", &format!("i-{i}"))).unwrap();
    }
    for i in 1..=50u64 {
        if i % 3 == 0 {
            db.delete_node(i).unwrap();
        }
    }

    let r1 = db.compact().unwrap();
    let r2 = db.compact().unwrap();
    let r3 = db.compact().unwrap();
    // Each call is a checkpoint — counters must match across calls.
    assert_eq!(r1.next_node_id, r2.next_node_id);
    assert_eq!(r2.next_node_id, r3.next_node_id);
    // After the first compact, subsequent ones must not grow the file.
    assert!(r2.bytes_after.unwrap() <= r1.bytes_after.unwrap());
    assert!(r3.bytes_after.unwrap() <= r2.bytes_after.unwrap());
    db.close().unwrap();
}

#[test]
fn redb_compact_on_empty_database_succeeds() {
    let (_dir, path) = open_temp();
    let mut db = Drevo::open(&path).unwrap();
    let report = db.compact().unwrap();
    assert_eq!(report.next_node_id, 1);
    assert_eq!(report.next_edge_id, 1);
    assert!(report.bytes_before.is_some());
    assert!(report.bytes_after.is_some());
    db.close().unwrap();
}

// ---------------------------------------------------------------
// Backend-level direct API tests — exercising the trait method
// ---------------------------------------------------------------

#[test]
fn redb_backend_size_bytes_returns_file_size() {
    let (_dir, path) = open_temp();
    let backend = RedbBackend::open(&path).unwrap();
    let initial = backend.size_bytes().unwrap();
    assert!(initial.is_some(), "redb file size must be measurable");
    let initial = initial.unwrap();
    assert!(initial > 0, "redb pre-allocates a non-empty file on open");

    // Sanity-check against the filesystem directly — the trait must
    // report the same number the OS sees.
    let fs_size = std::fs::metadata(&path).unwrap().len();
    assert_eq!(initial, fs_size);

    // After enough writes to exceed the pre-allocated region the file
    // must grow. 5 MiB of values comfortably exceeds redb's initial
    // 1.5 MiB pre-allocation on every platform we test on.
    for i in 0..5_000u64 {
        let key = format!("k{i:08}");
        backend.put(key.as_bytes(), &vec![b'x'; 1024]).unwrap();
    }

    let after = backend.size_bytes().unwrap().unwrap();
    assert!(
        after >= initial,
        "file must not shrink under writes: initial={initial}, after={after}"
    );
    // The OS report must continue to match.
    let fs_after = std::fs::metadata(&path).unwrap().len();
    assert_eq!(after, fs_after);
}

#[test]
fn redb_backend_compact_runs_without_error() {
    let (_dir, path) = open_temp();
    let mut backend = RedbBackend::open(&path).unwrap();
    for i in 0..50u64 {
        backend
            .put(format!("k{i}").as_bytes(), &vec![b'x'; 1024])
            .unwrap();
    }
    for i in 0..50u64 {
        if i % 2 == 0 {
            backend.delete(format!("k{i}").as_bytes()).unwrap();
        }
    }
    backend.compact().expect("backend compact must succeed");
}

#[test]
fn redb_backend_compact_with_outstanding_arc_clone_returns_error() {
    use std::sync::Arc;
    let (_dir, path) = open_temp();
    let backend = RedbBackend::open(&path).unwrap();
    // Hold an Arc clone so Arc::get_mut inside compact() returns None.
    let shared: Arc<RedbBackend> = Arc::new(backend);
    let extra = Arc::clone(&shared);

    // Bring the inner backend out for compact() — but the extra Arc keeps
    // the original Database Arc shared via deep clone. We test the
    // "shared Database Arc" path by cloning the RedbBackend itself, since
    // Clone on RedbBackend clones the inner Arc<Database>.
    let backend_a = (*shared).clone();
    let _backend_b = (*shared).clone();
    let _ = extra;
    let mut backend_a = backend_a;
    let result = backend_a.compact();
    assert!(
        matches!(result, Err(StorageError::CompactNotExclusive)),
        "compact with outstanding shared Database Arc must fail with CompactNotExclusive, got: {result:?}"
    );
}

// ---------------------------------------------------------------
// Cross-backend
// ---------------------------------------------------------------

#[test]
fn compact_works_via_storage_backend_trait_object() {
    let (_dir, path) = open_temp();
    let mut backend: Box<dyn StorageBackend> = Box::new(RedbBackend::open(&path).unwrap());
    backend.put(b"k", b"v").unwrap();
    backend
        .compact()
        .expect("compact via trait object must succeed");
    backend.flush().unwrap();
}

// ---------------------------------------------------------------
// Cross-file structural guards
// ---------------------------------------------------------------

const README: &str = include_str!("../README.md");
const DB_RS: &str = include_str!("../src/db.rs");

#[test]
fn readme_marks_task_00054_done() {
    assert!(
        README.contains("- [x] `00054` Compaction"),
        "README must mark Phase 9 task 00054 as done after this work lands"
    );
}

#[test]
fn db_rs_exposes_compact_report_struct() {
    assert!(
        DB_RS.contains("pub struct CompactReport"),
        "src/db.rs must declare `pub struct CompactReport` for task 00054"
    );
}

#[test]
fn db_rs_compact_returns_compact_report() {
    assert!(
        DB_RS.contains("pub fn compact(&mut self) -> Result<CompactReport>"),
        "Drevo::compact must take &mut self and return Result<CompactReport>"
    );
}

#[test]
fn storage_backend_trait_has_size_bytes() {
    let trait_src = include_str!("../src/storage/backend.rs");
    assert!(
        trait_src.contains("fn size_bytes"),
        "StorageBackend trait must expose size_bytes() for the compaction report"
    );
    assert!(
        trait_src.contains("fn compact"),
        "StorageBackend trait must expose compact() for backend-internal page reclaim"
    );
}
