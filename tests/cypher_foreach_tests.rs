//! End-to-end Cypher tests — Phase 10 follow-up task `00144`.
//!
//! `FOREACH (var IN list | update_clause …)` is Cypher's bulk-update
//! clause: for every current binding row it evaluates `list` and runs the
//! nested update clauses once per element with `var` bound to it. It is a
//! pure side-effecting clause — it never changes the outer cardinality,
//! and the loop variable is scoped to the body. The body is restricted to
//! update clauses (`CREATE` / `MERGE` / `SET` / `REMOVE` / `DELETE` and
//! nested `FOREACH`); read clauses are rejected at parse time.
//!
//! Before `00144` the `FOREACH` keyword tokenised (`00061`) but neither the
//! grammar nor the executor handled it, so any `FOREACH` query failed to
//! parse. This task adds the `Clause::Foreach` AST node, the parser
//! production, and the executor's per-row iteration. A `null` list is a
//! no-op (zero iterations), mirroring `UNWIND null`.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT
//! task manager, ERP, bug tracker) plus the cross-cutting semantics.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, ExecStats, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn exec(source: &str, drevo: &Drevo) -> ExecStats {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").stats
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_params(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

fn exec_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

/// The single scalar of a one-row, one-column result.
fn scalar(rows: &[Vec<Value>]) -> &Value {
    assert_eq!(rows.len(), 1, "expected exactly one row: {rows:?}");
    assert_eq!(rows[0].len(), 1, "expected exactly one column: {rows:?}");
    &rows[0][0]
}

// ---- CBT journal --------------------------------------------------------

#[test]
fn cbt_foreach_tags_a_thought_with_each_cognitive_distortion() {
    let db = db();
    exec(
        "CREATE (:Thought {title: 'I always fail', body: 'automatic negative thought'})",
        &db,
    );
    // Attach a distortion node per name and link it to the thought.
    exec(
        "MATCH (t:Thought {title: 'I always fail'}) \
         FOREACH (name IN ['catastrophising', 'overgeneralisation', 'mind-reading'] | \
           CREATE (t)-[:EXHIBITS]->(:Distortion {title: name}))",
        &db,
    );
    let count = run(
        "MATCH (:Thought {title: 'I always fail'})-[:EXHIBITS]->(d:Distortion) RETURN count(d)",
        &db,
    );
    assert_eq!(*scalar(&count), Value::Integer(3));
}

// ---- Story / book editor ------------------------------------------------

#[test]
fn story_foreach_creates_a_chapter_per_title_linked_to_the_book() {
    let db = db();
    exec("CREATE (:Book {title: 'The Long Road'})", &db);
    let stats = exec(
        "MATCH (b:Book {title: 'The Long Road'}) \
         FOREACH (ch IN ['Departure', 'The Crossing', 'Homecoming'] | \
           CREATE (b)-[:HAS_CHAPTER]->(:Chapter {title: ch}))",
        &db,
    );
    assert_eq!(stats.nodes_created, 3);
    assert_eq!(stats.relationships_created, 3);
    let chapters = run(
        "MATCH (:Book {title: 'The Long Road'})-[:HAS_CHAPTER]->(c:Chapter) \
         RETURN c.title ORDER BY c.title",
        &db,
    );
    assert_eq!(
        chapters,
        vec![
            vec![Value::String("Departure".into())],
            vec![Value::String("Homecoming".into())],
            vec![Value::String("The Crossing".into())],
        ]
    );
}

// ---- IT task manager ----------------------------------------------------

#[test]
fn task_foreach_creates_subtasks_from_a_parameter_list() {
    let db = db();
    exec("CREATE (:Project {title: 'Migrate DB'})", &db);
    let mut params = HashMap::new();
    params.insert(
        "subtasks".to_string(),
        Value::List(vec![
            Value::String("schema".into()),
            Value::String("backfill".into()),
            Value::String("cutover".into()),
        ]),
    );
    run_params(
        "MATCH (p:Project {title: 'Migrate DB'}) \
         FOREACH (s IN $subtasks | CREATE (p)-[:HAS_SUBTASK]->(:Task {title: s}))",
        &db,
        params,
    );
    let count = run(
        "MATCH (:Project {title: 'Migrate DB'})-[:HAS_SUBTASK]->(t:Task) RETURN count(t)",
        &db,
    );
    assert_eq!(*scalar(&count), Value::Integer(3));
}

#[test]
fn task_foreach_marks_every_collected_task_done() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'A'}) CREATE (:Task {title: 'B'}) CREATE (:Task {title: 'C'})",
        &db,
    );
    let stats = exec(
        "MATCH (t:Task) WITH collect(t) AS ts \
         FOREACH (n IN ts | SET n.status = 'done')",
        &db,
    );
    assert_eq!(stats.properties_set, 3);
    let done = run(
        "MATCH (t:Task) WHERE t.status = 'done' RETURN count(t)",
        &db,
    );
    assert_eq!(*scalar(&done), Value::Integer(3));
}

// ---- ERP ----------------------------------------------------------------

#[test]
fn erp_foreach_adds_line_items_to_an_order() {
    let db = db();
    exec("CREATE (:Order {title: 'PO-1001'})", &db);
    // Each line item carries a quantity computed from the loop element.
    exec(
        "MATCH (o:Order {title: 'PO-1001'}) \
         FOREACH (sku IN ['SKU-1', 'SKU-2'] | \
           CREATE (o)-[:CONTAINS]->(:LineItem {title: sku, qty: 5}))",
        &db,
    );
    let total = run(
        "MATCH (:Order {title: 'PO-1001'})-[:CONTAINS]->(li:LineItem) RETURN sum(li.qty)",
        &db,
    );
    assert_eq!(*scalar(&total), Value::Integer(10));
}

// ---- Bug tracker --------------------------------------------------------

#[test]
fn bug_foreach_merges_labels_idempotently() {
    let db = db();
    exec("CREATE (:Bug {title: 'NPE on save'})", &db);
    // MERGE inside FOREACH: running it twice must not duplicate labels.
    let q = "MATCH (b:Bug {title: 'NPE on save'}) \
             FOREACH (lbl IN ['regression', 'p1'] | \
               MERGE (l:Label {title: lbl}) \
               MERGE (b)-[:TAGGED]->(l))";
    exec(q, &db);
    exec(q, &db);
    let labels = run(
        "MATCH (:Bug {title: 'NPE on save'})-[:TAGGED]->(l:Label) RETURN count(l)",
        &db,
    );
    assert_eq!(*scalar(&labels), Value::Integer(2));
}

// ---- Cross-cutting semantics --------------------------------------------

#[test]
fn foreach_over_empty_list_makes_no_changes() {
    let db = db();
    let stats = exec(
        "FOREACH (x IN [] | CREATE (:Task {title: toString(x)}))",
        &db,
    );
    assert_eq!(stats.nodes_created, 0);
}

#[test]
fn foreach_over_null_list_makes_no_changes() {
    let db = db();
    let stats = exec(
        "FOREACH (x IN null | CREATE (:Task {title: toString(x)}))",
        &db,
    );
    assert_eq!(stats.nodes_created, 0);
}

#[test]
fn foreach_over_non_list_value_is_a_type_error() {
    let db = db();
    let e = exec_err("FOREACH (x IN 7 | CREATE (:Task {title: 'x'}))", &db);
    assert!(
        matches!(e, ExecError::TypeMismatch { ref expected, .. } if expected == "List"),
        "got {e:?}",
    );
}

#[test]
fn foreach_does_not_alter_outer_cardinality() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'A'}) CREATE (:Task {title: 'B'})",
        &db,
    );
    // FOREACH touches each matched row but passes them through unchanged.
    let rows = run(
        "MATCH (t:Task) FOREACH (n IN [1, 2, 3] | SET t.touched = true) \
         RETURN t.title ORDER BY t.title",
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::String("A".into())],
            vec![Value::String("B".into())],
        ]
    );
}

#[test]
fn foreach_loop_variable_does_not_leak_to_later_clauses() {
    let db = db();
    // After the FOREACH, `x` is out of scope; referencing it errors.
    let e = exec_err(
        "FOREACH (x IN [1, 2] | CREATE (:Task {title: toString(x)})) RETURN x",
        &db,
    );
    assert!(
        matches!(e, ExecError::UnboundVariable { ref name, .. } if name == "x"),
        "got {e:?}",
    );
}

#[test]
fn foreach_nested_creates_the_full_cross_product() {
    let db = db();
    exec(
        "FOREACH (row IN [['a', 'b'], ['c']] | \
           FOREACH (cell IN row | CREATE (:Cell {title: cell})))",
        &db,
    );
    let count = run("MATCH (c:Cell) RETURN count(c)", &db);
    assert_eq!(*scalar(&count), Value::Integer(3));
}

#[test]
fn foreach_deletes_every_collected_node() {
    let db = db();
    exec(
        "CREATE (:Temp {title: 'A'}) CREATE (:Temp {title: 'B'})",
        &db,
    );
    let stats = exec(
        "MATCH (t:Temp) WITH collect(t) AS ts FOREACH (n IN ts | DELETE n)",
        &db,
    );
    assert_eq!(stats.nodes_deleted, 2);
    let remaining = run("MATCH (t:Temp) RETURN count(t)", &db);
    assert_eq!(*scalar(&remaining), Value::Integer(0));
}

#[test]
fn foreach_over_empty_match_runs_zero_iterations() {
    let db = db();
    // No Project rows → the body never runs even though its list is full.
    exec(
        "MATCH (p:Project) \
         FOREACH (name IN ['a', 'b'] | CREATE (p)-[:HAS_SUBTASK]->(:Task {title: name}))",
        &db,
    );
    let count = run("MATCH (t:Task) RETURN count(t)", &db);
    assert_eq!(*scalar(&count), Value::Integer(0));
}
