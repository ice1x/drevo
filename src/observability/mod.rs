//! Observability — Phase 15 task `00130`.
//!
//! Production deployments need to answer two operational questions without a
//! debugger attached: *how much traffic is this process serving, and how fast?*
//! and *which queries ran, and did they fail?* This module supplies the
//! mechanism for both:
//!
//! * a dependency-free, lock-free **metrics registry** ([`Registry`]) with the
//!   three Prometheus metric shapes — [`Counter`], [`Gauge`], [`Histogram`] —
//!   and a [`Registry::render_prometheus`] renderer that emits the Prometheus
//!   text exposition format (version `0.0.4`) the `drevo-server` `/metrics`
//!   route serves;
//! * a **structured query log** built on a plain-data [`QueryObservation`] that
//!   both updates the standard counters/histogram bundled in [`DrevoMetrics`]
//!   and (under the `http` feature, where `tracing` is available) emits a
//!   `tracing` event tagged with OpenTelemetry semantic-convention fields
//!   (`db.system`, `db.operation`, `db.statement`, …) so any
//!   `tracing-opentelemetry` OTLP layer the operator installs exports it as a
//!   span without any further drevo change.
//!
//! Like the planner and MVCC engines, the metrics core is **always compiled**,
//! depends on nothing beyond `std`, and is WASM-safe (atomics + one
//! registration-time [`std::sync::Mutex`], no spawned threads). The HTTP
//! `/metrics` route and the per-request instrumentation live in
//! [`crate::api`] behind the `http` feature; the OTLP wire exporter
//! (`opentelemetry-otlp`) is deliberately **out of scope** for this task —
//! pulling tonic / gRPC / protobuf into the default dependency graph would
//! contradict the crate's "embeddable, no external system deps" property, so
//! it is left as an opt-in follow-up that attaches to the OTEL-semantic spans
//! this module already emits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// OpenTelemetry semantic-convention field names attached to the structured
/// query-log tracing event, exposed as constants so the emitting site and any
/// test that asserts on them share one source of truth. See the OpenTelemetry
/// database client semantic conventions.
pub mod otel_fields {
    /// `db.system` — identifies drevo as the database system.
    pub const DB_SYSTEM: &str = "db.system";
    /// `db.operation` — the logical operation name (e.g. `cypher`, `fts`).
    pub const DB_OPERATION: &str = "db.operation";
    /// `db.statement` — the query text that was executed.
    pub const DB_STATEMENT: &str = "db.statement";
    /// `otel.status_code` — `OK` or `ERROR`, the span status.
    pub const OTEL_STATUS_CODE: &str = "otel.status_code";
    /// The constant value of [`DB_SYSTEM`] for this database.
    pub const DB_SYSTEM_VALUE: &str = "drevo";
}

/// The default histogram bucket boundaries (in seconds) for latency metrics —
/// the canonical Prometheus client default ladder, covering sub-millisecond to
/// ten-second responses. Callers that know their latency profile can pass a
/// tighter ladder to [`Registry::histogram`].
pub const DEFAULT_LATENCY_BUCKETS_SECONDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The kind of a registered metric, rendered on the Prometheus `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A monotonically increasing total ([`Counter`]).
    Counter,
    /// A value that may go up or down ([`Gauge`]).
    Gauge,
    /// A bucketed distribution of observations ([`Histogram`]).
    Histogram,
}

impl MetricKind {
    /// The lower-case token Prometheus expects on the `# TYPE` line.
    fn as_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// A monotonically increasing counter — request totals, error totals, and the
/// like. Cloning shares the same underlying atomic, so the handle can be held
/// by every request handler at once and incremented lock-free.
#[derive(Debug, Clone)]
pub struct Counter {
    value: Arc<AtomicU64>,
}

impl Counter {
    /// A fresh counter starting at zero.
    fn new() -> Self {
        Self {
            value: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Add one.
    pub fn inc(&self) {
        self.inc_by(1);
    }

    /// Add `n`. Saturates at [`u64::MAX`] rather than wrapping — a counter must
    /// never appear to go backwards.
    pub fn inc_by(&self, n: u64) {
        // `fetch_add` wraps on overflow; a CAS loop lets us saturate instead so
        // the exported total stays monotonic even in the (practically
        // unreachable) overflow case.
        let mut cur = self.value.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(n);
            match self
                .value
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// The current total.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// A gauge — a value that can rise and fall, such as in-flight requests or the
/// process uptime. Backed by a signed atomic so it can be decremented below
/// its starting point. Cloning shares the underlying atomic.
#[derive(Debug, Clone)]
pub struct Gauge {
    value: Arc<AtomicI64>,
}

impl Gauge {
    /// A fresh gauge starting at zero.
    fn new() -> Self {
        Self {
            value: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Set the gauge to an absolute value.
    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    /// Add `delta` (which may be negative).
    pub fn add(&self, delta: i64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// Add one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Subtract one.
    pub fn dec(&self) {
        self.add(-1);
    }

    /// The current value.
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// Shared, atomically-updated histogram state behind every [`Histogram`] clone.
#[derive(Debug)]
struct HistogramInner {
    /// Upper bounds (inclusive `le`), strictly ascending and finite. The
    /// implicit `+Inf` bucket is rendered separately and equals `count`.
    bounds: Vec<f64>,
    /// Per-bound observation counts — `counts[i]` is the number of observations
    /// whose value is `<= bounds[i]` *and* `> bounds[i-1]` (i.e. the
    /// non-cumulative tally for that band). Cumulated at render time.
    counts: Vec<AtomicU64>,
    /// Total number of observations (the `+Inf` cumulative bucket and the
    /// `_count` series).
    count: AtomicU64,
    /// Sum of all observed values, stored as the bit pattern of an `f64` so it
    /// can live in an atomic; updated with a compare-exchange loop.
    sum_bits: AtomicU64,
}

/// A bucketed distribution — request and query latencies. Observations are
/// tallied into the band whose upper bound first contains them; the Prometheus
/// renderer cumulates the bands into `le` buckets plus `_sum` and `_count`
/// series. Cloning shares the underlying atomics.
#[derive(Debug, Clone)]
pub struct Histogram {
    inner: Arc<HistogramInner>,
}

impl Histogram {
    /// Build a histogram over the given upper bounds (seconds, or any unit).
    ///
    /// Non-finite or out-of-order bounds are dropped / sorted so the histogram
    /// can never be constructed in an invalid state: the bounds are filtered to
    /// the finite ones, sorted ascending, and de-duplicated. An empty bound
    /// list yields a histogram with only the implicit `+Inf` bucket (still a
    /// valid `_sum`/`_count` pair).
    fn new(bounds: &[f64]) -> Self {
        let mut clean: Vec<f64> = bounds.iter().copied().filter(|b| b.is_finite()).collect();
        clean.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        clean.dedup();
        let counts = clean.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            inner: Arc::new(HistogramInner {
                bounds: clean,
                counts,
                count: AtomicU64::new(0),
                sum_bits: AtomicU64::new(0.0_f64.to_bits()),
            }),
        }
    }

    /// Record one observation. A non-finite value still increments `_count` and
    /// lands in the `+Inf` bucket but is excluded from `_sum` (adding `NaN` /
    /// infinity would poison the running total for every later reader).
    pub fn observe(&self, value: f64) {
        // Find the first bound that contains the value; if none, it falls only
        // into the implicit +Inf bucket (count, not any explicit band).
        if let Some(idx) = self.inner.bounds.iter().position(|&b| value <= b) {
            self.inner.counts[idx].fetch_add(1, Ordering::Relaxed);
        }
        self.inner.count.fetch_add(1, Ordering::Relaxed);
        if value.is_finite() {
            let mut cur = self.inner.sum_bits.load(Ordering::Relaxed);
            loop {
                let next = (f64::from_bits(cur) + value).to_bits();
                match self.inner.sum_bits.compare_exchange_weak(
                    cur,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => cur = observed,
                }
            }
        }
    }

    /// The total number of observations.
    pub fn count(&self) -> u64 {
        self.inner.count.load(Ordering::Relaxed)
    }

    /// The sum of all (finite) observations.
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.inner.sum_bits.load(Ordering::Relaxed))
    }

    /// The cumulative count of observations `<= bound`, for the bound at `idx`
    /// in the configured ladder. Used by the renderer and by tests.
    fn cumulative_at(&self, idx: usize) -> u64 {
        (0..=idx)
            .map(|i| self.inner.counts[i].load(Ordering::Relaxed))
            .sum()
    }
}

/// One registered metric: its identity (`name` + sorted `labels`), its
/// documentation and kind, and the live handle the renderer reads.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    help: String,
    kind: MetricKind,
    labels: Vec<(String, String)>,
    handle: Handle,
}

/// A type-erased handle to a registered metric's live value.
#[derive(Debug, Clone)]
enum Handle {
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

/// Insertion-ordered set of registered metrics behind the registry's lock.
#[derive(Debug, Default)]
struct RegistryInner {
    /// Stable insertion order of `(name, labels)` keys, so `render_prometheus`
    /// output is deterministic across runs.
    order: Vec<(String, Vec<(String, String)>)>,
    /// `(name, sorted labels) -> Entry`.
    entries: HashMap<(String, Vec<(String, String)>), Entry>,
}

/// A metrics registry — the thread-safe, dependency-free collection of named
/// metrics a process exposes.
///
/// Metrics are registered once (typically at startup, via [`DrevoMetrics`]) and
/// then updated lock-free through the [`Counter`] / [`Gauge`] / [`Histogram`]
/// handles, which share the registered atomics. The single
/// [`std::sync::Mutex`] guards only registration and rendering, not the hot
/// update path. A poisoned lock is recovered in place — a panic in one
/// registrar must not wedge metric collection for the whole process.
#[derive(Debug, Clone)]
pub struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner::default())),
        }
    }

    /// Normalise a label slice into the sorted, owned key form used internally
    /// so `{a,b}` and `{b,a}` map to the same series.
    fn key_labels(labels: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut owned: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        owned.sort();
        owned
    }

    /// Register (or fetch the existing) counter named `name` with `labels`.
    ///
    /// Registering the same `(name, labels)` again returns the existing handle
    /// when the kind matches, so repeated registration is idempotent; a kind
    /// mismatch (a programming error) replaces the prior registration rather
    /// than panicking, keeping metric collection robust in production.
    pub fn counter(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Counter {
        let key_labels = Self::key_labels(labels);
        let mut guard = self.lock();
        if let Some(Entry {
            handle: Handle::Counter(c),
            ..
        }) = guard.entries.get(&(name.to_string(), key_labels.clone()))
        {
            return c.clone();
        }
        let counter = Counter::new();
        self.insert(
            &mut guard,
            name,
            help,
            MetricKind::Counter,
            key_labels,
            Handle::Counter(counter.clone()),
        );
        counter
    }

    /// Register (or fetch the existing) gauge. See [`Registry::counter`] for the
    /// idempotency / kind-mismatch contract.
    pub fn gauge(&self, name: &str, help: &str, labels: &[(&str, &str)]) -> Gauge {
        let key_labels = Self::key_labels(labels);
        let mut guard = self.lock();
        if let Some(Entry {
            handle: Handle::Gauge(g),
            ..
        }) = guard.entries.get(&(name.to_string(), key_labels.clone()))
        {
            return g.clone();
        }
        let gauge = Gauge::new();
        self.insert(
            &mut guard,
            name,
            help,
            MetricKind::Gauge,
            key_labels,
            Handle::Gauge(gauge.clone()),
        );
        gauge
    }

    /// Register (or fetch the existing) histogram over `bounds`. See
    /// [`Registry::counter`] for the idempotency / kind-mismatch contract; when
    /// the histogram already exists its original bounds are kept.
    pub fn histogram(
        &self,
        name: &str,
        help: &str,
        labels: &[(&str, &str)],
        bounds: &[f64],
    ) -> Histogram {
        let key_labels = Self::key_labels(labels);
        let mut guard = self.lock();
        if let Some(Entry {
            handle: Handle::Histogram(h),
            ..
        }) = guard.entries.get(&(name.to_string(), key_labels.clone()))
        {
            return h.clone();
        }
        let hist = Histogram::new(bounds);
        self.insert(
            &mut guard,
            name,
            help,
            MetricKind::Histogram,
            key_labels,
            Handle::Histogram(hist.clone()),
        );
        hist
    }

    /// Recover the lock in place if a previous holder panicked — stale metric
    /// state can never corrupt a rendered exposition, so proceeding is safe.
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Insert or replace an entry, maintaining stable insertion order.
    fn insert(
        &self,
        guard: &mut RegistryInner,
        name: &str,
        help: &str,
        kind: MetricKind,
        labels: Vec<(String, String)>,
        handle: Handle,
    ) {
        let key = (name.to_string(), labels.clone());
        if !guard.entries.contains_key(&key) {
            guard.order.push(key.clone());
        }
        guard.entries.insert(
            key,
            Entry {
                name: name.to_string(),
                help: help.to_string(),
                kind,
                labels,
                handle,
            },
        );
    }

    /// Render every registered metric in the Prometheus text exposition format
    /// (version `0.0.4`).
    ///
    /// `# HELP` and `# TYPE` lines are emitted once per metric name (the first
    /// time a series for that name is seen, in registration order); counters
    /// and gauges render one `name{labels} value` line each, and histograms
    /// expand into cumulative `name_bucket{le=…}` lines (including the implicit
    /// `+Inf` bucket) plus `name_sum` and `name_count`.
    pub fn render_prometheus(&self) -> String {
        let guard = self.lock();
        let mut out = String::new();
        let mut documented: Vec<String> = Vec::new();
        for key in &guard.order {
            let Some(entry) = guard.entries.get(key) else {
                continue;
            };
            if !documented.iter().any(|n| n == &entry.name) {
                out.push_str(&format!("# HELP {} {}\n", entry.name, entry.help));
                out.push_str(&format!("# TYPE {} {}\n", entry.name, entry.kind.as_str()));
                documented.push(entry.name.clone());
            }
            match &entry.handle {
                Handle::Counter(c) => {
                    out.push_str(&render_sample(&entry.name, "", &entry.labels, None));
                    out.push_str(&format!(" {}\n", c.get()));
                }
                Handle::Gauge(g) => {
                    out.push_str(&render_sample(&entry.name, "", &entry.labels, None));
                    out.push_str(&format!(" {}\n", g.get()));
                }
                Handle::Histogram(h) => {
                    for (idx, bound) in h.inner.bounds.iter().enumerate() {
                        let cumulative = h.cumulative_at(idx);
                        out.push_str(&render_sample(
                            &entry.name,
                            "_bucket",
                            &entry.labels,
                            Some(("le", &format_float(*bound))),
                        ));
                        out.push_str(&format!(" {cumulative}\n"));
                    }
                    let total = h.count();
                    out.push_str(&render_sample(
                        &entry.name,
                        "_bucket",
                        &entry.labels,
                        Some(("le", "+Inf")),
                    ));
                    out.push_str(&format!(" {total}\n"));
                    out.push_str(&render_sample(&entry.name, "_sum", &entry.labels, None));
                    out.push_str(&format!(" {}\n", format_float(h.sum())));
                    out.push_str(&render_sample(&entry.name, "_count", &entry.labels, None));
                    out.push_str(&format!(" {total}\n"));
                }
            }
        }
        out
    }
}

/// Render the `name{labels}` prefix of a sample line (value appended by the
/// caller). `suffix` is appended to the metric name (`_bucket` / `_sum` /
/// `_count` for histograms, empty otherwise); `extra` is an additional label
/// (the `le` bound for histogram buckets) merged into the label set.
fn render_sample(
    name: &str,
    suffix: &str,
    labels: &[(String, String)],
    extra: Option<(&str, &str)>,
) -> String {
    let mut pairs: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
        .collect();
    if let Some((k, v)) = extra {
        // `le` is conventionally rendered last.
        pairs.push(format!("{}=\"{}\"", k, escape_label_value(v)));
    }
    if pairs.is_empty() {
        format!("{name}{suffix}")
    } else {
        format!("{}{}{{{}}}", name, suffix, pairs.join(","))
    }
}

/// Escape a Prometheus label value: backslash, double-quote, and newline per
/// the exposition-format spec.
fn escape_label_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for ch in v.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Format an `f64` for the exposition format: `+Inf` / `-Inf` for infinities,
/// otherwise Rust's shortest round-tripping representation (e.g. `0.005`).
fn format_float(v: f64) -> String {
    if v.is_infinite() {
        if v.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else {
        format!("{v}")
    }
}

/// The outcome of an executed query, used to tag the structured query log and
/// pick the `status` label on the query counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryOutcome {
    /// The query completed successfully.
    Ok,
    /// The query failed.
    Error,
}

impl QueryOutcome {
    /// The `status` label value (`ok` / `error`) for the query counter.
    pub fn label(self) -> &'static str {
        match self {
            QueryOutcome::Ok => "ok",
            QueryOutcome::Error => "error",
        }
    }

    /// The OpenTelemetry `otel.status_code` value (`OK` / `ERROR`).
    pub fn otel_status(self) -> &'static str {
        match self {
            QueryOutcome::Ok => "OK",
            QueryOutcome::Error => "ERROR",
        }
    }
}

/// One structured query-log record — the plain data behind a single
/// observed query. Carries everything the metrics update and the
/// OTEL-semantic tracing event need; constructing it has no side effects, so
/// it is cheap to build and easy to assert on in tests.
#[derive(Debug, Clone)]
pub struct QueryObservation {
    /// The logical operation (`cypher`, `fts`, `traversal`, …) — the
    /// `db.operation` OTEL field and the `operation` metric label.
    pub operation: String,
    /// The query text — the `db.statement` OTEL field. Kept verbatim; callers
    /// that must redact secrets should do so before constructing the record.
    pub statement: String,
    /// Wall-clock duration of the query, in seconds.
    pub duration_seconds: f64,
    /// Whether the query succeeded.
    pub outcome: QueryOutcome,
}

impl QueryObservation {
    /// Build a query observation.
    pub fn new(
        operation: impl Into<String>,
        statement: impl Into<String>,
        duration_seconds: f64,
        outcome: QueryOutcome,
    ) -> Self {
        Self {
            operation: operation.into(),
            statement: statement.into(),
            duration_seconds,
            outcome,
        }
    }
}

/// The standard set of drevo process metrics, registered once on a [`Registry`]
/// and shared (cloned) wherever they are updated.
///
/// Bundling the handles in one struct keeps the metric names, help text, and
/// label schemas in a single place and gives the HTTP layer and the query log a
/// typed surface ([`DrevoMetrics::record_http`], [`DrevoMetrics::record_query`])
/// instead of stringly-typed registry lookups on the hot path.
#[derive(Debug, Clone)]
pub struct DrevoMetrics {
    /// The registry these metrics live in (for rendering).
    pub registry: Registry,
    /// `drevo_http_requests_total{status="2xx|3xx|4xx|5xx"}`.
    http_requests: [Counter; 4],
    /// `drevo_http_request_duration_seconds` (latency histogram).
    http_duration: Histogram,
    /// `drevo_http_requests_in_flight` (gauge).
    http_in_flight: Gauge,
    /// `drevo_queries_total{status="ok|error"}`.
    queries_ok: Counter,
    /// `drevo_queries_total{status="error"}`.
    queries_error: Counter,
    /// `drevo_query_duration_seconds` (query latency histogram).
    query_duration: Histogram,
    /// `drevo_process_uptime_seconds` (gauge, refreshed by the `/metrics`
    /// handler from the server's start instant).
    pub uptime_seconds: Gauge,
}

impl DrevoMetrics {
    /// Register the standard metric set on a fresh registry and return the
    /// bundle. The crate version is published once as `drevo_build_info`.
    pub fn new() -> Self {
        Self::with_registry(Registry::new())
    }

    /// Register the standard metric set on an existing (typically empty)
    /// registry. Exposed so a process that already owns a registry can fold the
    /// drevo metrics into it.
    pub fn with_registry(registry: Registry) -> Self {
        let http_requests = [
            registry.counter(
                "drevo_http_requests_total",
                "Total HTTP requests handled, by response status class.",
                &[("status", "2xx")],
            ),
            registry.counter(
                "drevo_http_requests_total",
                "Total HTTP requests handled, by response status class.",
                &[("status", "3xx")],
            ),
            registry.counter(
                "drevo_http_requests_total",
                "Total HTTP requests handled, by response status class.",
                &[("status", "4xx")],
            ),
            registry.counter(
                "drevo_http_requests_total",
                "Total HTTP requests handled, by response status class.",
                &[("status", "5xx")],
            ),
        ];
        let http_duration = registry.histogram(
            "drevo_http_request_duration_seconds",
            "HTTP request latency in seconds.",
            &[],
            DEFAULT_LATENCY_BUCKETS_SECONDS,
        );
        let http_in_flight = registry.gauge(
            "drevo_http_requests_in_flight",
            "HTTP requests currently being served.",
            &[],
        );
        let queries_ok = registry.counter(
            "drevo_queries_total",
            "Total queries executed, by outcome.",
            &[("status", "ok")],
        );
        let queries_error = registry.counter(
            "drevo_queries_total",
            "Total queries executed, by outcome.",
            &[("status", "error")],
        );
        let query_duration = registry.histogram(
            "drevo_query_duration_seconds",
            "Query execution latency in seconds.",
            &[],
            DEFAULT_LATENCY_BUCKETS_SECONDS,
        );
        let uptime_seconds = registry.gauge(
            "drevo_process_uptime_seconds",
            "Seconds since the server started serving traffic.",
            &[],
        );
        // Publish the build version as a `1`-valued info gauge — the idiomatic
        // Prometheus way to expose a constant string dimension.
        registry
            .gauge(
                "drevo_build_info",
                "Build information; the value is always 1.",
                &[("version", env!("CARGO_PKG_VERSION"))],
            )
            .set(1);
        Self {
            registry,
            http_requests,
            http_duration,
            http_in_flight,
            queries_ok,
            queries_error,
            query_duration,
            uptime_seconds,
        }
    }

    /// Map an HTTP status code (100–599) to its class index (`1xx`→0 … but only
    /// `2xx`..`5xx` are tracked; `1xx` folds into `2xx`, anything ≥600 into
    /// `5xx`).
    fn status_class_index(status: u16) -> usize {
        match status {
            0..=299 => 0,
            300..=399 => 1,
            400..=499 => 2,
            _ => 3,
        }
    }

    /// Record one completed HTTP request: bump the per-class counter and the
    /// latency histogram.
    pub fn record_http(&self, status: u16, duration_seconds: f64) {
        self.http_requests[Self::status_class_index(status)].inc();
        self.http_duration.observe(duration_seconds);
    }

    /// Mark a request as starting (increment in-flight). Pair with
    /// [`DrevoMetrics::request_finished`].
    pub fn request_started(&self) {
        self.http_in_flight.inc();
    }

    /// Mark a request as finished (decrement in-flight).
    pub fn request_finished(&self) {
        self.http_in_flight.dec();
    }

    /// The current in-flight request count (for tests / introspection).
    pub fn in_flight(&self) -> i64 {
        self.http_in_flight.get()
    }

    /// Record one executed query into the structured query log: update the
    /// outcome counter and the query-latency histogram, and (under the `http`
    /// feature, where `tracing` is compiled) emit a `tracing` event carrying
    /// the OpenTelemetry database semantic-convention fields so an OTLP
    /// exporter layer can turn it into a span.
    pub fn record_query(&self, obs: &QueryObservation) {
        match obs.outcome {
            QueryOutcome::Ok => self.queries_ok.inc(),
            QueryOutcome::Error => self.queries_error.inc(),
        }
        self.query_duration.observe(obs.duration_seconds);
        emit_query_trace(obs);
    }

    /// Render the underlying registry to the Prometheus exposition format.
    pub fn render_prometheus(&self) -> String {
        self.registry.render_prometheus()
    }
}

impl Default for DrevoMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Emit the structured query-log tracing event with OpenTelemetry
/// semantic-convention fields. Compiled only when `tracing` is available (it
/// rides the `http` feature today); a no-op otherwise so the metrics core stays
/// dependency-free.
#[cfg(feature = "http")]
fn emit_query_trace(obs: &QueryObservation) {
    use otel_fields::DB_SYSTEM_VALUE;
    // The `tracing` macro reads a bare string literal as the message, so dotted
    // OTEL keys (`db.system`) cannot be field names directly. We emit
    // valid-identifier fields whose names map one-to-one onto the canonical
    // OTEL attribute keys documented in [`otel_fields`] — `db_system` →
    // `db.system`, `db_operation` → `db.operation`, `db_statement` →
    // `db.statement`, `otel_status_code` → `otel.status_code`. A
    // `tracing-opentelemetry` layer (the opt-in OTLP-export follow-up) applies
    // that dotted rename when turning the event into a span attribute.
    match obs.outcome {
        QueryOutcome::Ok => tracing::info!(
            target: "drevo::query",
            db_system = DB_SYSTEM_VALUE,
            db_operation = %obs.operation,
            db_statement = %obs.statement,
            duration_seconds = obs.duration_seconds,
            otel_status_code = obs.outcome.otel_status(),
            "query executed"
        ),
        QueryOutcome::Error => tracing::warn!(
            target: "drevo::query",
            db_system = DB_SYSTEM_VALUE,
            db_operation = %obs.operation,
            db_statement = %obs.statement,
            duration_seconds = obs.duration_seconds,
            otel_status_code = obs.outcome.otel_status(),
            "query failed"
        ),
    }
}

/// No-op variant for feature-stripped builds (no `tracing` dependency). The
/// metrics still update; only the trace event is skipped.
#[cfg(not(feature = "http"))]
fn emit_query_trace(_obs: &QueryObservation) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc_and_get() {
        let r = Registry::new();
        let c = r.counter("c", "help", &[]);
        assert_eq!(c.get(), 0);
        c.inc();
        c.inc_by(4);
        assert_eq!(c.get(), 5);
    }

    #[test]
    fn counter_clone_shares_state() {
        let r = Registry::new();
        let c1 = r.counter("c", "help", &[]);
        let c2 = c1.clone();
        c1.inc();
        c2.inc();
        assert_eq!(c1.get(), 2);
        assert_eq!(c2.get(), 2);
    }

    #[test]
    fn counter_saturates_on_overflow() {
        let c = Counter::new();
        c.inc_by(u64::MAX);
        c.inc(); // would wrap; must saturate
        assert_eq!(c.get(), u64::MAX);
    }

    #[test]
    fn re_registering_counter_returns_same_handle() {
        let r = Registry::new();
        let a = r.counter("c", "help", &[("k", "v")]);
        a.inc_by(3);
        let b = r.counter("c", "help", &[("k", "v")]);
        assert_eq!(b.get(), 3, "second registration must alias the first");
    }

    #[test]
    fn label_order_is_normalised() {
        let r = Registry::new();
        let a = r.counter("c", "help", &[("a", "1"), ("b", "2")]);
        let b = r.counter("c", "help", &[("b", "2"), ("a", "1")]);
        a.inc();
        assert_eq!(b.get(), 1, "label order must not create a distinct series");
    }

    #[test]
    fn gauge_up_and_down() {
        let r = Registry::new();
        let g = r.gauge("g", "help", &[]);
        g.set(10);
        g.inc();
        g.dec();
        g.dec();
        g.add(-5);
        assert_eq!(g.get(), 4);
    }

    #[test]
    fn histogram_buckets_and_sum() {
        let r = Registry::new();
        let h = r.histogram("h", "help", &[], &[1.0, 5.0, 10.0]);
        for v in [0.5, 2.0, 2.0, 7.0, 100.0] {
            h.observe(v);
        }
        assert_eq!(h.count(), 5);
        assert!((h.sum() - 111.5).abs() < 1e-9);
        // <=1.0: {0.5} = 1
        assert_eq!(h.cumulative_at(0), 1);
        // <=5.0: {0.5, 2.0, 2.0} = 3
        assert_eq!(h.cumulative_at(1), 3);
        // <=10.0: {0.5, 2.0, 2.0, 7.0} = 4 (100.0 only in +Inf)
        assert_eq!(h.cumulative_at(2), 4);
    }

    #[test]
    fn histogram_drops_nonfinite_from_sum_but_counts_it() {
        let r = Registry::new();
        let h = r.histogram("h", "help", &[], &[1.0]);
        h.observe(0.5);
        h.observe(f64::INFINITY);
        h.observe(f64::NAN);
        assert_eq!(h.count(), 3, "all observations counted");
        assert!((h.sum() - 0.5).abs() < 1e-9, "sum excludes non-finite");
    }

    #[test]
    fn histogram_sorts_and_dedups_bounds() {
        let r = Registry::new();
        let h = r.histogram("h", "help", &[], &[10.0, 1.0, 1.0, f64::NAN, 5.0]);
        // Bounds become [1.0, 5.0, 10.0].
        h.observe(0.5);
        h.observe(3.0);
        assert_eq!(h.cumulative_at(0), 1);
        assert_eq!(h.cumulative_at(1), 2);
    }

    #[test]
    fn render_counter_with_labels() {
        let r = Registry::new();
        r.counter("drevo_x_total", "an x", &[("status", "ok")])
            .inc();
        let out = r.render_prometheus();
        assert!(out.contains("# HELP drevo_x_total an x\n"), "{out}");
        assert!(out.contains("# TYPE drevo_x_total counter\n"), "{out}");
        assert!(out.contains("drevo_x_total{status=\"ok\"} 1\n"), "{out}");
    }

    #[test]
    fn render_help_type_emitted_once_per_name() {
        let r = Registry::new();
        r.counter("drevo_x_total", "an x", &[("status", "ok")])
            .inc();
        r.counter("drevo_x_total", "an x", &[("status", "err")])
            .inc();
        let out = r.render_prometheus();
        assert_eq!(
            out.matches("# TYPE drevo_x_total counter").count(),
            1,
            "TYPE must appear once across both label series:\n{out}"
        );
        assert!(out.contains("drevo_x_total{status=\"ok\"} 1"));
        assert!(out.contains("drevo_x_total{status=\"err\"} 1"));
    }

    #[test]
    fn render_histogram_shape() {
        let r = Registry::new();
        let h = r.histogram("drevo_lat_seconds", "latency", &[], &[1.0, 5.0]);
        h.observe(0.5);
        h.observe(2.0);
        let out = r.render_prometheus();
        assert!(out.contains("# TYPE drevo_lat_seconds histogram"), "{out}");
        assert!(
            out.contains("drevo_lat_seconds_bucket{le=\"1\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("drevo_lat_seconds_bucket{le=\"5\"} 2"),
            "{out}"
        );
        assert!(
            out.contains("drevo_lat_seconds_bucket{le=\"+Inf\"} 2"),
            "{out}"
        );
        assert!(out.contains("drevo_lat_seconds_sum 2.5"), "{out}");
        assert!(out.contains("drevo_lat_seconds_count 2"), "{out}");
    }

    #[test]
    fn label_value_escaping() {
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn float_format_infinities() {
        assert_eq!(format_float(f64::INFINITY), "+Inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_float(0.005), "0.005");
    }

    #[test]
    fn drevo_metrics_record_http_classes() {
        let m = DrevoMetrics::new();
        m.record_http(200, 0.001);
        m.record_http(204, 0.002);
        m.record_http(301, 0.001);
        m.record_http(404, 0.001);
        m.record_http(500, 0.5);
        let out = m.render_prometheus();
        assert!(
            out.contains("drevo_http_requests_total{status=\"2xx\"} 2"),
            "{out}"
        );
        assert!(
            out.contains("drevo_http_requests_total{status=\"3xx\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("drevo_http_requests_total{status=\"4xx\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("drevo_http_requests_total{status=\"5xx\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("drevo_http_request_duration_seconds_count 5"),
            "{out}"
        );
    }

    #[test]
    fn drevo_metrics_in_flight_tracks() {
        let m = DrevoMetrics::new();
        m.request_started();
        m.request_started();
        assert_eq!(m.in_flight(), 2);
        m.request_finished();
        assert_eq!(m.in_flight(), 1);
    }

    #[test]
    fn drevo_metrics_record_query_outcomes() {
        let m = DrevoMetrics::new();
        m.record_query(&QueryObservation::new(
            "cypher",
            "MATCH (n) RETURN n",
            0.01,
            QueryOutcome::Ok,
        ));
        m.record_query(&QueryObservation::new(
            "fts",
            "bad",
            0.02,
            QueryOutcome::Error,
        ));
        m.record_query(&QueryObservation::new(
            "cypher",
            "MATCH (n) RETURN n",
            0.03,
            QueryOutcome::Ok,
        ));
        let out = m.render_prometheus();
        assert!(
            out.contains("drevo_queries_total{status=\"ok\"} 2"),
            "{out}"
        );
        assert!(
            out.contains("drevo_queries_total{status=\"error\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("drevo_query_duration_seconds_count 3"),
            "{out}"
        );
    }

    #[test]
    fn build_info_published() {
        let m = DrevoMetrics::new();
        let out = m.render_prometheus();
        let expected = format!(
            "drevo_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        assert!(out.contains(&expected), "{out}");
    }

    #[test]
    fn uptime_gauge_settable() {
        let m = DrevoMetrics::new();
        m.uptime_seconds.set(42);
        let out = m.render_prometheus();
        assert!(out.contains("drevo_process_uptime_seconds 42"), "{out}");
    }

    #[test]
    fn otel_field_constants_are_canonical() {
        assert_eq!(otel_fields::DB_SYSTEM, "db.system");
        assert_eq!(otel_fields::DB_OPERATION, "db.operation");
        assert_eq!(otel_fields::DB_STATEMENT, "db.statement");
        assert_eq!(otel_fields::OTEL_STATUS_CODE, "otel.status_code");
        assert_eq!(otel_fields::DB_SYSTEM_VALUE, "drevo");
    }

    #[test]
    fn query_outcome_labels() {
        assert_eq!(QueryOutcome::Ok.label(), "ok");
        assert_eq!(QueryOutcome::Error.label(), "error");
        assert_eq!(QueryOutcome::Ok.otel_status(), "OK");
        assert_eq!(QueryOutcome::Error.otel_status(), "ERROR");
    }

    #[test]
    fn render_is_deterministic_in_registration_order() {
        let r = Registry::new();
        r.counter("z_total", "z", &[]).inc();
        r.counter("a_total", "a", &[]).inc();
        let first = r.render_prometheus();
        let second = r.render_prometheus();
        assert_eq!(first, second);
        let z_pos = first.find("z_total").expect("z present");
        let a_pos = first.find("a_total").expect("a present");
        assert!(z_pos < a_pos, "registration order preserved:\n{first}");
    }

    #[test]
    fn concurrent_counter_increments_are_lossless() {
        use std::thread;
        let r = Registry::new();
        let c = r.counter("c", "help", &[]);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    c.inc();
                }
            }));
        }
        for h in handles {
            h.join().expect("thread joined");
        }
        assert_eq!(c.get(), 8000);
    }
}
