//! Phase 10.5 task `00125` — Concurrency suite (cross-layer).
//!
//! This harness answers a single architectural question: **how does drevo's
//! storage layer behave when many independent agents hammer one shared
//! [`Drevo`] handle at the same time?** The redb backend is *single-writer +
//! many-readers* — every write takes an exclusive `WriteTransaction` while any
//! number of `begin_read` snapshots overlap freely. The in-memory backend
//! mirrors that shape with an `RwLock` (Phase 13 task `00080`). Contention is
//! therefore unavoidable; the open question is whether it manifests as
//! *acceptable blocking* or as *deadlock / starvation / torn reads*. This
//! suite forces the issue and asserts the former.
//!
//! ## The agent mix
//!
//! A run spawns a fixed cast against one `Arc<Drevo>`:
//!
//! | role     | count | behaviour                                             |
//! |----------|-------|-------------------------------------------------------|
//! | writer   | 2     | bump the generation of *its own* invariant slots,     |
//! |          |       | plus churn (create then delete) throwaway nodes       |
//! | reader   | 4     | `get_node` / `get_node_by_title` / `bfs` / `search_fts`|
//! | mixed    | 2     | mostly reads, occasional invariant write of its slots |
//!
//! Each agent loops until a shared stop flag flips, then reports its tallies
//! over an `mpsc` channel.
//!
//! ## The four invariants the suite proves
//!
//! 1. **No deadlocks.** Every agent's report must arrive within a grace window
//!    after the stop flag flips. A missing report ⇒ a hung thread ⇒ the
//!    [`recv_timeout`](std::sync::mpsc::Receiver::recv_timeout) fires and the
//!    test fails loudly rather than hanging CI forever.
//! 2. **No panics.** An agent that panics never sends its report (caught by #1)
//!    and its `JoinHandle` surfaces the panic on `join`.
//! 3. **No torn reads + FTS stays consistent.** Every invariant node carries a
//!    self-describing `(generation, checksum)` pair in its body and properties.
//!    `update_node` writes the whole node in one backend `put`, so a reader can
//!    only ever observe a *self-consistent* triple — a torn read would surface
//!    a checksum mismatch. After the run, the FTS index is cross-checked
//!    against every slot's final generation: searching the final token must
//!    return the node, and the node must still satisfy its checksum.
//! 4. **Write-starvation is bounded.** Writers run on a single-writer backend,
//!    so they serialise — but each must still make forward progress. The suite
//!    asserts every writer committed at least one write and that the *worst*
//!    single-write latency stayed under a generous ceiling (no unbounded
//!    blocking).
//!
//! ## Why `std::thread`, not `tokio` tasks
//!
//! The roadmap sketch named "tokio tasks", but every `Drevo` method is
//! synchronous and *blocking* (it parks on the backend's `RwLock` / redb
//! transaction lock). Driving blocking work from async tasks would demand
//! `spawn_blocking` on every call and buy nothing — the contention we are
//! testing lives entirely below the async boundary. Plain OS threads model the
//! real "N concurrent agents" topology directly, match the established project
//! idiom (`tests/read_write_separation_tests.rs`), and keep this suite free of
//! the `http`/`tokio` feature gate so it also runs under
//! `--no-default-features --features redb-backend`.
//!
//! ## Test layout
//!
//! The 5-minute soak lives behind `#[ignore]` so the PR pipeline stays fast; it
//! is meant for nightly CI and on-demand runs:
//!
//! ```text
//! cargo nextest run --test concurrent_agents -- --ignored --nocapture
//! ```
//!
//! The remaining tests are fast scaffolding (sub-second) that prove the harness
//! machinery itself is sound — every role makes progress, the checksum detector
//! actually catches a corrupted node, the deadlock window is honoured, FTS is
//! reconciled, and both backends survive a short burst.
//!
//! The harness is deliberately **self-contained** in this one file, matching
//! the project convention (see `agentic_workload_rust_api.rs`,
//! `mcp_validation_e2e_tests.rs`) of keeping each integration test
//! independently grep-able rather than factoring a shared `tests/common/`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, NodePatch, Properties};

// ---------------------------------------------------------------------------
// Deterministic RNG
// ---------------------------------------------------------------------------

/// Tiny deterministic xorshift64 PRNG — mirrors the one in
/// `agentic_workload_rust_api.rs`. Each agent seeds its own instance from a
/// base seed plus its agent index so a failure at a fixed `(seed, agent)` is
/// reproducible without dragging in a `rand` dev-dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift64 has a fixed point at 0; force a non-zero state.
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

    /// Uniform-ish integer in `[0, n)`. `n == 0` returns 0.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant encoding — the heart of the torn-read / FTS-consistency check
// ---------------------------------------------------------------------------

/// A short, FTS-tokenizable word that is unique to a `(slot, generation)`
/// pair. Writers embed it in the node body; the final-state check searches for
/// it. Kept lowercase-alphanumeric so the trigram tokenizer indexes it cleanly.
fn gen_token(slot: u64, generation: u64) -> String {
    format!("invtok{slot:04}g{generation:06}")
}

/// Order-independent checksum binding a slot to its generation. Stored in the
/// node's `properties["checksum"]` and recomputed on read; a mismatch means
/// the reader observed a body/property pair that never existed together —
/// i.e. a torn read.
fn checksum(slot: u64, generation: u64) -> u64 {
    // FNV-1a over the two little-endian u64s — cheap and dependency-free.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in slot
        .to_le_bytes()
        .into_iter()
        .chain(generation.to_le_bytes())
    {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Build the `(body, properties)` payload for a given slot+generation so the
/// writer and the checker agree byte-for-byte on the encoding.
fn invariant_patch(slot: u64, generation: u64) -> NodePatch {
    let mut props = HashMap::new();
    props.insert("slot".to_string(), serde_json::json!(slot));
    props.insert("gen".to_string(), serde_json::json!(generation));
    props.insert(
        "checksum".to_string(),
        serde_json::json!(checksum(slot, generation)),
    );
    NodePatch {
        kind: None,
        title: None,
        body: Some(format!(
            "invariant slot {slot} {}",
            gen_token(slot, generation)
        )),
        body_html: None,
        properties: Some(Properties(props)),
    }
}

/// Pull `(slot, gen, checksum)` out of a node's properties, returning `None` if
/// any field is missing or the wrong type.
fn read_invariant(props: &Properties) -> Option<(u64, u64, u64)> {
    let slot = props.0.get("slot")?.as_u64()?;
    let generation = props.0.get("gen")?.as_u64()?;
    let cksum = props.0.get("checksum")?.as_u64()?;
    Some((slot, generation, cksum))
}

/// Verify a node read mid-flight is internally consistent: the stored checksum
/// must equal `checksum(slot, gen)` recomputed from the stored slot+gen, and
/// the body must contain the matching token. Returns `Err(reason)` on a torn
/// read so the caller can fail with a precise message.
fn check_node_consistent(node: &drevo::model::Node) -> Result<(), String> {
    let (slot, generation, cksum) = read_invariant(&node.properties)
        .ok_or_else(|| format!("node {} missing invariant properties", node.id))?;
    let expected = checksum(slot, generation);
    if cksum != expected {
        return Err(format!(
            "TORN READ on node {}: slot={slot} gen={generation} stored_checksum={cksum} expected={expected}",
            node.id
        ));
    }
    let token = gen_token(slot, generation);
    if !node.body.contains(&token) {
        return Err(format!(
            "TORN READ on node {}: body {:?} missing token {token}",
            node.id, node.body
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Configuration + reports
// ---------------------------------------------------------------------------

/// Inputs to a single concurrency run.
#[derive(Debug, Clone)]
struct ConcurrencyConfig {
    /// Number of fixed invariant nodes, partitioned across the writer-capable
    /// agents. Each slot has exactly one owner so its generation stays
    /// monotone and its final token is deterministic.
    invariant_nodes: u64,
    /// Number of pure-writer agents.
    writers: usize,
    /// Number of pure-reader agents.
    readers: usize,
    /// Number of mixed (read-mostly, occasional write) agents.
    mixed: usize,
    /// How long agents loop before the stop flag flips.
    duration: Duration,
    /// Grace window for every agent's final report to arrive after the stop
    /// flag flips. Exceeding it is reported as a deadlock.
    join_grace: Duration,
    /// Ceiling on the worst single-write latency. Exceeding it is reported as
    /// unbounded write-starvation.
    starvation_ceiling: Duration,
    /// Base RNG seed.
    seed: u64,
}

impl ConcurrencyConfig {
    /// The default 5-minute soak shape from the task spec: 2 writers + 4
    /// readers + 2 mixed.
    fn soak(duration: Duration) -> Self {
        Self {
            invariant_nodes: 256,
            writers: 2,
            readers: 4,
            mixed: 2,
            duration,
            join_grace: Duration::from_secs(30),
            starvation_ceiling: Duration::from_secs(30),
            seed: 0xC0FF_EE00,
        }
    }

    /// Total number of agents that ever write invariant slots (writers +
    /// mixed). Slots are striped across these by `slot % write_agents`.
    fn write_agents(&self) -> usize {
        self.writers + self.mixed
    }
}

/// What one agent did over the run.
#[derive(Debug, Clone, Default)]
struct AgentReport {
    label: String,
    reads: u64,
    writes: u64,
    creates: u64,
    deletes: u64,
    searches: u64,
    traversals: u64,
    /// Worst single successful `update_node` / write latency this agent saw.
    max_write_latency: Duration,
}

/// Aggregate outcome of a run.
#[derive(Debug, Default)]
struct ConcurrencyReport {
    agents: Vec<AgentReport>,
    elapsed: Duration,
    /// Set when an agent failed to report within the grace window.
    deadlock: Option<String>,
}

impl ConcurrencyReport {
    fn total_writes(&self) -> u64 {
        self.agents.iter().map(|a| a.writes).sum()
    }
    fn total_reads(&self) -> u64 {
        self.agents.iter().map(|a| a.reads).sum()
    }
    fn total_searches(&self) -> u64 {
        self.agents.iter().map(|a| a.searches).sum()
    }
    fn total_traversals(&self) -> u64 {
        self.agents.iter().map(|a| a.traversals).sum()
    }
    fn total_creates(&self) -> u64 {
        self.agents.iter().map(|a| a.creates).sum()
    }
    fn total_deletes(&self) -> u64 {
        self.agents.iter().map(|a| a.deletes).sum()
    }
    /// Worst single-write latency across all writer-capable agents.
    fn worst_write_latency(&self) -> Duration {
        self.agents
            .iter()
            .map(|a| a.max_write_latency)
            .max()
            .unwrap_or_default()
    }

    fn print(&self, title: &str) {
        println!("\n=== {title} ===");
        println!("elapsed: {:.2?}", self.elapsed);
        println!(
            "totals: reads={} writes={} creates={} deletes={} searches={} traversals={}",
            self.total_reads(),
            self.total_writes(),
            self.total_creates(),
            self.total_deletes(),
            self.total_searches(),
            self.total_traversals(),
        );
        println!(
            "worst single-write latency: {:.2?}",
            self.worst_write_latency()
        );
        for a in &self.agents {
            println!(
                "  {:<10} reads={:<8} writes={:<6} creates={:<6} deletes={:<6} searches={:<6} traversals={:<6} max_write={:.2?}",
                a.label, a.reads, a.writes, a.creates, a.deletes, a.searches, a.traversals, a.max_write_latency
            );
        }
        if let Some(d) = &self.deadlock {
            println!("DEADLOCK: {d}");
        }
    }
}

// ---------------------------------------------------------------------------
// Corpus seeding
// ---------------------------------------------------------------------------

/// Seed the invariant corpus: `invariant_nodes` nodes titled `inv:{slot}`,
/// each at generation 0, wired into a ring + chords so traversal agents have a
/// connected graph to walk. Returns the slot→node-id map.
fn seed_corpus(db: &Drevo, cfg: &ConcurrencyConfig) -> Vec<u64> {
    let mut ids = Vec::with_capacity(cfg.invariant_nodes as usize);
    for slot in 0..cfg.invariant_nodes {
        let mut props = HashMap::new();
        props.insert("slot".to_string(), serde_json::json!(slot));
        props.insert("gen".to_string(), serde_json::json!(0u64));
        props.insert("checksum".to_string(), serde_json::json!(checksum(slot, 0)));
        let node = db
            .create_node(NewNode {
                kind: "invariant".to_string(),
                title: format!("inv:{slot}"),
                body: format!("invariant slot {slot} {}", gen_token(slot, 0)),
                body_html: String::new(),
                properties: Properties(props),
            })
            .expect("seed invariant node");
        ids.push(node.id);
    }
    // Connect into a ring with +7 chords so bfs depth 2-3 returns several
    // nodes regardless of where a reader starts.
    let n = ids.len();
    for i in 0..n {
        let a = ids[i];
        let b = ids[(i + 1) % n];
        let c = ids[(i + 7) % n];
        db.create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "next".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .expect("seed ring edge");
        db.create_edge(NewEdge {
            from_id: a,
            to_id: c,
            kind: "chord".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .expect("seed chord edge");
    }
    ids
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Shared mutable state threaded through every agent.
struct Shared {
    db: Arc<Drevo>,
    /// slot → node id (immutable after seeding).
    slot_ids: Vec<u64>,
    /// slot → latest committed generation. Written only by the slot's single
    /// owner, read by the final-state checker. Indexed by slot.
    slot_gen: Vec<AtomicU64>,
    /// Flips to `true` when the run's clock expires; every agent loop watches it.
    stop: AtomicBool,
    /// Monotonic source of unique churn-node titles across all agents.
    churn_seq: AtomicU64,
}

/// One pure-writer / mixed-writer iteration: bump one of *this agent's* slots,
/// returning the write latency. `write_idx` is the agent's index among
/// write-capable agents; it owns slots where `slot % write_agents == write_idx`.
fn do_invariant_write(
    shared: &Shared,
    rng: &mut Rng,
    write_idx: usize,
    write_agents: usize,
) -> Option<Duration> {
    let n = shared.slot_ids.len() as u64;
    if n == 0 {
        return None;
    }
    // Pick one of this agent's owned slots.
    let owned: Vec<u64> = (0..n)
        .filter(|s| (*s as usize) % write_agents == write_idx)
        .collect();
    if owned.is_empty() {
        return None;
    }
    let slot = owned[rng.below(owned.len() as u64) as usize];
    let next_gen = shared.slot_gen[slot as usize].load(Ordering::Acquire) + 1;
    let id = shared.slot_ids[slot as usize];

    let t0 = Instant::now();
    shared
        .db
        .update_node(id, invariant_patch(slot, next_gen))
        .expect("invariant update_node must succeed");
    let lat = t0.elapsed();

    // Publish the new generation only *after* the write committed, so the
    // final-state checker never looks for a token that was never written.
    shared.slot_gen[slot as usize].store(next_gen, Ordering::Release);
    Some(lat)
}

/// One churn iteration: create a throwaway node, then immediately delete it.
/// Stresses index insert/remove churn (title, kind, FTS) concurrently with the
/// invariant updates. Returns the write latency of the create.
fn do_churn(shared: &Shared, label: &str) -> Duration {
    let seq = shared.churn_seq.fetch_add(1, Ordering::Relaxed);
    let title = format!("churn:{label}:{seq}");
    let t0 = Instant::now();
    let node = shared
        .db
        .create_node(NewNode {
            kind: "churn".to_string(),
            title,
            body: format!("ephemeral churn body {seq} needle{seq}"),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .expect("churn create_node must succeed");
    let lat = t0.elapsed();
    shared
        .db
        .delete_node(node.id)
        .expect("churn delete_node must succeed");
    lat
}

/// Reader iteration: hit a slot by id and by title, walk a short bfs, and run
/// an FTS search. Every invariant node read is checksum-verified; a torn read
/// panics with a precise message (surfaced by the agent's `JoinHandle`).
fn do_read(report: &mut AgentReport, shared: &Shared, rng: &mut Rng) {
    let n = shared.slot_ids.len() as u64;
    if n == 0 {
        return;
    }
    let slot = rng.below(n);
    let id = shared.slot_ids[slot as usize];

    // by id
    if let Some(node) = shared.db.get_node(id).expect("get_node must not error") {
        if let Err(e) = check_node_consistent(&node) {
            panic!("{e}");
        }
        report.reads += 1;
    }
    // by title
    if let Some(node) = shared
        .db
        .get_node_by_title(&format!("inv:{slot}"))
        .expect("get_node_by_title must not error")
    {
        if let Err(e) = check_node_consistent(&node) {
            panic!("{e}");
        }
        report.reads += 1;
    }

    // traversal
    let depth = 2 + (rng.below(2) as u8); // depth 2 or 3
    let hops = shared
        .db
        .bfs(id, depth, Direction::Outgoing, None)
        .expect("bfs must not error");
    for node in &hops {
        // Traversed invariant nodes must also be self-consistent.
        if node.kind == "invariant" {
            if let Err(e) = check_node_consistent(node) {
                panic!("{e}");
            }
        }
    }
    report.traversals += 1;

    // FTS — search for a stable substring present in every invariant body.
    let _ = shared
        .db
        .search_fts("invariant", 8)
        .expect("search_fts must not error");
    report.searches += 1;
}

/// Spawn one agent thread. `kind` selects the behaviour; `write_idx` is only
/// meaningful for writer/mixed agents (the slot-partition index).
#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    shared: Arc<Shared>,
    tx: mpsc::Sender<AgentReport>,
    label: String,
    kind: AgentKind,
    write_idx: usize,
    write_agents: usize,
    seed: u64,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut rng = Rng::new(seed);
        let mut report = AgentReport {
            label: label.clone(),
            ..Default::default()
        };

        while !shared.stop.load(Ordering::Relaxed) {
            match kind {
                AgentKind::Writer => {
                    if let Some(lat) =
                        do_invariant_write(&shared, &mut rng, write_idx, write_agents)
                    {
                        report.writes += 1;
                        report.max_write_latency = report.max_write_latency.max(lat);
                    }
                    // Every few invariant writes, churn a throwaway node.
                    if rng.below(4) == 0 {
                        let lat = do_churn(&shared, &label);
                        report.creates += 1;
                        report.deletes += 1;
                        report.max_write_latency = report.max_write_latency.max(lat);
                    }
                }
                AgentKind::Reader => {
                    do_read(&mut report, &shared, &mut rng);
                }
                AgentKind::Mixed => {
                    // ~20% writes, 80% reads.
                    if rng.below(5) == 0 {
                        if let Some(lat) =
                            do_invariant_write(&shared, &mut rng, write_idx, write_agents)
                        {
                            report.writes += 1;
                            report.max_write_latency = report.max_write_latency.max(lat);
                        }
                    } else {
                        do_read(&mut report, &shared, &mut rng);
                    }
                }
            }
        }

        // Report send is the liveness signal the deadlock detector waits on.
        let _ = tx.send(report);
    })
}

#[derive(Debug, Clone, Copy)]
enum AgentKind {
    Writer,
    Reader,
    Mixed,
}

/// Seed the corpus and wrap it in the shared state every agent reads. Kept
/// separate from [`drive`] so callers that need the post-run generation map
/// (the final-state checker) can hold onto the `Arc<Shared>`.
fn seed_shared(db: Arc<Drevo>, cfg: &ConcurrencyConfig) -> Arc<Shared> {
    let slot_ids = seed_corpus(&db, cfg);
    let slot_gen = (0..slot_ids.len()).map(|_| AtomicU64::new(0)).collect();
    Arc::new(Shared {
        db,
        slot_ids,
        slot_gen,
        stop: AtomicBool::new(false),
        churn_seq: AtomicU64::new(0),
    })
}

/// Run the full concurrent workload and return the aggregated report. The
/// caller is responsible for the final-state FTS / consistency cross-check via
/// [`verify_final_state`] (use [`run_and_verify`] to get both at once).
fn run_concurrent(db: Arc<Drevo>, cfg: &ConcurrencyConfig) -> ConcurrencyReport {
    drive(seed_shared(db, cfg), cfg)
}

/// After a run, cross-check the FTS index and node store against every slot's
/// final generation. Returns `Err(reason)` on the first inconsistency.
fn verify_final_state(
    db: &Drevo,
    shared_gen: &[AtomicU64],
    slot_ids: &[u64],
) -> Result<(), String> {
    for (slot, id) in slot_ids.iter().enumerate() {
        let generation = shared_gen[slot].load(Ordering::Acquire);
        let node = db
            .get_node(*id)
            .map_err(|e| format!("final get_node({id}) errored: {e}"))?
            .ok_or_else(|| format!("invariant node {id} (slot {slot}) vanished"))?;

        // Stored state must satisfy its own checksum.
        check_node_consistent(&node)?;

        // ...and must be at the generation we last published.
        let (got_slot, got_gen, _) = read_invariant(&node.properties)
            .ok_or_else(|| format!("node {id} lost invariant props"))?;
        if got_slot as usize != slot {
            return Err(format!(
                "node {id} slot drift: stored {got_slot} expected {slot}"
            ));
        }
        if got_gen != generation {
            return Err(format!(
                "node {id} (slot {slot}) generation drift: stored {got_gen} expected {generation}"
            ));
        }

        // The FTS index must resolve the *final* token to this node — proves
        // the index tracked the last committed body, not a stale one.
        let token = gen_token(slot as u64, generation);
        let hits = db
            .search_fts(&token, 16)
            .map_err(|e| format!("search_fts({token}) errored: {e}"))?;
        if !hits.iter().any(|h| h.node.id == *id) {
            return Err(format!(
                "FTS inconsistency: final token {token} for slot {slot} does not resolve to node {id} (hits: {:?})",
                hits.iter().map(|h| h.node.id).collect::<Vec<_>>()
            ));
        }
    }
    Ok(())
}

/// Convenience: run a workload and immediately verify final state, returning
/// both the report and the verification result. Holds onto the `Arc<Shared>`
/// produced by [`seed_shared`] so the post-run generation map is available for
/// the cross-check.
fn run_and_verify(
    db: Arc<Drevo>,
    cfg: &ConcurrencyConfig,
) -> (ConcurrencyReport, Result<(), String>) {
    // Hold onto `Shared` so we can read the post-run generation map for the
    // final-state cross-check.
    let shared = seed_shared(Arc::clone(&db), cfg);
    let report = drive(Arc::clone(&shared), cfg);
    let verify = if report.deadlock.is_none() {
        verify_final_state(&db, &shared.slot_gen, &shared.slot_ids)
    } else {
        Ok(())
    };
    (report, verify)
}

/// Core driver shared by [`run_concurrent`] and [`run_and_verify`]: spawns the
/// agent cast against an already-seeded [`Shared`], runs for `cfg.duration`,
/// and aggregates reports with deadlock detection.
fn drive(shared: Arc<Shared>, cfg: &ConcurrencyConfig) -> ConcurrencyReport {
    let write_agents = cfg.write_agents();
    let (tx, rx) = mpsc::channel::<AgentReport>();
    let mut handles = Vec::new();
    let mut expected_labels = Vec::new();
    let mut next_write_idx = 0usize;

    for w in 0..cfg.writers {
        let label = format!("writer-{w}");
        expected_labels.push(label.clone());
        handles.push(spawn_agent(
            Arc::clone(&shared),
            tx.clone(),
            label,
            AgentKind::Writer,
            next_write_idx,
            write_agents,
            cfg.seed ^ (0x1111 * (next_write_idx as u64 + 1)),
        ));
        next_write_idx += 1;
    }
    for r in 0..cfg.readers {
        let label = format!("reader-{r}");
        expected_labels.push(label.clone());
        handles.push(spawn_agent(
            Arc::clone(&shared),
            tx.clone(),
            label,
            AgentKind::Reader,
            0,
            write_agents,
            cfg.seed ^ (0x2222 * (r as u64 + 1)),
        ));
    }
    for m in 0..cfg.mixed {
        let label = format!("mixed-{m}");
        expected_labels.push(label.clone());
        handles.push(spawn_agent(
            Arc::clone(&shared),
            tx.clone(),
            label,
            AgentKind::Mixed,
            next_write_idx,
            write_agents,
            cfg.seed ^ (0x3333 * (next_write_idx as u64 + 1)),
        ));
        next_write_idx += 1;
    }
    drop(tx);

    let start = Instant::now();
    thread::sleep(cfg.duration);
    shared.stop.store(true, Ordering::Relaxed);

    let mut report = ConcurrencyReport::default();
    let deadline = Instant::now() + cfg.join_grace;
    for _ in 0..expected_labels.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(agent) => report.agents.push(agent),
            Err(_) => {
                let arrived: Vec<&str> = report.agents.iter().map(|a| a.label.as_str()).collect();
                let missing: Vec<&String> = expected_labels
                    .iter()
                    .filter(|l| !arrived.contains(&l.as_str()))
                    .collect();
                report.deadlock = Some(format!(
                    "agent(s) did not report within {:?}: {:?}",
                    cfg.join_grace, missing
                ));
                report.elapsed = start.elapsed();
                return report;
            }
        }
    }
    for h in handles {
        h.join().expect("agent thread panicked");
    }
    report.elapsed = start.elapsed();
    report
}

/// Apply the four headline assertions to a finished run. Shared by the fast
/// scaffolding tests and the soak so the acceptance criteria can never drift
/// apart.
fn assert_healthy(
    report: &ConcurrencyReport,
    verify: &Result<(), String>,
    cfg: &ConcurrencyConfig,
) {
    // (1) No deadlocks.
    assert!(report.deadlock.is_none(), "deadlock: {:?}", report.deadlock);

    // (3) No torn reads / FTS consistent.
    if let Err(e) = verify {
        panic!("final-state consistency check failed: {e}");
    }

    // (4) Writers made forward progress and starvation is bounded.
    assert!(
        report.total_writes() > 0,
        "no writes committed — writers starved to zero"
    );
    assert!(
        report.worst_write_latency() <= cfg.starvation_ceiling,
        "write-starvation: worst single-write latency {:?} exceeded ceiling {:?}",
        report.worst_write_latency(),
        cfg.starvation_ceiling
    );

    // Readers must have actually read (sanity on the mix).
    assert!(report.total_reads() > 0, "no reads recorded");
}

// ===========================================================================
// Fast scaffolding tests — run on every PR, sub-second.
// ===========================================================================

/// The checksum detector must reject a node whose stored checksum disagrees
/// with its slot/gen — this is the torn-read sensor, so prove it actually
/// fires before trusting it in the concurrent path.
#[test]
fn checksum_detector_catches_a_corrupted_node() {
    let db = Drevo::open_in_memory().unwrap();
    let cfg = ConcurrencyConfig::soak(Duration::from_millis(1));
    let ids = seed_corpus(
        &db,
        &ConcurrencyConfig {
            invariant_nodes: 4,
            ..cfg
        },
    );

    // A correctly-encoded node passes.
    let good = db.get_node(ids[0]).unwrap().unwrap();
    assert!(check_node_consistent(&good).is_ok());

    // Corrupt the checksum property without touching slot/gen → torn-read sensor fires.
    let mut props = good.properties.clone();
    props
        .0
        .insert("checksum".to_string(), serde_json::json!(0u64));
    db.update_node(
        ids[0],
        NodePatch {
            kind: None,
            title: None,
            body: None,
            body_html: None,
            properties: Some(props),
        },
    )
    .unwrap();
    let bad = db.get_node(ids[0]).unwrap().unwrap();
    let err = check_node_consistent(&bad).unwrap_err();
    assert!(err.contains("TORN READ"), "unexpected error: {err}");
}

/// `gen_token` / `checksum` must be deterministic and collision-free across the
/// small space the suite uses, so the final-state FTS lookup is unambiguous.
#[test]
fn token_and_checksum_are_deterministic_and_distinct() {
    assert_eq!(gen_token(3, 9), gen_token(3, 9));
    assert_ne!(gen_token(3, 9), gen_token(3, 10));
    assert_ne!(gen_token(3, 9), gen_token(4, 9));
    assert_eq!(checksum(3, 9), checksum(3, 9));
    assert_ne!(checksum(3, 9), checksum(3, 10));
    assert_ne!(checksum(3, 9), checksum(4, 9));
}

/// A short in-memory burst exercises every role, deadlock-free, with the final
/// state internally consistent and writers making progress.
#[test]
fn short_burst_in_memory_is_healthy() {
    let db = Arc::new(Drevo::open_in_memory().unwrap());
    let cfg = ConcurrencyConfig {
        invariant_nodes: 32,
        duration: Duration::from_millis(400),
        join_grace: Duration::from_secs(10),
        ..ConcurrencyConfig::soak(Duration::from_millis(400))
    };
    let (report, verify) = run_and_verify(db, &cfg);
    report.print("scaffolding — in-memory short burst");
    assert_healthy(&report, &verify, &cfg);
    // Every role contributed.
    assert!(report.total_searches() > 0, "no searches");
    assert!(report.total_traversals() > 0, "no traversals");
    assert!(report.total_creates() > 0, "no churn creates");
}

/// The same burst against the disk-backed redb backend — the path with real
/// MVCC snapshots. This is the one that proves redb's single-writer +
/// many-reader model neither deadlocks nor produces torn reads under load.
#[test]
#[cfg(feature = "redb-backend")]
fn short_burst_redb_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Drevo::open(&dir.path().join("concurrent.redb")).unwrap());
    let cfg = ConcurrencyConfig {
        invariant_nodes: 32,
        duration: Duration::from_millis(500),
        join_grace: Duration::from_secs(10),
        ..ConcurrencyConfig::soak(Duration::from_millis(500))
    };
    let (report, verify) = run_and_verify(db, &cfg);
    report.print("scaffolding — redb short burst");
    assert_healthy(&report, &verify, &cfg);
}

/// `run_concurrent` (the variant used by callers that don't need the
/// post-run generation map) also runs deadlock-free and reports every agent.
#[test]
fn run_concurrent_reports_every_agent() {
    let db = Arc::new(Drevo::open_in_memory().unwrap());
    let cfg = ConcurrencyConfig {
        invariant_nodes: 16,
        duration: Duration::from_millis(200),
        join_grace: Duration::from_secs(10),
        ..ConcurrencyConfig::soak(Duration::from_millis(200))
    };
    let report = run_concurrent(db, &cfg);
    report.print("scaffolding — run_concurrent agent coverage");
    assert!(report.deadlock.is_none());
    assert_eq!(
        report.agents.len(),
        cfg.writers + cfg.readers + cfg.mixed,
        "every spawned agent must report exactly once"
    );
}

/// Slot ownership must partition cleanly: with `write_agents` writers striping
/// `slot % write_agents`, every slot is owned by exactly one agent, so its
/// generation stays monotone. Guards the soundness of the final-state check.
#[test]
fn slot_partition_is_disjoint_and_total() {
    let cfg = ConcurrencyConfig::soak(Duration::from_millis(1));
    let write_agents = cfg.write_agents();
    let mut owner = vec![usize::MAX; cfg.invariant_nodes as usize];
    for write_idx in 0..write_agents {
        for slot in 0..cfg.invariant_nodes {
            if (slot as usize) % write_agents == write_idx {
                assert_eq!(owner[slot as usize], usize::MAX, "slot {slot} double-owned");
                owner[slot as usize] = write_idx;
            }
        }
    }
    assert!(owner.iter().all(|&o| o != usize::MAX), "some slot unowned");
}

/// The deadlock detector must *report* (not hang) when an agent never sends.
/// We simulate by collecting fewer reports than expected within a tiny grace
/// window and asserting `recv_timeout` surfaces the miss.
#[test]
fn deadlock_detector_times_out_on_missing_report() {
    let (tx, rx) = mpsc::channel::<AgentReport>();
    // Only one of two expected agents ever reports.
    tx.send(AgentReport {
        label: "writer-0".to_string(),
        ..Default::default()
    })
    .unwrap();
    // Keep `tx` alive so the channel doesn't close (mirrors a still-running,
    // hung thread holding its sender).
    let expected = ["writer-0", "writer-1"];
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut timed_out = false;
    for _ in 0..expected.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(a) => got.push(a.label),
            Err(_) => {
                timed_out = true;
                break;
            }
        }
    }
    drop(tx);
    assert_eq!(got, vec!["writer-0".to_string()]);
    assert!(
        timed_out,
        "detector should have timed out on the missing report"
    );
}

// ===========================================================================
// Soak — 5-minute concurrency suite (run via --ignored in nightly CI / on demand).
// ===========================================================================

/// The full task-`00125` deliverable: 2 writers + 4 readers + 2 mixed agents
/// pounding one shared `Drevo` for 5 minutes (env-overridable, floored at 5
/// min so the "5 minute" criterion can't be silently undercut).
///
/// Run with:
///
/// ```text
/// cargo nextest run --test concurrent_agents -- --ignored --nocapture
/// DREVO_CONCURRENCY_SECS=1800 cargo nextest run --test concurrent_agents -- --ignored --nocapture
/// ```
///
/// Asserts the four invariants: no deadlocks, no panics, no torn reads + FTS
/// consistent, and bounded write-starvation. The printed report (per-agent
/// tallies + worst write latency) is the operator-facing deliverable.
#[test]
#[ignore = "soak: 5+ min concurrency suite — run via --ignored in nightly CI / on demand"]
#[cfg(feature = "redb-backend")]
fn concurrency_suite_soak() {
    let secs = std::env::var("DREVO_CONCURRENCY_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5 * 60)
        .max(5 * 60);

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Drevo::open(&dir.path().join("soak.redb")).unwrap());
    let cfg = ConcurrencyConfig::soak(Duration::from_secs(secs));

    let (report, verify) = run_and_verify(db, &cfg);
    report.print("SOAK — 5-minute concurrency suite (redb)");

    assert!(
        report.elapsed >= Duration::from_secs(5 * 60),
        "soak ran for {:?}, under the 5-minute floor",
        report.elapsed
    );
    assert_healthy(&report, &verify, &cfg);
}
