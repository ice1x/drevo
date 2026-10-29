//! Walking isochrones on a road network the size of Sofia's:
//! `drevo.path.reachable` over a synthetic city.
//!
//! The city is a square grid of junctions with a street between neighbours
//! kept at random, so blocks have uneven shapes and some streets are dead
//! ends. Streets are 10–40 m long (OpenStreetMap splits a street at every junction and bend) and walked at 1.4 m/s. One in fifty
//! junctions is also a `StopAccess`, the entrance to a transit stop. The
//! sizes follow the Sofia OpenStreetMap road graph:
//!
//! | | synthetic | Sofia |
//! |---|---|---|
//! | junctions | 243,049 | 242,947 |
//! | streets (`ROAD` relationships) | ~281,000 | 281,421 |
//!
//! Each query starts at a random junction with a street and walks for up to
//! 15 minutes (`maxCost: 900`) in both directions along the streets:
//!
//! * `isochrone` — every junction within the budget;
//! * `stops` — only the `StopAccess` junctions within it
//!   (`targets: {label: 'StopAccess'}`), the walk to transit stops.
//!
//! # Running
//!
//! ```sh
//! cargo bench --bench reachable_bench
//! ```
//!
//! `DREVO_REACHABLE_QUERIES` sets the number of queries (default 1000). The
//! bench prints the latency percentiles of the native call
//! ([`NativeService::reachable`](drevo::native_service::NativeService::reachable))
//! and of the same queries through `CALL drevo.path.reachable`, and how many
//! junctions a query reaches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::native_service::NativeService;
use drevo::weighted_path::{EdgeCost, ReachableOptions, WeightedPathOptions};

const SIDE: u64 = 493;
/// The share of neighbouring junctions joined by a street, in percent.
const STREET_PCT: u64 = 58;
/// One junction in this many is a `StopAccess`.
const STOP_EVERY: u64 = 50;
const WALK_MPS: f64 = 1.4;
const BUDGET_S: f64 = 900.0;
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

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn props(entries: &[(&str, Json)]) -> Properties {
    let mut p = Properties::new();
    for (k, v) in entries {
        p.insert(*k, v.clone());
    }
    p
}

struct City {
    svc: NativeService,
    /// Junctions with at least one street.
    connected: Vec<u64>,
}

fn build(rng: &mut Rng) -> Result<City, Error> {
    let started = Instant::now();
    let svc = NativeService::in_memory();
    let mut junctions = Vec::with_capacity((SIDE * SIDE) as usize);
    let all: Vec<u64> = (0..SIDE * SIDE).collect();
    for chunk in all.chunks(BATCH) {
        let batch: Vec<NewNode> = chunk
            .iter()
            .map(|&i| {
                let mut properties = props(&[("junction", Json::from(i))]);
                if i % STOP_EVERY == 0 {
                    properties.insert("_labels", Json::from(vec!["StopAccess"]));
                }
                NewNode {
                    kind: "Junction".into(),
                    title: String::new(),
                    properties,
                    body: String::new(),
                    body_html: String::new(),
                }
            })
            .collect();
        junctions.extend(svc.create_nodes(batch)?.iter().map(|n| n.id));
    }

    let mut edges = Vec::new();
    let mut has_street = vec![false; junctions.len()];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let here = (y * SIDE + x) as usize;
            for there in [
                (x + 1 < SIDE).then(|| here + 1),
                (y + 1 < SIDE).then(|| here + SIDE as usize),
            ]
            .into_iter()
            .flatten()
            {
                if rng.below(100) >= STREET_PCT {
                    continue;
                }
                let metres = 10 + rng.below(31);
                has_street[here] = true;
                has_street[there] = true;
                edges.push(NewEdge {
                    from_id: junctions[here],
                    to_id: junctions[there],
                    kind: "ROAD".into(),
                    weight: 1.0,
                    properties: props(&[(
                        "walk_s",
                        Json::from((metres as f64 / WALK_MPS).round()),
                    )]),
                });
            }
        }
    }
    let streets = edges.len();
    let graph = svc.graph();
    while !edges.is_empty() {
        let rest = edges.split_off(edges.len().saturating_sub(BATCH));
        graph.create_edges(rest)?;
    }
    let connected = junctions
        .iter()
        .zip(&has_street)
        .filter_map(|(id, street)| street.then_some(*id))
        .collect();
    println!(
        "built in {:.1?}: {} junctions, {streets} streets",
        started.elapsed(),
        junctions.len(),
    );
    Ok(City { svc, connected })
}

fn walk() -> WeightedPathOptions {
    WeightedPathOptions {
        kinds: vec!["ROAD".into()],
        direction: Direction::Both,
        cost: EdgeCost::Property("walk_s".into()),
        max_cost: Some(BUDGET_S),
        ..WeightedPathOptions::default()
    }
}

const CYPHER: &str = "MATCH (a) WHERE id(a) = $origin \
     CALL drevo.path.reachable(a, {relTypes: ['ROAD'], direction: 'BOTH', cost: 'walk_s', \
       maxCost: 900, targets: $targets}) \
     YIELD node, cost RETURN count(node) AS reached, max(cost) AS furthest";

fn percentiles(label: &str, mut times: Vec<Duration>) {
    times.sort();
    let at = |p: f64| times[((times.len() as f64 * p).ceil() as usize).clamp(1, times.len()) - 1];
    println!(
        "{label:<36} n={:<5} p50 {:>8.2?}  p95 {:>8.2?}  p99 {:>8.2?}  max {:>8.2?}",
        times.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        times[times.len() - 1],
    );
}

fn main() -> Result<(), Error> {
    let n: usize = std::env::var("DREVO_REACHABLE_QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);
    let mut rng = Rng(0x05ee_d763);
    let city = build(&mut rng)?;
    let origins: Vec<u64> = (0..n)
        .map(|_| city.connected[rng.below(city.connected.len() as u64) as usize])
        .collect();
    let statement = parse(CYPHER)?;
    // The first statement of the run pays one-time costs that are not the
    // search's; it is timed on its own.
    let t = Instant::now();
    city.svc.execute(
        &statement,
        HashMap::from([
            ("origin".to_string(), Value::Integer(origins[0] as i64)),
            ("targets".to_string(), Value::Null),
        ]),
    )?;
    println!("first Cypher statement {:.2?}", t.elapsed());

    for (name, label) in [("isochrone", None), ("stops", Some("StopAccess"))] {
        let mut native = Vec::with_capacity(n);
        let mut counts = Vec::with_capacity(n);
        for &origin in &origins {
            let t = Instant::now();
            let found = city
                .svc
                .reachable(origin, &walk(), &ReachableOptions::default(), label)?;
            native.push(t.elapsed());
            counts.push(found.len());
        }
        percentiles(&format!("native reachable ({name})"), native);
        let mut sorted = counts.clone();
        sorted.sort_unstable();
        println!(
            "  nodes returned: median {}, p95 {}, max {}",
            sorted[sorted.len() / 2],
            sorted[sorted.len() * 95 / 100],
            sorted[sorted.len() - 1],
        );

        let targets = match label {
            Some(label) => Value::Map(
                [("label".to_string(), Value::String(label.into()))]
                    .into_iter()
                    .collect(),
            ),
            None => Value::Null,
        };
        let mut cypher = Vec::with_capacity(n);
        for (&origin, &count) in origins.iter().zip(&counts) {
            let params = HashMap::from([
                ("origin".to_string(), Value::Integer(origin as i64)),
                ("targets".to_string(), targets.clone()),
            ]);
            let t = Instant::now();
            let rows = city.svc.execute(&statement, params)?.rows;
            cypher.push(t.elapsed());
            assert_eq!(
                rows[0][0],
                Value::Integer(count as i64),
                "Cypher and the native call disagree from {origin}"
            );
        }
        percentiles(&format!("CALL drevo.path.reachable ({name})"), cypher);
    }
    Ok(())
}
