//! The scheduler that runs a database's ingest items in the background.
//!
//! One thread per database picks items: among the queued and running jobs,
//! highest `priority` first (then oldest), the lowest `seq` of a job whose
//! items in flight are under its `concurrency`, among items `queued` or
//! `failed` with `next_attempt_at` passed. Picking marks the item `running`
//! in a small transaction of its own, takes one of the server-wide slots
//! ([`IngestSettings::workers`](crate::ingest::IngestSettings::workers)) and
//! hands the item to a thread of its own, which asks the model outside any
//! transaction and then writes the result — or the failure, with the time
//! of the next attempt — in one transaction.
//!
//! The scheduler sleeps until [`wake`](crate::ingest::wake) (a job
//! submitted, resumed or retried, an item finished, an extraction endpoint
//! installed) or, while jobs are active, until the next retry is due, at
//! most a second. While no extraction endpoint is installed nothing can
//! run: the scheduler sleeps until one is, however many jobs are queued.
//!
//! An item deleted while the model answered it is gone: nothing is written
//! for it, and its job finishes — `cancelled` when it lost items — once
//! nothing else is left to run. An item whose chunk is not found after its
//! extraction was written (its job's `source_id` differs from the source of
//! its config) is `dead` with the reason.
//!
//! The scheduler and the item threads hold the service only weakly, and a
//! strong reference only while they run statements. [`Scheduler::stop`]
//! takes the gate the item threads write under, so once it returns no item
//! writes anything more; an item thread still waiting for the model finds
//! the scheduler stopped when the model answers and drops the answer. Its
//! item stays `running` in the graph until the store is opened again.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cypher::executor::{ExecError, ExecResult, Value};
use crate::extract::{ChunkExtraction, ExtractOptions};
use crate::native_service::NativeService;

use super::backoff::{self, Jitter};
use super::progress::{Coalescer, Progress};
use super::state::{self, ItemAction, ItemState, JobAction, JobState};
use super::Outcome;

/// How often the scheduler looks again while jobs are active and nothing
/// woke it.
const POLL: Duration = Duration::from_secs(1);

/// Items in flight across the server.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// One of the server-wide item slots, given back when dropped; giving back
/// a used slot wakes the schedulers.
struct Slot {
    used: bool,
}

impl Slot {
    /// A slot, unless `cap` are taken.
    fn take(cap: usize) -> Option<Self> {
        let mut n = IN_FLIGHT.load(Ordering::SeqCst);
        loop {
            if n >= cap {
                return None;
            }
            match IN_FLIGHT.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Self { used: false }),
                Err(now) => n = now,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        if self.used {
            super::wake();
        }
    }
}

/// What the scheduler thread and the item threads of one database share.
struct Shared {
    /// Set once the scheduler stops.
    stop: AtomicBool,
    /// Held while an item's outcome is written, and by [`Scheduler::stop`].
    gate: Mutex<()>,
    /// Items of each job in flight.
    in_flight: Mutex<HashMap<String, usize>>,
    /// Holds back `ingest` events to one a second per job.
    progress: Mutex<Coalescer>,
    /// Draws the random part of retry delays.
    jitter: Mutex<Jitter>,
    /// Active jobs this database counts in `drevo_ingest_jobs_active`.
    active: AtomicI64,
    /// The database's name, for logs.
    database: String,
}

impl Shared {
    /// Items of job `id` in flight.
    fn in_flight(&self, id: &str) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Offer `progress` and send it when it is due.
    fn offer(&self, service: &NativeService, progress: Progress) {
        let send = self
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .offer(progress, Instant::now());
        if let Some(progress) = send {
            service.publish_ingest(&progress);
        }
    }

    /// Send the held-back events that are due; how long until the next one.
    fn flush(&self, service: &NativeService) -> Option<Duration> {
        let now = Instant::now();
        let (due, next) = {
            let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            (progress.due(now), progress.next_due())
        };
        for progress in due {
            service.publish_ingest(&progress);
        }
        next.map(|at| at.saturating_duration_since(now))
    }

    /// Report `active` jobs of this database to the gauge.
    fn report_active(&self, active: usize) {
        let active = i64::try_from(active).unwrap_or(i64::MAX);
        let before = self.active.swap(active, Ordering::Relaxed);
        super::adjust_jobs_active(active - before);
    }
}

/// An item of a job counted in flight until dropped.
struct Flight {
    shared: Arc<Shared>,
    job: String,
}

impl Flight {
    fn new(shared: &Arc<Shared>, job: &str) -> Self {
        *shared
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(job.to_string())
            .or_insert(0) += 1;
        Self {
            shared: Arc::clone(shared),
            job: job.to_string(),
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut in_flight = self
            .shared
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(n) = in_flight.get_mut(&self.job) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                in_flight.remove(&self.job);
            }
        }
    }
}

/// The ingest scheduler of one database.
pub(crate) struct Scheduler {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Scheduler {
    /// Queue again the items `service` left running, then start the
    /// scheduler thread. `None` when the thread cannot be started.
    pub(crate) fn start(service: &Arc<NativeService>) -> Option<Self> {
        let database = service.database_name();
        if let Err(e) = recover(service) {
            log_warn(
                &database,
                "cannot requeue the ingest items left running",
                &e,
            );
        }
        let seed = super::settings().jitter_seed.unwrap_or_else(|| {
            u64::try_from(crate::model::now_ms()).unwrap_or(0) ^ std::process::id() as u64
        });
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            gate: Mutex::new(()),
            in_flight: Mutex::new(HashMap::new()),
            progress: Mutex::new(Coalescer::default()),
            jitter: Mutex::new(Jitter::seeded(seed)),
            active: AtomicI64::new(0),
            database,
        });
        let weak = Arc::downgrade(service);
        let looping = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("drevo-ingest".into())
            .spawn(move || run(&looping, &weak));
        match thread {
            Ok(thread) => Some(Self {
                shared,
                thread: Mutex::new(Some(thread)),
            }),
            Err(e) => {
                log_warn(
                    &shared.database,
                    "cannot start the ingest scheduler",
                    &e.to_string(),
                );
                None
            }
        }
    }

    /// Stop the scheduler and wait for its thread (unless called on it).
    /// Once this returns, no item outcome is written.
    pub(crate) fn stop(&self) {
        {
            let _gate = self.shared.gate.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        super::wake();
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
        self.shared.report_active(0);
    }

    /// Take in a change of a job's progress.
    pub(crate) fn offer(&self, service: &NativeService, progress: Progress) {
        self.shared.offer(service, progress);
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Log a scheduler problem at WARN.
fn log_warn(database: &str, what: &str, error: &str) {
    #[cfg(feature = "http")]
    tracing::warn!(target: "drevo::ingest", database, error, "{what}");
    #[cfg(not(feature = "http"))]
    let _ = (database, what, error);
}

/// Run statement `q` of the scheduler's own.
fn run_query(
    service: &NativeService,
    q: &str,
    params: Vec<(&str, Value)>,
) -> Result<ExecResult, ExecError> {
    let query = crate::cypher::parser::parse(q).map_err(|e| internal(e.to_string()))?;
    let params = params
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    service.execute_internal(&query, params)
}

/// A failure of the scheduler's own making, as a statement error.
fn internal(message: String) -> ExecError {
    ExecError::Storage(crate::error::DrevoError::Io(std::io::Error::other(
        format!("ingest scheduler: {message}"),
    )))
}

fn string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn integer(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(n) => Some(*n),
        _ => None,
    }
}

fn now_iso(now_ms: i64) -> Value {
    Value::String(crate::cypher::executor::iso8601_utc(now_ms))
}

/// Queue again every item left `running`.
fn recover(service: &NativeService) -> Result<(), String> {
    let left = run_query(
        service,
        "MATCH (i:IngestItem) WHERE i.state = $running RETURN count(i)",
        vec![("running", Value::String(ItemState::Running.as_str().into()))],
    )
    .map_err(|e| e.to_string())?;
    if left.rows.first().and_then(|r| r.first()).and_then(integer) == Some(0) {
        return Ok(());
    }
    let queued = state::item(ItemState::Running, ItemAction::Recover)?;
    run_query(
        service,
        "MATCH (i:IngestItem) WHERE i.state = $running \
         SET i.state = $queued, i.updated_at = $now",
        vec![
            ("running", Value::String(ItemState::Running.as_str().into())),
            ("queued", Value::String(queued.as_str().into())),
            ("now", now_iso(crate::model::now_ms())),
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// A queued or running job, as the scheduler sees it.
#[derive(Debug, Clone)]
struct Job {
    id: String,
    state: JobState,
    priority: i64,
    created_at: String,
    concurrency: Option<usize>,
    max_attempts: u32,
    config: Value,
    options: ExtractOptions,
}

/// The queued and running jobs; a job whose config no longer parses is
/// left out (and logged).
fn active_jobs(service: &NativeService, database: &str) -> Result<Vec<Job>, ExecError> {
    let res = run_query(
        service,
        "MATCH (j:IngestJob) WHERE j.state IN [$queued, $running] \
         RETURN j.id, j.state, j.priority, j.created_at, j.concurrency, j.max_attempts, j.config",
        vec![
            ("queued", Value::String(JobState::Queued.as_str().into())),
            ("running", Value::String(JobState::Running.as_str().into())),
        ],
    )?;
    let mut jobs: Vec<Job> = res
        .rows
        .iter()
        .filter_map(|r| {
            let config = string(&r[6])
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .map_or(Value::Null, |c| crate::cypher::executor::json_to_value(&c));
            let options = match ExtractOptions::from_value(&config, true) {
                Ok(options) => options,
                Err(e) => {
                    log_warn(database, "an ingest job has an unusable config", &e);
                    return None;
                }
            };
            Some(Job {
                id: string(&r[0])?,
                state: JobState::parse(&string(&r[1])?)?,
                priority: integer(&r[2]).unwrap_or(0),
                created_at: string(&r[3]).unwrap_or_default(),
                concurrency: integer(&r[4]).and_then(|n| usize::try_from(n).ok()),
                max_attempts: integer(&r[5])
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(super::settings().max_attempts)
                    .max(1),
                config,
                options,
            })
        })
        .collect();
    jobs.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(jobs)
}

/// What is left of a job.
struct Pending {
    /// The lowest `seq` ready to run now, with its state.
    ready: Option<(i64, ItemState)>,
    /// The earliest `next_attempt_at` still to come.
    next_at: Option<i64>,
    /// Whether any item is queued, failed or running.
    any: bool,
}

/// What job `id` has left at `now_ms`.
fn pending(service: &NativeService, id: &str, now_ms: i64) -> Result<Pending, ExecError> {
    let res = run_query(
        service,
        "MATCH (i:IngestItem {job_id: $job}) WHERE i.state IN [$queued, $failed, $running] \
         RETURN i.seq, i.state, i.next_attempt_at",
        vec![
            ("job", Value::String(id.into())),
            ("queued", Value::String(ItemState::Queued.as_str().into())),
            ("failed", Value::String(ItemState::Failed.as_str().into())),
            ("running", Value::String(ItemState::Running.as_str().into())),
        ],
    )?;
    let mut out = Pending {
        ready: None,
        next_at: None,
        any: !res.rows.is_empty(),
    };
    for r in &res.rows {
        let (Some(seq), Some(item_state)) = (
            integer(&r[0]),
            string(&r[1]).and_then(|s| ItemState::parse(&s)),
        ) else {
            continue;
        };
        if item_state == ItemState::Running {
            continue;
        }
        let at = integer(&r[2]).unwrap_or(0);
        if at <= now_ms {
            if out.ready.is_none_or(|(s, _)| seq < s) {
                out.ready = Some((seq, item_state));
            }
        } else if out.next_at.is_none_or(|n| at < n) {
            out.next_at = Some(at);
        }
    }
    Ok(out)
}

/// One item handed to an item thread.
struct Work {
    job: String,
    seq: i64,
    text: String,
    offset: usize,
    attempts: u32,
    max_attempts: u32,
    options: ExtractOptions,
    config: Value,
    source_id: String,
}

/// Mark item `seq` of `job` running (and the job with it); the item's work,
/// or `None` when it is no longer there to run.
fn mark_running(
    service: &NativeService,
    job: &Job,
    seq: i64,
    from: ItemState,
) -> Result<Option<Work>, String> {
    let item_to = state::item(from, ItemAction::Pick)?;
    let job_to = state::job(job.state, JobAction::Start)?;
    let now = now_iso(crate::model::now_ms());
    let res = run_query(
        service,
        "MATCH (j:IngestJob {id: $job}) WHERE j.state IN [$queued, $running] \
         MATCH (i:IngestItem {job_id: $job, seq: $seq}) WHERE i.state = $from \
         SET i.state = $item_to, i.attempts = coalesce(i.attempts, 0) + 1, i.updated_at = $now, \
             j.state = $job_to, j.started_at = coalesce(j.started_at, $now), j.updated_at = $now \
         RETURN i.text, i.offset, i.attempts, j.source_id",
        vec![
            ("job", Value::String(job.id.clone())),
            ("seq", Value::Integer(seq)),
            ("queued", Value::String(JobState::Queued.as_str().into())),
            ("running", Value::String(JobState::Running.as_str().into())),
            ("from", Value::String(from.as_str().into())),
            ("item_to", Value::String(item_to.as_str().into())),
            ("job_to", Value::String(job_to.as_str().into())),
            ("now", now),
        ],
    )
    .map_err(|e| e.to_string())?;
    let Some(row) = res.rows.first() else {
        return Ok(None);
    };
    Ok(Some(Work {
        job: job.id.clone(),
        seq,
        text: string(&row[0]).unwrap_or_default(),
        offset: integer(&row[1])
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0),
        attempts: integer(&row[2])
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(1),
        max_attempts: job.max_attempts,
        options: job.options.clone(),
        config: job.config.clone(),
        source_id: string(&row[3]).unwrap_or_default(),
    }))
}

/// The scheduler thread.
fn run(shared: &Arc<Shared>, service: &Weak<NativeService>) {
    let mut items: Vec<JoinHandle<()>> = Vec::new();
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let seen = super::generation();
        items.retain(|h| !h.is_finished());
        let Some(svc) = service.upgrade() else {
            break;
        };
        let flush = shared.flush(&svc);
        let next = pass(shared, &svc, service, &mut items);
        drop(svc);
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let wait = match (flush, next) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        super::wait(seen, wait);
    }
}

/// Start what can start now; how long to sleep at most (`None`: until
/// woken).
fn pass(
    shared: &Arc<Shared>,
    service: &Arc<NativeService>,
    weak: &Weak<NativeService>,
    items: &mut Vec<JoinHandle<()>>,
) -> Option<Duration> {
    if service.is_read_only() {
        return Some(POLL);
    }
    let jobs = match active_jobs(service, &shared.database) {
        Ok(jobs) => jobs,
        Err(e) => {
            log_warn(
                &shared.database,
                "cannot read the ingest jobs",
                &e.to_string(),
            );
            return Some(POLL);
        }
    };
    shared.report_active(jobs.len());
    if jobs.is_empty() {
        return None;
    }
    // Nothing can run without an endpoint: sleep until woken, which
    // installing one does.
    crate::extract::installed()?;
    let settings = super::settings();
    let now_ms = crate::model::now_ms();
    let mut next_at: Option<i64> = None;
    'jobs: for job in jobs {
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                return None;
            }
            let running = shared.in_flight(&job.id);
            let cap = job
                .concurrency
                .unwrap_or(settings.workers)
                .min(settings.workers);
            if running >= cap {
                continue 'jobs;
            }
            let left = match pending(service, &job.id, now_ms) {
                Ok(left) => left,
                Err(e) => {
                    log_warn(
                        &shared.database,
                        "cannot read an ingest job's items",
                        &e.to_string(),
                    );
                    continue 'jobs;
                }
            };
            if let Some(at) = left.next_at {
                next_at = Some(next_at.map_or(at, |n| n.min(at)));
            }
            let Some((seq, from)) = left.ready else {
                if !left.any && running == 0 {
                    refresh(shared, service, &job.id, None);
                }
                continue 'jobs;
            };
            let Some(mut slot) = Slot::take(settings.workers) else {
                break 'jobs;
            };
            let work = match mark_running(service, &job, seq, from) {
                Ok(Some(work)) => work,
                Ok(None) => continue 'jobs,
                Err(e) => {
                    log_warn(&shared.database, "cannot start an ingest item", &e);
                    continue 'jobs;
                }
            };
            slot.used = true;
            let flight = Flight::new(shared, &job.id);
            let item_shared = Arc::clone(shared);
            let item_weak = weak.clone();
            let spawned = std::thread::Builder::new()
                .name("drevo-ingest-item".into())
                .spawn(move || {
                    run_item(&item_shared, &item_weak, work);
                    drop(flight);
                    drop(slot);
                });
            match spawned {
                Ok(handle) => items.push(handle),
                Err(e) => {
                    log_warn(
                        &shared.database,
                        "cannot start an ingest item thread",
                        &e.to_string(),
                    );
                    return Some(POLL);
                }
            }
        }
    }
    let due = next_at
        .map(|at| Duration::from_millis(u64::try_from(at.saturating_sub(now_ms)).unwrap_or(0)));
    Some(due.map_or(POLL, |d| d.min(POLL)))
}

/// Extract one item, then write its outcome.
fn run_item(shared: &Arc<Shared>, weak: &Weak<NativeService>, work: Work) {
    // One passage: asked whole, not cut again.
    let options = ExtractOptions {
        chunk_chars: work.text.chars().count().clamp(
            crate::extract::MIN_CHUNK_CHARS,
            crate::extract::MAX_CHUNK_CHARS,
        ),
        ..work.options.clone()
    };
    let found = crate::extract::prepare_ingest(&work.text, &options);
    let gate = shared.gate.lock().unwrap_or_else(|e| e.into_inner());
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    let Some(service) = weak.upgrade() else {
        return;
    };
    let outcome = match found {
        Ok(found) => write_item(&service, &work, found),
        Err(e) => Err((e.is_retryable(), e.to_string())),
    };
    let error = match outcome {
        Ok(Written::Done) => {
            super::record(Outcome::Done);
            None
        }
        Ok(Written::Gone) => None,
        Err((retryable, message)) => {
            let retry = retryable && work.attempts < work.max_attempts;
            let jitter = shared
                .jitter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .draw();
            let wait = backoff::next(work.attempts, super::settings().retry_base, jitter);
            let next_at = crate::model::now_ms()
                .saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
            if let Err(e) = fail_item(&service, &work, retry, &message, next_at) {
                log_warn(&shared.database, "cannot record an ingest item failure", &e);
            }
            super::record(if retry {
                Outcome::Failed
            } else {
                Outcome::Dead
            });
            Some(message)
        }
    };
    refresh(shared, &service, &work.job, error.as_deref());
    drop(gate);
    // Dropped after the gate: if this was the last reference, the service
    // stops its scheduler, which takes the gate.
    drop(service);
}

/// How an item's write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// The extraction is written and the item is `done`.
    Done,
    /// The item was deleted meanwhile: nothing was written for it.
    Gone,
}

/// Write an item's extraction, mark it done and link its chunk, in one
/// transaction. A failure says whether another attempt may succeed.
fn write_item(
    service: &NativeService,
    work: &Work,
    mut found: Vec<ChunkExtraction>,
) -> Result<Written, (bool, String)> {
    let order = usize::try_from(work.seq).unwrap_or(0);
    for f in &mut found {
        f.chunk.order = order;
        f.chunk.offset += work.offset;
    }
    let done = state::item(ItemState::Running, ItemAction::Succeed).map_err(|e| (true, e))?;
    let chunk = crate::cypher::provenance::default_chunk_id(&work.source_id, work.seq);
    // Keyed on the job's options, which the statement's `$config` reads to:
    // the passage size the item was asked with is its own. The item is
    // matched first, so nothing is written for an item deleted meanwhile.
    let res = crate::extract::with_prepared(&work.text, &work.options, found, || {
        run_query(
            service,
            "MATCH (i:IngestItem {job_id: $job, seq: $seq}) \
             CALL drevo.extract.ingest($text, $config) YIELD source \
             MATCH (c:Chunk {id: $chunk}) \
             SET i.state = $done, i.last_error = null, i.updated_at = $now \
             MERGE (i)-[:PRODUCED]->(c) \
             RETURN count(i)",
            vec![
                ("text", Value::String(work.text.clone())),
                ("config", work.config.clone()),
                ("job", Value::String(work.job.clone())),
                ("seq", Value::Integer(work.seq)),
                ("chunk", Value::String(chunk.clone())),
                ("done", Value::String(done.as_str().into())),
                ("now", now_iso(crate::model::now_ms())),
            ],
        )
    })
    .map_err(|e| {
        // Our own refusal (an unknown source, a chunk of another source)
        // fails the same way again.
        let retry = !matches!(e, ExecError::InvalidProcedureCall { .. });
        (retry, e.to_string())
    })?;
    if first_integer(&res) != Some(0) {
        return Ok(Written::Done);
    }
    // Nothing was marked done: either the item is gone, or its chunk is
    // not where the job's source id says it is.
    let left = run_query(
        service,
        "MATCH (i:IngestItem {job_id: $job, seq: $seq}) RETURN count(i)",
        vec![
            ("job", Value::String(work.job.clone())),
            ("seq", Value::Integer(work.seq)),
        ],
    )
    .map_err(|e| (true, e.to_string()))?;
    if first_integer(&left) == Some(0) {
        return Ok(Written::Gone);
    }
    Err((false, missing_chunk(&chunk, work)))
}

/// The reason an item fails when its chunk is not found after its
/// extraction was written.
fn missing_chunk(chunk: &str, work: &Work) -> String {
    let configured = match &work.options.source {
        Some(crate::cypher::provenance::SourceRef::Id(id)) => Some(id.as_str()),
        Some(crate::cypher::provenance::SourceRef::Spec(spec)) => Some(spec.id.as_str()),
        None => None,
    };
    match configured {
        Some(configured) if configured != work.source_id => format!(
            "chunk `{chunk}` not found after ingest: the job's source_id `{}` differs from \
             config.source `{configured}`",
            work.source_id
        ),
        _ => format!("chunk `{chunk}` not found after ingest"),
    }
}

/// The first column of the first row, as an integer.
fn first_integer(res: &ExecResult) -> Option<i64> {
    res.rows.first().and_then(|r| r.first()).and_then(integer)
}

/// Record a failed attempt: `failed` with the time of the next attempt when
/// `retry`, else `dead`.
fn fail_item(
    service: &NativeService,
    work: &Work,
    retry: bool,
    message: &str,
    next_at: i64,
) -> Result<(), String> {
    let to = state::item(ItemState::Running, ItemAction::Fail { retry })?;
    run_query(
        service,
        "MATCH (i:IngestItem {job_id: $job, seq: $seq}) WHERE i.state = $running \
         SET i.state = $to, i.last_error = $error, i.next_attempt_at = $next, \
             i.updated_at = $now",
        vec![
            ("job", Value::String(work.job.clone())),
            ("seq", Value::Integer(work.seq)),
            ("running", Value::String(ItemState::Running.as_str().into())),
            ("to", Value::String(to.as_str().into())),
            ("error", Value::String(message.to_string())),
            ("next", Value::Integer(next_at)),
            ("now", now_iso(crate::model::now_ms())),
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Bring job `id`'s counters up to its items, finish it when nothing is
/// left to run, and offer its progress. `error` is the failure that ended
/// the last item, for the problem feed.
fn refresh(shared: &Shared, service: &NativeService, id: &str, error: Option<&str>) {
    if let Err(e) = try_refresh(shared, service, id, error) {
        log_warn(&shared.database, "cannot update an ingest job", &e);
    }
}

fn try_refresh(
    shared: &Shared,
    service: &NativeService,
    id: &str,
    error: Option<&str>,
) -> Result<(), String> {
    let job = run_query(
        service,
        "MATCH (j:IngestJob {id: $job}) RETURN j.state, j.total, j.done, j.failed",
        vec![("job", Value::String(id.into()))],
    )
    .map_err(|e| e.to_string())?;
    let Some(row) = job.rows.first() else {
        return Ok(());
    };
    let Some(from) = string(&row[0]).and_then(|s| JobState::parse(&s)) else {
        return Ok(());
    };
    let total = integer(&row[1]).unwrap_or(0);
    let items = run_query(
        service,
        "MATCH (i:IngestItem {job_id: $job}) RETURN i.state, count(*)",
        vec![("job", Value::String(id.into()))],
    )
    .map_err(|e| e.to_string())?;
    let counts: BTreeMap<String, i64> = items
        .rows
        .iter()
        .filter_map(|r| Some((string(&r[0])?, integer(&r[1])?)))
        .collect();
    // A purged job has no items left to count.
    let (done, dead) = if counts.is_empty() {
        (integer(&row[2]).unwrap_or(0), integer(&row[3]).unwrap_or(0))
    } else {
        let n = |s: ItemState| counts.get(s.as_str()).copied().unwrap_or(0);
        (n(ItemState::Done), n(ItemState::Dead))
    };
    let left = [ItemState::Queued, ItemState::Failed, ItemState::Running]
        .iter()
        .map(|s| counts.get(s.as_str()).copied().unwrap_or(0))
        .sum::<i64>();
    // A running job with nothing left finishes; one that lost items
    // (deleted before they were written) without any dead is cancelled.
    let finish = if from == JobState::Running && left == 0 {
        let lost = done.saturating_add(dead) < total;
        let action = if lost && dead == 0 {
            JobAction::Cancel
        } else {
            JobAction::Finish { dead: dead > 0 }
        };
        Some(state::job(from, action)?)
    } else {
        None
    };
    let now = now_iso(crate::model::now_ms());
    let res = run_query(
        service,
        "MATCH (j:IngestJob {id: $job}) \
         SET j.done = $done, j.failed = $failed, j.updated_at = $now \
         RETURN j.state",
        vec![
            ("job", Value::String(id.into())),
            ("done", Value::Integer(done)),
            ("failed", Value::Integer(dead)),
            ("now", now.clone()),
        ],
    )
    .map_err(|e| e.to_string())?;
    let mut state = res
        .rows
        .first()
        .and_then(|r| string(&r[0]))
        .unwrap_or_else(|| from.as_str().to_string());
    if let Some(finish) = finish {
        // Only a job still running finishes: a pause or cancel that came in
        // meanwhile stands.
        let res = run_query(
            service,
            "MATCH (j:IngestJob {id: $job}) WHERE j.state = $running \
             SET j.state = $finish, j.finished_at = $now \
             RETURN j.state",
            vec![
                ("job", Value::String(id.into())),
                ("running", Value::String(JobState::Running.as_str().into())),
                ("finish", Value::String(finish.as_str().into())),
                ("now", now),
            ],
        )
        .map_err(|e| e.to_string())?;
        if let Some(s) = res.rows.first().and_then(|r| string(&r[0])) {
            state = s;
        }
    }
    if finish == Some(JobState::Failed) && state == JobState::Failed.as_str() {
        #[cfg(feature = "http")]
        crate::problems::report_failed_ingest_job(
            &shared.database,
            id,
            u64::try_from(dead).unwrap_or(0),
            error,
        );
        #[cfg(not(feature = "http"))]
        let _ = error;
    }
    let count = |n: i64| u64::try_from(n).unwrap_or(0);
    shared.offer(
        service,
        Progress {
            job_id: id.to_string(),
            state,
            done: count(done),
            failed: count(dead),
            total: count(total),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::provenance::{SourceRef, SourceSpec};

    fn work(source_id: &str, source: Option<SourceRef>) -> Work {
        Work {
            job: "job-1".into(),
            seq: 0,
            text: "Dana joined Acme.".into(),
            offset: 0,
            attempts: 1,
            max_attempts: 5,
            options: ExtractOptions {
                source,
                ..ExtractOptions::default()
            },
            config: Value::Null,
            source_id: source_id.into(),
        }
    }

    #[test]
    fn a_missing_chunk_names_both_source_ids_when_they_differ() {
        let w = work("book-2", Some(SourceRef::Id("book-1".into())));
        assert_eq!(
            missing_chunk("book-2#0", &w),
            "chunk `book-2#0` not found after ingest: the job's source_id `book-2` differs \
             from config.source `book-1`"
        );
        let spec = SourceSpec {
            id: "book-1".into(),
            uri: None,
            title: None,
            kind: None,
        };
        let w = work("book-2", Some(SourceRef::Spec(spec)));
        assert!(missing_chunk("book-2#0", &w).contains("config.source `book-1`"));
    }

    #[test]
    fn a_missing_chunk_of_a_matching_source_says_only_that() {
        let w = work("book-1", Some(SourceRef::Id("book-1".into())));
        assert_eq!(
            missing_chunk("book-1#0", &w),
            "chunk `book-1#0` not found after ingest"
        );
        assert_eq!(
            missing_chunk("book-1#0", &work("book-1", None)),
            "chunk `book-1#0` not found after ingest"
        );
    }
}
