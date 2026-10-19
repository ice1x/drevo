//! Sequences of a native store: unique, increasing numbers handed out
//! outside transactions, kept in `sequences.json` next to the log.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo_core::backup::{restore, BackupOptions};
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::sequence::{SequenceError, BLOCK};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_sequences_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bug() -> NewNode {
    NewNode {
        kind: "Bug".to_string(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

#[test]
fn eight_threads_draw_eighty_thousand_distinct_values() {
    let tmp = TmpDir::new();
    let g = Arc::new(NativeGraph::open_durable(tmp.path("native.wal")).unwrap());
    g.sequences().create("bug", 1000, 1).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let g = Arc::clone(&g);
            std::thread::spawn(move || {
                (0..10_000)
                    .map(|_| g.sequence_next("bug").unwrap())
                    .collect::<Vec<i64>>()
            })
        })
        .collect();
    let mut all = Vec::new();
    for t in threads {
        let got = t.join().unwrap();
        assert!(got.windows(2).all(|w| w[0] < w[1]));
        all.extend(got);
    }
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 80_000);
    assert_eq!((all[0], all[79_999]), (1000, 80_999));
}

#[test]
fn a_reopened_store_never_reissues_a_value() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    let last = {
        let g = NativeGraph::open_durable(&wal).unwrap();
        g.sequences().create("invoice", 1, 1).unwrap();
        (0..42)
            .map(|_| g.sequence_next("invoice").unwrap())
            .last()
            .unwrap()
    };
    assert!(tmp.path("sequences.json").exists());
    let g = NativeGraph::open_durable(&wal).unwrap();
    let next = g.sequence_next("invoice").unwrap();
    assert!(next > last);
    assert_eq!(next, 1 + BLOCK, "the rest of the reserved block is skipped");
}

#[test]
fn a_rolled_back_transaction_leaves_a_gap() {
    let g = NativeGraph::new();
    g.sequences().create("bug", 1, 1).unwrap();
    let tx = g.tx_begin();
    let id = g.sequence_next("bug").unwrap();
    let mut node = bug();
    node.properties.insert("number", serde_json::json!(id));
    g.tx_engine(tx).unwrap().create_node(node).unwrap();
    g.tx_rollback(tx);
    assert_eq!(
        g.sequence_next("bug").unwrap(),
        2,
        "1 is not handed out again"
    );
}

#[test]
fn a_read_only_store_hands_out_nothing() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    {
        let g = NativeGraph::open_durable(&wal).unwrap();
        g.sequences().create("bug", 1, 1).unwrap();
        g.sequence_next("bug").unwrap();
    }
    let g = NativeGraph::open_read_only(&wal).unwrap();
    assert_eq!(g.sequence_next("bug"), Err(SequenceError::ReadOnly));
    assert!(g.sequences().get("bug").is_some());

    let live = NativeGraph::new();
    live.sequences().create("bug", 1, 1).unwrap();
    live.set_read_only(true).unwrap();
    assert_eq!(live.sequence_next("bug"), Err(SequenceError::ReadOnly));
    live.set_read_only(false).unwrap();
    assert_eq!(live.sequence_next("bug").unwrap(), 1);
}

#[test]
fn a_damaged_sequence_file_fails_the_open() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    drop(NativeGraph::open_durable(&wal).unwrap());
    fs::write(tmp.path("sequences.json"), b"{broken").unwrap();
    assert!(NativeGraph::open_durable(&wal).is_err());
}

#[test]
fn a_backup_restores_the_sequences() {
    let tmp = TmpDir::new();
    fs::create_dir_all(tmp.path("live")).unwrap();
    let g = NativeGraph::open_durable(tmp.path("live/native.wal")).unwrap();
    g.sequences().create("bug", 500, 1).unwrap();
    let before = (0..3)
        .map(|_| g.sequence_next("bug").unwrap())
        .last()
        .unwrap();
    g.backup_to(&tmp.path("backup"), &BackupOptions::default())
        .unwrap();
    drop(g);

    fs::create_dir_all(tmp.path("restored")).unwrap();
    let wal = tmp.path("restored/native.wal");
    restore(&[tmp.path("backup")], &wal).unwrap();
    let restored = NativeGraph::open_durable(&wal).unwrap();
    let info = restored.sequences().get("bug").unwrap();
    assert_eq!((info.start, info.step), (500, 1));
    assert!(restored.sequence_next("bug").unwrap() > before);
}
