//! Graph-RAG pipeline latency on a GraphML copy of real data: each stage of
//! `drevo.rag.context` timed alone, then the whole call.
//!
//! # Running
//!
//! Point `DREVO_BASELINE_GRAPHML` at a GraphML export of the graph to measure
//! (a `~/drevo_backups/*.graphml` snapshot or a fresh `GET /export/graphml`),
//! then:
//!
//! ```sh
//! DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/latest.graphml \
//!     cargo bench --bench rag_retrieve_bench
//! ```
//!
//! `DREVO_BASELINE_HYBRID_QUERY` sets the question (default `memory`).
//! Without `DREVO_BASELINE_GRAPHML` the bench prints how to enable itself and
//! exits successfully, so CI (which has no real data) only checks that it
//! compiles.
//!
//! # What is measured
//!
//! The embedder is a stub that answers every text at once with the stored
//! `embedding` of the first node carrying one, so the numbers are drevo's
//! work, not the embedding service's:
//!
//! * `hybrid_seeds_k8` — `drevo.search.hybrid` with `k: 8`, `candidates: 20`
//!   (stage 1, the seeds);
//! * `expand_2_hops` — `drevo.graph.expand` from those seeds with the
//!   pipeline's defaults: 2 hops, `perNodeLimit: 25`, `maxNodes: 200`, the
//!   bookkeeping labels left out (stage 2);
//! * `ppr_over_expanded` — Personalized PageRank over the expanded nodes and
//!   the relationships between them, teleporting to the seeds (stage 4);
//! * `serialize_2000_tokens` — writing the ranked subgraph as a 2000-token
//!   markdown context (stage 5);
//! * `rag_context_end_to_end` — `drevo.rag.context` with its defaults.
//!
//! With the `embeddings-proxy` feature and `DREVO_EMBEDDINGS_UPSTREAM` set,
//! `rag_context_real_embedder` times the same call embedding the question
//! through the configured service — reported apart, since it is mostly the
//! service's latency.

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::Criterion;

use drevo::cypher::executor::{ExecResult, NodeValue, Value};
use drevo::cypher::pagerank_config::{PageRankOptions, PageRankProcedure};
use drevo::cypher::parser::parse;
use drevo::model::Direction;
use drevo::native_service::NativeService;
use drevo::rag::serialize::{render, RenderOptions};
use drevo::rag::{personalized_scores, RankedNode, Subgraph};

/// Parse and run a read query, panicking on failure — a bench with a broken
/// workload has nothing to measure.
fn run(svc: &NativeService, source: &str) -> ExecResult {
    let q = parse(source).expect("workload query parses");
    svc.execute(&q, HashMap::new())
        .expect("workload query executes")
}

/// A label is safe to splice into a query only when it is a plain
/// identifier.
fn is_simple_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

/// An embedder that answers every text with the same vector.
#[cfg(feature = "http")]
struct FixedEmbedder(Vec<f32>);

#[cfg(feature = "http")]
impl drevo::embeddings::TextEmbedder for FixedEmbedder {
    fn embed_query(&self, _text: &str) -> Result<Vec<f32>, drevo::embeddings::EmbeddingsError> {
        Ok(self.0.clone())
    }
}

/// The label and stored `embedding` of the first node that has one.
fn first_embedding(svc: &NativeService) -> Option<(String, Vec<f32>)> {
    let found = run(
        svc,
        "MATCH (n) WHERE n.embedding IS NOT NULL \
         RETURN labels(n)[0] AS label, n.embedding AS embedding LIMIT 1",
    );
    let row = found.rows.first()?;
    let label = match row.first() {
        Some(Value::String(label)) if is_simple_ident(label) => label.clone(),
        _ => return None,
    };
    let Some(Value::List(items)) = row.get(1) else {
        return None;
    };
    let vector = items
        .iter()
        .map(|v| match v {
            Value::Float(f) => Some(*f as f32),
            Value::Integer(i) => Some(*i as f32),
            _ => None,
        })
        .collect::<Option<Vec<f32>>>()?;
    Some((label, vector))
}

/// The graph of `DREVO_BASELINE_GRAPHML` in a fresh in-memory service.
fn load() -> Result<NativeService, Box<dyn std::error::Error>> {
    let path = std::env::var("DREVO_BASELINE_GRAPHML")?;
    let xml = std::fs::read_to_string(&path)?;
    let svc = NativeService::in_memory();
    let report = svc.import_graphml(&xml)?;
    eprintln!(
        "loaded {path}: {} nodes, {} edges",
        report.nodes_imported, report.edges_imported
    );
    Ok(svc)
}

fn int(v: &Value) -> u64 {
    match v {
        Value::Integer(i) => u64::try_from(*i).expect("a node id"),
        other => panic!("expected an id, got {other:?}"),
    }
}

fn ids_literal(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Time one Cypher statement by its row count.
fn bench_cypher(c: &mut Criterion, svc: &NativeService, name: &str, source: &str) {
    let q = parse(source).expect("workload query parses");
    c.bench_function(name, |b| {
        b.iter(|| black_box(svc.execute(&q, HashMap::new()).expect("execute").rows.len()))
    });
}

/// The `drevo.rag.retrieve` defaults' subgraph for `text`: the ranked nodes
/// and the relationships between them.
fn retrieved(svc: &NativeService, text: &str, label: Option<&str>) -> Subgraph {
    let config = label.map_or_else(String::new, |l| format!(", {{label: '{l}'}}"));
    let rows = run(
        svc,
        &format!(
            "CALL drevo.rag.retrieve('{text}'{config}) \
             YIELD node, seedScore, distance, pprScore RETURN node, seedScore, distance, pprScore"
        ),
    )
    .rows;
    let nodes: Vec<RankedNode> = rows
        .into_iter()
        .map(|row| {
            let node: Arc<NodeValue> = match &row[0] {
                Value::Node(n) => Arc::clone(n),
                other => panic!("expected a node, got {other:?}"),
            };
            let float = |v: &Value| match v {
                Value::Float(f) => Some(*f),
                _ => None,
            };
            RankedNode {
                seed_score: float(&row[1]),
                distance: usize::try_from(int(&row[2])).expect("a distance"),
                ppr_score: float(&row[3]),
                ..RankedNode::seed(node, 0.0)
            }
        })
        .collect();
    let ids = ids_literal(&nodes.iter().map(|n| n.node.id).collect::<Vec<_>>());
    let rels = run(
        svc,
        &format!(
            "MATCH (a)-[r]->(b) WHERE id(a) IN [{ids}] AND id(b) IN [{ids}] \
             RETURN r ORDER BY id(r)"
        ),
    )
    .rows
    .into_iter()
    .map(|row| match &row[0] {
        Value::Relationship(r) => Arc::clone(r),
        other => panic!("expected a relationship, got {other:?}"),
    })
    .collect();
    Subgraph {
        nodes,
        rels,
        rel_weights: std::collections::BTreeMap::new(),
        truncated: false,
    }
}

const EXCLUDED: &str = "['Message', 'ReasoningTrace', 'ContextTrace', 'IngestJob', 'IngestItem']";

fn main() {
    let svc = match load() {
        Ok(svc) => svc,
        Err(e) => {
            eprintln!(
                "rag_retrieve_bench skipped ({e}). Set DREVO_BASELINE_GRAPHML to a GraphML \
                 export of the graph to measure, e.g.:\n  \
                 DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
                 cargo bench --bench rag_retrieve_bench"
            );
            return;
        }
    };
    let text = std::env::var("DREVO_BASELINE_HYBRID_QUERY")
        .unwrap_or_else(|_| "memory".to_string())
        .replace(['\'', '\\'], " ");

    let embedding = first_embedding(&svc);
    #[cfg(feature = "http")]
    if let Some((_, vector)) = &embedding {
        svc.set_embedder(Arc::new(FixedEmbedder(vector.clone())));
    }
    let label = embedding.as_ref().map(|(label, _)| label.clone());
    let label_arg = label
        .as_deref()
        .map_or_else(|| "null".to_string(), |l| format!("'{l}'"));
    eprintln!("question={text:?}, label={label:?}");

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20)
        .configure_from_args();

    // Stage 1: the seeds.
    let seeds_query = format!(
        "CALL drevo.search.hybrid('{text}', {label_arg}, 'embedding', 8, {{candidates: 20}}) \
         YIELD node, score RETURN id(node), score"
    );
    bench_cypher(&mut c, &svc, "hybrid_seeds_k8", &seeds_query);
    let seed_rows = run(&svc, &seeds_query).rows;
    let seeds: Vec<(u64, f64)> = seed_rows
        .iter()
        .map(|row| {
            let score = match &row[1] {
                Value::Float(f) => *f,
                _ => 0.0,
            };
            (int(&row[0]), score)
        })
        .collect();
    eprintln!("seeds: {}", seeds.len());
    if seeds.is_empty() {
        eprintln!("the question matched nothing; set DREVO_BASELINE_HYBRID_QUERY");
        c.final_summary();
        return;
    }

    // Stage 2: the expansion.
    let seed_ids: Vec<u64> = seeds.iter().map(|(id, _)| *id).collect();
    let expand_query = format!(
        "CALL drevo.graph.expand([{}], {{hops: 2, perNodeLimit: 25, maxNodes: 200, \
         excludeLabels: {EXCLUDED}}}) YIELD node RETURN id(node)",
        ids_literal(&seed_ids)
    );
    bench_cypher(&mut c, &svc, "expand_2_hops", &expand_query);
    let expanded: Vec<u64> = run(&svc, &expand_query)
        .rows
        .iter()
        .map(|row| int(&row[0]))
        .collect();

    // Stage 4: Personalized PageRank over the expanded nodes.
    let ids = ids_literal(&expanded);
    let edges: Vec<(u64, u64, f32)> = run(
        &svc,
        &format!(
            "MATCH (a)-[r]->(b) WHERE id(a) IN [{ids}] AND id(b) IN [{ids}] \
             RETURN id(a), id(b)"
        ),
    )
    .rows
    .iter()
    .map(|row| (int(&row[0]), int(&row[1]), 1.0))
    .collect();
    eprintln!(
        "expanded: {} nodes, {} relationships",
        expanded.len(),
        edges.len()
    );
    let options = PageRankOptions {
        direction: Direction::Both,
        ..PageRankOptions::defaults(PageRankProcedure::Personalized)
    };
    c.bench_function("ppr_over_expanded", |b| {
        b.iter(|| {
            black_box(
                personalized_scores(&expanded, &edges, &seeds, &options)
                    .expect("ppr")
                    .len(),
            )
        })
    });

    // Stage 5: the serialiser.
    let subgraph = retrieved(&svc, &text, label.as_deref());
    let render_options = RenderOptions::default();
    let sample = render(&subgraph, &render_options);
    eprintln!(
        "serialise: {} ranked nodes, {} relationships -> {} nodes, {} tokens",
        subgraph.nodes.len(),
        subgraph.rels.len(),
        sample.node_ids.len(),
        sample.tokens
    );
    c.bench_function("serialize_2000_tokens", |b| {
        b.iter(|| black_box(render(&subgraph, &render_options).tokens))
    });

    // The whole call.
    let label_config = label
        .as_deref()
        .map_or_else(String::new, |l| format!(", {{label: '{l}'}}"));
    let context_query =
        format!("CALL drevo.rag.context('{text}'{label_config}) YIELD tokens RETURN tokens");
    bench_cypher(&mut c, &svc, "rag_context_end_to_end", &context_query);

    real_embedder(&mut c, &context_query);
    c.final_summary();
}

/// `drevo.rag.context` embedding the question through the service of
/// `DREVO_EMBEDDINGS_UPSTREAM`, on a second copy of the graph.
#[cfg(feature = "embeddings-proxy")]
fn real_embedder(c: &mut Criterion, context_query: &str) {
    use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};
    let config = match EmbeddingsConfig::from_env(|k| std::env::var(k).ok()) {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!("rag_context_real_embedder skipped: DREVO_EMBEDDINGS_UPSTREAM is unset");
            return;
        }
        Err(e) => {
            eprintln!("rag_context_real_embedder skipped: {e}");
            return;
        }
    };
    let svc = load().expect("the graph loads again");
    let embedder = SyncEmbedder::from_config(config).expect("the embedder starts");
    svc.set_embedder(Arc::new(embedder));
    bench_cypher(c, &svc, "rag_context_real_embedder", context_query);
}

/// Without the `embeddings-proxy` feature there is no real embedder to time.
#[cfg(not(feature = "embeddings-proxy"))]
fn real_embedder(_c: &mut Criterion, _context_query: &str) {
    if std::env::var_os("DREVO_EMBEDDINGS_UPSTREAM").is_some() {
        eprintln!(
            "rag_context_real_embedder skipped: build with --features embeddings-proxy to \
             embed through DREVO_EMBEDDINGS_UPSTREAM"
        );
    }
}
