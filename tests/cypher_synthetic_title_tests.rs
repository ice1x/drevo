//! Untitled nodes have no title.
//!
//! A node created in Cypher without a `title` is stored with an empty title
//! (#602). Older versions stored a placeholder (`__cypher__:<Label>:<uuid>`)
//! because titles had to be unique; titles no longer are (#595). Nodes that
//! still carry such a placeholder read exactly like untitled ones. As in
//! Neo4j, a node created as `CREATE (n:X {x: 1})` has exactly the one
//! property `x`:
//!
//! - `n.title` is `null`, and `keys(n)` / `properties(n)` do not list it;
//! - copying a node's properties onto another (`SET b = properties(a)`) adds
//!   no title;
//! - a real title — given at create time or set later — is an ordinary
//!   property, and removing it leaves the node untitled again.

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

fn stored_title(db: &NativeService, label: &str) -> String {
    let id = match one(db, &format!("MATCH (n:{label}) RETURN id(n) AS id")) {
        Value::Integer(i) => u64::try_from(i).expect("id"),
        other => panic!("expected an id, got {other:?}"),
    };
    db.get_node(id).expect("node").title
}

#[test]
fn untitled_nodes_are_stored_with_an_empty_title() {
    let db = NativeService::in_memory();
    run(&db, "CREATE (:Probe {x: 1})");
    assert_eq!(stored_title(&db, "Probe"), "");

    // Replacing the property map without a title clears it.
    run(&db, "CREATE (:Named {title: 'given'})");
    run(&db, "MATCH (n:Named) SET n = {x: 2}");
    assert_eq!(stored_title(&db, "Named"), "");

    // So does removing it.
    run(&db, "CREATE (:Again {title: 'given'})");
    run(&db, "MATCH (n:Again) REMOVE n.title");
    assert_eq!(stored_title(&db, "Again"), "");

    // Many untitled nodes coexist; titles are not unique (#595).
    run(&db, "UNWIND range(1, 3) AS i CREATE (:Many {i: i})");
    assert_eq!(db.nodes_by_title("").len(), 6);
}

#[test]
fn a_stored_placeholder_from_older_versions_still_reads_as_untitled() {
    let db = NativeService::in_memory();
    db.create_node(drevo::model::NewNode {
        kind: "Legacy".into(),
        title: "__cypher__:Legacy:0190aa".into(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    })
    .expect("create");
    assert_eq!(
        one(&db, "MATCH (n:Legacy) RETURN n.title AS t"),
        Value::Null
    );
    assert_eq!(
        one(&db, "MATCH (n:Legacy) RETURN keys(n) AS k"),
        Value::List(vec![])
    );
}
