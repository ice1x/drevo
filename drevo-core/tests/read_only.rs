//! Read-only stores.
//!
//! - [`NativeGraph::open_read_only`] opens a durable store for reading: the
//!   log is replayed but never written, no sidecar is created or rewritten,
//!   and every write fails with [`CoreError::ReadOnly`]. It opens a store in
//!   a directory the process cannot write to.
//! - A read-only handle and a writer exclude each other; read-only handles
//!   share the store.
//! - [`NativeGraph::set_read_only`] refuses writes on a writable store until
//!   it is switched back.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::backup::BackupOptions;
use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::{CommitError, NativeGraph, PrepareError, WalOp};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_read_only_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("native.wal")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        #[cfg(unix)]
        make_writable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn task(title: &str) -> NewNode {
    NewNode {
        kind: "Task".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
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

/// A store with two tasks and an edge, written and closed.
fn seeded(tmp: &TmpDir) {
    let g = NativeGraph::open_durable(tmp.wal()).unwrap();
    let a = g.create_node(task("BUG-1 login fails")).unwrap();
    let b = g.create_node(task("BUG-2 logout fails")).unwrap();
    g.create_edge(NewEdge {
        from_id: a.id,
        to_id: b.id,
        kind: "blocks".to_string(),
        weight: 1.0,
        properties: Properties(Default::default()),
    })
    .unwrap();
}

/// Every file in `dir` with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn assert_read_only<T: std::fmt::Debug>(result: Result<T, CoreError>) {
    match result {
        Err(CoreError::ReadOnly) => {}
        other => panic!("expected CoreError::ReadOnly, got {other:?}"),
    }
}

#[test]
fn a_read_only_store_reads_what_was_written() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert!(g.is_read_only());
    assert_eq!(titles(&g), ["BUG-1 login fails", "BUG-2 logout fails"]);
    assert_eq!(g.all_edges().unwrap().len(), 1);
}

#[test]
fn every_write_to_a_read_only_store_fails() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    let id = g.all_nodes().unwrap()[0].id;
    let edge = g.all_edges().unwrap()[0].id;

    assert_read_only(g.create_node(task("BUG-3")));
    assert_read_only(g.create_nodes(vec![task("BUG-4")]));
    assert_read_only(g.update_node(
        id,
        NodePatch {
            title: Some("renamed".to_string()),
            ..NodePatch::default()
        },
    ));
    assert_read_only(g.delete_node(id));
    assert_read_only(g.delete_edge(edge));
    assert_read_only(g.set_embedding(id, vec![0.1, 0.2]));
    assert_read_only(g.apply_wal_ops(&[WalOp::DeleteNode(id)]));
    assert_read_only(g.compact_wal());

    assert_eq!(titles(&g), ["BUG-1 login fails", "BUG-2 logout fails"]);
    assert_eq!(g.all_edges().unwrap().len(), 1);
}

#[test]
fn a_transaction_that_writes_cannot_commit_or_prepare() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();

    let tx = g.tx_begin();
    let _ = g.tx_engine(tx).unwrap().create_node(task("BUG-3"));
    assert_eq!(g.tx_commit(tx), Err(CommitError::ReadOnly));

    let tx = g.tx_begin();
    let _ = g.tx_engine(tx).unwrap().create_node(task("BUG-4"));
    assert_eq!(g.tx_prepare(tx, "gid-1"), Err(PrepareError::ReadOnly));
    assert!(g.list_prepared().is_empty());

    assert_eq!(titles(&g), ["BUG-1 login fails", "BUG-2 logout fails"]);
}

#[test]
fn a_transaction_that_only_reads_commits() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    let tx = g.tx_begin();
    assert_eq!(g.tx_engine(tx).unwrap().all_nodes().unwrap().len(), 2);
    assert_eq!(g.tx_commit(tx), Ok(()));
}

#[test]
fn opening_read_only_changes_no_file() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let before = snapshot(&tmp.0);
    {
        let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
        let _ = g.create_node(task("BUG-3"));
        let _ = g.compact_wal();
    }
    assert_eq!(snapshot(&tmp.0), before);
}

#[test]
fn a_torn_tail_is_left_in_place() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    // An unacknowledged half record at the end of the log.
    let mut bytes = fs::read(tmp.wal()).unwrap();
    bytes.extend_from_slice(b"{\"UpsertNode\":{\"id\":9");
    fs::write(tmp.wal(), &bytes).unwrap();

    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert_eq!(titles(&g).len(), 2);
    drop(g);
    assert_eq!(fs::read(tmp.wal()).unwrap(), bytes);
}

#[test]
fn a_missing_log_is_not_found() {
    let tmp = TmpDir::new();
    match NativeGraph::open_read_only(tmp.wal()) {
        Err(CoreError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        Err(e) => panic!("expected a not-found error, got {e:?}"),
        Ok(_) => panic!("expected a not-found error, got a store"),
    }
    assert!(snapshot(&tmp.0).is_empty(), "nothing is created");
}

#[test]
fn a_writer_and_a_reader_exclude_each_other() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    {
        let _writer = NativeGraph::open_durable(tmp.wal()).unwrap();
        assert!(matches!(
            NativeGraph::open_read_only(tmp.wal()),
            Err(CoreError::Locked)
        ));
    }
    let first = NativeGraph::open_read_only(tmp.wal()).unwrap();
    let second = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert!(matches!(
        NativeGraph::open_durable(tmp.wal()),
        Err(CoreError::Locked)
    ));
    assert_eq!(titles(&first), titles(&second));
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o644));
        }
    }
}

/// Running as root ignores file modes, so a permission test proves nothing.
#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(unix)]
#[test]
fn a_store_in_a_directory_without_write_permission_opens_read_only() {
    if is_root() {
        return;
    }
    let tmp = TmpDir::new();
    seeded(&tmp);
    for name in snapshot(&tmp.0).keys() {
        set_mode(&tmp.0.join(name), 0o444);
    }
    set_mode(&tmp.0, 0o555);

    assert!(NativeGraph::open_durable(tmp.wal()).is_err());
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert_eq!(titles(&g).len(), 2);
    assert_read_only(g.create_node(task("BUG-3")));
}

#[cfg(unix)]
#[test]
fn a_store_without_a_lock_file_opens_read_only_on_read_only_media() {
    if is_root() {
        return;
    }
    let tmp = TmpDir::new();
    seeded(&tmp);
    fs::remove_file(tmp.0.join("native.wal.lock")).unwrap();
    for name in snapshot(&tmp.0).keys() {
        set_mode(&tmp.0.join(name), 0o444);
    }
    set_mode(&tmp.0, 0o555);

    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert_eq!(titles(&g).len(), 2);
}

#[test]
fn a_writable_store_can_be_switched_read_only_and_back() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_durable(tmp.wal()).unwrap();
    assert!(!g.is_read_only());

    g.set_read_only(true).unwrap();
    assert!(g.is_read_only());
    assert_read_only(g.create_node(task("BUG-3")));
    assert_eq!(titles(&g).len(), 2);

    g.set_read_only(false).unwrap();
    g.create_node(task("BUG-3")).unwrap();
    assert_eq!(titles(&g).len(), 3);
}

#[test]
fn a_store_opened_read_only_cannot_be_switched_writable() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    assert_read_only(g.set_read_only(false));
    assert!(g.is_read_only());
    g.set_read_only(true).unwrap();
}

#[test]
fn a_read_only_store_can_be_backed_up() {
    let tmp = TmpDir::new();
    seeded(&tmp);
    let g = NativeGraph::open_read_only(tmp.wal()).unwrap();
    let out = tmp.0.join("backup");
    let manifest = g.backup_to(&out, &BackupOptions::default()).unwrap();
    assert!(manifest.wal_end > 0);

    let restored = tmp.0.join("restored");
    drevo_core::backup::restore(&[out], &restored.join("native.wal")).unwrap();
    let copy = NativeGraph::open_durable(restored.join("native.wal")).unwrap();
    assert_eq!(titles(&copy), titles(&g));
}
