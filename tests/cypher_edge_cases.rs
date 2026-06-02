//! Cypher PoC edge-case stress catalogue — Phase 10.5 task `00126` (layer 2).
//!
//! This is the catalogue of "proof-of-concept killers": the corner cases a
//! Neo4j-literate reviewer reaches for in the first ten minutes of kicking a
//! new Cypher engine's tyres. Each item is an **explicit** test asserting the
//! *correct* observable behaviour — not merely "it didn't crash". When drevo
//! diverges from Neo4j the divergence is documented inline next to the
//! assertion so a future reader can tell intent from accident.
//!
//! Coverage (one `#[test]` per bullet from the README §Phase 10.5 `00126`
//! line, several bullets split into a positive + negative pair):
//!
//! * self-loops `(a)-[r]->(a)`
//! * variable-length cycles `(a)-[*1..10]->(a)` with BFS / trail dedup
//! * large `IN` lists (10 k elements — membership scan stays linear, not O(N²))
//! * fat properties (1 000-element JSON array round-trips intact)
//! * Unicode normalisation in `WHERE` equality (NFC vs NFD are *distinct*)
//! * full `NULL` semantics matrix (`1 + NULL`, `NULL = NULL`, `NULL IN [NULL]`,
//!   `[1, NULL, 3]`)
//! * string-coercion rejection (`1 + '2'` is a type error, never `"12"`)
//! * `DELETE` of a connected node without `DETACH` (must fail)
//! * `LIMIT 0` (empty result, no error)
//! * negative `SKIP` / `LIMIT` (runtime error)
//! * dense subgraph (a single node with 1 000+ neighbours)
//! * 5+ hop variable-length traversal on a chain of 100
//! * batch insert of 10 k nodes (linear, not quadratic)
//!
//! These run on the PR path (no `#[ignore]`): every cell is fast because the
//! "large" fixtures (10 k) are sized to finish in well under a second on the
//! shared self-hosted runner. The two scaling cells (`large_in_list_*`,
//! `batch_insert_*`, `dense_subgraph_*`) assert *correctness* on the large
//! input rather than a wall-clock bound — a timing assertion would be flaky
//! under nextest parallelism on the shared runner, whereas an O(N²) regression
//! on a 10 k input would blow the per-test budget and trip nextest's own
//! slow-test timeout, so the guard is structural rather than a brittle
//! `Instant::elapsed()` check.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

/// Parse + execute, panicking on either failure. Returns the result rows.
fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

/// Parse + execute, returning the full [`ExecResult`] (rows + stats).
fn run_full(source: &str, drevo: &Drevo) -> ExecResult {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute")
}

/// Parse (panicking on parse error) then execute, returning the raw
/// `Result` so a test can assert on the executor error variant.
fn try_exec(source: &str, drevo: &Drevo) -> Result<ExecResult, ExecError> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new())
}

fn scalar(rows: &[Vec<Value>]) -> &Value {
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one row, got {}",
        rows.len()
    );
    assert_eq!(rows[0].len(), 1, "expected exactly one column");
    &rows[0][0]
}

// ===== Self-loops ===========================================================

#[test]
fn self_loop_create_and_match() {
    let db = db();
    run("CREATE (:Person {name: 'Ouroboros'})", &db);
    // A node related to itself: (a)-[:KNOWS]->(a).
    let stats = run_full(
        "MATCH (a:Person {name: 'Ouroboros'}) CREATE (a)-[:KNOWS]->(a)",
        &db,
    )
    .stats;
    assert_eq!(stats.relationships_created, 1);
    assert_eq!(
        stats.nodes_created, 0,
        "self-loop must not create a new node"
    );

    // The loop is traversable: pattern (a)-[:KNOWS]->(b) binds a == b.
    let rows = run(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS an, b.name AS bn",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("Ouroboros".into()));
    assert_eq!(rows[0][1], Value::String("Ouroboros".into()));
}

#[test]
fn self_loop_varlen_terminates_with_trail_uniqueness() {
    let db = db();
    run("CREATE (:Node {name: 'solo'})", &db);
    run("MATCH (a:Node {name: 'solo'}) CREATE (a)-[:LOOP]->(a)", &db);
    // A single self-loop edge: with trail (relationship) uniqueness, BFS may
    // traverse the loop edge exactly once. `[*1..10]` must therefore return
    // the start node exactly once — and crucially terminate, not spin.
    let rows = run(
        "MATCH (a:Node {name: 'solo'})-[:LOOP*1..10]->(b:Node) RETURN b.name AS name",
        &db,
    );
    assert_eq!(rows.len(), 1, "trail uniqueness bounds the self-loop walk");
    assert_eq!(rows[0][0], Value::String("solo".into()));
}

// ===== Variable-length cycles ===============================================

#[test]
fn varlen_cycle_bfs_dedup_terminates() {
    let db = db();
    // Three-node cycle: a -> b -> c -> a.
    for n in ["a", "b", "c"] {
        run(&format!("CREATE (:N {{name: '{}'}})", n), &db);
    }
    for (x, y) in [("a", "b"), ("b", "c"), ("c", "a")] {
        run(
            &format!(
                "MATCH (x:N {{name: '{}'}}), (y:N {{name: '{}'}}) CREATE (x)-[:R]->(y)",
                x, y
            ),
            &db,
        );
    }
    // From `a`, `[*1..10]` over a 3-cycle. Trail uniqueness (no relationship
    // reused) caps the walk at three hops: a->b, a->b->c, a->b->c->a. The
    // reachable-node set is {b, c, a}; each surfaces once per distinct
    // shortest trail. The key assertion is *termination* — an engine without
    // dedup spins forever on a cycle.
    let mut names: Vec<String> = run(
        "MATCH (start:N {name: 'a'})-[:R*1..10]->(x:N) RETURN x.name AS name",
        &db,
    )
    .into_iter()
    .map(|r| match &r[0] {
        Value::String(s) => s.clone(),
        other => panic!("expected string, got {:?}", other),
    })
    .collect();
    names.sort();
    names.dedup();
    // Every node in the cycle is reachable from `a` (including `a` itself,
    // closing the loop a->b->c->a).
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn five_plus_hop_varlen_on_chain_of_100() {
    let db = db();
    // A 100-node chain n0 -> n1 -> ... -> n99.
    for i in 0..100 {
        run(&format!("CREATE (:Link {{idx: {}}})", i), &db);
    }
    for i in 0..99 {
        run(
            &format!(
                "MATCH (a:Link {{idx: {}}}), (b:Link {{idx: {}}}) CREATE (a)-[:NEXT]->(b)",
                i,
                i + 1
            ),
            &db,
        );
    }
    // Exactly the 5 nodes 5 hops downstream-or-closer from n0 are idx 1..=5.
    let rows = run(
        "MATCH (start:Link {idx: 0})-[:NEXT*1..5]->(x:Link) RETURN x.idx AS idx",
        &db,
    );
    let mut idxs: Vec<i64> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            other => panic!("expected integer, got {:?}", other),
        })
        .collect();
    idxs.sort();
    assert_eq!(idxs, vec![1, 2, 3, 4, 5]);

    // And a deeper reach (10 hops) lands exactly on idx 1..=10.
    let deep = run(
        "MATCH (start:Link {idx: 0})-[:NEXT*1..10]->(x:Link) RETURN x.idx AS idx",
        &db,
    );
    assert_eq!(deep.len(), 10, "10-hop reach over a chain yields 10 nodes");
}

// ===== Large IN lists =======================================================

#[test]
fn large_in_list_membership_found_and_missing() {
    let db = db();
    // Build a 10 000-element literal list [0, 1, ..., 9999].
    let list: String = (0..10_000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    // A member near the end of the list is found (full scan reaches it).
    let found = run(&format!("RETURN 9999 IN [{}] AS hit", list), &db);
    assert_eq!(*scalar(&found), Value::Bool(true));

    // A non-member returns FALSE (the list has no NULLs, so three-valued
    // logic does not kick in).
    let missing = run(&format!("RETURN 10000 IN [{}] AS hit", list), &db);
    assert_eq!(*scalar(&missing), Value::Bool(false));
}

#[test]
fn large_in_list_against_many_rows_is_not_quadratic() {
    let db = db();
    // 1 000 nodes, each with a distinct idx in [0, 1000).
    for i in 0..1_000 {
        run(&format!("CREATE (:Item {{idx: {}}})", i), &db);
    }
    // A 10 000-element IN list. Evaluating `idx IN [..10k..]` once per matched
    // row is 1_000 × 10_000 = 10^7 comparisons in the worst (linear-scan)
    // case — fine. An O(N²)-per-probe implementation would be 10^11 and trip
    // nextest's slow-test timeout long before this assertion is reached.
    let list: String = (0..10_000)
        .map(|i| (i * 2).to_string()) // even numbers only
        .collect::<Vec<_>>()
        .join(", ");
    let rows = run(
        &format!(
            "MATCH (n:Item) WHERE n.idx IN [{}] RETURN count(n) AS c",
            list
        ),
        &db,
    );
    // Items with an even idx in [0, 1000): 0, 2, ..., 998 → 500 of them.
    assert_eq!(*scalar(&rows), Value::Integer(500));
}

// ===== Fat properties =======================================================

#[test]
fn fat_property_thousand_element_array_round_trips() {
    let db = db();
    let arr: String = (0..1_000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    run(&format!("CREATE (:Doc {{tags: [{}]}})", arr), &db);

    let rows = run("MATCH (d:Doc) RETURN d.tags AS tags", &db);
    match scalar(&rows) {
        Value::List(items) => {
            assert_eq!(items.len(), 1_000, "all 1000 elements persist");
            assert_eq!(items[0], Value::Integer(0));
            assert_eq!(items[999], Value::Integer(999));
        }
        other => panic!("expected a List, got {:?}", other),
    }
}

// ===== Unicode normalisation ================================================

#[test]
fn unicode_nfc_vs_nfd_are_distinct_in_where_equality() {
    let db = db();
    // "café" composed (NFC): the é is a single code point U+00E9.
    let nfc = "caf\u{00E9}";
    // "café" decomposed (NFD): e + combining acute accent U+0301.
    let nfd = "cafe\u{0301}";
    assert_ne!(nfc, nfd, "the two byte sequences differ by construction");

    run(&format!("CREATE (:Word {{text: '{}'}})", nfc), &db);

    // drevo does NOT silently normalise — string equality is code-point exact,
    // matching Neo4j (Cypher string equality is not Unicode-normalising). The
    // NFD spelling must NOT match the stored NFC value.
    let nfd_match = run(
        &format!("MATCH (w:Word) WHERE w.text = '{}' RETURN w.text AS t", nfd),
        &db,
    );
    assert!(
        nfd_match.is_empty(),
        "NFD query must not match the stored NFC value (no implicit normalisation)"
    );

    // The exact NFC spelling matches.
    let nfc_match = run(
        &format!("MATCH (w:Word) WHERE w.text = '{}' RETURN w.text AS t", nfc),
        &db,
    );
    assert_eq!(nfc_match.len(), 1);
    assert_eq!(nfc_match[0][0], Value::String(nfc.to_string()));
}

// ===== NULL semantics matrix ================================================

#[test]
fn null_arithmetic_propagates() {
    let db = db();
    // 1 + NULL is NULL (not an error, not 1).
    assert_eq!(*scalar(&run("RETURN 1 + null AS x", &db)), Value::Null);
    assert_eq!(*scalar(&run("RETURN null + 1 AS x", &db)), Value::Null);
    assert_eq!(*scalar(&run("RETURN null * 5 AS x", &db)), Value::Null);
}

#[test]
fn null_equality_is_null_not_true() {
    let db = db();
    // NULL = NULL is NULL, not TRUE — the cardinal three-valued-logic trap.
    assert_eq!(*scalar(&run("RETURN null = null AS x", &db)), Value::Null);
    // NULL <> NULL is also NULL.
    assert_eq!(*scalar(&run("RETURN null <> null AS x", &db)), Value::Null);
    // 1 = NULL is NULL.
    assert_eq!(*scalar(&run("RETURN 1 = null AS x", &db)), Value::Null);
    // The IS NULL predicate is the *only* thing that yields a Boolean here.
    assert_eq!(
        *scalar(&run("RETURN null IS NULL AS x", &db)),
        Value::Bool(true)
    );
}

#[test]
fn null_in_list_with_only_null_is_null() {
    let db = db();
    // NULL IN [NULL] is NULL (you cannot prove membership of an unknown).
    assert_eq!(
        *scalar(&run("RETURN null IN [null] AS x", &db)),
        Value::Null
    );
}

#[test]
fn membership_with_null_in_list() {
    let db = db();
    // 1 IN [1, NULL, 3] is TRUE — a definite match short-circuits the unknown.
    assert_eq!(
        *scalar(&run("RETURN 1 IN [1, null, 3] AS x", &db)),
        Value::Bool(true)
    );
    // 2 IN [1, NULL, 3] is NULL — no match found, but a NULL in the list means
    // the answer is "unknown", not FALSE.
    assert_eq!(
        *scalar(&run("RETURN 2 IN [1, null, 3] AS x", &db)),
        Value::Null
    );
}

#[test]
fn list_literal_preserves_embedded_null() {
    let db = db();
    // [1, NULL, 3] is a three-element list with a NULL in the middle — the
    // NULL is preserved, not dropped or coalesced.
    let rows = run("RETURN [1, null, 3] AS xs", &db);
    match scalar(&rows) {
        Value::List(items) => {
            assert_eq!(items.len(), 3);
            assert_eq!(items[0], Value::Integer(1));
            assert_eq!(items[1], Value::Null);
            assert_eq!(items[2], Value::Integer(3));
        }
        other => panic!("expected a List, got {:?}", other),
    }
}

// ===== String coercion rejection ============================================

#[test]
fn string_coercion_in_arithmetic_is_rejected() {
    let db = db();
    // 1 + '2' must be a type error, NEVER the string "12" and NEVER 3. Cypher
    // does not coerce strings to numbers in arithmetic.
    let err = try_exec("RETURN 1 + '2' AS x", &db).expect_err("must error");
    assert!(
        matches!(err, ExecError::TypeMismatch { .. }),
        "expected TypeMismatch, got {:?}",
        err
    );

    // The symmetric case errors too.
    assert!(matches!(
        try_exec("RETURN '2' + 1 AS x", &db),
        // '2' + 1 — string LHS: this is string concatenation in Cypher only
        // when BOTH sides are strings; a String + Integer is a type error.
        Err(ExecError::TypeMismatch { .. })
    ));

    // Sanity: string + string IS concatenation (so the rejection above is
    // about *coercion*, not a blanket ban on `+` over strings).
    assert_eq!(
        *scalar(&run("RETURN 'a' + 'b' AS x", &db)),
        Value::String("ab".into())
    );
}

// ===== DELETE without DETACH ================================================

#[test]
fn delete_connected_node_without_detach_errors() {
    let db = db();
    run("CREATE (:Task {name: 'parent'})", &db);
    run("CREATE (:Task {name: 'child'})", &db);
    run(
        "MATCH (a:Task {name: 'parent'}), (b:Task {name: 'child'}) CREATE (a)-[:BLOCKS]->(b)",
        &db,
    );

    // Plain DELETE of a node that still has a relationship MUST fail.
    let err = try_exec("MATCH (a:Task {name: 'parent'}) DELETE a", &db)
        .expect_err("DELETE of connected node must error");
    assert!(
        matches!(err, ExecError::InvalidMutation(_)),
        "expected InvalidMutation, got {:?}",
        err
    );

    // The node and edge survive the failed DELETE.
    let survivors = run("MATCH (t:Task) RETURN count(t) AS c", &db);
    assert_eq!(*scalar(&survivors), Value::Integer(2));
}

#[test]
fn detach_delete_connected_node_succeeds() {
    let db = db();
    run("CREATE (:Task {name: 'parent'})", &db);
    run("CREATE (:Task {name: 'child'})", &db);
    run(
        "MATCH (a:Task {name: 'parent'}), (b:Task {name: 'child'}) CREATE (a)-[:BLOCKS]->(b)",
        &db,
    );

    // DETACH DELETE cascades the relationship and removes the node.
    let stats = run_full("MATCH (a:Task {name: 'parent'}) DETACH DELETE a", &db).stats;
    assert_eq!(stats.nodes_deleted, 1);
    assert_eq!(stats.relationships_deleted, 1);

    let survivors = run("MATCH (t:Task) RETURN t.name AS name", &db);
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0][0], Value::String("child".into()));
}

// ===== LIMIT / SKIP edge cases ==============================================

#[test]
fn limit_zero_yields_empty_result_no_error() {
    let db = db();
    for i in 0..5 {
        run(&format!("CREATE (:Row {{idx: {}}})", i), &db);
    }
    let rows = run("MATCH (r:Row) RETURN r.idx AS idx LIMIT 0", &db);
    assert!(
        rows.is_empty(),
        "LIMIT 0 returns no rows but is not an error"
    );
}

#[test]
fn negative_limit_is_a_runtime_error() {
    let db = db();
    run("CREATE (:Row {idx: 1})", &db);
    let err = try_exec("MATCH (r:Row) RETURN r.idx LIMIT -1", &db)
        .expect_err("negative LIMIT must error");
    assert!(
        matches!(err, ExecError::TypeMismatch { .. }),
        "expected TypeMismatch, got {:?}",
        err
    );
}

#[test]
fn negative_skip_is_a_runtime_error() {
    let db = db();
    run("CREATE (:Row {idx: 1})", &db);
    let err =
        try_exec("MATCH (r:Row) RETURN r.idx SKIP -1", &db).expect_err("negative SKIP must error");
    assert!(
        matches!(err, ExecError::TypeMismatch { .. }),
        "expected TypeMismatch, got {:?}",
        err
    );
}

// ===== Dense subgraph =======================================================

#[test]
fn dense_subgraph_single_node_with_many_neighbours() {
    let db = db();
    // One hub with 1 000 leaf neighbours: hub -[:HAS]-> leaf_i. Each leaf and
    // its edge are created in one statement that matches *only* the single-node
    // hub (no per-leaf `{idx: i}` rescan), so the fixture build stays linear
    // in the neighbour count instead of O(N²).
    run("CREATE (:Hub {name: 'hub'})", &db);
    for i in 0..1_000 {
        run(
            &format!(
                "MATCH (h:Hub {{name: 'hub'}}) CREATE (h)-[:HAS]->(:Leaf {{idx: {}}})",
                i
            ),
            &db,
        );
    }
    // A one-hop expansion from the hub must surface all 1 000 neighbours.
    let rows = run(
        "MATCH (h:Hub {name: 'hub'})-[:HAS]->(l:Leaf) RETURN count(l) AS c",
        &db,
    );
    assert_eq!(*scalar(&rows), Value::Integer(1_000));
}

// ===== Batch insert linearity ===============================================

#[test]
fn batch_insert_ten_thousand_nodes_is_linear() {
    let db = db();
    // 10 000 single-node CREATEs. A quadratic insert path (e.g. re-scanning
    // the whole store on every insert to maintain an index) would blow the
    // per-test budget and trip nextest's slow-test guard; a linear path
    // finishes well under a second. The assertion proves *all* nodes landed.
    for i in 0..10_000 {
        run(&format!("CREATE (:Bulk {{idx: {}}})", i), &db);
    }
    let rows = run("MATCH (n:Bulk) RETURN count(n) AS c", &db);
    assert_eq!(*scalar(&rows), Value::Integer(10_000));

    // Spot-check a node at each end actually persisted with its property.
    let first = run("MATCH (n:Bulk {idx: 0}) RETURN n.idx AS idx", &db);
    assert_eq!(*scalar(&first), Value::Integer(0));
    let last = run("MATCH (n:Bulk {idx: 9999}) RETURN n.idx AS idx", &db);
    assert_eq!(*scalar(&last), Value::Integer(9999));
}
