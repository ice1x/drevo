//! Measuring retrieval: does the graph help on this corpus? A gold set says
//! which nodes should reach the model for each question;
//! `drevo.rag.evaluate` runs every retrieval strategy over it with one token
//! budget and scores what each one would have handed over, and
//! `drevo.rag.evaluateSummary` averages the scores per strategy.
//!
//! This module holds the pure parts — the strategies, reading the gold set,
//! the metrics and their aggregation — tested without an engine:
//!
//! ```
//! use drevo::rag::eval::{score, Strategy};
//!
//! // The context held nodes 4, 9 and 2 in that order; nodes 9 and 7 were
//! // the right answer, and 7 was retrieved but did not fit the budget.
//! let s = score(&[4, 9, 2], &[4, 9, 2, 7], &[9, 7]);
//! assert!(s.hit);
//! assert_eq!(s.recall, 0.5);
//! assert_eq!(s.precision, 1.0 / 3.0);
//! assert_eq!(s.mrr, 0.5);
//! assert_eq!(s.recall_retrieved, 1.0);
//! assert_eq!(Strategy::parse("hybrid+expand"), Some(Strategy::HybridExpand));
//! ```

use std::collections::{BTreeMap, HashSet};

use crate::cypher::executor::Value;

/// One way of picking the nodes for a question's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// The top `k` nodes of `label` by embedding similarity alone, as
    /// `drevo.semantic.query` ranks them — the baseline.
    Vector,
    /// The top `k` BM25 matches.
    Fts,
    /// The top `k` hits of the hybrid BM25 + vector search.
    Hybrid,
    /// The hybrid hits and the nodes within `hops` of them, nearest first.
    HybridExpand,
    /// The full graph-RAG pipeline: hybrid seeds, expansion, Personalized
    /// PageRank.
    Rag,
    /// Retrieval over community summaries.
    RagGlobal,
}

impl Strategy {
    /// The strategy named `name` in a config's `strategies` list.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "vector" => Some(Self::Vector),
            "fts" => Some(Self::Fts),
            "hybrid" => Some(Self::Hybrid),
            "hybrid+expand" => Some(Self::HybridExpand),
            "rag" => Some(Self::Rag),
            "rag-global" => Some(Self::RagGlobal),
            _ => None,
        }
    }

    /// The strategy's name as written in `strategies` and returned in rows.
    ///
    /// ```
    /// use drevo::rag::eval::Strategy;
    /// assert_eq!(Strategy::HybridExpand.as_str(), "hybrid+expand");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Fts => "fts",
            Self::Hybrid => "hybrid",
            Self::HybridExpand => "hybrid+expand",
            Self::Rag => "rag",
            Self::RagGlobal => "rag-global",
        }
    }
}

/// The names a `strategies` list accepts, as listed in errors.
pub(crate) const STRATEGY_NAMES: &str =
    "'vector', 'fts', 'hybrid', 'hybrid+expand', 'rag' or 'rag-global'";

/// One expected node of a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedRef {
    /// The node with this id.
    Id(u64),
    /// The one node whose `name` or `title` property is `name`, with
    /// `label` when given.
    Named {
        /// The exact value of its `name` property.
        name: String,
        /// A label it carries.
        label: Option<String>,
    },
}

/// Whether a node with `properties` is the one an expected `{name}` names:
/// its `name` or its `title` property is exactly `name`.
///
/// ```
/// use std::collections::BTreeMap;
/// use drevo::cypher::executor::Value;
/// use drevo::rag::eval::names;
///
/// let props = BTreeMap::from([("title".to_string(), Value::String("Fix".to_string()))]);
/// assert!(names(&props, "Fix"));
/// assert!(!names(&props, "fix"));
/// ```
#[must_use]
pub fn names(properties: &BTreeMap<String, Value>, name: &str) -> bool {
    ["name", "title"]
        .iter()
        .any(|key| matches!(properties.get(*key), Some(Value::String(v)) if v == name))
}

/// What a question is scored against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gold {
    /// The nodes that should be in its context.
    Expected(Vec<ExpectedRef>),
    /// The nodes a recorded context trace included.
    Trace(String),
}

/// One question of a gold set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The question text; `None` only for a trace question, which then asks
    /// the trace's own question.
    pub query: Option<String>,
    /// What its answer is scored against.
    pub gold: Gold,
}

/// Read the `questions` argument: a list of `{query, expected}` maps, where
/// `expected` lists node ids or `{name, label}` maps, or `{query, traceId}`
/// maps (`query` optional) scored against what that trace included.
///
/// # Errors
///
/// A message naming the offending entry (`questions[2].expected[0]`) when the
/// shape is wrong.
pub fn parse_questions(value: &Value) -> Result<Vec<Question>, String> {
    let Value::List(items) = value else {
        return Err(format!(
            "questions must be a list of maps, got {}",
            value.type_name()
        ));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_question(i, item))
        .collect()
}

/// The question at index `i` of the gold set.
fn parse_question(i: usize, item: &Value) -> Result<Question, String> {
    let at = format!("questions[{i}]");
    let Value::Map(map) = item else {
        return Err(format!("{at} must be a map, got {}", item.type_name()));
    };
    if let Some(key) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "query" | "expected" | "traceId"))
    {
        return Err(format!(
            "{at}: unknown key `{key}`; expected query, expected, traceId"
        ));
    }
    let query = match map.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(q)) => Some(q.clone()),
        Some(other) => {
            return Err(format!(
                "{at}.query must be a string, got {}",
                other.type_name()
            ))
        }
    };
    let present = |key: &str| map.get(key).is_some_and(|v| !matches!(v, Value::Null));
    let gold = match (present("expected"), present("traceId")) {
        (true, true) => return Err(format!("{at} takes expected or traceId, not both")),
        (false, false) => return Err(format!("{at} needs expected or traceId")),
        (false, true) => match map.get("traceId") {
            Some(Value::String(id)) => Gold::Trace(id.clone()),
            other => {
                let got = other.map_or("Null", Value::type_name);
                return Err(format!("{at}.traceId must be a string, got {got}"));
            }
        },
        (true, false) => {
            if query.is_none() {
                return Err(format!("{at} needs a query"));
            }
            let expected = map.get("expected").unwrap_or(&Value::Null);
            Gold::Expected(parse_expected(&at, expected)?)
        }
    };
    Ok(Question { query, gold })
}

/// The `expected` list of the question at `at`.
fn parse_expected(at: &str, value: &Value) -> Result<Vec<ExpectedRef>, String> {
    let wanted = "a non-empty list of node ids or {name, label} maps";
    let items = match value {
        Value::List(items) if items.is_empty() => {
            return Err(format!("{at}.expected must be {wanted}, got an empty list"))
        }
        Value::List(items) => items,
        other => {
            return Err(format!(
                "{at}.expected must be {wanted}, got {}",
                other.type_name()
            ))
        }
    };
    items
        .iter()
        .enumerate()
        .map(|(j, item)| {
            let at = format!("{at}.expected[{j}]");
            let one = "must be a node id or a {name, label} map";
            match item {
                Value::Integer(id) => u64::try_from(*id)
                    .map(ExpectedRef::Id)
                    .map_err(|_| format!("{at} {one}, got {id}")),
                Value::Map(map) => {
                    if let Some(key) = map.keys().find(|k| !matches!(k.as_str(), "name" | "label"))
                    {
                        return Err(format!("{at}: unknown key `{key}`; expected name, label"));
                    }
                    let Some(Value::String(name)) = map.get("name") else {
                        return Err(format!("{at} {one} with a string name"));
                    };
                    let label = match map.get("label") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(label)) => Some(label.clone()),
                        Some(other) => {
                            return Err(format!(
                                "{at}.label must be a string, got {}",
                                other.type_name()
                            ))
                        }
                    };
                    Ok(ExpectedRef::Named {
                        name: name.clone(),
                        label,
                    })
                }
                other => Err(format!("{at} {one}, got {}", other.type_name())),
            }
        })
        .collect()
}

impl ExpectedRef {
    /// How the reference is written in an error: `id 7` or
    /// `{name: 'Mila', label: 'Person'}`.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Id(id) => format!("id {id}"),
            Self::Named {
                name,
                label: Some(label),
            } => format!("{{name: '{name}', label: '{label}'}}"),
            Self::Named { name, label: None } => format!("{{name: '{name}'}}"),
        }
    }
}

/// Split an evaluation config into the `strategies` it names (`None`: the
/// default) and the rest, a `drevo.rag.retrieve` config map.
///
/// # Errors
///
/// When the config is not a map or `null`, `strategies` is not a non-empty
/// list of strategy names, or the config asks for a trace (`session` or
/// `trace: true`): an evaluation never records one.
pub fn split_config(value: &Value) -> Result<(Value, Option<Vec<Strategy>>), String> {
    let mut map = match value {
        Value::Null => return Ok((Value::Null, None)),
        Value::Map(map) => map.clone(),
        other => {
            return Err(format!(
                "the config must be a map, got {}",
                other.type_name()
            ))
        }
    };
    let never = |key: &str| format!("an evaluation never records a context trace; remove `{key}`");
    match map.get("session") {
        None | Some(Value::Null) => {}
        Some(_) => return Err(never("session")),
    }
    if matches!(map.get("trace"), Some(Value::Bool(true))) {
        return Err(never("trace"));
    }
    let strategies = match map.remove("strategies") {
        None | Some(Value::Null) => None,
        Some(Value::List(names)) if !names.is_empty() => {
            let mut strategies = Vec::with_capacity(names.len());
            for name in &names {
                let strategy = match name {
                    Value::String(n) => Strategy::parse(n).ok_or_else(|| {
                        format!("unknown strategy '{n}'; expected {STRATEGY_NAMES}")
                    }),
                    other => Err(format!(
                        "unknown strategy {}; expected {STRATEGY_NAMES}",
                        other.type_name()
                    )),
                }?;
                if !strategies.contains(&strategy) {
                    strategies.push(strategy);
                }
            }
            Some(strategies)
        }
        Some(other) => {
            let got = match &other {
                Value::List(_) => "an empty list",
                v => v.type_name(),
            };
            return Err(format!(
                "strategies must be a non-empty list of {STRATEGY_NAMES}, got {got}"
            ));
        }
    };
    Ok((Value::Map(map), strategies))
}

/// How one strategy did on one question.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// Whether the context held any expected node.
    pub hit: bool,
    /// The share of the expected nodes the context held.
    pub recall: f64,
    /// The share of the context's nodes that were expected; `0` for an empty
    /// context.
    pub precision: f64,
    /// `1 / rank` of the first expected node in the context, `0` when none
    /// is there.
    pub mrr: f64,
    /// The share of the expected nodes retrieved at all, before the budget
    /// cut: `recall_retrieved - recall` is what the budget lost.
    pub recall_retrieved: f64,
}

/// Score a context: `included` the node ids written into it in order,
/// `retrieved` every node id retrieved for it before the budget, `expected`
/// the gold ids (duplicates count once; none gives zero recall).
#[must_use]
pub fn score(included: &[u64], retrieved: &[u64], expected: &[u64]) -> Score {
    let gold: HashSet<u64> = expected.iter().copied().collect();
    let found = |ids: &[u64]| {
        ids.iter()
            .copied()
            .collect::<HashSet<u64>>()
            .intersection(&gold)
            .count()
    };
    let share = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            part as f64 / whole as f64
        }
    };
    let in_context = found(included);
    let first = included.iter().position(|id| gold.contains(id));
    Score {
        hit: in_context > 0,
        recall: share(in_context, gold.len()),
        precision: share(in_context, included.len()),
        mrr: first.map_or(0.0, |rank| 1.0 / (rank + 1) as f64),
        recall_retrieved: share(found(retrieved), gold.len()),
    }
}

/// The `p`-th percentile (`0.0..=1.0`) of `values`, interpolating linearly
/// between the two nearest ranks: the median of an even count is the mean
/// of the middle two. `None` for no values.
///
/// ```
/// use drevo::rag::eval::percentile;
/// assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 0.5), Some(2.5));
/// ```
#[must_use]
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let last = sorted.len().checked_sub(1)?;
    let position = p.clamp(0.0, 1.0) * last as f64;
    // `position` lies in `0..=last`, so both casts stay in range.
    let below = position.floor() as usize;
    let above = position.ceil() as usize;
    let (low, high) = (sorted.get(below)?, sorted.get(above)?);
    Some(low + (high - low) * (position - below as f64))
}

/// One strategy's run on one question.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The strategy.
    pub strategy: Strategy,
    /// Its score.
    pub score: Score,
    /// The size of its context in tokens.
    pub tokens: usize,
    /// How long retrieval and writing the context took, in milliseconds.
    pub duration_ms: u64,
}

/// One strategy's runs over a whole gold set, averaged.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// The strategy.
    pub strategy: Strategy,
    /// How many questions it ran on.
    pub questions: usize,
    /// The share of questions with a hit.
    pub hit_rate: f64,
    /// The mean recall.
    pub mean_recall: f64,
    /// The mean precision.
    pub mean_precision: f64,
    /// The mean reciprocal rank.
    pub mrr: f64,
    /// The median duration in milliseconds.
    pub p50_ms: f64,
    /// The 95th-percentile duration in milliseconds.
    pub p95_ms: f64,
    /// The mean context size in tokens.
    pub mean_tokens: f64,
}

/// Average `runs` per strategy, one summary per strategy of `strategies`
/// that has runs, in that order.
#[must_use]
pub fn summarize(runs: &[Run], strategies: &[Strategy]) -> Vec<Summary> {
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    strategies
        .iter()
        .filter_map(|&strategy| {
            let mine: Vec<&Run> = runs.iter().filter(|r| r.strategy == strategy).collect();
            if mine.is_empty() {
                return None;
            }
            let of = |f: fn(&Run) -> f64| mine.iter().map(|r| f(r)).collect::<Vec<f64>>();
            let durations = of(|r| r.duration_ms as f64);
            Some(Summary {
                strategy,
                questions: mine.len(),
                hit_rate: mean(&of(|r| if r.score.hit { 1.0 } else { 0.0 })),
                mean_recall: mean(&of(|r| r.score.recall)),
                mean_precision: mean(&of(|r| r.score.precision)),
                mrr: mean(&of(|r| r.score.mrr)),
                p50_ms: percentile(&durations, 0.5).unwrap_or(0.0),
                p95_ms: percentile(&durations, 0.95).unwrap_or(0.0),
                mean_tokens: mean(&of(|r| r.tokens as f64)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Value {
        Value::String(text.to_string())
    }

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn list(items: Vec<Value>) -> Value {
        Value::List(items)
    }

    // ── strategies ──────────────────────────────────────────────────────

    #[test]
    fn strategy_names_round_trip() {
        for (name, strategy) in [
            ("vector", Strategy::Vector),
            ("fts", Strategy::Fts),
            ("hybrid", Strategy::Hybrid),
            ("hybrid+expand", Strategy::HybridExpand),
            ("rag", Strategy::Rag),
            ("rag-global", Strategy::RagGlobal),
        ] {
            assert_eq!(Strategy::parse(name), Some(strategy));
            assert_eq!(strategy.as_str(), name);
        }
        assert_eq!(Strategy::parse("Vector"), None);
        assert_eq!(Strategy::parse("graph"), None);
    }

    // ── metrics ─────────────────────────────────────────────────────────

    #[test]
    fn a_perfect_context_scores_one_everywhere() {
        let s = score(&[1, 2, 3], &[1, 2, 3], &[3, 2, 1]);
        assert_eq!(
            s,
            Score {
                hit: true,
                recall: 1.0,
                precision: 1.0,
                mrr: 1.0,
                recall_retrieved: 1.0,
            }
        );
    }

    #[test]
    fn hand_computed_partial_context() {
        // Context 10, 11, 12, 13; expected 12, 20, 30; 20 was retrieved but
        // cut by the budget.
        let s = score(&[10, 11, 12, 13], &[10, 11, 12, 13, 20, 21], &[12, 20, 30]);
        assert!(s.hit);
        assert_eq!(s.recall, 1.0 / 3.0);
        assert_eq!(s.precision, 1.0 / 4.0);
        // The first expected node, 12, is third.
        assert_eq!(s.mrr, 1.0 / 3.0);
        assert_eq!(s.recall_retrieved, 2.0 / 3.0);
    }

    #[test]
    fn a_miss_scores_zero_and_an_empty_context_has_zero_precision() {
        let miss = score(&[1, 2], &[1, 2], &[5]);
        assert!(!miss.hit);
        assert_eq!(
            (miss.recall, miss.precision, miss.mrr, miss.recall_retrieved),
            (0.0, 0.0, 0.0, 0.0)
        );
        let empty = score(&[], &[], &[5]);
        assert!(!empty.hit);
        assert_eq!((empty.precision, empty.mrr), (0.0, 0.0));
    }

    #[test]
    fn duplicate_expected_ids_count_once_and_none_expected_gives_zero_recall() {
        let s = score(&[7], &[7], &[7, 7, 8]);
        assert_eq!(s.recall, 0.5);
        assert_eq!(s.precision, 1.0);
        let none = score(&[7], &[7], &[]);
        assert!(!none.hit);
        assert_eq!((none.recall, none.recall_retrieved), (0.0, 0.0));
    }

    #[test]
    fn a_name_matches_the_name_or_the_title_exactly() {
        let props = |entries: &[(&str, Value)]| -> BTreeMap<String, Value> {
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect()
        };
        assert!(names(&props(&[("name", s("Fix"))]), "Fix"));
        assert!(names(&props(&[("title", s("Fix"))]), "Fix"));
        assert!(names(
            &props(&[("name", s("Other")), ("title", s("Fix"))]),
            "Fix"
        ));
        assert!(!names(&props(&[("name", s("Fix "))]), "Fix"));
        assert!(!names(&props(&[("body", s("Fix"))]), "Fix"));
        assert!(!names(&props(&[("title", Value::Integer(1))]), "1"));
        assert!(!names(&props(&[]), "Fix"));
    }

    // ── percentiles ─────────────────────────────────────────────────────

    #[test]
    fn percentiles_of_an_odd_count() {
        let v = [9.0, 1.0, 5.0, 3.0, 7.0];
        assert_eq!(percentile(&v, 0.5), Some(5.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
        assert_eq!(percentile(&v, 1.0), Some(9.0));
        // 0.95 · 4 = 3.8 → 7 + 0.8 · (9 − 7).
        let p95 = percentile(&v, 0.95).unwrap();
        assert!((p95 - 8.6).abs() < 1e-9, "{p95}");
    }

    #[test]
    fn percentiles_of_an_even_count() {
        let v = [10.0, 40.0, 20.0, 30.0];
        assert_eq!(percentile(&v, 0.5), Some(25.0));
        // 0.95 · 3 = 2.85 → 30 + 0.85 · 10.
        let p95 = percentile(&v, 0.95).unwrap();
        assert!((p95 - 38.5).abs() < 1e-9, "{p95}");
    }

    #[test]
    fn percentiles_of_one_and_no_values() {
        assert_eq!(percentile(&[3.0], 0.5), Some(3.0));
        assert_eq!(percentile(&[3.0], 0.95), Some(3.0));
        assert_eq!(percentile(&[], 0.5), None);
    }

    // ── aggregation ─────────────────────────────────────────────────────

    fn run(strategy: Strategy, hit: bool, recall: f64, tokens: usize, ms: u64) -> Run {
        Run {
            strategy,
            score: Score {
                hit,
                recall,
                precision: recall / 2.0,
                mrr: if hit { 1.0 } else { 0.0 },
                recall_retrieved: recall,
            },
            tokens,
            duration_ms: ms,
        }
    }

    #[test]
    fn summaries_average_per_strategy_in_the_order_asked() {
        let runs = [
            run(Strategy::Rag, true, 1.0, 100, 4),
            run(Strategy::Vector, false, 0.0, 10, 1),
            run(Strategy::Rag, true, 0.5, 200, 2),
            run(Strategy::Vector, true, 0.5, 30, 3),
            run(Strategy::Rag, false, 0.0, 300, 9),
        ];
        let summaries = summarize(&runs, &[Strategy::Vector, Strategy::Fts, Strategy::Rag]);
        assert_eq!(summaries.len(), 2, "no runs, no summary: {summaries:?}");
        let vector = &summaries[0];
        assert_eq!(vector.strategy, Strategy::Vector);
        assert_eq!(vector.questions, 2);
        assert_eq!(vector.hit_rate, 0.5);
        assert_eq!(vector.mean_recall, 0.25);
        assert_eq!(vector.mean_precision, 0.125);
        assert_eq!(vector.mrr, 0.5);
        assert_eq!(vector.p50_ms, 2.0);
        assert_eq!(vector.mean_tokens, 20.0);
        let rag = &summaries[1];
        assert_eq!(rag.strategy, Strategy::Rag);
        assert_eq!(rag.questions, 3);
        assert_eq!(rag.hit_rate, 2.0 / 3.0);
        assert_eq!(rag.mean_recall, 0.5);
        assert_eq!(rag.p50_ms, 4.0);
        // 0.95 · 2 = 1.9 → 4 + 0.9 · (9 − 4).
        assert!((rag.p95_ms - 8.5).abs() < 1e-9, "{}", rag.p95_ms);
        assert_eq!(rag.mean_tokens, 200.0);
    }

    // ── questions ───────────────────────────────────────────────────────

    #[test]
    fn questions_take_ids_names_and_traces() {
        let questions = list(vec![
            map(&[
                ("query", s("why does login fail")),
                (
                    "expected",
                    list(vec![
                        Value::Integer(4),
                        map(&[("name", s("Crash log")), ("label", s("CrashLog"))]),
                        map(&[("name", s("Fix"))]),
                    ]),
                ),
            ]),
            map(&[("query", s("again")), ("traceId", s("t-1"))]),
            map(&[("traceId", s("t-2"))]),
        ]);
        assert_eq!(
            parse_questions(&questions),
            Ok(vec![
                Question {
                    query: Some("why does login fail".to_string()),
                    gold: Gold::Expected(vec![
                        ExpectedRef::Id(4),
                        ExpectedRef::Named {
                            name: "Crash log".to_string(),
                            label: Some("CrashLog".to_string()),
                        },
                        ExpectedRef::Named {
                            name: "Fix".to_string(),
                            label: None,
                        },
                    ]),
                },
                Question {
                    query: Some("again".to_string()),
                    gold: Gold::Trace("t-1".to_string()),
                },
                Question {
                    query: None,
                    gold: Gold::Trace("t-2".to_string()),
                },
            ])
        );
        assert_eq!(parse_questions(&list(Vec::new())), Ok(Vec::new()));
    }

    #[test]
    fn every_malformed_question_is_named() {
        let q = |entries: &[(&str, Value)]| list(vec![map(entries)]);
        let ok_expected = list(vec![Value::Integer(1)]);
        for (value, message) in [
            (
                s("nope"),
                "questions must be a list of maps, got String".to_string(),
            ),
            (
                list(vec![Value::Integer(3)]),
                "questions[0] must be a map, got Integer".to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    ("expected", ok_expected.clone()),
                    ("why", s("x")),
                ]),
                "questions[0]: unknown key `why`; expected query, expected, traceId".to_string(),
            ),
            (
                q(&[
                    ("query", Value::Integer(1)),
                    ("expected", ok_expected.clone()),
                ]),
                "questions[0].query must be a string, got Integer".to_string(),
            ),
            (
                q(&[("expected", ok_expected.clone())]),
                "questions[0] needs a query".to_string(),
            ),
            (
                q(&[("query", s("a"))]),
                "questions[0] needs expected or traceId".to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    ("expected", ok_expected.clone()),
                    ("traceId", s("t")),
                ]),
                "questions[0] takes expected or traceId, not both".to_string(),
            ),
            (
                q(&[("query", s("a")), ("traceId", Value::Integer(2))]),
                "questions[0].traceId must be a string, got Integer".to_string(),
            ),
            (
                q(&[("query", s("a")), ("expected", list(Vec::new()))]),
                "questions[0].expected must be a non-empty list of node ids or \
                 {name, label} maps, got an empty list"
                    .to_string(),
            ),
            (
                q(&[("query", s("a")), ("expected", s("x"))]),
                "questions[0].expected must be a non-empty list of node ids or \
                 {name, label} maps, got String"
                    .to_string(),
            ),
            (
                q(&[("query", s("a")), ("expected", list(vec![s("x")]))]),
                "questions[0].expected[0] must be a node id or a {name, label} map, got String"
                    .to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    ("expected", list(vec![Value::Integer(-1)])),
                ]),
                "questions[0].expected[0] must be a node id or a {name, label} map, got -1"
                    .to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    ("expected", list(vec![map(&[("label", s("Ticket"))])])),
                ]),
                "questions[0].expected[0] must be a node id or a {name, label} map with a \
                 string name"
                    .to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    (
                        "expected",
                        list(vec![map(&[("name", s("x")), ("label", Value::Integer(1))])]),
                    ),
                ]),
                "questions[0].expected[0].label must be a string, got Integer".to_string(),
            ),
            (
                q(&[
                    ("query", s("a")),
                    (
                        "expected",
                        list(vec![map(&[("name", s("x")), ("title", s("y"))])]),
                    ),
                ]),
                "questions[0].expected[0]: unknown key `title`; expected name, label".to_string(),
            ),
        ] {
            assert_eq!(parse_questions(&value), Err(message));
        }
    }

    // ── config ──────────────────────────────────────────────────────────

    #[test]
    fn strategies_are_split_from_the_retrieval_config() {
        let config = map(&[
            ("strategies", list(vec![s("vector"), s("rag"), s("vector")])),
            ("k", Value::Integer(3)),
            ("session", Value::Null),
        ]);
        let (rest, strategies) = split_config(&config).unwrap();
        assert_eq!(strategies, Some(vec![Strategy::Vector, Strategy::Rag]));
        assert_eq!(
            rest,
            map(&[("k", Value::Integer(3)), ("session", Value::Null)])
        );
        assert_eq!(split_config(&Value::Null), Ok((Value::Null, None)));
        let (rest, strategies) = split_config(&map(&[("strategies", Value::Null)])).unwrap();
        assert_eq!((rest, strategies), (map(&[]), None));
    }

    #[test]
    fn every_bad_config_is_named() {
        for (value, message) in [
            (s("x"), "the config must be a map, got String".to_string()),
            (
                map(&[("strategies", s("rag"))]),
                format!("strategies must be a non-empty list of {STRATEGY_NAMES}, got String"),
            ),
            (
                map(&[("strategies", list(Vec::new()))]),
                format!(
                    "strategies must be a non-empty list of {STRATEGY_NAMES}, got an empty list"
                ),
            ),
            (
                map(&[("strategies", list(vec![s("graph")]))]),
                format!("unknown strategy 'graph'; expected {STRATEGY_NAMES}"),
            ),
            (
                map(&[("strategies", list(vec![Value::Integer(1)]))]),
                format!("unknown strategy Integer; expected {STRATEGY_NAMES}"),
            ),
            (
                map(&[("session", s("chat-1"))]),
                "an evaluation never records a context trace; remove `session`".to_string(),
            ),
            (
                map(&[("trace", Value::Bool(true))]),
                "an evaluation never records a context trace; remove `trace`".to_string(),
            ),
        ] {
            assert_eq!(split_config(&value), Err(message));
        }
        // `trace: false` asks for nothing.
        assert!(split_config(&map(&[("trace", Value::Bool(false))])).is_ok());
    }
}
