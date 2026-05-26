//! End-to-end Cypher executor tests — Phase 10 task `00063`.
//!
//! These tests exercise `parse → execute → result rows` on the five
//! drevo target scenario domains (CBT journal, story editor, IT task
//! manager, ERP, bug tracker). They are the integration counterpart to
//! the unit tests inside `src/cypher/executor.rs` — instead of poking
//! at internal state they assert the public contract from the Cypher
//! source string down to the storage layer.
//!
//! The five scenarios mirror the AST asserted by `cypher_parser_e2e_tests.rs`
//! and form the **definition of done** seed for Phase 10 — once `00064`
//! (`SET`/`DELETE`/`MERGE`) and `00065` (`WHERE`) land, the same tests
//! grow into the canonical "Phase 10 finished" suite required by the
//! README §"Phase 10 — Cypher Query Language" DoD line.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, ExecStats, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn stats(source: &str, drevo: &Drevo) -> ExecStats {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").stats
}

fn err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected error")
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_capture_thought_with_inline_mood_relationship() {
    let db = db();
    let s = stats(
        "CREATE (t:Thought {body: 'I am terrible at work', recorded_at: 1700000000})
              -[:HAD_MOOD]->(m:Mood {valence: -0.7, intensity: 0.8})",
        &db,
    );
    assert_eq!(s.nodes_created, 2);
    assert_eq!(s.relationships_created, 1);
    assert_eq!(db.list_nodes_by_kind("Thought", 10, 0).unwrap().len(), 1);
    assert_eq!(db.list_nodes_by_kind("Mood", 10, 0).unwrap().len(), 1);
}

#[test]
fn cbt_find_thoughts_by_distortion_kind() {
    let db = db();
    run(
        "CREATE (t:Thought {body: 'maybe everyone hates me', recorded_at: 1700001000})
              -[:HAS_DISTORTION]->(:Distortion {kind: 'mind_reading'})",
        &db,
    );
    run(
        "CREATE (t:Thought {body: 'I will lose my job', recorded_at: 1700002000})
              -[:HAS_DISTORTION]->(:Distortion {kind: 'catastrophizing'})",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought)-[:HAS_DISTORTION]->(:Distortion {kind: 'mind_reading'})
         RETURN t.body AS thought, t.recorded_at AS at
         ORDER BY t.recorded_at DESC LIMIT 50",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("maybe everyone hates me".into()));
    assert_eq!(rows[0][1], Value::Integer(1700001000));
}

// ===== Scenario 2 — story / book editor =====================================

#[test]
fn story_chapter_appears_in_scene() {
    let db = db();
    run(
        "CREATE (c:Chapter {number: 1, title: 'The Beginning'}),
                (s:Scene {summary: 'first encounter'}),
                (c)-[:CONTAINS]->(s)",
        &db,
    );
    let rows = run(
        "MATCH (c:Chapter)-[:CONTAINS]->(s:Scene)
         RETURN c.title AS chapter, s.summary AS scene
         ORDER BY c.number",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("The Beginning".into()));
    assert_eq!(rows[0][1], Value::String("first encounter".into()));
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_subtask_chain() {
    let db = db();
    run(
        "CREATE (e:Epic {title: 'Search v2'})-[:HAS_TASK]->(t:Task {title: 'Index refactor'})",
        &db,
    );
    run(
        "MATCH (t:Task {title: 'Index refactor'})
         CREATE (t)-[:HAS_SUBTASK]->(:Task {title: 'Trigram tokenizer'})",
        &db,
    );
    let rows = run(
        "MATCH (e:Epic)-[:HAS_TASK]->(t:Task)-[:HAS_SUBTASK]->(s:Task)
         RETURN e.title AS epic, t.title AS task, s.title AS subtask",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Search v2".into()));
    assert_eq!(rows[0][1], Value::String("Index refactor".into()));
    assert_eq!(rows[0][2], Value::String("Trigram tokenizer".into()));
}

#[test]
fn task_manager_parametrised_lookup() {
    let db = db();
    run(
        "CREATE (:Task {title: 'A', priority: 1}), (:Task {title: 'B', priority: 2})",
        &db,
    );
    let mut params = HashMap::new();
    params.insert("title".into(), Value::String("B".into()));
    let rows = run_with(
        "MATCH (t:Task {title: $title}) RETURN t.title AS title, t.priority AS priority",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("B".into()));
    assert_eq!(rows[0][1], Value::Integer(2));
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_purchase_order_line_items_ordered_total_amount() {
    let db = db();
    run(
        "CREATE (po:PurchaseOrder {number: 'PO-1', status: 'open'}),
                (po)-[:HAS_LINE]->(:LineItem {sku: 'A', qty: 2, unit_price: 5}),
                (po)-[:HAS_LINE]->(:LineItem {sku: 'B', qty: 1, unit_price: 10})",
        &db,
    );
    let rows = run(
        "MATCH (po:PurchaseOrder)-[:HAS_LINE]->(li:LineItem)
         RETURN li.sku AS sku, li.qty * li.unit_price AS total
         ORDER BY li.sku",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("A".into()));
    assert_eq!(rows[0][1], Value::Integer(10));
    assert_eq!(rows[1][0], Value::String("B".into()));
    assert_eq!(rows[1][1], Value::Integer(10));
}

// ===== Scenario 5 — bug tracker ============================================

#[test]
fn bug_tracker_assignee_lookup_by_severity_filter() {
    let db = db();
    run(
        "CREATE (alice:Engineer {name: 'Alice'}),
                (bob:Engineer {name: 'Bob'}),
                (b1:Bug {title: 'crash on save', severity: 'critical'}),
                (b2:Bug {title: 'typo in tooltip', severity: 'minor'}),
                (alice)-[:ASSIGNED]->(b1),
                (bob)-[:ASSIGNED]->(b2)",
        &db,
    );
    let rows = run(
        "MATCH (e:Engineer)-[:ASSIGNED]->(b:Bug {severity: 'critical'})
         RETURN e.name AS who, b.title AS what",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Alice".into()));
    assert_eq!(rows[0][1], Value::String("crash on save".into()));
}

#[test]
fn bug_tracker_return_distinct_severities() {
    let db = db();
    run("CREATE (:Bug {severity: 'critical'})", &db);
    run("CREATE (:Bug {severity: 'critical'})", &db);
    run("CREATE (:Bug {severity: 'minor'})", &db);
    let rows = run(
        "MATCH (b:Bug) RETURN DISTINCT b.severity AS severity ORDER BY severity",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::String("critical".into()));
    assert_eq!(rows[1][0], Value::String("minor".into()));
}

// ===== Cross-scenario behavioural contracts ================================

#[test]
fn empty_match_returns_empty_rows_not_error() {
    let db = db();
    let rows = run("MATCH (n:Person) RETURN n.name AS name", &db);
    assert!(rows.is_empty());
}

#[test]
fn return_skip_past_end_yields_empty() {
    let db = db();
    run("CREATE (:Item {name: 'A'}), (:Item {name: 'B'})", &db);
    let rows = run(
        "MATCH (i:Item) RETURN i.name AS name ORDER BY name SKIP 5 LIMIT 10",
        &db,
    );
    assert!(rows.is_empty());
}

#[test]
fn incoming_relationship_direction() {
    let db = db();
    run(
        "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})",
        &db,
    );
    let rows = run(
        "MATCH (b:Person)<-[:KNOWS]-(a:Person) RETURN a.name AS src, b.name AS dst",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("A".into()));
    assert_eq!(rows[0][1], Value::String("B".into()));
}

#[test]
fn parameter_in_property_map_during_create() {
    let db = db();
    let mut params = HashMap::new();
    params.insert("name".into(), Value::String("Carol".into()));
    let rows = run_with(
        "CREATE (n:Person {name: $name}) RETURN n.name AS name",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Carol".into()));
}

#[test]
fn set_clause_rejected_with_pointer_to_00064() {
    let db = db();
    let e = err("MATCH (n:Person) SET n.age = 30 RETURN n", &db);
    match e {
        ExecError::Unsupported { feature, task, .. } => {
            assert!(feature.contains("SET"));
            assert_eq!(task, "00064");
        }
        other => panic!("expected Unsupported, got {:?}", other),
    }
}

#[test]
fn aggregations_rejected_with_pointer_to_00066() {
    let db = db();
    run("CREATE (:Person {name: 'A'})", &db);
    let e = err("MATCH (n:Person) RETURN count(n) AS total", &db);
    match e {
        ExecError::Unsupported { task, .. } => assert_eq!(task, "00066"),
        other => panic!("expected Unsupported, got {:?}", other),
    }
}

#[test]
fn optional_match_rejected_with_pointer_to_00067() {
    let db = db();
    let e = err("OPTIONAL MATCH (n:Person) RETURN n", &db);
    match e {
        ExecError::Unsupported { task, .. } => assert_eq!(task, "00067"),
        other => panic!("expected Unsupported, got {:?}", other),
    }
}

#[test]
fn varlen_paths_rejected_with_pointer_to_00069() {
    let db = db();
    run(
        "CREATE (:Person)-[:KNOWS]->(:Person)-[:KNOWS]->(:Person)",
        &db,
    );
    let e = err("MATCH (a:Person)-[:KNOWS*1..3]->(b:Person) RETURN a", &db);
    match e {
        ExecError::Unsupported { task, .. } => assert_eq!(task, "00069"),
        other => panic!("expected Unsupported, got {:?}", other),
    }
}
