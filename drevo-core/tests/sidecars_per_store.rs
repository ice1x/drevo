//! A durable store's sidecars are named after its log file (issue #826):
//! `a.gbk.origin.json`, `a.gbk.stamps.jsonl`, `a.gbk.tombstones.json`,
//! `a.gbk.sequences.json`, as the lock already was (`a.gbk.lock`). Two stores
//! whose logs share a directory keep their own identity and stamps.
//!
//! A store written before had them under fixed names (`origin.json`, …). It
//! takes them over on open when it is the only store in its directory. When
//! the directory holds other stores, the fixed-name files may be another
//! store's: the store starts with a fresh origin and the lowest stamps, and
//! copies the tombstones and sequences, which can only make it more careful.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::delta::{StampedChange, VersionVector};
use drevo_core::engine::GraphEngine;
use drevo_core::hlc::Hlc;
use drevo_core::lww::Stamp;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::{NativeGraph, StampTarget};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_sidecars_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SIDECARS: [&str; 4] = [
    "origin.json",
    "stamps.jsonl",
    "tombstones.json",
    "sequences.json",
];

fn note(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn retitle(g: &NativeGraph, id: u64, title: &str) {
    g.update_node(
        id,
        NodePatch {
            title: Some(title.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn stamp(g: &NativeGraph, id: u64) -> Stamp {
    g.stamp_of(StampTarget::Node(id))
        .expect("a live node has a stamp")
}

fn tombstoned(g: &NativeGraph) -> Vec<[u8; 16]> {
    g.delta_since(&VersionVector::new())
        .changes
        .iter()
        .filter_map(|c| match c {
            StampedChange::DeleteNode(uuid, _) => Some(*uuid),
            _ => None,
        })
        .collect()
}

/// Rename a store's sidecars to the fixed names of the layout before #826.
fn to_legacy_layout(dir: &TmpDir, wal: &str) {
    for name in SIDECARS {
        let new = dir.path(&format!("{wal}.{name}"));
        if new.exists() {
            fs::rename(&new, dir.path(name)).unwrap();
        }
    }
}

fn wait() {
    // Stamps carry wall-clock milliseconds: make the later store's writes
    // later.
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[test]
fn two_stores_in_one_directory_keep_their_own_sidecars() {
    let dir = TmpDir::new();
    let (a_id, a_stamp, a_origin) = {
        let a = NativeGraph::open_durable(dir.path("a.gbk")).unwrap();
        let n = a.create_node(note("in A")).unwrap();
        (n.id, stamp(&a, n.id), a.origin_id())
    };
    wait();
    let b_origin = {
        let b = NativeGraph::open_durable(dir.path("b.gbk")).unwrap();
        let n = b.create_node(note("in B")).unwrap();
        for i in 0..3 {
            retitle(&b, n.id, &format!("in B, edit {i}"));
        }
        b.origin_id()
    };
    assert_ne!(a_origin, b_origin, "two stores are two replicas");
    let a = NativeGraph::open_durable(dir.path("a.gbk")).unwrap();
    assert_eq!(a.origin_id(), a_origin, "A keeps its identity");
    assert_eq!(
        stamp(&a, a_id),
        a_stamp,
        "node 1 of A keeps its own stamp, not B's later one"
    );
    let names = dir.names();
    for store in ["a.gbk", "b.gbk"] {
        for name in ["origin.json", "stamps.jsonl"] {
            assert!(
                names.contains(&format!("{store}.{name}")),
                "{store}.{name} in {names:?}"
            );
        }
    }
    for name in SIDECARS {
        assert!(
            !names.contains(&name.to_string()),
            "no shared `{name}`: {names:?}"
        );
    }
}

#[test]
fn a_store_alone_in_its_directory_takes_over_its_old_sidecars() {
    let dir = TmpDir::new();
    let (origin, kept, kept_stamp, gone) = {
        let g = NativeGraph::open_durable(dir.path("native.wal")).unwrap();
        let kept = g.create_node(note("kept")).unwrap();
        let gone = g.create_node(note("gone")).unwrap();
        g.delete_node(gone.id).unwrap();
        g.sequences().create("bug", 1000, 1).unwrap();
        assert_eq!(g.sequence_next("bug").unwrap(), 1000);
        (g.origin_id(), kept.id, stamp(&g, kept.id), gone.uuid)
    };
    to_legacy_layout(&dir, "native.wal");
    assert!(dir.path("origin.json").exists());

    let g = NativeGraph::open_durable(dir.path("native.wal")).unwrap();
    assert_eq!(g.origin_id(), origin, "the origin is taken over");
    assert_eq!(stamp(&g, kept), kept_stamp, "the stamps are taken over");
    assert!(
        tombstoned(&g).contains(&gone),
        "the tombstones are taken over"
    );
    assert!(
        g.sequence_next("bug").unwrap() > 1000,
        "the sequence continues"
    );
    let names = dir.names();
    for name in SIDECARS {
        assert!(
            !names.contains(&name.to_string()),
            "`{name}` moved: {names:?}"
        );
        assert!(
            names.contains(&format!("native.wal.{name}")),
            "native.wal.{name}: {names:?}"
        );
    }
}

#[test]
fn old_sidecars_in_a_shared_directory_are_not_taken_as_identity() {
    let dir = TmpDir::new();
    // A store written before #826, alone at first.
    let (origin, kept, gone) = {
        let a = NativeGraph::open_durable(dir.path("a.gbk")).unwrap();
        let kept = a.create_node(note("kept")).unwrap();
        let gone = a.create_node(note("gone")).unwrap();
        a.delete_node(gone.id).unwrap();
        a.sequences().create("bug", 1000, 1).unwrap();
        assert_eq!(a.sequence_next("bug").unwrap(), 1000);
        (a.origin_id(), kept.id, gone.uuid)
    };
    to_legacy_layout(&dir, "a.gbk");
    // Another store opened in the same directory: the fixed-name files could
    // now be either store's.
    drop(NativeGraph::open_durable(dir.path("b.gbk")).unwrap());

    let a = NativeGraph::open_durable(dir.path("a.gbk")).unwrap();
    assert_ne!(a.origin_id(), origin, "a fresh origin, not a shared one");
    assert_eq!(
        stamp(&a, kept),
        Stamp::new(Hlc::default(), a.origin_id()),
        "the lowest stamp: it never beats a real edit"
    );
    assert!(
        tombstoned(&a).contains(&gone),
        "tombstones are copied: a peer cannot resurrect the deleted node"
    );
    assert!(
        a.sequence_next("bug").unwrap() > 1000,
        "sequences are copied: no value is handed out twice"
    );
    let names = dir.names();
    for name in SIDECARS {
        assert!(
            names.contains(&name.to_string()),
            "`{name}` is left for the other store: {names:?}"
        );
    }
    assert_ne!(
        NativeGraph::open_durable(dir.path("b.gbk"))
            .unwrap()
            .origin_id(),
        a.origin_id()
    );
}

#[test]
fn a_read_only_store_reads_its_old_sidecars_without_moving_them() {
    let dir = TmpDir::new();
    let (origin, kept, kept_stamp) = {
        let g = NativeGraph::open_durable(dir.path("native.wal")).unwrap();
        let kept = g.create_node(note("kept")).unwrap();
        (g.origin_id(), kept.id, stamp(&g, kept.id))
    };
    to_legacy_layout(&dir, "native.wal");
    let before = dir.names();
    let g = NativeGraph::open_read_only(dir.path("native.wal")).unwrap();
    assert_eq!(g.origin_id(), origin);
    assert_eq!(stamp(&g, kept), kept_stamp);
    drop(g);
    let after: Vec<String> = dir
        .names()
        .into_iter()
        .filter(|n| !n.ends_with(".lock"))
        .collect();
    let before: Vec<String> = before
        .into_iter()
        .filter(|n| !n.ends_with(".lock"))
        .collect();
    assert_eq!(after, before, "a read-only open writes nothing");
}

#[test]
fn the_sidecar_path_is_the_log_file_name_and_the_sidecar_name() {
    assert_eq!(
        drevo_core::native::sidecar_path(Path::new("/data/x/a.gbk"), "origin.json"),
        PathBuf::from("/data/x/a.gbk.origin.json")
    );
    assert_eq!(
        drevo_core::native::sidecar_path(Path::new("native.wal"), "stamps.jsonl"),
        PathBuf::from("native.wal.stamps.jsonl")
    );
}

#[test]
fn a_restore_writes_the_sidecars_under_the_restored_log_name() {
    use drevo_core::backup::{restore, BackupOptions};
    let dir = TmpDir::new();
    let (origin, kept, kept_stamp) = {
        let a = NativeGraph::open_durable(dir.path("a.gbk")).unwrap();
        let kept = a.create_node(note("kept")).unwrap();
        a.backup_to(&dir.path("backup"), &BackupOptions::default())
            .unwrap();
        (a.origin_id(), kept.id, stamp(&a, kept.id))
    };
    restore(&[dir.path("backup")], &dir.path("c.gbk")).unwrap();
    let names = dir.names();
    for name in SIDECARS {
        assert!(
            names.contains(&format!("c.gbk.{name}")),
            "c.gbk.{name}: {names:?}"
        );
        assert!(!names.contains(&name.to_string()), "`{name}`: {names:?}");
    }
    let c = NativeGraph::open_durable(dir.path("c.gbk")).unwrap();
    assert_eq!(c.origin_id(), origin, "a restore is the same replica");
    assert_eq!(stamp(&c, kept), kept_stamp);
}
