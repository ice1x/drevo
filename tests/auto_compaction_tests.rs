//! Integration test for #253 slice 2 opt-in auto-compaction on `Drevo::open`.
//!
//! Lives in its own test binary so that mutating the process-global
//! `DREVO_AUTO_COMPACT*` environment does not race any other test. It drives
//! the real `open` → `AutoCompactPolicy::from_env` → `maybe_auto_compact`
//! → `compact` path (thresholds set so a compaction genuinely runs on open)
//! and asserts the graph data survives the reclaim intact — i.e. the automatic
//! maintenance never endangers the tree.

#![cfg(feature = "redb-backend")]

use drevo::db::Drevo;
use drevo::model::{NewNode, Properties};
use tempfile::TempDir;

fn seed(path: &std::path::Path, n: usize) {
    let db = Drevo::open(path).unwrap();
    for i in 0..n {
        db.create_node(NewNode {
            kind: "n".into(),
            title: format!("t{i}"),
            body: "x".repeat(128),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    }
    db.close().unwrap();
}

#[test]
fn open_with_env_enabled_auto_compacts_and_keeps_data() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("auto.redb");
    seed(&path, 25);

    // Thresholds low enough that any on-disk file (ratio ≥ 1, min_bytes 0)
    // triggers a compaction during open.
    std::env::set_var("DREVO_AUTO_COMPACT", "1");
    std::env::set_var("DREVO_AUTO_COMPACT_RATIO", "1.0");
    std::env::set_var("DREVO_AUTO_COMPACT_MIN_BYTES", "0");

    // Open honours the env policy and compacts in-line (best-effort). The call
    // must still succeed and hand back a fully intact database.
    let db = Drevo::open(&path).unwrap();
    let report = db.bloat_report().unwrap();
    assert_eq!(report.node_count, 25, "no data lost across auto-compaction");
    assert!(db.get_node_by_title("t0").unwrap().is_some());
    assert!(db.get_node_by_title("t24").unwrap().is_some());
    db.close().unwrap();

    // Disabling the policy leaves a subsequent open as a plain open.
    std::env::set_var("DREVO_AUTO_COMPACT", "0");
    let db = Drevo::open(&path).unwrap();
    assert_eq!(db.bloat_report().unwrap().node_count, 25);

    std::env::remove_var("DREVO_AUTO_COMPACT");
    std::env::remove_var("DREVO_AUTO_COMPACT_RATIO");
    std::env::remove_var("DREVO_AUTO_COMPACT_MIN_BYTES");
}
