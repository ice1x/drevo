//! A write-ahead log of zstd-compressed CBOR records (`cbor-zstd`): each
//! record is compressed on its own, so the durability and recovery rules of
//! the other framed formats hold unchanged.

#![cfg(all(feature = "wal-zstd", not(target_arch = "wasm32")))]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::replica::WalTailer;
use drevo_core::wal_format::{crc32, WalFormat};
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory and the WAL path inside it.
fn wal_path() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "drevo_wal_zstd_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    (dir, wal)
}

/// A bug report with an `f32` embedding and the fields a tracker repeats on
/// every ticket.
fn ticket(title: &str, seed: f32) -> NewNode {
    let embedding: Vec<f64> = (0..256_u16)
        .map(|i| f64::from((f32::from(i) * 0.37 + seed).sin() * 0.05))
        .collect();
    NewNode {
        kind: "Bug".into(),
        title: title.into(),
        body: "Steps to reproduce: open the editor, paste a long line, press save. \
               Expected: the file is saved. Actual: the window closes."
            .into(),
        body_html: String::new(),
        properties: Properties::from(HashMap::from([
            ("project".to_string(), json!("editor")),
            ("severity".to_string(), json!("high")),
            ("component".to_string(), json!("save")),
            ("embedding".to_string(), json!(embedding)),
        ])),
    }
}

fn link(from: u64, to: u64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "DUPLICATES".into(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

/// File a few bugs, a duplicate link, an update and a close. Returns the
/// titles left.
fn file_bugs(g: &NativeGraph) -> Vec<String> {
    let a = g.create_node(ticket("Crash on save", 1.0)).unwrap();
    let b = g.create_node(ticket("Slow scroll", 2.0)).unwrap();
    let c = g.create_node(ticket("Typo in menu", 3.0)).unwrap();
    g.create_edge(link(b.id, a.id)).unwrap();
    g.update_node(
        a.id,
        NodePatch {
            title: Some("Crash on save (fixed)".into()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    g.delete_node(c.id).unwrap();
    titles(g)
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

fn file_bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn the_format_is_named_and_compiled_in() {
    assert_eq!(WalFormat::CborZstd.name(), "cbor-zstd");
    assert!(WalFormat::CborZstd.compiled_in());
    assert_eq!(
        "cbor-zstd".parse::<WalFormat>().unwrap(),
        WalFormat::CborZstd
    );
    assert_eq!(
        " CBOR-ZSTD ".parse::<WalFormat>().unwrap(),
        WalFormat::CborZstd
    );
}

#[test]
fn a_compressed_log_recovers_exactly_what_was_written() {
    let (dir, wal) = wal_path();
    let (want, nodes, edges) = {
        let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
        let want = file_bugs(&g);
        (want, g.all_nodes().unwrap(), g.all_edges().unwrap())
    };
    let bytes = file_bytes(&wal);
    assert!(
        bytes.starts_with(b"DRVWAL\x01Z"),
        "header {:?}",
        &bytes[..8]
    );

    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
    assert_eq!(titles(&g), want);
    assert_eq!(g.all_nodes().unwrap(), nodes, "byte-identical nodes");
    assert_eq!(g.all_edges().unwrap(), edges);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compaction_switches_to_and_from_the_compressed_format() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable(&wal).unwrap(); // JSON
        file_bugs(&g);
    }
    {
        // Asked for cbor-zstd: the JSON log keeps taking JSON appends until
        // compaction rewrites it.
        let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
        g.create_node(ticket("Login loop", 5.0)).unwrap();
        assert!(!file_bytes(&wal).starts_with(b"DRVWAL"));
        g.compact_wal().unwrap();
        assert!(file_bytes(&wal).starts_with(b"DRVWAL\x01Z"));
        g.create_node(ticket("Stale cache", 6.0)).unwrap();
        assert_eq!(titles(&g).len(), 4);
    }
    // Any configured format opens the compressed log, and compacts it back.
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert_eq!(titles(&g).len(), 4);
    g.compact_wal().unwrap();
    assert!(file_bytes(&wal).starts_with(b"DRVWAL\x01C"));
    drop(g);
    let g = NativeGraph::open_durable(&wal).unwrap();
    assert_eq!(titles(&g).len(), 4);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_compressed_log_is_smaller_than_cbor() {
    let size = |format| {
        let (dir, wal) = wal_path();
        {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            for i in 0..40_u8 {
                g.create_node(ticket(&format!("Bug {i}"), f32::from(i)))
                    .unwrap();
            }
            g.compact_wal().unwrap();
            assert_eq!(
                g.wal_bytes().unwrap(),
                g.wal_compacted_bytes(),
                "{format}: compacted-size estimate matches the file"
            );
        }
        let n = file_bytes(&wal).len();
        let _ = std::fs::remove_dir_all(dir);
        n
    };
    let (cbor, zstd) = (size(WalFormat::Cbor), size(WalFormat::CborZstd));
    assert!(zstd * 10 < cbor * 9, "cbor-zstd {zstd} vs cbor {cbor}");
}

#[test]
fn a_torn_compressed_tail_is_dropped_and_the_log_reopens() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
        file_bugs(&g);
    }
    let bytes = file_bytes(&wal);
    // Half of a record: its frame says more bytes than the file has.
    let mut torn = bytes.clone();
    torn.extend_from_slice(&800_u32.to_le_bytes());
    torn.extend_from_slice(&0_u32.to_le_bytes());
    torn.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd, 1, 2, 3]);
    std::fs::write(&wal, &torn).unwrap();

    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
    assert_eq!(titles(&g).len(), 2);
    g.create_node(ticket("After the crash", 9.0)).unwrap();
    drop(g);
    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
    assert_eq!(titles(&g).len(), 3);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

/// The frames of a compressed log: `(start, payload_start, end)` of each.
fn frames(bytes: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        out.push((pos, pos + 8, pos + 8 + len));
        pos += 8 + len;
    }
    out
}

#[test]
fn a_record_that_does_not_decompress_is_a_torn_tail_or_corruption() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
        file_bugs(&g);
    }
    let bytes = file_bytes(&wal);
    let frames = frames(&bytes);
    assert!(frames.len() >= 3, "{frames:?}");

    // The checksum holds but the compressed bytes are garbage: as the last
    // record, a torn tail that is dropped.
    let (_, start, end) = *frames.last().unwrap();
    let mut damaged = bytes.clone();
    for b in &mut damaged[start + 4..end] {
        *b = 0xA5;
    }
    let crc = crc32(&damaged[start..end]);
    damaged[start - 4..start].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&wal, &damaged).unwrap();
    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
    // The last record was the delete: the deleted bug is back.
    assert_eq!(titles(&g).len(), 3);
    drop(g);

    // The same damage with valid records after it is corruption.
    let (_, start, end) = frames[0];
    let mut damaged = bytes.clone();
    for b in &mut damaged[start + 4..end] {
        *b = 0xA5;
    }
    let crc = crc32(&damaged[start..end]);
    damaged[start - 4..start].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&wal, &damaged).unwrap();
    let err = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd)
        .err()
        .expect("corruption refuses to open");
    assert!(err.to_string().contains("corrupt"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_tailer_follows_a_compressed_log_record_by_record() {
    let (dir, wal) = wal_path();
    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).unwrap();
    let mut tailer = WalTailer::new(&wal);
    assert_eq!(tailer.poll().unwrap().len(), 0);
    g.create_node(ticket("Crash on save", 1.0)).unwrap();
    g.create_node(ticket("Slow scroll", 2.0)).unwrap();
    let ops = tailer.poll().unwrap();
    assert_eq!(ops.len(), 2, "{ops:?}");
    // A record still being written is not delivered until it is whole.
    let mut f = std::fs::OpenOptions::new().append(true).open(&wal).unwrap();
    f.write_all(&[9, 9, 9]).unwrap();
    f.flush().unwrap();
    assert_eq!(tailer.poll().unwrap().len(), 0);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "encryption")]
#[test]
fn an_encrypted_store_can_hold_a_compressed_log() {
    use drevo_core::encryption::Key;
    let key = || Key::from_bytes([7; 32]);
    let (dir, wal) = wal_path();
    let want = {
        let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::CborZstd, key()).unwrap();
        file_bugs(&g)
    };
    let bytes = file_bytes(&wal);
    assert!(
        bytes.starts_with(b"DRVWAL\x02Z"),
        "header {:?}",
        &bytes[..8]
    );
    let g = NativeGraph::open_durable_encrypted(&wal, WalFormat::CborZstd, key()).unwrap();
    assert_eq!(titles(&g), want);
    drop(g);
    assert!(NativeGraph::open_durable_with(&wal, WalFormat::CborZstd).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
