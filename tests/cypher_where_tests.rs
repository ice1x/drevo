//! End-to-end Cypher WHERE tests — Phase 10 task `00065`.
//!
//! Exercises the WHERE filter on `MATCH` clauses across the five
//! drevo target scenario domains (CBT journal, story editor, IT task
//! manager, ERP, bug tracker) plus cross-scenario regressions. The
//! WHERE predicate evaluator is the executor's existing expression
//! evaluator from `00063` / `00064`, lifted from "rejected" status to
//! "filter rows by Bool(true)" semantics in this task.

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

fn names(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .collect()
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_find_thoughts_with_anxious_mood_above_threshold() {
    let db = db();
    for (title, mood, intensity) in [
        ("T1", "anxious", 8),
        ("T2", "anxious", 4),
        ("T3", "angry", 9),
        ("T4", "calm", 2),
    ] {
        run(
            &format!(
                "CREATE (:Thought {{title: '{}', mood: '{}', intensity: {}}})",
                title, mood, intensity
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (t:Thought) WHERE t.mood = 'anxious' AND t.intensity >= 6 RETURN t.title AS title ORDER BY t.title",
        &db,
    );
    assert_eq!(names(&rows), vec!["T1".to_string()]);
}

#[test]
fn cbt_distortion_kinds_in_list() {
    let db = db();
    for kind in [
        "catastrophizing",
        "mind_reading",
        "all_or_nothing",
        "fortune_telling",
    ] {
        run(&format!("CREATE (:Distortion {{kind: '{}'}})", kind), &db);
    }
    let rows = run(
        "MATCH (d:Distortion) WHERE d.kind IN ['catastrophizing', 'mind_reading'] RETURN d.kind AS kind ORDER BY d.kind",
        &db,
    );
    assert_eq!(
        names(&rows),
        vec!["catastrophizing".to_string(), "mind_reading".to_string()]
    );
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_find_scenes_in_act_with_tension_filter() {
    let db = db();
    for (title, act, tension) in [
        ("Opening", 1, 2),
        ("Setup", 1, 4),
        ("Confrontation", 2, 9),
        ("Climax", 3, 10),
        ("Resolution", 3, 3),
    ] {
        run(
            &format!(
                "CREATE (:Scene {{title: '{}', act: {}, tension: {}}})",
                title, act, tension
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (s:Scene) WHERE s.act >= 2 AND s.tension > 5 RETURN s.title AS title ORDER BY s.title",
        &db,
    );
    assert_eq!(
        names(&rows),
        vec!["Climax".to_string(), "Confrontation".to_string()]
    );
}

#[test]
fn story_scenes_whose_title_starts_with_prefix() {
    let db = db();
    for title in ["The Beginning", "The Middle", "The End", "Coda"] {
        run(&format!("CREATE (:Scene {{title: '{}'}})", title), &db);
    }
    let rows = run(
        "MATCH (s:Scene) WHERE s.title STARTS WITH 'The ' RETURN s.title AS title ORDER BY s.title",
        &db,
    );
    assert_eq!(
        names(&rows),
        vec![
            "The Beginning".to_string(),
            "The End".to_string(),
            "The Middle".to_string()
        ]
    );
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_open_tasks_assigned_to_user_via_parameter() {
    let db = db();
    run(
        "CREATE (a:User {name: 'alice'})-[:OWNS]->(t:Task {title: 'T1', status: 'open'})",
        &db,
    );
    run(
        "CREATE (b:User {name: 'bob'})-[:OWNS]->(t:Task {title: 'T2', status: 'open'})",
        &db,
    );
    run(
        "CREATE (a:User {name: 'alice'})-[:OWNS]->(t:Task {title: 'T3', status: 'done'})",
        &db,
    );
    let mut params = HashMap::new();
    params.insert("me".to_string(), Value::String("alice".into()));
    let rows = run_with(
        "MATCH (u:User)-[:OWNS]->(t:Task) WHERE u.name = $me AND t.status = 'open' RETURN t.title AS title ORDER BY t.title",
        &db,
        params,
    );
    assert_eq!(names(&rows), vec!["T1".to_string()]);
}

#[test]
fn task_manager_filter_by_subtask_via_relationship_property() {
    let db = db();
    run(
        "CREATE (p:Task {title: 'P1'})-[:HAS_SUBTASK {priority: 'high'}]->(c:Task {title: 'C1'})",
        &db,
    );
    run(
        "CREATE (p:Task {title: 'P2'})-[:HAS_SUBTASK {priority: 'low'}]->(c:Task {title: 'C2'})",
        &db,
    );
    let rows = run(
        "MATCH (p:Task)-[r:HAS_SUBTASK]->(c:Task) WHERE r.priority = 'high' RETURN p.title AS title",
        &db,
    );
    assert_eq!(names(&rows), vec!["P1".to_string()]);
}

#[test]
fn task_manager_tasks_with_missing_due_date() {
    let db = db();
    run("CREATE (:Task {title: 'T1', due: '2026-06-01'})", &db);
    run("CREATE (:Task {title: 'T2'})", &db);
    run("CREATE (:Task {title: 'T3', due: '2026-07-15'})", &db);
    let rows = run(
        "MATCH (t:Task) WHERE t.due IS NULL RETURN t.title AS title",
        &db,
    );
    assert_eq!(names(&rows), vec!["T2".to_string()]);
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_line_items_filtered_by_qty_and_price() {
    let db = db();
    for (sku, qty, price) in [
        ("X1", 1, 100),
        ("X2", 5, 50),
        ("X3", 10, 200),
        ("X4", 100, 10),
    ] {
        run(
            &format!(
                "CREATE (:Line {{sku: '{}', qty: {}, price: {}}})",
                sku, qty, price
            ),
            &db,
        );
    }
    // Lines where qty * price >= 500: X3 (2000) and X4 (1000) qualify;
    // X1 = 100, X2 = 250 don't.
    let rows = run(
        "MATCH (l:Line) WHERE l.qty * l.price >= 500 RETURN l.sku AS sku ORDER BY l.sku",
        &db,
    );
    assert_eq!(names(&rows), vec!["X3".to_string(), "X4".to_string()]);
}

#[test]
fn erp_purchase_orders_open_or_overdue() {
    let db = db();
    for (number, status, days) in [
        ("PO-1", "open", 5),
        ("PO-2", "closed", 15),
        ("PO-3", "open", 0),
        ("PO-4", "closed", 30),
    ] {
        run(
            &format!(
                "CREATE (:PurchaseOrder {{number: '{}', status: '{}', days_outstanding: {}}})",
                number, status, days
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (po:PurchaseOrder) WHERE po.status = 'open' OR po.days_outstanding > 20 RETURN po.number AS number ORDER BY po.number",
        &db,
    );
    assert_eq!(
        names(&rows),
        vec!["PO-1".to_string(), "PO-3".to_string(), "PO-4".to_string()]
    );
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_filter_by_severity_in_list() {
    let db = db();
    for (title, sev) in [
        ("Crash", "critical"),
        ("Typo", "trivial"),
        ("Hang", "high"),
        ("UI glitch", "low"),
    ] {
        run(
            &format!("CREATE (:Bug {{title: '{}', severity: '{}'}})", title, sev),
            &db,
        );
    }
    let rows = run(
        "MATCH (b:Bug) WHERE b.severity IN ['critical', 'high'] RETURN b.title AS title ORDER BY b.title",
        &db,
    );
    assert_eq!(names(&rows), vec!["Crash".to_string(), "Hang".to_string()]);
}

#[test]
fn bug_tracker_unassigned_open_bugs() {
    let db = db();
    run(
        "CREATE (:Bug {title: 'Assigned', status: 'open', assignee: 'alice'})",
        &db,
    );
    run("CREATE (:Bug {title: 'Floater', status: 'open'})", &db);
    run("CREATE (:Bug {title: 'Done', status: 'closed'})", &db);
    let rows = run(
        "MATCH (b:Bug) WHERE b.status = 'open' AND b.assignee IS NULL RETURN b.title AS title",
        &db,
    );
    assert_eq!(names(&rows), vec!["Floater".to_string()]);
}

#[test]
fn bug_tracker_title_contains_keyword() {
    let db = db();
    for title in [
        "NullPointerException in login",
        "Login screen blank",
        "Crash on startup",
        "Memory leak",
    ] {
        run(&format!("CREATE (:Bug {{title: '{}'}})", title), &db);
    }
    // CONTAINS is case-sensitive — lowercase 'login' only matches the
    // first title; 'Login' starts the second title with a capital L.
    let rows = run(
        "MATCH (b:Bug) WHERE b.title CONTAINS 'login' RETURN b.title AS title ORDER BY b.title",
        &db,
    );
    assert_eq!(
        names(&rows),
        vec!["NullPointerException in login".to_string()]
    );
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn where_with_set_round_trip_matches_only_targeted_rows() {
    let db = db();
    for (n, score) in [("A", 50), ("B", 80), ("C", 95)] {
        run(
            &format!("CREATE (:Item {{name: '{}', score: {}}})", n, score),
            &db,
        );
    }
    run("MATCH (i:Item) WHERE i.score >= 80 SET i.flag = true", &db);
    let rows = run(
        "MATCH (i:Item) WHERE i.flag = true RETURN i.name AS name ORDER BY i.name",
        &db,
    );
    assert_eq!(names(&rows), vec!["B".to_string(), "C".to_string()]);
}

#[test]
fn where_with_delete_removes_only_matched_rows() {
    let db = db();
    for n in ["A", "B", "C"] {
        run(&format!("CREATE (:Item {{name: '{}'}})", n), &db);
    }
    let q = parse("MATCH (i:Item) WHERE i.name = 'B' DELETE i").unwrap();
    let res = execute(&q, &db, HashMap::new()).unwrap();
    assert_eq!(res.stats.nodes_deleted, 1);
    let rows = run("MATCH (i:Item) RETURN i.name AS name ORDER BY i.name", &db);
    assert_eq!(names(&rows), vec!["A".to_string(), "C".to_string()]);
}

#[test]
fn where_predicate_with_unknown_var_errors_cleanly() {
    let db = db();
    run("CREATE (:Person {name: 'A'})", &db);
    let e = err("MATCH (n:Person) WHERE unknown.x = 1 RETURN n", &db);
    assert!(matches!(e, ExecError::UnboundVariable { .. }));
}

#[test]
fn where_combined_with_order_by_skip_limit() {
    let db = db();
    for (n, age) in [("A", 25), ("B", 30), ("C", 35), ("D", 40), ("E", 45)] {
        run(
            &format!("CREATE (:Person {{name: '{}', age: {}}})", n, age),
            &db,
        );
    }
    // age >= 30 filters to B(30), C(35), D(40), E(45); DESC by age =
    // E, D, C, B; SKIP 1 LIMIT 2 = D, C.
    let rows = run(
        "MATCH (n:Person) WHERE n.age >= 30 RETURN n.name AS name ORDER BY n.age DESC SKIP 1 LIMIT 2",
        &db,
    );
    assert_eq!(names(&rows), vec!["D".to_string(), "C".to_string()]);
}
