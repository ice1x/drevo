//! Phase 6 slice 1 — the Cypher executor runs over the native `drevo-core`
//! engine (RFC `docs/rfc-native-core.md`, #307).
//!
//! [`execute_on_engine`](drevo::cypher::executor::execute_on_engine) drives the
//! same executor against a [`NativeGraph`](drevo::native::NativeGraph) instead
//! of the KV [`Drevo`](drevo::db::Drevo). The core graph language — CREATE,
//! MATCH, MERGE, SET, DELETE, traversal, RETURN — must produce the same results
//! it does on KV, because both engines sit behind the shared
//! [`GraphEngine`](drevo::engine::GraphEngine) seam. Queries that reach for a
//! KV-only secondary subsystem (FTS, vector, keyword extraction) must fail
//! deterministically with [`ExecError::EngineCapability`], not panic.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, execute_on_engine, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::native::NativeGraph;

fn run_native(source: &str, g: &NativeGraph) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute_on_engine(&q, g, HashMap::new())
        .expect("execute on native")
        .rows
}

fn string(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s.as_str()),
        _ => None,
    }
}

#[test]
fn create_match_return_on_native() {
    let g = NativeGraph::new();
    run_native(
        "CREATE (:Person {name: 'alice'}), (:Person {name: 'bob'})",
        &g,
    );

    let mut names: Vec<String> = run_native("MATCH (p:Person) RETURN p.name", &g)
        .iter()
        .filter_map(|row| string(&row[0]).map(String::from))
        .collect();
    names.sort();
    assert_eq!(names, vec!["alice".to_string(), "bob".to_string()]);
}

#[test]
fn relationship_traversal_on_native() {
    let g = NativeGraph::new();
    run_native(
        "CREATE (:Person {name: 'alice'})-[:KNOWS]->(:Person {name: 'bob'})",
        &g,
    );

    let rows = run_native(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
        &g,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(string(&rows[0][0]), Some("alice"));
    assert_eq!(string(&rows[0][1]), Some("bob"));
}

#[test]
fn merge_set_delete_on_native() {
    let g = NativeGraph::new();
    // MERGE creates on first sight...
    run_native("MERGE (p:Person {name: 'carol'})", &g);
    // ...and matches (no duplicate) on the second.
    run_native("MERGE (p:Person {name: 'carol'})", &g);
    assert_eq!(run_native("MATCH (p:Person) RETURN p.name", &g).len(), 1);

    // SET a property, read it back.
    run_native("MATCH (p:Person {name: 'carol'}) SET p.age = 30", &g);
    let rows = run_native("MATCH (p:Person {name: 'carol'}) RETURN p.age", &g);
    assert_eq!(rows[0][0], Value::Integer(30));

    // DELETE removes it.
    run_native("MATCH (p:Person {name: 'carol'}) DELETE p", &g);
    assert_eq!(run_native("MATCH (p:Person) RETURN p", &g).len(), 0);
}

#[test]
fn native_and_kv_agree_on_the_same_workload() {
    let script = [
        "CREATE (:Task {title: 'design'})-[:BLOCKS]->(:Task {title: 'build'})",
        "CREATE (:Task {title: 'ship'})",
        "MATCH (t:Task {title: 'build'}) SET t.done = true",
    ];

    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    for stmt in script {
        let q = parse(stmt).unwrap();
        execute_on_engine(&q, &native, HashMap::new()).unwrap();
        execute(&q, &kv, HashMap::new()).unwrap();
    }

    let query = parse("MATCH (a:Task)-[:BLOCKS]->(b:Task) RETURN a.title, b.title").unwrap();
    let n_rows = execute_on_engine(&query, &native, HashMap::new())
        .unwrap()
        .rows;
    let k_rows = execute(&query, &kv, HashMap::new()).unwrap().rows;
    assert_eq!(n_rows, k_rows);
    assert_eq!(n_rows.len(), 1);
    assert_eq!(string(&n_rows[0][0]), Some("design"));
    assert_eq!(string(&n_rows[0][1]), Some("build"));
}

#[test]
fn secondary_subsystem_reports_engine_capability_on_native() {
    let g = NativeGraph::new();
    run_native(
        "CREATE (:Doc {title: 'hello world', body: 'the quick brown fox'})",
        &g,
    );

    // FTS is a KV-only secondary index; on the native engine it must fail with
    // a clear capability error, not panic or silently return nothing.
    let q = parse("CALL fts.search('hello', 10) YIELD node, score RETURN node, score").unwrap();
    let err = execute_on_engine(&q, &g, HashMap::new()).unwrap_err();
    assert!(
        matches!(err, ExecError::EngineCapability { ref feature } if feature == "fts.search"),
        "expected EngineCapability for fts.search, got: {err:?}"
    );
}
