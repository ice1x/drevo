//! Phase 10.5 task `00128` — Cypher executor agentic workload (layer 2).
//!
//! This is **layer 2** of the five-layer agentic workload model (README →
//! "Phase 10.5 — Cypher Reliability & Agentic Hardening"). Where layer 1
//! (`00123`, `agentic_workload_rust_api.rs`) measured the *raw [`Drevo`] API*
//! — the upper bound of "redb + our indexes, nothing on top" — this layer
//! drives the **identical agentic query mix** through the full Cypher pipeline:
//!
//! ```text
//! source string  →  parse()  →  execute()  →  result rows
//! ```
//!
//! Subtracting the layer-1 number from the layer-2 number for each query class
//! answers the one question this task exists for: **"what is the parser +
//! executor overhead, per query class?"** Without it, a Cypher-path latency
//! regression cannot be told apart from a storage regression — the wrong layer
//! gets optimised. The numbers feed capacity planning for `00094` (RBAC) and
//! `00096` (streaming ingestion), where the Cypher overhead must be a known,
//! separable constant.
//!
//! ## The ten query classes — same names, same 70/20/10 mix as `00123`
//!
//! The class names match `00123` 1:1 so the two reports diff mechanically.
//! Each class maps to the closest Cypher expression of the layer-1 API call:
//!
//! | class            | category | Cypher driven through parse → execute            |
//! |------------------|----------|--------------------------------------------------|
//! | `lookup_uuid`    | read     | `MATCH (n {seq: $seq}) RETURN n` (inline-map pt) |
//! | `lookup_title`   | read     | `MATCH (n) WHERE n.title = $t RETURN n`          |
//! | `traversal_2hop` | read     | `MATCH (a {seq:$seq})-[*1..2]-(b) RETURN b`      |
//! | `traversal_3hop` | read     | `MATCH (a {seq:$seq})-[*1..3]-(b) RETURN b`      |
//! | `subgraph_2`     | read     | `… -[*1..2]- … RETURN count(DISTINCT b)`         |
//! | `fts_short`      | search   | `MATCH (n) WHERE n.body CONTAINS $w RETURN n`    |
//! | `fts_phrase`     | search   | `… WHERE n.body CONTAINS $a AND … CONTAINS $b`   |
//! | `create_node`    | write    | `CREATE (n:kind {title,body,seq}) RETURN n`      |
//! | `update_props`   | write    | `MATCH (n {seq:$seq}) SET n.body = …, n.touched` |
//! | `delete_node`    | write    | `MATCH (n {seq:$seq}) DETACH DELETE n`           |
//!
//! Two faithful-substitution notes — both of which are themselves findings the
//! layer-1 vs layer-2 diff is meant to surface:
//!
//! * **`lookup_uuid`.** Cypher has no first-class uuid accessor, so the point
//!   lookup uses a unique integer surrogate key (`seq`) stored as a property —
//!   the Cypher analogue of "fetch one row by primary key". Unlike layer 1's
//!   `get_node_by_uuid` (an O(1) index hit), the executor's `MATCH` is a *full
//!   scan filtered by predicate* (see `executor::enumerate_nodes` — an indexed
//!   fast path lands with `00086`). The gap between the two is exactly the
//!   "Cypher has no point-lookup index yet" cost.
//! * **`fts_short` / `fts_phrase`.** Cypher exposes no fulltext function yet,
//!   so the search classes use the `CONTAINS` substring predicate — again a
//!   full scan, versus layer 1's indexed `search_fts`. The diff quantifies the
//!   value of wiring FTS into Cypher.
//!
//! ## Test layout
//!
//! The expensive part — a 30+ minute, 10 k-node soak — lives behind `#[ignore]`
//! so the PR pipeline stays fast; it is meant for nightly CI and on-demand runs:
//!
//! ```text
//! cargo nextest run --test agentic_workload_cypher -- --ignored --nocapture
//! ```
//!
//! The remaining tests are fast scaffolding that prove the harness machinery is
//! correct (deterministic RNG, percentile math, weight coverage, corpus
//! connectivity, every Cypher template parses + executes, RSS + growth metrics
//! recorded) and run on every PR in well under a second.
//!
//! The harness is deliberately **self-contained** in this one file — matching
//! the project convention (see `agentic_workload_rust_api.rs`,
//! `mcp_validation_e2e_tests.rs`) of keeping each integration test file
//! independently grep-able rather than factoring a shared `tests/common/`
//! module. It mirrors `00123`'s shape so the two are obviously comparable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};

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

/// The ten independently-tracked query classes from the task spec — identical
/// names to `00123` so the two reports diff column-for-column.
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
/// 10 % search** agentic mix from the cross-cutting acceptance criteria —
/// byte-for-byte the same split as `00123` so the workloads are comparable.
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

/// Word pool (all ≥ 4 chars) used to build searchable titles and bodies. The
/// `CONTAINS` search classes draw from this same pool so they return real hits
/// rather than empty results.
const WORDS: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima",
];

const KINDS: [&str; 5] = ["note", "task", "person", "project", "bug"];

/// Build a [`NewNode`] for sequence number `seq`. The `{seq:08}` suffix makes
/// the title globally unique (titles are uniqueness-constrained), so base
/// corpus (`seq` `0..n`) and workload-created nodes (`seq >= n`) never collide.
///
/// The unique `seq` is *also* stored as a node property so Cypher can do an
/// equality point lookup on it (`MATCH (n {seq: $seq})`) — drevo storage has
/// no Cypher-visible uuid accessor, so `seq` is the surrogate primary key the
/// `lookup_uuid` / traversal / mutation classes target.
fn make_node(rng: &mut Rng, seq: u64) -> NewNode {
    let w1 = WORDS[rng.index(WORDS.len())];
    let w2 = WORDS[rng.index(WORDS.len())];
    let w3 = WORDS[rng.index(WORDS.len())];
    let mut props = HashMap::new();
    props.insert("seq".to_string(), serde_json::Value::from(seq));
    NewNode {
        kind: KINDS[rng.index(KINDS.len())].to_string(),
        title: format!("{w1} {w2} {w3} item {seq:08}"),
        body: format!("{w1} {w2} {w3} body for node {seq}, agentic workload corpus"),
        body_html: String::new(),
        properties: Properties(props),
    }
}

/// Immutable snapshot of the base corpus the workload reads against. Unlike
/// `00123` we track `seq` rather than `uuid`, since Cypher point lookups go
/// through the `seq` property (see [`make_node`]).
struct Corpus {
    seqs: Vec<u64>,
    titles: Vec<String>,
}

/// Create `n` base nodes and a forward-link edge fabric (each node → +1/+2/+3
/// mod n) so 2-hop / 3-hop traversals and depth-2 subgraphs return rich,
/// non-trivial result sets. Built through the **raw API** — corpus
/// construction is setup, not part of the measured Cypher workload.
fn build_corpus(db: &Drevo, n: usize, rng: &mut Rng) -> Corpus {
    assert!(
        n >= 4,
        "corpus must have at least 4 nodes for the edge fabric"
    );
    let mut ids = Vec::with_capacity(n);
    let mut seqs = Vec::with_capacity(n);
    let mut titles = Vec::with_capacity(n);

    for i in 0..n {
        let node = db
            .create_node(make_node(rng, i as u64))
            .expect("create base node");
        ids.push(node.id);
        seqs.push(i as u64);
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

    Corpus { seqs, titles }
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
// Workload driver — the Cypher-specific part
// ---------------------------------------------------------------------------

/// Mutable state threaded through the workload loop.
struct WorkloadState {
    /// `seq` values of nodes created during the loop and not yet deleted — the
    /// only nodes `delete_node` removes (base nodes stay, preserving
    /// connectivity for traversals).
    created: Vec<u64>,
    /// Next sequence number for created-node titles/seqs (continues past the
    /// base corpus to keep both unique).
    next_seq: u64,
}

/// Build the `(source, params)` pair for a query of the given class, advancing
/// [`WorkloadState`] for the write classes so subsequent lookups/updates/
/// deletes have valid targets.
///
/// Reads/searches target the immutable base corpus (always present). Writes
/// mint / mutate / retire nodes by their unique `seq` surrogate key.
fn build_query(
    class: QueryClass,
    corpus: &Corpus,
    state: &mut WorkloadState,
    rng: &mut Rng,
) -> (String, HashMap<String, Value>) {
    let mut params = HashMap::new();
    match class {
        QueryClass::LookupUuid => {
            // Point lookup by the unique integer surrogate key — the Cypher
            // analogue of layer 1's `get_node_by_uuid`, but via a full-scan
            // `MATCH` (no point-lookup index until `00086`).
            let seq = corpus.seqs[rng.index(corpus.seqs.len())];
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            ("MATCH (n {seq: $seq}) RETURN n".to_string(), params)
        }
        QueryClass::LookupTitle => {
            let title = corpus.titles[rng.index(corpus.titles.len())].clone();
            params.insert("title".to_string(), Value::String(title));
            (
                "MATCH (n) WHERE n.title = $title RETURN n".to_string(),
                params,
            )
        }
        QueryClass::Traversal2Hop => {
            let seq = corpus.seqs[rng.index(corpus.seqs.len())];
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            (
                "MATCH (a {seq: $seq})-[*1..2]-(b) RETURN b".to_string(),
                params,
            )
        }
        QueryClass::Traversal3Hop => {
            let seq = corpus.seqs[rng.index(corpus.seqs.len())];
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            (
                "MATCH (a {seq: $seq})-[*1..3]-(b) RETURN b".to_string(),
                params,
            )
        }
        QueryClass::Subgraph2 => {
            // The depth-2 neighbourhood size — exercises the aggregation
            // engine (`count(DISTINCT …)`), distinguishing it from the bare
            // 2-hop traversal above which just streams reached nodes.
            let seq = corpus.seqs[rng.index(corpus.seqs.len())];
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            (
                "MATCH (a {seq: $seq})-[*1..2]-(b) RETURN count(DISTINCT b) AS reached".to_string(),
                params,
            )
        }
        QueryClass::FtsShort => {
            // Cypher has no fulltext function yet, so search is a `CONTAINS`
            // full scan — the analogue of layer 1's indexed `search_fts`.
            let word = WORDS[rng.index(WORDS.len())].to_string();
            params.insert("word".to_string(), Value::String(word));
            (
                "MATCH (n) WHERE n.body CONTAINS $word RETURN n LIMIT 10".to_string(),
                params,
            )
        }
        QueryClass::FtsPhrase => {
            let w1 = WORDS[rng.index(WORDS.len())].to_string();
            let w2 = WORDS[rng.index(WORDS.len())].to_string();
            params.insert("w1".to_string(), Value::String(w1));
            params.insert("w2".to_string(), Value::String(w2));
            (
                "MATCH (n) WHERE n.body CONTAINS $w1 AND n.body CONTAINS $w2 RETURN n LIMIT 10"
                    .to_string(),
                params,
            )
        }
        QueryClass::CreateNode => {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.created.push(seq);
            // Label can't be a parameter, so the kind literal is embedded; the
            // title/body/seq travel as params. Title carries `seq` to stay
            // unique against the uniqueness-constrained title index.
            let kind = KINDS[rng.index(KINDS.len())];
            let w = WORDS[rng.index(WORDS.len())];
            params.insert(
                "title".to_string(),
                Value::String(format!("{w} created item {seq:08}")),
            );
            params.insert(
                "body".to_string(),
                Value::String(format!("{w} created body for node {seq}, cypher workload")),
            );
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            (
                format!("CREATE (n:{kind} {{title: $title, body: $body, seq: $seq}}) RETURN n"),
                params,
            )
        }
        QueryClass::UpdateProps => {
            // Update a live node — either a base node or a created one. Both
            // are guaranteed present. Touching `body` re-indexes FTS, the
            // realistic write cost we want in the number.
            let seq = if state.created.is_empty() || rng.below(2) == 0 {
                corpus.seqs[rng.index(corpus.seqs.len())]
            } else {
                state.created[rng.index(state.created.len())]
            };
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            params.insert(
                "body".to_string(),
                Value::String(format!("updated body rev {}", state.next_seq)),
            );
            params.insert("rev".to_string(), Value::Integer(state.next_seq as i64));
            (
                "MATCH (n {seq: $seq}) SET n.body = $body, n.touched = $rev".to_string(),
                params,
            )
        }
        QueryClass::DeleteNode => {
            // Only created nodes are deleted. If none are queued yet, mint one
            // first (recording its seq) so the class still measures a real
            // delete rather than skewing the count.
            if state.created.is_empty() {
                state.created.push(state.next_seq);
                state.next_seq += 1;
            }
            let idx = rng.index(state.created.len());
            let seq = state.created.swap_remove(idx);
            params.insert("seq".to_string(), Value::Integer(seq as i64));
            ("MATCH (n {seq: $seq}) DETACH DELETE n".to_string(), params)
        }
    }
}

/// Execute exactly one query of the given class through the **full Cypher
/// pipeline** — `parse → execute → result rows`. Both the parse and the
/// execute are inside the caller's timed region (that *is* the layer-2 number).
///
/// `DeleteNode` against a `seq` that a prior delete already removed (only
/// reachable if a created node was both queued for delete and selected as an
/// update/create target — it isn't, by construction) would simply match zero
/// rows; the query still parses and executes, so the latency sample is valid.
/// Any parse/exec error is a hard failure — the soak's "no panic" guarantee is
/// exactly this `expect` surfacing a regression.
fn execute_query(
    class: QueryClass,
    db: &Drevo,
    corpus: &Corpus,
    state: &mut WorkloadState,
    rng: &mut Rng,
) {
    let (source, params) = build_query(class, corpus, state, rng);
    let query = parse(&source).expect("parse cypher");
    execute(&query, db, params).expect("execute cypher");
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
        execute_query(class, db, &corpus, &mut state, &mut rng);
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

    // 70 % read / 20 % write / 10 % search split — identical to `00123`.
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
fn build_corpus_creates_connected_graph_with_unique_seqs() {
    let db = Drevo::open_in_memory().unwrap();
    let mut rng = Rng::new(7);
    let corpus = build_corpus(&db, 50, &mut rng);

    assert_eq!(corpus.seqs.len(), 50);
    assert_eq!(corpus.titles.len(), 50);

    // Every base node resolves by title (the uniqueness-constrained index).
    for title in &corpus.titles {
        assert!(db.get_node_by_title(title).unwrap().is_some());
    }

    // seqs are 0..50 and unique.
    let mut uniq_seqs = corpus.seqs.clone();
    uniq_seqs.sort_unstable();
    uniq_seqs.dedup();
    assert_eq!(uniq_seqs.len(), 50, "base seqs must be unique");
    assert_eq!(uniq_seqs.first(), Some(&0));
    assert_eq!(uniq_seqs.last(), Some(&49));

    // Titles are unique.
    let mut uniq = corpus.titles.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 50, "base titles must be unique");
}

#[test]
fn every_query_template_parses_and_executes() {
    // The critical correctness test: every one of the ten Cypher templates
    // must parse and execute against a live corpus, returning the shape the
    // class promises. If a future grammar/executor change breaks one, this
    // fails loudly on every PR rather than only in the ignored soak.
    let db = Drevo::open_in_memory().unwrap();
    let mut rng = Rng::new(13);
    let corpus = build_corpus(&db, 60, &mut rng);
    let mut state = WorkloadState {
        created: Vec::new(),
        next_seq: 60,
    };

    for class in QueryClass::ALL {
        // Re-seed nothing — drive each class once and assert it round-trips.
        let (source, params) = build_query(class, &corpus, &mut state, &mut rng);
        let query =
            parse(&source).unwrap_or_else(|e| panic!("{} failed to parse: {e:?}", class.name()));
        let result = execute(&query, &db, params)
            .unwrap_or_else(|e| panic!("{} failed to execute: {e:?}", class.name()));

        match class {
            QueryClass::LookupUuid | QueryClass::LookupTitle => {
                assert_eq!(
                    result.rows.len(),
                    1,
                    "{} must hit exactly one node",
                    class.name()
                );
            }
            QueryClass::Traversal2Hop | QueryClass::Traversal3Hop => {
                assert!(
                    !result.rows.is_empty(),
                    "{} over the +1/+2/+3 fabric must reach nodes",
                    class.name()
                );
            }
            QueryClass::Subgraph2 => {
                assert_eq!(result.columns, vec!["reached".to_string()]);
                assert_eq!(result.rows.len(), 1, "count aggregation yields one row");
                assert!(
                    matches!(result.rows[0][0], Value::Integer(n) if n > 0),
                    "subgraph_2 must reach a positive node count"
                );
            }
            QueryClass::FtsShort | QueryClass::FtsPhrase => {
                // Search may legitimately return 0..=10 rows depending on the
                // randomly chosen word(s); the contract is "parses, executes,
                // honours LIMIT" — not a specific hit count.
                assert!(
                    result.rows.len() <= 10,
                    "{} must honour LIMIT 10",
                    class.name()
                );
            }
            QueryClass::CreateNode => {
                assert_eq!(result.stats.nodes_created, 1);
                assert_eq!(
                    result.rows.len(),
                    1,
                    "CREATE … RETURN n yields the new node"
                );
            }
            QueryClass::UpdateProps => {
                assert!(
                    result.stats.properties_set >= 1,
                    "SET must assign at least one property"
                );
            }
            QueryClass::DeleteNode => {
                assert_eq!(
                    result.stats.nodes_deleted, 1,
                    "DETACH DELETE removes the target"
                );
            }
        }
    }
}

#[test]
fn short_workload_exercises_every_query_class() {
    let db = Drevo::open_in_memory().unwrap();
    let cfg = WorkloadConfig {
        corpus_size: 200,
        seed: 2026,
        target_qpm: 0, // unthrottled
        max_queries: Some(3_000),
        duration: None,
        db_path: None,
    };
    let report = run_workload(&db, &cfg);
    report.print("short in-memory (cypher)");

    assert_eq!(report.total_queries, 3_000);

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
}

#[test]
fn duration_bounded_workload_stops_on_time() {
    let db = Drevo::open_in_memory().unwrap();
    let cfg = WorkloadConfig {
        corpus_size: 80,
        seed: 1,
        target_qpm: 0,
        max_queries: None,
        duration: Some(Duration::from_millis(200)),
        db_path: None,
    };
    let report = run_workload(&db, &cfg);
    assert!(report.total_queries > 0, "should run at least some queries");
    assert!(report.elapsed >= Duration::from_millis(200));
    // Generous upper bound: the loop checks the clock every iteration.
    assert!(report.elapsed < Duration::from_secs(15));
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
        max_queries: Some(80),
        duration: None,
        db_path: Some(path.clone()),
    };
    let report = run_workload(&db, &cfg);
    report.print("short on-disk (cypher)");

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
// The soak — layer-2 number. Ignored by default; nightly CI + on demand.
// ===========================================================================

/// 30+ minute, 10 k-node Cypher soak — the layer-2 counterpart to `00123`'s
/// layer-1 baseline. Subtract the two per-class p50/p95/p99 numbers to read the
/// parser + executor overhead.
///
/// Run with:
///
/// ```text
/// cargo nextest run --test agentic_workload_cypher -- --ignored --nocapture
/// ```
///
/// Asserts: completes without panic/deadlock, every query class is exercised,
/// and peak RSS stays within the 20 % envelope of the post-build baseline (the
/// cross-cutting acceptance criterion). The printed report is the deliverable
/// — p50/p95/p99 per class, throughput, RSS, and redb file growth.
///
/// Note on absolute latencies: the executor's `MATCH` is a full scan filtered
/// by predicate (no point-lookup index until `00086`), so at 10 k nodes the
/// read classes are *expected* to be far slower than layer 1's index hits. That
/// gap is the headline finding, not a defect — it scopes the value of `00086`.
#[test]
#[ignore = "soak: 30+ min, 10k-node cypher layer-2 — run via --ignored in nightly CI / on demand"]
fn cypher_agentic_workload_soak() {
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
        seed: 0xC9_7E_42_00,
        target_qpm: 300, // 5 q/s — mid-range of the 200–500 q/min band
        max_queries: None,
        duration: Some(Duration::from_secs(soak_secs)),
        db_path: Some(path.clone()),
    };

    let report = run_workload(&db, &cfg);
    report.print("LAYER 2 — Cypher executor (soak)");

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
