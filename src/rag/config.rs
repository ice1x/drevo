//! The config map of `drevo.rag.retrieve` and `drevo.rag.context`.
//!
//! One map configures every stage: the seed search (`label`,
//! `embeddingProperty`, `k`, `candidates`, `rrfK`, `mode`), the expansion
//! (`hops`, `direction`, `relTypes`, `labels`, `excludeLabels`,
//! `perNodeLimit`, `maxNodes`, `asOf`), the ranking (`rank`), the provenance
//! filter (`source`) and the serialiser (`tokenBudget`, `tokenizer`,
//! `format`, `textProperties`, `maxBodyChars`, `includeProvenance`), the
//! scope (`scope`, `globalThreshold`, `globalHint`, `communityLayer`:
//! nearby nodes or community summaries), plus `session` and `trace`, which
//! make `drevo.rag.context` record a `:ContextTrace`. An unknown key, or a
//! value of the wrong type or range, is refused by name:
//!
//! ```
//! use std::collections::BTreeMap;
//! use drevo::cypher::executor::Value;
//! use drevo::cypher::expand::ExpandCeilings;
//! use drevo::rag::config::{RagConfig, RankMode};
//! use drevo::rag::serialize::Format;
//!
//! let map = Value::Map(BTreeMap::from([
//!     ("format".to_string(), Value::String("triples".to_string())),
//!     ("rank".to_string(), Value::String("distance".to_string())),
//! ]));
//! let config = RagConfig::from_value(&map, ExpandCeilings::default()).unwrap();
//! assert_eq!(config.render.format, Format::Triples);
//! assert_eq!(config.rank, RankMode::Distance);
//! assert_eq!(config.expand.hops, 2);
//!
//! let small = Value::Map(BTreeMap::from([("tokenBudget".to_string(), Value::Integer(10))]));
//! let refused = RagConfig::from_value(&small, ExpandCeilings::default()).unwrap_err();
//! assert!(refused.contains("tokenBudget"));
//! ```

use std::collections::BTreeMap;

use crate::cypher::executor::Value;
use crate::cypher::expand::{expand_options, ExpandCeilings};
use crate::cypher::hybrid::{parse_k, HybridConfig};
use crate::rag::serialize::{Format, RenderOptions};
use crate::rag::tokens::Tokenizer;
use crate::traversal::ExpandOptions;

/// The keys a `drevo.rag.retrieve` / `drevo.rag.context` config map
/// accepts, as listed in errors.
pub const CONFIG_KEYS: &str = "label, embeddingProperty, k, candidates, rrfK, mode, hops, \
     direction, relTypes, labels, excludeLabels, perNodeLimit, maxNodes, asOf, rank, source, \
     tokenBudget, tokenizer, format, textProperties, maxBodyChars, includeProvenance, scope, \
     globalThreshold, globalHint, communityLayer, session, trace";

/// The labels left out of the expansion unless `excludeLabels` says
/// otherwise: conversation and bookkeeping nodes, not knowledge — and
/// community summaries, which the global scope reads on its own.
pub(crate) const DEFAULT_EXCLUDE_LABELS: [&str; 6] = [
    "Message",
    "ReasoningTrace",
    "ContextTrace",
    "Community",
    "IngestJob",
    "IngestItem",
];

/// The smallest `tokenBudget` accepted.
pub(crate) const MIN_TOKEN_BUDGET: usize = 50;

/// The number of seeds searched for when the config names no `k`.
pub(crate) const DEFAULT_K: usize = 8;

/// How deep the expansion goes when the config names no `hops`.
pub(crate) const DEFAULT_HOPS: usize = 2;

/// How the expanded nodes are ordered after the seeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankMode {
    /// Personalized PageRank over the expanded nodes, teleporting to the
    /// seeds weighted by their search score; best score first.
    Ppr,
    /// Nearest first, then by best seed score, then by node id.
    Distance,
    /// The order the expansion reached them in.
    None,
}

impl RankMode {
    /// The mode's name as written in a config map.
    ///
    /// ```
    /// use drevo::rag::config::RankMode;
    /// assert_eq!(RankMode::Ppr.as_str(), "ppr");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ppr => "ppr",
            Self::Distance => "distance",
            Self::None => "none",
        }
    }
}

/// The parsed config map of `drevo.rag.retrieve` / `drevo.rag.context`.
///
/// ```
/// use drevo::rag::config::{RagConfig, RankMode};
///
/// let config = RagConfig::default();
/// assert_eq!((config.k, config.rank), (8, RankMode::Ppr));
/// assert_eq!(config.render.token_budget, 2000);
/// assert!(config.expand.exclude_labels.contains(&"Message".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RagConfig {
    /// The label whose embeddings the vector leg ranks; `None` runs BM25
    /// only.
    pub label: Option<String>,
    /// The property holding those embeddings.
    pub embedding_property: String,
    /// How many seeds the search returns.
    pub k: usize,
    /// The search's `candidates`, `rrfK` and `mode`.
    pub hybrid: HybridConfig,
    /// The expansion from the seeds.
    pub expand: ExpandOptions,
    /// How the expanded nodes are ordered after the seeds.
    pub rank: RankMode,
    /// Keep only nodes and relationships whose `source_id` is one of these;
    /// those without a `source_id` are always kept. `None` keeps everything.
    pub sources: Option<Vec<String>>,
    /// How the context is written.
    pub render: RenderOptions,
    /// The session the context is for, if any. `drevo.rag.context` records
    /// a `:ContextTrace` for a session.
    pub session: Option<String>,
    /// Whether `drevo.rag.context` records a `:ContextTrace` without a
    /// session.
    pub trace: bool,
    /// Whether the context is built from the nearest nodes, from community
    /// summaries, or chosen per question.
    pub scope: Scope,
    /// Under `scope: 'auto'`, when both search legs ran, a best seed score
    /// below this picks the global scope. A single BM25 leg's RRF score says
    /// nothing about confidence, so it is then ignored.
    pub global_threshold: f64,
    /// Under `scope: 'auto'`, a question whose start matches this regular
    /// expression (case-insensitive) picks the global scope.
    pub global_hint: String,
    /// The layer of `:Community` nodes the global scope reads.
    pub community_layer: String,
}

/// Where a context comes from.
///
/// ```
/// use drevo::rag::config::{RagConfig, Scope};
///
/// assert_eq!(RagConfig::default().scope, Scope::Local);
/// assert_eq!(Scope::Global.as_str(), "global");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The seeds of a hybrid search and the nodes near them.
    Local,
    /// The best-matching community summaries and their best-ranked members.
    Global,
    /// Global for a question that finds no seed, matches the global hint,
    /// or — when both search legs ran — whose best seed scores below the
    /// global threshold; else local.
    Auto,
}

impl Scope {
    /// The scope's name as written in a config map and reported per row.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
            Self::Auto => "auto",
        }
    }
}

/// The global threshold when the config names none: an RRF score.
pub(crate) const DEFAULT_GLOBAL_THRESHOLD: f64 = 0.02;

/// The global hint when the config names none.
pub(crate) const DEFAULT_GLOBAL_HINT: &str = "^(what|что|о чём|overview|summary)";

/// Whether the start of `query` matches the regular expression `hint`,
/// ignoring case and line breaks.
pub(crate) fn global_hint_matches(hint: &str, query: &str) -> Result<bool, String> {
    use crate::cypher::regex::Regex;
    let pattern = format!("(?i)(?:{hint}).*");
    let regex = Regex::compile(&pattern)
        .map_err(|e| format!("globalHint is not a valid regular expression: {e}"))?;
    let question = query.to_lowercase().replace(['\n', '\r'], " ");
    regex
        .is_match(&question)
        .map_err(|e| format!("globalHint is not a valid regular expression: {e}"))
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            label: None,
            embedding_property: "embedding".to_string(),
            k: DEFAULT_K,
            hybrid: HybridConfig::default(),
            expand: ExpandOptions {
                hops: DEFAULT_HOPS,
                exclude_labels: DEFAULT_EXCLUDE_LABELS.map(str::to_string).to_vec(),
                ..ExpandOptions::default()
            },
            rank: RankMode::Ppr,
            sources: None,
            render: RenderOptions::default(),
            session: None,
            trace: false,
            scope: Scope::Local,
            global_threshold: DEFAULT_GLOBAL_THRESHOLD,
            global_hint: DEFAULT_GLOBAL_HINT.to_string(),
            community_layer: crate::community::DEFAULT_LAYER.to_string(),
        }
    }
}

impl RagConfig {
    /// Parse the optional config argument: `null` gives the defaults, a map
    /// sets the keys it names (a `null` value keeps that key's default).
    /// `hops` and `maxNodes` are held to `ceilings`, as in
    /// `drevo.graph.expand`.
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing [`CONFIG_KEYS`]) or a value has the wrong type or
    /// range.
    pub fn from_value(value: &Value, ceilings: ExpandCeilings) -> Result<Self, String> {
        let none = BTreeMap::new();
        let map = match value {
            Value::Null => &none,
            Value::Map(map) => map,
            other => {
                return Err(format!(
                    "the config must be a map, got {}",
                    other.type_name()
                ))
            }
        };
        let mut config = Self::default();
        // The search's and the expansion's keys are parsed by their own
        // procedures' parsers, so they mean exactly what they mean there.
        let mut search = BTreeMap::new();
        let mut walk = BTreeMap::from([
            ("hops".to_string(), Value::Integer(DEFAULT_HOPS as i64)),
            (
                "excludeLabels".to_string(),
                Value::List(
                    DEFAULT_EXCLUDE_LABELS
                        .iter()
                        .map(|l| Value::String((*l).to_string()))
                        .collect(),
                ),
            ),
        ]);
        for (key, v) in map {
            match key.as_str() {
                "candidates" | "rrfK" | "mode" => {
                    search.insert(key.clone(), v.clone());
                    continue;
                }
                "hops" | "direction" | "relTypes" | "labels" | "excludeLabels" | "perNodeLimit"
                | "maxNodes" | "asOf" => {
                    if !matches!(v, Value::Null) {
                        walk.insert(key.clone(), v.clone());
                    }
                    continue;
                }
                _ => {}
            }
            if matches!(v, Value::Null) {
                if CONFIG_KEYS.split(", ").any(|k| k == key) {
                    continue;
                }
                return Err(unknown(key));
            }
            match key.as_str() {
                "label" => match v {
                    Value::String(label) => config.label = Some(label.clone()),
                    other => {
                        return Err(format!(
                            "label must be a string or null, got {}",
                            describe(other)
                        ))
                    }
                },
                "embeddingProperty" => match v {
                    Value::String(p) => config.embedding_property = p.clone(),
                    other => {
                        return Err(format!(
                            "embeddingProperty must be a string, got {}",
                            describe(other)
                        ))
                    }
                },
                "k" => config.k = parse_k(v)?,
                "rank" => {
                    config.rank = match v {
                        Value::String(name) => RankMode::parse(name),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "rank must be 'ppr', 'distance' or 'none', got {}",
                            describe(v)
                        )
                    })?;
                }
                "source" => config.sources = Some(strings("source", v)?),
                "tokenBudget" => {
                    config.render.token_budget = whole(v, MIN_TOKEN_BUDGET).ok_or_else(|| {
                        format!(
                            "tokenBudget must be an integer ≥ {MIN_TOKEN_BUDGET}, got {}",
                            describe(v)
                        )
                    })?;
                }
                "tokenizer" => {
                    config.render.tokenizer = match v {
                        Value::String(name) => Tokenizer::parse(name),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!("tokenizer must be 'approx' or 'words', got {}", describe(v))
                    })?;
                }
                "format" => {
                    config.render.format = match v {
                        Value::String(name) => Format::parse(name),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "format must be 'markdown', 'triples' or 'json', got {}",
                            describe(v)
                        )
                    })?;
                }
                "textProperties" => {
                    config.render.text_properties = strings("textProperties", v)?;
                }
                "maxBodyChars" => {
                    config.render.max_body_chars = whole(v, 1).ok_or_else(|| {
                        format!("maxBodyChars must be an integer ≥ 1, got {}", describe(v))
                    })?;
                }
                "includeProvenance" => match v {
                    Value::Bool(b) => config.render.include_provenance = *b,
                    other => {
                        return Err(format!(
                            "includeProvenance must be a boolean, got {}",
                            describe(other)
                        ))
                    }
                },
                "session" => match v {
                    Value::String(session) => config.session = Some(session.clone()),
                    other => {
                        return Err(format!(
                            "session must be a string or null, got {}",
                            describe(other)
                        ))
                    }
                },
                "trace" => match v {
                    Value::Bool(b) => config.trace = *b,
                    other => {
                        return Err(format!("trace must be a boolean, got {}", describe(other)))
                    }
                },
                "scope" => {
                    config.scope = match v {
                        Value::String(name) => Scope::parse(name),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "scope must be 'local', 'global' or 'auto', got {}",
                            describe(v)
                        )
                    })?;
                }
                "globalThreshold" => {
                    config.global_threshold = match v {
                        Value::Integer(i) => Some(*i as f64),
                        Value::Float(f) => Some(*f),
                        _ => None,
                    }
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .ok_or_else(|| {
                        format!("globalThreshold must be a number ≥ 0, got {}", describe(v))
                    })?;
                }
                "communityLayer" => match v {
                    Value::String(layer) if !layer.is_empty() => {
                        config.community_layer = layer.clone();
                    }
                    other => {
                        return Err(format!(
                            "communityLayer must be a non-empty string, got {}",
                            describe(other)
                        ))
                    }
                },
                "globalHint" => match v {
                    Value::String(hint) => {
                        global_hint_matches(hint, "")?;
                        config.global_hint = hint.clone();
                    }
                    other => {
                        return Err(format!(
                            "globalHint must be a string, got {}",
                            describe(other)
                        ))
                    }
                },
                other => return Err(unknown(other)),
            }
        }
        config.hybrid = HybridConfig::from_value(&Value::Map(search))?;
        config.expand = expand_options(&Value::Map(walk), ceilings)?;
        Ok(config)
    }
}

impl RagConfig {
    /// The effective config as a JSON object: every key of
    /// [`CONFIG_KEYS`] but `session` and `trace`, with its value or default.
    /// Read back with [`from_value`](Self::from_value) it gives the same
    /// config without a session or trace — what a context trace stores to
    /// replay the call.
    ///
    /// ```
    /// use drevo::rag::config::RagConfig;
    ///
    /// let json: serde_json::Value =
    ///     serde_json::from_str(&RagConfig::default().to_json()).unwrap();
    /// assert_eq!(json["tokenBudget"], 2000);
    /// assert_eq!(json["format"], "markdown");
    /// assert!(json.get("session").is_none());
    /// ```
    #[must_use]
    pub fn to_json(&self) -> String {
        use serde_json::{json, Value as Json};
        let strings = |items: &[String]| Json::from(items.to_vec());
        let direction = match self.expand.direction {
            crate::model::Direction::Outgoing => "outgoing",
            crate::model::Direction::Incoming => "incoming",
            crate::model::Direction::Both => "both",
        };
        json!({
            "label": self.label,
            "embeddingProperty": self.embedding_property,
            "k": self.k,
            "candidates": self.hybrid.candidates,
            "rrfK": self.hybrid.rrf_k,
            "mode": self.hybrid.mode.as_str(),
            "hops": self.expand.hops,
            "direction": direction,
            "relTypes": self.expand.rel_types.as_deref().map(strings),
            "labels": self.expand.labels.as_deref().map(strings),
            "excludeLabels": strings(&self.expand.exclude_labels),
            "perNodeLimit": self.expand.per_node_limit,
            "maxNodes": self.expand.max_nodes,
            "asOf": self.expand.as_of,
            "rank": self.rank.as_str(),
            "source": self.sources.as_deref().map(strings),
            "tokenBudget": self.render.token_budget,
            "tokenizer": self.render.tokenizer.as_str(),
            "format": self.render.format.as_str(),
            "textProperties": strings(&self.render.text_properties),
            "maxBodyChars": self.render.max_body_chars,
            "includeProvenance": self.render.include_provenance,
            "scope": self.scope.as_str(),
            "globalThreshold": self.global_threshold,
            "globalHint": self.global_hint,
            "communityLayer": self.community_layer,
        })
        .to_string()
    }
}

impl Scope {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "global" => Some(Self::Global),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

impl RankMode {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "ppr" => Some(Self::Ppr),
            "distance" => Some(Self::Distance),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// The error for an unknown key.
fn unknown(key: &str) -> String {
    format!("unknown config key `{key}`; expected {CONFIG_KEYS}")
}

/// `value` as a whole number of at least `min`.
fn whole(value: &Value, min: usize) -> Option<usize> {
    match value {
        Value::Integer(n) => usize::try_from(*n).ok().filter(|n| *n >= min),
        _ => None,
    }
}

/// `value` as a list of strings, for the config key `key`.
fn strings(key: &str, value: &Value) -> Result<Vec<String>, String> {
    let Value::List(items) = value else {
        return Err(format!(
            "{key} must be a list of strings, got {}",
            describe(value)
        ));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(s) => Ok(s.clone()),
            other => Err(format!(
                "{key} must be a list of strings, got a list holding {}",
                describe(other)
            )),
        })
        .collect()
}

/// A config value as an error message shows it.
fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => other.type_name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::hybrid::HybridMode;
    use crate::model::Direction;
    use crate::rag::serialize::Format;
    use crate::rag::tokens::Tokenizer;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn parse(entries: &[(&str, Value)]) -> Result<RagConfig, String> {
        RagConfig::from_value(&map(entries), ExpandCeilings::default())
    }

    fn refused(entries: &[(&str, Value)]) -> String {
        match parse(entries) {
            Err(message) => message,
            Ok(config) => panic!("{entries:?} was accepted as {config:?}"),
        }
    }

    fn s(text: &str) -> Value {
        Value::String(text.to_string())
    }

    fn strings(items: &[&str]) -> Value {
        Value::List(items.iter().map(|i| s(i)).collect())
    }

    #[test]
    fn null_and_an_empty_map_give_the_documented_defaults() {
        for value in [Value::Null, map(&[])] {
            let config = RagConfig::from_value(&value, ExpandCeilings::default()).unwrap();
            assert_eq!(config, RagConfig::default());
        }
        let d = RagConfig::default();
        assert_eq!(d.label, None);
        assert_eq!(d.embedding_property, "embedding");
        assert_eq!(d.k, 8);
        assert_eq!(d.hybrid.candidates, None);
        assert_eq!(d.hybrid.depth(d.k), 20);
        assert_eq!(d.hybrid.rrf_k, 60.0);
        assert_eq!(d.hybrid.mode, HybridMode::Auto);
        assert_eq!(d.expand.hops, 2);
        assert_eq!(d.expand.direction, Direction::Both);
        assert_eq!(d.expand.rel_types, None);
        assert_eq!(d.expand.labels, None);
        assert_eq!(
            d.expand.exclude_labels,
            [
                "Message",
                "ReasoningTrace",
                "ContextTrace",
                "Community",
                "IngestJob",
                "IngestItem"
            ]
        );
        assert_eq!(d.expand.per_node_limit, 25);
        assert_eq!(d.expand.max_nodes, 200);
        assert_eq!(d.expand.as_of, None);
        assert!(d.expand.include_seeds);
        assert_eq!(d.rank, RankMode::Ppr);
        assert_eq!(d.sources, None);
        assert_eq!(d.render.token_budget, 2000);
        assert_eq!(d.render.tokenizer, Tokenizer::Approx);
        assert_eq!(d.render.format, Format::Markdown);
        assert_eq!(
            d.render.text_properties,
            ["name", "title", "description", "body", "text"]
        );
        assert_eq!(d.render.max_body_chars, 600);
        assert!(d.render.include_provenance);
        assert_eq!(d.session, None);
        assert!(!d.trace);
        assert_eq!(d.scope, Scope::Local);
        assert_eq!(d.global_threshold, 0.02);
        assert_eq!(d.global_hint, "^(what|что|о чём|overview|summary)");
        assert_eq!(d.community_layer, "default");
    }

    #[test]
    fn every_key_is_read() {
        let config = parse(&[
            ("label", s("Entry")),
            ("embeddingProperty", s("vec")),
            ("k", Value::Integer(3)),
            ("candidates", Value::Integer(40)),
            ("rrfK", Value::Integer(10)),
            ("mode", s("fts")),
            ("hops", Value::Integer(1)),
            ("direction", s("outgoing")),
            ("relTypes", strings(&["WROTE"])),
            ("labels", strings(&["Entry"])),
            ("excludeLabels", strings(&[])),
            ("perNodeLimit", Value::Integer(5)),
            ("maxNodes", Value::Integer(50)),
            ("asOf", s("2025-06-01")),
            ("rank", s("none")),
            ("source", strings(&["journal-1"])),
            ("tokenBudget", Value::Integer(50)),
            ("tokenizer", s("words")),
            ("format", s("json")),
            ("textProperties", strings(&["title"])),
            ("maxBodyChars", Value::Integer(80)),
            ("includeProvenance", Value::Bool(false)),
            ("session", s("chat-7")),
            ("trace", Value::Bool(true)),
            ("scope", s("auto")),
            ("globalThreshold", Value::Float(0.05)),
            ("globalHint", s("^(overview|tl;dr)")),
            ("communityLayer", s("fine")),
        ])
        .unwrap();
        assert_eq!(config.community_layer, "fine");
        assert_eq!(config.scope, Scope::Auto);
        assert_eq!(config.global_threshold, 0.05);
        assert_eq!(config.global_hint, "^(overview|tl;dr)");
        assert_eq!(config.label.as_deref(), Some("Entry"));
        assert_eq!(config.embedding_property, "vec");
        assert_eq!(config.k, 3);
        assert_eq!(config.hybrid.candidates, Some(40));
        assert_eq!(config.hybrid.rrf_k, 10.0);
        assert_eq!(config.hybrid.mode, HybridMode::Fts);
        assert_eq!(config.expand.hops, 1);
        assert_eq!(config.expand.direction, Direction::Outgoing);
        assert_eq!(config.expand.rel_types, Some(vec!["WROTE".to_string()]));
        assert_eq!(config.expand.labels, Some(vec!["Entry".to_string()]));
        assert!(config.expand.exclude_labels.is_empty());
        assert_eq!(config.expand.per_node_limit, 5);
        assert_eq!(config.expand.max_nodes, 50);
        assert_eq!(config.expand.as_of.as_deref(), Some("2025-06-01"));
        assert_eq!(config.rank, RankMode::None);
        assert_eq!(config.sources, Some(vec!["journal-1".to_string()]));
        assert_eq!(config.render.token_budget, 50);
        assert_eq!(config.render.tokenizer, Tokenizer::Words);
        assert_eq!(config.render.format, Format::Json);
        assert_eq!(config.render.text_properties, ["title"]);
        assert_eq!(config.render.max_body_chars, 80);
        assert!(!config.render.include_provenance);
        assert_eq!(config.session.as_deref(), Some("chat-7"));
        assert!(config.trace);
    }

    #[test]
    fn a_null_value_keeps_the_rag_default_not_the_expand_one() {
        let config = parse(&[
            ("hops", Value::Null),
            ("excludeLabels", Value::Null),
            ("session", Value::Null),
            ("trace", Value::Null),
            ("label", Value::Null),
        ])
        .unwrap();
        assert_eq!(config, RagConfig::default());
    }

    #[test]
    fn rank_modes_parse() {
        for mode in [RankMode::Ppr, RankMode::Distance, RankMode::None] {
            assert_eq!(parse(&[("rank", s(mode.as_str()))]).unwrap().rank, mode);
        }
    }

    #[test]
    fn every_error_names_its_key() {
        let cases: Vec<(Vec<(&str, Value)>, &str)> =
            vec![
            (
                vec![("depth", Value::Integer(2))],
                "unknown config key `depth`; expected label, embeddingProperty, k, candidates, \
                 rrfK, mode, hops, direction, relTypes, labels, excludeLabels, perNodeLimit, \
                 maxNodes, asOf, rank, source, tokenBudget, tokenizer, format, textProperties, \
                 maxBodyChars, includeProvenance, scope, globalThreshold, globalHint, \
                 communityLayer, session, trace",
            ),
            (
                vec![("communityLayer", s(""))],
                "communityLayer must be a non-empty string, got ''",
            ),
            (
                vec![("communityLayer", Value::Integer(2))],
                "communityLayer must be a non-empty string, got 2",
            ),
            (
                vec![("scope", s("galaxy"))],
                "scope must be 'local', 'global' or 'auto', got 'galaxy'",
            ),
            (
                vec![("globalThreshold", Value::Float(-0.1))],
                "globalThreshold must be a number ≥ 0, got -0.1",
            ),
            (
                vec![("globalThreshold", s("low"))],
                "globalThreshold must be a number ≥ 0, got 'low'",
            ),
            (
                vec![("globalHint", Value::Integer(1))],
                "globalHint must be a string, got 1",
            ),
            (
                vec![("globalHint", s("^(what"))],
                "globalHint is not a valid regular expression: ",
            ),
            (
                vec![("includeSeeds", Value::Bool(false))],
                "unknown config key `includeSeeds`; expected label",
            ),
            (
                vec![("tokenBudget", Value::Integer(10))],
                "tokenBudget must be an integer ≥ 50, got 10",
            ),
            (
                vec![("tokenBudget", s("lots"))],
                "tokenBudget must be an integer ≥ 50, got 'lots'",
            ),
            (
                vec![("format", s("yaml"))],
                "format must be 'markdown', 'triples' or 'json', got 'yaml'",
            ),
            (
                vec![("tokenizer", s("tiktoken"))],
                "tokenizer must be 'approx' or 'words', got 'tiktoken'",
            ),
            (
                vec![("rank", s("random"))],
                "rank must be 'ppr', 'distance' or 'none', got 'random'",
            ),
            (
                vec![("session", Value::Integer(5))],
                "session must be a string or null, got 5",
            ),
            (
                vec![("trace", s("yes"))],
                "trace must be a boolean, got 'yes'",
            ),
            (
                vec![("label", Value::Integer(5))],
                "label must be a string or null, got 5",
            ),
            (
                vec![("embeddingProperty", Value::Bool(true))],
                "embeddingProperty must be a string, got Boolean",
            ),
            (vec![("k", Value::Integer(0))], "k must be a positive integer, got 0"),
            (
                vec![("maxBodyChars", Value::Integer(0))],
                "maxBodyChars must be an integer ≥ 1, got 0",
            ),
            (
                vec![("includeProvenance", s("yes"))],
                "includeProvenance must be a boolean, got 'yes'",
            ),
            (
                vec![("textProperties", s("title"))],
                "textProperties must be a list of strings, got 'title'",
            ),
            (
                vec![("textProperties", Value::List(vec![Value::Integer(1)]))],
                "textProperties must be a list of strings, got a list holding 1",
            ),
            (
                vec![("source", s("journal-1"))],
                "source must be a list of strings, got 'journal-1'",
            ),
            // The search's and the expansion's own messages come through.
            (
                vec![("mode", s("magic"))],
                "mode must be 'auto', 'hybrid', 'fts' or 'vector', got 'magic'",
            ),
            (
                vec![("candidates", Value::Integer(0))],
                "candidates must be a positive integer, got 0",
            ),
            (
                vec![("hops", s("two"))],
                "hops must be an integer ≥ 0, got 'two'",
            ),
            (
                vec![("hops", Value::Integer(7))],
                "hops 7 is above the server's limit of 6 (DREVO_EXPAND_MAX_HOPS)",
            ),
            (
                vec![("direction", s("up"))],
                "direction must be 'outgoing', 'incoming' or 'both', got 'up'",
            ),
        ];
        for (entries, expected) in cases {
            let message = refused(&entries);
            assert!(
                message.starts_with(expected),
                "{entries:?}: expected {expected:?}, got {message:?}"
            );
        }
        let not_a_map = RagConfig::from_value(&s("fast"), ExpandCeilings::default());
        assert_eq!(
            not_a_map,
            Err("the config must be a map, got String".to_string())
        );
    }

    /// `json` parsed back into a config.
    fn reparse(json: &str) -> RagConfig {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        RagConfig::from_value(
            &crate::cypher::executor::json_to_value(&value),
            ExpandCeilings::default(),
        )
        .unwrap()
    }

    #[test]
    fn the_json_config_reads_back_as_the_same_config_without_session_or_trace() {
        let custom = parse(&[
            ("label", s("Entry")),
            ("embeddingProperty", s("vec")),
            ("k", Value::Integer(3)),
            ("candidates", Value::Integer(40)),
            ("rrfK", Value::Float(12.5)),
            ("mode", s("fts")),
            ("hops", Value::Integer(1)),
            ("direction", s("incoming")),
            ("relTypes", strings(&["WROTE"])),
            ("labels", strings(&["Entry", "Person"])),
            ("excludeLabels", strings(&[])),
            ("perNodeLimit", Value::Integer(5)),
            ("maxNodes", Value::Integer(50)),
            ("asOf", s("2025-06-01")),
            ("rank", s("distance")),
            ("source", strings(&["journal-1"])),
            ("tokenBudget", Value::Integer(75)),
            ("tokenizer", s("words")),
            ("format", s("triples")),
            ("textProperties", strings(&["title"])),
            ("maxBodyChars", Value::Integer(80)),
            ("includeProvenance", Value::Bool(false)),
            ("session", s("chat-7")),
            ("trace", Value::Bool(true)),
            ("scope", s("global")),
            ("globalThreshold", Value::Float(0.01)),
            ("globalHint", s("^overview")),
            ("communityLayer", s("fine")),
        ])
        .unwrap();
        for config in [RagConfig::default(), custom] {
            let back = reparse(&config.to_json());
            assert_eq!(
                back,
                RagConfig {
                    session: None,
                    trace: false,
                    ..config.clone()
                }
            );
        }
    }

    #[test]
    fn the_json_config_names_every_key_but_session_and_trace() {
        let json: serde_json::Value =
            serde_json::from_str(&RagConfig::default().to_json()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected: Vec<&str> = CONFIG_KEYS
            .split(", ")
            .filter(|k| *k != "session" && *k != "trace")
            .collect();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(json["tokenBudget"], 2000);
        assert_eq!(json["rank"], "ppr");
        assert_eq!(json["label"], serde_json::Value::Null);
        assert_eq!(json["scope"], "local");
        assert_eq!(json["globalThreshold"], 0.02);
    }

    #[test]
    fn scopes_parse() {
        for scope in [Scope::Local, Scope::Global, Scope::Auto] {
            assert_eq!(parse(&[("scope", s(scope.as_str()))]).unwrap().scope, scope);
        }
        let integer = parse(&[("globalThreshold", Value::Integer(0))]).unwrap();
        assert_eq!(integer.global_threshold, 0.0);
    }

    #[test]
    fn the_global_hint_matches_the_start_of_the_question_ignoring_case() {
        let hint = DEFAULT_GLOBAL_HINT;
        for q in [
            "What is this project about?",
            "what are the teams",
            "Что здесь происходит?",
            "о чём этот корпус",
            "Overview please",
            "SUMMARY\nof the year",
        ] {
            assert_eq!(global_hint_matches(hint, q), Ok(true), "{q}");
        }
        for q in ["BE-142 login token", "so what", "", "show me what failed"] {
            assert_eq!(global_hint_matches(hint, q), Ok(false), "{q}");
        }
        assert_eq!(
            global_hint_matches("^tl;dr", "TL;DR of the sprint"),
            Ok(true)
        );
        let bad = global_hint_matches("^(what", "what").unwrap_err();
        assert!(
            bad.starts_with("globalHint is not a valid regular expression: "),
            "{bad}"
        );
    }
}
