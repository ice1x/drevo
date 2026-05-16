//! Benchmark: FTS on 10K nodes.
//!
//! Task 00020 — measures full-text search performance on a realistic dataset.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use drevo::db::Drevo;
use drevo::model::NewNode;
use std::hint::black_box;

const NUM_NODES: usize = 10_000;

/// Create a node with varied content for realistic FTS benchmarking.
fn make_fts_node(i: usize) -> NewNode {
    let kinds = ["note", "task", "bug", "thought", "chapter"];
    let kind = kinds[i % kinds.len()];

    NewNode {
        kind: kind.to_string(),
        title: format!("{kind} number {i}: exploring topic {}", i % 50),
        body: format!(
            "This is the body of {kind} {i}. It discusses various aspects of \
             software development including testing, debugging, performance \
             optimization, and architecture design. Keywords: rust database \
             graph embedded search indexing trigram. Topic group {}. \
             Additional context for variety: project management workflow \
             automation continuous integration deployment monitoring alerting \
             logging observability metrics dashboards incidents postmortems.",
            i % 50
        ),
        body_html: String::new(),
        properties: Default::default(),
    }
}

/// Build a fully populated in-memory DB with 10K nodes for FTS benchmarking.
fn populated_fts_db() -> Drevo {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..NUM_NODES {
        db.create_node(make_fts_node(i)).unwrap();
    }
    db
}

// ---------------------------------------------------------------------------
// Benchmark: search_fts with various query lengths
// ---------------------------------------------------------------------------

fn bench_search_fts(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts_search_10k");

    let db = populated_fts_db();

    let queries: Vec<(&str, &str)> = vec![
        ("single_word", "rust"),
        ("two_words", "database graph"),
        ("three_words", "testing debugging performance"),
        ("phrase", "software development architecture"),
        ("rare_term", "postmortems"),
        ("common_term", "body"),
        ("kind_filter", "thought exploring"),
        ("mixed", "rust embedded search indexing"),
    ];

    for (label, query) in &queries {
        group.bench_with_input(BenchmarkId::new(*label, "limit_10"), query, |b, q| {
            b.iter(|| {
                let results = db.search_fts(black_box(q), 10).unwrap();
                black_box(&results);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: search_fts with varying limits
// ---------------------------------------------------------------------------

fn bench_search_fts_limits(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts_search_limits");

    let db = populated_fts_db();
    let query = "software development testing";

    for limit in [1, 10, 50, 100, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(limit), &limit, |b, &lim| {
            b.iter(|| {
                let results = db.search_fts(black_box(query), lim).unwrap();
                black_box(&results);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: FTS index maintenance (create_node with indexing)
// ---------------------------------------------------------------------------

fn bench_fts_index_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts_index_insert");
    group.sample_size(20);

    group.bench_function("1000_nodes", |b| {
        b.iter(|| {
            let db = Drevo::open_in_memory().unwrap();
            for i in 0..1_000 {
                db.create_node(make_fts_node(i)).unwrap();
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: list_recent on 10K nodes
// ---------------------------------------------------------------------------

fn bench_list_recent(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts_list_recent");

    let db = populated_fts_db();

    for limit in [10, 50, 100, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(limit), &limit, |b, &lim| {
            b.iter(|| {
                let results = db.list_recent(black_box(lim)).unwrap();
                black_box(&results);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_search_fts,
    bench_search_fts_limits,
    bench_fts_index_insert,
    bench_list_recent
);
criterion_main!(benches);
