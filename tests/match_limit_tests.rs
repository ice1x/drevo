//! A `MATCH` whose rows the next clause cuts to a `LIMIT` stops once it has
//! enough: `MATCH (n) WHERE … WITH n LIMIT 2000` no longer projects every
//! candidate first. Results are the same as before; only `WITH` / `RETURN`
//! with a constant `LIMIT` and no `ORDER BY`, `DISTINCT`, aggregation or
//! `WITH … WHERE` let the `MATCH` stop early.
//!
//! The scenario is an importer's delete loop over a road graph, measured
//! with a counting allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

/// The system allocator, tracking the live heap and its peak.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
/// The heap test measures alone.
static MEASURING: Mutex<()> = Mutex::new(());

fn bump() {
    PEAK.fetch_max(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; the counters are
// only bookkeeping.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
            bump();
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded with the caller's pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded with the caller's arguments.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
            bump();
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn rows(svc: &NativeService, q: &str) -> Vec<Vec<Value>> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}`: {e}"))
        .rows
}

fn count(svc: &NativeService, q: &str) -> i64 {
    match rows(svc, q)[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("`{q}`: {other:?}"),
    }
}

/// `n` junctions, every one with `c: 1`, numbered by `i`.
fn junctions(n: usize) -> NativeService {
    let svc = NativeService::in_memory();
    rows(
        &svc,
        &format!(
            "UNWIND range(1, {n}) AS i CREATE (:Junction {{c: 1, i: i, lat: 42.6, lon: 23.3}})"
        ),
    );
    svc
}

#[test]
fn a_limited_match_still_returns_the_same_rows() {
    let svc = junctions(1_000);
    assert_eq!(
        count(
            &svc,
            "MATCH (n) WHERE n.c = 1 WITH n LIMIT 200 RETURN count(*)"
        ),
        200
    );
    assert!(rows(&svc, "MATCH (n:Junction) RETURN n LIMIT 0").is_empty());
    // SKIP + LIMIT: the same rows as when the MATCH is not cut (ORDER BY
    // id keeps the scan order and turns the early stop off).
    let cut = rows(&svc, "MATCH (n) WITH n SKIP 10 LIMIT 5 RETURN n.i");
    let full = rows(
        &svc,
        "MATCH (n) WITH n ORDER BY id(n) SKIP 10 LIMIT 5 RETURN n.i",
    );
    assert_eq!(cut, full);
    assert_eq!(cut.len(), 5);
    let cut = rows(&svc, "MATCH (n) WHERE n.i > 500 RETURN n.i LIMIT 3");
    assert_eq!(
        cut,
        vec![
            vec![Value::Integer(501)],
            vec![Value::Integer(502)],
            vec![Value::Integer(503)]
        ]
    );
}

#[test]
fn clauses_that_need_every_row_still_get_them() {
    let svc = junctions(1_000);
    // WITH … LIMIT … WHERE filters the limited rows.
    assert_eq!(
        count(
            &svc,
            "MATCH (n) WITH n LIMIT 10 WHERE n.i > 5 RETURN count(*)"
        ),
        5
    );
    // ORDER BY, DISTINCT and aggregation see every match.
    assert_eq!(
        rows(
            &svc,
            "MATCH (n) WITH n ORDER BY n.i DESC LIMIT 2 RETURN n.i"
        ),
        vec![vec![Value::Integer(1000)], vec![Value::Integer(999)]]
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (n) WITH DISTINCT n.c AS c LIMIT 5 RETURN count(*)"
        ),
        1
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (n) WITH count(n) AS total LIMIT 1 RETURN total"
        ),
        1_000
    );
    // A non-constant LIMIT is the projection's to judge.
    let err = svc
        .execute(
            &parse("MATCH (n) WITH n LIMIT -1 RETURN n").unwrap(),
            HashMap::new(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("non-negative"), "{err}");
}

#[test]
fn optional_paths_and_unwound_rows_are_cut_at_the_limit_too() {
    let svc = junctions(100);
    rows(
        &svc,
        "MATCH (a:Junction), (b:Junction) WHERE b.i = a.i + 1 CREATE (a)-[:ROAD]->(b)",
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (a)-[:ROAD]->(b) WITH a, b LIMIT 7 RETURN count(*)"
        ),
        7
    );
    assert_eq!(
        count(
            &svc,
            "MATCH p = (a)-[:ROAD]->(b) WITH p LIMIT 3 RETURN count(*)"
        ),
        3
    );
    assert_eq!(
        count(
            &svc,
            "UNWIND [1, 2, 3] AS x MATCH (n {i: x}) WITH n LIMIT 2 RETURN count(*)"
        ),
        2
    );
    assert_eq!(
        count(
            &svc,
            "UNWIND range(1, 50) AS x MATCH (n) WHERE n.i <= 3 WITH n LIMIT 10 RETURN count(*)"
        ),
        10
    );
    assert_eq!(
        count(&svc, "MATCH (n) WHERE n.i = 1 OPTIONAL MATCH (n)<-[:ROAD]-(m) WITH n, m LIMIT 5 RETURN count(*)"),
        1
    );
}

#[test]
fn the_delete_loop_deletes_one_batch_at_a_time() {
    let svc = junctions(250);
    let mut rounds = 0;
    loop {
        let n = count(
            &svc,
            "MATCH (n) WHERE n.c = 1 WITH n LIMIT 100 DETACH DELETE n RETURN count(*)",
        );
        rounds += 1;
        if n < 100 {
            assert_eq!(n, 50);
            break;
        }
    }
    assert_eq!(rounds, 3);
    assert_eq!(count(&svc, "MATCH (n) RETURN count(n)"), 0);
}

#[test]
fn a_limited_match_holds_memory_for_the_limit_not_the_graph() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    let svc = junctions(50_000);
    // The first statement builds the indexes; measure after it.
    rows(&svc, "MATCH (n) WHERE n.c = 1 RETURN count(n)");
    let measure = |q: &str| {
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let before = LIVE.load(Ordering::Relaxed);
        rows(&svc, q);
        (PEAK.load(Ordering::Relaxed) - before) as f64 / 1e6
    };
    let limited = measure("MATCH (n) WHERE n.c = 1 WITH n LIMIT 2000 RETURN count(*)");
    // The trailing WHERE keeps the WITH from cutting the MATCH (an ORDER BY
    // alone no longer does, #743): every row is projected.
    let everything = measure(
        "MATCH (n) WHERE n.c = 1 WITH n, 1 AS one LIMIT 2000 WHERE one = 1 RETURN count(*)",
    );
    println!("peak: limited {limited:.1} MB, every match {everything:.1} MB");
    // Projecting all 50,000 junctions takes tens of MB; 2,000 a few.
    assert!(
        limited * 5.0 < everything,
        "limited peak {limited:.1} MB vs every match {everything:.1} MB"
    );
    assert!(limited < 15.0, "limited peak {limited:.1} MB");
}

// ----- ORDER BY … LIMIT k: the MATCH keeps the k best rows (#743) ----------

fn ints(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter()
        .map(|r| match r[0] {
            Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn an_ordered_limit_returns_the_same_rows_as_sorting_everything() {
    let svc = junctions(1_000);
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WITH n ORDER BY n.i DESC LIMIT 5 RETURN n.i"
        )),
        vec![1000, 999, 998, 997, 996]
    );
    assert_eq!(
        ints(&rows(&svc, "MATCH (n) RETURN n.i ORDER BY n.i LIMIT 3")),
        vec![1, 2, 3]
    );
    // SKIP counts into what the MATCH must keep.
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WITH n ORDER BY n.i SKIP 2 LIMIT 3 RETURN n.i"
        )),
        vec![3, 4, 5]
    );
    // An alias of the projection as the sort key.
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WITH n.i * 2 AS twice ORDER BY twice DESC LIMIT 2 RETURN twice"
        )),
        vec![2000, 1998]
    );
    // Ties keep the scan order, as a stable sort does: every c is 1.
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WITH n ORDER BY n.c LIMIT 3 RETURN n.i"
        )),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WITH n ORDER BY n.c, n.i DESC LIMIT 3 RETURN n.i"
        )),
        vec![1000, 999, 998]
    );
    // A WHERE on the MATCH still filters before the cut.
    assert_eq!(
        ints(&rows(
            &svc,
            "MATCH (n) WHERE n.i % 2 = 0 WITH n ORDER BY n.i LIMIT 3 RETURN n.i"
        )),
        vec![2, 4, 6]
    );
    // Rows from every prior row compete for the same k.
    let r = rows(
        &svc,
        "UNWIND [10, 20] AS k MATCH (n) WITH k, n ORDER BY n.i DESC LIMIT 3 RETURN k, n.i",
    );
    assert_eq!(
        r,
        vec![
            vec![Value::Integer(10), Value::Integer(1000)],
            vec![Value::Integer(20), Value::Integer(1000)],
            vec![Value::Integer(10), Value::Integer(999)],
        ]
    );
    assert!(rows(&svc, "MATCH (n) WITH n ORDER BY n.i LIMIT 0 RETURN n.i").is_empty());
    // Null sorts last ascending (Neo4j); a missing property is null.
    rows(&svc, "CREATE (:Junction {c: 1})");
    let last = ints(&rows(
        &svc,
        "MATCH (n) WITH n ORDER BY n.i DESC LIMIT 2 RETURN coalesce(n.i, -1)",
    ));
    assert_eq!(last, vec![-1, 1000]);
}

#[test]
fn an_ordered_limit_holds_memory_for_the_limit_not_the_graph() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    let svc = junctions(50_000);
    rows(&svc, "MATCH (n) WHERE n.c = 1 RETURN count(n)");
    let measure = |q: &str| {
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
        let before = LIVE.load(Ordering::Relaxed);
        rows(&svc, q);
        (PEAK.load(Ordering::Relaxed) - before) as f64 / 1e6
    };
    let topk = measure("MATCH (n) WHERE n.c = 1 WITH n ORDER BY n.i DESC LIMIT 6 RETURN n.i");
    // The trailing WHERE keeps the WITH from cutting the MATCH: every row
    // is projected and sorted.
    let everything = measure(
        "MATCH (n) WHERE n.c = 1 WITH n, 1 AS one ORDER BY n.i DESC LIMIT 6 WHERE one = 1 RETURN n.i",
    );
    println!("peak: top-k {topk:.1} MB, every row {everything:.1} MB");
    assert!(
        topk * 5.0 < everything,
        "top-k peak {topk:.1} MB vs every row {everything:.1} MB"
    );
    assert!(topk < 15.0, "top-k peak {topk:.1} MB");
}
