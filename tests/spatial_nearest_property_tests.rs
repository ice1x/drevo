//! [`NativeService::nearest`] against brute force on random points: with a
//! point index, without one, and after the points move, the answer is the
//! `k` nearest by haversine distance, ties by id.

use std::collections::HashMap;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_point_index::haversine_m;
use drevo::native_service::{NativeService, NearestOptions};
use proptest::prelude::*;

fn run(svc: &NativeService, q: &str, params: HashMap<String, Value>) {
    svc.execute(&parse(q).expect("parse"), params)
        .unwrap_or_else(|e| panic!("`{q}`: {e:?}"));
}

/// A point anywhere: half in a small city, the rest near the poles, at the
/// antimeridian or anywhere on the globe.
fn point() -> impl Strategy<Value = (f64, f64)> {
    prop_oneof![
        3 => (42.65..42.75f64, 23.25..23.40f64),
        1 => (88.0..=90.0f64, -180.0..=180.0f64),
        1 => (-90.0..=-88.0f64, -180.0..=180.0f64),
        1 => (-5.0..5.0f64, prop_oneof![179.0..=180.0f64, -180.0..-179.0f64]),
        1 => (-90.0..=90.0f64, -180.0..=180.0f64),
    ]
}

/// Ids of the `k` nearest of `points` (id, position) within `max`.
fn brute_force(
    points: &[(u64, (f64, f64))],
    from: (f64, f64),
    k: usize,
    max: Option<f64>,
) -> Vec<u64> {
    let mut all: Vec<(f64, u64)> = points
        .iter()
        .map(|(id, p)| (haversine_m(from, *p), *id))
        .filter(|(d, _)| max.is_none_or(|m| *d <= m))
        .collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    all.into_iter().take(k).map(|(_, id)| id).collect()
}

/// A service holding `points` as `:Stop {i, lat, lon}`, with a point index or
/// without one, and the ids of the stops in order.
fn stops(points: &[(f64, f64)], indexed: bool) -> (NativeService, Vec<u64>) {
    let svc = NativeService::in_memory();
    if indexed {
        run(
            &svc,
            "CREATE POINT INDEX FOR (s:Stop) ON (s.lat, s.lon)",
            HashMap::new(),
        );
    }
    let rows: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(i, (lat, lon))| {
            Value::Map(
                [
                    ("i".to_string(), Value::Integer(i as i64)),
                    ("lat".to_string(), Value::Float(*lat)),
                    ("lon".to_string(), Value::Float(*lon)),
                ]
                .into_iter()
                .collect(),
            )
        })
        .collect();
    run(
        &svc,
        "UNWIND $rows AS r CREATE (:Stop {i: r.i, lat: r.lat, lon: r.lon})",
        HashMap::from([("rows".to_string(), Value::List(rows))]),
    );
    let res = svc
        .execute(
            &parse("MATCH (s:Stop) RETURN s.i AS i, id(s) AS id ORDER BY i").expect("parse"),
            HashMap::new(),
        )
        .expect("ids");
    let ids = res
        .rows
        .iter()
        .map(|r| match r[1] {
            Value::Integer(id) => id as u64,
            ref other => panic!("{other:?}"),
        })
        .collect();
    (svc, ids)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nearest_agrees_with_brute_force(
        points in prop::collection::vec(point(), 0..120),
        moved in prop::collection::vec(point(), 0..20),
        queries in prop::collection::vec(point(), 1..6),
        k in 0usize..8,
        max in prop::option::of(0.0..500_000.0f64),
    ) {
        let (indexed, ids) = stops(&points, true);
        let (scanned, _) = stops(&points, false);
        let mut at: Vec<(u64, (f64, f64))> = ids.iter().copied().zip(points.iter().copied()).collect();
        // Move some of the stops: the index follows.
        for (i, p) in moved.iter().enumerate().take(at.len()) {
            let params = HashMap::from([
                ("i".to_string(), Value::Integer(i as i64)),
                ("lat".to_string(), Value::Float(p.0)),
                ("lon".to_string(), Value::Float(p.1)),
            ]);
            for svc in [&indexed, &scanned] {
                run(svc, "MATCH (s:Stop {i: $i}) SET s.lat = $lat, s.lon = $lon", params.clone());
            }
            at[i].1 = *p;
        }
        let options = NearestOptions { k, max_meters: max, ..NearestOptions::default() };
        for from in queries {
            let want = brute_force(&at, from, k, max);
            for svc in [&indexed, &scanned] {
                let got: Vec<u64> = svc
                    .nearest("Stop", from, &options)
                    .expect("nearest")
                    .iter()
                    .map(|n| n.node)
                    .collect();
                prop_assert_eq!(&got, &want, "from {:?}", from);
            }
        }
    }
}
