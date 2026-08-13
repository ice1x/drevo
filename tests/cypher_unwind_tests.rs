//! End-to-end Cypher `UNWIND` tests — Phase 10 follow-up task `00135`.
//!
//! `UNWIND list AS x` expands a list expression into one row per
//! element, carrying every existing binding forward. It is the
//! complement of `collect` (list → rows, where aggregation does
//! rows → list) and the join point that lets a single query both
//! read graph data and iterate a caller-supplied / derived list.
//!
//! These cases drive the real parser → executor pipeline across the
//! five drevo target scenario domains (CBT journal, story/book
//! editor, IT task manager, ERP, bug tracker) plus the cross-cutting
//! semantics (empty/null expansion, row multiplication, downstream
//! aggregation, feeding `CREATE`).

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_with_params(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({},{}), got {:?}", r, c, other),
    }
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> String {
    match &rows[r][c] {
        Value::String(s) => s.clone(),
        other => panic!("expected string at ({},{}), got {:?}", r, c, other),
    }
}

// ===== Core semantics =======================================================

#[test]
fn unwind_standalone_expands_literal_list() {
    let db = db();
    let rows = run("UNWIND [10, 20, 30] AS x RETURN x", &db);
    assert_eq!(rows.len(), 3);
    assert_eq!(int(&rows, 0, 0), 10);
    assert_eq!(int(&rows, 1, 0), 20);
    assert_eq!(int(&rows, 2, 0), 30);
}

#[test]
fn unwind_empty_list_produces_no_rows() {
    let db = db();
    assert!(run("UNWIND [] AS x RETURN x", &db).is_empty());
}

#[test]
fn unwind_null_produces_no_rows() {
    let db = db();
    assert!(run("UNWIND null AS x RETURN x", &db).is_empty());
}

#[test]
fn unwind_scalar_is_rejected_with_type_mismatch() {
    let db = db();
    let e = run_err("UNWIND 7 AS x RETURN x", &db);
    assert!(
        matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "List"),
        "got {:?}",
        e
    );
}

#[test]
fn unwind_then_collect_round_trips_the_list() {
    let db = db();
    // UNWIND is the inverse of collect: expand to rows then fold back.
    let rows = run(
        "UNWIND [1, 2, 3, 4] AS x WITH x WHERE x % 2 = 0 RETURN collect(x) AS evens",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Value::List(vec![Value::Integer(2), Value::Integer(4)])
    );
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_unwind_tags_creates_one_entry_per_tag() {
    let db = db();
    run(
        "UNWIND ['anxious', 'calm', 'hopeful'] AS mood \
         CREATE (:JournalEntry {title: mood, mood: mood})",
        &db,
    );
    let entries = db.list_nodes_by_kind("JournalEntry", 100, 0).unwrap();
    assert_eq!(entries.len(), 3);
}

// ===== Scenario 2 — story / book editor =====================================

#[test]
fn story_unwind_chapters_links_each_to_the_book() {
    let db = db();
    run("CREATE (:Book {title: 'The Long Road'})", &db);
    // Create three chapters and link each back to the book in one query.
    run(
        "MATCH (b:Book {title: 'The Long Road'}) \
         UNWIND ['Ch1', 'Ch2', 'Ch3'] AS ch \
         CREATE (b)-[:HAS_CHAPTER]->(:Chapter {title: ch})",
        &db,
    );
    let chapters = db.list_nodes_by_kind("Chapter", 100, 0).unwrap();
    assert_eq!(chapters.len(), 3);
    let rows = run(
        "MATCH (b:Book)-[:HAS_CHAPTER]->(c:Chapter) RETURN count(*) AS n",
        &db,
    );
    assert_eq!(int(&rows, 0, 0), 3);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_unwind_assignees_multiplies_matched_tasks() {
    let db = db();
    run("CREATE (:Task {title: 'Ship release'})", &db);
    run("CREATE (:Task {title: 'Write changelog'})", &db);
    // Each matched task × each assignee from the list.
    let rows = run(
        "MATCH (t:Task) UNWIND ['alice', 'bob'] AS who \
         RETURN t.title AS task, who ORDER BY task, who",
        &db,
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(string(&rows, 0, 0), "Ship release");
    assert_eq!(string(&rows, 0, 1), "alice");
    assert_eq!(string(&rows, 3, 0), "Write changelog");
    assert_eq!(string(&rows, 3, 1), "bob");
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_unwind_line_items_from_parameter_then_aggregate() {
    let db = db();
    let mut params = HashMap::new();
    params.insert(
        "amounts".into(),
        Value::List(vec![
            Value::Integer(100),
            Value::Integer(250),
            Value::Integer(75),
        ]),
    );
    // A caller passes order-line amounts; UNWIND + sum totals them.
    let rows = run_with_params(
        "UNWIND $amounts AS amt RETURN sum(amt) AS total, count(*) AS lines",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 425);
    assert_eq!(int(&rows, 0, 1), 3);
}

// ===== Scenario 5 — bug tracker =============================================

#[test]
fn bug_unwind_labels_grouped_with_counts() {
    let db = db();
    // Six label occurrences across bugs; UNWIND + GROUP BY tallies them.
    let rows = run(
        "UNWIND ['p0', 'p1', 'p1', 'p2', 'p2', 'p2'] AS sev \
         RETURN sev, count(*) AS freq ORDER BY freq DESC, sev",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0), "p2");
    assert_eq!(int(&rows, 0, 1), 3);
    assert_eq!(string(&rows, 1, 0), "p1");
    assert_eq!(int(&rows, 1, 1), 2);
    assert_eq!(string(&rows, 2, 0), "p0");
    assert_eq!(int(&rows, 2, 1), 1);
}

// ===== Cross-cutting regressions ============================================

#[test]
fn unwind_after_unmatched_pattern_yields_nothing() {
    let db = db();
    // No matching node → no input row → UNWIND expands nothing.
    let rows = run("MATCH (n:DoesNotExist) UNWIND [1, 2, 3] AS x RETURN x", &db);
    assert!(rows.is_empty());
}

#[test]
fn double_unwind_produces_cartesian_product() {
    let db = db();
    let rows = run(
        "UNWIND [1, 2] AS a UNWIND ['x', 'y'] AS b \
         RETURN a, b ORDER BY a, b",
        &db,
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(string(&rows, 0, 1), "x");
    assert_eq!(int(&rows, 3, 0), 2);
    assert_eq!(string(&rows, 3, 1), "y");
}

// ===== Empty UNWIND + write clause — issue #300 ==============================
//
// A write clause (CREATE / MERGE, with or without a trailing SET) that follows
// an `UNWIND $xs AS v` where `$xs` is empty must run ZERO times and yield zero
// rows — the standard "empty driving table" semantics — NOT resurrect one empty
// row and dereference the now-unbound loop variable `v`. Regression guard: this
// exact shape (graphiti's `get_entity_edge_save_bulk_query` on an empty batch)
// previously failed with `unbound variable v`.

fn count_label(db: &Drevo, label: &str) -> i64 {
    let rows = run(&format!("MATCH (n:{label}) RETURN count(n) AS c"), db);
    int(&rows, 0, 0)
}

#[test]
fn empty_unwind_then_create_is_zero_rows_and_writes_nothing() {
    let db = db();
    let rows = run(
        "UNWIND [] AS edge CREATE (a:Zz {uuid: edge.x}) RETURN edge.x AS u",
        &db,
    );
    assert!(
        rows.is_empty(),
        "empty UNWIND + CREATE must yield zero rows"
    );
    assert_eq!(count_label(&db, "Zz"), 0, "nothing must be created");
}

#[test]
fn empty_unwind_then_merge_is_zero_rows_and_writes_nothing() {
    let db = db();
    let rows = run(
        "UNWIND [] AS edge MERGE (a:Zz {uuid: edge.x}) RETURN edge.x AS u",
        &db,
    );
    assert!(rows.is_empty());
    assert_eq!(count_label(&db, "Zz"), 0);
}

#[test]
fn empty_unwind_then_merge_and_whole_map_set_is_zero_rows() {
    // graphiti's entity-edge bulk shape on an empty batch.
    let db = db();
    run("CREATE (:Entity {uuid: 'a'})", &db);
    run("CREATE (:Entity {uuid: 'b'})", &db);
    let rows = run(
        "UNWIND [] AS edge \
         MATCH (source:Entity {uuid: edge.source_node_uuid}) \
         MATCH (target:Entity {uuid: edge.target_node_uuid}) \
         MERGE (source)-[e:RELATES_TO {uuid: edge.uuid}]->(target) \
         SET e = edge RETURN edge.uuid AS uuid",
        &db,
    );
    assert!(
        rows.is_empty(),
        "empty batch must be a no-op yielding zero rows"
    );
}

#[test]
fn empty_unwind_then_write_returning_a_constant_is_zero_rows() {
    // The failure did not depend on the loop var appearing in RETURN.
    let db = db();
    let rows = run(
        "UNWIND [] AS edge MERGE (a:Zz {uuid: edge.x}) RETURN 1 AS u",
        &db,
    );
    assert!(rows.is_empty());
    assert_eq!(count_label(&db, "Zz"), 0);
}

#[test]
fn standalone_create_and_merge_still_run_once() {
    // The initial seeded driving row must still let a write with no prior
    // row-producing clause execute exactly once.
    let db = db();
    run("CREATE (:Zz {uuid: 'solo'})", &db);
    assert_eq!(count_label(&db, "Zz"), 1);
    run("MERGE (:Zz {uuid: 'solo'})", &db); // matches, no new node
    run("MERGE (:Zz {uuid: 'other'})", &db); // creates
    assert_eq!(count_label(&db, "Zz"), 2);
}

#[test]
fn nonempty_unwind_then_merge_and_set_creates_and_returns() {
    let db = db();
    let rows = run(
        "UNWIND [{uuid: 'k'}] AS edge MERGE (a:Zz {uuid: edge.uuid}) \
         SET a = edge RETURN edge.uuid AS u",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "k");
    assert_eq!(count_label(&db, "Zz"), 1);
}
