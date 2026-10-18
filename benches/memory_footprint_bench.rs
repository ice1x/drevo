//! Where the memory of a property-light graph goes: heap bytes per node and
//! per relationship for the engine and for each secondary index, on an
//! OpenStreetMap road network.
//!
//! Every OSM node on a street becomes a `Junction {osm_id, lat, lon}` and every
//! stretch between two of them a `ROAD {car_s, walk_s}` — the shape of a
//! routing graph. A counting allocator measures the live heap after each
//! step, so the numbers are exact allocations, not RSS:
//!
//! 1. the graph as written (`NativeGraph`, durable, nodes and relationships
//!    created in batches);
//! 2. the same store reopened from its log — what a server holds after a
//!    restart — and the part of it that is the change feed;
//! 3. each secondary index the server keeps, built alone over the reopened
//!    graph: labels, property values, the value cache, full-text over nodes
//!    and over relationships;
//! 4. the whole service reopened over the same log after one query — what a
//!    server holds after a restart — with every index, and with the
//!    secondary indexes turned off (`drevo.indexes.setSecondary(false)`),
//!    each followed by the resident set size of the whole process.
//!
//! # Running
//!
//! ```sh
//! DREVO_OSM_JSON=sofia-roads.json cargo bench --bench memory_footprint_bench
//! ```
//!
//! See `benches/sofia_routing_bench.rs` for how to download the extract.
//! Without the variable the bench prints that and exits.
//!
//! To read the resident set size of a restarted server without the loader
//! in the same process, keep the store and reopen it in a second run:
//!
//! ```sh
//! DREVO_OSM_JSON=sofia-roads.json DREVO_MEMORY_KEEP=/tmp/sofia \
//!     cargo bench --bench memory_footprint_bench
//! DREVO_MEMORY_REOPEN=/tmp/sofia/native.wal cargo bench --bench memory_footprint_bench
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};

use serde_json::Value as Json;

use drevo::model::{NewEdge, NewNode, Properties};
use drevo::native::NativeGraph;

/// The system allocator, counting the bytes currently allocated.
struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is forwarded to `System` unchanged; the counter is only
// bookkeeping.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now =
                LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed) + layout.size() as isize;
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` / `realloc` with this layout.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

fn mb(bytes: isize) -> f64 {
    bytes as f64 / 1_048_576.0
}

const BATCH: usize = 20_000;

fn props(pairs: &[(&str, Json)]) -> Properties {
    let mut p = Properties::default();
    for (k, v) in pairs {
        p.insert((*k).to_string(), v.clone());
    }
    p
}

/// `(junctions, roads)` of the extract: `(osm_id, lat, lon)` and
/// `(from index, to index, car_s, walk_s)`.
type Network = (Vec<(i64, f64, f64)>, Vec<(usize, usize, f64, f64)>);

fn read_network(path: &str) -> Result<Network, Box<dyn std::error::Error>> {
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
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut junctions = Vec::new();
    let mut roads = Vec::new();
    for w in ways {
        let ids: Vec<i64> = w
            .get("nodes")
            .and_then(Json::as_array)
            .map(|ns| ns.iter().filter_map(Json::as_i64).collect())
            .unwrap_or_default();
        let mut at = |osm: i64| -> Option<usize> {
            let (lat, lon) = *coords.get(&osm)?;
            Some(*index.entry(osm).or_insert_with(|| {
                junctions.push((osm, lat, lon));
                junctions.len() - 1
            }))
        };
        let mut prev: Option<usize> = None;
        for osm in ids {
            let Some(here) = at(osm) else { continue };
            if let Some(from) = prev {
                roads.push((from, here, 10.0, 60.0));
            }
            prev = Some(here);
        }
    }
    Ok((junctions, roads))
}

fn report(what: &str, bytes: isize, nodes: usize, edges: usize) {
    let per_entity = bytes as f64 / (nodes + edges).max(1) as f64;
    println!(
        "{what:<44} {:>9.1} MB  {per_entity:>7.0} B/entity",
        mb(bytes)
    );
}

/// The process's resident set size in MB as `ps` reports it — the number an
/// operator sees; `None` where `ps` is unavailable.
fn rss_mb() -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kb: f64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
    Some(kb / 1024.0)
}

fn report_rss(what: &str) {
    if let Some(rss) = rss_mb() {
        println!("{what:<44} {rss:>9.1} MB RSS");
    }
}

/// Open the service over an existing log in a fresh process and report its
/// heap and resident set size — the restart a server goes through, without
/// the loader's memory in the picture.
fn reopen_only(wal: &str) {
    let before = live();
    let svc = drevo::native_service::NativeService::open(wal).expect("open service");
    report_rss("  process after open, before any query");
    println!(
        "  peak heap during open {:.1} MB",
        mb(PEAK.load(Ordering::Relaxed) - before)
    );
    svc.execute(
        &drevo::cypher::parser::parse("MATCH (n) RETURN count(n)").expect("parse"),
        HashMap::new(),
    )
    .expect("query");
    let entities = svc.graph().node_count() + svc.graph().edge_count();
    let lean = if svc.secondary_indexes() {
        "all indexes"
    } else {
        "secondary indexes off"
    };
    report(
        &format!("service over {wal} ({lean})"),
        live() - before,
        entities as usize,
        0,
    );
    report_rss("  process");
}

fn main() {
    if let Ok(wal) = std::env::var("DREVO_MEMORY_REOPEN") {
        // `DREVO_MEMORY_SECONDARY=on|off` only switches the stored setting,
        // so the next reopen run measures that configuration.
        if let Ok(setting) = std::env::var("DREVO_MEMORY_SECONDARY") {
            let svc = drevo::native_service::NativeService::open(&wal).expect("open service");
            svc.set_secondary_indexes(setting == "on")
                .expect("switch secondary indexes");
            return;
        }
        reopen_only(&wal);
        return;
    }
    let Ok(path) = std::env::var("DREVO_OSM_JSON") else {
        eprintln!(
            "memory_footprint_bench skipped: set DREVO_OSM_JSON to an Overpass JSON extract \
             (see benches/sofia_routing_bench.rs)"
        );
        return;
    };
    let (junctions, roads) = match read_network(&path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory_footprint_bench: cannot read {path}: {e}");
            std::process::exit(1);
        }
    };
    let (n, m) = (junctions.len(), roads.len());
    println!("{n} junctions, {m} roads\n");
    // `DREVO_MEMORY_KEEP=<dir>` writes the store there and keeps it, for a
    // later `DREVO_MEMORY_REOPEN=<dir>/native.wal` run.
    let keep = std::env::var("DREVO_MEMORY_KEEP").ok();
    let dir = keep.as_ref().map_or_else(
        || std::env::temp_dir().join(format!("drevo_memory_{}", std::process::id())),
        std::path::PathBuf::from,
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let wal = dir.join("native.wal");

    let before = live();
    {
        let g = NativeGraph::open_durable(&wal).expect("open");
        let mut ids = Vec::with_capacity(n);
        for chunk in junctions.chunks(BATCH) {
            let batch = chunk
                .iter()
                .map(|(osm, lat, lon)| NewNode {
                    kind: "Junction".into(),
                    title: String::new(),
                    body: String::new(),
                    body_html: String::new(),
                    properties: props(&[
                        ("osm_id", Json::from(*osm)),
                        ("lat", Json::from(*lat)),
                        ("lon", Json::from(*lon)),
                    ]),
                })
                .collect();
            ids.extend(
                g.create_nodes(batch)
                    .expect("nodes")
                    .into_iter()
                    .map(|x| x.id),
            );
        }
        for chunk in roads.chunks(BATCH) {
            let batch = chunk
                .iter()
                .map(|(a, b, car, walk)| NewEdge {
                    from_id: ids[*a],
                    to_id: ids[*b],
                    kind: "ROAD".into(),
                    weight: *car as f32,
                    properties: props(&[
                        ("car_s", Json::from(*car)),
                        ("walk_s", Json::from(*walk)),
                    ]),
                })
                .collect();
            g.create_edges(batch).expect("edges");
        }
        report(
            "graph as written (incl. change feed)",
            live() - before,
            n,
            m,
        );
    }
    drop(junctions);
    drop(roads);

    let before = live();
    let g = NativeGraph::open_durable(&wal).expect("reopen");
    let reopened = live() - before;
    report("graph reopened from the log", reopened, n, m);
    let feed_before = live();
    g.trim_before(g.change_head());
    let feed = feed_before - live();
    report("  of which the change feed (trimmed now)", feed, n, m);
    report("  engine without the feed", reopened - feed, n, m);
    println!();

    let step = |what: &str, build: &dyn Fn() -> Box<dyn std::any::Any>| {
        let before = live();
        let index = build();
        report(what, live() - before, n, m);
        drop(index);
    };
    step("label index", &|| {
        let mut i = drevo::native_label_index::NativeLabelIndex::new();
        i.sync(&g);
        Box::new(i)
    });
    step("property index", &|| {
        let mut i = drevo::native_property_index::NativePropertyIndex::new();
        i.sync(&g);
        Box::new(i)
    });
    step("value cache", &|| {
        let mut i = drevo::native_value_cache::NativeValueCache::new();
        i.sync(&g);
        Box::new(i)
    });
    step("full-text index, nodes", &|| {
        let mut i = drevo::native_fts::NativeFtsIndex::new();
        i.sync(&g);
        Box::new(i)
    });
    step("full-text index, relationships", &|| {
        let mut i = drevo::native_fts::NativeFtsRelIndex::new();
        i.sync(&g);
        Box::new(i)
    });
    drop(g);

    // What a server holds after a restart: the service over the same log,
    // with every index it keeps built.
    println!();
    let before = live();
    let svc = drevo::native_service::NativeService::open(&wal).expect("open service");
    svc.execute(
        &drevo::cypher::parser::parse("MATCH (n:Junction) RETURN count(n)").expect("parse"),
        HashMap::new(),
    )
    .expect("query");
    report("service reopened, all indexes", live() - before, n, m);
    report_rss("  process");
    svc.set_secondary_indexes(false).expect("lean");
    drop(svc);

    // The same, for a database that turned its secondary indexes off.
    let before = live();
    let svc = drevo::native_service::NativeService::open(&wal).expect("open lean service");
    svc.execute(
        &drevo::cypher::parser::parse("MATCH (n:Junction) RETURN count(n)").expect("parse"),
        HashMap::new(),
    )
    .expect("query");
    report(
        "service reopened, secondary indexes off",
        live() - before,
        n,
        m,
    );
    report_rss("  process");
    drop(svc);
    if keep.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
