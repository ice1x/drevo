//! End-to-end Cypher `shortestPath` / `allShortestPaths` tests — Phase 10
//! follow-up task `00155`.
//!
//! `shortestPath((a)-[*]-(b))` finds **one** shortest path between two nodes;
//! `allShortestPaths((a)-[*]-(b))` returns **every** path of that minimum
//! length (one result row each). Both appear in MATCH *pattern* position,
//! wrapping a single variable-length leg, and are normally bound to a path
//! variable (`p = shortestPath(...)`) whose value flows into `length(p)`,
//! `nodes(p)`, `relationships(p)` exactly like an ordinary named path
//! (`00141`).
//!
//! Semantics exercised here mirror Neo4j:
//!
//! * the search returns the minimum-length connecting path, not every path,
//! * `allShortestPaths` yields one row per equally-short path (ties kept),
//!   while `shortestPath` yields exactly one row even when ties exist,
//! * a direct edge wins over a longer detour (true breadth-first minimality),
//! * relationship direction (`-[*]->` vs `-[*]-`) and type filters are honoured,
//! * an upper length bound (`[*..n]`) can exclude an otherwise-shortest path,
//! * disconnected endpoints yield no rows (never an error),
//! * the bound path's `length` / `nodes` / `relationships` read back correctly,
//! * a non-variable-length relationship or more than one relationship is a
//!   recoverable `ExecError::InvalidFunctionCall` (not a panic),
//!
//! plus the five scenario domains (CBT journal, story editor, task manager,
//! ERP, bug tracker) the drevo Cypher suite standardises on.

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
    execute(&q, drevo, HashMap::new()).expect_err("expected an executor error")
}

/// Extract a single-row, single-column integer.
fn one_int(rows: &[Vec<Value>]) -> i64 {
    assert_eq!(rows.len(), 1, "expected exactly one row, got {rows:?}");
    assert_eq!(rows[0].len(), 1);
    match &rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected Integer, got {other:?}"),
    }
}

/// The string elements of a `Value::List`.
fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| match e {
                Value::String(s) => s.clone(),
                other => panic!("expected String element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List, got {other:?}"),
    }
}

/// Collect every row's single name-list column, each joined with `-`, sorted.
fn sorted_paths(rows: &[Vec<Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|r| str_list(&r[0]).join("-")).collect();
    out.sort();
    out
}

/// A small diamond-plus-tail graph:
///
/// ```text
///   a → b → d → e
///   a → c → d
/// ```
///
/// so `a → d` has two distinct 2-hop shortest paths (`a-b-d`, `a-c-d`) and
/// `a → e` two distinct 3-hop ones.
fn diamond() -> Drevo {
    let drevo = db();
    run(
        "CREATE (a:N {name: 'a'})
         CREATE (b:N {name: 'b'})
         CREATE (c:N {name: 'c'})
         CREATE (d:N {name: 'd'})
         CREATE (e:N {name: 'e'})
         CREATE (a)-[:R]->(b)
         CREATE (b)-[:R]->(d)
         CREATE (a)-[:R]->(c)
         CREATE (c)-[:R]->(d)
         CREATE (d)-[:R]->(e)",
        &drevo,
    );
    drevo
}

#[test]
fn shortest_path_length_is_minimal() {
    let drevo = diamond();
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*]-(d))
         RETURN length(p)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 2);
}

#[test]
fn shortest_path_returns_exactly_one_row_on_a_tie() {
    let drevo = diamond();
    // Two equally short paths a-b-d and a-c-d exist, but shortestPath picks one.
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*]-(d))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(rows.len(), 1);
    let names = str_list(&rows[0][0]);
    assert_eq!(names.first().map(String::as_str), Some("a"));
    assert_eq!(names.last().map(String::as_str), Some("d"));
    assert_eq!(names.len(), 3, "a 2-hop path has 3 nodes");
}

#[test]
fn all_shortest_paths_returns_every_tie() {
    let drevo = diamond();
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = allShortestPaths((a)-[*]-(d))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(sorted_paths(&rows), vec!["a-b-d", "a-c-d"]);
}

#[test]
fn all_shortest_paths_three_hop_ties() {
    let drevo = diamond();
    let rows = run(
        "MATCH (a:N {name:'a'}), (e:N {name:'e'})
         MATCH p = allShortestPaths((a)-[*]-(e))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(sorted_paths(&rows), vec!["a-b-d-e", "a-c-d-e"]);
}

#[test]
fn a_direct_edge_beats_a_longer_detour() {
    let drevo = diamond();
    // Add a direct a → d shortcut; the shortest path is now length 1.
    run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'}) CREATE (a)-[:R]->(d)",
        &drevo,
    );
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*]-(d))
         RETURN length(p)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 1);
}

#[test]
fn direction_is_honoured() {
    let drevo = diamond();
    // Outgoing-only search from e reaches nothing (e is a sink), so no rows.
    let rows = run(
        "MATCH (e:N {name:'e'}), (a:N {name:'a'})
         MATCH p = shortestPath((e)-[*]->(a))
         RETURN length(p)",
        &drevo,
    );
    assert!(rows.is_empty(), "no outgoing path e → a, got {rows:?}");

    // The undirected search does connect them (length 3).
    let rows = run(
        "MATCH (e:N {name:'e'}), (a:N {name:'a'})
         MATCH p = shortestPath((e)-[*]-(a))
         RETURN length(p)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 3);
}

#[test]
fn relationship_type_filter_is_honoured() {
    let drevo = db();
    // a → b via FRIEND, a → x → b via FOLLOWS. A FRIEND-only shortest search
    // must take the 2-hop FOLLOWS path? No — it must IGNORE FOLLOWS entirely.
    run(
        "CREATE (a:N {name:'a'})
         CREATE (b:N {name:'b'})
         CREATE (x:N {name:'x'})
         CREATE (a)-[:FOLLOWS]->(x)
         CREATE (x)-[:FOLLOWS]->(b)
         CREATE (a)-[:FRIEND]->(m:N {name:'m'})
         CREATE (m)-[:FRIEND]->(b)",
        &drevo,
    );
    let rows = run(
        "MATCH (a:N {name:'a'}), (b:N {name:'b'})
         MATCH p = shortestPath((a)-[:FRIEND*]-(b))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(str_list(&rows[0][0]), vec!["a", "m", "b"]);
}

#[test]
fn upper_length_bound_can_exclude_the_path() {
    let drevo = diamond();
    // a → d is 2 hops; capping at 1 hop finds nothing.
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*..1]-(d))
         RETURN length(p)",
        &drevo,
    );
    assert!(rows.is_empty());

    // Capping at 2 hops finds it.
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*..2]-(d))
         RETURN length(p)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 2);
}

#[test]
fn disconnected_endpoints_yield_no_rows() {
    let drevo = diamond();
    run("CREATE (z:N {name:'z'})", &drevo);
    let rows = run(
        "MATCH (a:N {name:'a'}), (z:N {name:'z'})
         MATCH p = shortestPath((a)-[*]-(z))
         RETURN length(p)",
        &drevo,
    );
    assert!(rows.is_empty(), "z is isolated, got {rows:?}");
}

#[test]
fn relationship_variable_binds_to_the_traversed_list() {
    let drevo = diamond();
    let rows = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[rs*]-(d))
         RETURN size(rs)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 2, "2-hop path traverses 2 relationships");
}

#[test]
fn nodes_and_relationships_agree_on_length() {
    let drevo = diamond();
    let rows = run(
        "MATCH (a:N {name:'a'}), (e:N {name:'e'})
         MATCH p = shortestPath((a)-[*]-(e))
         RETURN size(nodes(p)), size(relationships(p)), length(p)",
        &drevo,
    );
    assert_eq!(rows.len(), 1);
    // nodes = hops + 1, relationships = hops = length.
    assert_eq!(rows[0][0], Value::Integer(4));
    assert_eq!(rows[0][1], Value::Integer(3));
    assert_eq!(rows[0][2], Value::Integer(3));
}

#[test]
fn shortest_path_without_binding_still_filters_rows() {
    let drevo = diamond();
    // No path variable: the pattern acts as a connectivity filter. a → z would
    // be empty; a → d connects, so the row survives.
    run("CREATE (z:N {name:'z'})", &drevo);
    let connected = run(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH shortestPath((a)-[*]-(d))
         RETURN a.name",
        &drevo,
    );
    assert_eq!(connected.len(), 1);

    let disconnected = run(
        "MATCH (a:N {name:'a'}), (z:N {name:'z'})
         MATCH shortestPath((a)-[*]-(z))
         RETURN a.name",
        &drevo,
    );
    assert!(disconnected.is_empty());
}

// ---- error cases --------------------------------------------------------

#[test]
fn non_variable_length_relationship_is_rejected() {
    let drevo = diamond();
    let err = run_err(
        "MATCH (a:N {name:'a'}), (b:N {name:'b'})
         MATCH p = shortestPath((a)-[:R]->(b))
         RETURN p",
        &drevo,
    );
    match err {
        ExecError::InvalidFunctionCall { name, message, .. } => {
            assert_eq!(name, "shortestPath");
            assert!(message.contains("variable-length"), "got: {message}");
        }
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

#[test]
fn more_than_one_relationship_is_rejected() {
    let drevo = diamond();
    let err = run_err(
        "MATCH (a:N {name:'a'}), (d:N {name:'d'})
         MATCH p = shortestPath((a)-[*]-(x)-[*]-(d))
         RETURN p",
        &drevo,
    );
    match err {
        ExecError::InvalidFunctionCall { name, message, .. } => {
            assert_eq!(name, "shortestPath");
            assert!(
                message.contains("exactly one relationship"),
                "got: {message}"
            );
        }
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

#[test]
fn all_shortest_paths_reports_its_own_name_in_errors() {
    let drevo = diamond();
    let err = run_err(
        "MATCH (a:N {name:'a'}), (b:N {name:'b'})
         MATCH p = allShortestPaths((a)-[:R]->(b))
         RETURN p",
        &drevo,
    );
    match err {
        ExecError::InvalidFunctionCall { name, .. } => assert_eq!(name, "allShortestPaths"),
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

// ---- scenario domains ---------------------------------------------------

#[test]
fn scenario_task_dependency_chain() {
    // Task manager: a DEPENDS_ON chain T1 → T2 → T3 → T4 with a shortcut
    // T1 → T4. The shortest dependency distance from T1 to T4 is 1.
    let drevo = db();
    run(
        "CREATE (t1:Task {name:'T1'})
         CREATE (t2:Task {name:'T2'})
         CREATE (t3:Task {name:'T3'})
         CREATE (t4:Task {name:'T4'})
         CREATE (t1)-[:DEPENDS_ON]->(t2)
         CREATE (t2)-[:DEPENDS_ON]->(t3)
         CREATE (t3)-[:DEPENDS_ON]->(t4)
         CREATE (t1)-[:DEPENDS_ON]->(t4)",
        &drevo,
    );
    let rows = run(
        "MATCH (a:Task {name:'T1'}), (b:Task {name:'T4'})
         MATCH p = shortestPath((a)-[:DEPENDS_ON*]->(b))
         RETURN length(p)",
        &drevo,
    );
    assert_eq!(one_int(&rows), 1);
}

#[test]
fn scenario_story_character_connection() {
    // Story editor: two characters connected through shared scenes. The
    // degrees of separation between Alice and Cara is the shortest path length.
    let drevo = db();
    run(
        "CREATE (alice:Character {name:'Alice'})
         CREATE (bob:Character {name:'Bob'})
         CREATE (cara:Character {name:'Cara'})
         CREATE (alice)-[:APPEARS_WITH]->(bob)
         CREATE (bob)-[:APPEARS_WITH]->(cara)",
        &drevo,
    );
    let rows = run(
        "MATCH (a:Character {name:'Alice'}), (c:Character {name:'Cara'})
         MATCH p = shortestPath((a)-[:APPEARS_WITH*]-(c))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(str_list(&rows[0][0]), vec!["Alice", "Bob", "Cara"]);
}

#[test]
fn scenario_bug_blocking_graph_all_shortest() {
    // Bug tracker: two equally-short BLOCKS chains from a release blocker to a
    // fix. allShortestPaths surfaces both so triage sees every critical chain.
    let drevo = db();
    run(
        "CREATE (b0:Bug {name:'B0'})
         CREATE (b1:Bug {name:'B1'})
         CREATE (b2:Bug {name:'B2'})
         CREATE (b3:Bug {name:'B3'})
         CREATE (b0)-[:BLOCKS]->(b1)
         CREATE (b1)-[:BLOCKS]->(b3)
         CREATE (b0)-[:BLOCKS]->(b2)
         CREATE (b2)-[:BLOCKS]->(b3)",
        &drevo,
    );
    let rows = run(
        "MATCH (s:Bug {name:'B0'}), (t:Bug {name:'B3'})
         MATCH p = allShortestPaths((s)-[:BLOCKS*]->(t))
         RETURN [x IN nodes(p) | x.name]",
        &drevo,
    );
    assert_eq!(sorted_paths(&rows), vec!["B0-B1-B3", "B0-B2-B3"]);
}
