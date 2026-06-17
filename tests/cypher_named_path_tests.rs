//! End-to-end Cypher named-path tests — Phase 10 follow-up task `00141`.
//!
//! The parser already produces a `NamedPattern` with an optional `variable`
//! for `MATCH p = (a)-->(b)` (`00062`); this task makes the executor *bind*
//! that variable to a first-class **path** value with Neo4j-compatible
//! semantics:
//!
//! * A path is an alternating sequence of nodes and relationships
//!   `n0, r1, n1, …, rk, nk`, captured in traversal order — including
//!   **anonymous** endpoints that carry no variable of their own.
//! * `length(p)` is the number of relationships (`k`); `nodes(p)` and
//!   `relationships(p)` return the node / relationship lists in path order.
//! * Variable-length segments (`-[:R*1..3]->`) bind a path spanning every
//!   traversed hop, honouring trail uniqueness.
//! * `CREATE p = (a)-[:R]->(b) RETURN p` binds the freshly created path.
//! * `nodes(NULL)`, `relationships(NULL)`, and `length(NULL)` are `NULL`.
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

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

/// One-row, one-column projection helper.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from {source:?}");
    rows[0][0].clone()
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected Integer, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Core path shape
// ---------------------------------------------------------------------------

#[test]
fn single_hop_named_path_binds_two_nodes_one_relationship() {
    let d = db();
    exec(
        "CREATE (a:Thought {title: 'trigger'})-[:LEADS_TO]->(b:Thought {title: 'spiral'})",
        &d,
    );
    match one(
        "MATCH p = (:Thought {title: 'trigger'})-[:LEADS_TO]->(:Thought) RETURN p",
        &d,
    ) {
        Value::Path(p) => {
            assert_eq!(p.nodes.len(), 2, "two nodes in a single-hop path");
            assert_eq!(
                p.relationships.len(),
                1,
                "one relationship in a single-hop path"
            );
            assert_eq!(
                p.nodes[0].properties.get("title"),
                Some(&Value::String("trigger".into()))
            );
            assert_eq!(
                p.nodes[1].properties.get("title"),
                Some(&Value::String("spiral".into()))
            );
            assert_eq!(p.relationships[0].kind, "LEADS_TO");
            // Endpoints of the relationship line up with the node sequence.
            assert_eq!(p.relationships[0].from_id, p.nodes[0].id);
            assert_eq!(p.relationships[0].to_id, p.nodes[1].id);
        }
        other => panic!("expected a Path, got {other:?}"),
    }
}

#[test]
fn length_of_named_path_is_relationship_count() {
    let d = db();
    exec("CREATE (a:Scene {title: 's1'})-[:NEXT]->(b:Scene {title: 's2'})-[:NEXT]->(c:Scene {title: 's3'})", &d);
    let v = one(
        "MATCH p = (:Scene {title: 's1'})-[:NEXT]->(:Scene)-[:NEXT]->(:Scene) RETURN length(p)",
        &d,
    );
    assert_eq!(as_i64(&v), 2, "two relationships → length 2");
}

#[test]
fn nodes_of_named_path_returns_nodes_in_order() {
    let d = db();
    exec("CREATE (a:Task {title: 'open'})-[:BLOCKS]->(b:Task {title: 'design'})-[:BLOCKS]->(c:Task {title: 'ship'})", &d);
    match one(
        "MATCH p = (:Task {title: 'open'})-[:BLOCKS]->(:Task)-[:BLOCKS]->(:Task) RETURN nodes(p)",
        &d,
    ) {
        Value::List(items) => {
            assert_eq!(items.len(), 3);
            let titles: Vec<String> = items
                .iter()
                .map(|n| match n {
                    Value::Node(nv) => match nv.properties.get("title") {
                        Some(Value::String(s)) => s.clone(),
                        _ => panic!("node missing title"),
                    },
                    other => panic!("expected Node, got {other:?}"),
                })
                .collect();
            assert_eq!(titles, vec!["open", "design", "ship"]);
        }
        other => panic!("expected a List of nodes, got {other:?}"),
    }
}

#[test]
fn relationships_of_named_path_returns_relationships_in_order() {
    let d = db();
    exec("CREATE (a:Account {title: 'cash'})-[:POSTS_TO {amount: 10}]->(b:Account {title: 'ar'})-[:POSTS_TO {amount: 20}]->(c:Account {title: 'rev'})", &d);
    match one("MATCH p = (:Account {title: 'cash'})-[:POSTS_TO]->(:Account)-[:POSTS_TO]->(:Account) RETURN relationships(p)", &d) {
        Value::List(items) => {
            assert_eq!(items.len(), 2);
            let amounts: Vec<i64> = items
                .iter()
                .map(|r| match r {
                    Value::Relationship(rv) => as_i64(rv.properties.get("amount").unwrap()),
                    other => panic!("expected Relationship, got {other:?}"),
                })
                .collect();
            assert_eq!(amounts, vec![10, 20]);
        }
        other => panic!("expected a List of relationships, got {other:?}"),
    }
}

#[test]
fn named_path_captures_anonymous_intermediate_nodes() {
    let d = db();
    // The middle node carries no variable in the pattern, yet must appear in
    // `nodes(p)` — paths capture every endpoint, bound or not.
    exec("CREATE (a:Bug {title: 'crash'})-[:DUPLICATES]->(b:Bug {title: 'dup'})-[:DUPLICATES]->(c:Bug {title: 'root'})", &d);
    let v = one("MATCH p = (src:Bug {title: 'crash'})-[:DUPLICATES]->(:Bug)-[:DUPLICATES]->(dst:Bug) RETURN length(p)", &d);
    assert_eq!(as_i64(&v), 2);
    match one("MATCH p = (src:Bug {title: 'crash'})-[:DUPLICATES]->(:Bug)-[:DUPLICATES]->(dst:Bug) RETURN nodes(p)", &d) {
        Value::List(items) => assert_eq!(items.len(), 3, "anonymous middle node still captured"),
        other => panic!("expected List, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Variable-length named paths
// ---------------------------------------------------------------------------

#[test]
fn variable_length_named_path_spans_every_hop() {
    let d = db();
    exec("CREATE (a:Task {title: 't1'})-[:DEPENDS_ON]->(b:Task {title: 't2'})-[:DEPENDS_ON]->(c:Task {title: 't3'})", &d);
    // The 2-hop path from t1 should report length 2 and 3 nodes.
    let rows = run("MATCH p = (:Task {title: 't1'})-[:DEPENDS_ON*1..3]->(:Task) RETURN length(p) AS len ORDER BY len", &d);
    let lens: Vec<i64> = rows.iter().map(|r| as_i64(&r[0])).collect();
    assert_eq!(lens, vec![1, 2], "reachable at 1 and 2 hops");
}

#[test]
fn variable_length_named_path_nodes_count_matches_length_plus_one() {
    let d = db();
    exec("CREATE (a:Module {title: 'm1'})-[:IMPORTS]->(b:Module {title: 'm2'})-[:IMPORTS]->(c:Module {title: 'm3'})", &d);
    let rows = run(
        "MATCH p = (:Module {title: 'm1'})-[:IMPORTS*1..5]->(:Module) RETURN length(p) AS len, size(nodes(p)) AS n ORDER BY len",
        &d,
    );
    for r in &rows {
        assert_eq!(as_i64(&r[1]), as_i64(&r[0]) + 1, "nodes == length + 1");
    }
}

// ---------------------------------------------------------------------------
// CREATE named paths
// ---------------------------------------------------------------------------

#[test]
fn create_named_path_binds_the_created_path() {
    let d = db();
    match one(
        "CREATE p = (a:Entry {title: 'mon'})-[:FOLLOWED_BY]->(b:Entry {title: 'tue'}) RETURN p",
        &d,
    ) {
        Value::Path(p) => {
            assert_eq!(p.nodes.len(), 2);
            assert_eq!(p.relationships.len(), 1);
            assert_eq!(p.relationships[0].kind, "FOLLOWED_BY");
        }
        other => panic!("expected Path, got {other:?}"),
    }
    // The path was actually persisted.
    let v = one(
        "MATCH (e:Entry {title: 'mon'})-[:FOLLOWED_BY]->(b:Entry) RETURN b.title",
        &d,
    );
    assert_eq!(v, Value::String("tue".into()));
}

#[test]
fn create_named_path_length_reflects_segments() {
    let d = db();
    let v = one(
        "CREATE p = (a:Step {title: 'a'})-[:THEN]->(b:Step {title: 'b'})-[:THEN]->(c:Step {title: 'c'}) RETURN length(p)",
        &d,
    );
    assert_eq!(as_i64(&v), 2);
}

// ---------------------------------------------------------------------------
// NULL semantics & type errors
// ---------------------------------------------------------------------------

#[test]
fn path_functions_propagate_null() {
    let d = db();
    assert_eq!(one("RETURN length(NULL)", &d), Value::Null);
    assert_eq!(one("RETURN nodes(NULL)", &d), Value::Null);
    assert_eq!(one("RETURN relationships(NULL)", &d), Value::Null);
}

#[test]
fn nodes_of_non_path_is_type_error() {
    let d = db();
    let err = run_err("RETURN nodes(42)", &d);
    assert!(
        matches!(err, ExecError::InvalidFunctionCall { .. }),
        "nodes() of a scalar should be an invalid-call error, got {err:?}"
    );
}

#[test]
fn relationships_of_non_path_is_type_error() {
    let d = db();
    let err = run_err("RETURN relationships('x')", &d);
    assert!(
        matches!(err, ExecError::InvalidFunctionCall { .. }),
        "relationships() of a scalar should be an invalid-call error, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Paths interacting with the rest of the language
// ---------------------------------------------------------------------------

#[test]
fn named_path_filtered_by_length_in_where() {
    let d = db();
    exec("CREATE (a:City {title: 'A'})-[:ROAD]->(b:City {title: 'B'})-[:ROAD]->(c:City {title: 'C'})", &d);
    // Only the 2-hop path satisfies the WHERE.
    let rows = run(
        "MATCH p = (:City {title: 'A'})-[:ROAD*1..3]->(:City) WHERE length(p) = 2 RETURN length(p)",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(as_i64(&rows[0][0]), 2);
}

#[test]
fn distinct_over_named_paths() {
    let d = db();
    exec("CREATE (a:P {title: 'x'})-[:R]->(b:P {title: 'y'})", &d);
    // Two MATCH patterns producing the same single path → DISTINCT collapses.
    let rows = run(
        "MATCH p = (:P {title: 'x'})-[:R]->(:P) RETURN DISTINCT length(p)",
        &d,
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn named_path_variable_usable_alongside_node_variables() {
    let d = db();
    exec(
        "CREATE (a:User {title: 'ann'})-[:KNOWS]->(b:User {title: 'bob'})",
        &d,
    );
    let rows = run(
        "MATCH p = (a:User {title: 'ann'})-[:KNOWS]->(b:User) RETURN a.title, b.title, length(p)",
        &d,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("ann".into()));
    assert_eq!(rows[0][1], Value::String("bob".into()));
    assert_eq!(as_i64(&rows[0][2]), 1);
}
