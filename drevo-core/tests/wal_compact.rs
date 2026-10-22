//! A write-ahead log of compact CBOR records (`cbor-compact`): the built-in
//! field names are small integers, property names stay strings. The
//! framing, recovery rules and compaction are those of the other binary
//! formats. `cbor-compact-zstd` adds per-record zstd on top.

#![cfg(all(feature = "wal-cbor", not(target_arch = "wasm32")))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::replica::WalTailer;
use drevo_core::wal_format::WalFormat;
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn wal_path() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "drevo_wal_compact_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    (dir, wal)
}

/// A road-network junction: a tiny node whose bytes are mostly field names.
fn junction(i: u32) -> NewNode {
    NewNode {
        kind: "Junction".into(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::from(HashMap::from([
            ("lat".to_string(), json!(42.65 + f64::from(i) * 1e-5)),
            ("lon".to_string(), json!(23.32 + f64::from(i) * 1e-5)),
            (
                "osm_id".to_string(),
                json!(3_060_000_000_u64 + u64::from(i)),
            ),
        ])),
    }
}

/// A node whose properties are named like the built-in fields.
fn impostor() -> NewNode {
    NewNode {
        kind: "Record".into(),
        title: "property names that look like fields".into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::from(HashMap::from([
            ("id".to_string(), json!("ticket-7")),
            (
                "kind".to_string(),
                json!({"title": "nested", "properties": [1, 2]}),
            ),
            ("UpsertNode".to_string(), json!(true)),
            ("ops".to_string(), json!([{"gid": "x"}])),
        ])),
    }
}

fn road(from: u64, to: u64, i: u32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "ROAD".into(),
        weight: 1.0,
        properties: Properties::from(HashMap::from([
            ("car_s".to_string(), json!(f64::from(i) * 0.7)),
            ("walk_s".to_string(), json!(f64::from(i) * 4.1)),
        ])),
    }
}

/// A small road network plus one impostor, with an update, a delete and an
/// embedding. Returns the ids left.
fn build(g: &NativeGraph) -> Vec<u64> {
    let ids: Vec<u64> = (0..6)
        .map(|i| g.create_node(junction(i)).unwrap().id)
        .collect();
    for w in ids.windows(2) {
        g.create_edge(road(w[0], w[1], 1)).unwrap();
    }
    let mut tx = g.begin();
    let extra = tx.create_node(junction(100)).unwrap();
    tx.create_edge(road(ids[5], extra.id, 2)).unwrap();
    tx.commit().unwrap();
    let imp = g.create_node(impostor()).unwrap();
    g.update_node(
        ids[0],
        NodePatch {
            title: Some("start".into()),
            ..NodePatch::default()
        },
    )
    .unwrap();
    g.set_embedding(imp.id, vec![0.25, -0.5, 1.0e-3, 7.0])
        .unwrap();
    g.delete_node(ids[1]).unwrap();
    let mut left: Vec<u64> = g.all_nodes().unwrap().iter().map(|n| n.id).collect();
    left.sort_unstable();
    left
}

fn file_bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn compact_formats() -> Vec<(WalFormat, u8)> {
    let mut v = vec![(WalFormat::CborCompact, b'K')];
    if WalFormat::CborCompactZstd.compiled_in() {
        v.push((WalFormat::CborCompactZstd, b'X'));
    }
    v
}

#[test]
fn the_formats_are_named() {
    assert_eq!(WalFormat::CborCompact.name(), "cbor-compact");
    assert!(WalFormat::CborCompact.compiled_in());
    assert_eq!(
        "cbor-compact".parse::<WalFormat>().unwrap(),
        WalFormat::CborCompact
    );
    assert_eq!(WalFormat::CborCompactZstd.name(), "cbor-compact-zstd");
    assert_eq!(
        WalFormat::CborCompactZstd.compiled_in(),
        cfg!(feature = "wal-zstd")
    );
}

#[test]
fn a_compact_log_recovers_exactly_what_was_written() {
    for (format, tag) in compact_formats() {
        let (dir, wal) = wal_path();
        let (ids, nodes, edges, embedding) = {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            let ids = build(&g);
            let imp = *ids.last().unwrap();
            (
                ids,
                g.all_nodes().unwrap(),
                g.all_edges().unwrap(),
                g.get_embedding(imp).unwrap(),
            )
        };
        let bytes = file_bytes(&wal);
        assert_eq!(
            &bytes[..8],
            &[b'D', b'R', b'V', b'W', b'A', b'L', 1, tag],
            "{format}"
        );

        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        let mut left: Vec<u64> = g.all_nodes().unwrap().iter().map(|n| n.id).collect();
        left.sort_unstable();
        assert_eq!(left, ids, "{format}");
        assert_eq!(
            g.all_nodes().unwrap(),
            nodes,
            "{format}: byte-identical nodes"
        );
        assert_eq!(g.all_edges().unwrap(), edges, "{format}");
        assert_eq!(g.get_embedding(*ids.last().unwrap()).unwrap(), embedding);
        // The impostor's properties came back as properties, names intact.
        let imp = g.get_node(*ids.last().unwrap()).unwrap().unwrap();
        assert_eq!(imp.properties.get("id"), Some(&json!("ticket-7")));
        assert_eq!(
            imp.properties.get("kind"),
            Some(&json!({"title": "nested", "properties": [1, 2]}))
        );
        assert_eq!(imp.properties.get("UpsertNode"), Some(&json!(true)));
        assert_eq!(imp.properties.get("ops"), Some(&json!([{"gid": "x"}])));
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_compact_log_of_small_records_is_much_smaller_than_cbor() {
    let size = |format| {
        let (dir, wal) = wal_path();
        {
            let g = NativeGraph::open_durable_with(&wal, format).unwrap();
            let ids: Vec<u64> = (0..300)
                .map(|i| g.create_node(junction(i)).unwrap().id)
                .collect();
            for (i, w) in ids.windows(2).enumerate() {
                g.create_edge(road(w[0], w[1], i as u32)).unwrap();
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
    let (cbor, compact) = (size(WalFormat::Cbor), size(WalFormat::CborCompact));
    // Measured on a real road graph: −37%.
    assert!(
        compact * 4 < cbor * 3,
        "cbor-compact {compact} vs cbor {cbor}"
    );
}

#[test]
fn compaction_switches_to_and_from_the_compact_formats() {
    let (dir, wal) = wal_path();
    {
        let g = NativeGraph::open_durable(&wal).unwrap(); // JSON
        build(&g);
    }
    let mut count = 0;
    for (format, tag) in compact_formats() {
        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        g.compact_wal().unwrap();
        assert_eq!(file_bytes(&wal)[7], tag, "{format}");
        g.create_node(junction(200)).unwrap();
        count = g.all_nodes().unwrap().len();
        drop(g);
    }
    // Any configured format opens the compact log, and compacts it back.
    let g = NativeGraph::open_durable_with(&wal, WalFormat::Cbor).unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), count);
    g.compact_wal().unwrap();
    assert_eq!(file_bytes(&wal)[7], b'C');
    drop(g);
    let g = NativeGraph::open_durable(&wal).unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), count);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_torn_compact_tail_is_dropped_and_the_log_reopens() {
    let (dir, wal) = wal_path();
    let ids = {
        let g = NativeGraph::open_durable_with(&wal, WalFormat::CborCompact).unwrap();
        build(&g)
    };
    let mut torn = file_bytes(&wal);
    torn.extend_from_slice(&900_u32.to_le_bytes());
    torn.extend_from_slice(&0_u32.to_le_bytes());
    torn.extend_from_slice(&[0x81, 0xa1, 0x00, 0xa9]);
    std::fs::write(&wal, &torn).unwrap();

    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborCompact).unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), ids.len());
    g.create_node(junction(300)).unwrap();
    drop(g);
    let g = NativeGraph::open_durable_with(&wal, WalFormat::CborCompact).unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), ids.len() + 1);
    drop(g);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_tailer_follows_a_compact_log_record_by_record() {
    for (format, _) in compact_formats() {
        let (dir, wal) = wal_path();
        let g = NativeGraph::open_durable_with(&wal, format).unwrap();
        let mut tailer = WalTailer::new(&wal);
        assert!(tailer.poll().unwrap().is_empty());
        let a = g.create_node(junction(1)).unwrap();
        let b = g.create_node(junction(2)).unwrap();
        g.create_edge(road(a.id, b.id, 3)).unwrap();
        let ops = tailer.poll().unwrap();
        assert_eq!(ops.len(), 3, "{format}: {ops:?}");
        let mirror = NativeGraph::new();
        mirror.apply_wal_ops(&ops).unwrap();
        assert_eq!(
            mirror.all_nodes().unwrap(),
            g.all_nodes().unwrap(),
            "{format}"
        );
        assert_eq!(
            mirror.all_edges().unwrap(),
            g.all_edges().unwrap(),
            "{format}"
        );
        drop(g);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(feature = "encryption")]
#[test]
fn an_encrypted_store_can_hold_a_compact_log() {
    use drevo_core::encryption::Key;
    let key = || Key::from_bytes([9; 32]);
    for (format, tag) in compact_formats() {
        let (dir, wal) = wal_path();
        let (ids, nodes) = {
            let g = NativeGraph::open_durable_encrypted(&wal, format, key()).unwrap();
            let ids = build(&g);
            (ids, g.all_nodes().unwrap())
        };
        assert_eq!(
            &file_bytes(&wal)[..8],
            &[b'D', b'R', b'V', b'W', b'A', b'L', 2, tag]
        );
        let g = NativeGraph::open_durable_encrypted(&wal, format, key()).unwrap();
        assert_eq!(g.all_nodes().unwrap(), nodes, "{format}");
        assert_eq!(g.all_nodes().unwrap().len(), ids.len());
        drop(g);
        assert!(NativeGraph::open_durable_with(&wal, format).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
