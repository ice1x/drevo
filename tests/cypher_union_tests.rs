//! End-to-end Cypher `UNION` / `UNION ALL` tests — Phase 10 follow-up task `00136`.
//!
//! `UNION` combines the result rows of two or more single-queries into a
//! single result set. `UNION ALL` concatenates every arm's rows in arm
//! order (duplicates preserved); `UNION` (the distinct form) additionally
//! removes duplicate rows across the *combined* set. All arms must project
//! the same column names in the same order, and a query may not mix
//! `UNION` and `UNION ALL` (Neo4j parity).
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics (duplicate
//! handling, column-name matching, arm ordering, stats accumulation).

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_full(source: &str, drevo: &Drevo) -> ExecResult {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute")
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({r},{c}), got {other:?}"),
    }
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> String {
    match &rows[r][c] {
        Value::String(s) => s.clone(),
        other => panic!("expected string at ({r},{c}), got {other:?}"),
    }
}

/// Collect column `c` of every row as a string vector (for set comparisons).
fn col_strings(rows: &[Vec<Value>], c: usize) -> Vec<String> {
    (0..rows.len()).map(|r| string(rows, r, c)).collect()
}

// ===== Core semantics =======================================================

#[test]
fn union_all_concatenates_literal_arms_in_order() {
    let db = db();
    let rows = run(
        "RETURN 1 AS n UNION ALL RETURN 2 AS n UNION ALL RETURN 3 AS n",
        &db,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(int(&rows, 1, 0), 2);
    assert_eq!(int(&rows, 2, 0), 3);
}

#[test]
fn union_all_preserves_duplicate_rows() {
    let db = db();
    let rows = run("RETURN 1 AS n UNION ALL RETURN 1 AS n", &db);
    assert_eq!(rows.len(), 2);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(int(&rows, 1, 0), 1);
}

#[test]
fn union_distinct_removes_duplicate_rows_across_arms() {
    let db = db();
    let rows = run("RETURN 1 AS n UNION RETURN 1 AS n", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 1);
}

#[test]
fn union_distinct_keeps_first_occurrence_order() {
    let db = db();
    // 2 appears in both arms; distinct collapses it to the first sighting.
    let rows = run(
        "RETURN 1 AS n UNION RETURN 2 AS n UNION RETURN 1 AS n UNION RETURN 3 AS n",
        &db,
    );
    let got: Vec<i64> = (0..rows.len()).map(|r| int(&rows, r, 0)).collect();
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn union_columns_take_first_arm_names() {
    let db = db();
    let res = run_full("RETURN 1 AS n UNION ALL RETURN 2 AS n", &db);
    assert_eq!(res.columns, vec!["n".to_string()]);
}

#[test]
fn union_combines_multiple_columns() {
    let db = db();
    let rows = run(
        "RETURN 1 AS a, 'x' AS b UNION ALL RETURN 2 AS a, 'y' AS b",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(int(&rows, 0, 0), 1);
    assert_eq!(string(&rows, 0, 1), "x");
    assert_eq!(int(&rows, 1, 0), 2);
    assert_eq!(string(&rows, 1, 1), "y");
}

// ===== Error model ==========================================================

#[test]
fn union_with_mismatched_column_names_is_an_error() {
    let db = db();
    let err = run_err("RETURN 1 AS a UNION RETURN 2 AS b", &db);
    assert!(
        matches!(err, ExecError::UnionMismatch { .. }),
        "expected UnionMismatch, got {err:?}"
    );
}

#[test]
fn union_with_swapped_column_order_is_an_error() {
    let db = db();
    // Same names, different order — Neo4j rejects this.
    let err = run_err("RETURN 1 AS a, 2 AS b UNION RETURN 3 AS b, 4 AS a", &db);
    assert!(matches!(err, ExecError::UnionMismatch { .. }));
}

#[test]
fn union_with_different_column_count_is_an_error() {
    let db = db();
    let err = run_err("RETURN 1 AS a UNION RETURN 2 AS a, 3 AS b", &db);
    assert!(matches!(err, ExecError::UnionMismatch { .. }));
}

#[test]
fn mixing_union_and_union_all_is_an_error() {
    let db = db();
    let err = run_err(
        "RETURN 1 AS n UNION RETURN 2 AS n UNION ALL RETURN 3 AS n",
        &db,
    );
    assert!(
        matches!(err, ExecError::UnionMismatch { .. }),
        "expected UnionMismatch for mixed UNION/UNION ALL, got {err:?}"
    );
}

#[test]
fn union_mismatch_reports_a_span() {
    let db = db();
    let err = run_err("RETURN 1 AS a UNION RETURN 2 AS b", &db);
    assert!(err.span().is_some(), "UnionMismatch should carry a span");
}

#[test]
fn unsupported_inside_a_union_arm_still_surfaces() {
    let db = db();
    // A function with no executor implementation (an unknown name now that the
    // `00138` built-ins plus `similar` / `keywords` are all recognised) is
    // still unsupported; the upfront sweep must fire per-arm.
    let err = run_err("RETURN 1 AS n UNION RETURN nosuchfn('x') AS n", &db);
    assert!(matches!(err, ExecError::Unsupported { .. }));
}

// ===== Regression — single-arm queries unaffected ===========================

#[test]
fn single_query_without_union_is_unchanged() {
    let db = db();
    let rows = run("RETURN 42 AS answer", &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 42);
}

// ===== Graph reads combined by UNION ========================================

#[test]
fn union_all_merges_two_label_scans() {
    let db = db();
    exec("CREATE (:Cat {title: 'Whiskers'})", &db);
    exec("CREATE (:Dog {title: 'Rex'})", &db);
    let rows = run(
        "MATCH (c:Cat) RETURN c.title AS name \
         UNION ALL \
         MATCH (d:Dog) RETURN d.title AS name",
        &db,
    );
    let names = col_strings(&rows, 0);
    assert_eq!(names, vec!["Whiskers".to_string(), "Rex".to_string()]);
}

#[test]
fn union_distinct_deduplicates_overlapping_graph_rows() {
    let db = db();
    // Two people, each tagged twice by overlapping criteria; UNION collapses.
    exec(
        "CREATE (:Person {title: 'Ada', active: true, vip: true})",
        &db,
    );
    exec(
        "CREATE (:Person {title: 'Grace', active: true, vip: false})",
        &db,
    );
    let rows = run(
        "MATCH (p:Person) WHERE p.active = true RETURN p.title AS name \
         UNION \
         MATCH (p:Person) WHERE p.vip = true RETURN p.title AS name",
        &db,
    );
    // Ada matches both arms but appears once; Grace matches only the first.
    let mut names = col_strings(&rows, 0);
    names.sort();
    assert_eq!(names, vec!["Ada".to_string(), "Grace".to_string()]);
}

// ===== Scenario-domain coverage =============================================

#[test]
fn cbt_journal_combine_distortions_and_emotions() {
    let db = db();
    exec("CREATE (:Distortion {title: 'Catastrophizing'})", &db);
    exec("CREATE (:Emotion {title: 'Anxiety'})", &db);
    let rows = run(
        "MATCH (d:Distortion) RETURN d.title AS label \
         UNION ALL \
         MATCH (e:Emotion) RETURN e.title AS label",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert!(col_strings(&rows, 0).contains(&"Catastrophizing".to_string()));
    assert!(col_strings(&rows, 0).contains(&"Anxiety".to_string()));
}

#[test]
fn story_editor_combine_chapters_from_two_books() {
    let db = db();
    exec(
        "CREATE (:Chapter {title: 'A Shadow Falls', book: 'One'})",
        &db,
    );
    exec(
        "CREATE (:Chapter {title: 'The Long Road', book: 'Two'})",
        &db,
    );
    let rows = run(
        "MATCH (c:Chapter) WHERE c.book = 'One' RETURN c.title AS chapter \
         UNION ALL \
         MATCH (c:Chapter) WHERE c.book = 'Two' RETURN c.title AS chapter",
        &db,
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn task_manager_combine_blocked_and_in_progress() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'Ship UNION', status: 'in_progress'})",
        &db,
    );
    exec(
        "CREATE (:Task {title: 'Write docs', status: 'blocked'})",
        &db,
    );
    exec("CREATE (:Task {title: 'Old idea', status: 'done'})", &db);
    let rows = run(
        "MATCH (t:Task) WHERE t.status = 'in_progress' RETURN t.title AS task \
         UNION \
         MATCH (t:Task) WHERE t.status = 'blocked' RETURN t.title AS task",
        &db,
    );
    let mut tasks = col_strings(&rows, 0);
    tasks.sort();
    assert_eq!(
        tasks,
        vec!["Ship UNION".to_string(), "Write docs".to_string()]
    );
}

#[test]
fn erp_combine_two_product_categories() {
    let db = db();
    exec(
        "CREATE (:Product {title: 'Bolt M6', category: 'fasteners', price: 5})",
        &db,
    );
    exec(
        "CREATE (:Product {title: 'Washer', category: 'fasteners', price: 2})",
        &db,
    );
    exec(
        "CREATE (:Product {title: 'Gear', category: 'transmission', price: 40})",
        &db,
    );
    let rows = run(
        "MATCH (p:Product) WHERE p.category = 'fasteners' RETURN p.title AS item \
         UNION ALL \
         MATCH (p:Product) WHERE p.category = 'transmission' RETURN p.title AS item",
        &db,
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn bug_tracker_open_plus_recently_closed() {
    let db = db();
    exec("CREATE (:Bug {title: 'Crash on save', state: 'open'})", &db);
    exec("CREATE (:Bug {title: 'Typo in UI', state: 'open'})", &db);
    exec("CREATE (:Bug {title: 'Memory leak', state: 'closed'})", &db);
    let rows = run(
        "MATCH (b:Bug) WHERE b.state = 'open' RETURN b.title AS bug \
         UNION ALL \
         MATCH (b:Bug) WHERE b.state = 'closed' RETURN b.title AS bug",
        &db,
    );
    assert_eq!(rows.len(), 3);
}

// ===== UNION composed with UNWIND (00135) ===================================

#[test]
fn union_all_combines_unwind_arms() {
    let db = db();
    let rows = run(
        "UNWIND [1, 2] AS n RETURN n \
         UNION ALL \
         UNWIND [3, 4] AS n RETURN n",
        &db,
    );
    let got: Vec<i64> = (0..rows.len()).map(|r| int(&rows, r, 0)).collect();
    assert_eq!(got, vec![1, 2, 3, 4]);
}

// ===== Stats accumulation across arms =======================================

#[test]
fn union_accumulates_mutation_stats_across_arms() {
    let db = db();
    // Each arm creates a node, then returns a matching label row so the
    // column names line up; stats must sum both creations.
    let res = run_full(
        "CREATE (:Note {title: 'Alpha'}) RETURN 1 AS k \
         UNION ALL \
         CREATE (:Note {title: 'Beta'}) RETURN 2 AS k",
        &db,
    );
    assert_eq!(res.stats.nodes_created, 2);
    assert_eq!(res.rows.len(), 2);
}
