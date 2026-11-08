//! Real-data native baseline (RFC `docs/rfc-native-core.md`, #307, Phase 2
//! measurement-first, #522).
//!
//! The synthetic `pagerank_bench` times an algorithm on a generated graph; this
//! bench runs the shipping engine — `NativeService`, the sole engine after epic
//! #444 — over a **GraphML copy of real data**, because synthetic shapes have
//! repeatedly misestimated real-world wins (the FTS posting-list rewrite
//! measured 2× off until validated on a live copy; see feedback
//! `validate_on_real_data_not_synthetic`). It is the measuring stick for RFC
//! Phase 2 (arena / CSR, #522): a naive CSR PageRank/WCC slice was already
//! built (#408–#416) and **removed as measured-slower** — so before any
//! re-attempt, this establishes the current native baseline the change must
//! actually beat.
//!
//! # Running
//!
//! Point `DREVO_BASELINE_GRAPHML` at a GraphML export of the graph to measure
//! (e.g. a `~/drevo_backups/*.graphml` snapshot or a fresh
//! `GET /export/graphml`), then:
//!
//! ```sh
//! DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/latest.graphml \
//!     cargo bench --bench real_data_baseline_bench
//! ```
//!
//! Without the variable the bench prints how to enable itself and exits
//! successfully, so CI (which has no real data) stays green while
//! `clippy --all-targets` still compile-checks it.
//!
//! # What is measured
//!
//! All workloads run on the native [`NativeService`](drevo::native_service::NativeService)
//! the shipping server uses. Two seams are timed:
//!
//! * The **Cypher executor** path a user actually hits — `count(*)`, a label
//!   scan, a 1-hop fan-out from the highest-degree hub, and `drevo.search.hybrid`
//!   (BM25 + cosine scan + RRF, `candidates: 20`) next to its BM25-only mode
//!   when the graph has nodes with an `embedding` property
//!   (`DREVO_BASELINE_HYBRID_QUERY` sets the query text), and
//!   `drevo.graph.expand` from 5 seeds over 2 hops with `perNodeLimit: 25`,
//!   and `drevo.rag.context` with its defaults (hybrid seeds, 2 hops, PPR,
//!   a 2000-token markdown context) on the same query and embedder.
//! * The **adjacency seam** underneath it — a 1-hop `neighbors`, a 2-hop
//!   frontier expansion, and a full-graph PageRank over an
//!   [`AdjacencyList`](drevo::algorithms::AdjacencyList) built from the live
//!   topology.
//!
//! The **2-hop frontier** row is the Phase-2 before/after anchor: a single
//! 1-hop lookup hides the per-edge iteration a CSR rewrite reshapes, whereas
//! expanding `hub -> N1 -> N2` exercises `|N1| + 1` adjacency probes and the
//! concatenation of their neighbour slices — exactly what an arena / CSR
//! representation would change. Workload parameters (the densest label, the
//! highest out-degree node) are derived from the data itself so the bench stays
//! meaningful as the graph evolves.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use criterion::Criterion;

use drevo::algorithms::{pagerank, AdjacencyList, PageRankConfig};
use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::engine::GraphEngine;
use drevo::model::Direction;
use drevo::native_service::NativeService;

/// Everything the workloads need, loaded once.
struct Loaded {
    svc: NativeService,
    /// Every node id, in scan order (PageRank vertex set).
    node_ids: Vec<u64>,
    /// The most frequent first label (label-scan workload), if any and if it
    /// is a simple identifier safe to splice into `MATCH (n:Label)`.
    top_label: Option<String>,
    /// The highest out-degree node (adjacency workloads), if the graph has
    /// any edges.
    hub_id: Option<u64>,
}

/// Parse + execute a read query on the native service, panicking on either
/// failure — a bench with a broken workload query has nothing to measure.
fn run(svc: &NativeService, source: &str) -> ExecResult {
    let q = parse(source).expect("workload query parses");
    svc.execute(&q, HashMap::new())
        .expect("workload query executes")
}

/// A label is safe to splice into `MATCH (n:Label)` only if it is a plain
/// identifier — real drevo kinds are (`note`, `person`, …); guarding keeps a
/// pathological export from turning into a parse error.
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

/// The label and stored `embedding` of the first node that has one, when the
/// label is a plain identifier.
#[cfg(feature = "http")]
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
    eprintln!("hybrid workload: label={label}, dimension={}", vector.len());
    Some((label, vector))
}

fn load() -> Result<Loaded, Box<dyn std::error::Error>> {
    let path = std::env::var("DREVO_BASELINE_GRAPHML")?;
    let xml = std::fs::read_to_string(&path)?;
    let svc = NativeService::in_memory();
    let report = svc.import_graphml(&xml)?;
    eprintln!(
        "loaded {}: {} nodes, {} edges",
        path, report.nodes_imported, report.edges_imported
    );

    // Node id set + densest first-label, derived from the data itself.
    let scan = run(&svc, "MATCH (n) RETURN id(n) AS id, labels(n) AS labels");
    let mut node_ids = Vec::with_capacity(scan.rows.len());
    let mut label_freq: HashMap<String, usize> = HashMap::new();
    for row in &scan.rows {
        if let Some(Value::Integer(id)) = row.first() {
            node_ids.push(*id as u64);
        }
        if let Some(Value::List(labels)) = row.get(1) {
            if let Some(Value::String(label)) = labels.first() {
                *label_freq.entry(label.clone()).or_default() += 1;
            }
        }
    }
    let top_label = label_freq
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(label, _)| label)
        .filter(|label| is_simple_ident(label));

    // Highest out-degree node — let the executor find it in one pass.
    let hub = run(
        &svc,
        "MATCH (a)-->(b) RETURN id(a) AS id, count(*) AS deg ORDER BY deg DESC LIMIT 1",
    );
    let hub_id = hub.rows.first().and_then(|row| match row.first() {
        Some(Value::Integer(id)) => Some(*id as u64),
        _ => None,
    });

    eprintln!(
        "workload params: nodes={}, top_label={top_label:?}, hub_id={hub_id:?}",
        node_ids.len()
    );
    Ok(Loaded {
        svc,
        node_ids,
        top_label,
        hub_id,
    })
}

/// Bench one pre-parsed Cypher query by row count (the executor materialises the
/// full result, so the length forces the work without allocating in the timing
/// loop beyond what the query itself does).
fn bench_cypher(c: &mut Criterion, svc: &NativeService, group: &str, source: &str) {
    let q = parse(source).expect("workload query parses");
    let mut g = c.benchmark_group(group.to_string());
    g.bench_function("native", |b| {
        b.iter(|| black_box(svc.execute(&q, HashMap::new()).expect("execute").rows.len()))
    });
    g.finish();
}

fn main() {
    let l = match load() {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "real_data_baseline_bench skipped ({e}). Set DREVO_BASELINE_GRAPHML to a \
                 GraphML export of the graph to measure, e.g.:\n  \
                 DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
                 cargo bench --bench real_data_baseline_bench"
            );
            return;
        }
    };

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20)
        .configure_from_args();

    // --- Cypher executor path (what a user hits) ---------------------------
    bench_cypher(
        &mut c,
        &l.svc,
        "count_all_nodes",
        "MATCH (n) RETURN count(*)",
    );
    if let Some(label) = &l.top_label {
        bench_cypher(
            &mut c,
            &l.svc,
            "label_scan_count",
            &format!("MATCH (n:{label}) RETURN count(*)"),
        );
    }
    if let Some(hub) = l.hub_id {
        bench_cypher(
            &mut c,
            &l.svc,
            "one_hop_from_hub_cypher",
            &format!("MATCH (a)-->(b) WHERE id(a) = {hub} RETURN count(b)"),
        );
    }

    // --- Hybrid search (issue #780): BM25 + cosine scan + RRF fusion -------
    // The query text is DREVO_BASELINE_HYBRID_QUERY (default `memory`). A stub
    // embedder answers with the stored `embedding` of the first node carrying
    // one, so the cosine scan runs at the graph's own dimension with no
    // upstream: the number is retrieval + fusion, not the embedder's latency.
    #[cfg(feature = "http")]
    if let Some((label, vector)) = first_embedding(&l.svc) {
        let text = std::env::var("DREVO_BASELINE_HYBRID_QUERY")
            .unwrap_or_else(|_| "memory".to_string())
            .replace(['\'', '\\'], " ");
        l.svc
            .set_embedder(std::sync::Arc::new(FixedEmbedder(vector)));
        for (group, mode) in [
            ("hybrid_search", "hybrid"),
            ("hybrid_search_fts_only", "fts"),
        ] {
            bench_cypher(
                &mut c,
                &l.svc,
                group,
                &format!(
                    "CALL drevo.search.hybrid('{text}', '{label}', 'embedding', 10, \
                     {{candidates: 20, mode: '{mode}'}}) YIELD node RETURN node"
                ),
            );
        }

        // --- Graph-RAG context with the defaults: hybrid seeds, 2 hops, PPR
        // over the expanded set, a 2000-token markdown context.
        bench_cypher(
            &mut c,
            &l.svc,
            "rag_context_default",
            &format!(
                "CALL drevo.rag.context('{text}', {{label: '{label}'}}) \
                 YIELD tokens RETURN tokens"
            ),
        );
    }

    // --- Graph expansion: 5 seeds x 2 hops, perNodeLimit 25 ---------------
    // The "widen the seeds" step of graph retrieval: the highest-degree hub
    // and the first nodes of the graph as seeds, as a search would hand them
    // over.
    let mut seeds: Vec<u64> = l.hub_id.into_iter().collect();
    for &id in &l.node_ids {
        if seeds.len() == 5 {
            break;
        }
        if !seeds.contains(&id) {
            seeds.push(id);
        }
    }
    if !seeds.is_empty() {
        let ids = seeds
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        bench_cypher(
            &mut c,
            &l.svc,
            "graph_expand_5_seeds_2_hops",
            &format!(
                "CALL drevo.graph.expand([{ids}], {{hops: 2, perNodeLimit: 25}}) \
                 YIELD node, distance RETURN id(node), distance"
            ),
        );
    }

    // --- Adjacency seam (isolates index-free adjacency from the executor) --
    // The id-only `neighbor_ids` fan-out reads straight from the adjacency
    // index without materialising node records — the layer an arena / CSR
    // representation reshapes. (`NativeService::neighbors` would fold in the
    // cost of cloning every `Node`, which is a different measurement.)
    if let Some(hub) = l.hub_id {
        let graph = l.svc.graph();
        let mut g = c.benchmark_group("one_hop_from_hub_seam");
        g.bench_function("native", |b| {
            b.iter(|| {
                black_box(
                    graph
                        .neighbor_ids(hub, Direction::Outgoing, None)
                        .expect("neighbor_ids")
                        .len(),
                )
            })
        });
        g.finish();

        // Two-hop frontier expansion — the arena / CSR before/after anchor. A
        // single 1-hop lookup a HashMap already serves well hides the per-edge
        // iteration a CSR rewrite changes; expanding hub -> N1 -> N2 exercises
        // `|N1| + 1` adjacency probes and the concatenation of their slices.
        let mut g = c.benchmark_group("two_hop_from_hub_seam");
        g.bench_function("native", |b| {
            b.iter(|| {
                let first = graph
                    .neighbor_ids(hub, Direction::Outgoing, None)
                    .expect("neighbor_ids");
                let mut total = 0usize;
                for &n in &first {
                    total += graph
                        .neighbor_ids(n, Direction::Outgoing, None)
                        .expect("neighbor_ids")
                        .len();
                }
                black_box(total)
            })
        });
        g.finish();
    }

    // --- Full-graph PageRank over the live topology ------------------------
    // Build the adjacency once from the real graph, then time only the power
    // iteration — the traversal-heavy algorithm whose per-edge cost an arena /
    // CSR layout targets.
    if !l.node_ids.is_empty() {
        let graph = l.svc.graph();
        let mut edges: Vec<(u64, u64, f32)> = Vec::new();
        for &id in &l.node_ids {
            for to in graph
                .neighbor_ids(id, Direction::Outgoing, None)
                .expect("neighbor_ids")
            {
                edges.push((id, to, 1.0));
            }
        }
        eprintln!(
            "pagerank input: {} nodes, {} edges",
            l.node_ids.len(),
            edges.len()
        );
        let adj = AdjacencyList::from_parts(l.node_ids.clone(), edges);
        let cfg = PageRankConfig::default();
        let mut g = c.benchmark_group("pagerank_full_graph");
        g.bench_function("native", |b| {
            b.iter(|| black_box(pagerank(&adj, &cfg).ranks.len()))
        });
        g.finish();
    }

    c.final_summary();
}
