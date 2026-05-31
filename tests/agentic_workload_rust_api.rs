//! Phase 10.5 task `00123` — Rust API agentic workload baseline (layer 1).
//!
//! This is **layer 1** of the five-layer agentic workload model described in
//! the roadmap (README → "Phase 10.5 — Cypher Reliability & Agentic
//! Hardening"). It measures the *upper bound of the storage layer alone* —
//! "what redb + our indexes can do, nothing on top". Every later layer
//! (Cypher executor `00128`, Python PyO3, MCP stdio `00127`, Bolt wire) is
//! compared against the numbers this baseline produces; without it,
//! perf-regression bisection across layers is blind.
//!
//! The workload drives the **raw [`Drevo`] API** (no Cypher, no wire
//! protocol) with a realistic agentic mix — 70 % reads, 20 % writes, 10 %
//! search — across ten independently-tracked query classes:
//!
//! | class            | category | API call                         |
//! |------------------|----------|----------------------------------|
//! | `lookup_uuid`    | read     | [`Drevo::get_node_by_uuid`]      |
//! | `lookup_title`   | read     | [`Drevo::get_node_by_title`]     |
//! | `traversal_2hop` | read     | [`Drevo::bfs`] depth 2           |
//! | `traversal_3hop` | read     | [`Drevo::bfs`] depth 3           |
//! | `subgraph_2`     | read     | [`Drevo::subgraph`] depth 2      |
//! | `fts_short`      | search   | [`Drevo::search_fts`] (1 token)  |
//! | `fts_phrase`     | search   | [`Drevo::search_fts`] (2 tokens) |
//! | `create_node`    | write    | [`Drevo::create_node`]           |
//! | `update_props`   | write    | [`Drevo::update_node`]           |
//! | `delete_node`    | write    | [`Drevo::delete_node`]           |
//!
//! For every class the harness records a latency sample and reports
//! **p50 / p95 / p99**, alongside **peak RSS** and **redb file growth** over
//! the run.
//!
//! ## Test layout
//!
//! The expensive part — a 30+ minute, 10 k-node soak — lives behind
//! `#[ignore]` so the PR pipeline stays fast; it is meant for nightly CI and
//! on-demand runs:
//!
//! ```text
//! cargo nextest run --test agentic_workload_rust_api -- --ignored --nocapture
//! ```
//!
//! The remaining tests are fast scaffolding that prove the harness machinery
//! itself is correct (deterministic RNG, percentile math, weight coverage,
//! corpus connectivity, every class exercised, RSS + growth metrics
//! recorded) and run on every PR in well under a second.
//!
//! The harness is deliberately **self-contained** in this one file — matching
//! the project convention (see `mcp_validation_e2e_tests.rs`) of keeping each
//! integration test file independently grep-able rather than factoring a
//! shared `tests/common/` module. Task `00128` (Cypher executor workload)
//! reuses the *same query mix* but drives it through `parse → execute`, and
//! will adapt this shape against the executor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, NodePatch, Properties};

// ---------------------------------------------------------------------------
// Deterministic RNG
// ---------------------------------------------------------------------------

/// Tiny deterministic xorshift64 PRNG.
///
/// The workload must be **reproducible** so a regression at a fixed seed is
/// re-runnable. We roll our own rather than pulling a `rand` dev-dependency:
/// xorshift is two lines, fast, and good enough to spread query selection and
/// target choice across the corpus without statistical pretension.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift64 has a single fixed point at 0 (it would emit 0 forever),
        // so force a non-zero state.
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform-ish value in `0..n`. Panics on `n == 0` (a programming error in
    /// the harness, never reachable from real corpora which are non-empty).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }
}

// ---------------------------------------------------------------------------
// Query classes + weighted selection
// ---------------------------------------------------------------------------

/// The ten independently-tracked query classes from the task spec.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum QueryClass {
    LookupUuid,
    LookupTitle,
    Traversal2Hop,
    Traversal3Hop,
    Subgraph2,
    FtsShort,
    FtsPhrase,
    CreateNode,
    UpdateProps,
    DeleteNode,
}

impl QueryClass {
    /// Every class, in report order.
    const ALL: [QueryClass; 10] = [
        QueryClass::LookupUuid,
        QueryClass::LookupTitle,
        QueryClass::Traversal2Hop,
        QueryClass::Traversal3Hop,
        QueryClass::Subgraph2,
        QueryClass::FtsShort,
        QueryClass::FtsPhrase,
        QueryClass::CreateNode,
        QueryClass::UpdateProps,
        QueryClass::DeleteNode,
    ];

    fn name(self) -> &'static str {
        match self {
            QueryClass::LookupUuid => "lookup_uuid",
            QueryClass::LookupTitle => "lookup_title",
            QueryClass::Traversal2Hop => "traversal_2hop",
            QueryClass::Traversal3Hop => "traversal_3hop",
            QueryClass::Subgraph2 => "subgraph_2",
            QueryClass::FtsShort => "fts_short",
            QueryClass::FtsPhrase => "fts_phrase",
            QueryClass::CreateNode => "create_node",
            QueryClass::UpdateProps => "update_props",
            QueryClass::DeleteNode => "delete_node",
        }
    }
}

/// Selection weights summing to 100, encoding the **70 % read / 20 % write /
/// 10 % search** agentic mix from the cross-cutting acceptance criteria.
///
/// Reads (70): five classes × 14. Search (10): two classes × 5. Writes (20):
/// `create_node` 8, `update_props` 6, `delete_node` 6 — `create` is kept a
/// touch above `delete` so the live set grows modestly over a long run
/// (exercising redb file growth) while never starving `delete` of targets.
const WEIGHTS: [(QueryClass, u32); 10] = [
    (QueryClass::LookupUuid, 14),
    (QueryClass::LookupTitle, 14),
    (QueryClass::Traversal2Hop, 14),
    (QueryClass::Traversal3Hop, 14),
    (QueryClass::Subgraph2, 14),
    (QueryClass::FtsShort, 5),
    (QueryClass::FtsPhrase, 5),
    (QueryClass::CreateNode, 8),
    (QueryClass::UpdateProps, 6),
    (QueryClass::DeleteNode, 6),
];

fn weight_total() -> u32 {
    WEIGHTS.iter().map(|(_, w)| *w).sum()
}

fn select_class(rng: &mut Rng) -> QueryClass {
    let mut r = rng.below(weight_total() as u64) as u32;
    for (class, weight) in WEIGHTS {
        if r < weight {
            return class;
        }
        r -= weight;
    }
    // Unreachable while `r < weight_total()`, but return a sensible default
    // rather than panic if the invariant is ever broken.
    WEIGHTS[0].0
}

// ---------------------------------------------------------------------------
// Latency statistics
// ---------------------------------------------------------------------------

/// Per-class latency sample collector. Samples are nanosecond durations;
/// percentiles are computed on demand (nearest-rank) by cloning + sorting,
/// which is cheap at the sample counts a single run produces.
#[derive(Default)]
struct LatencyStats {
    samples: Vec<u64>,
}

impl LatencyStats {
    fn record(&mut self, nanos: u64) {
        self.samples.push(nanos);
    }

    fn count(&self) -> usize {
        self.samples.len()
    }

    /// Nearest-rank percentile (`p` in `0.0..=100.0`). `None` if no samples.
    fn percentile(&self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(Duration::from_nanos(sorted[idx]))
    }
}

// ---------------------------------------------------------------------------
// Workload configuration + report
// ---------------------------------------------------------------------------

/// Parameters for a single workload run. The same harness powers the fast
/// scaffolding tests (small corpus, `max_queries`, unthrottled) and the soak
/// (`duration`, throttled to `target_qpm`).
struct WorkloadConfig {
    /// Number of base nodes built before the workload loop. Base nodes are
    /// never deleted, so they remain valid traversal roots / lookup targets.
    corpus_size: usize,
    /// RNG seed — fixes the entire run for reproducibility.
    seed: u64,
    /// Target queries-per-minute throttle. `0` means run unthrottled (used by
    /// the fast tests).
    target_qpm: u32,
    /// Stop after this many queries, if set.
    max_queries: Option<u64>,
    /// Stop after this wall-clock duration, if set. At least one of
    /// `max_queries` / `duration` must be set.
    duration: Option<Duration>,
    /// On-disk path of the redb file, used to measure file growth. `None` for
    /// in-memory runs (growth is not measurable).
    db_path: Option<PathBuf>,
}

/// The result of a workload run: per-class latency stats plus process-level
/// memory and on-disk growth metrics.
struct WorkloadReport {
    stats: HashMap<QueryClass, LatencyStats>,
    total_queries: u64,
    elapsed: Duration,
    /// RSS sampled right after the corpus was built (steady-state baseline).
    baseline_rss_kb: Option<u64>,
    /// Maximum RSS observed during the workload loop.
    peak_rss_kb: Option<u64>,
    /// redb file size delta (end − baseline) in bytes; `None` in-memory.
    db_growth_bytes: Option<i64>,
}

impl WorkloadReport {
    fn class(&self, c: QueryClass) -> &LatencyStats {
        &self.stats[&c]
    }

    /// Emit a human-readable report (visible under `--nocapture`).
    fn print(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        eprintln!("\n=== agentic workload: {label} ===");
        eprintln!(
            "total queries: {}  elapsed: {:.1}s  throughput: {:.1} q/s ({:.0} q/min)",
            self.total_queries,
            secs,
            self.total_queries as f64 / secs,
            self.total_queries as f64 / secs * 60.0,
        );
        eprintln!(
            "{:<15} {:>7} {:>10} {:>10} {:>10}",
            "class", "count", "p50(µs)", "p95(µs)", "p99(µs)"
        );
        for c in QueryClass::ALL {
            let s = self.class(c);
            let us = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1e6).unwrap_or(0.0);
            eprintln!(
                "{:<15} {:>7} {:>10.1} {:>10.1} {:>10.1}",
                c.name(),
                s.count(),
                us(s.percentile(50.0)),
                us(s.percentile(95.0)),
                us(s.percentile(99.0)),
            );
        }
        match (self.baseline_rss_kb, self.peak_rss_kb) {
            (Some(b), Some(p)) => eprintln!(
                "RSS: baseline={b} KB  peak={p} KB  ratio={:.2} (envelope target ≤ 1.20)",
                p as f64 / b.max(1) as f64
            ),
            _ => eprintln!("RSS: unavailable on this platform"),
        }
        match self.db_growth_bytes {
            Some(g) => eprintln!("redb file growth: {g} bytes"),
            None => eprintln!("redb file growth: n/a (in-memory)"),
        }
    }
}

// ---------------------------------------------------------------------------
// Corpus construction
// ---------------------------------------------------------------------------

/// Word pool (all ≥ 4 chars, so every token yields FTS trigrams) used to build
/// searchable titles and bodies. FTS queries are drawn from this same pool so
/// `fts_short` / `fts_phrase` return real hits rather than empty results.
const WORDS: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima",
];

const KINDS: [&str; 5] = ["note", "task", "person", "project", "bug"];

/// Build a [`NewNode`] for sequence number `seq`. The `{seq:08}` suffix makes
/// the title globally unique (titles are uniqueness-constrained), so base
/// corpus (`seq` `0..n`) and workload-created nodes (`seq >= n`) never collide.
fn make_node(rng: &mut Rng, seq: u64) -> NewNode {
    let w1 = WORDS[rng.index(WORDS.len())];
    let w2 = WORDS[rng.index(WORDS.len())];
    let w3 = WORDS[rng.index(WORDS.len())];
    NewNode {
        kind: KINDS[rng.index(KINDS.len())].to_string(),
        title: format!("{w1} {w2} {w3} item {seq:08}"),
        body: format!("{w1} {w2} {w3} body for node {seq}, agentic workload corpus"),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

/// Immutable snapshot of the base corpus the workload reads against.
struct Corpus {
    ids: Vec<u64>,
    uuids: Vec<[u8; 16]>,
    titles: Vec<String>,
}

/// Create `n` base nodes and a forward-link edge fabric (each node → +1/+2/+3
/// mod n) so 2-hop / 3-hop traversals and depth-2 subgraphs return rich,
/// non-trivial result sets.
fn build_corpus(db: &Drevo, n: usize, rng: &mut Rng) -> Corpus {
    assert!(
        n >= 4,
        "corpus must have at least 4 nodes for the edge fabric"
    );
    let mut ids = Vec::with_capacity(n);
    let mut uuids = Vec::with_capacity(n);
    let mut titles = Vec::with_capacity(n);

    for i in 0..n {
        let node = db
            .create_node(make_node(rng, i as u64))
            .expect("create base node");
        ids.push(node.id);
        uuids.push(node.uuid);
        titles.push(node.title);
    }

    for i in 0..n {
        for off in [1usize, 2, 3] {
            let to = ids[(i + off) % n];
            db.create_edge(NewEdge {
                from_id: ids[i],
                to_id: to,
                kind: "rel".to_string(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .expect("create base edge");
        }
    }

    Corpus { ids, uuids, titles }
}

// ---------------------------------------------------------------------------
// Process metrics
// ---------------------------------------------------------------------------

/// Current resident set size in KB, or `None` if it cannot be determined.
///
/// On Linux we read `/proc/self/statm` (resident pages × 4 KB — the standard
/// page size; an approximation that is more than adequate for a growth
/// envelope). Elsewhere (notably macOS dev machines) we fall back to
/// `ps -o rss= -p <pid>`, which already reports KB.
fn current_rss_kb() -> Option<u64> {
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(resident) = statm.split_whitespace().nth(1) {
            if let Ok(pages) = resident.parse::<u64>() {
                return Some(pages.saturating_mul(4));
            }
        }
    }
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|m| m.len()).ok()
}

// ---------------------------------------------------------------------------
// Workload driver
// ---------------------------------------------------------------------------

/// Mutable state threaded through the workload loop.
struct WorkloadState {
    /// Ids of nodes created during the loop and not yet deleted — the only
    /// nodes `delete_node` removes (base nodes stay, preserving connectivity).
    created: Vec<u64>,
    /// Next sequence number for created-node titles (continues past the base
    /// corpus to keep titles unique).
    next_seq: u64,
}

/// Execute exactly one query of the given class against the live database.
///
/// Reads/searches discard their results (we measure latency, not payload).
/// Writes mutate [`WorkloadState`] so subsequent lookups/updates/deletes have
/// valid targets. Any storage error is a hard failure — the soak's "no panic"
/// guarantee is exactly this `expect` surfacing a regression.
fn execute(
    class: QueryClass,
    db: &Drevo,
    corpus: &Corpus,
    state: &mut WorkloadState,
    rng: &mut Rng,
) {
    match class {
        QueryClass::LookupUuid => {
            let uuid = corpus.uuids[rng.index(corpus.uuids.len())];
            db.get_node_by_uuid(&uuid).expect("lookup_uuid");
        }
        QueryClass::LookupTitle => {
            let title = &corpus.titles[rng.index(corpus.titles.len())];
            db.get_node_by_title(title).expect("lookup_title");
        }
        QueryClass::Traversal2Hop => {
            let root = corpus.ids[rng.index(corpus.ids.len())];
            db.bfs(root, 2, Direction::Both, None)
                .expect("traversal_2hop");
        }
        QueryClass::Traversal3Hop => {
            let root = corpus.ids[rng.index(corpus.ids.len())];
            db.bfs(root, 3, Direction::Both, None)
                .expect("traversal_3hop");
        }
        QueryClass::Subgraph2 => {
            let root = corpus.ids[rng.index(corpus.ids.len())];
            db.subgraph(root, 2).expect("subgraph_2");
        }
        QueryClass::FtsShort => {
            let word = WORDS[rng.index(WORDS.len())];
            db.search_fts(word, 10).expect("fts_short");
        }
        QueryClass::FtsPhrase => {
            let w1 = WORDS[rng.index(WORDS.len())];
            let w2 = WORDS[rng.index(WORDS.len())];
            db.search_fts(&format!("{w1} {w2}"), 10)
                .expect("fts_phrase");
        }
        QueryClass::CreateNode => {
            let node = db
                .create_node(make_node(rng, state.next_seq))
                .expect("create_node");
            state.next_seq += 1;
            state.created.push(node.id);
        }
        QueryClass::UpdateProps => {
            // Update a live node — either a base node or a created one. Both
            // are guaranteed present. Touching `body` re-indexes FTS, which is
            // the realistic write cost we want in the number.
            let target = if state.created.is_empty() || rng.below(2) == 0 {
                corpus.ids[rng.index(corpus.ids.len())]
            } else {
                state.created[rng.index(state.created.len())]
            };
            let mut props = HashMap::new();
            props.insert(
                "touched".to_string(),
                serde_json::Value::from(state.next_seq),
            );
            db.update_node(
                target,
                NodePatch {
                    body: Some(format!("updated body rev {}", state.next_seq)),
                    properties: Some(Properties(props)),
                    ..Default::default()
                },
            )
            .expect("update_props");
        }
        QueryClass::DeleteNode => {
            // Only created nodes are deleted. If none are queued yet, mint one
            // first so the class still measures a real delete rather than
            // skewing the count.
            if state.created.is_empty() {
                let node = db
                    .create_node(make_node(rng, state.next_seq))
                    .expect("seed delete target");
                state.next_seq += 1;
                state.created.push(node.id);
            }
            let idx = rng.index(state.created.len());
            let id = state.created.swap_remove(idx);
            db.delete_node(id).expect("delete_node");
        }
    }
}

/// Run a workload to completion and return its report.
fn run_workload(db: &Drevo, cfg: &WorkloadConfig) -> WorkloadReport {
    assert!(
        cfg.max_queries.is_some() || cfg.duration.is_some(),
        "workload needs a stop condition (max_queries and/or duration)"
    );

    let mut rng = Rng::new(cfg.seed);
    let corpus = build_corpus(db, cfg.corpus_size, &mut rng);

    let baseline_rss_kb = current_rss_kb();
    let mut peak_rss_kb = baseline_rss_kb;
    let size_before = cfg.db_path.as_deref().and_then(file_size);

    let mut stats: HashMap<QueryClass, LatencyStats> = QueryClass::ALL
        .into_iter()
        .map(|c| (c, LatencyStats::default()))
        .collect();

    let mut state = WorkloadState {
        created: Vec::new(),
        next_seq: cfg.corpus_size as u64,
    };

    // Per-query pacing interval (0 qpm ⇒ unthrottled).
    let interval = if cfg.target_qpm > 0 {
        Some(Duration::from_secs_f64(60.0 / cfg.target_qpm as f64))
    } else {
        None
    };

    let start = Instant::now();
    let mut total: u64 = 0;
    loop {
        if let Some(max) = cfg.max_queries {
            if total >= max {
                break;
            }
        }
        if let Some(dur) = cfg.duration {
            if start.elapsed() >= dur {
                break;
            }
        }

        let class = select_class(&mut rng);
        let op_start = Instant::now();
        execute(class, db, &corpus, &mut state, &mut rng);
        let nanos = op_start.elapsed().as_nanos() as u64;
        stats
            .get_mut(&class)
            .expect("class registered")
            .record(nanos);
        total += 1;

        // Sample RSS periodically — cheap enough but not per-query.
        if total % 64 == 0 {
            if let Some(rss) = current_rss_kb() {
                peak_rss_kb = Some(peak_rss_kb.map_or(rss, |p| p.max(rss)));
            }
        }

        if let Some(iv) = interval {
            let spent = op_start.elapsed();
            if spent < iv {
                std::thread::sleep(iv - spent);
            }
        }
    }
    let elapsed = start.elapsed();

    // Final RSS sample + file growth.
    if let Some(rss) = current_rss_kb() {
        peak_rss_kb = Some(peak_rss_kb.map_or(rss, |p| p.max(rss)));
    }
    let db_growth_bytes = match (size_before, cfg.db_path.as_deref().and_then(file_size)) {
        (Some(before), Some(after)) => Some(after as i64 - before as i64),
        _ => None,
    };

    WorkloadReport {
        stats,
        total_queries: total,
        elapsed,
        baseline_rss_kb,
        peak_rss_kb,
        db_growth_bytes,
    }
}

// ===========================================================================
// Fast scaffolding tests — run on every PR
// ===========================================================================

#[test]
fn xorshift_is_deterministic_and_nonzero() {
    let mut a = Rng::new(12345);
    let mut b = Rng::new(12345);
    let seq_a: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
    let seq_b: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
    assert_eq!(seq_a, seq_b, "same seed must yield the same sequence");

    // Seed 0 must not collapse to an all-zero stream.
    let mut zero = Rng::new(0);
    let vals: Vec<u64> = (0..8).map(|_| zero.next_u64()).collect();
    assert!(
        vals.iter().any(|&v| v != 0),
        "seed 0 must not emit only zeros"
    );

    // Different seeds diverge.
    let mut c = Rng::new(999);
    assert_ne!(a.next_u64(), c.next_u64());
}

#[test]
fn query_weights_cover_all_ten_classes_and_sum_to_100() {
    assert_eq!(weight_total(), 100, "weights must sum to 100");
    assert_eq!(WEIGHTS.len(), 10);
    assert_eq!(QueryClass::ALL.len(), 10);

    // Every class in ALL appears in WEIGHTS with a non-zero weight.
    for c in QueryClass::ALL {
        let w = WEIGHTS.iter().find(|(qc, _)| *qc == c).map(|(_, w)| *w);
        assert!(
            matches!(w, Some(w) if w > 0),
            "{} missing/zero weight",
            c.name()
        );
    }

    // 70 % read / 20 % write / 10 % search split.
    let sum = |classes: &[QueryClass]| -> u32 {
        classes
            .iter()
            .map(|c| WEIGHTS.iter().find(|(qc, _)| qc == c).unwrap().1)
            .sum()
    };
    let reads = sum(&[
        QueryClass::LookupUuid,
        QueryClass::LookupTitle,
        QueryClass::Traversal2Hop,
        QueryClass::Traversal3Hop,
        QueryClass::Subgraph2,
    ]);
    let search = sum(&[QueryClass::FtsShort, QueryClass::FtsPhrase]);
    let writes = sum(&[
        QueryClass::CreateNode,
        QueryClass::UpdateProps,
        QueryClass::DeleteNode,
    ]);
    assert_eq!((reads, writes, search), (70, 20, 10));
}

#[test]
fn select_class_respects_weight_proportions() {
    // Over many draws, observed frequencies should land near the weights.
    let mut rng = Rng::new(42);
    let mut counts: HashMap<QueryClass, u32> = HashMap::new();
    let draws = 100_000u32;
    for _ in 0..draws {
        *counts.entry(select_class(&mut rng)).or_default() += 1;
    }
    for (class, weight) in WEIGHTS {
        let observed = *counts.get(&class).unwrap_or(&0) as f64 / draws as f64 * 100.0;
        let expected = weight as f64;
        assert!(
            (observed - expected).abs() < 1.5,
            "{}: observed {:.2}% vs expected {:.0}%",
            class.name(),
            observed,
            expected
        );
    }
}

#[test]
fn latency_percentiles_match_known_distribution() {
    let mut stats = LatencyStats::default();
    // Samples 1..=100 ns.
    for n in 1..=100u64 {
        stats.record(n);
    }
    assert_eq!(stats.count(), 100);
    assert_eq!(stats.percentile(50.0), Some(Duration::from_nanos(50)));
    assert_eq!(stats.percentile(95.0), Some(Duration::from_nanos(95)));
    assert_eq!(stats.percentile(99.0), Some(Duration::from_nanos(99)));
    assert_eq!(stats.percentile(100.0), Some(Duration::from_nanos(100)));

    // Empty stats yield None.
    assert_eq!(LatencyStats::default().percentile(50.0), None);
}

#[test]
fn build_corpus_creates_connected_graph() {
    let db = Drevo::open_in_memory().unwrap();
    let mut rng = Rng::new(7);
    let corpus = build_corpus(&db, 50, &mut rng);

    assert_eq!(corpus.ids.len(), 50);
    assert_eq!(corpus.uuids.len(), 50);
    assert_eq!(corpus.titles.len(), 50);

    // Every base node resolves by id, uuid, and title.
    for i in 0..50 {
        assert!(db.get_node(corpus.ids[i]).unwrap().is_some());
        assert!(db.get_node_by_uuid(&corpus.uuids[i]).unwrap().is_some());
        assert!(db.get_node_by_title(&corpus.titles[i]).unwrap().is_some());
    }

    // The +1/+2/+3 fabric gives each node 3 outgoing edges, so a 2-hop BFS
    // reaches strictly more nodes than a 1-hop one.
    let one_hop = db.bfs(corpus.ids[0], 1, Direction::Outgoing, None).unwrap();
    let two_hop = db.bfs(corpus.ids[0], 2, Direction::Outgoing, None).unwrap();
    assert_eq!(one_hop.len(), 3);
    assert!(
        two_hop.len() > one_hop.len(),
        "2-hop must expand the frontier"
    );

    // Titles are unique.
    let mut uniq = corpus.titles.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 50, "base titles must be unique");
}

#[test]
fn short_workload_exercises_every_query_class() {
    let db = Drevo::open_in_memory().unwrap();
    let cfg = WorkloadConfig {
        corpus_size: 300,
        seed: 2026,
        target_qpm: 0, // unthrottled
        max_queries: Some(5_000),
        duration: None,
        db_path: None,
    };
    let report = run_workload(&db, &cfg);
    report.print("short in-memory");

    assert_eq!(report.total_queries, 5_000);

    // Every class was exercised and has computable percentiles.
    for c in QueryClass::ALL {
        let s = report.class(c);
        assert!(s.count() > 0, "class {} never ran", c.name());
        assert!(s.percentile(50.0).is_some());
        assert!(s.percentile(95.0).is_some());
        assert!(s.percentile(99.0).is_some());
    }

    // Sanity: total recorded samples equal total queries.
    let recorded: usize = QueryClass::ALL
        .iter()
        .map(|c| report.class(*c).count())
        .sum();
    assert_eq!(recorded as u64, report.total_queries);

    // FTS classes should find real hits (corpus titles are drawn from WORDS),
    // proving the search path is genuinely exercised, not short-circuited.
    let hits = db.search_fts("alpha", 10).unwrap();
    assert!(!hits.is_empty(), "fts should match seeded corpus words");
}

#[test]
fn duration_bounded_workload_stops_on_time() {
    let db = Drevo::open_in_memory().unwrap();
    let cfg = WorkloadConfig {
        corpus_size: 100,
        seed: 1,
        target_qpm: 0,
        max_queries: None,
        duration: Some(Duration::from_millis(150)),
        db_path: None,
    };
    let report = run_workload(&db, &cfg);
    assert!(report.total_queries > 0, "should run at least some queries");
    assert!(report.elapsed >= Duration::from_millis(150));
    // Generous upper bound: the loop checks the clock every iteration.
    assert!(report.elapsed < Duration::from_secs(10));
}

#[test]
fn on_disk_workload_records_growth_and_rss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workload.db");
    let db = Drevo::open(&path).unwrap();

    // On-disk redb fsyncs per commit, so keep this small — it validates the
    // growth + RSS measurement path, not throughput (the soak does that).
    let cfg = WorkloadConfig {
        corpus_size: 30,
        seed: 99,
        target_qpm: 0,
        max_queries: Some(120),
        duration: None,
        db_path: Some(path.clone()),
    };
    let report = run_workload(&db, &cfg);
    report.print("short on-disk");

    // Growth is measurable on disk (records the delta over the workload loop,
    // post corpus build). create_node outweighs delete_node, so the loop adds
    // net data — growth must be recorded and non-negative.
    let growth = report
        .db_growth_bytes
        .expect("on-disk run must record growth");
    assert!(
        growth >= 0,
        "net-growing workload should not shrink the file: {growth}"
    );
    assert!(file_size(&path).unwrap() > 0, "db file must be non-empty");

    // RSS sampling is best-effort but should succeed on supported CI/dev OSes.
    assert!(
        report.baseline_rss_kb.is_some(),
        "RSS baseline must be sampled"
    );
    assert!(report.peak_rss_kb.unwrap() >= report.baseline_rss_kb.unwrap());
}

#[test]
fn rss_sampling_returns_a_value() {
    let rss = current_rss_kb();
    assert!(rss.is_some(), "RSS must be readable on this platform");
    assert!(rss.unwrap() > 0, "RSS must be positive");
}

// ===========================================================================
// The soak — layer-1 baseline. Ignored by default; nightly CI + on demand.
// ===========================================================================

/// 30+ minute, 10 k-node baseline soak — the number every later workload layer
/// is compared against.
///
/// Run with:
///
/// ```text
/// cargo nextest run --test agentic_workload_rust_api -- --ignored --nocapture
/// ```
///
/// Asserts: completes without panic/deadlock, every query class is exercised,
/// and peak RSS stays within the 20 % envelope of the post-build baseline (the
/// cross-cutting acceptance criterion). The printed report is the deliverable
/// — p50/p95/p99 per class, throughput, RSS, and redb file growth.
#[test]
#[ignore = "soak: 30+ min, 10k-node baseline — run via --ignored in nightly CI / on demand"]
fn rust_api_agentic_workload_soak() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("soak.db");
    let db = Drevo::open(&path).unwrap();

    // Duration is env-overridable so the *same* test serves both the 30-minute
    // default (the acceptance floor) and the roadmap's 8-hour nightly soak
    // (`DREVO_SOAK_SECS=28800`) without a code change. Clamped to ≥ 30 min so
    // the "30+ minute session" criterion can never be silently undercut.
    let soak_secs = std::env::var("DREVO_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(31 * 60)
        .max(30 * 60);

    let cfg = WorkloadConfig {
        corpus_size: 10_000,
        seed: 0xA9E7_1C00,
        target_qpm: 300, // 5 q/s — mid-range of the 200–500 q/min band
        max_queries: None,
        duration: Some(Duration::from_secs(soak_secs)),
        db_path: Some(path.clone()),
    };

    let report = run_workload(&db, &cfg);
    report.print("LAYER 1 — Rust API baseline (soak)");

    // Completed a 30+ minute session.
    assert!(report.elapsed >= Duration::from_secs(30 * 60));

    // Every query class exercised with measurable tail latency.
    for c in QueryClass::ALL {
        let s = report.class(c);
        assert!(s.count() > 0, "class {} never ran", c.name());
        assert!(s.percentile(99.0).is_some());
    }

    // RSS within the 20 % growth envelope (acceptance criterion). Only enforced
    // when RSS is observable; the baseline must be above a noise floor for the
    // ratio to be meaningful.
    if let (Some(baseline), Some(peak)) = (report.baseline_rss_kb, report.peak_rss_kb) {
        if baseline > 4_096 {
            let ratio = peak as f64 / baseline as f64;
            assert!(
                ratio <= 1.20,
                "RSS grew beyond 20% envelope: baseline={baseline}KB peak={peak}KB ratio={ratio:.2}"
            );
        }
    }
}
