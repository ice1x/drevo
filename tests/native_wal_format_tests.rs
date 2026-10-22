//! `DREVO_WAL_FORMAT` end to end through `NativeService` (issue #582): a store
//! opened while a binary format is the default is written in it, an existing
//! JSON store is converted at open, and the data survives every switch.
//!
//! One test per binary: the default format is process-wide.

#![cfg(all(feature = "format-cbor", feature = "format-msgpack"))]

use std::collections::HashMap;

use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;
use drevo::wal_format::WalFormat;

fn run(svc: &NativeService, q: &str) -> drevo::cypher::executor::ExecResult {
    svc.execute(&parse(q).unwrap(), HashMap::new()).unwrap()
}

#[test]
fn switching_the_wal_format_converts_existing_stores_at_open() {
    let dir = std::env::temp_dir().join(format!("drevo_wal_fmt_svc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    let count = |svc: &NativeService| run(svc, "MATCH (t:Ticket) RETURN count(t) AS c").rows;

    // A bug tracker written with the default JSON log.
    {
        let svc = NativeService::open(&wal).unwrap();
        run(
            &svc,
            "UNWIND range(1, 30) AS i CREATE (:Ticket {title: 'bug-' + toString(i), \
             severity: i % 3, vec: [x IN range(1, 32) | toFloat(x) / 7.0]})",
        );
    }
    assert!(!std::fs::read(&wal).unwrap().starts_with(b"DRVWAL"));

    for (format, header) in [
        (WalFormat::Cbor, Some(b'C')),
        (WalFormat::MsgPack, Some(b'M')),
        (WalFormat::Json, None),
    ] {
        NativeService::set_default_wal_format(format);
        let svc = NativeService::open(&wal).unwrap();
        let bytes = std::fs::read(&wal).unwrap();
        match header {
            Some(tag) => assert_eq!(&bytes[..8], &[b'D', b'R', b'V', b'W', b'A', b'L', 1, tag]),
            None => assert!(!bytes.starts_with(b"DRVWAL")),
        }
        assert_eq!(
            count(&svc),
            vec![vec![drevo::cypher::executor::Value::Integer(30)]]
        );
        // Writes after the switch land in the new format and survive reopen.
        run(
            &svc,
            &format!("CREATE (:Ticket {{title: 'after-{format}'}})"),
        );
        drop(svc);
        let svc = NativeService::open(&wal).unwrap();
        let titles = run(
            &svc,
            &format!("MATCH (t:Ticket {{title: 'after-{format}'}}) RETURN t.title"),
        );
        assert_eq!(titles.rows.len(), 1, "{format}");
        run(
            &svc,
            &format!("MATCH (t:Ticket {{title: 'after-{format}'}}) DELETE t"),
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "wal-zstd")]
#[test]
fn switching_to_cbor_zstd_compresses_the_store_at_open() {
    let dir = std::env::temp_dir().join(format!("drevo_wal_zstd_svc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    let count = |svc: &NativeService| run(svc, "MATCH (t:Ticket) RETURN count(t) AS c").rows;

    // A bug tracker written with the default JSON log: every ticket repeats
    // the same component and status strings.
    {
        let svc = NativeService::open(&wal).unwrap();
        run(
            &svc,
            "UNWIND range(1, 60) AS i CREATE (:Ticket {title: 'bug-' + toString(i), \
             component: 'editor/save-dialog', status: 'open', severity: i % 3, \
             vec: [x IN range(1, 32) | toFloat(x) / 7.0]})",
        );
    }
    let json_bytes = std::fs::read(&wal).unwrap().len();

    NativeService::set_default_wal_format(WalFormat::CborZstd);
    let svc = NativeService::open(&wal).unwrap();
    let bytes = std::fs::read(&wal).unwrap();
    assert_eq!(&bytes[..8], b"DRVWAL\x01Z");
    assert!(
        bytes.len() * 2 < json_bytes,
        "cbor-zstd {} vs json {json_bytes}",
        bytes.len()
    );
    assert_eq!(
        count(&svc),
        vec![vec![drevo::cypher::executor::Value::Integer(60)]]
    );
    run(
        &svc,
        "CREATE (:Ticket {title: 'after-cbor-zstd', status: 'open'})",
    );
    drop(svc);
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(
        count(&svc),
        vec![vec![drevo::cypher::executor::Value::Integer(61)]]
    );
    NativeService::set_default_wal_format(WalFormat::Json);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn switching_to_cbor_compact_shrinks_a_store_of_small_records_at_open() {
    let dir = std::env::temp_dir().join(format!("drevo_wal_compact_svc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    let count = |svc: &NativeService| run(svc, "MATCH (j:Junction) RETURN count(j) AS c").rows;

    // A road network: tiny nodes, whose bytes are mostly field names.
    NativeService::set_default_wal_format(WalFormat::Cbor);
    {
        let svc = NativeService::open(&wal).unwrap();
        run(
            &svc,
            "UNWIND range(1, 400) AS i CREATE (:Junction {lat: 42.65 + i / 1e5, \
             lon: 23.32 + i / 1e5, osm_id: 3060000000 + i})",
        );
        run(
            &svc,
            "MATCH (a:Junction), (b:Junction) WHERE b.osm_id = a.osm_id + 1 \
             CREATE (a)-[:ROAD {car_s: 1.5, walk_s: 9.0}]->(b)",
        );
        svc.compact().unwrap();
    }
    let cbor_bytes = std::fs::read(&wal).unwrap().len();

    NativeService::set_default_wal_format(WalFormat::CborCompact);
    let svc = NativeService::open(&wal).unwrap();
    let bytes = std::fs::read(&wal).unwrap();
    assert_eq!(&bytes[..8], b"DRVWAL\x01K");
    assert!(
        bytes.len() * 4 < cbor_bytes * 3,
        "cbor-compact {} vs cbor {cbor_bytes}",
        bytes.len()
    );
    assert_eq!(
        count(&svc),
        vec![vec![drevo::cypher::executor::Value::Integer(400)]]
    );
    run(&svc, "CREATE (:Junction {lat: 1.0, lon: 2.0, osm_id: 1})");
    drop(svc);
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(
        count(&svc),
        vec![vec![drevo::cypher::executor::Value::Integer(401)]]
    );
    NativeService::set_default_wal_format(WalFormat::Json);
    let _ = std::fs::remove_dir_all(&dir);
}
