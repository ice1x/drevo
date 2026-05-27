//! End-to-end Cypher `OPTIONAL MATCH` tests — Phase 10 task `00067`.
//!
//! Exercises left-join semantics (each upstream row is preserved; any
//! pattern variables that fail to bind become `NULL`) across the five
//! drevo target scenario domains plus cross-scenario regressions.
//! `OPTIONAL MATCH` is the Cypher analogue of SQL's LEFT OUTER JOIN
//! and is the building block for "every X plus its optional Y"
//! patterns (every user plus their last login; every task plus its
//! optional subtask; every order plus its optional invoice; etc.).

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> Option<String> {
    match &rows[r][c] {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => panic!("expected string or null at ({},{}), got {:?}", r, c, other),
    }
}

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({},{}), got {:?}", r, c, other),
    }
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_every_thought_with_optional_distortion() {
    let db = db();
    // Thoughts: A (with distortion), B (no distortion), C (with distortion).
    run(
        "CREATE (:Thought {title: 'A'})-[:DISTORTED_BY]->(:Distortion {kind: 'catastrophising'})",
        &db,
    );
    run("CREATE (:Thought {title: 'B'})", &db);
    run(
        "CREATE (:Thought {title: 'C'})-[:DISTORTED_BY]->(:Distortion {kind: 'mind_reading'})",
        &db,
    );
    let rows = run(
        "MATCH (t:Thought) OPTIONAL MATCH (t)-[:DISTORTED_BY]->(d:Distortion) RETURN t.title AS thought, d.kind AS distortion ORDER BY thought",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("A"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("catastrophising"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("B"));
    assert_eq!(string(&rows, 1, 1), None);
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("C"));
    assert_eq!(string(&rows, 2, 1).as_deref(), Some("mind_reading"));
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_every_scene_with_optional_followup() {
    let db = db();
    run(
        "CREATE (:Scene {title: 'S1'})-[:NEXT]->(:Scene {title: 'S2'})",
        &db,
    );
    // S2 already exists from above; S3 has no NEXT (orphan).
    run("CREATE (:Scene {title: 'S3'})", &db);
    let rows = run(
        "MATCH (s:Scene) OPTIONAL MATCH (s)-[:NEXT]->(n:Scene) RETURN s.title AS scene, n.title AS next ORDER BY scene",
        &db,
    );
    assert_eq!(rows.len(), 3);
    // S1 → S2; S2 has no NEXT; S3 has no NEXT.
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("S1"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("S2"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("S2"));
    assert_eq!(string(&rows, 1, 1), None);
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("S3"));
    assert_eq!(string(&rows, 2, 1), None);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_every_task_with_optional_assignee() {
    let db = db();
    // T1 assigned to alice; T2 unassigned; T3 assigned to bob.
    run(
        "CREATE (:Task {title: 'T1'})-[:ASSIGNED_TO]->(:User {name: 'alice'})",
        &db,
    );
    run("CREATE (:Task {title: 'T2'})", &db);
    run(
        "CREATE (:Task {title: 'T3'})-[:ASSIGNED_TO]->(:User {name: 'bob'})",
        &db,
    );
    let rows = run(
        "MATCH (t:Task) OPTIONAL MATCH (t)-[:ASSIGNED_TO]->(u:User) RETURN t.title AS task, u.name AS owner ORDER BY task",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("T1"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("alice"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("T2"));
    assert_eq!(string(&rows, 1, 1), None);
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("T3"));
    assert_eq!(string(&rows, 2, 1).as_deref(), Some("bob"));
}

#[test]
fn task_manager_count_subtasks_with_zero_for_leafs() {
    let db = db();
    // T1 has 2 subtasks; T2 has 1; T3 has none.
    run(
        "CREATE (:Task {title: 'T1'})-[:HAS_SUBTASK]->(:Task {title: 'T1a'})",
        &db,
    );
    run(
        "MATCH (t:Task {title: 'T1'}) CREATE (t)-[:HAS_SUBTASK]->(:Task {title: 'T1b'})",
        &db,
    );
    run(
        "CREATE (:Task {title: 'T2'})-[:HAS_SUBTASK]->(:Task {title: 'T2a'})",
        &db,
    );
    run("CREATE (:Task {title: 'T3'})", &db);
    let rows = run(
        "MATCH (t:Task) WHERE t.title IN ['T1', 'T2', 'T3'] OPTIONAL MATCH (t)-[:HAS_SUBTASK]->(s:Task) RETURN t.title AS task, count(s) AS subtasks ORDER BY task",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("T1"));
    assert_eq!(int(&rows, 0, 1), 2);
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("T2"));
    assert_eq!(int(&rows, 1, 1), 1);
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("T3"));
    assert_eq!(int(&rows, 2, 1), 0);
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_every_order_with_optional_invoice() {
    let db = db();
    // O1 has invoice; O2 has none.
    run(
        "CREATE (:Order {ref: 'O1'})-[:BILLED_BY]->(:Invoice {ref: 'I1'})",
        &db,
    );
    run("CREATE (:Order {ref: 'O2'})", &db);
    let rows = run(
        "MATCH (o:Order) OPTIONAL MATCH (o)-[:BILLED_BY]->(i:Invoice) RETURN o.ref AS order_ref, i.ref AS invoice_ref ORDER BY order_ref",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("O1"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("I1"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("O2"));
    assert_eq!(string(&rows, 1, 1), None);
}

#[test]
fn erp_revenue_per_buyer_including_zero_buyers() {
    let db = db();
    run(
        "CREATE (:Buyer {name: 'acme'})-[:PLACED]->(:Order {total: 1000})",
        &db,
    );
    run(
        "MATCH (b:Buyer {name: 'acme'}) CREATE (b)-[:PLACED]->(:Order {total: 500})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'globex'})-[:PLACED]->(:Order {total: 2500})",
        &db,
    );
    run("CREATE (:Buyer {name: 'initech'})", &db);
    let rows = run(
        "MATCH (b:Buyer) OPTIONAL MATCH (b)-[:PLACED]->(o:Order) RETURN b.name AS buyer, sum(o.total) AS revenue ORDER BY buyer",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("acme"));
    assert_eq!(int(&rows, 0, 1), 1500);
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("globex"));
    assert_eq!(int(&rows, 1, 1), 2500);
    // initech has zero placed orders → sum(NULL) = 0.
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("initech"));
    assert_eq!(int(&rows, 2, 1), 0);
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_every_bug_with_optional_assignee_and_fix() {
    let db = db();
    // Bug B1: assigned + fixed; B2: assigned, no fix; B3: unassigned.
    run(
        "CREATE (:Bug {id: 'B1'})-[:ASSIGNED_TO]->(:User {name: 'alice'})",
        &db,
    );
    run(
        "MATCH (b:Bug {id: 'B1'}) CREATE (b)-[:FIXED_BY]->(:Commit {sha: 'deadbeef'})",
        &db,
    );
    run(
        "CREATE (:Bug {id: 'B2'})-[:ASSIGNED_TO]->(:User {name: 'bob'})",
        &db,
    );
    run("CREATE (:Bug {id: 'B3'})", &db);
    let rows = run(
        "MATCH (b:Bug) OPTIONAL MATCH (b)-[:ASSIGNED_TO]->(u:User) OPTIONAL MATCH (b)-[:FIXED_BY]->(c:Commit) RETURN b.id AS bug, u.name AS owner, c.sha AS fix ORDER BY bug",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("B1"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("alice"));
    assert_eq!(string(&rows, 0, 2).as_deref(), Some("deadbeef"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("B2"));
    assert_eq!(string(&rows, 1, 1).as_deref(), Some("bob"));
    assert_eq!(string(&rows, 1, 2), None);
    assert_eq!(string(&rows, 2, 0).as_deref(), Some("B3"));
    assert_eq!(string(&rows, 2, 1), None);
    assert_eq!(string(&rows, 2, 2), None);
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn optional_match_with_where_inside_treats_failed_match_as_no_match() {
    let db = db();
    // Two thoughts; one has a high-intensity distortion, the other a low-intensity one.
    run("CREATE (:Thought {title: 'high'})-[:DISTORTED_BY {intensity: 9}]->(:Distortion {kind: 'k1'})", &db);
    run("CREATE (:Thought {title: 'low'})-[:DISTORTED_BY {intensity: 2}]->(:Distortion {kind: 'k2'})", &db);
    let rows = run(
        "MATCH (t:Thought) OPTIONAL MATCH (t)-[r:DISTORTED_BY]->(d:Distortion) WHERE r.intensity >= 5 RETURN t.title AS thought, d.kind AS distortion ORDER BY thought",
        &db,
    );
    // `high` matches the WHERE → has a distortion; `low` fails WHERE → distortion is NULL but row stays.
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("high"));
    assert_eq!(string(&rows, 0, 1).as_deref(), Some("k1"));
    assert_eq!(string(&rows, 1, 0).as_deref(), Some("low"));
    assert_eq!(string(&rows, 1, 1), None);
}

#[test]
fn optional_match_alone_on_empty_db_returns_single_null_row() {
    let db = db();
    let rows = run("OPTIONAL MATCH (n:Person)-[:KNOWS]->(f) RETURN n, f", &db);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Null));
    assert!(matches!(rows[0][1], Value::Null));
}

#[test]
fn optional_match_multi_pattern_each_independent_null() {
    let db = db();
    run("CREATE (:Person {name: 'A'})", &db);
    // No KNOWS, no LIKES. Two OPTIONAL MATCHes in a chain — each
    // independently emits a NULL row on miss; the result has one row
    // with both rel variables NULL.
    let rows = run(
        "MATCH (n:Person) OPTIONAL MATCH (n)-[r:KNOWS]->(f) OPTIONAL MATCH (n)-[l:LIKES]->(t) RETURN n.name AS who, r, f, l, t",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0).as_deref(), Some("A"));
    for (col, value) in rows[0].iter().enumerate().take(5).skip(1) {
        assert!(
            matches!(value, Value::Null),
            "column {} should be NULL, got {:?}",
            col,
            value
        );
    }
}
