//! Binary write-ahead-log encodings (issue #582): CBOR and MessagePack next
//! to the default JSON Lines, with the same durability and recovery rules.

#![cfg(all(
    feature = "wal-cbor",
    feature = "wal-msgpack",
    not(target_arch = "wasm32")
))]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::replica::WalTailer;
use drevo_core::wal_format::WalFormat;
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory and the WAL path inside it.
fn wal_path() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "drevo_wal_formats_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    (dir, wal)
}

/// A journal entry (CBT journal) with an `f32` embedding, as a model returns.
fn entry(title: &str, seed: f32) -> NewNode {
    let embedding: Vec<f64> = (0..256_u16)
        .map(|i| f64::from((f32::from(i) * 0.37 + seed).sin() * 0.05))
        .collect();
    NewNode {
        kind: "Entry".into(),
        title: title.into(),
        body: "felt anxious before the meeting".into(),
        body_html: String::new(),
        properties: Properties(HashMap::from([
            (
                "mood".to_string(),
                json!({"score": 4, "tags": ["work", "sleep"]}),
            ),
            ("embedding".to_string(), json!(embedding)),
        ])),
    }
}

fn link(from: u64, to: u64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "FOLLOWS".into(),
        weight: 0.5,
        properties: Properties::default(),
    }
}

/// Write a small journal: entries, an edge, a transaction, an update and a
/// delete. Returns the expected node titles.
fn write_journal(g: &NativeGraph) -> Vec<String> {
    let a = g.create_node(entry("monday", 1.0)).unwrap();
    let b = g.create_node(entry("tuesday", 2.0)).unwrap();
    g.create_edge(link(a.id, b.id)).unwrap();
    let mut tx = g.begin();
    let c = tx.create_node(entry("wednesday", 3.0)).unwrap();
    tx.create_edge(link(b.id, c.id)).unwrap();
    tx.commit().unwrap();
    let gone = g.create_node(entry("draft", 4.0)).unwrap();
    g.delete_node(gone.id).unwrap();
    vec!["monday".into(), "tuesday".into(), "wednesday".into()]
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
fn each_format_recovers_exactly_what_was_written() {
    for format in [WalFormat::Json, WalFormat::Cbor, WalFormat::MsgPack] {
        let (dir, wal) = wal_path();
        let (want, nodes, edges) = {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            let want = write_journal(&g);
            (want, g.all_nodes().unwrap(), g.all_edges().unwrap())
        };
        let bytes = file_bytes(&wal);
        assert_eq!(
            bytes.starts_with(b"DRVWAL"),
            format != WalFormat::Json,
            "{format}"
        );

        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        assert_eq!(titles(&g), want, "{format}");
        assert_eq!(
            g.all_nodes().unwrap(),
            nodes,
            "{format}: byte-identical nodes"
        );
        assert_eq!(g.all_edges().unwrap(), edges, "{format}");
        // Recovered floats are the written ones, bit for bit.
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_torn_binary_tail_is_dropped_and_the_log_reopens() {
    for format in [WalFormat::Cbor, WalFormat::MsgPack] {
        let (dir, wal) = wal_path();
        {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            write_journal(&g);
        }
        let clean_len = file_bytes(&wal).len();
        // A record cut off mid-write: a frame announcing more bytes than exist.
        let mut f = std::fs::OpenOptions::new().append(true).open(&wal).unwrap();
        f.write_all(&500_u32.to_le_bytes()).unwrap();
        f.write_all(&[1, 2, 3, 4, 5, 6]).unwrap();
        drop(f);

        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        assert_eq!(titles(&g).len(), 3, "{format}");
        assert_eq!(
            file_bytes(&wal).len(),
            clean_len,
            "{format}: tail truncated"
        );
        // And the log keeps working.
        g.create_node(entry("thursday", 5.0)).unwrap();
        drop(g);
        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        assert_eq!(titles(&g).len(), 4, "{format}");
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_bad_checksum_on_the_last_record_is_a_torn_tail() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
        write_journal(&g);
        g.create_node(entry("last", 9.0)).unwrap();
    }
    let mut bytes = file_bytes(&wal);
    let n = bytes.len();
    bytes[n - 1] ^= 0xFF; // corrupt the final record's payload
    std::fs::write(&wal, &bytes).unwrap();
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert_eq!(
        titles(&g).len(),
        3,
        "the unacknowledged last record is dropped"
    );
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn corruption_before_valid_records_refuses_to_open() {
    for format in [WalFormat::Cbor, WalFormat::MsgPack] {
        let (dir, wal) = wal_path();
        {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            write_journal(&g);
        }
        let mut bytes = file_bytes(&wal);
        // First record's payload starts after the 8-byte header and 8-byte frame.
        bytes[8 + 8 + 3] ^= 0xFF;
        std::fs::write(&wal, &bytes).unwrap();
        let err = NativeGraph::open_durable_with(&wal, format)
            .err()
            .expect("acknowledged history is corrupt");
        assert!(err.to_string().contains("corrupt"), "{format}: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_torn_header_starts_a_new_log() {
    let (dir, wal) = wal_path();
    std::fs::write(&wal, b"DRVW").unwrap();
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert!(titles(&g).is_empty());
    g.create_node(entry("first", 1.0)).unwrap();
    drop(g);
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert_eq!(titles(&g), ["first"]);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compaction_switches_formats_and_any_format_opens_any_log() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable(&wal).unwrap(); // JSON
        write_journal(&g);
    }
    {
        // Asked for CBOR: the JSON log keeps taking JSON appends …
        let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
        g.create_node(entry("thursday", 5.0)).unwrap();
        assert!(!file_bytes(&wal).starts_with(b"DRVWAL"));
        // … until compaction rewrites it as CBOR.
        g.compact_wal().unwrap();
        assert!(file_bytes(&wal).starts_with(b"DRVWAL\x01C"));
        g.create_node(entry("friday", 6.0)).unwrap();
    }
    // A JSON-configured open still reads the CBOR log, and compacts it back.
    let g = NativeGraph::open_durable(&wal).unwrap();
    assert_eq!(titles(&g).len(), 5);
    g.compact_wal().unwrap();
    assert!(!file_bytes(&wal).starts_with(b"DRVWAL"));
    drop(g);
    let g = NativeGraph::open_durable_with(&wal, WalFormat::MsgPack).unwrap();
    assert_eq!(titles(&g).len(), 5);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn writes_racing_a_format_switch_never_mix_formats() {
    let (dir, wal) = wal_path();
    {
        let g = Arc::new(NativeGraph::open_durable(&wal).unwrap());
        drop(g);
    }
    let g = Arc::new(NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap());
    let writers: Vec<_> = (0..4)
        .map(|t| {
            let g = Arc::clone(&g);
            std::thread::spawn(move || {
                for i in 0..25 {
                    g.create_node(entry(&format!("t{t}-{i}"), i as f32))
                        .unwrap();
                }
            })
        })
        .collect();
    for _ in 0..5 {
        g.compact_wal().unwrap();
    }
    for w in writers {
        w.join().unwrap();
    }
    drop(g);
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert_eq!(titles(&g).len(), 100, "every acknowledged write recovers");
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cbor_logs_of_embeddings_are_much_smaller() {
    let size = |format| {
        let (dir, wal) = wal_path();
        {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            for i in 0..20_u8 {
                g.create_node(entry(&format!("e{i}"), f32::from(i)))
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
    let (json, cbor, msgpack) = (
        size(WalFormat::Json),
        size(WalFormat::Cbor),
        size(WalFormat::MsgPack),
    );
    assert!(cbor * 3 < json, "cbor {cbor} vs json {json}");
    assert!(msgpack < json, "msgpack {msgpack} vs json {json}");
}

#[test]
fn a_tailer_follows_a_binary_log_record_by_record() {
    for format in [WalFormat::Cbor, WalFormat::MsgPack] {
        let (dir, wal) = wal_path();
        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        let mut tailer = WalTailer::new(&wal);
        assert!(tailer.poll().unwrap().is_empty());

        g.create_node(entry("monday", 1.0)).unwrap();
        g.create_node(entry("tuesday", 2.0)).unwrap();
        let first = tailer.poll().unwrap();
        assert_eq!(first.len(), 2, "{format}");

        // A record still being appended is not consumed.
        let mut f = std::fs::OpenOptions::new().append(true).open(&wal).unwrap();
        f.write_all(&64_u32.to_le_bytes()).unwrap();
        drop(f);
        assert!(tailer.poll().unwrap().is_empty(), "{format}");

        let mirror = NativeGraph::new();
        mirror.apply_wal_ops(&first).unwrap();
        assert_eq!(titles(&mirror), ["monday", "tuesday"], "{format}");
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}
