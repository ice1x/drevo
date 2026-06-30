//! Integration tests for special characters *inside stored content*.
//!
//! The existing suites cover special characters where they are *interpreted*:
//! the Cypher lexer's escape handling (`cypher_lexer_tests`), FTS tokenization
//! and normalization (`fts_tokenizer_tests`, `fts_recall_tests`,
//! `proptest_fts_tokenizer`), GraphML XML-escaping (`graphml_export_tests`),
//! Bolt byte-length encoding (`bolt_packstream_tests`) and the FFI UTF-8 fence
//! (`ffi_tests`). What was *not* pinned down is the storage layer's promise to
//! return arbitrary content **byte-for-byte** regardless of what control bytes,
//! NUL bytes, exotic whitespace, injection-shaped text, or oversized payloads
//! the caller puts in a title / body / property value.
//!
//! These are characterization tests: drevo stores strings as UTF-8 and
//! properties as `serde_json` values, so a faithful round-trip is the
//! contract. Any failure here is a real storage bug, not a flaky expectation.
//!
//! Gap origin: the special-character coverage audit (2026-06-29) flagged NUL
//! bytes, control characters beyond `\n\r\t`, very long titles/bodies,
//! adversarial/injection-shaped content, Unicode whitespace and Cypher string
//! parameters carrying special characters as untested categories.

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
use serde_json::json;
use std::collections::HashMap;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

/// Create a node whose title and body both carry `content`, read it back by id,
/// and assert every text field survives verbatim.
fn round_trip_text(content: &str) {
    let db = db();
    let mut props = HashMap::new();
    props.insert("note".to_string(), json!(content));
    let created = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: content.to_string(),
            body: content.to_string(),
            body_html: content.to_string(),
            properties: Properties::from(props),
        })
        .expect("create_node");

    let fetched = db
        .get_node(created.id)
        .expect("get_node")
        .expect("node exists");

    assert_eq!(fetched.title, content, "title must round-trip verbatim");
    assert_eq!(fetched.body, content, "body must round-trip verbatim");
    assert_eq!(
        fetched.body_html, content,
        "body_html must round-trip verbatim"
    );
    assert_eq!(
        fetched.properties.get("note"),
        Some(&json!(content)),
        "string property must round-trip verbatim"
    );
}

// ===== NUL bytes ============================================================

#[test]
fn nul_byte_in_the_middle_of_content_round_trips() {
    round_trip_text("before\u{0}after");
}

#[test]
fn leading_and_trailing_nul_bytes_round_trip() {
    round_trip_text("\u{0}wrapped\u{0}");
}

#[test]
fn content_that_is_only_a_nul_byte_round_trips() {
    round_trip_text("\u{0}");
}

#[test]
fn run_of_nul_bytes_round_trips() {
    round_trip_text("\u{0}\u{0}\u{0}\u{0}");
}

// ===== Control characters beyond \n \r \t ===================================

#[test]
fn c0_control_characters_round_trip() {
    // Backspace, vertical tab, form feed, escape, unit separator, delete.
    round_trip_text("a\u{8}b\u{B}c\u{C}d\u{1B}e\u{1F}f\u{7F}g");
}

#[test]
fn every_c0_control_character_round_trips() {
    // The full C0 block 0x01..=0x1F plus DEL 0x7F, interleaved with markers
    // so a silent truncation at any control byte is caught.
    let mut s = String::new();
    for code in 0x01u32..=0x1F {
        s.push('x');
        s.push(char::from_u32(code).unwrap());
    }
    s.push('x');
    s.push('\u{7F}');
    round_trip_text(&s);
}

#[test]
fn c1_control_characters_round_trip() {
    // C1 controls (U+0080..=U+009F) are multi-byte in UTF-8.
    round_trip_text("a\u{80}b\u{85}c\u{9F}d");
}

// ===== Unicode whitespace ===================================================

#[test]
fn unicode_whitespace_in_content_round_trips() {
    // Non-breaking space, en quad, em space, ideographic space, narrow NBSP,
    // line/paragraph separators, zero-width space.
    round_trip_text("a\u{A0}b\u{2000}c\u{3000}d\u{202F}e\u{2028}f\u{2029}g\u{200B}h");
}

// ===== Very long content ====================================================

#[test]
fn megabyte_title_and_body_round_trip() {
    // 1 MiB of repeated multibyte text — exercises large-value storage paths
    // that the FTS panic-guard test (100K chars, tokenizer-only) does not.
    let big = "λ ".repeat(512 * 1024); // ~1.5 MiB UTF-8
    round_trip_text(&big);
}

#[test]
fn long_property_array_round_trips() {
    let db = db();
    let items: Vec<String> = (0..2000).map(|i| format!("item-{i}-λ")).collect();
    let mut props = HashMap::new();
    props.insert("items".to_string(), json!(items));
    let created = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "big-array".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::from(props),
        })
        .expect("create_node");
    let fetched = db.get_node(created.id).unwrap().unwrap();
    assert_eq!(fetched.properties.get("items"), Some(&json!(items)));
}

// ===== Injection-shaped content (stored as data, never executed) ============

#[test]
fn cypher_injection_shaped_content_is_stored_as_inert_data() {
    // A title that looks like a Cypher statement must be stored verbatim and
    // must NOT create extra nodes or otherwise be interpreted.
    let payload = "x'}) DETACH DELETE n; MATCH (m) DETACH DELETE m //";
    let db = db();
    let created = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: payload.to_string(),
            body: payload.to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .expect("create_node");
    let fetched = db.get_node(created.id).unwrap().unwrap();
    assert_eq!(fetched.title, payload);
    assert_eq!(fetched.body, payload);
    assert_eq!(
        db.list_recent(usize::MAX).unwrap().len(),
        1,
        "no phantom nodes created"
    );
}

#[test]
fn quote_and_backslash_heavy_content_round_trips() {
    round_trip_text(r#"he said "it's \"fine\"" \ \\ \n not-a-newline 'single'"#);
}

#[test]
fn sql_and_template_injection_shaped_content_round_trips() {
    round_trip_text("'; DROP TABLE nodes;-- ${env:SECRET} {{7*7}} <script>alert(1)</script>");
}

// ===== Special characters on edges ==========================================

#[test]
fn edge_properties_with_special_chars_round_trip() {
    let db = db();
    let a = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "a".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    let b = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "b".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let weird = "edge\u{0}\u{1B}\u{7F}\u{A0}note '\"\\";
    let mut props = HashMap::new();
    props.insert("label".to_string(), json!(weird));
    let edge = db
        .create_edge(NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::from(props),
        })
        .expect("create_edge");

    let fetched = db.get_edge(edge.id).unwrap().unwrap();
    assert_eq!(fetched.properties.get("label"), Some(&json!(weird)));
}

// ===== Cypher string parameters carrying special characters =================

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

#[test]
fn cypher_string_parameter_with_special_chars_passes_through() {
    let db = db();
    let payload = "line1\nline2\ttab \u{0} nul \u{1B} esc 'q' \"dq\" \\ back ❤ λ";
    let mut params = HashMap::new();
    params.insert("p".to_string(), Value::String(payload.to_string()));
    let rows = run_with("RETURN $p AS x", &db, params);
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::String(s) => assert_eq!(s, payload, "parameter must pass through verbatim"),
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn cypher_create_with_special_char_parameter_persists_verbatim() {
    let db = db();
    let payload = "title with \u{0} nul, \u{1B} esc, '\"\\ and ❤";
    let mut params = HashMap::new();
    params.insert("t".to_string(), Value::String(payload.to_string()));
    let rows = run_with(
        "CREATE (n:Note {title: $t}) RETURN n.title AS title",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::String(s) => assert_eq!(s, payload),
        other => panic!("expected String, got {other:?}"),
    }
    // And it must be the title actually persisted to storage.
    let node = db.list_nodes_by_kind("Note", 10, 0).unwrap();
    assert_eq!(node.len(), 1);
    assert_eq!(node[0].title, payload);
}
