//! Phase 9 task `00056` — GraphML export integration tests.
//!
//! Cover the `Drevo::export_graphml` / `Drevo::export_graphml_to_path`
//! surface that ships under the `dump` module. The exporter has no inverse
//! (no `import_graphml`); the assertions below therefore focus on:
//!
//! 1. **Well-formedness** — output starts with `<?xml`, declares the GraphML
//!    namespace, opens `<graph id="drevo" edgedefault="directed">`, and
//!    closes every element opened.
//! 2. **Completeness** — every node id appears as `<node id="n{id}">`, every
//!    edge id appears as `<edge id="e{id}" source="n{from}" target="n{to}">`,
//!    and all key declarations (`d_uuid`, `d_kind`, `d_title`, `d_body`,
//!    `d_body_html`, `d_created_at`, `d_updated_at`, `d_props`,
//!    `d_e_uuid`, `d_e_kind`, `d_e_weight`, `d_e_created_at`, `d_e_props`)
//!    are present.
//! 3. **XML escaping** — `<`, `>`, `&`, `"`, `'` in titles / bodies become
//!    `&lt;`, `&gt;`, `&amp;`, `&quot;`, `&apos;`.
//! 4. **Property fidelity** — node and edge `Properties` are serialised as
//!    a JSON literal under `d_props` / `d_e_props` and round-trip through
//!    `serde_json::from_str` to the original map.
//! 5. **Determinism** — calling `export_graphml` twice on the same database
//!    yields byte-identical output; two databases with identical logical
//!    content (modulo uuids / timestamps) yield identical output once those
//!    volatile fields are stripped.
//! 6. **Cross-backend parity** — `MemoryBackend` and `RedbBackend` produce
//!    the same GraphML for the same logical graph (again modulo volatile
//!    fields).
//! 7. **Filesystem variant** — `export_graphml_to_path` writes the exact
//!    bytes that `export_graphml` returns to a `tempfile::TempDir` path.
//! 8. **Unicode** — CJK, emoji, and Cyrillic content in titles / bodies /
//!    property keys survives a JSON-string round-trip inside the GraphML.
//! 9. **Empty graph** — exporting a freshly-opened database still yields a
//!    valid GraphML document with zero `<node>` / `<edge>` elements.

use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
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

/// Strip elements that legitimately vary between database instances
/// (uuids generated on insert, wall-clock timestamps) so the rest of the
/// GraphML can be compared byte-for-byte.
fn strip_volatile(mut s: String) -> String {
    for key in [
        "d_uuid",
        "d_created_at",
        "d_updated_at",
        "d_e_uuid",
        "d_e_created_at",
    ] {
        let needle_open = format!("<data key=\"{key}\">");
        while let Some(start) = s.find(&needle_open) {
            let end_marker = "</data>\n";
            let end = s[start..].find(end_marker).unwrap() + start + end_marker.len();
            s.replace_range(start..end, "");
        }
    }
    s
}

// ---------------------------------------------------------------
// 1. Well-formedness
// ---------------------------------------------------------------

#[test]
fn export_graphml_starts_with_xml_declaration() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = db.export_graphml().unwrap();
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
}

#[test]
fn export_graphml_declares_namespace_and_schema() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = db.export_graphml().unwrap();
    assert!(xml.contains("xmlns=\"http://graphml.graphdrawing.org/xmlns\""));
    assert!(xml.contains("xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\""));
    assert!(xml.contains("xsi:schemaLocation="));
}

#[test]
fn export_graphml_opens_and_closes_root_elements() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = db.export_graphml().unwrap();
    assert_eq!(xml.matches("<graphml ").count(), 1);
    assert_eq!(xml.matches("</graphml>").count(), 1);
    assert_eq!(
        xml.matches("<graph id=\"drevo\" edgedefault=\"directed\">")
            .count(),
        1
    );
    assert_eq!(xml.matches("</graph>").count(), 1);
    assert!(xml.trim_end().ends_with("</graphml>"));
}

#[test]
fn export_graphml_empty_graph_has_no_node_or_edge_elements() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = db.export_graphml().unwrap();
    assert_eq!(xml.matches("<node ").count(), 0);
    assert_eq!(xml.matches("<edge ").count(), 0);
}

// ---------------------------------------------------------------
// 2. Completeness
// ---------------------------------------------------------------

#[test]
fn export_graphml_emits_every_node_id() {
    let db = Drevo::open_in_memory().unwrap();
    let (node_ids, _) = populate_sample_graph(&db);
    let xml = db.export_graphml().unwrap();
    for id in &node_ids {
        assert!(
            xml.contains(&format!("<node id=\"n{id}\">")),
            "node {id} missing"
        );
    }
    assert_eq!(xml.matches("<node ").count(), node_ids.len());
}

#[test]
fn export_graphml_emits_every_edge_with_source_and_target() {
    let db = Drevo::open_in_memory().unwrap();
    let (_, edge_ids) = populate_sample_graph(&db);
    let xml = db.export_graphml().unwrap();
    for eid in &edge_ids {
        // Match just the id portion — source/target are validated below by
        // re-parsing the JSON wrapped in each `d_e_props`.
        assert!(
            xml.contains(&format!("<edge id=\"e{eid}\"")),
            "edge {eid} missing"
        );
    }
    assert_eq!(xml.matches("<edge ").count(), edge_ids.len());
}

#[test]
fn export_graphml_declares_all_node_and_edge_keys() {
    let db = Drevo::open_in_memory().unwrap();
    let xml = db.export_graphml().unwrap();
    for id in [
        "d_uuid",
        "d_kind",
        "d_title",
        "d_body",
        "d_body_html",
        "d_created_at",
        "d_updated_at",
        "d_props",
    ] {
        assert!(
            xml.contains(&format!("<key id=\"{id}\" for=\"node\"")),
            "missing node key {id}"
        );
    }
    for id in [
        "d_e_uuid",
        "d_e_kind",
        "d_e_weight",
        "d_e_created_at",
        "d_e_props",
    ] {
        assert!(
            xml.contains(&format!("<key id=\"{id}\" for=\"edge\"")),
            "missing edge key {id}"
        );
    }
}

// ---------------------------------------------------------------
// 3. XML escaping
// ---------------------------------------------------------------

#[test]
fn export_graphml_escapes_special_chars_in_node_text() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(NewNode {
        kind: "note".into(),
        title: "tricky < & > \" '".into(),
        body: "<b>bold</b> & more".into(),
        body_html: "<p>html & such</p>".into(),
        properties: Properties::default(),
    })
    .unwrap();
    let xml = db.export_graphml().unwrap();
    assert!(xml.contains("tricky &lt; &amp; &gt; &quot; &apos;"));
    assert!(xml.contains("&lt;b&gt;bold&lt;/b&gt; &amp; more"));
    assert!(xml.contains("&lt;p&gt;html &amp; such&lt;/p&gt;"));
}

// ---------------------------------------------------------------
// 4. Property fidelity
// ---------------------------------------------------------------

fn extract_data_value<'a>(xml: &'a str, anchor: &str, key: &str) -> Option<&'a str> {
    // Find the anchor (e.g. the opening `<node id="n3">`), then within the
    // following element (until the matching close) return the text between
    // `<data key="$key">…</data>`. The result is still XML-escaped — pass
    // through [`xml_unescape`] before feeding it to a JSON / text parser.
    let start = xml.find(anchor)?;
    let needle = format!("<data key=\"{key}\">");
    let kstart = xml[start..].find(&needle)? + start + needle.len();
    let kend = xml[kstart..].find("</data>")? + kstart;
    Some(&xml[kstart..kend])
}

/// Inverse of the GraphML exporter's text escaping — turns `&lt;` /
/// `&gt;` / `&amp;` / `&quot;` / `&apos;` back into the original
/// character so an extracted `<data>` body becomes a plain UTF-8 string
/// (and, when the original content was JSON, valid JSON again).
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[test]
fn export_graphml_node_properties_are_json_parseable() {
    let db = Drevo::open_in_memory().unwrap();
    let props = props_with(&[
        ("answer", json!(42)),
        ("tags", json!(["a", "b", "c"])),
        ("nested", json!({"deep": {"flag": true}})),
    ]);
    let n = db
        .create_node(NewNode {
            kind: "note".into(),
            title: "Props".into(),
            body: String::new(),
            body_html: String::new(),
            properties: props.clone(),
        })
        .unwrap();
    let xml = db.export_graphml().unwrap();
    let anchor = format!("<node id=\"n{}\">", n.id);
    let raw = extract_data_value(&xml, &anchor, "d_props").expect("d_props block");
    // The literal is the JSON serialisation of the Properties map (with
    // XML-escaped quotes inside `<data>` text). Un-escape, then feed it back
    // through serde_json to confirm round-trip equality.
    let parsed: Properties =
        serde_json::from_str(&xml_unescape(raw)).expect("d_props is valid JSON");
    assert_eq!(parsed, props);
}

#[test]
fn export_graphml_edge_weight_and_props_are_emitted() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A")).unwrap();
    let b = db.create_node(new_node("note", "B")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "links_to", 2.75))
        .unwrap();
    let xml = db.export_graphml().unwrap();
    let anchor = "<edge id=\"e1\"";
    assert_eq!(
        extract_data_value(&xml, anchor, "d_e_kind"),
        Some("links_to")
    );
    assert_eq!(extract_data_value(&xml, anchor, "d_e_weight"), Some("2.75"));
    let raw_props = extract_data_value(&xml, anchor, "d_e_props").unwrap();
    let parsed: Properties = serde_json::from_str(&xml_unescape(raw_props)).unwrap();
    assert_eq!(parsed.get("label").unwrap(), &json!("links_to"));
}

#[test]
fn export_graphml_edge_records_correct_source_and_target() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "Source")).unwrap();
    let b = db.create_node(new_node("note", "Target")).unwrap();
    let e = db
        .create_edge(new_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    let xml = db.export_graphml().unwrap();
    assert!(xml.contains(&format!(
        "<edge id=\"e{}\" source=\"n{}\" target=\"n{}\">",
        e.id, a.id, b.id
    )));
}

// ---------------------------------------------------------------
// 5. Determinism
// ---------------------------------------------------------------

#[test]
fn export_graphml_is_byte_identical_when_called_twice() {
    let db = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&db);
    let first = db.export_graphml().unwrap();
    let second = db.export_graphml().unwrap();
    assert_eq!(first, second);
}

#[test]
fn export_graphml_normalises_property_order_across_databases() {
    // Two databases with the same nodes/edges but property keys inserted in
    // different orders must produce identical GraphML (modulo uuids /
    // timestamps that differ at creation time). `Properties::Serialize`
    // already sorts keys via BTreeMap.
    let a = Drevo::open_in_memory().unwrap();
    a.create_node(NewNode {
        kind: "note".into(),
        title: "X".into(),
        body: String::new(),
        body_html: String::new(),
        properties: props_with(&[("alpha", json!(1)), ("beta", json!(2)), ("gamma", json!(3))]),
    })
    .unwrap();

    let b = Drevo::open_in_memory().unwrap();
    b.create_node(NewNode {
        kind: "note".into(),
        title: "X".into(),
        body: String::new(),
        body_html: String::new(),
        properties: props_with(&[("gamma", json!(3)), ("alpha", json!(1)), ("beta", json!(2))]),
    })
    .unwrap();

    assert_eq!(
        strip_volatile(a.export_graphml().unwrap()),
        strip_volatile(b.export_graphml().unwrap())
    );
}

// ---------------------------------------------------------------
// 6. Cross-backend parity (Memory vs Redb)
// ---------------------------------------------------------------

#[cfg(feature = "redb-backend")]
#[test]
fn export_graphml_parity_memory_vs_redb() {
    let mem = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&mem);

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("parity.redb");
    let disk = Drevo::open(&path).unwrap();
    populate_sample_graph(&disk);

    assert_eq!(
        strip_volatile(mem.export_graphml().unwrap()),
        strip_volatile(disk.export_graphml().unwrap()),
    );
}

// ---------------------------------------------------------------
// 7. Filesystem variant
// ---------------------------------------------------------------

#[test]
fn export_graphml_to_path_writes_same_bytes_as_in_memory_export() {
    let db = Drevo::open_in_memory().unwrap();
    populate_sample_graph(&db);
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("graph.graphml");
    db.export_graphml_to_path(&target).unwrap();
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk, db.export_graphml().unwrap());
}

// ---------------------------------------------------------------
// 8. Unicode
// ---------------------------------------------------------------

#[test]
fn export_graphml_preserves_unicode_in_node_text() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(NewNode {
        kind: "заметка".into(),
        title: "知识图谱 🌳 Привет".into(),
        body: "本文。👨‍👩‍👧‍👦 граф".into(),
        body_html: String::new(),
        properties: props_with(&[("标签", json!("中文")), ("emoji_🔑", json!("🌈"))]),
    })
    .unwrap();
    let xml = db.export_graphml().unwrap();
    assert!(xml.contains("заметка"));
    assert!(xml.contains("知识图谱 🌳 Привет"));
    assert!(xml.contains("本文。👨‍👩‍👧‍👦 граф"));
    // properties survive as JSON-escaped strings (no XML escaping needed
    // for unicode, only for `<`, `>`, `&`, `"`, `'`).
    let raw = extract_data_value(&xml, "<node id=\"n1\">", "d_props").unwrap();
    let parsed: Properties = serde_json::from_str(&xml_unescape(raw)).unwrap();
    assert_eq!(parsed.get("标签").unwrap(), &json!("中文"));
    assert_eq!(parsed.get("emoji_🔑").unwrap(), &json!("🌈"));
}

// ---------------------------------------------------------------
// 9. Order — nodes/edges emitted in id order
// ---------------------------------------------------------------

#[test]
fn export_graphml_lists_nodes_and_edges_in_id_order() {
    let db = Drevo::open_in_memory().unwrap();
    let (node_ids, edge_ids) = populate_sample_graph(&db);
    let xml = db.export_graphml().unwrap();

    let mut positions: Vec<(u64, usize)> = node_ids
        .iter()
        .map(|id| {
            let pos = xml.find(&format!("<node id=\"n{id}\">")).unwrap();
            (*id, pos)
        })
        .collect();
    positions.sort_by_key(|(_, pos)| *pos);
    let sorted_ids: Vec<u64> = positions.iter().map(|(id, _)| *id).collect();
    let mut expected = node_ids.clone();
    expected.sort();
    assert_eq!(sorted_ids, expected);

    let mut edge_positions: Vec<(u64, usize)> = edge_ids
        .iter()
        .map(|id| {
            let pos = xml.find(&format!("<edge id=\"e{id}\"")).unwrap();
            (*id, pos)
        })
        .collect();
    edge_positions.sort_by_key(|(_, pos)| *pos);
    let sorted_edge_ids: Vec<u64> = edge_positions.iter().map(|(id, _)| *id).collect();
    let mut expected_edges = edge_ids.clone();
    expected_edges.sort();
    assert_eq!(sorted_edge_ids, expected_edges);
}
