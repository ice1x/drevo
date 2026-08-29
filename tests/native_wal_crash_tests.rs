//! Crash-recovery guards for the native engine's write-ahead log (RFC ACID
//! "D", Phase 3/4 — the durability track toward retiring redb).
//!
//! A crash can only ever tear the **unacknowledged tail** of the log (an
//! acknowledged write returns after fsync), so recovery must (a) truncate a
//! torn tail and open successfully — losing only what was never acknowledged
//! — while (b) still refusing a log whose *acknowledged middle* is corrupt,
//! and (c) never replaying half of a transaction's batch: a commit is
//! all-or-nothing on disk exactly as it is in memory.

use std::fs;
use std::io::Write;

use drevo::engine::GraphEngine;
use drevo::model::{NewNode, Properties};
use drevo::native::NativeGraph;

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

fn wal_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("native.wal")
}

fn titles(g: &NativeGraph) -> Vec<String> {
    let mut t: Vec<String> = GraphEngine::all_nodes(g)
        .unwrap()
        .into_iter()
        .map(|n| n.title.clone())
        .collect();
    t.sort();
    t
}

// ── torn tail (crash mid-append) ───────────────────────────────────────

#[test]
fn torn_tail_garbage_is_truncated_and_the_log_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        GraphEngine::create_node(&g, node("ada")).unwrap();
        GraphEngine::create_node(&g, node("bob")).unwrap();
    }
    // Simulate a crash mid-append: a partial JSON record at the tail.
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"UpsertNode\":{\"id\":9,\"uu").unwrap();
    drop(f);

    let g = NativeGraph::open_durable(&path).expect("torn tail must recover");
    assert_eq!(titles(&g), ["ada", "bob"], "acknowledged writes survive");

    // The tail was truncated, not skipped-over: appending keeps working and
    // a further reopen sees everything.
    GraphEngine::create_node(&g, node("cy")).unwrap();
    drop(g);
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(titles(&g), ["ada", "bob", "cy"]);
}

#[test]
fn torn_tail_cut_mid_record_recovers_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        GraphEngine::create_node(&g, node("ada")).unwrap();
        GraphEngine::create_node(&g, node("bob")).unwrap();
    }
    // Cut the file mid-way through the LAST record (never before it — an
    // fsynced record cannot be torn).
    let bytes = fs::read(&path).unwrap();
    let body = &bytes[..bytes.len() - 1]; // drop the final newline
    let last_line_start = body.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let cut = last_line_start + (bytes.len() - last_line_start) / 2;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cut as u64)
        .unwrap();

    let g = NativeGraph::open_durable(&path).expect("mid-record cut must recover");
    assert_eq!(titles(&g), ["ada"], "only the torn tail record is lost");
}

// ── acknowledged-middle corruption must still refuse ───────────────────

#[test]
fn corrupt_middle_record_is_an_error_not_silent_data_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        GraphEngine::create_node(&g, node("ada")).unwrap();
        GraphEngine::create_node(&g, node("bob")).unwrap();
        GraphEngine::create_node(&g, node("cy")).unwrap();
    }
    // Corrupt the SECOND line; valid data follows it, so this is not a torn
    // tail — it is lost acknowledged history and must be surfaced, never
    // silently dropped.
    let text = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 3);
    lines[1] = "{\"UpsertNode\":GARBAGE}";
    fs::write(&path, lines.join("\n") + "\n").unwrap();

    assert!(
        NativeGraph::open_durable(&path).is_err(),
        "corruption before valid records must refuse to open"
    );
}

// ── transaction batches are all-or-nothing on disk ─────────────────────

#[test]
fn a_torn_transaction_batch_replays_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        GraphEngine::create_node(&g, node("ada")).unwrap();
        let mut tx = g.begin();
        tx.create_node(node("bob")).unwrap();
        tx.create_node(node("cy")).unwrap();
        tx.commit().unwrap();
    }
    {
        // Both committed writes are there after a clean reopen.
        let g = NativeGraph::open_durable(&path).unwrap();
        assert_eq!(titles(&g), ["ada", "bob", "cy"]);
    }

    // Simulate the crash landing INSIDE the transaction's batch record: cut
    // the file at several points within the final record. However much of
    // the batch bytes survived, recovery must apply either none of the
    // transaction or all of it — never one of its two writes.
    let bytes = fs::read(&path).unwrap();
    let body = &bytes[..bytes.len() - 1];
    let batch_start = body.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let batch_len = bytes.len() - batch_start;
    assert!(batch_len > 8, "expected a real batch record at the tail");
    for cut_fraction in [1, 2, 3] {
        let cut = batch_start + batch_len * cut_fraction / 4;
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = dir2.path().join("native.wal");
        fs::write(&path2, &bytes[..cut]).unwrap();
        let g = NativeGraph::open_durable(&path2)
            .unwrap_or_else(|e| panic!("cut at {cut} must recover, got {e}"));
        assert_eq!(
            titles(&g),
            ["ada"],
            "a torn batch (cut at {cut}) must replay none of the transaction"
        );
    }
}

// ── format compatibility ───────────────────────────────────────────────

#[test]
fn bare_single_op_lines_from_older_logs_still_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = wal_path(&dir);
    // Produce a log through the current engine, then rewrite it as strictly
    // one-op-per-line (the pre-batch format) and reopen.
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        GraphEngine::create_node(&g, node("ada")).unwrap();
        GraphEngine::create_node(&g, node("bob")).unwrap();
    }
    let g = NativeGraph::open_durable(&path).unwrap();
    let ops = g.dump_wal();
    drop(g);
    let mut f = fs::File::create(&path).unwrap();
    for op in &ops {
        let line = serde_json::to_string(op).unwrap();
        writeln!(f, "{line}").unwrap();
    }
    drop(f);
    let g = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(titles(&g), ["ada", "bob"]);
}
