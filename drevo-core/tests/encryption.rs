//! A store whose write-ahead log is encrypted at rest: every record sealed
//! with a caller-supplied key, unreadable without it.
#![cfg(feature = "encryption")]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::backup::{restore, BackupOptions};
use drevo_core::encryption::Key;
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::wal_format::WalFormat;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_encryption_{}_{}",
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

const SECRET: &str = "patient-record-7731";

fn key() -> Key {
    Key::from_bytes([0x5a; 32])
}

fn secret_note(i: usize) -> NewNode {
    NewNode {
        kind: "Note".to_string(),
        title: format!("{SECRET} #{i}"),
        body: "diagnosis: confidential".to_string(),
        body_html: String::new(),
        properties: Properties::from([("ssn", serde_json::json!("123-45-6789"))]),
    }
}

fn contains(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
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

#[test]
fn an_encrypted_store_reopens_with_its_key_and_hides_its_data() {
    for format in [WalFormat::Json, WalFormat::Cbor, WalFormat::MsgPack] {
        if !format.compiled_in() {
            continue;
        }
        let tmp = TmpDir::new();
        let wal = tmp.path("native.wal");
        {
            let g = NativeGraph::open_durable_encrypted(&wal, format, key()).unwrap();
            for i in 0..3 {
                g.create_node(secret_note(i)).unwrap();
            }
        }
        let bytes = fs::read(&wal).unwrap();
        for needle in [SECRET, "confidential", "123-45-6789", "Note", "UpsertNode"] {
            assert!(
                !contains(&bytes, needle),
                "{format}: `{needle}` is readable in the log"
            );
        }
        let g = NativeGraph::open_durable_encrypted(&wal, format, key()).unwrap();
        assert_eq!(g.node_count(), 3, "{format}");
        assert!(titles(&g)[0].starts_with(SECRET));
    }
}

#[test]
fn a_wrong_or_missing_key_fails_the_open() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    drop(NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap());
    let wrong =
        NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, Key::from_bytes([1; 32]))
            .err()
            .expect("a wrong key must fail");
    assert!(
        wrong.to_string().contains("wrong encryption key"),
        "{wrong}"
    );
    let missing = NativeGraph::open_durable(&wal)
        .err()
        .expect("no key must fail");
    assert!(missing.to_string().contains("encrypted"), "{missing}");
    // The failed opens changed nothing: the right key still works.
    assert!(NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).is_ok());
}

#[test]
fn a_plain_store_is_not_opened_with_a_key() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    NativeGraph::open_durable(&wal)
        .unwrap()
        .create_node(secret_note(0))
        .unwrap();
    let err = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key())
        .err()
        .expect("a plain log must not open with a key");
    assert!(err.to_string().contains("not encrypted"), "{err}");
}

#[test]
fn a_crash_mid_append_recovers_the_acknowledged_writes() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    {
        let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
        for i in 0..4 {
            g.create_node(secret_note(i)).unwrap();
        }
    }
    let intact = fs::metadata(&wal).unwrap().len();
    // Half a frame: the record being written when the process died.
    fs::OpenOptions::new()
        .append(true)
        .open(&wal)
        .unwrap()
        .write_all(&[200, 0, 0, 0, 1, 2, 3])
        .unwrap();
    let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
    assert_eq!(g.node_count(), 4);
    assert_eq!(
        fs::metadata(&wal).unwrap().len(),
        intact,
        "the torn tail is dropped"
    );
    g.create_node(secret_note(9)).unwrap();
    drop(g);
    let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
    assert_eq!(g.node_count(), 5);
}

#[test]
fn compaction_and_transactions_stay_encrypted() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    {
        let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
        g.create_node(secret_note(0)).unwrap();
        let tx = g.tx_begin();
        g.tx_engine(tx)
            .unwrap()
            .create_node(secret_note(1))
            .unwrap();
        g.tx_engine(tx)
            .unwrap()
            .create_node(secret_note(2))
            .unwrap();
        g.tx_commit(tx).unwrap();
        g.compact_wal().unwrap();
        g.create_node(secret_note(3)).unwrap();
    }
    let bytes = fs::read(&wal).unwrap();
    assert!(!contains(&bytes, SECRET));
    let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
    assert_eq!(g.node_count(), 4);
}

#[test]
fn a_backup_stays_encrypted_and_restores_with_the_key() {
    let tmp = TmpDir::new();
    fs::create_dir_all(tmp.path("live")).unwrap();
    let g =
        NativeGraph::open_durable_encrypted(tmp.path("live/native.wal"), WalFormat::Json, key())
            .unwrap();
    g.create_node(secret_note(0)).unwrap();
    g.backup_to(&tmp.path("backup"), &BackupOptions::default())
        .unwrap();
    drop(g);
    let backed_up = fs::read(tmp.path("backup/native.wal")).unwrap();
    assert!(!contains(&backed_up, SECRET));

    fs::create_dir_all(tmp.path("restored")).unwrap();
    let wal = tmp.path("restored/native.wal");
    restore(&[tmp.path("backup")], &wal).unwrap();
    assert!(NativeGraph::open_durable(&wal).is_err());
    let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key()).unwrap();
    assert_eq!(g.node_count(), 1);
}

#[test]
fn a_read_only_open_needs_the_key_too() {
    let tmp = TmpDir::new();
    let wal = tmp.path("native.wal");
    NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key())
        .unwrap()
        .create_node(secret_note(0))
        .unwrap();
    assert!(NativeGraph::open_read_only(&wal).is_err());
    let g = NativeGraph::open_read_only_encrypted(&wal, key()).unwrap();
    assert!(g.is_read_only());
    assert_eq!(g.node_count(), 1);
}
