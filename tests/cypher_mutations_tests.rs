//! End-to-end Cypher mutation tests — Phase 10 task `00064`.
//!
//! The five scenario domains (CBT journal, story editor, IT task
//! manager, ERP, bug tracker) are the same ones exercised by
//! `cypher_parser_e2e_tests.rs` and `cypher_executor_tests.rs`. Here we
//! drive the *mutation* surface — `SET`, `REMOVE`, `DELETE` /
//! `DETACH DELETE`, and `MERGE` (incl. `ON CREATE` / `ON MATCH`) — using
//! realistic mixed-clause queries the agentic workload (Phase 10.5,
//! `00123` / `00128`) will replay against this same executor.

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

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected error")
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_set_thought_reframing_text_via_match_set() {
    let db = db();
    run(
        "CREATE (t:Thought {title: 'I always fail', mood: 'anxious'})",
        &db,
    );
    run(
        "MATCH (t:Thought {title: 'I always fail'}) SET t.reframe = 'I sometimes fail; I also succeed'",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought {title: 'I always fail'}) RETURN t.reframe AS reframe",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Value::String("I sometimes fail; I also succeed".into())
    );
}

#[test]
fn cbt_merge_distortion_does_not_duplicate() {
    let db = db();
    for _ in 0..3 {
        run("MERGE (d:Distortion {kind: 'catastrophizing'})", &db);
    }
    let rows = run("MATCH (d:Distortion) RETURN d.kind AS kind", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("catastrophizing".into()));
}

#[test]
fn cbt_attach_distortion_to_thought_via_merge_between_bound_vars() {
    let db = db();
    run("CREATE (t:Thought {title: 'T1'})", &db);
    run("CREATE (d:Distortion {kind: 'mind_reading'})", &db);
    run(
        "MATCH (t:Thought {title: 'T1'}), (d:Distortion {kind: 'mind_reading'}) MERGE (t)-[:HAS_DISTORTION]->(d)",
        &db,
    );
    // Re-running MERGE must not create a duplicate edge.
    run(
        "MATCH (t:Thought {title: 'T1'}), (d:Distortion {kind: 'mind_reading'}) MERGE (t)-[:HAS_DISTORTION]->(d)",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought)-[:HAS_DISTORTION]->(d:Distortion) RETURN d.kind AS k",
        &db,
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn cbt_remove_property_when_user_clears_reframe() {
    let db = db();
    run("CREATE (t:Thought {title: 'T', reframe: 'old'})", &db);
    run("MATCH (t:Thought {title: 'T'}) REMOVE t.reframe", &db);
    let rows = run("MATCH (t:Thought {title: 'T'}) RETURN t.reframe AS r", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_promote_scene_to_climax_via_set_label() {
    let db = db();
    run("CREATE (s:Scene {title: 'The Confrontation'})", &db);
    run(
        "MATCH (s:Scene {title: 'The Confrontation'}) SET s:Climax",
        &db,
    );
    let rows = run("MATCH (s:Climax) RETURN s.title AS title", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("The Confrontation".into()));
}

#[test]
fn story_detach_delete_chapter_removes_its_scenes_link() {
    let db = db();
    run(
        "CREATE (c:Chapter {title: 'Chapter 1'})-[:CONTAINS]->(s:Scene {title: 'Opening'})",
        &db,
    );
    run(
        "MATCH (c:Chapter {title: 'Chapter 1'}) DETACH DELETE c",
        &db,
    );
    // The chapter is gone; the scene survives (orphaned but present).
    let chapters = run("MATCH (c:Chapter) RETURN c.title AS t", &db);
    let scenes = run("MATCH (s:Scene) RETURN s.title AS t", &db);
    assert!(chapters.is_empty());
    assert_eq!(scenes.len(), 1);
}

#[test]
fn story_replace_scene_properties_with_assignment() {
    let db = db();
    run(
        "CREATE (s:Scene {title: 'Draft', mood: 'tense', wordcount: 800})",
        &db,
    );
    run(
        "MATCH (s:Scene) SET s = {title: 'Final', mood: 'resolved'}",
        &db,
    );
    let rows = run(
        "MATCH (s:Scene) RETURN s.title AS title, s.mood AS mood, s.wordcount AS wc",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Final".into()));
    assert_eq!(rows[0][1], Value::String("resolved".into()));
    assert_eq!(rows[0][2], Value::Null);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_mark_task_done_via_set() {
    let db = db();
    run("CREATE (t:Task {title: 'Ship 00064', status: 'open'})", &db);
    run(
        "MATCH (t:Task {title: 'Ship 00064'}) SET t.status = 'done'",
        &db,
    );
    let rows = run(
        "MATCH (t:Task {title: 'Ship 00064'}) RETURN t.status AS s",
        &db,
    );
    assert_eq!(rows[0][0], Value::String("done".into()));
}

#[test]
fn task_manager_merge_assignment_relationship() {
    let db = db();
    run("CREATE (t:Task {title: 'Review PR'})", &db);
    run("CREATE (u:User {name: 'alice'})", &db);
    run(
        "MATCH (t:Task {title: 'Review PR'}), (u:User {name: 'alice'}) MERGE (u)-[:ASSIGNED_TO]->(t)",
        &db,
    );
    run(
        "MATCH (t:Task {title: 'Review PR'}), (u:User {name: 'alice'}) MERGE (u)-[:ASSIGNED_TO]->(t)",
        &db,
    );
    let rows = run(
        "MATCH (u:User)-[:ASSIGNED_TO]->(t:Task) RETURN u.name AS name, t.title AS task",
        &db,
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn task_manager_delete_task_with_subtask_requires_detach() {
    let db = db();
    run(
        "CREATE (p:Task {title: 'Parent'})-[:HAS_SUBTASK]->(c:Task {title: 'Child'})",
        &db,
    );
    let e = err("MATCH (p:Task {title: 'Parent'}) DELETE p", &db);
    assert!(matches!(e, ExecError::InvalidMutation(_)));
    // Now use DETACH DELETE — should succeed and cascade-remove the edge.
    run("MATCH (p:Task {title: 'Parent'}) DETACH DELETE p", &db);
    let remaining = run("MATCH (t:Task) RETURN t.title AS title", &db);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0][0], Value::String("Child".into()));
}

#[test]
fn task_manager_parametrised_set_via_with_params() {
    let db = db();
    run("CREATE (t:Task {title: 'Bump version'})", &db);
    let mut params = HashMap::new();
    params.insert(
        "new_status".to_string(),
        Value::String("in_progress".into()),
    );
    run_with(
        "MATCH (t:Task {title: 'Bump version'}) SET t.status = $new_status",
        &db,
        params,
    );
    let rows = run(
        "MATCH (t:Task {title: 'Bump version'}) RETURN t.status AS s",
        &db,
    );
    assert_eq!(rows[0][0], Value::String("in_progress".into()));
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_merge_supplier_idempotent_across_runs() {
    let db = db();
    for _ in 0..5 {
        run(
            "MERGE (s:Supplier {name: 'Acme'}) ON CREATE SET s.country = 'US' ON MATCH SET s.last_seen = 'today'",
            &db,
        );
    }
    let rows = run(
        "MATCH (s:Supplier {name: 'Acme'}) RETURN s.country AS country, s.last_seen AS last_seen",
        &db,
    );
    assert_eq!(rows.len(), 1);
    // First run goes through ON CREATE; subsequent through ON MATCH.
    assert_eq!(rows[0][0], Value::String("US".into()));
    assert_eq!(rows[0][1], Value::String("today".into()));
}

#[test]
fn erp_purchase_order_line_total_via_set_arithmetic_on_property() {
    let db = db();
    run(
        "CREATE (po:PurchaseOrder {number: 'PO-1'})-[:HAS_LINE]->(l:Line {sku: 'X', qty: 3, price: 100})",
        &db,
    );
    run(
        "MATCH (po:PurchaseOrder {number: 'PO-1'})-[:HAS_LINE]->(l:Line) SET l.total = l.qty * l.price",
        &db,
    );
    let rows = run(
        "MATCH (po:PurchaseOrder)-[:HAS_LINE]->(l:Line) RETURN l.total AS total",
        &db,
    );
    assert_eq!(rows[0][0], Value::Integer(300));
}

#[test]
fn erp_remove_outdated_property_from_legacy_orders() {
    let db = db();
    run(
        "CREATE (po:PurchaseOrder {number: 'PO-2', legacy_flag: true, status: 'sent'})",
        &db,
    );
    run(
        "MATCH (po:PurchaseOrder {number: 'PO-2'}) REMOVE po.legacy_flag",
        &db,
    );
    let rows = run(
        "MATCH (po:PurchaseOrder {number: 'PO-2'}) RETURN po.legacy_flag AS f, po.status AS s",
        &db,
    );
    assert_eq!(rows[0][0], Value::Null);
    assert_eq!(rows[0][1], Value::String("sent".into()));
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_escalate_severity_via_set_merge_map() {
    let db = db();
    run(
        "CREATE (b:Bug {title: 'Crash on startup', severity: 'medium', open: true})",
        &db,
    );
    // SET += merges new keys, preserves the rest.
    run(
        "MATCH (b:Bug {title: 'Crash on startup'}) SET b += {severity: 'critical', triaged_by: 'oncall'}",
        &db,
    );
    let rows = run(
        "MATCH (b:Bug {title: 'Crash on startup'}) RETURN b.severity AS sev, b.open AS open, b.triaged_by AS who",
        &db,
    );
    assert_eq!(rows[0][0], Value::String("critical".into()));
    assert_eq!(rows[0][1], Value::Bool(true));
    assert_eq!(rows[0][2], Value::String("oncall".into()));
}

#[test]
fn bug_tracker_close_bug_then_remove_assignee_link() {
    let db = db();
    run(
        "CREATE (b:Bug {title: 'Heap leak', status: 'open'})-[:ASSIGNED_TO]->(u:User {name: 'alice'})",
        &db,
    );
    run(
        "MATCH (b:Bug {title: 'Heap leak'}) SET b.status = 'closed'",
        &db,
    );
    run(
        "MATCH (b:Bug {title: 'Heap leak'})-[r:ASSIGNED_TO]->(u:User) DELETE r",
        &db,
    );
    let rows = run(
        "MATCH (b:Bug {title: 'Heap leak'})-[:ASSIGNED_TO]->(u:User) RETURN u.name AS name",
        &db,
    );
    assert!(rows.is_empty());
    let bug_rows = run(
        "MATCH (b:Bug {title: 'Heap leak'}) RETURN b.status AS s",
        &db,
    );
    assert_eq!(bug_rows[0][0], Value::String("closed".into()));
}

#[test]
fn bug_tracker_merge_label_does_not_duplicate_node_kind() {
    let db = db();
    run("CREATE (b:Bug {title: 'Login broken'})", &db);
    // SET secondary label, then re-MATCH by it.
    run("MATCH (b:Bug) SET b:Regression", &db);
    let rows = run("MATCH (b:Regression) RETURN b.title AS title", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Login broken".into()));
    // Remove the secondary label — the node still exists under :Bug.
    run("MATCH (b:Regression) REMOVE b:Regression", &db);
    let none = run("MATCH (b:Regression) RETURN b.title AS title", &db);
    assert!(none.is_empty());
    let still_bug = run("MATCH (b:Bug) RETURN b.title AS title", &db);
    assert_eq!(still_bug.len(), 1);
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn match_set_delete_round_trip_keeps_stats_accurate() {
    let db = db();
    let q = parse("CREATE (n:Person {name: 'A', age: 30, role: 'eng'})").unwrap();
    let res = execute(&q, &db, HashMap::new()).unwrap();
    assert_eq!(res.stats.nodes_created, 1);

    let q = parse("MATCH (n:Person {name: 'A'}) SET n.age = 31, n.role = 'staff_eng'").unwrap();
    let res = execute(&q, &db, HashMap::new()).unwrap();
    assert_eq!(res.stats.properties_set, 2);

    let q = parse("MATCH (n:Person {name: 'A'}) DELETE n").unwrap();
    let res = execute(&q, &db, HashMap::new()).unwrap();
    assert_eq!(res.stats.nodes_deleted, 1);
    assert_eq!(res.stats.relationships_deleted, 0);
}

#[test]
fn merge_then_match_returns_the_merged_node() {
    let db = db();
    run("MERGE (n:Person {name: 'first'})", &db);
    let rows = run(
        "MATCH (n:Person {name: 'first'}) RETURN n.name AS name",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("first".into()));
}

#[test]
fn detach_delete_clears_orphan_chain() {
    let db = db();
    run(
        "CREATE (a:Node {name: 'a'})-[:R]->(b:Node {name: 'b'})-[:R]->(c:Node {name: 'c'})",
        &db,
    );
    run("MATCH (n:Node {name: 'b'}) DETACH DELETE n", &db);
    let rows = run("MATCH (n:Node) RETURN n.name AS name", &db);
    let mut names: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
}
