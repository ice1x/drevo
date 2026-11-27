//! Schema procedures and `SHOW PROCEDURES` / `SHOW FUNCTIONS` (#869).
//!
//! `db.schema.nodeTypeProperties()`, `db.schema.relTypeProperties()` and
//! `db.schema.visualization()` describe how labels, relationship types and
//! properties fit together; `dbms.components()` names the server;
//! `SHOW PROCEDURES` / `SHOW FUNCTIONS` list what can be called.

use std::collections::HashMap;

use drevo::cypher::executor::{execute_on_engine as execute, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

/// Ann (30) knows Bob (25) since 2020, Bob knows Cid (41) since 2021, Cid
/// works at Acme.
fn people() -> NativeGraph {
    let db = NativeGraph::new();
    run(
        "CREATE (a:Person {name: 'Ann', age: 30})-[:KNOWS {since: 2020}]->\
         (b:Person {name: 'Bob', age: 25}), \
         (b)-[:KNOWS {since: 2021}]->(c:Person {name: 'Cid', age: 41}), \
         (c)-[:WORKS_AT]->(:Company {name: 'Acme'})",
        &db,
    );
    db
}

fn try_run(source: &str, db: &NativeGraph) -> Result<ExecResult, String> {
    let query = parse(source).map_err(|e| e.to_string())?;
    execute(&query, db, HashMap::new()).map_err(|e| e.to_string())
}

fn run(source: &str, db: &NativeGraph) -> ExecResult {
    try_run(source, db).unwrap_or_else(|e| panic!("`{source}` failed: {e}"))
}

fn rows(source: &str, db: &NativeGraph) -> Vec<Vec<Value>> {
    run(source, db).rows
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}

fn strings(source: &str, db: &NativeGraph) -> Vec<String> {
    rows(source, db)
        .into_iter()
        .map(|row| match &row[0] {
            Value::String(text) => text.clone(),
            other => panic!("`{source}` returns strings, got {other:?}"),
        })
        .collect()
}

#[test]
fn node_type_properties_lists_each_label_s_properties() {
    let db = people();
    let got = rows(
        "CALL db.schema.nodeTypeProperties() \
         YIELD nodeLabels, propertyName, propertyTypes, mandatory \
         RETURN nodeLabels, propertyName, propertyTypes, mandatory \
         ORDER BY nodeLabels[0], propertyName",
        &db,
    );
    let labels = |l: &str| Value::List(vec![s(l)]);
    let types = |t: &str| Value::List(vec![s(t)]);
    assert_eq!(
        got,
        vec![
            vec![
                labels("Company"),
                s("name"),
                types("String"),
                Value::Bool(true)
            ],
            vec![labels("Person"), s("age"), types("Long"), Value::Bool(true)],
            vec![
                labels("Person"),
                s("name"),
                types("String"),
                Value::Bool(true)
            ],
        ]
    );
}

#[test]
fn a_property_some_nodes_lack_is_not_mandatory() {
    let db = people();
    run("CREATE (:Person {name: 'Dee'})", &db);
    let got = rows(
        "CALL db.schema.nodeTypeProperties() YIELD nodeLabels, propertyName, mandatory \
         WHERE propertyName = 'age' RETURN mandatory",
        &db,
    );
    assert_eq!(got, vec![vec![Value::Bool(false)]]);
}

#[test]
fn rel_type_properties_lists_each_type_s_properties() {
    let db = people();
    let got = rows(
        "CALL db.schema.relTypeProperties() YIELD relType, propertyName, propertyTypes \
         WHERE propertyName IS NOT NULL \
         RETURN relType, propertyName, propertyTypes",
        &db,
    );
    assert_eq!(
        got,
        vec![vec![
            s(":`KNOWS`"),
            s("since"),
            Value::List(vec![s("Long")])
        ]]
    );
}

#[test]
fn visualization_returns_one_node_per_label_and_one_edge_per_shape() {
    let db = people();
    let got = rows(
        "CALL db.schema.visualization() YIELD nodes, relationships \
         RETURN size(nodes) AS n, size(relationships) AS r",
        &db,
    );
    assert_eq!(got, vec![vec![i(2), i(2)]]);
}

#[test]
fn components_names_the_server_and_its_version() {
    let db = people();
    let got = rows(
        "CALL dbms.components() YIELD name, versions, edition RETURN name, size(versions)",
        &db,
    );
    assert_eq!(got.len(), 1);
    assert!(matches!(&got[0][0], Value::String(name) if !name.is_empty()));
    assert_eq!(got[0][1], i(1));
}

#[test]
fn show_procedures_lists_what_can_be_called() {
    let db = people();
    let names = strings(
        "SHOW PROCEDURES YIELD name WHERE name STARTS WITH 'db.' RETURN name ORDER BY name",
        &db,
    );
    for expected in [
        "db.labels",
        "db.propertyKeys",
        "db.relationshipTypes",
        "db.schema.nodeTypeProperties",
        "db.schema.relTypeProperties",
        "db.schema.visualization",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
}

#[test]
fn show_functions_lists_the_built_in_functions() {
    let db = people();
    let names = strings("SHOW FUNCTIONS YIELD name RETURN name ORDER BY name", &db);
    for expected in [
        "toUpper", "size", "coalesce", "count", "collect", "datetime",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
}

#[test]
fn a_label_set_and_a_type_without_properties_have_a_row_of_their_own() {
    let db = people();
    run(
        "CREATE (:Person:Admin {name: 'Root', tags: ['a', 'b'], scores: [1, 2.5]}), (:Empty)",
        &db,
    );
    let got = rows(
        "CALL db.schema.nodeTypeProperties() \
         YIELD nodeType, nodeLabels, propertyName, propertyTypes, mandatory \
         WHERE nodeType <> ':`Person`' AND nodeType <> ':`Company`' \
         RETURN nodeType, propertyName, propertyTypes, mandatory ORDER BY nodeType, propertyName",
        &db,
    );
    let types = |names: &[&str]| Value::List(names.iter().map(|n| s(n)).collect());
    assert_eq!(
        got,
        vec![
            vec![
                s(":`Admin`:`Person`"),
                s("name"),
                types(&["String"]),
                Value::Bool(true)
            ],
            vec![
                s(":`Admin`:`Person`"),
                s("scores"),
                types(&["List"]),
                Value::Bool(true)
            ],
            vec![
                s(":`Admin`:`Person`"),
                s("tags"),
                types(&["StringArray"]),
                Value::Bool(true)
            ],
            vec![s(":`Empty`"), Value::Null, Value::Null, Value::Bool(false)],
        ]
    );
    let got = rows(
        "CALL db.schema.relTypeProperties() YIELD relType, propertyName, mandatory \
         RETURN relType, propertyName, mandatory ORDER BY relType",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s(":`KNOWS`"), s("since"), Value::Bool(true)],
            vec![s(":`WORKS_AT`"), Value::Null, Value::Bool(false)],
        ]
    );
}

#[test]
fn visualization_names_the_labels_and_what_joins_them() {
    let db = people();
    let got = rows(
        "CALL db.schema.visualization() YIELD nodes, relationships \
         UNWIND relationships AS r \
         RETURN [n IN nodes | n.name] AS labels, r.from + '-' + r.type + '->' + r.to AS shape \
         ORDER BY shape",
        &db,
    );
    let labels = Value::List(vec![s("Company"), s("Person")]);
    assert_eq!(
        got,
        vec![
            vec![labels.clone(), s("Person-KNOWS->Person")],
            vec![labels, s("Person-WORKS_AT->Company")],
        ]
    );
    let db = NativeGraph::new();
    assert_eq!(
        rows("CALL db.schema.visualization() YIELD nodes, relationships RETURN size(nodes), size(relationships)", &db),
        vec![vec![i(0), i(0)]]
    );
}

#[test]
fn components_agrees_with_drevo_info() {
    let db = people();
    let got = rows(
        "CALL dbms.components() YIELD name, versions, edition \
         CALL drevo.info() YIELD version \
         RETURN name, versions[0] = version AS same, edition",
        &db,
    );
    assert_eq!(
        got,
        vec![vec![s("drevo"), Value::Bool(true), s("community")]]
    );
}

#[test]
fn show_procedures_says_what_a_read_session_may_call() {
    let db = people();
    let got = run("SHOW PROCEDURES", &db);
    assert_eq!(got.columns, ["name", "mode", "outputs"]);
    assert!(got.rows.len() > 80, "{} procedures", got.rows.len());
    let names: Vec<&Value> = got.rows.iter().map(|row| &row[0]).collect();
    let mut sorted = names.clone();
    sorted.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(names, sorted, "by name");
    let got = rows(
        "SHOW PROCEDURES YIELD name, mode, outputs \
         WHERE name IN ['db.labels', 'drevo.memory.addMessage', 'fts.search'] \
         RETURN name, mode, outputs ORDER BY name",
        &db,
    );
    let list = |names: &[&str]| Value::List(names.iter().map(|n| s(n)).collect());
    assert_eq!(
        got,
        vec![
            vec![s("db.labels"), s("READ"), list(&["label"])],
            vec![s("drevo.memory.addMessage"), s("WRITE"), list(&["node"])],
            vec![s("fts.search"), s("READ"), list(&["node", "score"])],
        ]
    );
    // `WHERE` without `YIELD`, like the other SHOW commands.
    let got = run("SHOW PROCEDURES WHERE name = 'dbms.components'", &db);
    assert_eq!(got.rows.len(), 1);
}

#[test]
fn every_listed_procedure_is_known_to_call() {
    let db = people();
    let names = strings("SHOW PROCEDURES YIELD name RETURN name", &db);
    for name in names {
        // With no arguments the call may be refused for its arguments, but
        // never as a procedure that does not exist.
        if let Err(err) = try_run(&format!("EXPLAIN CALL {name}()"), &db) {
            assert!(!err.contains("no such procedure"), "{name}: {err}");
        }
    }
}

#[test]
fn show_functions_names_them_as_they_are_written_and_says_their_kind() {
    let db = people();
    let got = rows(
        "SHOW FUNCTIONS YIELD name, category \
         WHERE name IN ['toUpper', 'elementId', 'randomUUID', 'percentileCont', 'startNode', \
                        'duration.between', 'toupper'] \
         RETURN name, category ORDER BY name",
        &db,
    );
    assert_eq!(
        got,
        vec![
            vec![s("duration.between"), s("scalar")],
            vec![s("elementId"), s("scalar")],
            vec![s("percentileCont"), s("aggregating")],
            vec![s("randomUUID"), s("scalar")],
            vec![s("startNode"), s("scalar")],
            vec![s("toUpper"), s("scalar")],
        ]
    );
    // A name works in any case, as before the list existed.
    assert_eq!(
        rows(
            "RETURN TOUPPER('a') AS a, tolower('B') AS b, COUNT(*) AS c",
            &db
        ),
        vec![vec![s("A"), s("b"), i(1)]]
    );
}

#[test]
fn the_new_commands_read_and_nothing_else() {
    use drevo::cypher::executor::read_access_refusal;
    for query in [
        "SHOW PROCEDURES",
        "SHOW FUNCTIONS YIELD name RETURN name",
        "CALL db.schema.nodeTypeProperties()",
        "CALL db.schema.relTypeProperties()",
        "CALL db.schema.visualization()",
        "CALL dbms.components()",
    ] {
        let parsed = parse(query).unwrap_or_else(|e| panic!("`{query}`: {e}"));
        assert!(read_access_refusal(&parsed).is_none(), "{query}");
    }
    let db = people();
    let err = try_run("SHOW TABLES", &db).expect_err("not a SHOW command");
    assert!(
        err.contains("PROCEDURES") && err.contains("INDEXES"),
        "{err}"
    );
}
