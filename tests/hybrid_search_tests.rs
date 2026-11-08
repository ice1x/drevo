//! `CALL drevo.search.hybrid(query, label, embeddingProperty, k[, config])
//! YIELD node, score, ftsRank, vectorRank, mode` and its relationship twin
//! `drevo.search.hybridRelationships` (issue #780): BM25 and a cosine scan
//! over a server-side embedding of the query, fused with Reciprocal Rank
//! Fusion inside the database.
//!
//! The embedder is an in-process stub: text mentioning a cat points at
//! `[1, 0, 0]`, text naming Zephyrine at `[0, 1, 0]`, anything else at
//! `[0, 0, 1]` — so which documents each leg finds is fixed by the fixture.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsError, TextEmbedder};
use drevo::native_service::NativeService;

struct StubEmbedder;

impl TextEmbedder for StubEmbedder {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        let text = text.to_lowercase();
        Ok(if text.contains("cat") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("zephyrine") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.0, 0.0, 1.0]
        })
    }
}

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

/// Five documents:
/// - `Zephyrine Quartermaine` — the exact name; its embedding points away
///   from every query, so only BM25 finds it;
/// - `notes on feline companions` — no shared text with "cat", but its
///   embedding is the cat direction, so only the vector leg finds it;
/// - `Zephyrine travel log` — both legs find it for "Zephyrine";
/// - two fillers, in the "other" direction.
fn library(db: &NativeService) {
    run(
        db,
        "CREATE (:Doc {title: 'Zephyrine Quartermaine', group_id: 'a', embedding: [0.0, 0.0, 1.0]}), \
                (:Doc {title: 'notes on feline companions', group_id: 'a', embedding: [1.0, 0.0, 0.0]}), \
                (:Doc {title: 'Zephyrine travel log', group_id: 'b', embedding: [0.0, 1.0, 0.0]}), \
                (:Doc {title: 'sourdough starter', group_id: 'b', embedding: [0.1, 0.9, 0.0]}), \
                (:Doc {title: 'tide tables', group_id: 'b', embedding: [0.2, 0.8, 0.0]})",
    );
}

fn with_embedder() -> NativeService {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    library(&db);
    db
}

fn without_embedder() -> NativeService {
    let db = NativeService::in_memory();
    library(&db);
    db
}

/// One returned row: `(title, score, ftsRank, vectorRank, mode)`.
#[derive(Debug, Clone, PartialEq)]
struct Hit {
    title: String,
    score: f64,
    fts_rank: Option<i64>,
    vector_rank: Option<i64>,
    mode: String,
}

fn rank(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Null => None,
        other => panic!("a rank is an Integer or null, got {other:?}"),
    }
}

/// Run a hybrid call (node or relationship) and decode its rows; the call
/// must `YIELD` its item as `item` and `RETURN item.title, score, ftsRank,
/// vectorRank, mode`.
fn hits(db: &NativeService, source: &str) -> Vec<Hit> {
    run(db, source)
        .rows
        .iter()
        .map(|row| Hit {
            title: match &row[0] {
                Value::String(s) => s.clone(),
                other => panic!("title: {other:?}"),
            },
            score: match &row[1] {
                Value::Float(f) => *f,
                other => panic!("score is a Float, got {other:?}"),
            },
            fts_rank: rank(&row[2]),
            vector_rank: rank(&row[3]),
            mode: match &row[4] {
                Value::String(s) => s.clone(),
                other => panic!("mode: {other:?}"),
            },
        })
        .collect()
}

fn node_hits(db: &NativeService, args: &str) -> Vec<Hit> {
    hits(
        db,
        &format!(
            "CALL drevo.search.hybrid({args}) YIELD node, score, ftsRank, vectorRank, mode \
             RETURN node.title, score, ftsRank, vectorRank, mode"
        ),
    )
}

fn titles(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.title.as_str()).collect()
}

// ── the two legs and their fusion ───────────────────────────────────────────

#[test]
fn exact_name_is_found_by_bm25_when_the_vector_leg_misses_it() {
    let db = with_embedder();
    let rows = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 2, {candidates: 2}");
    // In both legs: the travel log; BM25 only: the exact-name document.
    assert_eq!(
        titles(&rows),
        ["Zephyrine travel log", "Zephyrine Quartermaine"],
        "{rows:?}"
    );
    assert_eq!(rows[0].vector_rank, Some(1));
    assert!(rows[0].fts_rank.is_some());
    assert!(rows[1].fts_rank.is_some());
    assert_eq!(rows[1].vector_rank, None, "{rows:?}");
    assert!(rows[0].score > rows[1].score);
    assert!(rows.iter().all(|h| h.mode == "hybrid"));
}

#[test]
fn paraphrase_is_found_by_the_vector_leg_when_bm25_misses_it() {
    let db = with_embedder();
    let rows = node_hits(&db, "'a cat', 'Doc', 'embedding', 3");
    assert_eq!(rows[0].title, "notes on feline companions", "{rows:?}");
    assert_eq!(rows[0].fts_rank, None);
    assert_eq!(rows[0].vector_rank, Some(1));
    assert_eq!(rows.len(), 3, "k rows");
    // Scores descend.
    assert!(
        rows.windows(2).all(|w| w[0].score >= w[1].score),
        "{rows:?}"
    );
}

#[test]
fn default_score_is_reciprocal_rank_with_rrf_k_60() {
    let db = with_embedder();
    let rows = node_hits(&db, "'a cat', 'Doc', 'embedding', 1, {mode: 'vector'}");
    assert!((rows[0].score - 1.0 / 61.0).abs() < 1e-9, "{rows:?}");
    let rows = node_hits(
        &db,
        "'a cat', 'Doc', 'embedding', 1, {mode: 'vector', rrfK: 0}",
    );
    assert!((rows[0].score - 1.0).abs() < 1e-9, "{rows:?}");
}

#[test]
fn weights_scale_each_leg() {
    let db = with_embedder();
    // With the vector leg off, the BM25 order alone remains.
    let rows = node_hits(
        &db,
        "'Zephyrine', 'Doc', 'embedding', 5, {weights: {vector: 0}}",
    );
    assert!(rows.iter().all(|h| h.fts_rank.is_some()), "{rows:?}");
    assert_eq!(rows.len(), 2, "only BM25 hits score above zero: {rows:?}");
}

#[test]
fn modes_fts_and_vector_run_one_leg() {
    let db = with_embedder();
    let fts = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5, {mode: 'fts'}");
    assert_eq!(fts.len(), 2);
    assert!(fts
        .iter()
        .all(|h| h.mode == "fts" && h.vector_rank.is_none() && h.fts_rank.is_some()));
    let vector = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5, {mode: 'vector'}");
    assert_eq!(vector.len(), 5);
    assert!(vector
        .iter()
        .all(|h| h.mode == "vector" && h.fts_rank.is_none() && h.vector_rank.is_some()));
}

#[test]
fn a_null_label_runs_bm25_only() {
    let db = with_embedder();
    let rows = node_hits(&db, "'Zephyrine', null, 'embedding', 5");
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|h| h.mode == "fts" && h.vector_rank.is_none()));
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', null, 'embedding', 5, {mode: 'vector'}) \
         YIELD node RETURN node",
    );
    assert!(message.contains("label"), "{message}");
}

#[test]
fn fts_filter_restricts_the_bm25_leg_to_a_label() {
    let db = with_embedder();
    run(&db, "CREATE (:Memo {title: 'Zephyrine memo'})");
    let unfiltered = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 10, {mode: 'fts'}");
    assert!(
        titles(&unfiltered).contains(&"Zephyrine memo"),
        "{unfiltered:?}"
    );
    let filtered = node_hits(
        &db,
        "'Zephyrine', 'Doc', 'embedding', 10, {mode: 'fts', ftsFilter: 'Doc'}",
    );
    assert_eq!(filtered.len(), 2, "{filtered:?}");
    assert!(!titles(&filtered).contains(&"Zephyrine memo"));
}

#[test]
fn candidates_below_k_are_clamped_to_k() {
    let db = with_embedder();
    let rows = node_hits(
        &db,
        "'a cat', 'Doc', 'embedding', 4, {mode: 'vector', candidates: 1}",
    );
    assert_eq!(rows.len(), 4, "{rows:?}");
}

#[test]
fn k_applies_before_yield_where() {
    let db = with_embedder();
    // The single best hit is in group a; filtering on group b afterwards
    // leaves nothing, as for fts.search.
    let rows = run(
        &db,
        "CALL drevo.search.hybrid('a cat', 'Doc', 'embedding', 1) YIELD node, score \
         WHERE node.group_id = 'b' RETURN node.title",
    );
    assert!(rows.rows.is_empty(), "{:?}", rows.rows);
    let rows = run(
        &db,
        "CALL drevo.search.hybrid('a cat', 'Doc', 'embedding', 1) YIELD node, score \
         WHERE node.group_id = 'a' RETURN node.title",
    );
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn ranking_is_deterministic() {
    let db = with_embedder();
    let a = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5");
    let b = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5");
    assert_eq!(a, b);
}

// ── degradation without an embedder ────────────────────────────────────────

#[test]
fn auto_without_an_embedder_degrades_to_bm25() {
    let db = without_embedder();
    let rows = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|h| h.mode == "fts"));
    assert!(rows.iter().all(|h| h.vector_rank.is_none()));
    assert_eq!(
        rows.iter().map(|h| h.fts_rank).collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
}

#[test]
fn hybrid_without_an_embedder_is_an_engine_capability_error() {
    let db = without_embedder();
    for mode in ["hybrid", "vector"] {
        let source = format!(
            "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 5, {{mode: '{mode}'}}) \
             YIELD node RETURN node"
        );
        match try_run(&db, &source) {
            Err(ExecError::EngineCapability { feature }) => {
                assert!(feature.starts_with("semantic embedding"), "{feature}");
                assert!(feature.contains("drevo.semantic.status()"), "{feature}");
            }
            other => panic!("{mode}: expected EngineCapability, got {other:?}"),
        }
    }
}

// ── an embedder that fails ──────────────────────────────────────────────────

/// An installed embedder whose upstream always fails.
struct FailingEmbedder;

impl TextEmbedder for FailingEmbedder {
    fn embed_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        Err(EmbeddingsError::Upstream("connection refused".to_string()))
    }
}

fn with_failing_embedder() -> NativeService {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(FailingEmbedder)));
    library(&db);
    db
}

#[test]
fn auto_falls_back_to_bm25_when_the_embedder_fails() {
    let db = with_failing_embedder();
    let rows = node_hits(&db, "'Zephyrine', 'Doc', 'embedding', 5");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|h| h.mode == "fts"), "{rows:?}");
    assert!(rows.iter().all(|h| h.vector_rank.is_none()), "{rows:?}");
    assert!(rows.iter().all(|h| h.fts_rank.is_some()), "{rows:?}");
}

#[test]
fn hybrid_mode_surfaces_the_embedder_error() {
    let db = with_failing_embedder();
    for mode in ["hybrid", "vector"] {
        let message = invalid_call(
            &db,
            &format!(
                "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 5, {{mode: '{mode}'}}) \
                 YIELD node RETURN node"
            ),
        );
        assert!(message.contains("connection refused"), "{mode}: {message}");
    }
}

// ── argument validation ─────────────────────────────────────────────────────

#[test]
fn arity_four_and_five_are_accepted() {
    let db = without_embedder();
    run(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1) YIELD node RETURN node",
    );
    run(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1, {}) YIELD node RETURN node",
    );
    run(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1, null) YIELD node RETURN node",
    );
}

#[test]
fn arity_three_and_six_are_rejected() {
    let db = without_embedder();
    for args in [
        "'Zephyrine', 'Doc', 'embedding'",
        "'Zephyrine', 'Doc', 'embedding', 1, {}, 1",
    ] {
        for proc in ["drevo.search.hybrid", "drevo.search.hybridRelationships"] {
            let message = invalid_call(
                &db,
                &format!("CALL {proc}({args}) YIELD score RETURN score"),
            );
            assert!(message.contains("expected 4 or 5 arguments"), "{message}");
        }
    }
}

#[test]
fn k_zero_is_rejected() {
    let db = without_embedder();
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 0) YIELD node RETURN node",
    );
    assert!(
        message.contains("k must be a positive integer"),
        "{message}"
    );
}

#[test]
fn negative_rrf_k_is_rejected() {
    let db = without_embedder();
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1, {rrfK: -1}) \
         YIELD node RETURN node",
    );
    assert!(message.contains("rrfK"), "{message}");
}

#[test]
fn unknown_config_key_is_rejected_listing_the_accepted_keys() {
    let db = without_embedder();
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1, {topK: 3}) \
         YIELD node RETURN node",
    );
    assert!(message.contains("`topK`"), "{message}");
    assert!(
        message.contains("candidates, rrfK, mode, ftsFilter, weights"),
        "{message}"
    );
}

#[test]
fn non_map_config_is_rejected() {
    let db = without_embedder();
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1, 'fts') \
         YIELD node RETURN node",
    );
    assert!(message.contains("config must be a map"), "{message}");
}

#[test]
fn non_string_query_and_label_are_rejected() {
    let db = without_embedder();
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid(42, 'Doc', 'embedding', 1) YIELD node RETURN node",
    );
    assert!(message.contains("query must be a string"), "{message}");
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 7, 'embedding', 1) YIELD node RETURN node",
    );
    assert!(
        message.contains("label must be a string or null"),
        "{message}"
    );
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 7, 1) YIELD node RETURN node",
    );
    assert!(
        message.contains("embeddingProperty must be a string"),
        "{message}"
    );
}

#[test]
fn yield_columns_are_node_score_ftsrank_vectorrank_mode() {
    let db = without_embedder();
    let result = run(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1) \
         YIELD node, score, ftsRank, vectorRank, mode",
    );
    assert_eq!(
        result.columns,
        ["node", "score", "ftsRank", "vectorRank", "mode"]
    );
    let message = invalid_call(
        &db,
        "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 1) YIELD rank RETURN rank",
    );
    assert!(
        message.contains("does not yield a column `rank`"),
        "{message}"
    );
}

// ── relationships ───────────────────────────────────────────────────────────

/// Three `MENTIONS` edges: one whose title names Zephyrine but which has no
/// embedding (BM25 only), one about felines in the cat direction (vector
/// only), one Zephyrine postcard in the Zephyrine direction (both); and a
/// `NOTES` edge naming Zephyrine.
fn edges(db: &NativeService) {
    run(
        db,
        "CREATE (a:Person {title: 'ann'}), (b:Person {title: 'bo'}) \
         CREATE (a)-[:MENTIONS {title: 'Zephyrine ledger'}]->(b) \
         CREATE (a)-[:MENTIONS {title: 'feline gossip', emb: [1.0, 0.0, 0.0]}]->(b) \
         CREATE (a)-[:MENTIONS {title: 'Zephyrine postcard', emb: [0.0, 1.0, 0.0]}]->(b) \
         CREATE (a)-[:NOTES {title: 'Zephyrine aside'}]->(b)",
    );
}

fn rel_hits(db: &NativeService, args: &str) -> Vec<Hit> {
    hits(
        db,
        &format!(
            "CALL drevo.search.hybridRelationships({args}) \
             YIELD rel, score, ftsRank, vectorRank, mode \
             RETURN rel.title, score, ftsRank, vectorRank, mode"
        ),
    )
}

#[test]
fn relationships_bm25_leg() {
    let db = NativeService::in_memory();
    edges(&db);
    let rows = rel_hits(&db, "'Zephyrine', 'MENTIONS', 'emb', 5, {mode: 'fts'}");
    assert_eq!(rows.len(), 3, "graph-wide BM25: {rows:?}");
    assert!(rows
        .iter()
        .all(|h| h.mode == "fts" && h.vector_rank.is_none()));
    let rows = rel_hits(
        &db,
        "'Zephyrine', 'MENTIONS', 'emb', 5, {mode: 'fts', ftsFilter: 'MENTIONS'}",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[test]
fn relationships_vector_leg() {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    edges(&db);
    let rows = rel_hits(&db, "'a cat', 'MENTIONS', 'emb', 1, {mode: 'vector'}");
    assert_eq!(titles(&rows), ["feline gossip"]);
    assert_eq!(rows[0].vector_rank, Some(1));
    assert_eq!(rows[0].fts_rank, None);
}

#[test]
fn relationships_fused() {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    edges(&db);
    let rows = rel_hits(
        &db,
        "'Zephyrine', 'MENTIONS', 'emb', 2, {candidates: 1, ftsFilter: 'MENTIONS'}",
    );
    assert_eq!(rows[0].title, "Zephyrine postcard", "{rows:?}");
    assert_eq!(rows[0].vector_rank, Some(1));
    assert!(rows[0].fts_rank.is_some());
    assert!(rows.iter().all(|h| h.mode == "hybrid"));
}

#[test]
fn relationships_auto_without_embedder_degrades_to_bm25() {
    let db = NativeService::in_memory();
    edges(&db);
    let rows = rel_hits(&db, "'Zephyrine', 'MENTIONS', 'emb', 5");
    assert!(!rows.is_empty());
    assert!(rows
        .iter()
        .all(|h| h.mode == "fts" && h.vector_rank.is_none()));
}

// ── Bolt ────────────────────────────────────────────────────────────────────

#[test]
fn bolt_round_trip_returns_integer_and_null_ranks() {
    let db = Arc::new(with_embedder());
    let mut s = Session::new_durable(Arc::clone(&db));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: "CALL drevo.search.hybrid('Zephyrine', 'Doc', 'embedding', 2, {candidates: 2}) \
                YIELD node, score, ftsRank, vectorRank, mode \
                RETURN node.title, score, ftsRank, vectorRank, mode"
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
    // Both legs: integer ranks.
    assert!(matches!(records[0][1], Bolt::Float(_)), "{:?}", records[0]);
    assert!(
        matches!(records[0][2], Bolt::Integer(_)),
        "{:?}",
        records[0]
    );
    assert!(
        matches!(records[0][3], Bolt::Integer(1)),
        "{:?}",
        records[0]
    );
    assert_eq!(records[0][4], Bolt::String("hybrid".to_string()));
    // BM25 only: a null vector rank.
    assert!(
        matches!(records[1][2], Bolt::Integer(_)),
        "{:?}",
        records[1]
    );
    assert_eq!(records[1][3], Bolt::Null, "{:?}", records[1]);
}
