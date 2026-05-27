//! End-to-end Cypher `WITH` tests — Phase 10 task `00068`.
//!
//! Exercises query pipelining (intermediate projection, DISTINCT,
//! ORDER BY / SKIP / LIMIT, post-projection WHERE, aggregation-then-
//! filter, multi-stage WITH chains, variable-scope reshape) across
//! the five drevo target scenario domains plus cross-scenario
//! regressions. `WITH` is the only point at which the variable
//! scope can be reshaped — projecting `n.team AS team` drops `n`
//! from the scope of downstream clauses.

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

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({},{}), got {:?}", r, c, other),
    }
}

fn float(rows: &[Vec<Value>], r: usize, c: usize) -> f64 {
    match &rows[r][c] {
        Value::Float(f) => *f,
        Value::Integer(i) => *i as f64,
        other => panic!("expected float at ({},{}), got {:?}", r, c, other),
    }
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> String {
    match &rows[r][c] {
        Value::String(s) => s.clone(),
        other => panic!("expected string at ({},{}), got {:?}", r, c, other),
    }
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_chained_with_filters_to_frequent_moods() {
    let db = db();
    // anxious x4, calm x2, angry x2, rare x1.
    for mood in [
        "anxious", "anxious", "calm", "angry", "anxious", "calm", "angry", "anxious", "rare",
    ] {
        run(&format!("CREATE (:Thought {{mood: '{}'}})", mood), &db);
    }
    // Stage 1 narrows scope to mood; stage 2 aggregates and keeps c >= 3.
    let rows = run(
        "MATCH (t:Thought) WITH t.mood AS mood WITH mood, count(*) AS c WHERE c >= 3 RETURN mood, c ORDER BY mood",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "anxious");
    assert_eq!(int(&rows, 0, 1), 4);
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_top_acts_by_total_words() {
    let db = db();
    for (act, words) in [
        (1, 1000),
        (1, 1500),
        (1, 1200),
        (2, 2000),
        (2, 1800),
        (3, 500),
    ] {
        run(
            &format!("CREATE (:Scene {{act: {}, words: {}}})", act, words),
            &db,
        );
    }
    // Aggregate per act, then filter top with WHERE, then RETURN top 2 by total.
    let rows = run(
        "MATCH (s:Scene) WITH s.act AS act, sum(s.words) AS total WHERE total >= 2000 RETURN act, total ORDER BY total DESC",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // Act 1: 3700; Act 2: 3800; Act 3: 500 (filtered).
    assert_eq!(int(&rows, 0, 0), 2);
    assert_eq!(int(&rows, 0, 1), 3800);
    assert_eq!(int(&rows, 1, 0), 1);
    assert_eq!(int(&rows, 1, 1), 3700);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_high_load_assignees() {
    let db = db();
    for (a, est) in [
        ("alice", 3),
        ("alice", 5),
        ("alice", 8),
        ("alice", 13),
        ("bob", 2),
        ("bob", 3),
        ("carol", 21),
        ("carol", 13),
        ("carol", 8),
    ] {
        run(
            &format!("CREATE (:Task {{assignee: '{}', estimate: {}}})", a, est),
            &db,
        );
    }
    // Group by assignee, filter >= 15 estimate, return ordered.
    let rows = run(
        "MATCH (t:Task) WITH t.assignee AS who, sum(t.estimate) AS load WHERE load >= 15 RETURN who, load ORDER BY load DESC",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "carol");
    assert_eq!(int(&rows, 0, 1), 42);
    assert_eq!(string(&rows, 1, 0), "alice");
    assert_eq!(int(&rows, 1, 1), 29);
}

#[test]
fn task_manager_with_renaming_for_downstream_use() {
    let db = db();
    run(
        "CREATE (:Task {title: 'T1'})-[:ASSIGNED_TO]->(:User {name: 'alice'})",
        &db,
    );
    run("CREATE (:Task {title: 'T2'})", &db);
    // WITH narrows scope to just `task` and `owner`; downstream
    // reference uses the new names.
    let rows = run(
        "MATCH (t:Task) OPTIONAL MATCH (t)-[:ASSIGNED_TO]->(u:User) WITH t AS task, u AS owner RETURN task.title AS title, owner.name AS user_name ORDER BY title",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "T1");
    assert!(matches!(rows[0][1], Value::String(ref s) if s == "alice"));
    assert_eq!(string(&rows, 1, 0), "T2");
    assert!(matches!(rows[1][1], Value::Null));
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_average_order_value_per_buyer_filtered() {
    let db = db();
    run(
        "CREATE (:Buyer {name: 'acme'})-[:PLACED]->(:Order {total: 1000})",
        &db,
    );
    run(
        "MATCH (b:Buyer {name: 'acme'}) CREATE (b)-[:PLACED]->(:Order {total: 2000})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'globex'})-[:PLACED]->(:Order {total: 100})",
        &db,
    );
    run(
        "MATCH (b:Buyer {name: 'globex'}) CREATE (b)-[:PLACED]->(:Order {total: 200})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'initech'})-[:PLACED]->(:Order {total: 5000})",
        &db,
    );
    // Average per buyer, filter to high-value (>= 1000), order by avg DESC.
    let rows = run(
        "MATCH (b:Buyer)-[:PLACED]->(o:Order) WITH b.name AS buyer, avg(o.total) AS avg_value WHERE avg_value >= 1000 RETURN buyer, avg_value ORDER BY avg_value DESC",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "initech");
    assert!((float(&rows, 0, 1) - 5000.0).abs() < 1e-9);
    assert_eq!(string(&rows, 1, 0), "acme");
    assert!((float(&rows, 1, 1) - 1500.0).abs() < 1e-9);
}

#[test]
fn erp_multi_stage_pipeline_buyer_to_high_volume_product() {
    let db = db();
    run(
        "CREATE (:Buyer {name: 'acme'})-[:PLACED]->(:Order)-[:CONTAINS]->(:Product {sku: 'X'})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'acme'})-[:PLACED]->(:Order)-[:CONTAINS]->(:Product {sku: 'Y'})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'globex'})-[:PLACED]->(:Order)-[:CONTAINS]->(:Product {sku: 'X'})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'globex'})-[:PLACED]->(:Order)-[:CONTAINS]->(:Product {sku: 'X'})",
        &db,
    );
    run(
        "CREATE (:Buyer {name: 'initech'})-[:PLACED]->(:Order)-[:CONTAINS]->(:Product {sku: 'Z'})",
        &db,
    );
    // Stage 1: collapse buyer-order-product to (buyer, sku) pairs.
    // Stage 2: count occurrences of each sku across all orders; keep those with count >= 2.
    let rows = run(
        "MATCH (b:Buyer)-[:PLACED]->(o)-[:CONTAINS]->(p:Product) WITH p.sku AS sku WITH sku, count(*) AS orders WHERE orders >= 2 RETURN sku, orders ORDER BY sku",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "X");
    assert_eq!(int(&rows, 0, 1), 3);
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_severity_distribution_filtered() {
    let db = db();
    for sev in [
        "critical", "critical", "high", "high", "high", "low", "low", "low", "low", "trivial",
    ] {
        run(&format!("CREATE (:Bug {{severity: '{}'}})", sev), &db);
    }
    // Keep severities with at least 3 reports.
    let rows = run(
        "MATCH (b:Bug) WITH b.severity AS sev, count(*) AS c WHERE c >= 3 RETURN sev, c ORDER BY c DESC",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "low");
    assert_eq!(int(&rows, 0, 1), 4);
    assert_eq!(string(&rows, 1, 0), "high");
    assert_eq!(int(&rows, 1, 1), 3);
}

#[test]
fn bug_tracker_top_bugs_by_age_then_severity_aggregation() {
    let db = db();
    for (sev, age) in [
        ("critical", 30),
        ("critical", 5),
        ("high", 10),
        ("high", 200),
        ("low", 500),
    ] {
        run(
            &format!("CREATE (:Bug {{severity: '{}', age: {}}})", sev, age),
            &db,
        );
    }
    // First WITH narrows to the oldest 3; second WITH aggregates by severity.
    let rows = run(
        "MATCH (b:Bug) WITH b.severity AS sev, b.age AS age ORDER BY age DESC LIMIT 3 WITH sev, count(*) AS c RETURN sev, c ORDER BY sev",
        &db,
    );
    // Top 3 by age: (low, 500), (high, 200), (critical, 30) → c=1 each.
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0), "critical");
    assert_eq!(int(&rows, 0, 1), 1);
    assert_eq!(string(&rows, 1, 0), "high");
    assert_eq!(int(&rows, 1, 1), 1);
    assert_eq!(string(&rows, 2, 0), "low");
    assert_eq!(int(&rows, 2, 1), 1);
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn with_drops_pattern_variables_not_in_projection() {
    let db = db();
    run("CREATE (:Person {name: 'A', age: 30})", &db);
    // After WITH p.name AS name, the variable `p` is no longer bound;
    // referencing it downstream must fail.
    let q = parse("MATCH (p:Person) WITH p.name AS name RETURN p.age").expect("parse");
    let res = execute(&q, &db, HashMap::new());
    assert!(res.is_err(), "expected error, got {:?}", res);
}

#[test]
fn with_distinct_dedupes_intermediate_then_aggregates() {
    let db = db();
    // Multiple thoughts share moods; we want count of *distinct* moods total.
    for mood in ["a", "a", "b", "c", "c", "c", "d"] {
        run(&format!("CREATE (:Thought {{mood: '{}'}})", mood), &db);
    }
    let rows = run(
        "MATCH (t:Thought) WITH DISTINCT t.mood AS mood RETURN count(mood) AS distinct_moods",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 4);
}

#[test]
fn with_aggregation_zero_input_emits_single_synthetic_row() {
    let db = db();
    let rows = run("MATCH (n:Nothing) WITH count(*) AS c RETURN c", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 0);
}

#[test]
fn with_chains_carry_intermediate_aliases_through_multiple_stages() {
    let db = db();
    for v in 1..=20 {
        run(&format!("CREATE (:N {{v: {}}})", v), &db);
    }
    // Stage 1: keep evens. Stage 2: keep > 10. Stage 3: sum.
    let rows = run(
        "MATCH (n:N) WITH n.v AS v WHERE v % 2 = 0 WITH v WHERE v > 10 RETURN sum(v) AS total",
        &db,
    );
    // Even & > 10: 12, 14, 16, 18, 20 → sum = 80.
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 80);
}
