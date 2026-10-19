//! What encryption at rest costs: durable writes with the log in the clear
//! and sealed with XChaCha20-Poly1305, and the time to reopen the store.
//!
//! ```sh
//! cargo bench --features encryption --bench encryption_bench
//! ```

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput};

use drevo::engine::GraphEngine;
use drevo::model::{NewNode, Properties};
use drevo::native::NativeGraph;
use drevo::wal_format::WalFormat;
use drevo_core::encryption::Key;

fn note(i: u64) -> NewNode {
    NewNode {
        kind: "Note".to_string(),
        title: format!("note {i}"),
        body: "a body of a few dozen bytes, like a short note".to_string(),
        body_html: String::new(),
        properties: Properties::from([
            ("n", serde_json::json!(i)),
            ("tag", serde_json::json!("x")),
        ]),
    }
}

fn open(dir: &std::path::Path, key: Option<&Key>) -> NativeGraph {
    let wal = dir.join("native.wal");
    match key {
        Some(key) => NativeGraph::open_durable_encrypted(&wal, WalFormat::Json, key.clone()),
        None => NativeGraph::open_durable(&wal),
    }
    .expect("open")
}

fn main() {
    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(5))
        .configure_from_args();
    let key = Key::from_bytes([7; 32]);

    // One fsynced record per write, and one record per 1,000-node batch.
    let mut g = c.benchmark_group("durable_write");
    for (name, k) in [("plain", None), ("encrypted", Some(&key))] {
        let dir = tempfile::tempdir().expect("tempdir");
        let graph = open(dir.path(), k);
        let mut i = 0;
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("{name}/single"), |b| {
            b.iter(|| {
                i += 1;
                black_box(graph.create_node(note(i)).expect("write"))
            })
        });
        g.throughput(Throughput::Elements(1_000));
        g.bench_function(format!("{name}/batch_1000"), |b| {
            b.iter_batched(
                || (0..1_000).map(note).collect::<Vec<_>>(),
                |batch| black_box(graph.create_nodes(batch).expect("batch")),
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();

    // Reopening a store of 20,000 nodes: replaying (and decrypting) the log.
    let mut g = c.benchmark_group("reopen_20k_nodes");
    g.sample_size(10);
    for (name, k) in [("plain", None), ("encrypted", Some(&key))] {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let graph = open(dir.path(), k);
            for chunk in (0..20_000u64).collect::<Vec<_>>().chunks(1_000) {
                graph
                    .create_nodes(chunk.iter().map(|&i| note(i)).collect())
                    .expect("seed");
            }
        }
        g.bench_function(name, |b| {
            b.iter(|| black_box(open(dir.path(), k).node_count()))
        });
    }
    g.finish();
    c.final_summary();
}
