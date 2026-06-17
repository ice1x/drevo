//! End-to-end Cypher tests for **aggregations nested inside a `CASE` arm** —
//! Phase 10 follow-up task `00142`.
//!
//! A `CASE` whose scrutinee, `WHEN`, `THEN`, or `ELSE` sub-expression
//! contains an aggregation function (`count(*)`, `sum(x)`, `avg(x)`, …) is
//! folded over the current group, exactly like a bare aggregation column:
//!
//! ```cypher
//! MATCH (t:Task)
//! RETURN t.status AS status,
//!        CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END AS load
//! ```
//!
//! Before `00142` such a query returned `ExecError::Unsupported`; the `CASE`
//! sub-expressions were validated with the non-aggregation validator and
//! `eval_with_agg` did not fold them. Now the projection validator accepts an
//! aggregation in any arm (an aggregation nested *inside another* aggregation
//! is still rejected, matching Neo4j) and `eval_with_agg` reduces each arm
//! across the group.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics (group keys,
//! the simple form, `ELSE` folding, empty groups, `WITH` filtering, type
//! errors, and parameterised arms).

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

fn run_params(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
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

fn float(rows: &[Vec<Value>], r: usize, c: usize) -> f64 {
    match &rows[r][c] {
        Value::Float(f) => *f,
        other => panic!("expected float at ({r},{c}), got {other:?}"),
    }
}

fn string(rows: &[Vec<Value>], r: usize, c: usize) -> String {
    match &rows[r][c] {
        Value::String(s) => s.clone(),
        other => panic!("expected string at ({r},{c}), got {other:?}"),
    }
}

// ===== CBT journal ==========================================================

/// Classify how often a cognitive distortion appears across journal entries:
/// `count(*)` inside the `WHEN` condition decides the textual label.
#[test]
fn cbt_distortion_frequency_label() {
    let db = db();
    exec(
        "CREATE (:Thought {distortion: 'catastrophizing'}), \
         (:Thought {distortion: 'catastrophizing'}), \
         (:Thought {distortion: 'catastrophizing'}), \
         (:Thought {distortion: 'mind-reading'})",
        &db,
    );
    let mut rows = run(
        "MATCH (t:Thought) \
         RETURN t.distortion AS distortion, \
                CASE WHEN count(*) >= 3 THEN 'frequent' \
                     WHEN count(*) = 2 THEN 'occasional' \
                     ELSE 'rare' END AS frequency \
         ORDER BY distortion",
        &db,
    );
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(rows.len(), 2);
    // catastrophizing ×3 → frequent, mind-reading ×1 → rare
    let cata = rows
        .iter()
        .find(|r| matches!(&r[0], Value::String(s) if s == "catastrophizing"))
        .unwrap();
    assert_eq!(cata[1], Value::String("frequent".into()));
    let mind = rows
        .iter()
        .find(|r| matches!(&r[0], Value::String(s) if s == "mind-reading"))
        .unwrap();
    assert_eq!(mind[1], Value::String("rare".into()));
}

// ===== Story / book editor ==================================================

/// Per-book chapter count drives a "multi-chapter" vs "single-chapter" tag.
#[test]
fn story_chapter_count_tag_per_book() {
    let db = db();
    exec(
        "CREATE (:Chapter {book: 'Dune', title: 'Arrakis'}), \
         (:Chapter {book: 'Dune', title: 'Muad-Dib'}), \
         (:Chapter {book: 'Dune', title: 'The Prophet'}), \
         (:Chapter {book: 'Novella', title: 'Only One'})",
        &db,
    );
    let rows = run(
        "MATCH (c:Chapter) \
         RETURN c.book AS book, \
                CASE WHEN count(*) > 1 THEN 'multi-chapter' ELSE 'single-chapter' END AS shape \
         ORDER BY book",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "Dune");
    assert_eq!(string(&rows, 0, 1), "multi-chapter");
    assert_eq!(string(&rows, 1, 0), "Novella");
    assert_eq!(string(&rows, 1, 1), "single-chapter");
}

// ===== IT task manager ======================================================

/// Group tasks by status and fold `count(*)` inside the `CASE` to label load.
#[test]
fn task_status_load_label() {
    let db = db();
    exec(
        "CREATE (:Task {status: 'open'}), (:Task {status: 'open'}), \
         (:Task {status: 'open'}), (:Task {status: 'done'})",
        &db,
    );
    let rows = run(
        "MATCH (t:Task) \
         RETURN t.status AS status, \
                CASE WHEN count(*) > 2 THEN 'overloaded' ELSE 'normal' END AS load \
         ORDER BY status",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "done");
    assert_eq!(string(&rows, 0, 1), "normal");
    assert_eq!(string(&rows, 1, 0), "open");
    assert_eq!(string(&rows, 1, 1), "overloaded");
}

/// `THEN` folds the aggregation: total story points when the group is big
/// enough, otherwise a sentinel.
#[test]
fn task_then_sums_points_for_large_sprints() {
    let db = db();
    exec(
        "CREATE (:Story {sprint: 'S1', points: 5}), \
         (:Story {sprint: 'S1', points: 8}), \
         (:Story {sprint: 'S1', points: 3}), \
         (:Story {sprint: 'S2', points: 13})",
        &db,
    );
    let rows = run(
        "MATCH (s:Story) \
         RETURN s.sprint AS sprint, \
                CASE WHEN count(*) >= 3 THEN sum(s.points) ELSE -1 END AS committed \
         ORDER BY sprint",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "S1");
    assert_eq!(int(&rows, 0, 1), 16); // 5 + 8 + 3
    assert_eq!(string(&rows, 1, 0), "S2");
    assert_eq!(int(&rows, 1, 1), -1); // only one story
}

// ===== ERP ==================================================================

/// Average order value per category, bucketed via the simple `CASE` form over
/// an aggregated scrutinee is awkward, so use the searched form on `avg`.
#[test]
fn erp_category_avg_value_bucket() {
    let db = db();
    exec(
        "CREATE (:Order {category: 'hardware', total: 100.0}), \
         (:Order {category: 'hardware', total: 300.0}), \
         (:Order {category: 'software', total: 50.0})",
        &db,
    );
    let rows = run(
        "MATCH (o:Order) \
         RETURN o.category AS category, \
                CASE WHEN avg(o.total) >= 200.0 THEN 'high-value' ELSE 'standard' END AS tier \
         ORDER BY category",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "hardware");
    assert_eq!(string(&rows, 0, 1), "high-value"); // avg = 200.0
    assert_eq!(string(&rows, 1, 0), "software");
    assert_eq!(string(&rows, 1, 1), "standard"); // avg = 50.0
}

/// `ELSE` branch folds the aggregation when no `WHEN` matches.
#[test]
fn erp_else_branch_folds_revenue() {
    let db = db();
    exec("CREATE (:Sale {amount: 40}), (:Sale {amount: 60})", &db);
    let rows = run(
        "MATCH (s:Sale) \
         RETURN CASE WHEN false THEN 0 ELSE sum(s.amount) END AS revenue",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 100);
}

// ===== Bug tracker ==========================================================

/// Per-severity bug counts label triage urgency; the simple `CASE` form uses
/// an aggregated scrutinee.
#[test]
fn bug_severity_count_simple_form() {
    let db = db();
    exec(
        "CREATE (:Bug {severity: 'critical'}), (:Bug {severity: 'critical'}), \
         (:Bug {severity: 'low'})",
        &db,
    );
    let rows = run(
        "MATCH (b:Bug) \
         RETURN b.severity AS severity, \
                CASE count(*) WHEN 1 THEN 'isolated' WHEN 2 THEN 'paired' ELSE 'cluster' END AS grouping \
         ORDER BY severity",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(string(&rows, 0, 0), "critical");
    assert_eq!(string(&rows, 0, 1), "paired");
    assert_eq!(string(&rows, 1, 0), "low");
    assert_eq!(string(&rows, 1, 1), "isolated");
}

// ===== Cross-cutting semantics ==============================================

/// A pure-aggregation query (no group key) still emits one synthetic group,
/// so the `CASE` evaluates against `count(*) = 0` over zero matches.
#[test]
fn empty_group_selects_else() {
    let db = db();
    let rows = run(
        "MATCH (n:Missing) \
         RETURN CASE WHEN count(*) > 0 THEN 'present' ELSE 'absent' END AS state",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "absent");
}

/// `collect(...)` inside a `THEN` folds into a list.
#[test]
fn then_collects_group_into_list() {
    let db = db();
    exec(
        "CREATE (:Item {tag: 'a', name: 'x'}), (:Item {tag: 'a', name: 'y'})",
        &db,
    );
    let rows = run(
        "MATCH (i:Item) \
         RETURN CASE WHEN count(*) > 0 THEN collect(i.name) ELSE [] END AS names",
        &db,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::List(items) => {
            assert_eq!(items.len(), 2);
            assert!(items.contains(&Value::String("x".into())));
            assert!(items.contains(&Value::String("y".into())));
        }
        other => panic!("expected list, got {other:?}"),
    }
}

/// A `CASE`-with-aggregation column survives a post-aggregation `WITH … WHERE`.
#[test]
fn aggregation_case_filterable_via_with() {
    let db = db();
    exec(
        "CREATE (:T {status: 'open'}), (:T {status: 'open'}), (:T {status: 'done'})",
        &db,
    );
    let rows = run(
        "MATCH (t:T) \
         WITH t.status AS status, \
              CASE WHEN count(*) > 1 THEN 'many' ELSE 'one' END AS load \
         WHERE load = 'many' \
         RETURN status",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "open");
}

/// A non-boolean `WHEN` in the searched form is a type error even when the
/// condition involves an aggregation.
#[test]
fn non_boolean_when_with_aggregation_is_type_error() {
    let db = db();
    exec("CREATE (:N), (:N)", &db);
    let e = run_err(
        "MATCH (n:N) RETURN CASE WHEN count(*) THEN 'x' ELSE 'y' END AS r",
        &db,
    );
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

/// An aggregation nested *inside another* aggregation, reached through a
/// `CASE` arm, stays rejected — Cypher forbids nested aggregates.
#[test]
fn nested_aggregation_through_case_is_rejected() {
    let db = db();
    exec("CREATE (:N), (:N)", &db);
    let e = run_err(
        "MATCH (n:N) RETURN sum(CASE WHEN true THEN count(*) ELSE 0 END) AS r",
        &db,
    );
    assert!(matches!(e, ExecError::InvalidMutation(_)), "{e:?}");
}

/// A parameter is usable inside a `CASE` arm alongside an aggregation.
#[test]
fn parameter_threshold_in_case_with_aggregation() {
    let db = db();
    exec("CREATE (:N), (:N), (:N)", &db);
    let mut params = HashMap::new();
    params.insert("threshold".to_string(), Value::Integer(2));
    let rows = run_params(
        "MATCH (n:N) RETURN CASE WHEN count(*) > $threshold THEN 'over' ELSE 'under' END AS r",
        &db,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "over");
}

/// Averaging inside a `THEN` returns a float; verifies numeric folding path.
#[test]
fn then_returns_avg_float() {
    let db = db();
    exec("CREATE (:R {v: 2.0}), (:R {v: 4.0}), (:R {v: 6.0})", &db);
    let rows = run(
        "MATCH (r:R) RETURN CASE WHEN count(*) = 3 THEN avg(r.v) ELSE 0.0 END AS mean",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert!((float(&rows, 0, 0) - 4.0).abs() < 1e-9);
}

/// Two aggregating `CASE` columns in one projection both fold over the group.
#[test]
fn two_aggregating_case_columns() {
    let db = db();
    exec(
        "CREATE (:E {dept: 'eng', salary: 100}), \
         (:E {dept: 'eng', salary: 200}), \
         (:E {dept: 'eng', salary: 300})",
        &db,
    );
    let rows = run(
        "MATCH (e:E) \
         RETURN e.dept AS dept, \
                CASE WHEN count(*) > 2 THEN 'team' ELSE 'solo' END AS size, \
                CASE WHEN sum(e.salary) > 500 THEN 'expensive' ELSE 'lean' END AS cost \
         ORDER BY dept",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows, 0, 0), "eng");
    assert_eq!(string(&rows, 0, 1), "team");
    assert_eq!(string(&rows, 0, 2), "expensive"); // 600 > 500
}
