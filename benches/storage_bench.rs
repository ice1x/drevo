use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use drevo::storage::{MemoryBackend, StorageBackend};
use std::hint::black_box;

const NUM_ENTRIES: usize = 100_000;
const VALUE_SIZE: usize = 256;

fn make_key(i: usize) -> Vec<u8> {
    format!("key:{i:08}").into_bytes()
}

fn make_prefixed_key(prefix: &str, i: usize) -> Vec<u8> {
    format!("{prefix}:{i:08}").into_bytes()
}

fn make_value(i: usize) -> Vec<u8> {
    let seed = (i % 256) as u8;
    vec![seed; VALUE_SIZE]
}

// ---------------------------------------------------------------------------
// Helpers to create pre-populated backends
// ---------------------------------------------------------------------------

fn populated_memory_backend() -> MemoryBackend {
    let backend = MemoryBackend::new();
    for i in 0..NUM_ENTRIES {
        backend.put(&make_key(i), &make_value(i)).unwrap();
    }
    backend
}

// ---------------------------------------------------------------------------
// Benchmarks: put
// ---------------------------------------------------------------------------

fn bench_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("put");
    group.sample_size(50);

    group.bench_function("MemoryBackend", |b| {
        b.iter_batched(
            MemoryBackend::new,
            |backend| {
                for i in 0..1_000 {
                    backend.put(&make_key(i), &make_value(i)).unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: get (random access from 100K entries)
// ---------------------------------------------------------------------------

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");

    let mem = populated_memory_backend();
    group.bench_function("MemoryBackend", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let key = make_key(i % NUM_ENTRIES);
            let val = mem.get(black_box(&key)).unwrap();
            assert!(val.is_some());
            i = i.wrapping_add(7919); // prime stride for pseudo-random access
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: scan_prefix
// ---------------------------------------------------------------------------

fn bench_scan_prefix(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_prefix");
    group.sample_size(30);

    // Populate the backend with structured keys: "grp:XX:NNNNNNNN"
    // 100 groups of 1000 keys each = 100K total
    let groups = 100;
    let per_group = 1_000;

    let mem = MemoryBackend::new();
    for g in 0..groups {
        let prefix = format!("grp:{g:04}");
        for j in 0..per_group {
            mem.put(&make_prefixed_key(&prefix, j), &make_value(j))
                .unwrap();
        }
    }

    for &scan_size in &[10, 100, 1_000] {
        let grp_idx = match scan_size {
            10 => "grp:0000",
            100 => "grp:0010",
            1_000 => "grp:0020",
            _ => unreachable!(),
        };
        let prefix = grp_idx.as_bytes();

        group.bench_with_input(
            BenchmarkId::new("MemoryBackend", scan_size),
            &scan_size,
            |b, _| {
                b.iter(|| {
                    let results = mem.scan_prefix(black_box(prefix)).unwrap();
                    assert!(!results.is_empty());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: bulk put 100K entries
// ---------------------------------------------------------------------------

fn bench_bulk_put_100k(c: &mut Criterion) {
    let mut group = c.benchmark_group("bulk_put_100k");
    group.sample_size(10);

    group.bench_function("MemoryBackend", |b| {
        b.iter(|| {
            let backend = MemoryBackend::new();
            for i in 0..NUM_ENTRIES {
                backend.put(&make_key(i), &make_value(i)).unwrap();
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_put,
    bench_get,
    bench_scan_prefix,
    bench_bulk_put_100k
);
criterion_main!(benches);
