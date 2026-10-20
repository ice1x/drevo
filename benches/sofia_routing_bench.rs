//! Routing over a real road network: weighted path search on an
//! OpenStreetMap extract of Sofia.
//!
//! Every OSM node that lies on a street, footway or path becomes a
//! `Junction` node with `lat` and `lon`; every stretch of a way between two
//! consecutive OSM nodes becomes a relationship:
//!
//! * `ROAD` — a stretch cars may drive, in each direction they may drive it,
//!   with `car_s` (seconds at the street's speed) and, unless it is a
//!   motorway, `walk_s` (seconds on foot at 1.4 m/s);
//! * `PATH` — a footway, path, cycleway, pedestrian street or flight of
//!   steps, with `walk_s` only.
//!
//! Driving is `relTypes: ['ROAD'], direction: 'OUTGOING', cost: 'car_s'`, so
//! one-way streets are honoured; walking is
//! `relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s'`.
//!
//! # Running
//!
//! Download the streets of the Sofia bounding box from the Overpass API:
//!
//! ```sh
//! curl -A drevo-bench -o sofia-roads.json --data-urlencode \
//!   'data=[out:json][timeout:600];way["highway"](42.62,23.20,42.77,23.42);(._;>;);out body;' \
//!   https://overpass-api.de/api/interpreter
//! ```
//!
//! then point `DREVO_OSM_JSON` at the file:
//!
//! ```sh
//! DREVO_OSM_JSON=sofia-roads.json cargo bench --bench sofia_routing_bench
//! ```
//!
//! Without the variable the bench prints these instructions and exits. Map
//! data © OpenStreetMap contributors, available under the Open Database
//! License.
//!
//! # What is measured
//!
//! Routes between fixed pairs of junctions more than 3 km apart, as the
//! crow flies, picked with a fixed seed among junctions a car can drive
//! between:
//!
//! * `car_route` and `walk_route` — the native
//!   [`NativeService::weighted_path`](drevo::native_service::NativeService::weighted_path) with A* (a heuristic over `lat`/`lon`)
//!   and without (bidirectional Dijkstra);
//! * `car_route_cypher` — the same A* search through
//!   `CALL drevo.path.weighted`, the way a client calls it;
//! * `car_route_shortest_path` — [`NativeService::shortest_path_filtered`](drevo::native_service::NativeService::shortest_path_filtered),
//!   a one-directional Dijkstra over the relationship `weight`, which holds
//!   `car_s` too;
//! * `adjacency_sweep` — reading the outgoing relationships of every
//!   drivable junction once: copied as full records, read by reference (what
//!   a search does), and as ids only, to put the per-route numbers in
//!   proportion.
//!
//! Before timing, every pair is routed both ways and the bench stops if A*
//! and Dijkstra disagree on a cost.

use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::Criterion;
use serde_json::Value as Json;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::native_service::NativeService;
use drevo::weighted_path::{haversine_m, EdgeCost, GeoHeuristic, WeightedPathOptions};

/// Walking speed, in metres per second.
const WALK_MPS: f64 = 1.4;
/// Walking speed on steps, in metres per second.
const STEPS_MPS: f64 = 0.7;
/// Routed pairs must be at least this far apart, in metres.
const MIN_PAIR_M: f64 = 3_000.0;
/// How many pairs each workload routes per iteration.
const PAIRS: usize = 8;
/// Rows written per batch while loading.
const BATCH: usize = 20_000;

type Error = Box<dyn std::error::Error>;

/// The default speed of a street class in km/h, or `None` when cars may not
/// use it.
fn car_kmh(highway: &str) -> Option<f64> {
    Some(match highway {
        "motorway" => 100.0,
        "trunk" => 80.0,
        "primary" => 60.0,
        "secondary" => 50.0,
        "tertiary" => 40.0,
        "motorway_link" | "trunk_link" => 50.0,
        "primary_link" | "secondary_link" | "tertiary_link" => 30.0,
        "unclassified" | "residential" => 30.0,
        "service" | "track" => 15.0,
        "living_street" => 10.0,
        _ => return None,
    })
}

/// The walking speed of a street class in m/s, or `None` when it is closed
/// to pedestrians or is not a usable way at all (`construction`,
/// `proposed`, …).
fn walk_mps(highway: &str) -> Option<f64> {
    match highway {
        "steps" => Some(STEPS_MPS),
        "footway" | "path" | "pedestrian" | "cycleway" | "track" | "living_street"
        | "residential" | "service" | "unclassified" | "tertiary" | "tertiary_link"
        | "secondary" | "secondary_link" | "primary" | "primary_link" | "trunk" | "trunk_link" => {
            Some(WALK_MPS)
        }
        _ => None,
    }
}

/// A numeric `maxspeed` tag in km/h (`"50"`, `"50 km/h"`); anything else,
/// such as `"BG:urban"` or `"30 mph"`, gives `None`.
fn maxspeed_kmh(tag: Option<&str>) -> Option<f64> {
    let tag = tag?.trim();
    let digits = tag.strip_suffix("km/h").unwrap_or(tag).trim();
    digits.parse::<f64>().ok().filter(|v| *v > 0.0)
}

/// Which way cars may drive a way: `(forward, backward)`.
fn car_directions(tags: &serde_json::Map<String, Json>, highway: &str) -> (bool, bool) {
    let tag = |k: &str| tags.get(k).and_then(Json::as_str);
    match tag("oneway") {
        Some("yes" | "1" | "true") => (true, false),
        Some("-1" | "reverse") => (false, true),
        Some("no" | "0" | "false") => (true, true),
        _ if highway == "motorway" || tag("junction") == Some("roundabout") => (true, false),
        _ => (true, true),
    }
}

/// The loaded graph and the facts the workloads need.
struct Network {
    svc: NativeService,
    /// The `(lat, lon)` of every junction, by drevo node id.
    positions: HashMap<u64, (f64, f64)>,
    /// Junctions with at least one `ROAD` relationship.
    drivable: Vec<u64>,
    /// The fastest `car_s` speed of any `ROAD`, in m/s.
    max_car_mps: f64,
}

fn props(pairs: &[(&str, Json)]) -> Properties {
    let mut p = Properties::default();
    for (k, v) in pairs {
        p.insert(*k, v.clone());
    }
    p
}

fn load(path: &str) -> Result<Network, Error> {
    let started = Instant::now();
    let doc: Json = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let elements = doc
        .get("elements")
        .and_then(Json::as_array)
        .ok_or("no `elements` array: not an Overpass JSON file")?;

    let mut coords: HashMap<i64, (f64, f64)> = HashMap::new();
    let mut ways = Vec::new();
    for e in elements {
        match e.get("type").and_then(Json::as_str) {
            Some("node") => {
                if let (Some(id), Some(lat), Some(lon)) = (
                    e.get("id").and_then(Json::as_i64),
                    e.get("lat").and_then(Json::as_f64),
                    e.get("lon").and_then(Json::as_f64),
                ) {
                    coords.insert(id, (lat, lon));
                }
            }
            Some("way") => ways.push(e),
            _ => {}
        }
    }

    // One drevo node per OSM node some way passes through, in OSM id order.
    let mut used: Vec<i64> = ways
        .iter()
        .filter_map(|w| w.get("nodes").and_then(Json::as_array))
        .flatten()
        .filter_map(Json::as_i64)
        .filter(|id| coords.contains_key(id))
        .collect();
    used.sort_unstable();
    used.dedup();

    let svc = NativeService::in_memory();
    let mut node_of: HashMap<i64, u64> = HashMap::with_capacity(used.len());
    let mut positions = HashMap::with_capacity(used.len());
    for chunk in used.chunks(BATCH) {
        let batch = chunk
            .iter()
            .map(|osm| {
                let (lat, lon) = coords[osm];
                NewNode {
                    kind: "Junction".into(),
                    title: format!("osm node {osm}"),
                    properties: props(&[
                        ("osm_id", Json::from(*osm)),
                        ("lat", Json::from(lat)),
                        ("lon", Json::from(lon)),
                    ]),
                    body: String::new(),
                    body_html: String::new(),
                }
            })
            .collect();
        for (osm, node) in chunk.iter().zip(svc.create_nodes(batch)?) {
            node_of.insert(*osm, node.id);
            positions.insert(node.id, coords[osm]);
        }
    }

    let mut edges = Vec::new();
    let mut max_car_mps: f64 = 0.0;
    let mut drivable = Vec::new();
    for w in &ways {
        let Some(tags) = w.get("tags").and_then(Json::as_object) else {
            continue;
        };
        let Some(highway) = tags.get("highway").and_then(Json::as_str) else {
            continue;
        };
        let car = car_kmh(highway).map(|default| {
            maxspeed_kmh(tags.get("maxspeed").and_then(Json::as_str)).unwrap_or(default) / 3.6
        });
        let walk = walk_mps(highway);
        let (forward, backward) = car_directions(tags, highway);
        let ids: Vec<i64> = w
            .get("nodes")
            .and_then(Json::as_array)
            .map(|ns| ns.iter().filter_map(Json::as_i64).collect())
            .unwrap_or_default();
        for pair in ids.windows(2) {
            let (Some(&a), Some(&b)) = (node_of.get(&pair[0]), node_of.get(&pair[1])) else {
                continue;
            };
            let (pa, pb) = (positions[&a], positions[&b]);
            let metres = haversine_m(pa, pb);
            let walk_s = walk.map(|mps| Json::from(metres / mps));
            match car {
                Some(mps) => {
                    max_car_mps = max_car_mps.max(mps);
                    let car_s = metres / mps;
                    let mut p = vec![("car_s", Json::from(car_s))];
                    if let Some(walk_s) = &walk_s {
                        p.push(("walk_s", walk_s.clone()));
                    }
                    let mut push = |from: u64, to: u64| {
                        drivable.push(from);
                        edges.push(NewEdge {
                            from_id: from,
                            to_id: to,
                            kind: "ROAD".into(),
                            weight: car_s as f32,
                            properties: props(&p),
                        });
                    };
                    if forward {
                        push(a, b);
                    }
                    if backward {
                        push(b, a);
                    }
                }
                None => {
                    if let Some(walk_s) = walk_s {
                        edges.push(NewEdge {
                            from_id: a,
                            to_id: b,
                            kind: "PATH".into(),
                            weight: 1.0,
                            properties: props(&[("walk_s", walk_s)]),
                        });
                    }
                }
            }
        }
    }
    let edge_count = edges.len();
    let graph = svc.graph();
    while !edges.is_empty() {
        let rest = edges.split_off(edges.len().saturating_sub(BATCH));
        graph.create_edges(rest)?;
    }
    drivable.sort_unstable();
    drivable.dedup();

    eprintln!(
        "loaded {path} in {:.1?}: {} junctions, {edge_count} relationships, \
         {} drivable junctions, fastest road {:.1} m/s",
        started.elapsed(),
        used.len(),
        drivable.len(),
        max_car_mps,
    );
    Ok(Network {
        svc,
        positions,
        drivable,
        max_car_mps,
    })
}

fn car_options(heuristic: Option<f64>) -> WeightedPathOptions {
    WeightedPathOptions {
        kinds: vec!["ROAD".into()],
        direction: Direction::Outgoing,
        cost: EdgeCost::Property("car_s".into()),
        heuristic: heuristic.map(geo),
        max_cost: None,
    }
}

fn walk_options(heuristic: bool) -> WeightedPathOptions {
    WeightedPathOptions {
        kinds: vec!["ROAD".into(), "PATH".into()],
        direction: Direction::Both,
        cost: EdgeCost::Property("walk_s".into()),
        heuristic: heuristic.then(|| geo(WALK_MPS)),
        max_cost: None,
    }
}

fn geo(max_speed_mps: f64) -> GeoHeuristic {
    GeoHeuristic {
        lat: "lat".into(),
        lon: "lon".into(),
        max_speed_mps,
    }
}

/// `PAIRS` pairs of drivable junctions at least `MIN_PAIR_M` apart that a
/// car can drive between, chosen with a fixed-seed generator.
fn pick_pairs(net: &Network) -> Result<Vec<(u64, u64)>, Error> {
    let mut state: u64 = 0x0005_eed0_f50f_1a00;
    let mut next = |n: usize| {
        // splitmix64
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) % n as u64) as usize
    };
    let options = car_options(Some(net.max_car_mps));
    let mut pairs = Vec::new();
    for _ in 0..1_000 {
        if pairs.len() == PAIRS {
            break;
        }
        let a = net.drivable[next(net.drivable.len())];
        let b = net.drivable[next(net.drivable.len())];
        let (pa, pb) = (net.positions[&a], net.positions[&b]);
        if haversine_m(pa, pb) < MIN_PAIR_M {
            continue;
        }
        if net.svc.weighted_path(a, b, &options)?.is_some() {
            pairs.push((a, b));
        }
    }
    if pairs.len() < PAIRS {
        return Err(format!("found only {} routable pairs", pairs.len()).into());
    }
    Ok(pairs)
}

/// Route every pair with A* and with Dijkstra and stop on a cost mismatch.
fn check(net: &Network, pairs: &[(u64, u64)]) -> Result<(), Error> {
    let modes = [
        ("car", car_options(Some(net.max_car_mps)), car_options(None)),
        ("walk", walk_options(true), walk_options(false)),
    ];
    for (mode, a_star, dijkstra) in &modes {
        for &(a, b) in pairs {
            let fast = net.svc.weighted_path(a, b, a_star)?;
            let slow = net.svc.weighted_path(a, b, dijkstra)?;
            let (Some(fast), Some(slow)) = (fast, slow) else {
                return Err(
                    format!("{mode} {a}->{b}: A* and Dijkstra disagree on reachability").into(),
                );
            };
            if (fast.cost - slow.cost).abs() > 1e-6 * slow.cost.max(1.0) {
                return Err(format!(
                    "{mode} {a}->{b}: A* cost {} but Dijkstra cost {}",
                    fast.cost, slow.cost
                )
                .into());
            }
            eprintln!(
                "{mode} {a}->{b}: {:.0} s over {} junctions",
                slow.cost,
                slow.nodes.len()
            );
        }
    }
    Ok(())
}

fn bench_native(
    c: &mut Criterion,
    group: &str,
    net: &Network,
    pairs: &[(u64, u64)],
    variants: &[(&str, WeightedPathOptions)],
) {
    let mut g = c.benchmark_group(group.to_string());
    for (name, options) in variants {
        g.bench_function(*name, |b| {
            b.iter(|| {
                for &(from, to) in pairs {
                    black_box(net.svc.weighted_path(from, to, options).expect("route"));
                }
            })
        });
    }
    g.finish();
}

fn main() {
    let Ok(path) = std::env::var("DREVO_OSM_JSON") else {
        eprintln!(
            "sofia_routing_bench skipped: set DREVO_OSM_JSON to an Overpass JSON extract \
             of a road network; see the header of benches/sofia_routing_bench.rs"
        );
        return;
    };
    let net = match load(&path) {
        Ok(net) => net,
        Err(e) => {
            eprintln!("sofia_routing_bench: cannot load {path}: {e}");
            std::process::exit(1);
        }
    };
    let pairs = match pick_pairs(&net).and_then(|p| check(&net, &p).map(|()| p)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sofia_routing_bench: {e}");
            std::process::exit(1);
        }
    };

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(10))
        .sample_size(10)
        .configure_from_args();

    bench_native(
        &mut c,
        "car_route",
        &net,
        &pairs,
        &[
            ("a_star", car_options(Some(net.max_car_mps))),
            ("dijkstra", car_options(None)),
        ],
    );
    bench_native(
        &mut c,
        "walk_route",
        &net,
        &pairs,
        &[
            ("a_star", walk_options(true)),
            ("dijkstra", walk_options(false)),
        ],
    );

    let query = parse(&format!(
        "MATCH (a) WHERE id(a) = $from MATCH (b) WHERE id(b) = $to \
         CALL drevo.path.weighted(a, b, {{relTypes: ['ROAD'], direction: 'OUTGOING', \
         cost: 'car_s', heuristic: {{lat: 'lat', lon: 'lon', maxSpeedMps: {}}}}}) \
         YIELD cost RETURN cost",
        net.max_car_mps
    ))
    .expect("route query parses");
    let mut g = c.benchmark_group("car_route_cypher");
    g.bench_function("a_star", |b| {
        b.iter(|| {
            for &(from, to) in &pairs {
                let params = HashMap::from([
                    ("from".to_string(), Value::Integer(from as i64)),
                    ("to".to_string(), Value::Integer(to as i64)),
                ]);
                let rows = net.svc.execute(&query, params).expect("route query").rows;
                assert_eq!(rows.len(), 1, "one route per pair");
                black_box(rows);
            }
        })
    });
    g.finish();

    let mut g = c.benchmark_group("car_route_shortest_path");
    g.bench_function("dijkstra_weight", |b| {
        b.iter(|| {
            for &(from, to) in &pairs {
                black_box(
                    net.svc
                        .shortest_path_filtered(from, to, Some("ROAD"))
                        .expect("shortest path"),
                );
            }
        })
    });
    g.finish();

    // The adjacency reads underneath a search: every drivable junction's
    // outgoing relationships as full records (what the search reads) and as
    // ids only.
    let snap = net.svc.graph().snapshot();
    let mut g = c.benchmark_group("adjacency_sweep");
    g.bench_function("edges_of", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for &id in &net.drivable {
                total += snap.edges_of(id, Direction::Outgoing).len();
            }
            black_box(total)
        })
    });
    g.bench_function("for_each_edge_of", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for &id in &net.drivable {
                snap.for_each_edge_of(id, Direction::Outgoing, |_| total += 1);
            }
            black_box(total)
        })
    });
    g.bench_function("neighbor_ids", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for &id in &net.drivable {
                total += snap.neighbor_ids(id, Direction::Outgoing, None).len();
            }
            black_box(total)
        })
    });
    g.finish();

    c.final_summary();
}
