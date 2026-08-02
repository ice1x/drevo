//! End-to-end Cypher tests — `startNode(rel)` / `endNode(rel)` (issue #232).
//!
//! The two standard Neo4j relationship-endpoint functions: `startNode(r)`
//! returns a relationship's source node, `endNode(r)` its target. They are
//! the natural companion to `fts.searchRelationships` (#229) — a `YIELD rel`
//! result is far more useful when its endpoints are directly reachable —
//! and are what Neo4j-compatible tooling (the graphiti connector) emits for
//! edge endpoint projection instead of a slower re-`MATCH`.
//!
//! Because a `RelationshipValue` carries only the endpoint *ids*, these two
//! functions need the graph to resolve the node, so they are evaluated in
//! the executor (with DB access) rather than in the pure `call_scalar`
//! library — alongside `similar()` / `keywords()`.
//!
//! Cases drive the real parser → executor pipeline across the drevo target
//! scenario domains (IT task manager, ERP, bug tracker, story editor) plus
//! the cross-cutting semantics (NULL propagation, type / arity errors, and
//! composition with `fts.searchRelationships`).

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

#[test]
fn start_and_end_node_return_the_relationship_endpoints() {
    let db = db();
    exec(
        "CREATE (a:Person {name:'Ada'})-[:KNOWS]->(b:Person {name:'Bob'})",
        &db,
    );
    let rows = run(
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) \
         RETURN id(startNode(r)) = id(a) AS start_ok, id(endNode(r)) = id(b) AS end_ok",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Bool(true), "startNode(r) must be a");
    assert_eq!(rows[0][1], Value::Bool(true), "endNode(r) must be b");
}

#[test]
fn start_end_node_property_projection() {
    // The endpoint is a full node: property access works like any node.
    let db = db();
    exec(
        "CREATE (a:Task {name:'design'})-[:BLOCKS]->(b:Task {name:'build'})",
        &db,
    );
    let rows = run(
        "MATCH (:Task)-[r:BLOCKS]->(:Task) \
         RETURN startNode(r).name AS blocker, endNode(r).name AS blocked",
        &db,
    );
    assert_eq!(rows[0][0], Value::String("design".into()));
    assert_eq!(rows[0][1], Value::String("build".into()));
}

#[test]
fn start_end_node_compose_with_fts_search_relationships() {
    // The #232 motivating case: project a searched relationship's endpoints
    // directly, no re-MATCH (the graphiti connector's pattern).
    let db = db();
    exec(
        "CREATE (a:Entity {name:'Acme'})-[:RELATES_TO {fact:'zebra merger deal'}]->(b:Entity {name:'Beta'})",
        &db,
    );
    let rows = run(
        "CALL fts.searchRelationships('zebra', 10) YIELD rel, score \
         RETURN startNode(rel).name AS source, endNode(rel).name AS target",
        &db,
    );
    assert_eq!(rows.len(), 1, "the indexed edge must be found");
    assert_eq!(rows[0][0], Value::String("Acme".into()));
    assert_eq!(rows[0][1], Value::String("Beta".into()));
}

#[test]
fn start_end_node_direction_is_respected() {
    // endNode is the arrow's head even when the MATCH is written in reverse.
    let db = db();
    exec(
        "CREATE (o:Order {name:'SO-1'})-[:CONTAINS]->(l:LineItem {name:'widget'})",
        &db,
    );
    let rows = run(
        "MATCH (l:LineItem)<-[r:CONTAINS]-(o:Order) \
         RETURN startNode(r).name AS s, endNode(r).name AS e",
        &db,
    );
    assert_eq!(rows[0][0], Value::String("SO-1".into()), "start = tail");
    assert_eq!(rows[0][1], Value::String("widget".into()), "end = head");
}

#[test]
fn start_end_node_null_propagates() {
    let db = db();
    let rows = run("RETURN startNode(null) AS s, endNode(null) AS e", &db);
    assert_eq!(rows[0][0], Value::Null);
    assert_eq!(rows[0][1], Value::Null);
}

#[test]
fn start_node_rejects_a_non_relationship_argument() {
    let db = db();
    let err = exec_err("RETURN startNode(42)", &db);
    assert!(
        matches!(err, ExecError::InvalidFunctionCall { .. }),
        "expected InvalidFunctionCall, got {err:?}"
    );
}

#[test]
fn end_node_rejects_a_non_relationship_argument() {
    let db = db();
    let err = exec_err("RETURN endNode('bug-123')", &db);
    assert!(
        matches!(err, ExecError::InvalidFunctionCall { .. }),
        "expected InvalidFunctionCall, got {err:?}"
    );
}

#[test]
fn start_end_node_reject_wrong_arity() {
    let db = db();
    assert!(matches!(
        exec_err("RETURN startNode()", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
    assert!(matches!(
        exec_err("RETURN endNode(1, 2)", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}
