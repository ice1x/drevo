//! End-to-end Cypher variable-length path tests — Phase 10 task `00069`.
//!
//! Exercises `[*N..M]` BFS traversals (the last task of Phase 10)
//! across the five drevo target scenario domains plus cross-scenario
//! regressions. Cypher trail uniqueness is enforced — no relationship
//! is traversed twice within a single path; nodes may repeat. The
//! variable-length expansion is the building block for ancestry /
//! reachability / shortest-path-by-length queries every drevo target
//! scenario reaches for once their data model has chains or trees.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn names_sorted(rows: &[Vec<Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string, got {:?}", other),
        })
        .collect();
    out.sort();
    out
}

fn int(rows: &[Vec<Value>], r: usize, c: usize) -> i64 {
    match &rows[r][c] {
        Value::Integer(i) => *i,
        other => panic!("expected integer at ({},{}), got {:?}", r, c, other),
    }
}

// ===== Scenario 1 — CBT journal =============================================

#[test]
fn cbt_reach_all_consequent_thoughts_via_chain() {
    let db = db();
    // A reasoning chain: anxiety → catastrophising → avoidance → relapse.
    run("CREATE (:Thought {name: 'anxiety'})", &db);
    run("CREATE (:Thought {name: 'catastrophising'})", &db);
    run("CREATE (:Thought {name: 'avoidance'})", &db);
    run("CREATE (:Thought {name: 'relapse'})", &db);
    for (a, b) in [
        ("anxiety", "catastrophising"),
        ("catastrophising", "avoidance"),
        ("avoidance", "relapse"),
    ] {
        run(
            &format!(
                "MATCH (a:Thought {{name: '{}'}}), (b:Thought {{name: '{}'}}) CREATE (a)-[:LEADS_TO]->(b)",
                a, b
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (root:Thought {name: 'anxiety'})-[:LEADS_TO*]->(t:Thought) RETURN t.name AS name",
        &db,
    );
    assert_eq!(
        names_sorted(&rows),
        vec!["avoidance", "catastrophising", "relapse"]
    );
}

// ===== Scenario 2 — Story editor ============================================

#[test]
fn story_descendant_scenes_via_next_chain() {
    let db = db();
    // S1 → S2 → S3 → S4, S5 (orphan).
    for n in ["S1", "S2", "S3", "S4", "S5"] {
        run(&format!("CREATE (:Scene {{title: '{}'}})", n), &db);
    }
    for (a, b) in [("S1", "S2"), ("S2", "S3"), ("S3", "S4")] {
        run(
            &format!(
                "MATCH (a:Scene {{title: '{}'}}), (b:Scene {{title: '{}'}}) CREATE (a)-[:NEXT]->(b)",
                a, b
            ),
            &db,
        );
    }
    // From S1, all reachable via 1..3 hops.
    let rows = run(
        "MATCH (s:Scene {title: 'S1'})-[:NEXT*1..3]->(t:Scene) RETURN t.title AS title",
        &db,
    );
    assert_eq!(names_sorted(&rows), vec!["S2", "S3", "S4"]);
    // From S5, nothing reachable.
    let rows = run(
        "MATCH (s:Scene {title: 'S5'})-[:NEXT*1..3]->(t:Scene) RETURN t.title AS title",
        &db,
    );
    assert_eq!(rows.len(), 0);
}

// ===== Scenario 3 — IT task manager =========================================

#[test]
fn task_manager_subtree_via_has_subtask_chain() {
    let db = db();
    // T1 → T1a (→ T1aa), T1 → T1b. T2 is independent.
    for n in ["T1", "T1a", "T1aa", "T1b", "T2"] {
        run(&format!("CREATE (:Task {{title: '{}'}})", n), &db);
    }
    for (a, b) in [("T1", "T1a"), ("T1a", "T1aa"), ("T1", "T1b")] {
        run(
            &format!(
                "MATCH (a:Task {{title: '{}'}}), (b:Task {{title: '{}'}}) CREATE (a)-[:HAS_SUBTASK]->(b)",
                a, b
            ),
            &db,
        );
    }
    // All descendants of T1.
    let rows = run(
        "MATCH (root:Task {title: 'T1'})-[:HAS_SUBTASK*]->(t:Task) RETURN t.title AS title",
        &db,
    );
    assert_eq!(names_sorted(&rows), vec!["T1a", "T1aa", "T1b"]);
    // Count of descendants of T1.
    let rows = run(
        "MATCH (root:Task {title: 'T1'})-[:HAS_SUBTASK*]->(t:Task) RETURN count(t) AS total",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 3);
}

#[test]
fn task_manager_direct_or_indirect_assignment_via_team() {
    let db = db();
    // Direct: T1 ASSIGNED_TO alice.
    // Indirect: T2 ASSIGNED_TO team-x; team-x MEMBER bob.
    run(
        "CREATE (:Task {title: 'T1'})-[:ASSIGNED_TO]->(:User {name: 'alice'})",
        &db,
    );
    run(
        "CREATE (:Task {title: 'T2'})-[:ASSIGNED_TO]->(:Team {name: 'team-x'})",
        &db,
    );
    run(
        "MATCH (t:Team {name: 'team-x'}) CREATE (t)-[:MEMBER]->(:User {name: 'bob'})",
        &db,
    );
    // `[*1..2]` covers both the direct (one hop) and team-mediated (two hops) cases.
    let rows = run(
        "MATCH (t:Task)-[*1..2]->(u:User) RETURN t.title AS task, u.name AS who ORDER BY task",
        &db,
    );
    assert_eq!(rows.len(), 2);
    match (&rows[0][0], &rows[0][1]) {
        (Value::String(t), Value::String(u)) => {
            assert_eq!(t, "T1");
            assert_eq!(u, "alice");
        }
        other => panic!("unexpected: {:?}", other),
    }
    match (&rows[1][0], &rows[1][1]) {
        (Value::String(t), Value::String(u)) => {
            assert_eq!(t, "T2");
            assert_eq!(u, "bob");
        }
        other => panic!("unexpected: {:?}", other),
    }
}

// ===== Scenario 4 — ERP =====================================================

#[test]
fn erp_bill_of_materials_indirect_components() {
    let db = db();
    // Product A made of B and C; B made of D; D is a raw material.
    for n in ["A", "B", "C", "D"] {
        run(&format!("CREATE (:Product {{sku: '{}'}})", n), &db);
    }
    for (parent, child) in [("A", "B"), ("A", "C"), ("B", "D")] {
        run(
            &format!(
                "MATCH (p:Product {{sku: '{}'}}), (c:Product {{sku: '{}'}}) CREATE (p)-[:CONTAINS]->(c)",
                parent, child
            ),
            &db,
        );
    }
    // All components (direct + transitive) of A.
    let rows = run(
        "MATCH (root:Product {sku: 'A'})-[:CONTAINS*]->(c:Product) RETURN c.sku AS sku",
        &db,
    );
    assert_eq!(names_sorted(&rows), vec!["B", "C", "D"]);
}

// ===== Scenario 5 — Bug tracker =============================================

#[test]
fn bug_tracker_blocking_chain_root_causes() {
    let db = db();
    // B3 blocks B2; B2 blocks B1; B4 unblocked.
    for id in ["B1", "B2", "B3", "B4"] {
        run(&format!("CREATE (:Bug {{id: '{}'}})", id), &db);
    }
    for (blocker, blocked) in [("B3", "B2"), ("B2", "B1")] {
        run(
            &format!(
                "MATCH (a:Bug {{id: '{}'}}), (b:Bug {{id: '{}'}}) CREATE (a)-[:BLOCKS]->(b)",
                blocker, blocked
            ),
            &db,
        );
    }
    // Root causes of B1 = all ancestors.
    let rows = run(
        "MATCH (root)-[:BLOCKS*]->(b:Bug {id: 'B1'}) RETURN root.id AS root",
        &db,
    );
    assert_eq!(names_sorted(&rows), vec!["B2", "B3"]);
}

// ===== Cross-scenario regressions ==========================================

#[test]
fn varlen_relationship_variable_collects_full_path() {
    let db = db();
    for n in ["A", "B", "C"] {
        run(&format!("CREATE (:N {{name: '{}'}})", n), &db);
    }
    for (a, b) in [("A", "B"), ("B", "C")] {
        run(
            &format!(
                "MATCH (a:N {{name: '{}'}}), (b:N {{name: '{}'}}) CREATE (a)-[:NEXT]->(b)",
                a, b
            ),
            &db,
        );
    }
    // Path A→B→C captured as a list of two relationships.
    let rows = run(
        "MATCH (a:N {name: 'A'})-[r:NEXT*2]->(b:N {name: 'C'}) RETURN r",
        &db,
    );
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        Value::List(items) => {
            assert_eq!(items.len(), 2);
            for item in items {
                assert!(matches!(item, Value::Relationship(_)));
            }
        }
        other => panic!("expected list, got {:?}", other),
    }
}

#[test]
fn varlen_with_optional_match_falls_to_null_on_no_path() {
    let db = db();
    run("CREATE (:N {name: 'isolated'})", &db);
    let rows = run(
        "MATCH (a:N {name: 'isolated'}) OPTIONAL MATCH (a)-[:R*1..5]->(b:N) RETURN a.name AS who, b.name AS reach",
        &db,
    );
    assert_eq!(rows.len(), 1);
    match (&rows[0][0], &rows[0][1]) {
        (Value::String(s), Value::Null) => assert_eq!(s, "isolated"),
        other => panic!("expected (isolated, NULL), got {:?}", other),
    }
}

#[test]
fn varlen_with_aggregation_count_reachable() {
    let db = db();
    // Star: hub → 4 leaves.
    run("CREATE (:N {name: 'hub'})", &db);
    for leaf in ["L1", "L2", "L3", "L4"] {
        run(&format!("CREATE (:N {{name: '{}'}})", leaf), &db);
        run(
            &format!(
                "MATCH (h:N {{name: 'hub'}}), (l:N {{name: '{}'}}) CREATE (h)-[:R]->(l)",
                leaf
            ),
            &db,
        );
    }
    let rows = run(
        "MATCH (h:N {name: 'hub'})-[:R*]->(t:N) RETURN count(t) AS reachable",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows, 0, 0), 4);
}

#[test]
fn varlen_trail_uniqueness_terminates_on_cycle() {
    let db = db();
    // Tight cycle A → B → A. Trail uniqueness means a *-hop walk
    // terminates: 1→B, 2→A, 3→none (would require reusing an edge).
    run("CREATE (:N {name: 'A'})", &db);
    run("CREATE (:N {name: 'B'})", &db);
    run(
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}) CREATE (a)-[:R]->(b)",
        &db,
    );
    run(
        "MATCH (a:N {name: 'B'}), (b:N {name: 'A'}) CREATE (a)-[:R]->(b)",
        &db,
    );
    let rows = run(
        "MATCH (a:N {name: 'A'})-[:R*]->(b:N) RETURN b.name AS name",
        &db,
    );
    // Reachable from A under trail uniqueness: B (1 hop), A (2 hops).
    assert_eq!(names_sorted(&rows), vec!["A", "B"]);
}
