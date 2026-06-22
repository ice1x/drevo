//! End-to-end Cypher map-projection tests — Phase 10 follow-up task `00149`.
//!
//! A map projection `base { selector, … }` builds a new map by projecting
//! selected entries off `base` (a node, relationship, or map value). It is the
//! shaping idiom that lets a query return a tailored record per row without
//! enumerating every property by hand — `RETURN p {.title, .priority, tag:
//! 'urgent'}`. Four selector forms compose in any mix:
//!
//! * `.key`     — copy property `key` off the base (absent → `null`),
//! * `.*`       — copy every property of the base,
//! * `key: expr`— a computed entry (`expr` evaluated in the current row),
//! * `var`      — shorthand for `var: var`, an in-scope variable.
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * selectors apply in source order, a later key overwriting an earlier one,
//! * a `null` base propagates to `null` (so projecting an unmatched
//!   `OPTIONAL MATCH` variable is `null`, not an error),
//! * a scalar (non-map) base is a recoverable `TypeMismatch`,
//! * a map projection is a group key alongside an aggregation,
//!
//! plus the four scenario domains (CBT journal, story editor, task manager,
//! ERP, bug tracker) the drevo Cypher suite standardises on.

use std::collections::{BTreeMap, HashMap};

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

/// The single `Map` value of a one-row, one-column result.
fn one_map(rows: &[Vec<Value>]) -> BTreeMap<String, Value> {
    assert_eq!(rows.len(), 1, "expected exactly one row");
    assert_eq!(rows[0].len(), 1, "expected exactly one column");
    match &rows[0][0] {
        Value::Map(m) => m.clone(),
        other => panic!("expected a Map value, got {other:?}"),
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

// ===== Core semantics =======================================================

#[test]
fn property_selectors_copy_named_properties() {
    let d = db();
    run("CREATE (:Person {name: 'Ann', age: 30, city: 'NYC'})", &d);
    let m = one_map(&run("MATCH (p:Person) RETURN p {.name, .age} AS m", &d));
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("age"), Some(&Value::Integer(30)));
    assert!(!m.contains_key("city"), "city was not selected");
}

#[test]
fn absent_property_selector_is_null() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    let m = one_map(&run(
        "MATCH (p:Person) RETURN p {.name, .nickname} AS m",
        &d,
    ));
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("nickname"), Some(&Value::Null));
}

#[test]
fn all_properties_selector_copies_every_property() {
    let d = db();
    run("CREATE (:Person {name: 'Ann', age: 30})", &d);
    let m = one_map(&run("MATCH (p:Person) RETURN p {.*} AS m", &d));
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("age"), Some(&Value::Integer(30)));
}

#[test]
fn literal_entry_is_evaluated_in_the_row_scope() {
    let d = db();
    run("CREATE (:Person {name: 'Ann', age: 30})", &d);
    let m = one_map(&run(
        "MATCH (p:Person) RETURN p {.name, doubled: p.age * 2, role: 'admin'} AS m",
        &d,
    ));
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("doubled"), Some(&Value::Integer(60)));
    assert_eq!(m.get("role"), Some(&s("admin")));
}

#[test]
fn variable_selector_is_var_colon_var_shorthand() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    let m = one_map(&run(
        "MATCH (p:Person) WITH p, 99 AS extra RETURN p {.name, extra} AS m",
        &d,
    ));
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("extra"), Some(&Value::Integer(99)));
}

#[test]
fn selectors_apply_in_order_later_overwrites_earlier() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    let m = one_map(&run(
        "MATCH (p:Person) RETURN p {.name, name: 'override'} AS m",
        &d,
    ));
    assert_eq!(m.get("name"), Some(&s("override")));
}

#[test]
fn empty_projection_yields_empty_map() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    let m = one_map(&run("MATCH (p:Person) RETURN p {} AS m", &d));
    assert!(m.is_empty());
}

#[test]
fn projection_over_a_map_literal_base() {
    let d = db();
    let m = one_map(&run("RETURN {a: 1, b: 2, c: 3} {.a, .c} AS m", &d));
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("a"), Some(&Value::Integer(1)));
    assert_eq!(m.get("c"), Some(&Value::Integer(3)));
}

#[test]
fn projection_over_a_relationship_base() {
    let d = db();
    run(
        "CREATE (:Person {name: 'A'})-[:KNOWS {since: 2020, weight: 5}]->(:Person {name: 'B'})",
        &d,
    );
    let m = one_map(&run(
        "MATCH (:Person)-[r:KNOWS]->(:Person) RETURN r {.since, .weight} AS m",
        &d,
    ));
    assert_eq!(m.get("since"), Some(&Value::Integer(2020)));
    assert_eq!(m.get("weight"), Some(&Value::Integer(5)));
}

#[test]
fn parameter_drives_a_literal_entry() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    let mut params = HashMap::new();
    params.insert("tag".to_string(), s("vip"));
    let rows = run_with_params(
        "MATCH (p:Person) RETURN p {.name, tag: $tag} AS m",
        &d,
        params,
    );
    let m = one_map(&rows);
    assert_eq!(m.get("name"), Some(&s("Ann")));
    assert_eq!(m.get("tag"), Some(&s("vip")));
}

// ===== Null & error handling ================================================

#[test]
fn null_base_propagates_to_null() {
    let d = db();
    // An unmatched OPTIONAL MATCH binds `z` to null.
    let rows = run("OPTIONAL MATCH (z:Nope) RETURN z {.name} AS m", &d);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn scalar_base_is_type_mismatch() {
    let d = db();
    assert!(matches!(
        run_err("RETURN 7 {.name} AS m", &d),
        ExecError::TypeMismatch { .. }
    ));
}

#[test]
fn unbound_variable_selector_errors() {
    let d = db();
    run("CREATE (:Person {name: 'Ann'})", &d);
    assert!(matches!(
        run_err("MATCH (p:Person) RETURN p {.name, missing} AS m", &d),
        ExecError::UnboundVariable { .. }
    ));
}

// ===== Composition ==========================================================

#[test]
fn projection_is_a_group_key_alongside_aggregation() {
    let d = db();
    run("CREATE (:Item {cat: 'a', n: 1})", &d);
    run("CREATE (:Item {cat: 'a', n: 2})", &d);
    run("CREATE (:Item {cat: 'b', n: 5})", &d);
    let rows = run(
        "MATCH (i:Item) RETURN i {.cat} AS key, sum(i.n) AS total",
        &d,
    );
    assert_eq!(rows.len(), 2);
    let mut totals: Vec<(String, i64)> = rows
        .iter()
        .map(|row| {
            let cat = match &row[0] {
                Value::Map(m) => match m.get("cat") {
                    Some(Value::String(c)) => c.clone(),
                    other => panic!("expected cat string, got {other:?}"),
                },
                other => panic!("expected map key, got {other:?}"),
            };
            let total = match &row[1] {
                Value::Integer(i) => *i,
                other => panic!("expected integer total, got {other:?}"),
            };
            (cat, total)
        })
        .collect();
    totals.sort();
    assert_eq!(totals, vec![("a".to_string(), 3), ("b".to_string(), 5)]);
}

#[test]
fn projection_inside_a_collected_list() {
    let d = db();
    run("CREATE (:Tag {label: 'x', weight: 1})", &d);
    run("CREATE (:Tag {label: 'y', weight: 2})", &d);
    // `collect(t {.label, .weight})` gathers one projected map per tag.
    let rows = run(
        "MATCH (t:Tag) WITH collect(t {.label, .weight}) AS tags RETURN tags AS m",
        &d,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::List(items) => {
            assert_eq!(items.len(), 2);
            assert!(items.iter().all(|v| matches!(v, Value::Map(_))));
        }
        other => panic!("expected a list of maps, got {other:?}"),
    }
}

// ===== Scenario domains =====================================================

#[test]
fn cbt_journal_thought_record_shape() {
    let d = db();
    run(
        "CREATE (:Thought {situation: 'missed deadline', emotion: 'anxious', intensity: 80})",
        &d,
    );
    let m = one_map(&run(
        "MATCH (t:Thought) RETURN t {.situation, .emotion, severe: t.intensity >= 70} AS record",
        &d,
    ));
    assert_eq!(m.get("situation"), Some(&s("missed deadline")));
    assert_eq!(m.get("emotion"), Some(&s("anxious")));
    assert_eq!(m.get("severe"), Some(&Value::Bool(true)));
}

#[test]
fn story_editor_chapter_card() {
    let d = db();
    run(
        "CREATE (:Chapter {title: 'The Door', words: 3200, status: 'draft'})",
        &d,
    );
    let m = one_map(&run("MATCH (c:Chapter) RETURN c {.*} AS card", &d));
    assert_eq!(m.get("words"), Some(&Value::Integer(3200)));
    assert_eq!(m.get("status"), Some(&s("draft")));
}

#[test]
fn task_manager_assignment_summary() {
    let d = db();
    run(
        "CREATE (:Task {summary: 'Ship v2', priority: 'high', estimate: 5})",
        &d,
    );
    let m = one_map(&run(
        "MATCH (t:Task) RETURN t {.summary, .priority, kind: 'work-item'} AS m",
        &d,
    ));
    assert_eq!(m.get("summary"), Some(&s("Ship v2")));
    assert_eq!(m.get("priority"), Some(&s("high")));
    assert_eq!(m.get("kind"), Some(&s("work-item")));
}

#[test]
fn erp_order_line_projection() {
    let d = db();
    run("CREATE (:Line {sku: 'A-1', qty: 4, unit_price: 25})", &d);
    let m = one_map(&run(
        "MATCH (l:Line) RETURN l {.sku, .qty, subtotal: l.qty * l.unit_price} AS m",
        &d,
    ));
    assert_eq!(m.get("sku"), Some(&s("A-1")));
    assert_eq!(m.get("qty"), Some(&Value::Integer(4)));
    assert_eq!(m.get("subtotal"), Some(&Value::Integer(100)));
}

#[test]
fn bug_tracker_report_card_with_optional_assignee() {
    let d = db();
    run(
        "CREATE (:Bug {id: 'BUG-7', severity: 'critical', open: true})",
        &d,
    );
    // No assignee relationship exists, so the OPTIONAL MATCH leaves `u` null
    // and the variable selector carries that null through faithfully.
    let m = one_map(&run(
        "MATCH (b:Bug {id: 'BUG-7'})
         OPTIONAL MATCH (b)-[:ASSIGNED_TO]->(u:User)
         RETURN b {.id, .severity, assignee: u} AS m",
        &d,
    ));
    assert_eq!(m.get("id"), Some(&s("BUG-7")));
    assert_eq!(m.get("severity"), Some(&s("critical")));
    assert_eq!(m.get("assignee"), Some(&Value::Null));
}
