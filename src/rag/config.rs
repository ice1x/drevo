//! The config map of `drevo.rag.retrieve` and `drevo.rag.context`.
//!
//! One map configures every stage: the seed search (`label`,
//! `embeddingProperty`, `k`, `candidates`, `rrfK`, `mode`), the expansion
//! (`hops`, `direction`, `relTypes`, `labels`, `excludeLabels`,
//! `perNodeLimit`, `maxNodes`, `asOf`), the ranking (`rank`), the provenance
//! filter (`source`) and the serialiser (`tokenBudget`, `tokenizer`,
//! `format`, `textProperties`, `maxBodyChars`, `includeProvenance`), plus
//! `session`. An unknown key, or a value of the wrong type or range, is
//! refused by name:
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
     tokenBudget, tokenizer, format, textProperties, maxBodyChars, includeProvenance, session";

/// The labels left out of the expansion unless `excludeLabels` says
/// otherwise: conversation and bookkeeping nodes, not knowledge.
pub(crate) const DEFAULT_EXCLUDE_LABELS: [&str; 5] = [
    "Message",
    "ReasoningTrace",
    "ContextTrace",
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
    /// The session the context is for, if any.
    pub session: Option<String>,
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
                other => return Err(unknown(other)),
            }
        }
        config.hybrid = HybridConfig::from_value(&Value::Map(search))?;
        config.expand = expand_options(&Value::Map(walk), ceilings)?;
        Ok(config)
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
        ])
        .unwrap();
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
    }

    #[test]
    fn a_null_value_keeps_the_rag_default_not_the_expand_one() {
        let config = parse(&[
            ("hops", Value::Null),
            ("excludeLabels", Value::Null),
            ("session", Value::Null),
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
                 maxBodyChars, includeProvenance, session",
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
}
