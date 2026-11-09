//! Durable ingest jobs: a long text, or many texts, extracted into long-term
//! memory one passage at a time, with retries, in the background.
//!
//! `CALL drevo.ingest.submit(items, config)` stores a job in the graph — an
//! `:IngestJob` node with one `:IngestItem` per passage, each holding its
//! text — and returns at once. A scheduler per database runs the items: each
//! one is a `drevo.extract.ingest` of its passage, written in one
//! transaction, after which the item points at the `:Chunk` it produced. An
//! item whose model call fails is tried again later, waiting longer each
//! time, until it has used `maxAttempts`; then it is `dead`. Because the
//! jobs live in the graph they survive a restart: an item that was running
//! when the server stopped is run again.
//!
//! ```text
//! CALL drevo.ingest.submit({text: $book}, {source: {id: 'book-1', title: 'The Book'}})
//! YIELD jobId, total
//!
//! CALL drevo.ingest.status($jobId) YIELD job, items
//! RETURN job.state, job.done, job.total, items.dead
//! ```
//!
//! [`configure`](crate::ingest::configure) sets the server-wide limits: how many items are in flight
//! at once across every database, how many attempts an item gets, and the
//! base of the retry delay.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod backoff;
pub(crate) mod progress;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod scheduler;
pub(crate) mod state;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, RwLock};
use std::time::Duration;

use crate::cypher::executor::Value;
use crate::extract::ExtractOptions;

/// Items in flight at once across the server when nothing is configured.
pub(crate) const DEFAULT_WORKERS: usize = 2;
/// Attempts an item gets when its job names none.
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 5;
/// The base of the retry delay when nothing is configured.
pub(crate) const DEFAULT_RETRY_BASE: Duration = Duration::from_millis(2000);
/// The most attempts a job may ask for.
pub(crate) const MAX_ATTEMPTS_LIMIT: i64 = 100;

/// The server-wide settings of ingest jobs.
///
/// ```
/// use std::time::Duration;
/// use drevo::ingest::{configure, settings, IngestSettings};
///
/// configure(IngestSettings { workers: 4, ..IngestSettings::default() });
/// assert_eq!(settings().workers, 4);
/// assert_eq!(settings().retry_base, Duration::from_millis(2000));
/// configure(IngestSettings::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestSettings {
    /// How many items are in flight at once, across every database
    /// (`DREVO_INGEST_WORKERS`, default 2). A job's `concurrency` is capped
    /// by it.
    pub workers: usize,
    /// How many attempts an item gets when its job names no `maxAttempts`
    /// (`DREVO_INGEST_MAX_ATTEMPTS`, default 5).
    pub max_attempts: u32,
    /// The retry delay after attempt `n` is `retry_base × 2^n`, at most 5
    /// minutes, less a random part of up to half
    /// (`DREVO_INGEST_RETRY_BASE_MS`, default 2000 ms).
    pub retry_base: Duration,
    /// Seeds the random part of the retry delay, for repeatable runs;
    /// `None` seeds it from the clock.
    pub jitter_seed: Option<u64>,
}

impl Default for IngestSettings {
    fn default() -> Self {
        Self {
            workers: DEFAULT_WORKERS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            retry_base: DEFAULT_RETRY_BASE,
            jitter_seed: None,
        }
    }
}

static SETTINGS: RwLock<IngestSettings> = RwLock::new(IngestSettings {
    workers: DEFAULT_WORKERS,
    max_attempts: DEFAULT_MAX_ATTEMPTS,
    retry_base: DEFAULT_RETRY_BASE,
    jitter_seed: None,
});

/// Use `settings` from now on, in every database; a `workers` or
/// `max_attempts` of 0 counts as 1.
pub fn configure(settings: IngestSettings) {
    let settings = IngestSettings {
        workers: settings.workers.max(1),
        max_attempts: settings.max_attempts.max(1),
        ..settings
    };
    *SETTINGS.write().unwrap_or_else(|e| e.into_inner()) = settings;
    wake();
}

/// The settings in use.
#[must_use]
pub fn settings() -> IngestSettings {
    *SETTINGS.read().unwrap_or_else(|e| e.into_inner())
}

/// Bumped on every change a scheduler may act on.
static WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

/// The current wake-up count.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn generation() -> u64 {
    *WAKE.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wake every scheduler: a job was submitted, resumed or retried, an item
/// finished, or a setting changed.
pub(crate) fn wake() {
    let mut generation = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    *generation = generation.wrapping_add(1);
    WAKE.1.notify_all();
}

/// Wait until the wake-up count moves past `seen`, or `timeout` passes
/// (`None`: no limit).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn wait(seen: u64, timeout: Option<Duration>) {
    let guard = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    let guard = match timeout {
        Some(timeout) => {
            WAKE.1
                .wait_timeout_while(guard, timeout, |g| *g == seen)
                .unwrap_or_else(|e| e.into_inner())
                .0
        }
        None => WAKE
            .1
            .wait_while(guard, |g| *g == seen)
            .unwrap_or_else(|e| e.into_inner()),
    };
    drop(guard);
}

/// How an item attempt ended, for `drevo_ingest_items_total{state}`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Written.
    Done,
    /// Failed, to be tried again.
    Failed,
    /// Failed for good.
    Dead,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
static ITEMS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
static JOBS_ACTIVE: AtomicI64 = AtomicI64::new(0);

/// Count an item attempt that ended with `outcome`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn record(outcome: Outcome) {
    ITEMS[outcome as usize].fetch_add(1, Ordering::Relaxed);
}

/// Item attempts so far that ended done, failed and dead, in that order.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn item_totals() -> [u64; 3] {
    [
        ITEMS[0].load(Ordering::Relaxed),
        ITEMS[1].load(Ordering::Relaxed),
        ITEMS[2].load(Ordering::Relaxed),
    ]
}

/// Jobs queued or running, in every database.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn jobs_active() -> i64 {
    JOBS_ACTIVE.load(Ordering::Relaxed).max(0)
}

/// Move the count of active jobs by `delta`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn adjust_jobs_active(delta: i64) {
    JOBS_ACTIVE.fetch_add(delta, Ordering::Relaxed);
}

/// The config keys only `drevo.ingest.submit` takes.
pub(crate) const JOB_KEYS: [&str; 3] = ["priority", "maxAttempts", "concurrency"];

/// The keys a job config accepts, as listed in errors.
pub(crate) const CONFIG_KEYS: &str = "types, relations, schemaHint, language, model, \
     chunkChars, minConfidence, source, session, resolve, priority, maxAttempts, concurrency";

/// A `drevo.ingest.submit` config, parsed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobConfig {
    /// What each item's `drevo.extract.ingest` is given: the config less the
    /// job keys.
    pub(crate) extract: Value,
    /// The same, parsed.
    pub(crate) options: ExtractOptions,
    /// Higher runs first.
    pub(crate) priority: i64,
    /// Attempts per item.
    pub(crate) max_attempts: u32,
    /// Items of this job in flight at once; `None` is the server's cap.
    pub(crate) concurrency: Option<i64>,
    /// The id of the source every item is a passage of.
    pub(crate) source_id: String,
}

impl JobConfig {
    /// Parse a submit config; a job that names no `maxAttempts` gets
    /// `default_max_attempts`.
    ///
    /// # Errors
    ///
    /// A message naming what is wrong: no `source`, an unknown key (listing
    /// the accepted ones), `dryRun`, a job key out of range, or anything
    /// `drevo.extract.ingest` would refuse.
    pub(crate) fn from_value(value: &Value, default_max_attempts: u32) -> Result<Self, String> {
        const NEEDS_SOURCE: &str = "drevo.ingest.submit needs a config with a `source`: an id \
             or a map {id, uri, title, kind}";
        let map = match value {
            Value::Null => return Err(NEEDS_SOURCE.into()),
            Value::Map(map) => map,
            other => {
                return Err(format!(
                    "the config must be a map, got {}",
                    other.type_name()
                ))
            }
        };
        if matches!(map.get("source"), None | Some(Value::Null)) {
            return Err(NEEDS_SOURCE.into());
        }
        let mut extract = BTreeMap::new();
        let mut priority = 0;
        let mut max_attempts = default_max_attempts.max(1);
        let mut concurrency = None;
        for (key, v) in map {
            match key.as_str() {
                "dryRun" => return Err("`dryRun` does not apply to ingest jobs".into()),
                k if JOB_KEYS.contains(&k) && matches!(v, Value::Null) => {}
                "priority" => match v {
                    Value::Integer(n) => priority = *n,
                    other => {
                        return Err(format!(
                            "priority must be an integer, got {}",
                            other.type_name()
                        ))
                    }
                },
                "maxAttempts" => {
                    max_attempts = match v {
                        Value::Integer(n) if (1..=MAX_ATTEMPTS_LIMIT).contains(n) => {
                            u32::try_from(*n).unwrap_or(1)
                        }
                        other => {
                            return Err(format!(
                                "maxAttempts must be an integer from 1 to \
                                 {MAX_ATTEMPTS_LIMIT}, got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "concurrency" => match v {
                    Value::Integer(n) if *n >= 1 => concurrency = Some(*n),
                    other => {
                        return Err(format!(
                            "concurrency must be an integer of at least 1, got {}",
                            describe(other)
                        ))
                    }
                },
                k if EXTRACT_KEYS.contains(&k) => {
                    extract.insert(key.clone(), v.clone());
                }
                other => {
                    return Err(format!(
                        "unknown config key `{other}`; expected {CONFIG_KEYS}"
                    ))
                }
            }
        }
        let extract = Value::Map(extract);
        let options = ExtractOptions::from_value(&extract, true)?;
        let source_id = match &options.source {
            Some(crate::cypher::provenance::SourceRef::Id(id)) => id.clone(),
            Some(crate::cypher::provenance::SourceRef::Spec(spec)) => spec.id.clone(),
            None => return Err(NEEDS_SOURCE.into()),
        };
        Ok(Self {
            extract,
            options,
            priority,
            max_attempts,
            concurrency,
            source_id,
        })
    }
}

/// The config keys a job hands on to `drevo.extract.ingest`.
const EXTRACT_KEYS: [&str; 10] = [
    "types",
    "relations",
    "schemaHint",
    "language",
    "model",
    "chunkChars",
    "minConfidence",
    "source",
    "session",
    "resolve",
];

/// How a value is shown in an error: an integer as itself, anything else by
/// its type.
fn describe(v: &Value) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        other => other.type_name().to_string(),
    }
}

/// One passage of a job, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemSpec {
    /// Character offset of the passage in its source.
    pub(crate) offset: usize,
    /// Its length in characters.
    pub(crate) length: usize,
    /// The passage.
    pub(crate) text: String,
    /// What the client attached to the item it came from, as JSON.
    pub(crate) meta: Option<String>,
}

/// The passages of submit argument `items`, cut to `chunk_chars`: a list of
/// strings or `{text, offset?, meta?}` maps, or one document `{text}` (or a
/// string). An item without an `offset` follows the one before it.
///
/// # Errors
///
/// A message naming the item and what is wrong with it, or saying there is
/// no text at all.
pub(crate) fn split_items(items: &Value, chunk_chars: usize) -> Result<Vec<ItemSpec>, String> {
    let parts = match items {
        Value::String(text) => vec![Part {
            offset: None,
            text: text.clone(),
            meta: None,
        }],
        Value::Map(map) => vec![Part::from_map(map, "the document", |field| {
            format!("the document's {field}")
        })?],
        Value::List(list) => {
            let mut parts = Vec::with_capacity(list.len());
            for (i, item) in list.iter().enumerate() {
                parts.push(match item {
                    Value::String(text) => Part {
                        offset: None,
                        text: text.clone(),
                        meta: None,
                    },
                    Value::Map(map) => Part::from_map(map, &format!("items[{i}]"), |field| {
                        format!("items[{i}].{field}")
                    })?,
                    other => {
                        return Err(format!(
                            "items[{i}] must be a string or a map {{text, offset, meta}}, got {}",
                            other.type_name()
                        ))
                    }
                });
            }
            parts
        }
        other => {
            return Err(format!(
                "items must be a list of strings or maps {{text, offset, meta}}, or a \
                 document {{text}}, got {}",
                other.type_name()
            ))
        }
    };
    let mut out = Vec::new();
    let mut next = 0;
    for part in parts {
        let base = part.offset.unwrap_or(next);
        let length = part.text.chars().count();
        next = base + length;
        for chunk in crate::extract::chunk::split(&part.text, chunk_chars) {
            out.push(ItemSpec {
                offset: base + chunk.offset,
                length: chunk.length,
                text: chunk.text,
                meta: part.meta.clone(),
            });
        }
    }
    if out.is_empty() {
        return Err("there is no text to ingest".into());
    }
    Ok(out)
}

/// One submitted text before it is cut into passages.
struct Part {
    offset: Option<usize>,
    text: String,
    meta: Option<String>,
}

impl Part {
    /// Read a `{text, offset?, meta?}` map; `name` names it in errors and
    /// `field` names one of its fields.
    fn from_map(
        map: &BTreeMap<String, Value>,
        name: &str,
        field: impl Fn(&str) -> String,
    ) -> Result<Self, String> {
        if let Some(key) = map
            .keys()
            .find(|k| !matches!(k.as_str(), "text" | "offset" | "meta"))
        {
            return Err(format!(
                "{name} has an unknown key `{key}`; expected text, offset, meta"
            ));
        }
        let text = match map.get("text") {
            Some(Value::String(text)) => text.clone(),
            other => {
                return Err(format!(
                    "{} must be a string, got {}",
                    field("text"),
                    other.unwrap_or(&Value::Null).type_name()
                ))
            }
        };
        let offset = match map.get("offset") {
            None | Some(Value::Null) => None,
            Some(Value::Integer(n)) if *n >= 0 => usize::try_from(*n).ok(),
            Some(other) => {
                return Err(format!(
                    "{} must be a non-negative integer, got {}",
                    field("offset"),
                    describe(other)
                ))
            }
        };
        let meta = match map.get("meta") {
            None | Some(Value::Null) => None,
            Some(meta @ Value::Map(_)) => Some(
                crate::cypher::executor::value_to_json(meta)
                    .ok_or_else(|| {
                        format!(
                            "{} must hold plain values, not nodes or relationships",
                            field("meta")
                        )
                    })?
                    .to_string(),
            ),
            Some(other) => {
                return Err(format!(
                    "{} must be a map, got {}",
                    field("meta"),
                    other.type_name()
                ))
            }
        };
        Ok(Self { offset, text, meta })
    }
}

/// A map value from `pairs`.
#[cfg(test)]
fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::provenance::{SourceRef, SourceSpec};

    fn s(v: &str) -> Value {
        Value::String(v.into())
    }

    fn i(n: i64) -> Value {
        Value::Integer(n)
    }

    fn err(config: &Value) -> String {
        JobConfig::from_value(config, 5).unwrap_err()
    }

    #[test]
    fn a_config_needs_a_source_and_keeps_the_extract_keys() {
        let c = JobConfig::from_value(
            &map(&[
                ("source", s("book-1")),
                ("chunkChars", i(400)),
                ("types", Value::List(vec![s("person")])),
                ("priority", i(7)),
                ("maxAttempts", i(3)),
                ("concurrency", i(2)),
            ]),
            5,
        )
        .unwrap();
        assert_eq!(c.priority, 7);
        assert_eq!(c.max_attempts, 3);
        assert_eq!(c.concurrency, Some(2));
        assert_eq!(c.source_id, "book-1");
        assert_eq!(c.options.chunk_chars, 400);
        assert_eq!(c.options.types, ["PERSON"]);
        assert_eq!(
            c.extract,
            map(&[
                ("source", s("book-1")),
                ("chunkChars", i(400)),
                ("types", Value::List(vec![s("person")])),
            ])
        );
    }

    #[test]
    fn the_job_keys_have_defaults() {
        let source = map(&[("id", s("doc-1")), ("title", s("Notes"))]);
        let c = JobConfig::from_value(&map(&[("source", source)]), 4).unwrap();
        assert_eq!(c.priority, 0);
        assert_eq!(c.max_attempts, 4);
        assert_eq!(c.concurrency, None);
        assert_eq!(c.source_id, "doc-1");
        assert_eq!(
            c.options.source,
            Some(SourceRef::Spec(SourceSpec {
                id: "doc-1".into(),
                uri: None,
                title: Some("Notes".into()),
                kind: None,
            }))
        );
        // A null job key is its default.
        let c = JobConfig::from_value(&map(&[("source", s("b")), ("priority", Value::Null)]), 5)
            .unwrap();
        assert_eq!(c.priority, 0);
        assert_eq!(c.extract, map(&[("source", s("b"))]));
    }

    #[test]
    fn bad_configs_are_refused_with_the_reason() {
        let cases: Vec<(Value, String)> = vec![
            (
                Value::Null,
                "drevo.ingest.submit needs a config with a `source`: an id or a map \
                 {id, uri, title, kind}"
                    .into(),
            ),
            (
                map(&[("chunkChars", i(400))]),
                "drevo.ingest.submit needs a config with a `source`: an id or a map \
                 {id, uri, title, kind}"
                    .into(),
            ),
            (i(3), "the config must be a map, got Integer".into()),
            (
                map(&[("source", s("b")), ("dryRun", Value::Bool(true))]),
                "`dryRun` does not apply to ingest jobs".into(),
            ),
            (
                map(&[("source", s("b")), ("retries", i(3))]),
                format!("unknown config key `retries`; expected {CONFIG_KEYS}"),
            ),
            (
                map(&[("source", s("b")), ("priority", s("high"))]),
                "priority must be an integer, got String".into(),
            ),
            (
                map(&[("source", s("b")), ("maxAttempts", i(0))]),
                "maxAttempts must be an integer from 1 to 100, got 0".into(),
            ),
            (
                map(&[("source", s("b")), ("maxAttempts", i(101))]),
                "maxAttempts must be an integer from 1 to 100, got 101".into(),
            ),
            (
                map(&[("source", s("b")), ("maxAttempts", Value::Float(2.5))]),
                "maxAttempts must be an integer from 1 to 100, got Float".into(),
            ),
            (
                map(&[("source", s("b")), ("concurrency", i(0))]),
                "concurrency must be an integer of at least 1, got 0".into(),
            ),
            (
                map(&[("source", s("b")), ("concurrency", s("2"))]),
                "concurrency must be an integer of at least 1, got String".into(),
            ),
            (
                map(&[("source", s("b")), ("chunkChars", i(5))]),
                "chunkChars must be an integer from 20 to 100000, got 5".into(),
            ),
            (
                map(&[("source", i(1))]),
                "source must be an id or a map {id, uri, title, kind}, got Integer".into(),
            ),
        ];
        for (config, want) in cases {
            assert_eq!(err(&config), want, "{config:?}");
        }
    }

    fn texts(items: &[ItemSpec]) -> Vec<(usize, usize, &str)> {
        items
            .iter()
            .map(|i| (i.offset, i.length, i.text.as_str()))
            .collect()
    }

    #[test]
    fn a_document_is_cut_into_passages_with_offsets_into_it() {
        let text = "Dana joined Acme. Acme printed the book. Dana left in May. ".repeat(3);
        let items = split_items(&map(&[("text", s(&text))]), 40).unwrap();
        assert!(items.len() > 3, "{items:?}");
        for item in &items {
            let at: String = text.chars().skip(item.offset).take(item.length).collect();
            assert_eq!(at, item.text);
            assert!(item.length <= 40);
            assert_eq!(item.meta, None);
        }
        // A bare string is a document too.
        assert_eq!(split_items(&s(&text), 40).unwrap(), items);
    }

    #[test]
    fn list_items_follow_each_other_unless_they_say_where_they_are() {
        let items = split_items(
            &Value::List(vec![
                s("Глава 1."),
                s("Chapter 2"),
                map(&[
                    ("text", s("Epilogue")),
                    ("offset", i(500)),
                    ("meta", map(&[("page", i(9))])),
                ]),
                map(&[("text", s("After"))]),
            ]),
            100,
        )
        .unwrap();
        assert_eq!(
            texts(&items),
            [
                (0, 8, "Глава 1."),
                (8, 9, "Chapter 2"),
                (500, 8, "Epilogue"),
                (508, 5, "After"),
            ]
        );
        assert_eq!(items[2].meta.as_deref(), Some(r#"{"page":9}"#));
        assert_eq!(items[3].meta, None);
    }

    #[test]
    fn a_long_list_item_is_cut_too() {
        let long = "word ".repeat(30);
        let items = split_items(&Value::List(vec![s("intro"), s(&long)]), 40).unwrap();
        assert!(items.len() > 3);
        assert_eq!(items[0].offset, 0);
        assert_eq!(items[1].offset, 5);
        for item in &items[1..] {
            let at: String = long
                .chars()
                .skip(item.offset - 5)
                .take(item.length)
                .collect();
            assert_eq!(at, item.text);
        }
    }

    #[test]
    fn bad_items_are_refused_with_the_reason() {
        let cases: Vec<(Value, &str)> = vec![
            (
                i(4),
                "items must be a list of strings or maps {text, offset, meta}, or a document \
                 {text}, got Integer",
            ),
            (Value::List(vec![]), "there is no text to ingest"),
            (Value::List(vec![s("")]), "there is no text to ingest"),
            (map(&[("text", s(""))]), "there is no text to ingest"),
            (
                Value::List(vec![s("a"), i(1)]),
                "items[1] must be a string or a map {text, offset, meta}, got Integer",
            ),
            (
                Value::List(vec![map(&[("offset", i(1))])]),
                "items[0].text must be a string, got Null",
            ),
            (
                Value::List(vec![map(&[("text", s("a")), ("offset", i(-1))])]),
                "items[0].offset must be a non-negative integer, got -1",
            ),
            (
                Value::List(vec![map(&[("text", s("a")), ("offset", s("1"))])]),
                "items[0].offset must be a non-negative integer, got String",
            ),
            (
                Value::List(vec![map(&[("text", s("a")), ("meta", i(1))])]),
                "items[0].meta must be a map, got Integer",
            ),
            (
                Value::List(vec![map(&[("text", s("a")), ("page", i(1))])]),
                "items[0] has an unknown key `page`; expected text, offset, meta",
            ),
            (
                map(&[("text", i(3))]),
                "the document's text must be a string, got Integer",
            ),
        ];
        for (items, want) in cases {
            assert_eq!(split_items(&items, 100).unwrap_err(), want, "{items:?}");
        }
    }

    #[test]
    fn configure_clamps_zero_to_one() {
        let before = settings();
        configure(IngestSettings {
            workers: 0,
            max_attempts: 0,
            ..IngestSettings::default()
        });
        assert_eq!(settings().workers, 1);
        assert_eq!(settings().max_attempts, 1);
        configure(before);
    }

    #[test]
    fn a_wake_ends_a_wait() {
        let seen = generation();
        let waiter = std::thread::spawn(move || {
            wait(seen, Some(Duration::from_secs(10)));
            generation()
        });
        wake();
        let after = waiter.join().unwrap();
        assert_ne!(after, seen);
        // A wait on an old count returns at once.
        let start = std::time::Instant::now();
        wait(seen, None);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
