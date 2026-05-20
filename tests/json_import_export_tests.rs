//! Phase 9 task `00055` — JSON import / export integration tests.
//!
//! These tests cover the `Drevo::export_json` / `Drevo::import_json` round-trip
//! contract that ships under the `dump` module (declared in `src/dump.rs`):
//!
//! 1. **Empty graph round-trip** — exporting an empty database and importing
//!    the result yields an empty database.
//! 2. **Single-node round-trip** — a single node with rich properties survives
//!    `export → import` byte-for-byte (id, uuid, timestamps, kind, body,
//!    body_html, JSON-typed properties).
//! 3. **Full graph round-trip** — a multi-kind graph (5+ nodes, mixed edges
//!    with weights and properties) survives a round-trip on both
//!    [`Drevo::open_in_memory`] and the disk-backed [`Drevo::open`].
//! 4. **Indexes rebuilt on import** — after `import_json`, every public lookup
//!    (`get_node_by_uuid`, `get_node_by_title`, `list_nodes_by_kind`,
//!    `edges_of`, `search_fts`, `list_recent`) returns results consistent with
//!    a freshly-built graph. Adjacency invariants
//!    ([`Drevo::verify_invariants`]) hold.
//! 5. **ID-counter restore** — after `import_json`, allocating a new node /
//!    edge yields an id strictly greater than every imported id (no
//!    collisions). This protects Phase 13 (MVCC) from "id reuse after
//!    backup-restore" anomalies.
//! 6. **File round-trip** — `export_json_to_path` + `import_json_from_path`
//!    work against `tempfile::TempDir`.
//! 7. **Format header** — exports include `format: "drevo-json-v1"`. Import
//!    rejects payloads with an unknown / missing format string with a typed
//!    error.
//! 8. **Malformed JSON** — `import_json("not json")` returns
//!    [`DrevoError::Io`] (the `serde_json` failure is mapped through `Io`).
//! 9. **Idempotent re-import into populated DB** — importing the same dump
//!    twice into the same DB does NOT duplicate nodes / edges (the second
//!    import is a no-op for IDs that already exist with byte-identical
//!    content). Conflicts produce [`DrevoError::DuplicateTitle`].
//! 10. **Cross-backend parity** — exporting from `MemoryBackend` and importing
//!     into `RedbBackend` (and vice versa) round-trips identically.
//!
//! All assertions use `English` test data per the project skill convention.

use drevo::db::Drevo;
use drevo::dump::DumpError;
use drevo::error::DrevoError;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn props_with(pairs: &[(&str, serde_json::Value)]) -> Properties {
    let mut map = HashMap::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    Properties::from(map)
}

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("Body of {title}"),
        body_html: format!("<p>Body of {title}</p>"),
        properties: props_with(&[("priority", json!(1)), ("tag", json!("test"))]),
    }
}

fn new_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight,
        properties: props_with(&[("label", json!(kind))]),
    }
}

fn populate_sample_graph(db: &Drevo) -> (Vec<u64>, Vec<u64>) {
    let n1 = db.create_node(new_node("note", "Alpha")).unwrap();
    let n2 = db.create_node(new_node("note", "Beta")).unwrap();
    let n3 = db.create_node(new_node("tag", "Gamma")).unwrap();
    let n4 = db.create_node(new_node("person", "Delta")).unwrap();
    let n5 = db.create_node(new_node("note", "Epsilon")).unwrap();

    let e1 = db
        .create_edge(new_edge(n1.id, n2.id, "links_to", 1.0))
        .unwrap();
    let e2 = db
        .create_edge(new_edge(n2.id, n3.id, "tagged_with", 0.5))
        .unwrap();
    let e3 = db
        .create_edge(new_edge(n4.id, n1.id, "authored", 2.0))
        .unwrap();
    let e4 = db
        .create_edge(new_edge(n1.id, n5.id, "links_to", 1.5))
        .unwrap();

    (
        vec![n1.id, n2.id, n3.id, n4.id, n5.id],
        vec![e1.id, e2.id, e3.id, e4.id],
    )
}

// ---------------------------------------------------------------
// 1. Empty graph round-trip
// ---------------------------------------------------------------

#[test]
fn export_empty_graph_is_well_formed() {
    let db = Drevo::open_in_memory().unwrap();
    let dump = db.export_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&dump).unwrap();
    assert_eq!(parsed["format"], "drevo-json-v1");
    assert!(parsed["nodes"].is_array());
    assert!(parsed["edges"].is_array());
    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 0);
    assert_eq!(parsed["edges"].as_array().unwrap().len(), 0);
}

#[test]
fn import_empty_graph_into_empty_db_is_noop() {
    let src = Drevo::open_in_memory().unwrap();
    let dst = Drevo::open_in_memory().unwrap();
    let dump = src.export_json().unwrap();
    let report = dst.import_json(&dump).unwrap();
    assert_eq!(report.nodes_imported, 0);
    assert_eq!(report.edges_imported, 0);
    assert!(dst.verify_invariants().unwrap().is_empty());
}

// ---------------------------------------------------------------
// 2. Single-node round-trip preserves every field
// ---------------------------------------------------------------

#[test]
fn single_node_round_trip_preserves_every_field() {
    let src = Drevo::open_in_memory().unwrap();
    let original = src
        .create_node(NewNode {
            kind: "concept".to_string(),
            title: "Single rich node".to_string(),
            body: "# Heading\n\nBody with **markdown**.".to_string(),
            body_html: "<h1>Heading</h1><p>Body with <strong>markdown</strong>.</p>".to_string(),
            properties: props_with(&[
                ("priority", json!(7)),
                ("nested", json!({"a": 1, "b": [true, false]})),
                ("array", json!([1, 2, 3])),
                ("text", json!("value with unicode 你好 emoji 🦀")),
            ]),
        })
        .unwrap();

    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_json(&dump).unwrap();
    assert_eq!(report.nodes_imported, 1);
    assert_eq!(report.edges_imported, 0);

    let restored = dst.get_node(original.id).unwrap().expect("node missing");
    assert_eq!(restored, original);
}

// ---------------------------------------------------------------
// 3. Full graph round-trip on MemoryBackend
// ---------------------------------------------------------------

#[test]
fn full_graph_round_trip_in_memory() {
    let src = Drevo::open_in_memory().unwrap();
    let (node_ids, edge_ids) = populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_json(&dump).unwrap();
    assert_eq!(report.nodes_imported, node_ids.len());
    assert_eq!(report.edges_imported, edge_ids.len());

    for id in &node_ids {
        let src_node = src.get_node(*id).unwrap().unwrap();
        let dst_node = dst.get_node(*id).unwrap().unwrap();
        assert_eq!(src_node, dst_node, "node {id} mismatch");
    }
    for id in &edge_ids {
        let src_edge = src.get_edge(*id).unwrap().unwrap();
        let dst_edge = dst.get_edge(*id).unwrap().unwrap();
        assert_eq!(src_edge, dst_edge, "edge {id} mismatch");
    }
}

// ---------------------------------------------------------------
// 3b. Round-trip on disk-backed RedbBackend
// ---------------------------------------------------------------

#[test]
fn full_graph_round_trip_on_disk_redb_backend() {
    let dir = TempDir::new().unwrap();
    let src_path = dir.path().join("src.db");
    let dst_path = dir.path().join("dst.db");

    let dump = {
        let src = Drevo::open(&src_path).unwrap();
        let (_nodes, _edges) = populate_sample_graph(&src);
        let dump = src.export_json().unwrap();
        src.close().unwrap();
        dump
    };

    {
        let dst = Drevo::open(&dst_path).unwrap();
        let report = dst.import_json(&dump).unwrap();
        assert_eq!(report.nodes_imported, 5);
        assert_eq!(report.edges_imported, 4);
        assert!(dst.verify_invariants().unwrap().is_empty());
        dst.close().unwrap();
    }
}

// ---------------------------------------------------------------
// 4. Indexes are fully rebuilt on import
// ---------------------------------------------------------------

#[test]
fn import_rebuilds_kind_index() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    let notes = dst.list_nodes_by_kind("note", 100, 0).unwrap();
    assert_eq!(notes.len(), 3, "expected 3 notes after import");
}

#[test]
fn import_rebuilds_title_index() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    let alpha = dst.get_node_by_title("Alpha").unwrap();
    assert!(alpha.is_some(), "title index missing for 'Alpha'");
}

#[test]
fn import_rebuilds_uuid_index() {
    let src = Drevo::open_in_memory().unwrap();
    let (node_ids, _) = populate_sample_graph(&src);
    let src_node = src.get_node(node_ids[0]).unwrap().unwrap();
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    let lookup = dst.get_node_by_uuid(&src_node.uuid).unwrap();
    assert!(lookup.is_some(), "uuid index missing for first node");
    assert_eq!(lookup.unwrap().id, src_node.id);
}

#[test]
fn import_rebuilds_adjacency_lists() {
    let src = Drevo::open_in_memory().unwrap();
    let (node_ids, _) = populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    // Alpha (n1) has out_edges to Beta and Epsilon (2 outgoing), and an
    // incoming edge from Delta (authored). Both is 3 total.
    let edges = dst.edges_of(node_ids[0], Direction::Both).unwrap();
    assert_eq!(edges.len(), 3, "Alpha adjacency mismatch");
    let out = dst.edges_of(node_ids[0], Direction::Outgoing).unwrap();
    assert_eq!(out.len(), 2);
    let inc = dst.edges_of(node_ids[0], Direction::Incoming).unwrap();
    assert_eq!(inc.len(), 1);
}

#[test]
fn import_rebuilds_fts_index() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    // "Alpha" and "Body of Alpha" both contain the trigrams of "alpha"
    let results = dst.search_fts("alpha", 10).unwrap();
    assert!(
        !results.is_empty(),
        "FTS index should yield results for 'alpha'"
    );
}

#[test]
fn import_rebuilds_updated_index() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    let recent = dst.list_recent(100).unwrap();
    assert_eq!(recent.len(), 5, "list_recent should see all imported nodes");
}

#[test]
fn import_preserves_invariants() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();
    let violations = dst.verify_invariants().unwrap();
    assert!(
        violations.is_empty(),
        "violations after import: {violations:?}"
    );
}

// ---------------------------------------------------------------
// 5. ID-counter restoration
// ---------------------------------------------------------------

#[test]
fn import_restores_id_counters_above_imported_ids() {
    let src = Drevo::open_in_memory().unwrap();
    let (node_ids, edge_ids) = populate_sample_graph(&src);
    let max_node = *node_ids.iter().max().unwrap();
    let max_edge = *edge_ids.iter().max().unwrap();
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();

    let next_node = dst.alloc_node_id();
    let next_edge = dst.alloc_edge_id();
    assert!(
        next_node > max_node,
        "next node id {next_node} must exceed max imported {max_node}"
    );
    assert!(
        next_edge > max_edge,
        "next edge id {next_edge} must exceed max imported {max_edge}"
    );
}

// ---------------------------------------------------------------
// 6. File round-trip
// ---------------------------------------------------------------

#[test]
fn export_then_import_through_file() {
    let dir = TempDir::new().unwrap();
    let dump_path = dir.path().join("dump.json");

    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    src.export_json_to_path(&dump_path).unwrap();

    assert!(dump_path.exists(), "dump file should be written");
    let bytes = std::fs::read(&dump_path).unwrap();
    assert!(bytes.starts_with(b"{"), "dump file should be JSON");

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_json_from_path(&dump_path).unwrap();
    assert_eq!(report.nodes_imported, 5);
    assert_eq!(report.edges_imported, 4);
}

// ---------------------------------------------------------------
// 7. Format header
// ---------------------------------------------------------------

#[test]
fn import_rejects_unknown_format() {
    let dst = Drevo::open_in_memory().unwrap();
    let bad = r#"{"format":"unknown-v999","nodes":[],"edges":[]}"#;
    let err = dst.import_json(bad).unwrap_err();
    match err {
        DrevoError::Io(_) => { /* ok — UnsupportedFormat is surfaced as Io */ }
        other => panic!("expected Io(UnsupportedFormat), got {other:?}"),
    }
}

#[test]
fn import_rejects_missing_format() {
    let dst = Drevo::open_in_memory().unwrap();
    let bad = r#"{"nodes":[],"edges":[]}"#;
    let err = dst.import_json(bad).unwrap_err();
    match err {
        DrevoError::Io(_) => {}
        other => panic!("expected Io for missing format, got {other:?}"),
    }
}

// ---------------------------------------------------------------
// 8. Malformed JSON
// ---------------------------------------------------------------

#[test]
fn import_rejects_malformed_json() {
    let dst = Drevo::open_in_memory().unwrap();
    let err = dst.import_json("not json at all").unwrap_err();
    match err {
        DrevoError::Io(_) => {}
        other => panic!("expected Io for malformed JSON, got {other:?}"),
    }
}

#[test]
fn dump_error_can_be_inspected() {
    // Ensure DumpError is a public, debuggable error type that round-trips
    // through DrevoError::Io without panicking.
    let err = DumpError::UnsupportedFormat("xxx".to_string());
    let drevo_err: DrevoError = err.into();
    assert!(matches!(drevo_err, DrevoError::Io(_)));
}

// ---------------------------------------------------------------
// 9. Idempotent re-import / conflicts
// ---------------------------------------------------------------

#[test]
fn re_importing_identical_dump_is_idempotent() {
    let src = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    let r1 = dst.import_json(&dump).unwrap();
    let r2 = dst.import_json(&dump).unwrap();
    assert_eq!(r1.nodes_imported, 5);
    assert_eq!(r1.edges_imported, 4);
    assert_eq!(
        r2.nodes_skipped, 5,
        "second import should skip identical nodes"
    );
    assert_eq!(
        r2.edges_skipped, 4,
        "second import should skip identical edges"
    );
    assert_eq!(r2.nodes_imported, 0);
    assert_eq!(r2.edges_imported, 0);
    assert!(dst.verify_invariants().unwrap().is_empty());

    // Total node count unchanged
    let kinds = ["note", "tag", "person"];
    let total: usize = kinds
        .iter()
        .map(|k| dst.list_nodes_by_kind(k, 100, 0).unwrap().len())
        .sum();
    assert_eq!(total, 5);
}

#[test]
fn import_into_populated_db_with_title_conflict_yields_duplicate_title() {
    // Source: bump counter to id 10, then create "Conflict" at id 10.
    let src = Drevo::open_in_memory().unwrap();
    for _ in 0..9 {
        src.alloc_node_id();
    }
    let conflict = src.create_node(new_node("note", "Conflict")).unwrap();
    assert_eq!(
        conflict.id, 10,
        "test prerequisite: source 'Conflict' id is 10"
    );
    let dump = src.export_json().unwrap();

    // Dst: a single node with the same title but a different id (1, not 10).
    // No id collision possible — the imported row lands at id 10, but its
    // title is already owned by id 1, so `DuplicateTitle` wins.
    let dst = Drevo::open_in_memory().unwrap();
    dst.create_node(new_node("note", "Conflict")).unwrap();

    let err = dst.import_json(&dump).unwrap_err();
    assert!(
        matches!(err, DrevoError::DuplicateTitle(_)),
        "expected DuplicateTitle, got {err:?}"
    );
}

#[test]
fn import_into_populated_db_with_id_collision_yields_io_error() {
    let src = Drevo::open_in_memory().unwrap();
    src.create_node(new_node("note", "Original")).unwrap();
    let dump = src.export_json().unwrap();

    // Dst already has a different node at id 1.
    let dst = Drevo::open_in_memory().unwrap();
    dst.create_node(new_node("note", "Different at id 1"))
        .unwrap();

    let err = dst.import_json(&dump).unwrap_err();
    match err {
        DrevoError::Io(io) => {
            assert!(io.to_string().contains("id collision"), "got: {io}");
        }
        other => panic!("expected Io(id collision), got {other:?}"),
    }
}

// ---------------------------------------------------------------
// 10. Cross-backend parity
// ---------------------------------------------------------------

#[test]
fn round_trip_memory_to_redb_preserves_graph() {
    let dir = TempDir::new().unwrap();
    let dst_path = dir.path().join("imported.db");

    let src = Drevo::open_in_memory().unwrap();
    let (node_ids, edge_ids) = populate_sample_graph(&src);
    let dump = src.export_json().unwrap();

    let dst = Drevo::open(&dst_path).unwrap();
    dst.import_json(&dump).unwrap();
    for id in &node_ids {
        let original = src.get_node(*id).unwrap().unwrap();
        let restored = dst.get_node(*id).unwrap().unwrap();
        assert_eq!(restored, original, "node {id} differs across backends");
    }
    for id in &edge_ids {
        let original = src.get_edge(*id).unwrap().unwrap();
        let restored = dst.get_edge(*id).unwrap().unwrap();
        assert_eq!(restored, original, "edge {id} differs across backends");
    }
    dst.close().unwrap();
}

#[test]
fn round_trip_redb_to_memory_preserves_graph() {
    let dir = TempDir::new().unwrap();
    let src_path = dir.path().join("source.db");

    let dump = {
        let src = Drevo::open(&src_path).unwrap();
        populate_sample_graph(&src);
        let dump = src.export_json().unwrap();
        src.close().unwrap();
        dump
    };

    let dst = Drevo::open_in_memory().unwrap();
    let report = dst.import_json(&dump).unwrap();
    assert_eq!(report.nodes_imported, 5);
    assert_eq!(report.edges_imported, 4);
    assert!(dst.verify_invariants().unwrap().is_empty());
}

// ---------------------------------------------------------------
// Edge-case coverage
// ---------------------------------------------------------------

#[test]
fn export_is_pretty_printed_and_human_readable() {
    let src = Drevo::open_in_memory().unwrap();
    src.create_node(new_node("note", "Readable")).unwrap();
    let dump = src.export_json().unwrap();
    // Pretty-printed JSON contains newlines and 2-space indentation
    assert!(dump.contains('\n'), "dump should be multi-line");
    assert!(
        dump.contains("\"format\""),
        "dump should mention the format field"
    );
}

#[test]
fn import_preserves_edge_weight_and_properties() {
    let src = Drevo::open_in_memory().unwrap();
    let a = src.create_node(new_node("note", "A")).unwrap();
    let b = src.create_node(new_node("note", "B")).unwrap();
    let edge = src
        .create_edge(NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "weighted".to_string(),
            weight: std::f32::consts::PI,
            properties: props_with(&[("note", json!("custom"))]),
        })
        .unwrap();
    let dump = src.export_json().unwrap();

    let dst = Drevo::open_in_memory().unwrap();
    dst.import_json(&dump).unwrap();
    let restored = dst.get_edge(edge.id).unwrap().unwrap();
    assert_eq!(restored, edge);
    // Edge weight is bit-equal (f32::PI round-trips through JSON for this value).
    assert!((restored.weight - std::f32::consts::PI).abs() < f32::EPSILON);
}
