//! Integration tests for Phase 9 task `00053` — WAL / crash recovery.
//!
//! drevo persists every redb transaction durably (each `RedbBackend::put` is
//! its own committed transaction with double-write+fsync inside redb's
//! storage format — that *is* the WAL). What was historically missing was a
//! documented **crash recovery model**:
//!
//! 1. ID-counter durability — `Drevo::create_node` / `create_edge` only
//!    persisted the next-id counters via `Drevo::close()`. A process kill
//!    between writes therefore rewound the counter on next open and the
//!    very next `create_node` collided with an already-stored id. Task
//!    00053 makes `Drevo::open` re-derive the counter from
//!    `max(stored_id) + 1`, so the persisted counter becomes a *hint* and
//!    the on-disk node rows become the source of truth.
//! 2. Integrity inspection — `Drevo::check_integrity` returns an
//!    [`IntegrityReport`] enumerating any structural issues (orphan index
//!    entries, dangling edge endpoints, counter drift, corrupt rows).
//! 3. Explicit recovery entry point — `Drevo::recover` opens the
//!    database, runs `check_integrity`, and returns both the handle and
//!    the report so operators can react to surprises after a known-bad
//!    crash.
//!
//! Tests are grouped:
//!
//! - **Crash-simulation tests** (drop without `close`) — verify durability
//!   of committed data and the absence of id collisions after a rewind.
//! - **Counter recovery tests** — exercise the `load_counters` rescan path.
//! - **`check_integrity` tests** — happy path on empty/healthy DB, plus
//!   synthetic orphan/dangling injections via the raw `RedbBackend`.
//! - **`recover` tests** — opens-and-reports surface.
//! - **Cross-file structural tests** — README marks 00053 done, module docs
//!   describe the WAL crash-recovery model.

use std::path::Path;

use drevo::db::{Drevo, IntegrityReport};
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::storage::{RedbBackend, StorageBackend};
use tempfile::TempDir;

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
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

/// Open the raw `RedbBackend` against the same file the Drevo handle uses,
/// for white-box injection of synthetic corruption between two Drevo
/// lifetimes. The Drevo handle MUST be dropped before this is called so the
/// redb file lock is released.
fn raw(path: &Path) -> RedbBackend {
    RedbBackend::open(path).unwrap()
}

const PREFIX_NODE: &[u8] = b"node:";
const PREFIX_NODE_KIND: &[u8] = b"node_kind:";
const PREFIX_NODE_TITLE: &[u8] = b"node_title:";
const PREFIX_NODE_UUID: &[u8] = b"node_uuid:";
const PREFIX_OUT: &[u8] = b"out:";
const PREFIX_IN: &[u8] = b"in:";
const META_NEXT_NODE_ID: &[u8] = b"meta:next_node_id";
const META_NEXT_EDGE_ID: &[u8] = b"meta:next_edge_id";

fn node_kind_key(kind: &str, id: u64) -> Vec<u8> {
    let mut key = PREFIX_NODE_KIND.to_vec();
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&id.to_le_bytes());
    key
}

fn node_title_key(title: &str) -> Vec<u8> {
    let mut key = PREFIX_NODE_TITLE.to_vec();
    key.extend_from_slice(title.as_bytes());
    key
}

fn node_uuid_key(uuid: &[u8; 16]) -> Vec<u8> {
    let mut key = PREFIX_NODE_UUID.to_vec();
    key.extend_from_slice(uuid);
    key
}

// v2 kind-in-key adjacency layout (#243 slice 2): `{prefix}{node_8}:{kind}:
// {edge_8}`. Mirrors `src/db.rs`. A v1 key here would trip the open-time
// migration gate, so these injected orphan entries use the current layout.
fn out_edge_key(from_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    adjacency_key(PREFIX_OUT, from_id, kind, edge_id)
}

fn in_edge_key(to_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    adjacency_key(PREFIX_IN, to_id, kind, edge_id)
}

fn adjacency_key(prefix: &[u8], node_id: u64, kind: &str, edge_id: u64) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key.push(b':');
    key.extend_from_slice(kind.as_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

// ---------------------------------------------------------------
// Crash simulation: drop without close (== process kill for redb)
// ---------------------------------------------------------------

#[test]
fn committed_nodes_survive_drop_without_close() {
    let (_dir, path) = open_temp();
    let titles = ["a", "b", "c", "d", "e"];

    {
        let db = Drevo::open(&path).unwrap();
        for t in &titles {
            db.create_node(new_node("note", t)).unwrap();
        }
        // Intentionally NO close() — simulates a process kill mid-life.
    }

    let db = Drevo::open(&path).unwrap();
    for t in &titles {
        assert!(
            db.get_node_by_title(t).unwrap().is_some(),
            "node {t} lost after simulated crash"
        );
    }
}

#[test]
fn committed_edges_survive_drop_without_close() {
    let (_dir, path) = open_temp();

    let (a_id, b_id) = {
        let db = Drevo::open(&path).unwrap();
        let a = db.create_node(new_node("note", "a")).unwrap();
        let b = db.create_node(new_node("note", "b")).unwrap();
        db.create_edge(new_edge(a.id, b.id, "links_to")).unwrap();
        // No close.
        (a.id, b.id)
    };

    let db = Drevo::open(&path).unwrap();
    let neighbors = db
        .neighbors(a_id, drevo::model::Direction::Outgoing, None)
        .unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].id, b_id);
}

#[test]
fn drop_without_close_does_not_cause_node_id_collision() {
    // The headline 00053 fix: prior to this task, dropping without close()
    // left `meta:next_node_id` at 1 (its default), and the next `Drevo::open`
    // would re-allocate id=1 — colliding with the node already at id=1.
    let (_dir, path) = open_temp();

    {
        let db = Drevo::open(&path).unwrap();
        for i in 0..5 {
            db.create_node(new_node("note", &format!("first-{i}")))
                .unwrap();
        }
        // No close — counter persistence is intentionally skipped.
    }

    let db = Drevo::open(&path).unwrap();
    let new_id = db.alloc_node_id();
    assert!(
        new_id >= 6,
        "expected next node id ≥ 6 after 5 prior creates, got {new_id} (counter rewound — crash recovery broken)"
    );

    // And a real create succeeds without DuplicateTitle / id-collision panic.
    let n = db.create_node(new_node("note", "post-crash")).unwrap();
    assert!(n.id >= 6, "post-crash node id rewound to {}", n.id);
    assert!(db.get_node_by_title("first-0").unwrap().is_some());
    assert!(db.get_node_by_title("post-crash").unwrap().is_some());
}

#[test]
fn drop_without_close_does_not_cause_edge_id_collision() {
    let (_dir, path) = open_temp();

    let (a, b) = {
        let db = Drevo::open(&path).unwrap();
        let a = db.create_node(new_node("note", "a")).unwrap();
        let b = db.create_node(new_node("note", "b")).unwrap();
        for _ in 0..3 {
            db.create_edge(new_edge(a.id, b.id, "links_to")).unwrap();
        }
        (a.id, b.id)
    };

    let db = Drevo::open(&path).unwrap();
    let new_edge_id = db.alloc_edge_id();
    assert!(
        new_edge_id >= 4,
        "expected next edge id ≥ 4 after 3 prior edges, got {new_edge_id}"
    );

    // Real create succeeds without colliding on adjacency key.
    let e = db.create_edge(new_edge(a, b, "links_to")).unwrap();
    assert!(e.id >= 4, "post-crash edge id rewound to {}", e.id);
}

#[test]
fn many_simulated_crashes_keep_ids_monotonic() {
    // Across multiple kill/reopen cycles the allocator must never reuse an
    // already-stored id.
    let (_dir, path) = open_temp();
    let mut all_ids: Vec<u64> = Vec::new();

    for round in 0..4 {
        let db = Drevo::open(&path).unwrap();
        for i in 0..3 {
            let n = db
                .create_node(new_node("note", &format!("r{round}-n{i}")))
                .unwrap();
            assert!(
                !all_ids.contains(&n.id),
                "node id {} reused after crash (round {round})",
                n.id
            );
            all_ids.push(n.id);
        }
        // No close.
    }

    assert_eq!(all_ids.len(), 12);
}

// ---------------------------------------------------------------
// Counter recovery directly via load_counters semantics
// ---------------------------------------------------------------

#[test]
fn open_recovers_counter_above_max_persisted_node_id() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        for i in 0..7 {
            db.create_node(new_node("note", &format!("n{i}"))).unwrap();
        }
    }

    // Force the persisted counter back to 1 (synthetic counter drift),
    // mimicking the pre-fix crash-window state.
    {
        let b = raw(&path);
        b.put(META_NEXT_NODE_ID, &1u64.to_le_bytes()).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    // With the recovery scan, the counter must be re-derived from on-disk
    // node ids (max id was 7, so next id ≥ 8).
    assert!(
        db.alloc_node_id() >= 8,
        "load_counters did not rescan max node id"
    );
}

#[test]
fn open_recovers_counter_above_max_persisted_edge_id() {
    let (_dir, path) = open_temp();
    let (a, b) = {
        let db = Drevo::open(&path).unwrap();
        let a = db.create_node(new_node("note", "a")).unwrap();
        let b = db.create_node(new_node("note", "b")).unwrap();
        for _ in 0..4 {
            db.create_edge(new_edge(a.id, b.id, "links_to")).unwrap();
        }
        (a.id, b.id)
    };

    {
        let bk = raw(&path);
        bk.put(META_NEXT_EDGE_ID, &1u64.to_le_bytes()).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    assert!(
        db.alloc_edge_id() >= 5,
        "load_counters did not rescan max edge id"
    );
    let _ = (a, b);
}

#[test]
fn open_with_clean_close_preserves_consistent_counter() {
    // Sanity: when close() runs cleanly, the next-id counter matches what
    // a max-scan would also produce. No regression for the close-friendly
    // path.
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "x")).unwrap();
        db.create_node(new_node("note", "y")).unwrap();
        db.close().unwrap();
    }
    let db = Drevo::open(&path).unwrap();
    assert_eq!(db.alloc_node_id(), 3);
}

#[test]
fn open_on_empty_db_starts_counter_at_one() {
    let (_dir, path) = open_temp();
    let db = Drevo::open(&path).unwrap();
    assert_eq!(db.alloc_node_id(), 1);
    assert_eq!(db.alloc_edge_id(), 1);
}

// ---------------------------------------------------------------
// IntegrityReport — happy paths
// ---------------------------------------------------------------

#[test]
fn check_integrity_on_empty_db_is_clean() {
    let (_dir, path) = open_temp();
    let db = Drevo::open(&path).unwrap();
    let report: IntegrityReport = db.check_integrity().unwrap();
    assert!(
        report.is_clean(),
        "expected clean report on empty db, got {:?}",
        report
    );
    assert_eq!(report.node_count, 0);
    assert_eq!(report.edge_count, 0);
}

#[test]
fn check_integrity_on_healthy_db_is_clean() {
    let (_dir, path) = open_temp();
    let db = Drevo::open(&path).unwrap();
    let a = db.create_node(new_node("note", "a")).unwrap();
    let b = db.create_node(new_node("note", "b")).unwrap();
    let c = db.create_node(new_node("note", "c")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "links_to")).unwrap();
    db.create_edge(new_edge(b.id, c.id, "links_to")).unwrap();

    let report = db.check_integrity().unwrap();
    assert!(report.is_clean(), "expected clean report, got {:?}", report);
    assert_eq!(report.node_count, 3);
    assert_eq!(report.edge_count, 2);
}

#[test]
fn check_integrity_on_in_memory_db_works() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(new_node("note", "x")).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(report.is_clean());
    assert_eq!(report.node_count, 1);
}

// ---------------------------------------------------------------
// IntegrityReport — synthetic corruption detection
// ---------------------------------------------------------------

#[test]
fn check_integrity_detects_orphan_node_kind_index_entry() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "real")).unwrap();
        db.close().unwrap();
    }
    {
        // Inject orphan: kind index entry for id 9999 that has no node row.
        let b = raw(&path);
        b.put(&node_kind_key("note", 9999), &[]).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(
        !report.is_clean(),
        "expected dirty report after orphan injection"
    );
    assert!(
        report.orphan_node_kind_entries >= 1,
        "expected orphan_node_kind_entries ≥ 1, got {}",
        report.orphan_node_kind_entries
    );
}

#[test]
fn check_integrity_detects_orphan_node_title_index_entry() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "real")).unwrap();
        db.close().unwrap();
    }
    {
        let b = raw(&path);
        b.put(&node_title_key("ghost-title"), &9999u64.to_le_bytes())
            .unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(report.orphan_node_title_entries >= 1);
}

#[test]
fn check_integrity_detects_orphan_node_uuid_index_entry() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "real")).unwrap();
        db.close().unwrap();
    }
    {
        let b = raw(&path);
        let ghost_uuid = [0xAAu8; 16];
        b.put(&node_uuid_key(&ghost_uuid), &9999u64.to_le_bytes())
            .unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(report.orphan_node_uuid_entries >= 1);
}

#[test]
fn check_integrity_detects_dangling_edge_source() {
    let (_dir, path) = open_temp();
    let edge_id;
    {
        let db = Drevo::open(&path).unwrap();
        let a = db.create_node(new_node("note", "a")).unwrap();
        let b = db.create_node(new_node("note", "b")).unwrap();
        let e = db.create_edge(new_edge(a.id, b.id, "links_to")).unwrap();
        edge_id = e.id;
        // Manually delete node `a` row but leave the edge in place —
        // hard to reach via the public API since delete_node cascades,
        // so we do the surgery raw.
        db.close().unwrap();
    }
    {
        let bk = raw(&path);
        // Delete the row for node id 1 (the source) but keep the edge.
        let mut nk = PREFIX_NODE.to_vec();
        nk.extend_from_slice(&1u64.to_le_bytes());
        bk.delete(&nk).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(
        report.dangling_edge_endpoints >= 1,
        "expected dangling_edge_endpoints ≥ 1 (edge {edge_id} now references missing source), got {}",
        report.dangling_edge_endpoints
    );
}

#[test]
fn check_integrity_detects_orphan_out_adjacency_entry() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "a")).unwrap();
        db.close().unwrap();
    }
    {
        let b = raw(&path);
        // Out-adjacency entry pointing to an edge id that doesn't exist.
        b.put(&out_edge_key(1, "knows", 7777), &[]).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(report.orphan_adjacency_entries >= 1);
}

#[test]
fn check_integrity_detects_orphan_in_adjacency_entry() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "a")).unwrap();
        db.close().unwrap();
    }
    {
        let b = raw(&path);
        b.put(&in_edge_key(1, "knows", 8888), &[]).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(report.orphan_adjacency_entries >= 1);
}

#[test]
fn check_integrity_detects_counter_drift() {
    // After the rescan in load_counters, counter drift is auto-repaired in
    // memory — but the persisted counter on disk may still be stale. We
    // surface that as a *warning* class so an operator running
    // `check_integrity` after a hard crash can see what was repaired.
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        for i in 0..3 {
            db.create_node(new_node("note", &format!("n{i}"))).unwrap();
        }
    }
    {
        // Rewind the persisted counter, mimicking a crash that lost the
        // close()-time persist_counters call.
        let b = raw(&path);
        b.put(META_NEXT_NODE_ID, &1u64.to_le_bytes()).unwrap();
    }

    let db = Drevo::open(&path).unwrap();
    let report = db.check_integrity().unwrap();
    assert!(
        report.counter_drift_repaired,
        "expected counter_drift_repaired=true after persisted-counter rewind, got {:?}",
        report
    );
}

// ---------------------------------------------------------------
// recover() entry point
// ---------------------------------------------------------------

#[test]
fn recover_opens_db_and_returns_report() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(new_node("note", "x")).unwrap();
        db.close().unwrap();
    }

    let (db, report) = Drevo::recover(&path).unwrap();
    assert!(report.is_clean());
    assert_eq!(report.node_count, 1);
    // The recovered handle is fully usable.
    let _ = db.create_node(new_node("note", "y")).unwrap();
}

#[test]
fn recover_after_simulated_crash_reports_counter_drift_repaired() {
    let (_dir, path) = open_temp();
    {
        let db = Drevo::open(&path).unwrap();
        for i in 0..4 {
            db.create_node(new_node("note", &format!("c{i}"))).unwrap();
        }
        // No close — simulated crash.
    }

    let (db, report) = Drevo::recover(&path).unwrap();
    assert!(
        report.counter_drift_repaired,
        "recover() should report counter_drift_repaired after a crash"
    );
    // And the next allocation does not collide.
    let n = db.create_node(new_node("note", "after-recover")).unwrap();
    assert!(n.id >= 5);
}

// ---------------------------------------------------------------
// Structural / cross-file tests
// ---------------------------------------------------------------

#[test]
fn readme_marks_wal_crash_recovery_task_done() {
    let readme = std::fs::read_to_string("README.md").unwrap();
    assert!(
        readme.contains("[x] `00053`"),
        "README does not mark 00053 done"
    );
    // The README should also describe the recovery model (counter rescan).
    assert!(
        readme.contains("crash recovery") || readme.contains("crash-recovery"),
        "README does not mention crash recovery"
    );
}

#[test]
fn db_module_docs_describe_crash_recovery() {
    let src = std::fs::read_to_string("src/db.rs").unwrap();
    assert!(
        src.contains("crash recovery") || src.contains("crash-recovery"),
        "src/db.rs module docs do not describe the crash-recovery model"
    );
}

#[test]
fn check_integrity_report_is_public_in_db_module() {
    // Locks the `pub` surface from being downgraded to `pub(crate)` by a
    // future refactor — the type is part of the documented recovery API.
    let src = std::fs::read_to_string("src/db.rs").unwrap();
    assert!(
        src.contains("pub struct IntegrityReport")
            || src.contains("pub use") && src.contains("IntegrityReport"),
        "IntegrityReport is not publicly exported from src/db.rs"
    );
}
