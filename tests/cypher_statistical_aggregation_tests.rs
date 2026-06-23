//! End-to-end Cypher statistical-aggregation tests — Phase 10 follow-up task
//! `00154`.
//!
//! `00066` shipped the core aggregation set (`count` / `sum` / `avg` / `min` /
//! `max` / `collect`). This task adds the four standard Neo4j *statistical*
//! aggregations, which fold a group to a scalar exactly like `avg`:
//!
//! * `stDev(expr)` — **sample** standard deviation (divides the sum of squared
//!   deviations by `n - 1`),
//! * `stDevP(expr)` — **population** standard deviation (divides by `n`),
//! * `percentileCont(expr, p)` — the **continuous** percentile of the group at
//!   fraction `p ∈ [0, 1]`, linearly interpolating between the two closest
//!   ranks (so the median `p = 0.5` of `[1, 2, 3, 4]` is `2.5`),
//! * `percentileDisc(expr, p)` — the **discrete** percentile: the nearest
//!   actual value at fraction `p` (no interpolation, so the value's stored type
//!   — `Integer` vs `Float` — is preserved).
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * every function **null-skips** its input (an absent property never aborts a
//!   heterogeneous scan) and rejects a non-numeric value with a recoverable
//!   `TypeMismatch`,
//! * `stDev` / `stDevP` over an empty (all-null) group are `0.0`, and `stDev`
//!   of a single value is `0.0` (the `n - 1` divisor is special-cased),
//! * `percentileCont` / `percentileDisc` over an empty group are `null`,
//! * the percentile fraction must be a number in `[0.0, 1.0]` — anything else
//!   is a recoverable `InvalidFunctionCall`,
//! * a non-aggregated projection item is the `GROUP BY` key, so the statistic
//!   is computed per group, and the statistics coexist with the existing
//!   aggregations in one `RETURN`,
//!
//! plus the five scenario domains (CBT journal, story editor, task manager,
//! ERP, bug tracker) the drevo Cypher suite standardises on.

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

/// Execute, expecting exactly one row with one column, and return that value.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from `{source}`");
    assert_eq!(
        rows[0].len(),
        1,
        "expected exactly one column from `{source}`"
    );
    rows[0][0].clone()
}

/// Pull an `f64` out of a `Value::Float` (statistics always return `Float`,
/// except `percentileDisc`, which preserves the stored type).
fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Integer(i) => *i as f64,
        other => panic!("expected a numeric value, got {other:?}"),
    }
}

fn approx(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
}

fn err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected an execution error")
}

/// The textbook eight-value sample whose population stdev is exactly `2.0`.
fn make_textbook(drevo: &Drevo) {
    for v in [2, 4, 4, 4, 5, 5, 7, 9] {
        run(&format!("CREATE (:M {{v: {v}}})"), drevo);
    }
}

// ---------------------------------------------------------------------------
// stDev / stDevP
// ---------------------------------------------------------------------------

#[test]
fn stdevp_population_standard_deviation() {
    let db = db();
    make_textbook(&db);
    // mean = 5, sum of squared deviations = 32, population variance = 32/8 = 4.
    approx(as_f64(&one("MATCH (n:M) RETURN stDevP(n.v)", &db)), 2.0);
}

#[test]
fn stdev_sample_standard_deviation() {
    let db = db();
    make_textbook(&db);
    // sample variance = 32 / 7, stdev = sqrt(32/7).
    approx(
        as_f64(&one("MATCH (n:M) RETURN stDev(n.v)", &db)),
        (32.0f64 / 7.0).sqrt(),
    );
}

#[test]
fn stdev_of_single_value_is_zero() {
    let db = db();
    run("CREATE (:M {v: 42})", &db);
    // n - 1 = 0 divisor is special-cased to 0.0 (matching Neo4j).
    approx(as_f64(&one("MATCH (n:M) RETURN stDev(n.v)", &db)), 0.0);
    approx(as_f64(&one("MATCH (n:M) RETURN stDevP(n.v)", &db)), 0.0);
}

#[test]
fn stdev_over_empty_group_is_zero() {
    let db = db();
    run("CREATE (:M {other: 1})", &db);
    run("CREATE (:M {other: 2})", &db);
    // `n.v` is absent on every row → all values null-skipped → empty fold.
    approx(as_f64(&one("MATCH (n:M) RETURN stDev(n.v)", &db)), 0.0);
    approx(as_f64(&one("MATCH (n:M) RETURN stDevP(n.v)", &db)), 0.0);
}

#[test]
fn stdev_null_skips_absent_values() {
    let db = db();
    run("CREATE (:M {v: 2})", &db);
    run("CREATE (:M {other: 1})", &db); // null v — skipped
    run("CREATE (:M {v: 4})", &db);
    run("CREATE (:M {other: 2})", &db); // null v — skipped
                                        // Effective sample is [2, 4]: mean 3, ss = 1 + 1 = 2, sample var = 2/1 = 2.
    approx(
        as_f64(&one("MATCH (n:M) RETURN stDev(n.v)", &db)),
        2.0f64.sqrt(),
    );
}

#[test]
fn stdevp_accepts_floats() {
    let db = db();
    for v in ["1.0", "2.0", "3.0"] {
        run(&format!("CREATE (:M {{v: {v}}})"), &db);
    }
    // mean 2, ss = 1 + 0 + 1 = 2, population var = 2/3.
    approx(
        as_f64(&one("MATCH (n:M) RETURN stDevP(n.v)", &db)),
        (2.0f64 / 3.0).sqrt(),
    );
}

#[test]
fn stdev_rejects_non_numeric_value() {
    let db = db();
    run("CREATE (:M {v: 'not a number'})", &db);
    assert!(matches!(
        err("MATCH (n:M) RETURN stDev(n.v)", &db),
        ExecError::TypeMismatch { .. }
    ));
}

// ---------------------------------------------------------------------------
// percentileCont
// ---------------------------------------------------------------------------

fn make_quartet(drevo: &Drevo) {
    for v in [1, 2, 3, 4] {
        run(&format!("CREATE (:M {{v: {v}}})"), drevo);
    }
}

#[test]
fn percentile_cont_median_interpolates() {
    let db = db();
    make_quartet(&db);
    // p = 0.5 over [1,2,3,4]: float_idx = 0.5*3 = 1.5 → 2*0.5 + 3*0.5 = 2.5.
    approx(
        as_f64(&one("MATCH (n:M) RETURN percentileCont(n.v, 0.5)", &db)),
        2.5,
    );
}

#[test]
fn percentile_cont_zero_and_one_are_min_and_max() {
    let db = db();
    make_quartet(&db);
    approx(
        as_f64(&one("MATCH (n:M) RETURN percentileCont(n.v, 0.0)", &db)),
        1.0,
    );
    approx(
        as_f64(&one("MATCH (n:M) RETURN percentileCont(n.v, 1.0)", &db)),
        4.0,
    );
}

#[test]
fn percentile_cont_interpolates_three_quarter() {
    let db = db();
    make_quartet(&db);
    // p = 0.75: float_idx = 2.25 → 3*0.75 + 4*0.25 = 3.25.
    approx(
        as_f64(&one("MATCH (n:M) RETURN percentileCont(n.v, 0.75)", &db)),
        3.25,
    );
}

#[test]
fn percentile_cont_always_returns_float() {
    let db = db();
    make_quartet(&db);
    // Even at an exact rank, the continuous percentile is a Float.
    assert!(matches!(
        one("MATCH (n:M) RETURN percentileCont(n.v, 0.0)", &db),
        Value::Float(_)
    ));
}

#[test]
fn percentile_cont_over_empty_group_is_null() {
    let db = db();
    run("CREATE (:M {other: 1})", &db);
    assert_eq!(
        one("MATCH (n:M) RETURN percentileCont(n.v, 0.5)", &db),
        Value::Null
    );
}

// ---------------------------------------------------------------------------
// percentileDisc
// ---------------------------------------------------------------------------

#[test]
fn percentile_disc_picks_actual_value() {
    let db = db();
    make_quartet(&db);
    // p = 0.5 over [1,2,3,4]: float_idx = 2.0 (exact, !=0) → idx 1 → value 2.
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 0.5)", &db),
        Value::Integer(2)
    );
}

#[test]
fn percentile_disc_preserves_integer_type() {
    let db = db();
    make_quartet(&db);
    // No interpolation, so an integer column stays an Integer.
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 0.75)", &db),
        Value::Integer(3)
    );
}

#[test]
fn percentile_disc_zero_and_one() {
    let db = db();
    make_quartet(&db);
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 0.0)", &db),
        Value::Integer(1)
    );
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 1.0)", &db),
        Value::Integer(4)
    );
}

#[test]
fn percentile_disc_single_value() {
    let db = db();
    run("CREATE (:M {v: 42})", &db);
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 0.5)", &db),
        Value::Integer(42)
    );
}

#[test]
fn percentile_disc_over_empty_group_is_null() {
    let db = db();
    run("CREATE (:M {other: 1})", &db);
    assert_eq!(
        one("MATCH (n:M) RETURN percentileDisc(n.v, 0.9)", &db),
        Value::Null
    );
}

// ---------------------------------------------------------------------------
// Percentile fraction validation
// ---------------------------------------------------------------------------

#[test]
fn percentile_fraction_above_one_is_an_error() {
    let db = db();
    make_quartet(&db);
    assert!(matches!(
        err("MATCH (n:M) RETURN percentileCont(n.v, 1.5)", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn percentile_fraction_below_zero_is_an_error() {
    let db = db();
    make_quartet(&db);
    assert!(matches!(
        err("MATCH (n:M) RETURN percentileDisc(n.v, -0.1)", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn percentile_fraction_must_be_numeric() {
    let db = db();
    make_quartet(&db);
    assert!(matches!(
        err("MATCH (n:M) RETURN percentileCont(n.v, 'half')", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn percentile_fraction_accepts_a_parameter() {
    let db = db();
    make_quartet(&db);
    let q = parse("MATCH (n:M) RETURN percentileCont(n.v, $p)").expect("parse");
    let mut params = HashMap::new();
    params.insert("p".to_string(), Value::Float(0.5));
    let rows = execute(&q, &db, params).expect("execute").rows;
    approx(as_f64(&rows[0][0]), 2.5);
}

// ---------------------------------------------------------------------------
// Grouping, DISTINCT, and coexistence with other aggregations
// ---------------------------------------------------------------------------

#[test]
fn statistics_are_computed_per_group() {
    let db = db();
    // Group "a": [1,2,3] → stDevP variance = ((1)+(0)+(1))/3 = 2/3.
    // Group "b": [10,10] → stDevP = 0.
    for (g, v) in [("a", 1), ("a", 2), ("a", 3), ("b", 10), ("b", 10)] {
        run(&format!("CREATE (:M {{g: '{g}', v: {v}}})"), &db);
    }
    let rows = run(
        "MATCH (n:M) RETURN n.g AS g, stDevP(n.v) AS sd ORDER BY g",
        &db,
    );
    assert_eq!(rows.len(), 2);
    approx(as_f64(&rows[0][1]), (2.0f64 / 3.0).sqrt());
    approx(as_f64(&rows[1][1]), 0.0);
}

#[test]
fn distinct_deduplicates_before_the_fold() {
    let db = db();
    for v in [5, 5, 5, 5] {
        run(&format!("CREATE (:M {{v: {v}}})"), &db);
    }
    // After DISTINCT the group is a single value → stDevP 0.0, median 5.
    approx(
        as_f64(&one("MATCH (n:M) RETURN stDevP(DISTINCT n.v)", &db)),
        0.0,
    );
    approx(
        as_f64(&one(
            "MATCH (n:M) RETURN percentileCont(DISTINCT n.v, 0.5)",
            &db,
        )),
        5.0,
    );
}

#[test]
fn coexists_with_core_aggregations() {
    let db = db();
    make_textbook(&db);
    let rows = run(
        "MATCH (n:M) RETURN count(*) AS c, avg(n.v) AS a, stDevP(n.v) AS sd",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(8));
    approx(as_f64(&rows[0][1]), 5.0);
    approx(as_f64(&rows[0][2]), 2.0);
}

// ---------------------------------------------------------------------------
// Scenario domains
// ---------------------------------------------------------------------------

#[test]
fn cbt_mood_score_volatility() {
    // CBT journal: how variable is a client's daily mood rating?
    let db = db();
    for score in [3, 5, 4, 8, 2, 6] {
        run(&format!("CREATE (:MoodEntry {{score: {score}}})"), &db);
    }
    // A non-zero spread confirms the fold ran over all six entries.
    assert!(as_f64(&one("MATCH (m:MoodEntry) RETURN stDev(m.score)", &db)) > 0.0);
}

#[test]
fn story_median_chapter_length() {
    // Story editor: the median chapter word count across a book.
    let db = db();
    for words in [1200, 800, 1500, 950, 1100] {
        run(&format!("CREATE (:Chapter {{words: {words}}})"), &db);
    }
    // Sorted [800, 950, 1100, 1200, 1500], discrete median = 1100.
    assert_eq!(
        one("MATCH (c:Chapter) RETURN percentileDisc(c.words, 0.5)", &db),
        Value::Integer(1100)
    );
}

#[test]
fn task_estimate_p90() {
    // Task manager: the 90th-percentile task estimate informs a sprint buffer.
    let db = db();
    for est in [1, 2, 3, 5, 8, 13] {
        run(&format!("CREATE (:Task {{estimate: {est}}})"), &db);
    }
    // p90 continuous over [1,2,3,5,8,13]: float_idx = 0.9*5 = 4.5 →
    // 8*0.5 + 13*0.5 = 10.5.
    approx(
        as_f64(&one(
            "MATCH (t:Task) RETURN percentileCont(t.estimate, 0.9)",
            &db,
        )),
        10.5,
    );
}

#[test]
fn erp_order_value_spread_per_region() {
    // ERP: stdev of order totals per sales region.
    let db = db();
    for (region, total) in [
        ("EU", 100),
        ("EU", 300),
        ("US", 200),
        ("US", 200),
        ("US", 200),
    ] {
        run(
            &format!("CREATE (:Order {{region: '{region}', total: {total}}})"),
            &db,
        );
    }
    let rows = run(
        "MATCH (o:Order) RETURN o.region AS r, stDevP(o.total) AS sd ORDER BY r",
        &db,
    );
    // EU [100,300]: mean 200, var = (10000+10000)/2 = 10000, stdev 100.
    approx(as_f64(&rows[0][1]), 100.0);
    // US [200,200,200]: zero spread.
    approx(as_f64(&rows[1][1]), 0.0);
}

#[test]
fn bug_tracker_median_resolution_time() {
    // Bug tracker: the median time-to-resolve (in hours) across closed bugs.
    let db = db();
    for hours in [4, 24, 2, 48, 12] {
        run(&format!("CREATE (:Bug {{resolve_hours: {hours}}})"), &db);
    }
    // Sorted [2,4,12,24,48], continuous median = 12.0.
    approx(
        as_f64(&one(
            "MATCH (b:Bug) RETURN percentileCont(b.resolve_hours, 0.5)",
            &db,
        )),
        12.0,
    );
}
