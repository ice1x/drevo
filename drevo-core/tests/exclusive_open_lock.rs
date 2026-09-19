//! Exclusive-open protection for the durable `native` engine (#455).
//!
//! The KV/redb backend took an exclusive file lock, so opening the same store
//! twice raised — protecting an embedded user (or agent) from accidentally
//! double-opening one store and corrupting it with interleaved writes. The
//! native durable engine must restore that guarantee: a second
//! [`NativeGraph::open_durable`] on a path already held by a live handle fails
//! with [`CoreError::Locked`] rather than opening a second writer onto the same
//! write-ahead log.
//!
//! The lock is an OS **advisory** lock tied to the open file description, so it
//! is released when the owning handle drops (or the process dies) — a crash
//! never strands a stale lock that would block the prod watchdog's restart.
//! These tests exercise the Rust [`drevo_core`] seam directly (no HTTP layer).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::NativeGraph;

// std-only temp dir (drevo-core is dependency-light; no `tempfile` dev-dep).
static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_lock_{}_{}",
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
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Assert that opening `path` fails with `Locked` (`NativeGraph` is not `Debug`,
/// so `expect_err` can't be used on the `Ok` side).
fn assert_locked(path: &std::path::Path) {
    match NativeGraph::open_durable(path) {
        Ok(_) => panic!("open must be rejected while the store is locked"),
        Err(CoreError::Locked) => {}
        Err(other) => panic!("expected CoreError::Locked, got {other:?}"),
    }
}

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

/// A second `open_durable` on the same path, while the first handle is alive,
/// is rejected with `Locked` — the core of the double-open protection.
#[test]
fn second_open_on_same_path_is_rejected_while_first_is_held() {
    let tmp = TmpDir::new();
    let path = tmp.wal();

    let first = NativeGraph::open_durable(&path).expect("first open succeeds");
    first.create_node(node("alice")).expect("write");

    assert_locked(&path);

    // The living first handle is unharmed by the rejected second open.
    first.create_node(node("bob")).expect("write still works");
    drop(first);
}

/// Dropping the first handle releases the advisory lock, so the store can be
/// reopened — and the reopened handle recovers the acknowledged writes. This is
/// the crash-safety property: an advisory fd lock dies with its owner, unlike a
/// content lockfile that would need manual cleanup after a crash.
#[test]
fn reopen_succeeds_after_the_first_handle_is_dropped() {
    let tmp = TmpDir::new();
    let path = tmp.wal();

    {
        let db = NativeGraph::open_durable(&path).expect("open");
        db.create_node(node("carol")).expect("write");
    } // lock released here

    let reopened = NativeGraph::open_durable(&path).expect("reopen after release");
    assert_eq!(reopened.node_count(), 1, "recovered the acknowledged write");

    // And it can be locked again — a third open while this one lives is rejected.
    assert_locked(&path);
}

/// Two *different* paths lock independently — the guard is per-store, not global.
#[test]
fn distinct_paths_do_not_contend() {
    let tmp = TmpDir::new();
    let a = tmp.0.join("a.wal");
    let b = tmp.0.join("b.wal");

    let da = NativeGraph::open_durable(&a).expect("open a");
    let db = NativeGraph::open_durable(&b).expect("open b — different store");
    da.create_node(node("x")).expect("write a");
    db.create_node(node("y")).expect("write b");
}
