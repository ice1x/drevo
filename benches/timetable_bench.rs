//! Public-transport routing on a timetable the size of Sofia's:
//! `drevo.path.timetable` over a synthetic city.
//!
//! The city is a square grid of stops 250 m apart. Bus lines run straight
//! across it with turns; each line has one stop pattern and trips all day.
//! The sizes follow the Sofia GTFS feed on a weekday:
//!
//! | | synthetic | Sofia |
//! |---|---|---|
//! | stops | 4,900 | 4,799 |
//! | trip patterns | 540 | 538 |
//! | pattern hops (`HOP` relationships) | 9,720 | 10,430 |
//! | trips | 16,740 | 16,478 |
//! | stop times | 318,060 | 381,928 |
//! | footpaths (`WALK` relationships) | ~137,000 | 143,026 |
//!
//! Each query picks an origin and a destination at least 3 km apart and a
//! departure between 06:00 and 20:00. The 20 stops nearest to each, within a
//! 15-minute walk, are the starts and ends, at their walking time. The
//! options are those a journey planner uses: one service day, a 60 s
//! transfer, at most 4 transfers and 2 hours.
//!
//! # Running
//!
//! ```sh
//! cargo bench --bench timetable_bench
//! ```
//!
//! `DREVO_TIMETABLE_QUERIES` sets the number of queries (default 1000). The
//! bench prints the latency percentiles of the native call
//! ([`NativeService::timetable`](drevo::native_service::NativeService::timetable))
//! and of the same queries through `CALL drevo.path.timetable`, how many
//! queries found a journey, and the resident set size.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::native_service::NativeService;
use drevo::timetable::{Endpoint, ServiceDay, TimetableOptions, WalkFields};

const SIDE: i64 = 70;
const SPACING_M: f64 = 250.0;
const PATTERNS: usize = 540;
const STOPS_PER_PATTERN: usize = 19;
const TRIPS_PER_PATTERN: usize = 31;
/// Footpaths join stops at most this many grid cells apart.
const WALK_CELLS: i64 = 3;
const WALK_MPS: f64 = 1.4;
const BUS_MPS: f64 = 7.0;
const DWELL_S: i64 = 20;
const ACCESS_STOPS: usize = 20;
const ACCESS_MAX_S: f64 = 900.0;
const MIN_TRIP_M: f64 = 3_000.0;
const BATCH: usize = 20_000;

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

    fn below(&mut self, n: i64) -> i64 {
        (self.next() % n as u64) as i64
    }
}

fn props(entries: &[(&str, Json)]) -> Properties {
    let mut p = Properties::new();
    for (k, v) in entries {
        p.insert(*k, v.clone());
    }
    p
}

fn cell_m(a: (i64, i64), b: (i64, i64)) -> f64 {
    let (dx, dy) = ((a.0 - b.0) as f64, (a.1 - b.1) as f64);
    (dx * dx + dy * dy).sqrt() * SPACING_M
}

struct City {
    svc: NativeService,
    /// Node id of the stop in each cell, row by row.
    stops: Vec<u64>,
}

impl City {
    fn stop(&self, cell: (i64, i64)) -> u64 {
        self.stops[(cell.1 * SIDE + cell.0) as usize]
    }
}

fn build(rng: &mut Rng) -> Result<City, Error> {
    let started = Instant::now();
    let svc = NativeService::in_memory();
    let batch: Vec<NewNode> = (0..SIDE * SIDE)
        .map(|i| NewNode {
            kind: "Stop".into(),
            title: format!("stop {i}"),
            properties: props(&[("stop", Json::from(i))]),
            body: String::new(),
            body_html: String::new(),
        })
        .collect();
    let stops: Vec<u64> = svc.create_nodes(batch)?.iter().map(|n| n.id).collect();
    let city = City { svc, stops };

    let mut edges = Vec::new();
    let mut stop_times = 0;
    for _ in 0..PATTERNS {
        // A line: straight runs with an occasional turn, bouncing off the edges.
        let mut cell = (rng.below(SIDE), rng.below(SIDE));
        let dirs = [(1, 0), (-1, 0), (0, 1), (0, -1)];
        let mut dir = dirs[rng.below(4) as usize];
        let mut cells = vec![cell];
        while cells.len() < STOPS_PER_PATTERN {
            if rng.below(5) == 0 {
                dir = dirs[rng.below(4) as usize];
            }
            let step = 1 + rng.below(2);
            let next = (cell.0 + dir.0 * step, cell.1 + dir.1 * step);
            if next.0 < 0 || next.0 >= SIDE || next.1 < 0 || next.1 >= SIDE {
                dir = (-dir.0, -dir.1);
                continue;
            }
            cell = next;
            cells.push(cell);
        }
        // Trips every ~35 minutes from about 05:00.
        let first = 18_000 + rng.below(1_800);
        let headway = (82_800 - first) / TRIPS_PER_PATTERN as i64;
        let starts: Vec<i64> = (0..TRIPS_PER_PATTERN as i64)
            .map(|t| first + t * headway + rng.below(120))
            .collect();
        let trip_base = edges.len() as i64 * 100;
        let mut offset = 0;
        for pair in cells.windows(2) {
            let run = (cell_m(pair[0], pair[1]) / BUS_MPS).round() as i64;
            let dep: Vec<i64> = starts.iter().map(|s| s + offset).collect();
            let arr: Vec<i64> = dep.iter().map(|d| d + run).collect();
            let trip: Vec<i64> = (0..starts.len() as i64).map(|t| trip_base + t).collect();
            stop_times += starts.len();
            edges.push(NewEdge {
                from_id: city.stop(pair[0]),
                to_id: city.stop(pair[1]),
                kind: "HOP".into(),
                weight: 1.0,
                properties: props(&[
                    ("dep", Json::from(dep)),
                    ("arr", Json::from(arr)),
                    ("trip", Json::from(trip)),
                    ("svc", Json::from(vec![1; starts.len()])),
                ]),
            });
            offset += run + DWELL_S;
        }
        stop_times += starts.len();
    }
    let hops = edges.len();
    for y in 0..SIDE {
        for x in 0..SIDE {
            for dy in -WALK_CELLS..=WALK_CELLS {
                for dx in -WALK_CELLS..=WALK_CELLS {
                    let (nx, ny) = (x + dx, y + dy);
                    if (dx, dy) == (0, 0)
                        || dx * dx + dy * dy > WALK_CELLS * WALK_CELLS
                        || nx < 0
                        || ny < 0
                        || nx >= SIDE
                        || ny >= SIDE
                    {
                        continue;
                    }
                    let s = (cell_m((x, y), (nx, ny)) / WALK_MPS).round() as i64;
                    edges.push(NewEdge {
                        from_id: city.stop((x, y)),
                        to_id: city.stop((nx, ny)),
                        kind: "WALK".into(),
                        weight: 1.0,
                        properties: props(&[("s", Json::from(s))]),
                    });
                }
            }
        }
    }
    let walks = edges.len() - hops;
    let graph = city.svc.graph();
    while !edges.is_empty() {
        let rest = edges.split_off(edges.len().saturating_sub(BATCH));
        graph.create_edges(rest)?;
    }
    city.svc.execute(
        &parse("CREATE INDEX FOR (s:Stop) ON (s.stop)")?,
        HashMap::new(),
    )?;
    println!(
        "built in {:.1?}: {} stops, {PATTERNS} patterns, {hops} hops, {} trips, \
         {stop_times} stop times, {walks} footpaths",
        started.elapsed(),
        SIDE * SIDE,
        PATTERNS * TRIPS_PER_PATTERN,
    );
    Ok(city)
}

/// A journey planner's question: walk-in and walk-out stops with seconds,
/// and when to leave.
struct Query {
    starts: Vec<(i64, Endpoint)>,
    ends: Vec<(i64, Endpoint)>,
    depart_at: i64,
}

/// The stops nearest to `cell` within the access walk, at their walking time.
fn nearby(city: &City, cell: (i64, i64)) -> Vec<(i64, Endpoint)> {
    let mut near: Vec<(f64, i64)> = (0..SIDE * SIDE)
        .map(|i| (cell_m(cell, (i % SIDE, i / SIDE)), i))
        .filter(|(m, _)| m / WALK_MPS <= ACCESS_MAX_S)
        .collect();
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    near.truncate(ACCESS_STOPS);
    near.into_iter()
        .map(|(m, i)| {
            (
                i,
                Endpoint {
                    node: city.stops[i as usize],
                    cost: (m / WALK_MPS).round() as i64,
                },
            )
        })
        .collect()
}

fn queries(city: &City, rng: &mut Rng, n: usize) -> Vec<Query> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let from = (rng.below(SIDE), rng.below(SIDE));
        let to = (rng.below(SIDE), rng.below(SIDE));
        if cell_m(from, to) < MIN_TRIP_M {
            continue;
        }
        out.push(Query {
            starts: nearby(city, from),
            ends: nearby(city, to),
            depart_at: 21_600 + rng.below(50_400),
        });
    }
    out
}

fn options(depart_at: i64) -> TimetableOptions {
    TimetableOptions {
        depart_at,
        days: vec![ServiceDay {
            offset: 0,
            services: vec![1],
        }],
        walk: Some(WalkFields {
            kinds: vec!["WALK".into()],
            cost: "s".into(),
        }),
        min_transfer: 60,
        max_transfers: 4,
        max_duration: Some(7_200),
        ..TimetableOptions::default()
    }
}

const CYPHER: &str = "UNWIND range(0, size($sid) - 1) AS i MATCH (s:Stop {stop: $sid[i]}) \
     WITH collect({node: s, cost: $scost[i]}) AS origin \
     UNWIND range(0, size($eid) - 1) AS j MATCH (e:Stop {stop: $eid[j]}) \
     WITH origin, collect({node: e, cost: $ecost[j]}) AS destination \
     CALL drevo.path.timetable(origin, destination, {departAt: $at, days: [{offset: 0, services: [1]}], \
       walk: {relTypes: ['WALK'], cost: 's'}, minTransfer: 60, maxTransfers: 4, maxDuration: 7200}) \
     YIELD arriveAt, transfers, legs RETURN arriveAt, transfers, size(legs) AS legs";

fn ints(v: impl Iterator<Item = i64>) -> Value {
    Value::List(v.map(Value::Integer).collect())
}

fn percentiles(label: &str, mut times: Vec<Duration>) {
    times.sort();
    let at = |p: f64| times[((times.len() as f64 * p).ceil() as usize).clamp(1, times.len()) - 1];
    println!(
        "{label:<28} n={:<5} p50 {:>8.2?}  p95 {:>8.2?}  p99 {:>8.2?}  max {:>8.2?}",
        times.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        times[times.len() - 1],
    );
}

fn rss_mb() -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kb: f64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
    Some(kb / 1024.0)
}

fn main() -> Result<(), Error> {
    let n: usize = std::env::var("DREVO_TIMETABLE_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);
    let mut rng = Rng(0x05ee_d762);
    let city = build(&mut rng)?;
    if let Some(rss) = rss_mb() {
        println!("RSS after load {rss:.1} MB");
    }
    let qs = queries(&city, &mut rng, n);

    let mut native = Vec::with_capacity(n);
    let mut found = 0;
    let mut transfers = [0usize; 6];
    for q in &qs {
        let starts: Vec<Endpoint> = q.starts.iter().map(|s| s.1).collect();
        let ends: Vec<Endpoint> = q.ends.iter().map(|e| e.1).collect();
        let t = Instant::now();
        let journeys = city.svc.timetable(&starts, &ends, &options(q.depart_at))?;
        native.push(t.elapsed());
        if let Some(fastest) = journeys.first() {
            found += 1;
            transfers[fastest.transfers.min(5)] += 1;
        }
    }
    percentiles("native timetable", native);
    println!(
        "found a journey for {found}/{n} queries; transfers of the fastest: {:?}",
        &transfers[..5]
    );

    let statement = parse(CYPHER)?;
    let mut cypher = Vec::with_capacity(n);
    let mut agree = 0;
    for q in &qs {
        let params = HashMap::from([
            ("sid".to_string(), ints(q.starts.iter().map(|s| s.0))),
            ("scost".to_string(), ints(q.starts.iter().map(|s| s.1.cost))),
            ("eid".to_string(), ints(q.ends.iter().map(|e| e.0))),
            ("ecost".to_string(), ints(q.ends.iter().map(|e| e.1.cost))),
            ("at".to_string(), Value::Integer(q.depart_at)),
        ]);
        let t = Instant::now();
        let rows = city.svc.execute(&statement, params)?.rows;
        cypher.push(t.elapsed());
        let starts: Vec<Endpoint> = q.starts.iter().map(|s| s.1).collect();
        let ends: Vec<Endpoint> = q.ends.iter().map(|e| e.1).collect();
        let native = city.svc.timetable(&starts, &ends, &options(q.depart_at))?;
        let arrivals: Vec<Value> = native.iter().map(|j| Value::Integer(j.arrive_at)).collect();
        if rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>() == arrivals {
            agree += 1;
        }
    }
    percentiles("CALL drevo.path.timetable", cypher);
    println!("Cypher and native agree on {agree}/{n} queries");
    assert_eq!(agree, n, "Cypher and the native call disagree");
    if let Some(rss) = rss_mb() {
        println!("RSS after queries {rss:.1} MB");
    }
    Ok(())
}
