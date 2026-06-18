//! End-to-end Cypher tests — Phase 10 follow-up task `00143`.
//!
//! A `MATCH` pattern whose **head** (or any non-final intermediate) node is
//! *anonymous* — `MATCH (:Person)-[:KNOWS]->(b)` or `MATCH ()-->(b)` or
//! `MATCH (a)-->()-->(c)` — is ordinary, extremely common Cypher. The parser
//! has always produced these (`00062`) but the executor used to re-derive the
//! predecessor node of each segment by looking it up in the bindings; for an
//! anonymous node there is no variable to look up and (in an unnamed path) no
//! path accumulator either, so the executor wrongly returned an internal
//! `InvalidCreate("anonymous intermediate node in multi-hop path")` error.
//!
//! This task threads the actual endpoint node *forward* through the matcher
//! (`match_head` → `match_segment` → next segment), so anonymous predecessors
//! chain correctly. Named paths (`00141`) and variable-length segments
//! (`00069`) keep working unchanged.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_params(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
}

/// Collect a one-column projection into a sorted `Vec<String>` for
/// order-independent assertions.
fn sorted_strings(rows: &[Vec<Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

// ---- CBT journal --------------------------------------------------------

#[test]
fn cbt_anonymous_thought_head_finds_distortions() {
    let d = db();
    exec(
        "CREATE (:Thought {title: 'I will fail'})-[:HAS_DISTORTION]->(:Distortion {kind: 'catastrophizing'})",
        &d,
    );
    exec(
        "CREATE (:Thought {title: 'They hate me'})-[:HAS_DISTORTION]->(:Distortion {kind: 'mind_reading'})",
        &d,
    );
    // We do not care which thought — just enumerate the distortions reached
    // from any thought via an anonymous head.
    let rows = run(
        "MATCH (:Thought)-[:HAS_DISTORTION]->(distortion) RETURN distortion.kind AS kind",
        &d,
    );
    assert_eq!(
        sorted_strings(&rows),
        vec!["catastrophizing".to_string(), "mind_reading".to_string()]
    );
}

// ---- story / book editor ------------------------------------------------

#[test]
fn story_anonymous_intermediate_chapter_chain() {
    let d = db();
    // Book -> Chapter1 -> Chapter2 (NEXT chain). We want the chapter two hops
    // out from the book, threading through an anonymous middle chapter.
    exec(
        "CREATE (b:Book {title: 'Dune'})-[:HAS_CHAPTER]->(c1:Chapter {title: 'Chapter 1'})",
        &d,
    );
    exec(
        "MATCH (b:Book {title: 'Dune'}), (c1:Chapter {title: 'Chapter 1'}) \
         CREATE (c1)-[:NEXT]->(c2:Chapter {title: 'Chapter 2'})",
        &d,
    );
    let rows = run(
        "MATCH (:Book {title: 'Dune'})-[:HAS_CHAPTER]->()-[:NEXT]->(c) RETURN c.title AS title",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Chapter 2".into()));
}

// ---- IT task manager ----------------------------------------------------

#[test]
fn task_bare_anonymous_head_lists_assignees() {
    let d = db();
    exec(
        "CREATE (:Task {key: 'PROJ-1'})-[:ASSIGNED_TO]->(:Person {name: 'Ada'})",
        &d,
    );
    exec(
        "CREATE (:Task {key: 'PROJ-2'})-[:ASSIGNED_TO]->(:Person {name: 'Linus'})",
        &d,
    );
    // Bare anonymous head with no label at all.
    let rows = run(
        "MATCH ()-[:ASSIGNED_TO]->(p:Person) RETURN p.name AS name",
        &d,
    );
    assert_eq!(
        sorted_strings(&rows),
        vec!["Ada".to_string(), "Linus".to_string()]
    );
}

// ---- ERP ----------------------------------------------------------------

#[test]
fn erp_anonymous_head_varlen_reaches_components() {
    let d = db();
    // Order -> Product -> Component bill-of-materials. Variable-length from an
    // anonymous order head.
    exec(
        "CREATE (o:Order {ref: 'SO-100'})-[:CONTAINS]->(p:Product {sku: 'WIDGET'})",
        &d,
    );
    exec(
        "MATCH (p:Product {sku: 'WIDGET'}) \
         CREATE (p)-[:MADE_OF]->(:Component {sku: 'BOLT'})",
        &d,
    );
    // From any order, walk 1..2 hops and collect everything reachable.
    let rows = run(
        "MATCH (:Order)-[*1..2]->(reached) RETURN reached.sku AS sku",
        &d,
    );
    assert_eq!(
        sorted_strings(&rows),
        vec!["BOLT".to_string(), "WIDGET".to_string()]
    );
}

// ---- bug tracker --------------------------------------------------------

#[test]
fn bug_anonymous_head_with_parameter_target_filter() {
    let d = db();
    exec(
        "CREATE (:Bug {id: 'B-1'})-[:HAS_SEVERITY]->(:Severity {level: 'critical'})",
        &d,
    );
    exec(
        "CREATE (:Bug {id: 'B-2'})-[:HAS_SEVERITY]->(:Severity {level: 'minor'})",
        &d,
    );
    let mut params = HashMap::new();
    params.insert("level".to_string(), Value::String("critical".into()));
    // Anonymous bug head, parameterised severity filter on the target.
    let rows = run_params(
        "MATCH (:Bug)-[:HAS_SEVERITY]->(s:Severity) WHERE s.level = $level RETURN s.level AS level",
        &d,
        params,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("critical".into()));
}

// ---- cross-cutting semantics -------------------------------------------

#[test]
fn anonymous_head_no_match_is_empty_not_error() {
    let d = db();
    exec("CREATE (:Person {name: 'Solo'})", &d);
    // No outgoing :KNOWS — must yield zero rows, never the old InvalidCreate.
    let rows = run("MATCH (:Person)-[:KNOWS]->(b) RETURN b.name AS name", &d);
    assert!(rows.is_empty());
}

#[test]
fn anonymous_head_aggregation_counts_targets() {
    let d = db();
    exec(
        "CREATE (h:Hub {name: 'h'})-[:LINK]->(:Leaf {name: 'a'})",
        &d,
    );
    exec("MATCH (h:Hub) CREATE (h)-[:LINK]->(:Leaf {name: 'b'})", &d);
    exec("MATCH (h:Hub) CREATE (h)-[:LINK]->(:Leaf {name: 'c'})", &d);
    let rows = run("MATCH (:Hub)-[:LINK]->(leaf) RETURN count(leaf) AS n", &d);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(3));
}

#[test]
fn optional_match_anonymous_head_synthesises_null_row() {
    let d = db();
    exec("CREATE (:Person {name: 'Alone'})", &d);
    // OPTIONAL MATCH with an anonymous head that matches nothing keeps the
    // driving row and binds the target to NULL (left-join semantics).
    let rows = run(
        "MATCH (p:Person) OPTIONAL MATCH (:Person)-[:KNOWS]->(friend) \
         RETURN p.name AS name, friend AS friend",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Alone".into()));
    assert_eq!(rows[0][1], Value::Null);
}

#[test]
fn named_path_over_anonymous_head_binds_full_path() {
    let d = db();
    exec(
        "CREATE (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})-[:KNOWS]->(:Person {name: 'Carol'})",
        &d,
    );
    // Named path whose head AND intermediate are anonymous still records every
    // hop — `nodes(p)` returns all three endpoints.
    let rows = run(
        "MATCH p = (:Person {name: 'Alice'})-[:KNOWS]->()-[:KNOWS]->(c) \
         RETURN length(p) AS hops, c.name AS last",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(2));
    assert_eq!(rows[0][1], Value::String("Carol".into()));
}

#[test]
fn anonymous_head_with_property_filter_on_head() {
    let d = db();
    exec(
        "CREATE (:Person {name: 'Alice', team: 'red'})-[:KNOWS]->(:Person {name: 'Bob'})",
        &d,
    );
    exec(
        "CREATE (:Person {name: 'Eve', team: 'blue'})-[:KNOWS]->(:Person {name: 'Mallory'})",
        &d,
    );
    // The anonymous head still carries an inline property predicate that
    // filters which relationships are walked.
    let rows = run(
        "MATCH (:Person {team: 'red'})-[:KNOWS]->(b) RETURN b.name AS name",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Bob".into()));
}

#[test]
fn anonymous_head_undirected_finds_both_directions() {
    let d = db();
    exec(
        "CREATE (:City {name: 'A'})-[:ROAD]->(:City {name: 'B'})",
        &d,
    );
    // Undirected traversal from an anonymous head reaches both endpoints.
    let rows = run(
        "MATCH (:City)-[:ROAD]-(other) RETURN other.name AS name",
        &d,
    );
    assert_eq!(
        sorted_strings(&rows),
        vec!["A".to_string(), "B".to_string()]
    );
}

#[test]
fn type_mismatch_when_bound_head_variable_is_not_a_node() {
    // A *named* head bound to a non-node value is still a clean TypeMismatch,
    // proving the threading change did not weaken existing validation.
    let d = db();
    exec(
        "CREATE (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})",
        &d,
    );
    let q = parse("WITH 1 AS a MATCH (a)-[:KNOWS]->(b) RETURN b.name").expect("parse");
    let err = execute(&q, &d, HashMap::new()).expect_err("expected type error");
    assert!(matches!(err, ExecError::TypeMismatch { .. }), "got {err:?}");
}
