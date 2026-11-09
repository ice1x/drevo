//! Entity and fact extraction with a language model: the engine behind
//! `drevo.extract.entities`, `drevo.extract.facts`, `drevo.extract.ingest`
//! and `drevo.extract.chunks`.
//!
//! A text is cut into overlapping passages ([`chunk`](crate::extract::chunk)); each passage is sent
//! to an OpenAI-compatible chat endpoint with instructions to answer with one
//! JSON object listing the entities and facts it states, each quoting the
//! passage verbatim with a character offset ([`prompt`](crate::extract::prompt)). The answer is read
//! ([`parse`](crate::extract::parse)) and every quote is located in the passage ([`offsets`](crate::extract::offsets)), so a
//! quote the passage does not contain never reaches the graph. An answer
//! that is not JSON is asked for once more before the call fails.
//!
//! The chat endpoint is installed once per process ([`install`](crate::extract::install)); a server
//! installs it from `DREVO_EXTRACT_*` (falling back to
//! `DREVO_TEXT2CYPHER_*`). Nothing here touches the graph:
//!
//! ```
//! use drevo::extract::{extract_text, ExtractOptions, Want};
//! use drevo::llm::{ChatCompleter, ChatRequest, LlmError};
//!
//! struct Canned;
//! impl ChatCompleter for Canned {
//!     fn complete(&self, _: &ChatRequest) -> Result<String, LlmError> {
//!         Ok(r#"{"entities": [{"name": "Dana", "type": "PERSON",
//!                "surface": "Dana", "offset": 0}], "facts": []}"#.into())
//!     }
//! }
//!
//! let found = extract_text(&Canned, "Dana joined Acme.", &ExtractOptions::default(), Want::Entities)
//!     .unwrap();
//! assert_eq!(found.entities[0].name, "Dana");
//! ```

pub mod chunk;
pub mod offsets;
pub mod parse;
pub mod prompt;
#[cfg(feature = "http")]
pub mod settings;

use std::sync::Arc;

use std::collections::{BTreeMap, HashSet};

use crate::cypher::executor::Value;
use crate::cypher::provenance::{SourceRef, SourceSpec};
use crate::llm::{ChatCompleter, ChatMessage, ChatRequest, LlmError};

use self::chunk::Chunk;
use self::parse::{upper_snake, AcceptOptions, Accepted};
use self::prompt::{PromptOptions, JSON_NUDGE};

/// The entity types looked for when a call names none: the POLE+O set of
/// `drevo.memory.rememberEntity`.
pub const DEFAULT_TYPES: [&str; 5] = ["PERSON", "OBJECT", "LOCATION", "EVENT", "ORGANIZATION"];

/// The keys an extraction config map accepts, as listed in errors.
pub(crate) const CONFIG_KEYS: &str = "types, relations, schemaHint, language, model, chunkChars, \
     minConfidence, source, session, resolve, dryRun";

/// Why an extraction failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// No chat endpoint is installed.
    #[error(
        "no chat upstream is configured for extraction (set DREVO_EXTRACT_UPSTREAM or \
         DREVO_TEXT2CYPHER_UPSTREAM; see CALL drevo.extract.status())"
    )]
    NotConfigured,
    /// The endpoint setting is unusable.
    #[error("invalid extraction setting: {0}")]
    Invalid(String),
    /// The endpoint call failed.
    #[error("extraction upstream error: {0}")]
    Upstream(String),
    /// The model did not answer with the JSON object, even when asked again.
    #[error("the model did not return the extraction JSON after a retry: {0}")]
    Malformed(String),
}

/// How `drevo.extract.ingest` finds an entity that is already known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Resolve {
    /// The same name and type.
    #[default]
    Exact,
    /// Also a similar name of the same type, as `rememberEntity`
    /// resolves it with `resolve: 'fuzzy'`.
    Fuzzy,
}

/// An extraction config map, parsed (see [`ExtractOptions::from_value`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractOptions {
    /// Entity types to look for, upper-snake-case.
    pub types: Vec<String>,
    /// Allowed relations, upper-snake-case; `None` lets the model name them.
    pub relations: Option<Vec<String>>,
    /// Domain vocabulary for the instructions.
    pub schema_hint: Option<String>,
    /// The text's language, or `auto`.
    pub language: String,
    /// The model to ask instead of the endpoint's.
    pub model: Option<String>,
    /// Passage size in characters.
    pub chunk_chars: usize,
    /// Items below this confidence are dropped.
    pub min_confidence: f64,
    /// `ingest`: the source the text is.
    pub source: Option<SourceRef>,
    /// `ingest`: the conversation whose newest message mentions the entities.
    pub session: Option<String>,
    /// `ingest`: how known entities are found.
    pub resolve: Resolve,
    /// `ingest`: count without writing.
    pub dry_run: bool,
}

/// The keys only `drevo.extract.ingest` takes.
const INGEST_ONLY: [&str; 4] = ["source", "session", "resolve", "dryRun"];

/// How a config value is shown in an error: a number or string as itself,
/// anything else by its type.
fn describe(v: &Value) -> String {
    match v {
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => format!("'{s}'"),
        other => other.type_name().to_string(),
    }
}

/// How a value that should be a number is shown: the number, else its type.
fn number(v: &Value) -> String {
    match v {
        Value::Integer(_) | Value::Float(_) => describe(v),
        other => other.type_name().to_string(),
    }
}

/// A string config value.
fn string(key: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(format!("{key} must be a string, got {}", other.type_name())),
    }
}

/// A list of strings, or `None` when `v` is not one.
fn strings(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// The `types` list: non-empty, no blank entry, upper-snake-cased.
fn type_list(v: &Value) -> Result<Vec<String>, String> {
    let refuse = |got: &str| format!("types must be a non-empty list of strings, got {got}");
    let list = strings(v).ok_or_else(|| refuse(v.type_name()))?;
    if list.is_empty() {
        return Err(refuse("an empty list"));
    }
    let mut types: Vec<String> = Vec::with_capacity(list.len());
    for t in list {
        let t = upper_snake(&t);
        if t.is_empty() {
            return Err(refuse("a blank string"));
        }
        if !types.contains(&t) {
            types.push(t);
        }
    }
    Ok(types)
}

/// The passage size when a call names none.
pub(crate) const DEFAULT_CHUNK_CHARS: usize = 1500;
/// The smallest passage size a call may ask for.
pub(crate) const MIN_CHUNK_CHARS: usize = 20;
/// The largest passage size a call may ask for.
pub(crate) const MAX_CHUNK_CHARS: usize = 100_000;

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            types: DEFAULT_TYPES.iter().map(|t| (*t).to_string()).collect(),
            relations: None,
            schema_hint: None,
            language: "auto".into(),
            model: None,
            chunk_chars: DEFAULT_CHUNK_CHARS,
            min_confidence: 0.5,
            source: None,
            session: None,
            resolve: Resolve::Exact,
            dry_run: false,
        }
    }
}

impl ExtractOptions {
    /// Parse a config map; `null` gives the defaults. `ingest` allows the
    /// keys only `drevo.extract.ingest` takes (`source`, `session`,
    /// `resolve`, `dryRun`) and requires `source`.
    ///
    /// # Errors
    ///
    /// A message naming the key when it is unknown (listing the accepted
    /// keys), only for `ingest`, of the wrong type or out of range, or when
    /// `ingest` has no `source`.
    pub fn from_value(value: &Value, ingest: bool) -> Result<Self, String> {
        let mut o = Self::default();
        let map = match value {
            Value::Null => BTreeMap::new(),
            Value::Map(map) => map.clone(),
            other => {
                return Err(format!(
                    "the config must be a map, got {}",
                    other.type_name()
                ))
            }
        };
        for (key, v) in &map {
            if matches!(v, Value::Null) {
                continue;
            }
            let key = key.as_str();
            if !ingest && INGEST_ONLY.contains(&key) {
                return Err(format!("`{key}` applies to drevo.extract.ingest only"));
            }
            match key {
                "types" => o.types = type_list(v)?,
                "relations" => {
                    o.relations = Some(
                        strings(v)
                            .ok_or_else(|| {
                                format!(
                                    "relations must be a list of strings or null, got {}",
                                    v.type_name()
                                )
                            })?
                            .iter()
                            .map(|r| upper_snake(r))
                            .filter(|r| !r.is_empty())
                            .collect(),
                    );
                }
                "schemaHint" => o.schema_hint = Some(string("schemaHint", v)?),
                "language" => o.language = string("language", v)?,
                "model" => {
                    let model = string("model", v)?;
                    if model.trim().is_empty() {
                        return Err("model must be a non-blank string, got a blank string".into());
                    }
                    o.model = Some(model.trim().to_string());
                }
                "chunkChars" => {
                    o.chunk_chars = match v {
                        Value::Integer(n) => usize::try_from(*n)
                            .ok()
                            .filter(|n| (MIN_CHUNK_CHARS..=MAX_CHUNK_CHARS).contains(n)),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "chunkChars must be an integer from {MIN_CHUNK_CHARS} to \
                             {MAX_CHUNK_CHARS}, got {}",
                            number(v)
                        )
                    })?;
                }
                "minConfidence" => {
                    o.min_confidence = match v {
                        Value::Integer(n) => Some(*n as f64),
                        Value::Float(f) => Some(*f),
                        _ => None,
                    }
                    .filter(|c| (0.0..=1.0).contains(c))
                    .ok_or_else(|| {
                        format!(
                            "minConfidence must be a number from 0 to 1, got {}",
                            number(v)
                        )
                    })?;
                }
                "source" => {
                    o.source = Some(match v {
                        Value::String(id) => SourceRef::Id(id.clone()),
                        Value::Map(_) => SourceRef::Spec(SourceSpec::from_value(v)?),
                        other => {
                            return Err(format!(
                                "source must be an id or a map {{id, uri, title, kind}}, got {}",
                                other.type_name()
                            ))
                        }
                    });
                }
                "session" => match v {
                    Value::String(s) => o.session = Some(s.clone()),
                    other => {
                        return Err(format!(
                            "session must be a string or null, got {}",
                            other.type_name()
                        ))
                    }
                },
                "resolve" => {
                    o.resolve = match v {
                        Value::String(s) if s.eq_ignore_ascii_case("exact") => Resolve::Exact,
                        Value::String(s) if s.eq_ignore_ascii_case("fuzzy") => Resolve::Fuzzy,
                        other => {
                            return Err(format!(
                                "resolve must be 'exact' or 'fuzzy', got {}",
                                describe(other)
                            ))
                        }
                    };
                }
                "dryRun" => match v {
                    Value::Bool(b) => o.dry_run = *b,
                    other => {
                        return Err(format!(
                            "dryRun must be a boolean, got {}",
                            other.type_name()
                        ))
                    }
                },
                other => {
                    return Err(format!(
                        "unknown config key `{other}`; expected {CONFIG_KEYS}"
                    ))
                }
            }
        }
        if ingest && o.source.is_none() {
            return Err(
                "drevo.extract.ingest needs a `source`: an id or a map {id, uri, title, kind}"
                    .into(),
            );
        }
        Ok(o)
    }
}

/// Which lists an extraction asks the model to fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Entity mentions only.
    Entities,
    /// Facts only.
    Facts,
    /// Both.
    Both,
}

/// One passage and what was kept from the model's answer for it, with
/// offsets into the passage.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkExtraction {
    /// The passage.
    pub chunk: Chunk,
    /// The entities and facts located in it.
    pub accepted: Accepted,
}

/// Extract from every passage of `text` (cut at `options.chunk_chars`), one
/// call per passage. A passage that is only whitespace is not sent.
///
/// # Errors
///
/// [`ExtractError::Upstream`] when a call fails, [`ExtractError::Malformed`]
/// when an answer is not the JSON object twice in a row.
pub fn extract_chunks(
    completer: &dyn ChatCompleter,
    text: &str,
    options: &ExtractOptions,
    want: Want,
) -> Result<Vec<ChunkExtraction>, ExtractError> {
    let prompt = prompt::system_prompt(&PromptOptions {
        types: &options.types,
        relations: options.relations.as_deref(),
        schema_hint: options.schema_hint.as_deref(),
        language: &options.language,
        entities: want != Want::Facts,
        facts: want != Want::Entities,
    });
    let accept_options = AcceptOptions {
        types: &options.types,
        relations: options.relations.as_deref(),
        min_confidence: options.min_confidence,
    };
    let mut out = Vec::new();
    for chunk in chunk::split(text, options.chunk_chars) {
        let accepted = if chunk.text.trim().is_empty() {
            Accepted::default()
        } else {
            let raw = ask(completer, &prompt, &chunk.text, options.model.as_deref())?;
            let mut accepted = parse::accept(&raw, &chunk.text, &accept_options);
            if want == Want::Facts {
                accepted.entities.clear();
            }
            if want == Want::Entities {
                accepted.facts.clear();
            }
            accepted
        };
        out.push(ChunkExtraction { chunk, accepted });
    }
    Ok(out)
}

/// One extraction call for `passage`, asked once more with [`JSON_NUDGE`]
/// when the answer is not the JSON object.
fn ask(
    completer: &dyn ChatCompleter,
    system: &str,
    passage: &str,
    model: Option<&str>,
) -> Result<parse::RawResponse, ExtractError> {
    let upstream = |e: LlmError| ExtractError::Upstream(e.to_string());
    let mut request = ChatRequest {
        model: model.map(str::to_string),
        messages: vec![
            ChatMessage::system(system),
            ChatMessage::user(prompt::user_message(passage)),
        ],
    };
    let first = completer.complete(&request).map_err(upstream)?;
    if let Ok(raw) = parse::parse_response(&first) {
        return Ok(raw);
    }
    request.messages.push(ChatMessage::assistant(first));
    request.messages.push(ChatMessage::user(JSON_NUDGE));
    let second = completer.complete(&request).map_err(upstream)?;
    parse::parse_response(&second).map_err(ExtractError::Malformed)
}

/// Extract from `text` with offsets into the whole text: what
/// [`extract_chunks`] finds, with an item read twice where passages overlap
/// kept once.
///
/// # Errors
///
/// As [`extract_chunks`].
pub fn extract_text(
    completer: &dyn ChatCompleter,
    text: &str,
    options: &ExtractOptions,
    want: Want,
) -> Result<Accepted, ExtractError> {
    let mut out = Accepted::default();
    let mut seen_entities = HashSet::new();
    let mut seen_facts = HashSet::new();
    for found in extract_chunks(completer, text, options, want)? {
        let base = found.chunk.offset;
        out.skipped += found.accepted.skipped;
        for mut e in found.accepted.entities {
            e.offset += base;
            if seen_entities.insert((e.offset, e.name.clone(), e.entity_type.clone())) {
                out.entities.push(e);
            }
        }
        for mut f in found.accepted.facts {
            f.offset += base;
            let key = (
                f.offset,
                f.subject.clone(),
                f.relation.clone(),
                f.object.clone(),
            );
            if seen_facts.insert(key) {
                out.facts.push(f);
            }
        }
    }
    Ok(out)
}

/// The process-wide chat endpoint extraction uses.
static GLOBAL: std::sync::RwLock<Option<Arc<dyn ChatCompleter>>> = std::sync::RwLock::new(None);

/// What the last extraction call reported, for `drevo.extract.status()`.
static LAST: std::sync::Mutex<LastRun> = std::sync::Mutex::new(LastRun {
    error: None,
    skipped: None,
});

#[derive(Debug)]
struct LastRun {
    error: Option<String>,
    skipped: Option<u64>,
}

/// Install the chat endpoint extraction uses, replacing any other.
pub fn install(completer: Arc<dyn ChatCompleter>) {
    *GLOBAL.write().unwrap_or_else(|e| e.into_inner()) = Some(completer);
}

/// Remove the installed chat endpoint: extraction reports "not configured".
pub fn uninstall() {
    *GLOBAL.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The installed chat endpoint, if any.
#[must_use]
pub fn installed() -> Option<Arc<dyn ChatCompleter>> {
    GLOBAL.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Record the outcome of an extraction call for [`status`]: its error, or
/// how many items it dropped.
pub fn record(outcome: Result<usize, &ExtractError>) {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match outcome {
        Ok(skipped) => {
            last.error = None;
            last.skipped = Some(u64::try_from(skipped).unwrap_or(u64::MAX));
        }
        Err(e) => last.error = Some(e.to_string()),
    }
}

/// The state `drevo.extract.status()` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractStatus {
    /// Whether a chat endpoint is installed.
    pub configured: bool,
    /// Its default model.
    pub model: Option<String>,
    /// Its URL (never a key).
    pub upstream: Option<String>,
    /// The error of the last call, `None` when it succeeded.
    pub last_error: Option<String>,
    /// How many items the last successful call dropped.
    pub last_skipped: Option<u64>,
}

/// The installed endpoint and the last call's outcome.
#[must_use]
pub fn status() -> ExtractStatus {
    let completer = installed();
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    ExtractStatus {
        configured: completer.is_some(),
        model: completer.as_ref().and_then(|c| c.model()),
        upstream: completer.as_ref().and_then(|c| c.upstream()),
        last_error: last.error.clone(),
        last_skipped: last.skipped,
    }
}

/// Serialises the unit tests that install a process-wide endpoint.
#[cfg(test)]
pub(crate) static TEST_GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use crate::cypher::provenance::SourceSpec;
    use crate::llm::{ChatRequest, LlmError};

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn s(v: &str) -> Value {
        Value::String(v.into())
    }

    fn list(xs: &[&str]) -> Value {
        Value::List(xs.iter().map(|x| s(x)).collect())
    }

    fn err(value: &Value, ingest: bool) -> String {
        ExtractOptions::from_value(value, ingest).unwrap_err()
    }

    // ── options ──────────────────────────────────────────────────────────

    #[test]
    fn null_config_gives_the_defaults() {
        let o = ExtractOptions::from_value(&Value::Null, false).unwrap();
        assert_eq!(o, ExtractOptions::default());
        assert_eq!(o.types, DEFAULT_TYPES);
        assert_eq!(o.chunk_chars, 1500);
        assert!((o.min_confidence - 0.5).abs() < 1e-9);
        assert_eq!(o.language, "auto");
    }

    #[test]
    fn every_key_is_read() {
        let o = ExtractOptions::from_value(
            &map(&[
                ("types", list(&["person", "space ship"])),
                ("relations", list(&["works at"])),
                ("schemaHint", s("A novel.")),
                ("language", s("ru")),
                ("model", s("llama3.1")),
                ("chunkChars", Value::Integer(400)),
                ("minConfidence", Value::Float(0.7)),
                ("source", s("book-1")),
                ("session", s("chat-1")),
                ("resolve", s("fuzzy")),
                ("dryRun", Value::Bool(true)),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(o.types, ["PERSON", "SPACE_SHIP"]);
        assert_eq!(o.relations, Some(vec!["WORKS_AT".to_string()]));
        assert_eq!(o.schema_hint.as_deref(), Some("A novel."));
        assert_eq!(o.language, "ru");
        assert_eq!(o.model.as_deref(), Some("llama3.1"));
        assert_eq!(o.chunk_chars, 400);
        assert!((o.min_confidence - 0.7).abs() < 1e-9);
        assert_eq!(o.source, Some(SourceRef::Id("book-1".into())));
        assert_eq!(o.session.as_deref(), Some("chat-1"));
        assert_eq!(o.resolve, Resolve::Fuzzy);
        assert!(o.dry_run);
    }

    #[test]
    fn a_source_map_is_a_spec() {
        let o = ExtractOptions::from_value(
            &map(&[(
                "source",
                map(&[
                    ("id", s("doc-1")),
                    ("title", s("Notes")),
                    ("kind", s("document")),
                ]),
            )]),
            true,
        )
        .unwrap();
        assert_eq!(
            o.source,
            Some(SourceRef::Spec(SourceSpec {
                id: "doc-1".into(),
                uri: None,
                title: Some("Notes".into()),
                kind: Some("document".into()),
            }))
        );
    }

    #[test]
    fn a_null_value_is_the_default() {
        let o = ExtractOptions::from_value(&map(&[("model", Value::Null)]), false).unwrap();
        assert_eq!(o.model, None);
    }

    #[test]
    fn an_unknown_key_is_named_with_the_accepted_ones() {
        let e = err(&map(&[("chunkSize", Value::Integer(5))]), false);
        assert_eq!(
            e,
            format!("unknown config key `chunkSize`; expected {CONFIG_KEYS}")
        );
    }

    #[test]
    fn ingest_only_keys_are_refused_elsewhere() {
        for key in ["source", "session", "resolve", "dryRun"] {
            let e = err(&map(&[(key, s("x"))]), false);
            assert_eq!(
                e,
                format!("`{key}` applies to drevo.extract.ingest only"),
                "{key}"
            );
        }
    }

    #[test]
    fn ingest_needs_a_source() {
        assert_eq!(
            err(&Value::Null, true),
            "drevo.extract.ingest needs a `source`: an id or a map {id, uri, title, kind}"
        );
    }

    #[test]
    fn a_config_that_is_not_a_map_is_refused() {
        assert_eq!(
            err(&Value::Integer(3), false),
            "the config must be a map, got Integer"
        );
    }

    #[test]
    fn wrong_types_and_ranges_are_refused() {
        let cases: Vec<(Value, &str)> = vec![
            (
                map(&[("types", s("PERSON"))]),
                "types must be a non-empty list of strings, got String",
            ),
            (
                map(&[("types", Value::List(vec![]))]),
                "types must be a non-empty list of strings, got an empty list",
            ),
            (
                map(&[("types", list(&["  "]))]),
                "types must be a non-empty list of strings, got a blank string",
            ),
            (
                map(&[("relations", Value::Integer(1))]),
                "relations must be a list of strings or null, got Integer",
            ),
            (
                map(&[("schemaHint", Value::Integer(1))]),
                "schemaHint must be a string, got Integer",
            ),
            (
                map(&[("language", Value::Bool(true))]),
                "language must be a string, got Boolean",
            ),
            (
                map(&[("model", s(" "))]),
                "model must be a non-blank string, got a blank string",
            ),
            (
                map(&[("chunkChars", Value::Integer(5))]),
                "chunkChars must be an integer from 20 to 100000, got 5",
            ),
            (
                map(&[("chunkChars", s("big"))]),
                "chunkChars must be an integer from 20 to 100000, got String",
            ),
            (
                map(&[("minConfidence", Value::Float(1.5))]),
                "minConfidence must be a number from 0 to 1, got 1.5",
            ),
            (
                map(&[("minConfidence", s("high"))]),
                "minConfidence must be a number from 0 to 1, got String",
            ),
        ];
        for (value, want) in cases {
            assert_eq!(err(&value, false), want);
        }
        let ingest_cases: Vec<(Value, &str)> = vec![
            (
                map(&[("source", Value::Integer(1))]),
                "source must be an id or a map {id, uri, title, kind}, got Integer",
            ),
            (
                map(&[("source", map(&[("title", s("x"))]))]),
                "a source map needs an id",
            ),
            (
                map(&[("source", s("b")), ("session", Value::Integer(1))]),
                "session must be a string or null, got Integer",
            ),
            (
                map(&[("source", s("b")), ("resolve", s("nearest"))]),
                "resolve must be 'exact' or 'fuzzy', got 'nearest'",
            ),
            (
                map(&[("source", s("b")), ("dryRun", s("yes"))]),
                "dryRun must be a boolean, got String",
            ),
        ];
        for (value, want) in ingest_cases {
            assert_eq!(err(&value, true), want);
        }
    }

    // ── the pipeline, with fake endpoints ────────────────────────────────

    /// Answers each call with the next canned reply and records the requests.
    struct Script {
        replies: Mutex<Vec<Result<String, LlmError>>>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl Script {
        fn new(replies: Vec<Result<&str, LlmError>>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .rev()
                        .map(|r| r.map(str::to_string))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<ChatRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ChatCompleter for Script {
        fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
            self.seen.lock().unwrap().push(request.clone());
            self.replies
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Err(LlmError::Upstream("no more replies".into())))
        }
    }

    const DANA: &str = r#"{"entities": [{"name": "Dana", "type": "PERSON", "surface": "Dana", "offset": 0}],
  "facts": [{"subject": "Dana", "subjectType": "PERSON", "relation": "joined", "object": "Acme",
             "objectType": "ORGANIZATION", "quote": "Dana joined Acme", "offset": 0}]}"#;

    #[test]
    fn one_passage_is_one_call_with_the_prompt_and_the_text() {
        let script = Script::new(vec![Ok(DANA)]);
        let options = ExtractOptions {
            model: Some("llama3.1".into()),
            ..ExtractOptions::default()
        };
        let found = extract_chunks(&script, "Dana joined Acme.", &options, Want::Both).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].accepted.entities.len(), 1);
        assert_eq!(found[0].accepted.facts[0].relation, "JOINED");
        let calls = script.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model.as_deref(), Some("llama3.1"));
        assert_eq!(calls[0].messages[0].role, "system");
        assert!(calls[0].messages[0].content.contains("PERSON, OBJECT"));
        assert!(calls[0].messages[1].content.contains("Dana joined Acme."));
    }

    #[test]
    fn a_malformed_answer_is_asked_for_once_more() {
        let script = Script::new(vec![Ok("Sorry, here you go: Dana."), Ok(DANA)]);
        let found = extract_chunks(
            &script,
            "Dana joined Acme.",
            &ExtractOptions::default(),
            Want::Both,
        )
        .unwrap();
        assert_eq!(found[0].accepted.entities.len(), 1);
        let calls = script.calls();
        assert_eq!(calls.len(), 2);
        let retry = &calls[1].messages;
        assert_eq!(retry[2].role, "assistant");
        assert_eq!(retry[2].content, "Sorry, here you go: Dana.");
        assert_eq!(retry[3].content, prompt::JSON_NUDGE);
    }

    #[test]
    fn a_second_malformed_answer_fails() {
        let script = Script::new(vec![Ok("nope"), Ok("still nope")]);
        let e = extract_chunks(
            &script,
            "Dana joined Acme.",
            &ExtractOptions::default(),
            Want::Both,
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "the model did not return the extraction JSON after a retry: \
         no JSON object in the answer ('still nope')"
        );
        assert_eq!(script.calls().len(), 2);
    }

    #[test]
    fn an_upstream_failure_is_not_retried() {
        let script = Script::new(vec![Err(LlmError::Upstream(
            "upstream returned 500".into(),
        ))]);
        let e = extract_chunks(
            &script,
            "Dana joined Acme.",
            &ExtractOptions::default(),
            Want::Both,
        )
        .unwrap_err();
        assert_eq!(e, ExtractError::Upstream("upstream returned 500".into()));
        assert_eq!(
            e.to_string(),
            "extraction upstream error: upstream returned 500"
        );
        assert_eq!(script.calls().len(), 1);
    }

    #[test]
    fn a_blank_text_makes_no_call() {
        let script = Script::new(vec![]);
        let found =
            extract_chunks(&script, "  \n ", &ExtractOptions::default(), Want::Both).unwrap();
        assert!(found.iter().all(|c| c.accepted == Accepted::default()));
        assert!(script.calls().is_empty());
    }

    /// Answers with the mention of every "Dana" in the passage it is sent.
    struct EveryDana;
    impl ChatCompleter for EveryDana {
        fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
            let text = request
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let passage = text
                .split_once("Text:\n")
                .map(|(_, p)| p)
                .unwrap_or_default()
                .to_string();
            let mut items = Vec::new();
            let mut from = 0;
            while let Some(i) = passage[from..].find("Dana") {
                let byte = from + i;
                let offset = passage[..byte].chars().count();
                items.push(serde_json::json!({
                    "name": "Dana", "type": "PERSON", "surface": "Dana", "offset": offset
                }));
                from = byte + 4;
            }
            Ok(serde_json::json!({ "entities": items, "facts": [] }).to_string())
        }
    }

    #[test]
    fn whole_text_offsets_span_passages_and_overlaps_are_kept_once() {
        let text = "Дана и Dana. ".repeat(12);
        let options = ExtractOptions {
            chunk_chars: 40,
            ..ExtractOptions::default()
        };
        let found = extract_text(&EveryDana, &text, &options, Want::Entities).unwrap();
        assert_eq!(found.entities.len(), 12, "{:?}", found.entities);
        for e in &found.entities {
            let at: String = text.chars().skip(e.offset).take(e.length).collect();
            assert_eq!(at, "Dana");
        }
        let mut offsets: Vec<usize> = found.entities.iter().map(|e| e.offset).collect();
        offsets.dedup();
        assert_eq!(offsets.len(), 12);
    }

    #[test]
    fn want_picks_the_lists() {
        let script = Script::new(vec![Ok(DANA)]);
        let found = extract_text(
            &script,
            "Dana joined Acme.",
            &ExtractOptions::default(),
            Want::Facts,
        )
        .unwrap();
        assert!(found.entities.is_empty());
        assert_eq!(found.facts.len(), 1);
        assert!(script.calls()[0].messages[0]
            .content
            .contains("Leave \"entities\" empty"));
    }

    // ── the process-wide endpoint and its status ─────────────────────────

    struct Named;
    impl ChatCompleter for Named {
        fn complete(&self, _: &ChatRequest) -> Result<String, LlmError> {
            Ok("{}".into())
        }
        fn model(&self) -> Option<String> {
            Some("m".into())
        }
        fn upstream(&self) -> Option<String> {
            Some("http://llm.test/v1/chat/completions".into())
        }
    }

    #[test]
    fn install_status_and_uninstall() {
        let _serial = TEST_GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        uninstall();
        assert!(installed().is_none());
        assert!(!status().configured);
        install(Arc::new(Named));
        assert!(installed().is_some());
        let st = status();
        assert!(st.configured);
        assert_eq!(st.model.as_deref(), Some("m"));
        assert_eq!(
            st.upstream.as_deref(),
            Some("http://llm.test/v1/chat/completions")
        );
        record(Err(&ExtractError::Upstream("boom".into())));
        assert_eq!(
            status().last_error.as_deref(),
            Some("extraction upstream error: boom")
        );
        record(Ok(3));
        assert_eq!(status().last_error, None);
        assert_eq!(status().last_skipped, Some(3));
        uninstall();
        assert!(installed().is_none());
    }

    #[test]
    fn not_configured_names_the_status_procedure() {
        assert!(ExtractError::NotConfigured
            .to_string()
            .contains("CALL drevo.extract.status()"));
    }
}
