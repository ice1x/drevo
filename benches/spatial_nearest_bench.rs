//! Snapping points to the nearest junction or stop: `drevo.spatial.nearest`
//! on a synthetic city the size of Sofia's road graph.
//!
//! The city is 243,049 junctions spread at random over Sofia's bounding box
//! (42.62–42.76 N, 23.22–23.42 E), the size of the Sofia OpenStreetMap road
//! graph; 85% are drivable (`r: 1`), the rest are footways. 4,800 tram and
//! bus stops are spread over the same box. Both labels have a point index on
//! `(lat, lon)`.
//!
//! Each query is a random point in the box:
//!
//! * `junction` — the nearest drivable junction (`k: 1, where: {r: 1}`), the
//!   start of a street route;
//! * `stops` — the five nearest stops within 500 m (`k: 5, maxMeters: 500`),
//!   the start of a transit route.
//!
//! The same junction lookups also run without a point index, over a few
//! queries, for comparison.
//!
//! # Running
//!
//! ```sh
//! cargo bench --bench spatial_nearest_bench
//! ```
//!
//! `DREVO_NEAREST_QUERIES` sets the number of queries (default 1000). The
//! bench prints the latency percentiles of the native call
//! ([`NativeService::nearest`](drevo::native_service::NativeService::nearest))
//! and of the same queries through `CALL drevo.spatial.nearest`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::model::{NewNode, Properties};
use drevo::native_service::{NativeService, NearestOptions};

const JUNCTIONS: u64 = 243_049;
const STOPS: u64 = 4_800;
/// The share of drivable junctions, in percent.
const DRIVABLE_PCT: u64 = 85;
const SOUTH: f64 = 42.62;
const NORTH: f64 = 42.76;
const WEST: f64 = 23.22;
const EAST: f64 = 23.42;
const BATCH: usize = 20_000;
/// Lookups without a point index scan every node; a few are enough.
const SCAN_QUERIES: usize = 20;

type Error = Box<dyn std::error::Error>;

/// A small deterministic generator (xorshift64*), so every run builds the
/// same city and asks the same questions.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Uniform in `lo..hi`.
    fn between(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn point(&mut self) -> (f64, f64) {
        (self.between(SOUTH, NORTH), self.between(WEST, EAST))
    }
}

fn run(svc: &NativeService, q: &str) -> Result<(), Error> {
    svc.execute(&parse(q)?, HashMap::new())?;
    Ok(())
}

/// Load `count` nodes of `kind` at random points, with `extra` properties.
fn load(
    svc: &NativeService,
    rng: &mut Rng,
    kind: &str,
    count: u64,
    mut extra: impl FnMut(&mut Rng, &mut Properties),
) -> Result<(), Error> {
    let mut left = count;
    while left > 0 {
        let n = left.min(BATCH as u64);
        left -= n;
        let batch: Vec<NewNode> = (0..n)
            .map(|_| {
                let (lat, lon) = rng.point();
                let mut properties = Properties::new();
                properties.insert("lat", Json::from(lat));
                properties.insert("lon", Json::from(lon));
                extra(rng, &mut properties);
                NewNode {
                    kind: kind.into(),
                    title: String::new(),
                    properties,
                    body: String::new(),
                    body_html: String::new(),
                }
            })
            .collect();
        svc.create_nodes(batch)?;
    }
    Ok(())
}

fn build(rng: &mut Rng, indexed: bool) -> Result<NativeService, Error> {
    let started = Instant::now();
    let svc = NativeService::in_memory();
    if indexed {
        run(
            &svc,
            "CREATE POINT INDEX FOR (j:Junction) ON (j.lat, j.lon)",
        )?;
        run(&svc, "CREATE POINT INDEX FOR (s:Stop) ON (s.lat, s.lon)")?;
    }
    load(&svc, rng, "Junction", JUNCTIONS, |rng, p| {
        p.insert("r", Json::from(u64::from(rng.below(100) < DRIVABLE_PCT)));
    })?;
    load(&svc, rng, "Stop", STOPS, |_, _| {})?;
    println!(
        "built in {:.1?}: {JUNCTIONS} junctions, {STOPS} stops, point indexes: {indexed}",
        started.elapsed()
    );
    Ok(svc)
}

fn percentiles(label: &str, mut times: Vec<Duration>) {
    times.sort();
    let at = |p: f64| times[((times.len() as f64 * p).ceil() as usize).clamp(1, times.len()) - 1];
    println!(
        "{label:<40} n={:<5} p50 {:>8.2?}  p95 {:>8.2?}  p99 {:>8.2?}  max {:>8.2?}",
        times.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        times[times.len() - 1],
    );
}

fn drivable() -> NearestOptions {
    NearestOptions {
        filter: vec![("r".to_string(), Json::from(1))],
        ..NearestOptions::default()
    }
}

fn stops() -> NearestOptions {
    NearestOptions {
        k: 5,
        max_meters: Some(500.0),
        ..NearestOptions::default()
    }
}

const CYPHER: [(&str, &str, &str); 2] = [
    (
        "junction",
        "Junction",
        "CALL drevo.spatial.nearest('Junction', {lat: $lat, lon: $lon}, {where: {r: 1}}) \
         YIELD node, meters RETURN id(node) AS id",
    ),
    (
        "stops",
        "Stop",
        "CALL drevo.spatial.nearest('Stop', {lat: $lat, lon: $lon}, {k: 5, maxMeters: 500}) \
         YIELD node, meters RETURN id(node) AS id",
    ),
];

fn main() -> Result<(), Error> {
    let n: usize = std::env::var("DREVO_NEAREST_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);
    let svc = build(&mut Rng(0x05ee_d764), true)?;
    let mut rng = Rng(0x0a5c_0764);
    let points: Vec<(f64, f64)> = (0..n).map(|_| rng.point()).collect();

    // The first lookup brings the point indexes up to the loaded nodes.
    let t = Instant::now();
    svc.nearest("Junction", points[0], &drivable())?;
    println!("first lookup (indexes the city) {:.2?}", t.elapsed());

    for (name, label, cypher) in CYPHER {
        let options = if label == "Junction" {
            drivable()
        } else {
            stops()
        };
        let mut native = Vec::with_capacity(n);
        let mut found = Vec::with_capacity(n);
        for &from in &points {
            let t = Instant::now();
            let near = svc.nearest(label, from, &options)?;
            native.push(t.elapsed());
            found.push(near.iter().map(|n| n.node as i64).collect::<Vec<_>>());
        }
        percentiles(&format!("native nearest ({name})"), native);
        let mut counts: Vec<usize> = found.iter().map(Vec::len).collect();
        counts.sort_unstable();
        println!(
            "  nodes returned: median {}, max {}",
            counts[counts.len() / 2],
            counts[counts.len() - 1]
        );

        let statement = parse(cypher)?;
        let mut times = Vec::with_capacity(n);
        for (&(lat, lon), want) in points.iter().zip(&found) {
            let params = HashMap::from([
                ("lat".to_string(), Value::Float(lat)),
                ("lon".to_string(), Value::Float(lon)),
            ]);
            let t = Instant::now();
            let rows = svc.execute(&statement, params)?.rows;
            times.push(t.elapsed());
            let got: Vec<i64> = rows
                .iter()
                .map(|r| match r[0] {
                    Value::Integer(id) => id,
                    _ => -1,
                })
                .collect();
            assert_eq!(&got, want, "Cypher and the native call disagree");
        }
        percentiles(&format!("CALL drevo.spatial.nearest ({name})"), times);
    }

    drop(svc);
    let scan = build(&mut Rng(0x05ee_d764), false)?;
    let mut times = Vec::with_capacity(SCAN_QUERIES);
    for &from in points.iter().take(SCAN_QUERIES) {
        let t = Instant::now();
        scan.nearest("Junction", from, &drivable())?;
        times.push(t.elapsed());
    }
    percentiles("native nearest (junction, no index)", times);
    Ok(())
}
