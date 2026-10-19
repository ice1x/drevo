//! Online physical backup of a durable store, and restore.
//!
//! - [`NativeGraph::backup_to`] copies the write-ahead log and its sidecars
//!   at a commit boundary while the store keeps serving, plus a manifest with
//!   checksums.
//! - An incremental backup holds only the log bytes written since the
//!   previous backup; after a compaction it is taken in full, and says why.
//! - [`restore`] verifies every checksum and the chain, then writes a new
//!   store that opens to the backed-up state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use drevo_core::backup::{
    restore, BackupError, BackupKind, BackupManifest, BackupOptions, MANIFEST_FILE,
};
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_backup_{}_{}",
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

/// The store being backed up, in `live/`.
fn live(tmp: &TmpDir) -> NativeGraph {
    fs::create_dir_all(tmp.path("live")).unwrap();
    NativeGraph::open_durable(tmp.path("live/native.wal")).unwrap()
}

fn task(title: &str) -> NewNode {
    NewNode {
        kind: "Task".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn titles(g: &NativeGraph) -> Vec<String> {
    let mut t: Vec<String> = g
        .all_nodes()
        .unwrap()
        .iter()
        .map(|n| n.title.clone())
        .collect();
    t.sort();
    t
}

fn edges(g: &NativeGraph) -> Vec<(String, String, String)> {
    let nodes = g.all_nodes().unwrap();
    let title = |id: u64| {
        nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.title.clone())
            .unwrap_or_default()
    };
    let mut e: Vec<_> = g
        .all_edges()
        .unwrap()
        .iter()
        .map(|e| (title(e.from_id), e.kind.clone(), title(e.to_id)))
        .collect();
    e.sort();
    e
}

/// An IT task board: tasks with dependencies, one deleted.
fn task_board(g: &NativeGraph) {
    let spec = g.create_node(task("write spec")).unwrap();
    let build = g.create_node(task("build")).unwrap();
    let scrap = g.create_node(task("scrapped idea")).unwrap();
    g.create_edge(NewEdge {
        from_id: build.id,
        to_id: spec.id,
        kind: "DEPENDS_ON".into(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap();
    g.delete_node(scrap.id).unwrap();
}

fn full(dir: &Path, g: &NativeGraph) -> BackupManifest {
    g.backup_to(dir, &BackupOptions::default()).unwrap()
}

fn incremental(dir: &Path, g: &NativeGraph, since: &BackupManifest) -> BackupManifest {
    g.backup_to(
        dir,
        &BackupOptions {
            since: Some(since.clone()),
            ..BackupOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn a_full_backup_restores_the_same_graph() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let manifest = full(&tmp.path("b1"), &g);
    assert_eq!(manifest.kind, BackupKind::Full);
    assert_eq!(manifest.wal_start, 0);
    assert!(manifest.wal_end > 0);
    assert!(manifest.full_because.is_none());
    for name in [
        "native.wal",
        "origin.json",
        "tombstones.json",
        "stamps.jsonl",
    ] {
        let file = manifest
            .files
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {:?}", manifest.files));
        assert_eq!(
            fs::metadata(tmp.path("b1").join(name)).unwrap().len(),
            file.bytes
        );
        assert_eq!(file.sha256.len(), 64);
    }
    // The manifest on disk is the one returned.
    assert_eq!(BackupManifest::read(&tmp.path("b1")).unwrap(), manifest);
    assert!(tmp.path("b1").join(MANIFEST_FILE).exists());

    // The store keeps working after the backup.
    g.create_node(task("after the backup")).unwrap();

    let report = restore(&[tmp.path("b1")], &tmp.path("restored/native.wal")).unwrap();
    assert_eq!(report.wal_bytes, manifest.wal_end);
    let r = NativeGraph::open_durable(tmp.path("restored/native.wal")).unwrap();
    assert_eq!(titles(&r), ["build", "write spec"]);
    assert_eq!(
        edges(&r),
        [("build".into(), "DEPENDS_ON".into(), "write spec".into())]
    );
    // The same replica identity, and a store that takes writes.
    assert_eq!(r.origin_id(), g.origin_id());
    r.create_node(task("restored and writable")).unwrap();
}

#[test]
fn a_backup_under_concurrent_writes_restores_to_a_commit_point() {
    let tmp = TmpDir::new();
    let g = Arc::new(live(&tmp));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let g = Arc::clone(&g);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // Each transaction writes a pair: a backup must never hold
                // half of one.
                let tx = g.tx_begin();
                let engine = g.tx_engine(tx).unwrap();
                engine.create_node(task(&format!("a-{i:05}"))).unwrap();
                engine.create_node(task(&format!("b-{i:05}"))).unwrap();
                g.tx_commit(tx).unwrap();
                i += 1;
            }
            i
        })
    };
    // Let some writes land, then back up while they keep coming.
    while titles(&g).len() < 20 {
        std::thread::yield_now();
    }
    full(&tmp.path("b1"), &g);
    stop.store(true, Ordering::Relaxed);
    let written = writer.join().unwrap();

    restore(&[tmp.path("b1")], &tmp.path("restored/native.wal")).unwrap();
    let r = NativeGraph::open_durable(tmp.path("restored/native.wal")).unwrap();
    let got = titles(&r);
    assert!(got.len() >= 20, "{} nodes", got.len());
    assert_eq!(got.len() % 2, 0, "half a transaction: {got:?}");
    let pairs = (got.len() / 2) as u64;
    assert!(pairs <= written);
    // Exactly the first `pairs` transactions: a prefix of the commits.
    let mut want: Vec<String> = (0..pairs)
        .flat_map(|i| [format!("a-{i:05}"), format!("b-{i:05}")])
        .collect();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn a_full_backup_and_two_incrementals_restore_the_source() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);

    let review = g.create_node(task("review")).unwrap();
    let b2 = incremental(&tmp.path("b2"), &g, &b1);
    assert_eq!(b2.kind, BackupKind::Incremental);
    assert_eq!(b2.wal_start, b1.wal_end);
    assert_eq!(b2.base_sha256.as_deref(), Some(b1.wal_sha256.as_str()));
    // Only the new log bytes.
    let tail = b2
        .files
        .iter()
        .find(|f| f.name == "native.wal.tail")
        .unwrap();
    assert_eq!(tail.bytes, b2.wal_end - b2.wal_start);
    assert!(b2.files.iter().all(|f| f.name != "native.wal"));

    g.update_node(
        review.id,
        drevo_core::model::NodePatch {
            title: Some("code review".into()),
            ..Default::default()
        },
    )
    .unwrap();
    g.create_node(task("release")).unwrap();
    let b3 = incremental(&tmp.path("b3"), &g, &b2);
    assert_eq!(b3.wal_start, b2.wal_end);

    let chain = [tmp.path("b1"), tmp.path("b2"), tmp.path("b3")];
    let report = restore(&chain, &tmp.path("restored/native.wal")).unwrap();
    assert_eq!(report.wal_bytes, b3.wal_end);
    let r = NativeGraph::open_durable(tmp.path("restored/native.wal")).unwrap();
    assert_eq!(titles(&r), titles(&g));
    assert_eq!(edges(&r), edges(&g));

    // A shorter chain restores the earlier point.
    restore(&chain[..2], &tmp.path("at-b2/native.wal")).unwrap();
    let r2 = NativeGraph::open_durable(tmp.path("at-b2/native.wal")).unwrap();
    assert_eq!(titles(&r2), ["build", "review", "write spec"]);
}

#[test]
fn an_incremental_with_nothing_new_is_empty_and_still_chains() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);
    let b2 = incremental(&tmp.path("b2"), &g, &b1);
    assert_eq!(b2.wal_start, b2.wal_end);
    assert_eq!(b2.wal_sha256, b1.wal_sha256);
    restore(&[tmp.path("b1"), tmp.path("b2")], &tmp.path("r/native.wal")).unwrap();
    let r = NativeGraph::open_durable(tmp.path("r/native.wal")).unwrap();
    assert_eq!(titles(&r), ["build", "write spec"]);
}

#[test]
fn an_incremental_after_a_compaction_is_taken_in_full() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);
    g.create_node(task("review")).unwrap();
    g.compact_wal().unwrap();
    let b2 = incremental(&tmp.path("b2"), &g, &b1);
    assert_eq!(b2.kind, BackupKind::Full);
    let why = b2.full_because.expect("says why it is full");
    assert!(why.contains("compacted"), "{why}");
    // It restores on its own.
    restore(&[tmp.path("b2")], &tmp.path("r/native.wal")).unwrap();
    let r = NativeGraph::open_durable(tmp.path("r/native.wal")).unwrap();
    assert_eq!(titles(&r), ["build", "review", "write spec"]);
}

#[test]
fn an_incremental_after_a_compaction_that_grew_the_log_is_taken_in_full() {
    // Compacted, yet longer than at the previous backup: only the bytes tell.
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);
    for i in 0..50 {
        g.create_node(task(&format!("ticket {i}"))).unwrap();
    }
    g.compact_wal().unwrap();
    let compacted = fs::metadata(tmp.path("live/native.wal")).unwrap().len();
    assert!(compacted > b1.wal_end, "{compacted} <= {}", b1.wal_end);
    let b2 = incremental(&tmp.path("b2"), &g, &b1);
    assert_eq!(b2.kind, BackupKind::Full);
    assert!(b2.full_because.unwrap().contains("compacted"));
    restore(&[tmp.path("b2")], &tmp.path("r/native.wal")).unwrap();
    let r = NativeGraph::open_durable(tmp.path("r/native.wal")).unwrap();
    assert_eq!(titles(&r), titles(&g));
}

#[test]
fn a_damaged_backup_is_refused() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);
    g.create_node(task("review")).unwrap();
    incremental(&tmp.path("b2"), &g, &b1);

    // One flipped byte in the log.
    let wal = tmp.path("b1").join("native.wal");
    let mut bytes = fs::read(&wal).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x20;
    fs::write(&wal, &bytes).unwrap();
    let e = restore(&[tmp.path("b1")], &tmp.path("r1/native.wal")).unwrap_err();
    assert!(
        matches!(&e, BackupError::Corrupt { file, .. } if file.ends_with("native.wal")),
        "{e}"
    );
    assert!(!tmp.path("r1/native.wal").exists(), "nothing is written");

    // A truncated tail.
    bytes[mid] ^= 0x20;
    fs::write(&wal, &bytes).unwrap();
    let tail = tmp.path("b2").join("native.wal.tail");
    let t = fs::read(&tail).unwrap();
    fs::write(&tail, &t[..t.len() - 1]).unwrap();
    let e = restore(
        &[tmp.path("b1"), tmp.path("b2")],
        &tmp.path("r2/native.wal"),
    )
    .unwrap_err();
    assert!(
        matches!(&e, BackupError::Corrupt { file, .. } if file.ends_with("native.wal.tail")),
        "{e}"
    );

    // A sidecar.
    fs::write(tmp.path("b1").join("origin.json"), b"{}").unwrap();
    let e = restore(&[tmp.path("b1")], &tmp.path("r3/native.wal")).unwrap_err();
    assert!(
        matches!(&e, BackupError::Corrupt { file, .. } if file.ends_with("origin.json")),
        "{e}"
    );

    // A missing manifest.
    let e = restore(&[tmp.path("nowhere")], &tmp.path("r4/native.wal")).unwrap_err();
    assert!(matches!(e, BackupError::Manifest { .. }), "{e}");
}

#[test]
fn a_broken_chain_is_refused() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let b1 = full(&tmp.path("b1"), &g);
    g.create_node(task("one")).unwrap();
    let b2 = incremental(&tmp.path("b2"), &g, &b1);
    g.create_node(task("two")).unwrap();
    incremental(&tmp.path("b3"), &g, &b2);

    // A gap: b3 continues b2, not b1.
    let e = restore(
        &[tmp.path("b1"), tmp.path("b3")],
        &tmp.path("r1/native.wal"),
    )
    .unwrap_err();
    assert!(matches!(e, BackupError::BrokenChain(_)), "{e}");
    // No full backup first.
    let e = restore(&[tmp.path("b2")], &tmp.path("r2/native.wal")).unwrap_err();
    assert!(matches!(e, BackupError::BrokenChain(_)), "{e}");
    // A second full backup in the middle.
    let e = restore(
        &[tmp.path("b1"), tmp.path("b1")],
        &tmp.path("r3/native.wal"),
    )
    .unwrap_err();
    assert!(matches!(e, BackupError::BrokenChain(_)), "{e}");
    // Nothing to restore.
    let e = restore::<PathBuf>(&[], &tmp.path("r4/native.wal")).unwrap_err();
    assert!(matches!(e, BackupError::BrokenChain(_)), "{e}");
}

#[test]
fn restore_never_overwrites_a_store() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    full(&tmp.path("b1"), &g);
    fs::create_dir_all(tmp.path("busy")).unwrap();
    fs::write(tmp.path("busy/native.wal"), b"precious").unwrap();
    let e = restore(&[tmp.path("b1")], &tmp.path("busy/native.wal")).unwrap_err();
    assert!(matches!(e, BackupError::TargetExists(_)), "{e}");
    assert_eq!(fs::read(tmp.path("busy/native.wal")).unwrap(), b"precious");
}

#[test]
fn a_backup_never_lands_on_another_backup() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    full(&tmp.path("b1"), &g);
    let e = g
        .backup_to(&tmp.path("b1"), &BackupOptions::default())
        .unwrap_err();
    assert!(matches!(e, BackupError::TargetExists(_)), "{e}");
}

#[test]
fn an_in_memory_graph_has_nothing_to_back_up() {
    let tmp = TmpDir::new();
    let g = NativeGraph::new();
    let e = g
        .backup_to(&tmp.path("b1"), &BackupOptions::default())
        .unwrap_err();
    assert!(matches!(e, BackupError::NotDurable), "{e}");
}

#[test]
fn extra_files_travel_with_the_backup_and_come_back_next_to_the_log() {
    let tmp = TmpDir::new();
    let g = live(&tmp);
    task_board(&g);
    let options = BackupOptions {
        extra_files: vec![("semantic.json".into(), br#"{"node":[]}"#.to_vec())],
        ..BackupOptions::default()
    };
    let b1 = g.backup_to(&tmp.path("b1"), &options).unwrap();
    assert!(b1.files.iter().any(|f| f.name == "semantic.json"));
    restore(&[tmp.path("b1")], &tmp.path("r/native.wal")).unwrap();
    assert_eq!(
        fs::read(tmp.path("r/semantic.json")).unwrap(),
        br#"{"node":[]}"#
    );

    // An extra file may not pose as the log or a sidecar.
    for bad in [
        "native.wal",
        "manifest.json",
        "../escape.json",
        "a/b.json",
        "",
    ] {
        let options = BackupOptions {
            extra_files: vec![(bad.into(), b"x".to_vec())],
            ..BackupOptions::default()
        };
        let e = g.backup_to(&tmp.path(&format!("bad-{}", bad.len())), &options);
        assert!(
            matches!(e, Err(BackupError::InvalidFileName(_))),
            "{bad}: {e:?}"
        );
    }
}
