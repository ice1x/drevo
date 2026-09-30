//! Placeholder titles are storage plumbing, not user properties.
//!
//! drevo keeps every node's `title` globally unique, so a node created in
//! Cypher without one gets a synthesised placeholder
//! (`__cypher__:<Label>:<uuid>`). That placeholder must stay invisible to
//! Cypher — as in Neo4j, a node created as `CREATE (n:X {x: 1})` has exactly
//! the one property `x`:
//!
//! - `n.title` is `null`, and `keys(n)` / `properties(n)` do not list it;
//! - copying a node's properties onto another (`SET b = properties(a)`) does
//!   not carry the placeholder across — which would otherwise collide with
//!   the unique-title rule;
//! - a real title — given at create time or set later — is an ordinary
//!   property, and removing it hides the fresh placeholder again.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn one(svc: &NativeService, q: &str) -> Value {
    let res = run(svc, q);
    assert_eq!(res.rows.len(), 1, "`{q}` should return one row");
    res.rows[0][0].clone()
}

fn strings(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::String(s.to_string())).collect())
}

#[test]
fn untitled_node_has_no_title_property() {
    let db = NativeService::in_memory();
    run(&db, "CREATE (:Probe {x: 1})");

    assert_eq!(one(&db, "MATCH (n:Probe) RETURN n.title AS t"), Value::Null);
    assert_eq!(
        one(&db, "MATCH (n:Probe) RETURN keys(n) AS k"),
        strings(&["x"])
    );
    match one(&db, "MATCH (n:Probe) RETURN properties(n) AS p") {
        Value::Map(m) => {
            assert!(!m.contains_key("title"), "placeholder leaked: {m:?}");
            assert_eq!(m.get("x"), Some(&Value::Integer(1)));
        }
        other => panic!("expected Map, got {other:?}"),
    }
}

#[test]
fn returned_node_value_omits_the_placeholder() {
    let db = NativeService::in_memory();
    let res = run(&db, "CREATE (n:Probe {x: 1}) RETURN n");
    match &res.rows[0][0] {
        Value::Node(n) => assert!(
            !n.properties.contains_key("title"),
            "placeholder leaked into the node: {:?}",
            n.properties
        ),
        other => panic!("expected Node, got {other:?}"),
    }
}

#[test]
fn untitled_nodes_match_title_is_null() {
    let db = NativeService::in_memory();
    run(
        &db,
        "CREATE (:Probe {x: 1}), (:Probe {x: 2, title: 'named'})",
    );
    assert_eq!(
        one(&db, "MATCH (n:Probe) WHERE n.title IS NULL RETURN n.x AS x"),
        Value::Integer(1)
    );
}

#[test]
fn copying_properties_does_not_carry_the_placeholder() {
    let db = NativeService::in_memory();
    run(&db, "CREATE (:Src {x: 1, y: 'a'})");
    run(&db, "MATCH (a:Src) CREATE (b:Dst) SET b = properties(a)");
    run(&db, "MATCH (a:Src) CREATE (b:Dst2) SET b += properties(a)");

    for label in ["Dst", "Dst2"] {
        assert_eq!(
            one(&db, &format!("MATCH (n:{label}) RETURN keys(n) AS k")),
            strings(&["x", "y"]),
            "{label}"
        );
    }
}

#[test]
fn a_real_title_is_an_ordinary_property() {
    let db = NativeService::in_memory();
    run(&db, "CREATE (:Probe {title: 'given', x: 1})");
    assert_eq!(
        one(&db, "MATCH (n:Probe) RETURN keys(n) AS k"),
        strings(&["title", "x"])
    );
    assert_eq!(
        one(&db, "MATCH (n:Probe) RETURN n.title AS t"),
        Value::String("given".into())
    );
}

#[test]
fn set_then_remove_title_round_trips() {
    let db = NativeService::in_memory();
    run(&db, "CREATE (:Probe {x: 1})");

    run(&db, "MATCH (n:Probe) SET n.title = 'later'");
    assert_eq!(
        one(&db, "MATCH (n:Probe) RETURN n.title AS t"),
        Value::String("later".into())
    );

    run(&db, "MATCH (n:Probe) REMOVE n.title");
    assert_eq!(one(&db, "MATCH (n:Probe) RETURN n.title AS t"), Value::Null);
    assert_eq!(
        one(&db, "MATCH (n:Probe) RETURN keys(n) AS k"),
        strings(&["x"])
    );
}
