//! End-to-end Cypher aggregation tests — Phase 10 task `00066`.
//!
//! Exercises the COUNT / SUM / AVG / MIN / MAX / COLLECT aggregation
//! functions together with implicit GROUP BY semantics across the five
//! drevo target scenario domains (CBT journal, story editor, IT task
//! manager, ERP, bug tracker) plus cross-scenario regressions. The
//! aggregation evaluator is layered on top of the existing `00063`
//! pattern-matching / `00065` WHERE pipeline: every non-aggregation
//! `RETURN` projection becomes an implicit group key; aggregations
//! fold across the bindings in each group, skipping `NULL` per Cypher
//! semantics, and `DISTINCT` deduplicates per-row argument values
//! before folding.

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

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
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
fn cbt_count_thoughts_per_mood() {
    let db = db();
    for (title, mood) in [
        ("T1", "anxious"),
        ("T2", "anxious"),
        ("T3", "angry"),
        ("T4", "calm"),
        ("T5", "anxious"),
        ("T6", "angry"),
    ] {
        run(
            &format!("CREATE (:Thought {{title: '{}', mood: '{}'}})", title, mood),
            &db,
        );
    }
    let rows = run(
        "MATCH (n:Thought) RETURN n.mood AS mood, count(*) AS c ORDER BY mood",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0), "angry");
    assert_eq!(int(&rows, 0, 1), 2);
    assert_eq!(string(&rows, 1, 0), "anxious");
    assert_eq!(int(&rows, 1, 1), 3);
    assert_eq!(string(&rows, 2, 0), "calm");
    assert_eq!(int(&rows, 2, 1), 1);
}

#[test]
fn cbt_average_intensity_per_mood_skips_nulls() {
    let db = db();
    for (mood, intensity) in [
        ("anxious", Some(8)),
        ("anxious", Some(4)),
        ("anxious", None),
        ("angry", Some(9)),
        ("angry", Some(7)),
    ] {
        match intensity {
            Some(i) => run(
                &format!("CREATE (:Thought {{mood: '{}', intensity: {}}})", mood, i),
                &db,
            ),
            None => run(&format!("CREATE (:Thought {{mood: '{}'}})", mood), &db),
        };
    }
    let rows = run(
        "MATCH (n:Thought) RETURN n.mood AS mood, avg(n.intensity) AS avg_i ORDER BY mood",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "angry");
    assert!((float(&rows, 0, 1) - 8.0).abs() < 1e-9);
    assert_eq!(string(&rows, 1, 0), "anxious");
    // null skipped: (8 + 4) / 2 = 6.0
    assert!((float(&rows, 1, 1) - 6.0).abs() < 1e-9);
}

#[test]
fn cbt_distinct_distortions_collected() {
    let db = db();
    for d in [
        "catastrophising",
        "catastrophising",
        "mind_reading",
        "all_or_nothing",
        "mind_reading",
    ] {
        run(&format!("CREATE (:Distortion {{kind: '{}'}})", d), &db);
    }
    let rows = run(
        "MATCH (n:Distortion) RETURN collect(DISTINCT n.kind) AS kinds",
        &db,
    );
    assert_eq!(rows.len(), 1);
    let list = match &rows[0][0] {
        Value::List(items) => items.clone(),
        other => panic!("expected list, got {:?}", other),
    };
    let mut kinds: Vec<String> = list
        .into_iter()
        .map(|v| match v {
            Value::String(s) => s,
            other => panic!("expected string, got {:?}", other),
        })
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            "all_or_nothing".to_string(),
            "catastrophising".to_string(),
            "mind_reading".to_string()
        ]
    );
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_total_words_per_act() {
    let db = db();
    for (act, title, words) in [
        (1, "S1", 1200),
        (1, "S2", 800),
        (1, "S3", 1500),
        (2, "S4", 2000),
        (2, "S5", 1700),
        (3, "S6", 3000),
    ] {
        run(
            &format!(
                "CREATE (:Scene {{act: {}, title: '{}', words: {}}})",
                act, title, words
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (n:Scene) RETURN n.act AS act, sum(n.words) AS total ORDER BY act",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(int(&rows, 0, 1), 3500);
    assert_eq!(int(&rows, 1, 0), 2);
    assert_eq!(int(&rows, 1, 1), 3700);
    assert_eq!(int(&rows, 2, 0), 3);
    assert_eq!(int(&rows, 2, 1), 3000);
}

#[test]
fn story_min_max_tension_globally() {
    let db = db();
    for (title, tension) in [("S1", 3), ("S2", 8), ("S3", 5), ("S4", 9), ("S5", 1)] {
        run(
            &format!(
                "CREATE (:Scene {{title: '{}', tension: {}}})",
                title, tension
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (n:Scene) RETURN min(n.tension) AS lo, max(n.tension) AS hi",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(int(&rows, 0, 1), 9);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_open_count_per_assignee() {
    let db = db();
    for (assignee, status) in [
        ("alice", "open"),
        ("alice", "open"),
        ("alice", "closed"),
        ("bob", "open"),
        ("bob", "closed"),
        ("carol", "closed"),
    ] {
        run(
            &format!(
                "CREATE (:Task {{assignee: '{}', status: '{}'}})",
                assignee, status
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (t:Task) WHERE t.status = 'open' RETURN t.assignee AS who, count(*) AS open_count ORDER BY who",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "alice");
    assert_eq!(int(&rows, 0, 1), 2);
    assert_eq!(string(&rows, 1, 0), "bob");
    assert_eq!(int(&rows, 1, 1), 1);
}

#[test]
fn task_manager_total_estimate_with_parameter_filter() {
    let db = db();
    for (assignee, estimate) in [
        ("alice", 3),
        ("alice", 5),
        ("bob", 8),
        ("alice", 13),
        ("bob", 2),
    ] {
        run(
            &format!(
                "CREATE (:Task {{assignee: '{}', estimate: {}}})",
                assignee, estimate
            ),
            &db,
        );
    }
    let mut params = HashMap::new();
    params.insert("who".to_string(), Value::String("alice".into()));
    let rows = run_with(
        "MATCH (t:Task) WHERE t.assignee = $who RETURN sum(t.estimate) AS total",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 21);
}

#[test]
fn task_manager_distinct_assignees_collected() {
    let db = db();
    for assignee in ["alice", "alice", "bob", "carol", "bob", "alice"] {
        run(&format!("CREATE (:Task {{assignee: '{}'}})", assignee), &db);
    }
    let rows = run(
        "MATCH (t:Task) RETURN count(DISTINCT t.assignee) AS unique_assignees",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 3);
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_line_item_total_revenue_per_product() {
    let db = db();
    // Each line item: product + qty * price.
    for (product, qty, price) in [
        ("widget", 2, 100),
        ("widget", 5, 100),
        ("gizmo", 1, 250),
        ("gizmo", 3, 250),
        ("sprocket", 10, 25),
    ] {
        run(
            &format!(
                "CREATE (:LineItem {{product: '{}', qty: {}, price: {}}})",
                product, qty, price
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (li:LineItem) RETURN li.product AS product, sum(li.qty * li.price) AS revenue ORDER BY product",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0), "gizmo");
    assert_eq!(int(&rows, 0, 1), 1000); // 250 + 750
    assert_eq!(string(&rows, 1, 0), "sprocket");
    assert_eq!(int(&rows, 1, 1), 250);
    assert_eq!(string(&rows, 2, 0), "widget");
    assert_eq!(int(&rows, 2, 1), 700); // 200 + 500
}

#[test]
fn erp_top_buyer_by_count_with_order_limit() {
    let db = db();
    for buyer in ["acme", "acme", "acme", "globex", "globex", "initech"] {
        run(&format!("CREATE (:Order {{buyer: '{}'}})", buyer), &db);
    }
    let rows = run(
        "MATCH (o:Order) RETURN o.buyer AS buyer, count(*) AS orders ORDER BY orders DESC LIMIT 1",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "acme");
    assert_eq!(int(&rows, 0, 1), 3);
}

#[test]
fn erp_avg_line_item_price_per_warehouse_via_relationship() {
    let db = db();
    run("CREATE (:Warehouse {name: 'WH1'})", &db);
    run("CREATE (:Warehouse {name: 'WH2'})", &db);
    for (wh, item, price) in [
        ("WH1", "I1", 100),
        ("WH1", "I2", 200),
        ("WH1", "I3", 300),
        ("WH2", "I4", 1000),
        ("WH2", "I5", 2000),
    ] {
        run(
            &format!(
                "MATCH (w:Warehouse {{name: '{}'}}) CREATE (w)-[:STOCKS]->(:Item {{title: '{}', price: {}}})",
                wh, item, price
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (w:Warehouse)-[:STOCKS]->(i:Item) RETURN w.name AS wh, avg(i.price) AS avg_price ORDER BY wh",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "WH1");
    assert!((float(&rows, 0, 1) - 200.0).abs() < 1e-9);
    assert_eq!(string(&rows, 1, 0), "WH2");
    assert!((float(&rows, 1, 1) - 1500.0).abs() < 1e-9);
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_count_per_severity() {
    let db = db();
    for sev in [
        "critical", "critical", "high", "high", "high", "low", "low", "low", "low",
    ] {
        run(&format!("CREATE (:Bug {{severity: '{}'}})", sev), &db);
    }
    let rows = run(
        "MATCH (b:Bug) RETURN b.severity AS sev, count(*) AS c ORDER BY sev",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(string(&rows, 0, 0), "critical");
    assert_eq!(int(&rows, 0, 1), 2);
    assert_eq!(string(&rows, 1, 0), "high");
    assert_eq!(int(&rows, 1, 1), 3);
    assert_eq!(string(&rows, 2, 0), "low");
    assert_eq!(int(&rows, 2, 1), 4);
}

#[test]
fn bug_tracker_open_bug_titles_collected_per_component() {
    let db = db();
    for (component, title, status) in [
        ("auth", "Login fails", "open"),
        ("auth", "Token leaks", "open"),
        ("auth", "Old issue", "closed"),
        ("billing", "Tax wrong", "open"),
        ("billing", "Invoice missing", "closed"),
    ] {
        run(
            &format!(
                "CREATE (:Bug {{component: '{}', title: '{}', status: '{}'}})",
                component, title, status
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (b:Bug) WHERE b.status = 'open' RETURN b.component AS comp, collect(b.title) AS titles ORDER BY comp",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "auth");
    let auth_titles = match &rows[0][1] {
        Value::List(items) => items.clone(),
        other => panic!("expected list, got {:?}", other),
    };
    let mut auth_strs: Vec<String> = auth_titles
        .into_iter()
        .map(|v| match v {
            Value::String(s) => s,
            other => panic!("expected string, got {:?}", other),
        })
        .collect();
    auth_strs.sort();
    assert_eq!(
        auth_strs,
        vec!["Login fails".to_string(), "Token leaks".to_string()]
    );
    assert_eq!(string(&rows, 1, 0), "billing");
    let billing_titles = match &rows[1][1] {
        Value::List(items) => items.clone(),
        other => panic!("expected list, got {:?}", other),
    };
    assert_eq!(billing_titles.len(), 1);
}

#[test]
fn bug_tracker_min_max_priority_score_globally() {
    let db = db();
    for score in [3, 1, 4, 1, 5, 9, 2, 6] {
        run(&format!("CREATE (:Bug {{score: {}}})", score), &db);
    }
    let rows = run(
        "MATCH (b:Bug) RETURN min(b.score) AS lo, max(b.score) AS hi",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(int(&rows, 0, 1), 9);
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn aggregation_combined_with_where_and_order_by() {
    let db = db();
    for (kind, score) in [
        ("a", 10),
        ("a", 20),
        ("a", 5),
        ("b", 100),
        ("b", 200),
        ("c", 50),
        ("c", 5),
    ] {
        run(
            &format!("CREATE (:Item {{kind: '{}', score: {}}})", kind, score),
            &db,
        );
    }
    let rows = run(
        "MATCH (n:Item) WHERE n.score >= 10 RETURN n.kind AS k, sum(n.score) AS total ORDER BY total DESC",
        &db,
    );
    assert_eq!(rows.len(), 3);
    // Filter score >= 10: a=[10,20] (sum 30), b=[100,200] (sum 300), c=[50] (sum 50).
    // ORDER BY total DESC → b, c, a.
    assert_eq!(string(&rows, 0, 0), "b");
    assert_eq!(int(&rows, 0, 1), 300);
    assert_eq!(string(&rows, 1, 0), "c");
    assert_eq!(int(&rows, 1, 1), 50);
    assert_eq!(string(&rows, 2, 0), "a");
    assert_eq!(int(&rows, 2, 1), 30);
}

#[test]
fn aggregation_on_empty_match_emits_one_zero_row() {
    let db = db();
    let rows = run(
        "MATCH (n:Person) RETURN count(*) AS c, sum(n.score) AS s",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 0);
    assert_eq!(int(&rows, 0, 1), 0);
}

#[test]
fn multi_aggregation_per_group_returns_all_columns() {
    let db = db();
    for (group, val) in [("a", 10), ("a", 20), ("a", 30), ("b", 50), ("b", 50)] {
        run(&format!("CREATE (:N {{g: '{}', v: {}}})", group, val), &db);
    }
    let rows = run(
        "MATCH (n:N) RETURN n.g AS g, count(*) AS c, sum(n.v) AS s, avg(n.v) AS a, min(n.v) AS lo, max(n.v) AS hi ORDER BY g",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // a: count=3, sum=60, avg=20.0, min=10, max=30
    assert_eq!(string(&rows, 0, 0), "a");
    assert_eq!(int(&rows, 0, 1), 3);
    assert_eq!(int(&rows, 0, 2), 60);
    assert!((float(&rows, 0, 3) - 20.0).abs() < 1e-9);
    assert_eq!(int(&rows, 0, 4), 10);
    assert_eq!(int(&rows, 0, 5), 30);
    // b: count=2, sum=100, avg=50.0, min=50, max=50
    assert_eq!(string(&rows, 1, 0), "b");
    assert_eq!(int(&rows, 1, 1), 2);
    assert_eq!(int(&rows, 1, 2), 100);
    assert!((float(&rows, 1, 3) - 50.0).abs() < 1e-9);
    assert_eq!(int(&rows, 1, 4), 50);
    assert_eq!(int(&rows, 1, 5), 50);
}
