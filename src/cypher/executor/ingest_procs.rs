//! The `drevo.ingest.*` procedures: store an ingest job of passages in the
//! graph, watch it, and steer it. The scheduler in [`crate::ingest`] runs
//! the jobs; these procedures only read and write the `:IngestJob` and
//! `:IngestItem` nodes, so a job submitted in a transaction exists once it
//! commits.

use super::*;
use crate::cypher::provenance::SourceRef;
use crate::ingest::progress::Progress;
use crate::ingest::state::{self, ItemState, JobAction, JobState};
use crate::ingest::{split_items, JobConfig, Pipeline};

/// `CALL drevo.ingest.submit(items, config) YIELD jobId, total`.
pub(super) const SUBMIT: &str = "drevo.ingest.submit";
/// `CALL drevo.ingest.status(jobId) YIELD job, items`.
pub(super) const STATUS: &str = "drevo.ingest.status";
/// `CALL drevo.ingest.list(state, limit) YIELD job`.
pub(super) const LIST: &str = "drevo.ingest.list";
/// `CALL drevo.ingest.cancel(jobId) YIELD job`.
pub(super) const CANCEL: &str = "drevo.ingest.cancel";
/// `CALL drevo.ingest.pause(jobId) YIELD job`.
pub(super) const PAUSE: &str = "drevo.ingest.pause";
/// `CALL drevo.ingest.resume(jobId) YIELD job`.
pub(super) const RESUME: &str = "drevo.ingest.resume";
/// `CALL drevo.ingest.retryFailed(jobId) YIELD requeued`.
pub(super) const RETRY_FAILED: &str = "drevo.ingest.retryFailed";
/// `CALL drevo.ingest.errors(jobId, limit) YIELD seq, attempts, last_error`.
pub(super) const ERRORS: &str = "drevo.ingest.errors";
/// `CALL drevo.ingest.purge(jobId) YIELD deleted`.
pub(super) const PURGE: &str = "drevo.ingest.purge";

/// The label of a job node.
pub(crate) const JOB_LABEL: &str = "IngestJob";
/// The label of an item node.
pub(crate) const ITEM_LABEL: &str = "IngestItem";

/// Whether `name` is one of the `drevo.ingest.*` procedures.
pub(super) fn is_ingest_procedure(name: &str) -> bool {
    matches!(
        name,
        SUBMIT | STATUS | LIST | CANCEL | PAUSE | RESUME | RETRY_FAILED | ERRORS | PURGE
    )
}

/// The columns of `drevo.ingest.*` procedure `name`.
pub(super) fn columns(name: &str) -> Option<&'static [&'static str]> {
    match name {
        SUBMIT => Some(&["jobId", "total"]),
        STATUS => Some(&["job", "items"]),
        LIST | CANCEL | PAUSE | RESUME => Some(&["job"]),
        RETRY_FAILED => Some(&["requeued"]),
        ERRORS => Some(&["seq", "attempts", "last_error"]),
        PURGE => Some(&["deleted"]),
        _ => None,
    }
}

/// How many arguments `drevo.ingest.*` procedure `name` takes.
pub(super) fn arity(name: &str) -> Option<usize> {
    match name {
        SUBMIT | LIST | ERRORS => Some(2),
        STATUS | CANCEL | PAUSE | RESUME | RETRY_FAILED | PURGE => Some(1),
        _ => None,
    }
}

/// The `InvalidProcedureCall` of procedure `name`.
fn refusal(name: &str, message: String, span: Span) -> ExecError {
    ExecError::InvalidProcedureCall {
        name: name.to_string(),
        message,
        span,
    }
}

/// A count as a Cypher integer.
fn int(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

/// A string property of `node`.
fn text<'n>(node: &'n NodeValue, key: &str) -> Option<&'n str> {
    match node.properties.get(key) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// An integer property of `node`, 0 when it has none.
fn number(node: &NodeValue, key: &str) -> i64 {
    match node.properties.get(key) {
        Some(Value::Integer(n)) => *n,
        _ => 0,
    }
}

/// The `ingest` event of `job`'s progress.
fn progress_of(job: &NodeValue) -> Progress {
    let count = |key: &str| u64::try_from(number(job, key)).unwrap_or(0);
    Progress {
        job_id: text(job, "id").unwrap_or_default().to_string(),
        state: text(job, "state").unwrap_or_default().to_string(),
        done: count("done"),
        failed: count("failed"),
        total: count("total"),
    }
}

/// Post `job`'s progress with the statement: it reaches `/events` and wakes
/// the scheduler once the statement commits.
fn post_progress(job: &NodeValue) {
    crate::event_bus::post(crate::event_bus::Pending {
        name: "ingest".into(),
        payload: progress_of(job).payload(),
        kind: crate::event_bus::EventKind::Ingest,
    });
}

/// How a value is shown in an error: a number or string as itself, anything
/// else by its type.
fn describe(v: &Value) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        Value::String(s) => format!("'{s}'"),
        other => other.type_name().to_string(),
    }
}

impl Executor<'_> {
    /// Run one of the `drevo.ingest.*` procedures, once per input row.
    pub(super) fn run_ingest(
        &mut self,
        name: &str,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        match name {
            SUBMIT => self.run_per_row_mut(c, columns, &mut |exec, row| exec.ingest_submit(c, row)),
            STATUS => self.run_per_row(c, columns, &mut |exec, row| exec.ingest_status(c, row)),
            LIST => self.run_per_row(c, columns, &mut |exec, row| exec.ingest_list(c, row)),
            ERRORS => self.run_per_row(c, columns, &mut |exec, row| exec.ingest_errors(c, row)),
            RETRY_FAILED => {
                self.run_per_row_mut(c, columns, &mut |exec, row| exec.ingest_retry(c, row))
            }
            PURGE => self.run_per_row_mut(c, columns, &mut |exec, row| exec.ingest_purge(c, row)),
            _ => self.run_per_row_mut(c, columns, &mut |exec, row| exec.ingest_move(name, c, row)),
        }
    }

    /// Refuse a change to a job the signed-in user may not make: steering a
    /// job writes its node, whether or not anything else changes.
    fn ingest_guard_write(&self) -> ExecResultT<()> {
        self.guard_node(
            drevo_auth::privileges::Action::SetProperty,
            &[JOB_LABEL.to_string()],
            Some("state"),
        )
    }

    /// The job id argument of `name`.
    fn ingest_job_id(&self, name: &str, c: &CallClause, row: &Bindings) -> ExecResultT<String> {
        match self.eval(&c.args[0], row)? {
            Value::String(id) => Ok(id),
            other => Err(refusal(
                name,
                format!("the job id must be a string, got {}", other.type_name()),
                c.span,
            )),
        }
    }

    /// The `:IngestJob` with id `id`.
    fn ingest_job(&self, name: &str, id: &str, span: Span) -> ExecResultT<Arc<NodeValue>> {
        self.memory_node_by_id(JOB_LABEL, id)?
            .ok_or_else(|| refusal(name, format!("no ingest job `{id}`"), span))
    }

    /// The state of `job`.
    fn ingest_job_state(&self, name: &str, job: &NodeValue, span: Span) -> ExecResultT<JobState> {
        let found = text(job, "state").unwrap_or_default();
        JobState::parse(found).ok_or_else(|| {
            refusal(
                name,
                format!(
                    "job `{}` has an unknown state `{found}`",
                    text(job, "id").unwrap_or_default()
                ),
                span,
            )
        })
    }

    /// The items of job `id`, in `seq` order.
    fn ingest_items(&self, id: &str) -> ExecResultT<Vec<Arc<NodeValue>>> {
        let mut items = Vec::new();
        for node in self.engine().all_nodes()? {
            if !node_labels_from_storage(&node)
                .iter()
                .any(|l| l == ITEM_LABEL)
            {
                continue;
            }
            let nv = node_to_value(&node);
            if text(&nv, "job_id") == Some(id) {
                items.push(nv);
            }
        }
        items.sort_by_key(|i| number(i, "seq"));
        Ok(items)
    }

    /// `drevo.ingest.submit`: store the job and its passages, queued.
    fn ingest_submit(&mut self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        const PROC: &str = SUBMIT;
        let span = c.span;
        self.guard_node(
            drevo_auth::privileges::Action::Create,
            &[JOB_LABEL.to_string()],
            None,
        )?;
        let items = self.eval(&c.args[0], row)?;
        let config = self.eval(&c.args[1], row)?;
        let settings = crate::ingest::settings();
        let job_config = JobConfig::from_value(&config, settings.max_attempts)
            .map_err(|m| refusal(PROC, m, span))?;
        let specs = split_items(&items, job_config.options.chunk_chars)
            .map_err(|m| refusal(PROC, m, span))?;
        if let Some(SourceRef::Id(id)) = &job_config.options.source {
            self.memory_source(PROC, id, span)?;
        }
        let config_json = value_to_json(&job_config.extract)
            .unwrap_or(serde_json::Value::Null)
            .to_string();
        let now = Self::memory_now(span)?;
        let id = uuid::Uuid::now_v7().to_string();
        let total = specs.len();
        let mut props = vec![
            ("id", Value::String(id.clone())),
            ("state", Value::String(JobState::Queued.as_str().into())),
            ("pipeline", Value::String(Pipeline::Extract.as_str().into())),
            ("total", int(total)),
            ("done", Value::Integer(0)),
            ("failed", Value::Integer(0)),
            ("created_at", now.clone()),
            ("updated_at", now.clone()),
            ("config", Value::String(config_json)),
            ("source_id", Value::String(job_config.source_id.clone())),
            ("priority", Value::Integer(job_config.priority)),
            (
                "max_attempts",
                Value::Integer(i64::from(job_config.max_attempts)),
            ),
        ];
        if let Some(concurrency) = job_config.concurrency {
            props.push(("concurrency", Value::Integer(concurrency)));
        }
        let job = self.memory_create_node(&[JOB_LABEL], props, span)?;
        for (seq, spec) in specs.into_iter().enumerate() {
            let mut props = vec![
                ("job_id", Value::String(id.clone())),
                ("seq", int(seq)),
                ("state", Value::String(ItemState::Queued.as_str().into())),
                ("attempts", Value::Integer(0)),
                ("next_attempt_at", Value::Integer(0)),
                ("text", Value::String(spec.text)),
                ("offset", int(spec.offset)),
                ("length", int(spec.length)),
                ("created_at", now.clone()),
            ];
            if let Some(meta) = spec.meta {
                props.push(("meta", Value::String(meta)));
            }
            let item = self.memory_create_node(&[ITEM_LABEL], props, span)?;
            self.memory_link(&job, "HAS_ITEM", &item, Vec::new(), span)?;
        }
        post_progress(&job);
        Ok(vec![vec![Value::String(id), int(total)]])
    }

    /// Store a job of the `community` pipeline, queued: one item, run as
    /// `drevo.community.summarize(config)` where `config` is the JSON of
    /// the summarize config map. Returns the job's id.
    pub(super) fn ingest_store_community_job(
        &mut self,
        config_json: String,
        span: Span,
    ) -> ExecResultT<String> {
        self.guard_node(
            drevo_auth::privileges::Action::Create,
            &[JOB_LABEL.to_string()],
            None,
        )?;
        let now = Self::memory_now(span)?;
        let id = uuid::Uuid::now_v7().to_string();
        let max_attempts = crate::ingest::settings().max_attempts;
        let job = self.memory_create_node(
            &[JOB_LABEL],
            vec![
                ("id", Value::String(id.clone())),
                ("state", Value::String(JobState::Queued.as_str().into())),
                (
                    "pipeline",
                    Value::String(Pipeline::Community.as_str().into()),
                ),
                ("total", Value::Integer(1)),
                ("done", Value::Integer(0)),
                ("failed", Value::Integer(0)),
                ("created_at", now.clone()),
                ("updated_at", now.clone()),
                ("config", Value::String(config_json)),
                ("priority", Value::Integer(0)),
                ("max_attempts", Value::Integer(i64::from(max_attempts))),
            ],
            span,
        )?;
        let item = self.memory_create_node(
            &[ITEM_LABEL],
            vec![
                ("job_id", Value::String(id.clone())),
                ("seq", Value::Integer(0)),
                ("state", Value::String(ItemState::Queued.as_str().into())),
                ("attempts", Value::Integer(0)),
                ("next_attempt_at", Value::Integer(0)),
                ("text", Value::String(String::new())),
                ("offset", Value::Integer(0)),
                ("length", Value::Integer(0)),
                ("created_at", now),
            ],
            span,
        )?;
        self.memory_link(&job, "HAS_ITEM", &item, Vec::new(), span)?;
        post_progress(&job);
        Ok(id)
    }

    /// `drevo.ingest.status`: the job node and its items counted by state.
    fn ingest_status(&self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        let id = self.ingest_job_id(STATUS, c, row)?;
        let job = self.ingest_job(STATUS, &id, c.span)?;
        let mut counts: BTreeMap<String, Value> = [
            ItemState::Queued,
            ItemState::Running,
            ItemState::Done,
            ItemState::Failed,
            ItemState::Dead,
        ]
        .into_iter()
        .map(|s| (s.as_str().to_string(), Value::Integer(0)))
        .collect();
        for item in self.ingest_items(&id)? {
            if let Some(Value::Integer(n)) = text(&item, "state").and_then(|s| counts.get_mut(s)) {
                *n += 1;
            }
        }
        Ok(vec![vec![Value::Node(job), Value::Map(counts)]])
    }

    /// `drevo.ingest.list`: the jobs in `state` (all for `null`), newest
    /// first, at most `limit`.
    fn ingest_list(&self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        let span = c.span;
        let wanted = match self.eval(&c.args[0], row)? {
            Value::Null => None,
            Value::String(s) if JobState::parse(&s).is_some() => Some(s),
            other => {
                return Err(refusal(
                    LIST,
                    format!(
                        "state must be one of queued, running, paused, done, failed, \
                         cancelled, or null, got {}",
                        describe(&other)
                    ),
                    span,
                ))
            }
        };
        let limit = match self.eval(&c.args[1], row)? {
            Value::Integer(n) if n >= 0 => usize::try_from(n).unwrap_or(usize::MAX),
            other => {
                return Err(refusal(
                    LIST,
                    format!(
                        "limit must be a non-negative integer, got {}",
                        describe(&other)
                    ),
                    span,
                ))
            }
        };
        let mut jobs = Vec::new();
        for node in self.engine().all_nodes()? {
            if !node_labels_from_storage(&node)
                .iter()
                .any(|l| l == JOB_LABEL)
            {
                continue;
            }
            let Some(job) = self.seen_node(&node) else {
                continue;
            };
            if wanted
                .as_deref()
                .is_none_or(|w| text(&job, "state") == Some(w))
            {
                jobs.push(job);
            }
        }
        let key = |j: &Arc<NodeValue>| {
            (
                text(j, "created_at").unwrap_or_default().to_string(),
                text(j, "id").unwrap_or_default().to_string(),
            )
        };
        jobs.sort_by_key(|j| std::cmp::Reverse(key(j)));
        Ok(jobs
            .into_iter()
            .take(limit)
            .map(|j| vec![Value::Node(j)])
            .collect())
    }

    /// `drevo.ingest.errors`: the items of a job with an error, in `seq`
    /// order, at most `limit`.
    fn ingest_errors(&self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        let id = self.ingest_job_id(ERRORS, c, row)?;
        self.ingest_job(ERRORS, &id, c.span)?;
        let limit = match self.eval(&c.args[1], row)? {
            Value::Integer(n) if n >= 0 => usize::try_from(n).unwrap_or(usize::MAX),
            other => {
                return Err(refusal(
                    ERRORS,
                    format!(
                        "limit must be a non-negative integer, got {}",
                        describe(&other)
                    ),
                    c.span,
                ))
            }
        };
        Ok(self
            .ingest_items(&id)?
            .into_iter()
            .filter_map(|item| {
                let error = text(&item, "last_error")?.to_string();
                Some(vec![
                    Value::Integer(number(&item, "seq")),
                    Value::Integer(number(&item, "attempts")),
                    Value::String(error),
                ])
            })
            .take(limit)
            .collect())
    }

    /// `drevo.ingest.cancel` / `pause` / `resume`: move the job.
    fn ingest_move(&mut self, name: &str, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        let span = c.span;
        self.ingest_guard_write()?;
        let id = self.ingest_job_id(name, c, row)?;
        let job = self.ingest_job(name, &id, span)?;
        let from = self.ingest_job_state(name, &job, span)?;
        let action = match name {
            PAUSE => JobAction::Pause,
            CANCEL => JobAction::Cancel,
            _ => JobAction::Resume {
                started: text(&job, "started_at").is_some(),
            },
        };
        let to = state::job(from, action).map_err(|m| refusal(name, m, span))?;
        let now = Self::memory_now(span)?;
        let mut props = vec![
            ("state", Value::String(to.as_str().into())),
            ("updated_at", now.clone()),
        ];
        if to.finished() {
            props.push(("finished_at", now));
        }
        let job = self.memory_set_props(job, props)?;
        post_progress(&job);
        Ok(vec![vec![Value::Node(job)]])
    }

    /// `drevo.ingest.retryFailed`: queue the dead items again with fresh
    /// attempts; a failed job runs again.
    fn ingest_retry(&mut self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        const PROC: &str = RETRY_FAILED;
        let span = c.span;
        self.ingest_guard_write()?;
        let id = self.ingest_job_id(PROC, c, row)?;
        let job = self.ingest_job(PROC, &id, span)?;
        let from = self.ingest_job_state(PROC, &job, span)?;
        let dead: Vec<Arc<NodeValue>> = self
            .ingest_items(&id)?
            .into_iter()
            .filter(|i| text(i, "state") == Some(ItemState::Dead.as_str()))
            .collect();
        let to = state::job(
            from,
            JobAction::RetryFailed {
                requeued: !dead.is_empty(),
            },
        )
        .map_err(|m| refusal(PROC, m, span))?;
        let now = Self::memory_now(span)?;
        let requeued = dead.len();
        for item in dead {
            let next = state::item(ItemState::Dead, state::ItemAction::Requeue)
                .map_err(|m| refusal(PROC, m, span))?;
            self.memory_set_props(
                item,
                vec![
                    ("state", Value::String(next.as_str().into())),
                    ("attempts", Value::Integer(0)),
                    ("next_attempt_at", Value::Integer(0)),
                    ("updated_at", now.clone()),
                ],
            )?;
        }
        if requeued > 0 || to != from {
            let failed = (number(&job, "failed") - int_of(requeued)).max(0);
            let mut props = vec![
                ("state", Value::String(to.as_str().into())),
                ("failed", Value::Integer(failed)),
                ("updated_at", now),
            ];
            if !to.finished() {
                props.push(("finished_at", Value::Null));
            }
            let job = self.memory_set_props(job, props)?;
            post_progress(&job);
        }
        Ok(vec![vec![int(requeued)]])
    }

    /// `drevo.ingest.purge`: delete a finished job's items with their texts;
    /// the job node keeps its counters.
    fn ingest_purge(&mut self, c: &CallClause, row: &Bindings) -> ExecResultT<Rows> {
        const PROC: &str = PURGE;
        let span = c.span;
        self.ingest_guard_write()?;
        let id = self.ingest_job_id(PROC, c, row)?;
        let job = self.ingest_job(PROC, &id, span)?;
        let from = self.ingest_job_state(PROC, &job, span)?;
        if !from.finished() {
            return Err(refusal(
                PROC,
                format!(
                    "job `{id}` is {}: cancel it or let it finish before purging it",
                    from.as_str()
                ),
                span,
            ));
        }
        let items = self.ingest_items(&id)?;
        if items
            .iter()
            .any(|i| text(i, "state") == Some(ItemState::Running.as_str()))
        {
            return Err(refusal(
                PROC,
                format!("job `{id}` still has an item running: purge it once that finishes"),
                span,
            ));
        }
        let deleted = items.len();
        for item in items {
            self.guard_node(drevo_auth::privileges::Action::Delete, &item.labels, None)?;
            let edges = self.engine().edges_of(item.id, ModelDirection::Both)?.len();
            self.engine().delete_node(item.id)?;
            self.stats.nodes_deleted += 1;
            self.stats.relationships_deleted += edges;
        }
        if deleted > 0 {
            let now = Self::memory_now(span)?;
            self.memory_set_props(job, vec![("purged_at", now.clone()), ("updated_at", now)])?;
        }
        Ok(vec![vec![int(deleted)]])
    }
}

/// A count as an `i64`.
fn int_of(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
