//! Point indexes for nearest-neighbour lookups by latitude and longitude.
//!
//! A [`NativePointIndex`] keeps the position of every node with a declared
//! label in a grid, follows the graph's change feed, and answers "the `k`
//! nodes nearest to this point" by great-circle distance, the same answer a
//! scan of every node gives.

use std::collections::HashMap;

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_point_index::{
    haversine_m, position, NativePointIndex, Nearest, PointIndexSpec, PointSpecError,
};
use serde_json::{json, Value};

fn node(g: &NativeGraph, kind: &str, props: Value) -> u64 {
    let Value::Object(map) = props else {
        panic!("props must be an object")
    };
    g.create_node(NewNode {
        kind: kind.into(),
        title: String::new(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::from(map.into_iter().collect::<HashMap<_, _>>()),
    })
    .expect("create node")
    .id
}

fn set_props(g: &NativeGraph, id: u64, props: Value) {
    let Value::Object(map) = props else {
        panic!("props must be an object")
    };
    g.update_node(
        id,
        NodePatch {
            properties: Some(Properties::from_iter(map)),
            ..NodePatch::default()
        },
    )
    .expect("update node");
}

fn stop_spec() -> PointIndexSpec {
    PointIndexSpec::new("Stop", "lat", "lon").expect("valid spec")
}

fn indexed(g: &NativeGraph) -> NativePointIndex {
    let mut idx = NativePointIndex::new(vec![stop_spec()]);
    idx.sync(g);
    idx
}

fn nearest(idx: &NativePointIndex, from: (f64, f64), k: usize, max: Option<f64>) -> Vec<u64> {
    nearest_m(idx, from, k, max)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

fn nearest_m(
    idx: &NativePointIndex,
    from: (f64, f64),
    k: usize,
    max: Option<f64>,
) -> Vec<(u64, f64)> {
    let query = Nearest { k, max_meters: max };
    idx.nearest("Stop", "lat", "lon", from, &query, &mut |_| true)
        .expect("a spec covers Stop(lat, lon)")
}

/// Sofia city centre.
const CENTRE: (f64, f64) = (42.6977, 23.3219);

/// Three stops in Sofia: Opera (~750 m from the centre), Lions Bridge
/// (~1.1 km) and the airport (~7 km).
fn sofia() -> (NativeGraph, u64, u64, u64) {
    let g = NativeGraph::new();
    let opera = node(&g, "Stop", json!({"lat": 42.6935, "lon": 23.3290}));
    let lions = node(&g, "Stop", json!({"lat": 42.7075, "lon": 23.3216}));
    let airport = node(&g, "Stop", json!({"lat": 42.6952, "lon": 23.4062}));
    (g, opera, lions, airport)
}

#[test]
fn a_spec_needs_a_label_and_two_different_properties() {
    assert_eq!(
        PointIndexSpec::new("", "lat", "lon"),
        Err(PointSpecError::EmptyLabel)
    );
    assert_eq!(
        PointIndexSpec::new("Stop", "", "lon"),
        Err(PointSpecError::EmptyProperty)
    );
    assert_eq!(
        PointIndexSpec::new("Stop", "lat", ""),
        Err(PointSpecError::EmptyProperty)
    );
    assert_eq!(
        PointIndexSpec::new("Stop", "lat", "lat"),
        Err(PointSpecError::SameProperty)
    );
    let spec = stop_spec();
    assert_eq!(
        (spec.label(), spec.lat(), spec.lon()),
        ("Stop", "lat", "lon")
    );
}

#[test]
fn a_position_is_two_numbers_in_range() {
    let g = NativeGraph::new();
    let read = |props: Value| {
        let id = node(&g, "Stop", props);
        let n = g.get_node(id).expect("get").expect("exists");
        position(&n, "lat", "lon")
    };
    assert_eq!(read(json!({"lat": 42.5, "lon": 23})), Some((42.5, 23.0)));
    assert_eq!(read(json!({"lat": -90, "lon": 180})), Some((-90.0, 180.0)));
    assert_eq!(read(json!({"lat": 90.5, "lon": 23})), None);
    assert_eq!(read(json!({"lat": 42.5, "lon": -180.5})), None);
    assert_eq!(read(json!({"lat": "42.5", "lon": 23})), None);
    assert_eq!(read(json!({"lat": 42.5})), None);
    assert_eq!(read(json!({"lat": null, "lon": 23})), None);
}

#[test]
fn nearest_first_with_distances_in_metres() {
    let (g, opera, lions, airport) = sofia();
    let idx = indexed(&g);
    let hits = nearest_m(&idx, CENTRE, 3, None);
    assert_eq!(
        hits.iter().map(|h| h.0).collect::<Vec<_>>(),
        vec![opera, lions, airport]
    );
    let opera_m = haversine_m(CENTRE, (42.6935, 23.3290));
    assert!((hits[0].1 - opera_m).abs() < 1e-6, "{hits:?}");
    assert!(hits[0].1 > 700.0 && hits[0].1 < 800.0, "{hits:?}");
    assert!(hits[2].1 > 6_500.0 && hits[2].1 < 7_500.0, "{hits:?}");
    assert_eq!(idx.len("Stop", "lat", "lon"), Some(3));
}

#[test]
fn k_limits_the_answer() {
    let (g, opera, lions, _) = sofia();
    let idx = indexed(&g);
    assert_eq!(nearest(&idx, CENTRE, 1, None), vec![opera]);
    assert_eq!(nearest(&idx, CENTRE, 2, None), vec![opera, lions]);
    assert_eq!(nearest(&idx, CENTRE, 0, None), Vec::<u64>::new());
    assert_eq!(nearest(&idx, CENTRE, 10, None).len(), 3);
}

#[test]
fn max_meters_bounds_the_answer() {
    let (g, opera, lions, _) = sofia();
    let idx = indexed(&g);
    assert_eq!(nearest(&idx, CENTRE, 10, Some(2_000.0)), vec![opera, lions]);
    assert_eq!(nearest(&idx, CENTRE, 10, Some(100.0)), Vec::<u64>::new());
}

#[test]
fn equal_distances_are_ordered_by_id() {
    let g = NativeGraph::new();
    let a = node(&g, "Stop", json!({"lat": 42.70, "lon": 23.33}));
    let b = node(&g, "Stop", json!({"lat": 42.70, "lon": 23.33}));
    let idx = indexed(&g);
    assert_eq!(nearest(&idx, CENTRE, 2, None), vec![a.min(b), a.max(b)]);
}

#[test]
fn only_nodes_with_the_label_and_a_position_are_indexed() {
    let g = NativeGraph::new();
    let _shop = node(&g, "Shop", json!({"lat": 42.6977, "lon": 23.3219}));
    let _nowhere = node(&g, "Stop", json!({"name": "no position"}));
    let _bad = node(&g, "Stop", json!({"lat": "north", "lon": 23.3219}));
    let platform = node(
        &g,
        "Place",
        json!({"lat": 42.6980, "lon": 23.3220, "_labels": ["Stop"]}),
    );
    let idx = indexed(&g);
    assert_eq!(nearest(&idx, CENTRE, 10, None), vec![platform]);
    assert_eq!(idx.len("Stop", "lat", "lon"), Some(1));
}

#[test]
fn accept_filters_nearest_first_and_once_per_node() {
    let (g, opera, lions, airport) = sofia();
    let idx = indexed(&g);
    let mut asked = Vec::new();
    let query = Nearest {
        k: 1,
        max_meters: None,
    };
    let hits = idx
        .nearest("Stop", "lat", "lon", CENTRE, &query, &mut |id| {
            asked.push(id);
            id != opera
        })
        .expect("covered");
    assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![lions]);
    assert_eq!(asked, vec![opera, lions]);

    let mut asked = Vec::new();
    let all = Nearest {
        k: 10,
        max_meters: None,
    };
    let hits = idx
        .nearest("Stop", "lat", "lon", CENTRE, &all, &mut |id| {
            asked.push(id);
            id == airport
        })
        .expect("covered");
    assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![airport]);
    assert_eq!(asked, vec![opera, lions, airport]);
}

#[test]
fn an_undeclared_spec_is_not_covered() {
    let (g, ..) = sofia();
    let idx = indexed(&g);
    let query = Nearest {
        k: 1,
        max_meters: None,
    };
    let mut any = |_| true;
    assert_eq!(
        idx.nearest("Shop", "lat", "lon", CENTRE, &query, &mut any),
        None
    );
    assert_eq!(
        idx.nearest("Stop", "y", "x", CENTRE, &query, &mut any),
        None
    );
    assert_eq!(idx.len("Stop", "y", "x"), None);
}

#[test]
fn the_index_follows_creates_moves_and_deletes() {
    let (g, opera, lions, airport) = sofia();
    let mut idx = indexed(&g);

    // A new stop right at the centre.
    let centre = node(&g, "Stop", json!({"lat": 42.6977, "lon": 23.3219}));
    idx.sync(&g);
    assert_eq!(nearest(&idx, CENTRE, 1, None), vec![centre]);

    // It moves out to the far side of the airport.
    set_props(&g, centre, json!({"lat": 42.70, "lon": 23.50}));
    idx.sync(&g);
    assert_eq!(
        nearest(&idx, CENTRE, 4, None),
        vec![opera, lions, airport, centre]
    );

    // Opera loses its position, Lions Bridge stops being a stop.
    set_props(&g, opera, json!({"name": "Opera"}));
    set_props(
        &g,
        lions,
        json!({"lat": 42.7075, "lon": 23.3216, "_labels": []}),
    );
    g.update_node(
        lions,
        NodePatch {
            kind: Some("Bridge".into()),
            ..NodePatch::default()
        },
    )
    .expect("relabel");
    idx.sync(&g);
    assert_eq!(nearest(&idx, CENTRE, 4, None), vec![airport, centre]);

    g.delete_node(airport).expect("delete");
    idx.sync(&g);
    assert_eq!(nearest(&idx, CENTRE, 4, None), vec![centre]);
    assert_eq!(idx.len("Stop", "lat", "lon"), Some(1));
}

#[test]
fn set_specs_builds_from_the_graph() {
    let (g, opera, ..) = sofia();
    let mut idx = NativePointIndex::new(Vec::new());
    idx.sync(&g);
    idx.set_specs(vec![stop_spec()], &g);
    assert_eq!(nearest(&idx, CENTRE, 1, None), vec![opera]);
    assert_eq!(idx.cursor(), g.change_head());
}

/// A small deterministic generator (xorshift64*), so the comparison below
/// sees the same points on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `lo..hi`.
    fn between(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The answer of a scan over every position, nearest first, ties by id.
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

#[test]
fn agrees_with_a_scan_in_a_city_and_around_the_globe() {
    let g = NativeGraph::new();
    let mut rng = Rng(0x05ee_d764);
    let mut points = Vec::new();
    let mut add = |rng: &mut Rng, lat: (f64, f64), lon: (f64, f64)| {
        let p = (rng.between(lat.0, lat.1), rng.between(lon.0, lon.1));
        let id = node(&g, "Stop", json!({"lat": p.0, "lon": p.1}));
        points.push((id, p));
    };
    for _ in 0..1_500 {
        add(&mut rng, (42.62, 42.76), (23.22, 23.42)); // Sofia
    }
    for _ in 0..200 {
        add(&mut rng, (89.0, 90.0), (-180.0, 180.0)); // near the North Pole
        add(&mut rng, (-90.0, -89.0), (-180.0, 180.0)); // near the South Pole
        add(&mut rng, (-10.0, 10.0), (179.0, 180.0)); // either side of
        add(&mut rng, (-10.0, 10.0), (-180.0, -179.0)); // the antimeridian
        add(&mut rng, (-90.0, 90.0), (-180.0, 180.0)); // anywhere
    }
    let idx = indexed(&g);

    let mut queries = Vec::new();
    for _ in 0..100 {
        queries.push((rng.between(42.60, 42.78), rng.between(23.20, 23.44)));
        queries.push((rng.between(88.0, 90.0), rng.between(-180.0, 180.0)));
        queries.push((rng.between(-90.0, -88.0), rng.between(-180.0, 180.0)));
        queries.push((rng.between(-5.0, 5.0), rng.between(179.5, 180.0)));
        queries.push((rng.between(-5.0, 5.0), rng.between(-180.0, -179.5)));
        queries.push((rng.between(-90.0, 90.0), rng.between(-180.0, 180.0)));
    }
    for from in queries {
        for (k, max) in [
            (1, None),
            (7, None),
            (50, Some(3_000.0)),
            (5, Some(250_000.0)),
        ] {
            assert_eq!(
                nearest(&idx, from, k, max),
                brute_force(&points, from, k, max),
                "from {from:?}, k {k}, max {max:?}"
            );
        }
    }
}
