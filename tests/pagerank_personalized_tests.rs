//! `CALL drevo.pagerank.personalized(sourceNodes[, config]) YIELD node,
//! score` — PageRank whose random walk teleports back to the source nodes —
//! and the config map `drevo.pagerank([config])` accepts (issue #781).
//!
//! The fixture has two components:
//!
//! ```text
//! a -LINKS-> b -LINKS-> c -LINKS-> a        x -LINKS-> y
//! a -LINKS-> d -CITES-> e -CITES-> b
//! ```
//!
//! Node ids follow creation order: a=1, b=2, c=3, d=4, e=5, x=6, y=7.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn try_run(db: &NativeService, source: &str) -> Result<ExecResult, ExecError> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    db.execute(&q, HashMap::new())
}

fn run(db: &NativeService, source: &str) -> ExecResult {
    try_run(db, source).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

/// The `InvalidProcedureCall` message `source` fails with.
fn invalid_call(db: &NativeService, source: &str) -> String {
    match try_run(db, source) {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{source}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

fn fixture() -> NativeService {
    let db = NativeService::in_memory();
    run(
        &db,
        "CREATE (a:Page {title: 'a'}), (b:Page {title: 'b'}), (c:Page {title: 'c'}), \
                (d:Page {title: 'd'}), (e:Page {title: 'e'}), \
                (x:Page {title: 'x'}), (y:Page {title: 'y'}), \
                (a)-[:LINKS]->(b), (b)-[:LINKS]->(c), (c)-[:LINKS]->(a), \
                (a)-[:LINKS]->(d), (d)-[:CITES]->(e), (e)-[:CITES]->(b), \
                (x)-[:LINKS]->(y)",
    );
    db
}

/// `(title, score)` rows of a query that returns `node.title, score`.
fn scores(db: &NativeService, source: &str) -> Vec<(String, f64)> {
    run(db, source)
        .rows
        .iter()
        .map(|row| {
            let title = match &row[0] {
                Value::String(s) => s.clone(),
                other => panic!("title: {other:?}"),
            };
            let score = match &row[1] {
                Value::Float(f) => *f,
                other => panic!("score is a Float, got {other:?}"),
            };
            (title, score)
        })
        .collect()
}

fn titles(rows: &[(String, f64)]) -> Vec<&str> {
    rows.iter().map(|(t, _)| t.as_str()).collect()
}

fn score_of(rows: &[(String, f64)], title: &str) -> f64 {
    rows.iter()
        .find(|(t, _)| t == title)
        .unwrap_or_else(|| panic!("no row for {title}: {rows:?}"))
        .1
}

/// Whether `title` has no row.
fn absent(rows: &[(String, f64)], title: &str) -> bool {
    rows.iter().all(|(t, _)| t != title)
}

fn total(rows: &[(String, f64)]) -> f64 {
    rows.iter().map(|(_, s)| s).sum()
}

/// A personalised call from the node titled `source`, returning every row.
fn ppr_from(db: &NativeService, source: &str, config: &str) -> Vec<(String, f64)> {
    scores(
        db,
        &format!(
            "MATCH (s:Page {{title: '{source}'}}) \
             CALL drevo.pagerank.personalized(s, {config}) YIELD node, score \
             RETURN node.title, score"
        ),
    )
}

// ── drevo.pagerank: unchanged output, config map ───────────────────────────

/// The first five rows of `drevo.pagerank()` on the fixture, captured on
/// `main` before the procedure took a config: `(title, id, score bits)`.
const PAGERANK_SNAPSHOT: [(&str, i64, u64); 5] = [
    ("b", 2, 0x3fcd_7ce2_0145_1daa), // 0.2303736215783208
    ("c", 3, 0x3fcc_9a2a_50e4_1b0c), // 0.22345475148923588
    ("a", 1, 0x3fcb_d971_634c_6b44), // 0.21757333134231704
    ("e", 5, 0x3fc0_9ae4_7ae8_d841), // 0.12972694398348675
    ("d", 4, 0x3fbe_bf41_0bdd_4423), // 0.12010580577522538
];

fn assert_matches_snapshot(db: &NativeService, call: &str) {
    let r = run(
        db,
        &format!("{call} YIELD node, score RETURN node.title, id(node), score"),
    );
    assert_eq!(r.rows.len(), 7, "{call}: one row per node");
    for (row, (title, id, bits)) in r.rows.iter().zip(PAGERANK_SNAPSHOT) {
        assert_eq!(row[0], Value::String(title.to_string()), "{call}: {row:?}");
        assert_eq!(row[1], Value::Integer(id), "{call}: {row:?}");
        match &row[2] {
            Value::Float(f) => assert_eq!(f.to_bits(), bits, "{call}: {title} scored {f}"),
            other => panic!("{call}: score is a Float, got {other:?}"),
        }
    }
}

#[test]
fn pagerank_without_config_output_is_unchanged() {
    assert_matches_snapshot(&fixture(), "CALL drevo.pagerank()");
}

#[test]
fn pagerank_with_empty_or_null_config_equals_no_config() {
    let db = fixture();
    assert_matches_snapshot(&db, "CALL drevo.pagerank({})");
    assert_matches_snapshot(&db, "CALL drevo.pagerank(null)");
}

#[test]
fn pagerank_config_damping_changes_scores_and_keeps_mass() {
    let db = fixture();
    let default = scores(
        &db,
        "CALL drevo.pagerank() YIELD node, score RETURN node.title, score",
    );
    let damped = scores(
        &db,
        "CALL drevo.pagerank({damping: 0.5, maxIterations: 200, tolerance: 1e-12}) \
         YIELD node, score RETURN node.title, score",
    );
    assert_eq!(damped.len(), 7);
    assert!((total(&damped) - 1.0).abs() < 1e-9, "{damped:?}");
    assert!(
        (score_of(&damped, "b") - score_of(&default, "b")).abs() > 1e-3,
        "damping 0.5 must move b: {damped:?} vs {default:?}"
    );
}

#[test]
fn pagerank_config_limit_min_score_and_rel_types() {
    let db = fixture();
    let all = scores(
        &db,
        "CALL drevo.pagerank() YIELD node, score RETURN node.title, score",
    );
    let top = scores(
        &db,
        "CALL drevo.pagerank({limit: 2}) YIELD node, score RETURN node.title, score",
    );
    assert_eq!(top, all[..2].to_vec());
    let above = scores(
        &db,
        "CALL drevo.pagerank({minScore: 0.2}) YIELD node, score RETURN node.title, score",
    );
    assert_eq!(titles(&above), ["b", "c", "a"]);
    // LINKS only: e has no incoming edge left, so it falls below d.
    let links = scores(
        &db,
        "CALL drevo.pagerank({relTypes: ['LINKS']}) YIELD node, score RETURN node.title, score",
    );
    assert!(score_of(&links, "e") < score_of(&links, "d"), "{links:?}");
}

#[test]
fn pagerank_config_rejects_source_weights_and_unknown_keys() {
    let db = fixture();
    let message = invalid_call(&db, "CALL drevo.pagerank({sourceWeights: [1.0]})");
    assert!(
        message.contains("sourceWeights") && message.contains("drevo.pagerank.personalized"),
        "{message}"
    );
    let message = invalid_call(&db, "CALL drevo.pagerank({dampening: 0.5})");
    assert!(
        message.contains("unknown config key `dampening`") && message.contains("damping"),
        "{message}"
    );
}

#[test]
fn pagerank_takes_zero_or_one_argument() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank({}, {})"),
        "expected 0 or 1 arguments, got 2"
    );
}

// ── drevo.pagerank.personalized: sources ───────────────────────────────────

#[test]
fn accepts_a_single_node_and_ranks_the_source_first() {
    let db = fixture();
    let rows = ppr_from(&db, "a", "{}");
    assert_eq!(rows[0].0, "a", "{rows:?}");
    assert!((total(&rows) - 1.0).abs() < 1e-6, "{rows:?}");
}

#[test]
fn accepts_a_node_id_and_a_mixed_list() {
    let db = fixture();
    let by_node = ppr_from(&db, "a", "{}");
    let by_id = scores(
        &db,
        "MATCH (s:Page {title: 'a'}) WITH id(s) AS sid \
         CALL drevo.pagerank.personalized(sid) YIELD node, score RETURN node.title, score",
    );
    assert_eq!(by_id, by_node);

    let nodes = scores(
        &db,
        "MATCH (a:Page {title: 'a'}), (d:Page {title: 'd'}) \
         CALL drevo.pagerank.personalized([a, d]) YIELD node, score RETURN node.title, score",
    );
    let mixed = scores(
        &db,
        "MATCH (a:Page {title: 'a'}), (d:Page {title: 'd'}) \
         CALL drevo.pagerank.personalized([a, id(d)]) YIELD node, score RETURN node.title, score",
    );
    assert_eq!(mixed, nodes);
    assert_ne!(nodes, by_node, "two sources differ from one");
}

#[test]
fn seeds_collected_from_a_query_work_like_the_graph_rag_example() {
    let db = fixture();
    let rows = scores(
        &db,
        "MATCH (n:Page) WHERE n.title IN ['a', 'd'] WITH collect(n) AS seeds \
         CALL drevo.pagerank.personalized(seeds, {limit: 3}) YIELD node, score \
         RETURN node.title, score",
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.windows(2).all(|w| w[0].1 >= w[1].1), "{rows:?}");
}

/// The graph-RAG example of docs/cypher-reference.md: hybrid-search hits
/// (BM25 only here — no embedder) seed the walk; no hit, no call.
#[test]
fn hybrid_search_hits_seed_the_walk() {
    let db = NativeService::in_memory();
    run(
        &db,
        "CREATE (c1:Chunk {title: 'anxious thoughts about work'}), \
                (c2:Chunk {title: 'sleep diary'}), (p:Person {title: 'manager'}), \
                (o:Topic {title: 'gardening'}), \
                (c1)-[:MENTIONS]->(p), (c2)-[:MENTIONS]->(p)",
    );
    let query = |text: &str| {
        format!(
            "CALL drevo.search.hybrid('{text}', 'Chunk', 'embedding', 5) YIELD node \
             WITH collect(node) AS seeds WHERE size(seeds) > 0 \
             CALL drevo.pagerank.personalized(seeds, {{limit: 50, direction: 'both'}}) \
             YIELD node, score RETURN node.title, score"
        )
    };
    let rows = scores(&db, &query("anxious"));
    // The hit's only neighbour collects its walk; the other chunk is reached
    // through it, one hop further.
    assert_eq!(
        titles(&rows)[..3],
        ["manager", "anxious thoughts about work", "sleep diary"]
    );
    assert!(absent(&rows, "gardening"), "{rows:?}");
    assert!(scores(&db, &query("zzzzqqq")).is_empty());
}

#[test]
fn unreachable_nodes_are_not_returned() {
    let db = fixture();
    // Sources in component {a, b, c, d, e}: x and y get no row.
    let rows = ppr_from(&db, "a", "{}");
    assert_eq!(titles(&rows).len(), 5, "{rows:?}");
    assert!(absent(&rows, "x") && absent(&rows, "y"), "{rows:?}");
    assert!(rows.iter().all(|(_, s)| *s > 0.0), "{rows:?}");
    assert!((total(&rows) - 1.0).abs() < 1e-6, "{rows:?}");
    // And the other way round.
    let other = ppr_from(&db, "x", "{}");
    assert_eq!(titles(&other), ["x", "y"], "{other:?}");
    assert!((total(&other) - 1.0).abs() < 1e-6, "{other:?}");
}

#[test]
fn rel_types_restrict_the_walk() {
    let db = fixture();
    let every = ppr_from(&db, "a", "{}");
    assert!(score_of(&every, "e") > 0.0, "{every:?}");
    let links = ppr_from(&db, "a", "{relTypes: ['LINKS']}");
    assert!(absent(&links, "e"), "d -CITES-> e is not walked: {links:?}");
    assert!(score_of(&links, "d") > 0.0, "{links:?}");
}

#[test]
fn direction_incoming_walks_edges_backwards() {
    let db = fixture();
    // Along the edges e's first step is b; against them it is d.
    let incoming = ppr_from(&db, "e", "{direction: 'incoming'}");
    assert!(
        score_of(&incoming, "d") > score_of(&incoming, "b"),
        "{incoming:?}"
    );
    let outgoing = ppr_from(&db, "e", "{direction: 'outgoing'}");
    assert!(
        score_of(&outgoing, "b") > score_of(&outgoing, "d"),
        "{outgoing:?}"
    );
    // Both: x and y are still another component.
    let both = ppr_from(&db, "e", "{direction: 'BOTH'}");
    assert!(absent(&both, "x") && absent(&both, "y"), "{both:?}");
    assert!((total(&both) - 1.0).abs() < 1e-6, "{both:?}");
}

#[test]
fn limit_and_min_score_apply_after_ranking() {
    let db = fixture();
    let all = ppr_from(&db, "a", "{}");
    let top = ppr_from(&db, "a", "{limit: 2}");
    assert_eq!(top, all[..2].to_vec());
    // A threshold between the second and third scores keeps two rows.
    let cut = (all[1].1 + all[2].1) / 2.0;
    let above = ppr_from(&db, "a", &format!("{{minScore: {cut}}}"));
    assert_eq!(above, all[..2].to_vec());
    let both = ppr_from(&db, "a", &format!("{{minScore: {cut}, limit: 1}}"));
    assert_eq!(both, all[..1].to_vec());
    assert!(ppr_from(&db, "a", "{limit: 0}").is_empty());
}

#[test]
fn order_is_best_first_with_ties_by_node_id() {
    let db = NativeService::in_memory();
    run(
        &db,
        "CREATE (s:Hub {title: 's'}), (l1:Leaf {title: 'l1'}), (l2:Leaf {title: 'l2'}), \
                (l3:Leaf {title: 'l3'}), (s)-[:R]->(l3), (s)-[:R]->(l1), (s)-[:R]->(l2)",
    );
    let q = "MATCH (s:Hub) CALL drevo.pagerank.personalized(s) YIELD node, score \
             RETURN node.title, score";
    let rows = scores(&db, q);
    assert_eq!(titles(&rows), ["s", "l1", "l2", "l3"], "{rows:?}");
    assert_eq!(rows[1].1, rows[2].1);
    assert_eq!(scores(&db, q), rows, "deterministic");
}

#[test]
fn source_weights_bias_the_teleport() {
    let db = fixture();
    let single = ppr_from(&db, "a", "{}");
    let weighted = scores(
        &db,
        "MATCH (a:Page {title: 'a'}), (x:Page {title: 'x'}) \
         CALL drevo.pagerank.personalized([a, x], {sourceWeights: [1, 0]}) \
         YIELD node, score RETURN node.title, score",
    );
    assert_eq!(weighted, single);
    let tilted = scores(
        &db,
        "MATCH (a:Page {title: 'a'}), (x:Page {title: 'x'}) \
         CALL drevo.pagerank.personalized([a, x], {sourceWeights: [3, 1]}) \
         YIELD node, score RETURN node.title, score",
    );
    assert!(
        score_of(&tilted, "a") > score_of(&tilted, "x"),
        "{tilted:?}"
    );
    assert!((total(&tilted) - 1.0).abs() < 1e-6, "{tilted:?}");
}

// ── drevo.pagerank.personalized: errors ────────────────────────────────────

#[test]
fn empty_sources_are_refused() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized([])"),
        "sourceNodes must name at least one node"
    );
}

#[test]
fn an_id_that_is_not_a_node_is_refused_by_id() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized([1, 999])"),
        "sourceNodes: 999 is not a node id"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(-3)"),
        "sourceNodes: -3 is not a node id"
    );
}

#[test]
fn sources_of_the_wrong_type_are_refused() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized('a')"),
        "sourceNodes must be a node, a node id or a list of them, got String"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized([1, 'a'])"),
        "sourceNodes must be a node, a node id or a list of them, got String"
    );
}

#[test]
fn source_weights_errors_are_refused() {
    let db = fixture();
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized([1, 2], {sourceWeights: [1.0]})"
        ),
        "sourceWeights has 1 entries but sourceNodes has 2"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized([1, 2], {sourceWeights: [0, 0.0]})"
        ),
        "sourceWeights must not all be zero"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized([1, 2], {sourceWeights: [1, -1]})"
        ),
        "sourceWeights must be finite numbers ≥ 0, got -1"
    );
}

#[test]
fn unknown_config_key_is_refused_with_the_accepted_keys() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(1, {top: 3})"),
        "unknown config key `top`; expected damping, maxIterations, tolerance, direction, \
         relTypes, sourceWeights, limit, minScore"
    );
}

#[test]
fn direction_outside_the_set_is_refused() {
    let db = fixture();
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized(1, {direction: 'up'})"
        ),
        "direction must be 'outgoing', 'incoming' or 'both', got 'up'"
    );
}

#[test]
fn out_of_range_numbers_are_refused() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(1, {damping: 1.0})"),
        "damping must be a number in the open interval (0, 1), got 1.0"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized(1, {maxIterations: 0})"
        ),
        "maxIterations must be an integer ≥ 1, got 0"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(1, {tolerance: -1})"),
        "tolerance must be a finite number ≥ 0, got -1"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(1, {limit: -1})"),
        "limit must be an integer ≥ 0, got -1"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized(1, {minScore: 'high'})"
        ),
        "minScore must be a finite number, got 'high'"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.pagerank.personalized(1, {relTypes: 'LINKS'})"
        ),
        "relTypes must be a list of relationship types, got String"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized(1, [])"),
        "the config must be a map, got List"
    );
}

#[test]
fn personalized_takes_one_or_two_arguments() {
    let db = fixture();
    assert_eq!(
        invalid_call(&db, "CALL drevo.pagerank.personalized()"),
        "expected 1 or 2 arguments, got 0"
    );
}

// ── Bolt ────────────────────────────────────────────────────────────────────

#[test]
fn bolt_round_trip_returns_a_node_and_a_float() {
    let db = Arc::new(fixture());
    let mut s = Session::new_durable(Arc::clone(&db));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: "MATCH (s:Page {title: 'a'}) \
                CALL drevo.pagerank.personalized(s, {limit: 2}) YIELD node, score \
                RETURN node, score"
            .to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    replies.extend(s.handle(ClientMessage::Pull {
        extra: BTreeMap::new(),
    }));
    let records: Vec<&Vec<Bolt>> = replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2, "{replies:?}");
    for record in records {
        assert!(
            matches!(record[0], Bolt::Structure { tag: 0x4E, .. }),
            "node is a Bolt Node: {record:?}"
        );
        assert!(matches!(record[1], Bolt::Float(_)), "{record:?}");
    }
}
