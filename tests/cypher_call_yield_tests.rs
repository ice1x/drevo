//! End-to-end Cypher tests — Phase 10 follow-up task `00145`.
//!
//! `CALL proc.name(args) [YIELD col [AS alias] … [WHERE pred]]` invokes a
//! built-in procedure. drevo ships the read-only schema-introspection
//! procedures Neo4j drivers and tools expect:
//!
//! * `db.labels()` — YIELD `label`: every distinct node label (the primary
//!   kind plus any secondary `:Extra` labels), sorted.
//! * `db.relationshipTypes()` — YIELD `relationshipType`: every distinct
//!   edge kind, sorted.
//! * `db.propertyKeys()` — YIELD `propertyKey`: every distinct property key
//!   across nodes and edges (the reserved `_labels` key is never exposed),
//!   sorted.
//!
//! Before `00145` the `CALL` / `YIELD` keywords tokenised (`00061`) but
//! neither the grammar nor the executor handled them, so any `CALL` query
//! failed to parse. This task adds the `Clause::Call` AST node, the parser
//! production (dotted name + argument list + optional `YIELD … WHERE`), the
//! executor's procedure registry, and `ExecError::InvalidProcedureCall`
//! (unknown procedure / wrong arity / unknown yield column). A standalone
//! `CALL` with no `YIELD` projects every output column directly; `YIELD`
//! binds the named columns into the row stream for downstream clauses.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT
//! task manager, ERP, bug tracker) plus the cross-cutting semantics.

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

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

fn exec_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

/// Flatten a single-column string result into a `Vec<String>`.
fn strings(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect()
}

// ---- CBT journal -----------------------------------------------------------

#[test]
fn cbt_journal_discovers_its_label_vocabulary() {
    let db = db();
    exec(
        "CREATE (:Thought {body: 'I always fail'}), \
         (:Distortion {name: 'catastrophising'}), \
         (:Reframe {body: 'sometimes I succeed'})",
        &db,
    );
    let labels = strings(&run("CALL db.labels()", &db));
    assert_eq!(labels, vec!["Distortion", "Reframe", "Thought"]);
}

#[test]
fn cbt_journal_lists_relationship_vocabulary() {
    let db = db();
    exec(
        "CREATE (t:Thought)-[:HAS_DISTORTION]->(d:Distortion), \
         (t)-[:REFRAMED_AS]->(r:Reframe)",
        &db,
    );
    let kinds = strings(&run("CALL db.relationshipTypes()", &db));
    assert_eq!(kinds, vec!["HAS_DISTORTION", "REFRAMED_AS"]);
}

// ---- Story / book editor ---------------------------------------------------

#[test]
fn story_editor_counts_labels_via_yield_aggregation() {
    let db = db();
    exec(
        "CREATE (:Character {title: 'Ahab'}), (:Character {title: 'Ishmael'}), \
         (:Chapter {title: 'Loomings'}), (:Scene {title: 'The Pequod'})",
        &db,
    );
    // CALL feeding an aggregation: how many distinct labels does the
    // manuscript use?
    let rows = run(
        "CALL db.labels() YIELD label RETURN count(label) AS kinds",
        &db,
    );
    assert_eq!(rows[0][0], Value::Integer(3));
}

#[test]
fn story_editor_property_keys_union_nodes_and_edges() {
    let db = db();
    exec(
        "CREATE (a:Character {title: 'Ahab', mood: 'vengeful'}) \
         -[:APPEARS_IN {page: 12}]->(c:Chapter {title: 'The Quarter-Deck'})",
        &db,
    );
    let keys = strings(&run("CALL db.propertyKeys()", &db));
    // `title` from nodes, `mood` from a node, `page` from the edge.
    assert_eq!(keys, vec!["mood", "page", "title"]);
    assert!(!keys.contains(&"_labels".to_string()));
}

// ---- IT task manager -------------------------------------------------------

#[test]
fn task_manager_yield_where_filters_to_one_label() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'ship'}), (:Sprint {title: 'S1'}), (:User {title: 'dev'})",
        &db,
    );
    let rows = run(
        "CALL db.labels() YIELD label WHERE label = 'Task' RETURN label",
        &db,
    );
    assert_eq!(strings(&rows), vec!["Task"]);
}

#[test]
fn task_manager_yield_alias_renames_column() {
    let db = db();
    exec("CREATE (:Task {title: 'ship'})", &db);
    let rows = run("CALL db.labels() YIELD label AS kind RETURN kind", &db);
    assert_eq!(strings(&rows), vec!["Task"]);
}

// ---- ERP -------------------------------------------------------------------

#[test]
fn erp_secondary_labels_appear_in_label_listing() {
    let db = db();
    // Multi-label node: an Employee that is also a Manager.
    exec("CREATE (:Person:Employee:Manager {title: 'Dana'})", &db);
    let labels = strings(&run("CALL db.labels()", &db));
    assert_eq!(labels, vec!["Employee", "Manager", "Person"]);
}

#[test]
fn erp_relationship_types_are_distinct_and_sorted() {
    let db = db();
    // Avoid `CONTAINS` here — it is a reserved Cypher keyword, and using a
    // keyword as a relationship type lowercases it (a pre-existing parser
    // quirk unrelated to CALL/YIELD).
    exec(
        "CREATE (o:Order)-[:HAS_ITEM]->(:LineItem), \
         (o)-[:PLACED_BY]->(:Customer), \
         (o)-[:HAS_ITEM]->(:LineItem)",
        &db,
    );
    let kinds = strings(&run("CALL db.relationshipTypes()", &db));
    assert_eq!(kinds, vec!["HAS_ITEM", "PLACED_BY"]);
}

// ---- Bug tracker -----------------------------------------------------------

#[test]
fn bug_tracker_empty_graph_yields_no_labels() {
    let db = db();
    let rows = run("CALL db.labels()", &db);
    assert!(rows.is_empty());
}

#[test]
fn bug_tracker_standalone_call_projects_output_column() {
    let db = db();
    exec(
        "CREATE (:Bug {title: 'crash'}), (:Component {title: 'parser'})",
        &db,
    );
    let q = parse("CALL db.labels()").expect("parse");
    let res = execute(&q, &db, HashMap::new()).expect("execute");
    assert_eq!(res.columns, vec!["label"]);
    assert_eq!(strings(&res.rows), vec!["Bug", "Component"]);
}

// ---- Cross-cutting error semantics -----------------------------------------

#[test]
fn unknown_procedure_is_rejected() {
    let db = db();
    let e = exec_err("CALL db.nope()", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref name, .. } if name == "db.nope"),
        "got {e:?}"
    );
}

#[test]
fn arguments_to_zero_arity_procedure_are_rejected() {
    let db = db();
    let e = exec_err("CALL db.labels(42)", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("0 arguments")),
        "got {e:?}"
    );
}

#[test]
fn yield_of_unknown_column_is_rejected() {
    let db = db();
    let e = exec_err("CALL db.labels() YIELD bogus RETURN bogus", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { ref message, .. } if message.contains("does not yield")),
        "got {e:?}"
    );
}

#[test]
fn error_is_deterministic_on_empty_graph() {
    // The upfront validation sweep surfaces the error before any rows are
    // produced, so an empty graph still rejects a bad CALL.
    let db = db();
    let e = exec_err("CALL db.nope() YIELD x RETURN x", &db);
    assert!(
        matches!(e, ExecError::InvalidProcedureCall { .. }),
        "got {e:?}"
    );
}
