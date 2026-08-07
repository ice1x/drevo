//! Reconnaissance / reproduction for the "compaction doesn't help — and a
//! dump→import rebuild makes the file BIGGER" report (the live 412 MB agent
//! -memory tree: `compact()` reclaimed 0, and `shrink` produced ~514 MB).
//!
//! Manual benchmark (`#[ignore]`d, opens real redb files). It answers two
//! questions with hard numbers and guards whatever fix lands:
//!
//!   A. Does a `dump→import` rebuild produce a SMALLER file than the source,
//!      or (as observed) a BIGGER one?  ← the "shrink grew my disk" report
//!   B. Does in-place `compact()` reclaim space freed by deletes?
//!
//! Kept fast by avoiding per-commit churn (batched creates; a modest,
//! bounded delete set for B). Run:
//!
//! ```text
//! cargo test --features redb-backend --test compaction_repro_tests -- --ignored --nocapture
//! ```

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::db::Drevo;
use drevo::model::{NewNode, Properties};

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// ~4 KB of varied words → many distinct FTS trigrams per node (the FTS index
/// is what dominates a text-heavy tree's on-disk size).
fn body(seed: usize) -> String {
    let mut s = String::new();
    for w in 0..480 {
        s.push_str(&format!(
            "n{seed}w{w} anxious deadline mentor graph vector "
        ));
    }
    s
}

fn text_nodes(n: usize) -> Vec<NewNode> {
    (0..n)
        .map(|i| NewNode {
            kind: "Entity".into(),
            title: format!("n{i}"),
            body: body(i),
            body_html: String::new(),
            properties: Properties(HashMap::new()),
        })
        .collect()
}

/// A — the core repro: source vs in-place compact vs dump→import rebuild, on a
/// freshly-written text-heavy graph (no churn — this is the exact `shrink`
/// path a client runs on a healthy tree).
#[test]
#[ignore = "manual compaction reproduction benchmark; run with --ignored --nocapture"]
fn rebuild_vs_source_size() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.redb");

    const N: usize = 2500;
    let mut db = Drevo::open(&src).unwrap();
    db.create_nodes(text_nodes(N)).unwrap();

    let source = file_len(&src);
    let rep = db.bloat_report().unwrap();
    let ks = db.keyspace_stats().unwrap();

    // in-place compact()
    let creport = db.compact().unwrap();
    let after_compact = file_len(&src);
    let graphml = db.export_graphml().unwrap();
    db.close().unwrap();

    // dump → import into a FRESH file (the `drevo shrink` path)
    let rebuilt = dir.path().join("rebuilt.redb");
    let dst = Drevo::open(&rebuilt).unwrap();
    let ireport = dst.import_graphml(&graphml).unwrap();
    let after_rebuild = file_len(&rebuilt);
    let rrep = dst.bloat_report().unwrap();
    dst.close().unwrap();

    eprintln!("\n============ A. REBUILD vs SOURCE ({N} text-heavy nodes) ============");
    eprintln!(
        "source file            : {:8.2} MiB   (stored {:.2} = records {:.2} + index {:.2}; ratio {:?})",
        mib(source),
        mib(rep.stored_bytes),
        mib(rep.logical_bytes),
        mib(rep.index_bytes),
        rep.bloat_ratio.map(|r| (r * 100.0).round() / 100.0),
    );
    eprintln!(
        "after compact() in place: {:8.2} MiB   (reclaimed {:+.2} MiB; before={} after={})",
        mib(after_compact),
        mib(source) - mib(after_compact),
        creport.bytes_before.unwrap_or(0),
        creport.bytes_after.unwrap_or(0),
    );
    eprintln!(
        "after dump→import rebuild: {:8.2} MiB   (vs source {:+.2} MiB)  stored {:.2} (index {:.2})",
        mib(after_rebuild),
        mib(after_rebuild) - mib(source),
        mib(rrep.stored_bytes),
        mib(rrep.index_bytes),
    );
    eprintln!("import: {ireport:?}");
    eprintln!("--- keyspace breakdown (rows / content) — top 6 by row count ---");
    for s in ks.iter().take(6) {
        eprintln!(
            "  {:>10}: {:>9} rows   {:>8.2} MiB content",
            s.prefix,
            s.entries,
            mib(s.content_bytes)
        );
    }
    eprintln!("=====================================================================\n");

    assert_eq!(ireport.nodes_imported, N);
}

/// B — does compact() reclaim space freed by deletes? Bounded delete set so the
/// per-commit deletes stay quick.
#[test]
#[ignore = "manual compaction reproduction benchmark; run with --ignored --nocapture"]
fn compact_reclaims_deleted_space() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del.redb");

    const N: usize = 800;
    const DELETE: usize = 400;
    let mut db = Drevo::open(&path).unwrap();
    let created = db.create_nodes(text_nodes(N)).unwrap();
    let full = file_len(&path);

    // Free half the nodes (per-commit, but bounded).
    for node in created.iter().take(DELETE) {
        db.delete_node(node.id).unwrap();
    }
    let after_delete = file_len(&path);
    let rep_before = db.bloat_report().unwrap();

    let creport = db.compact().unwrap();
    let after_compact = file_len(&path);
    let rep_after = db.bloat_report().unwrap();
    db.close().unwrap();

    eprintln!("\n============ B. COMPACT RECLAIM ({N} nodes, {DELETE} deleted) ============");
    eprintln!("full ({N} nodes)        : {:8.2} MiB", mib(full));
    eprintln!(
        "after deleting {DELETE}     : {:8.2} MiB   (file {:+.2} MiB — redb keeps the high-water mark)",
        mib(after_delete),
        mib(after_delete) - mib(full),
    );
    eprintln!(
        "  stored dropped         : {:.2} → {:.2} MiB (real data shrank; file did not)",
        mib(rep_before.stored_bytes),
        mib(rep_after.stored_bytes),
    );
    eprintln!(
        "after compact()          : {:8.2} MiB   (reclaimed {:+.2} MiB; before={} after={})",
        mib(after_compact),
        mib(after_delete) - mib(after_compact),
        creport.bytes_before.unwrap_or(0),
        creport.bytes_after.unwrap_or(0),
    );
    eprintln!("======================================================================\n");
}
