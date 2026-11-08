//! The config map of `drevo.pagerank` and `drevo.pagerank.personalized`, and
//! the `sourceNodes` argument of the latter.

use crate::algorithms::{PageRankConfig, Personalization};
use crate::cypher::executor::Value;
use crate::model::Direction;

/// The keys a `drevo.pagerank.personalized` config map accepts, as listed in
/// errors.
pub const PERSONALIZED_KEYS: &str =
    "damping, maxIterations, tolerance, direction, relTypes, sourceWeights, limit, minScore";

/// The keys a `drevo.pagerank` config map accepts, as listed in errors.
pub const GLOBAL_KEYS: &str =
    "damping, maxIterations, tolerance, direction, relTypes, limit, minScore";

/// Which procedure a config map belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRankProcedure {
    /// `drevo.pagerank`: teleports to every node; `maxIterations` defaults to
    /// `100`.
    Global,
    /// `drevo.pagerank.personalized`: teleports to the source nodes;
    /// `maxIterations` defaults to `20`.
    Personalized,
}

/// The parsed config map of `drevo.pagerank` / `drevo.pagerank.personalized`.
///
/// ```
/// use std::collections::BTreeMap;
/// use drevo::cypher::executor::Value;
/// use drevo::cypher::pagerank_config::{PageRankOptions, PageRankProcedure};
///
/// let map = BTreeMap::from([
///     ("damping".to_string(), Value::Float(0.5)),
///     ("limit".to_string(), Value::Integer(10)),
/// ]);
/// let options =
///     PageRankOptions::from_value(&Value::Map(map), PageRankProcedure::Personalized).unwrap();
/// assert_eq!(options.damping, 0.5);
/// assert_eq!(options.max_iterations, 20); // the personalised default
/// assert_eq!(options.limit, Some(10));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PageRankOptions {
    /// Damping factor, in the open interval `(0, 1)`; default `0.85`.
    pub damping: f64,
    /// Iteration cap, at least `1`.
    pub max_iterations: usize,
    /// L1 convergence tolerance, finite and `≥ 0`; default `1e-6`.
    pub tolerance: f64,
    /// Which way edges are walked; default outgoing.
    pub direction: Direction,
    /// The relationship types walked; empty walks every type.
    pub rel_types: Vec<String>,
    /// One weight per source node (personalised only); `None` weights them
    /// equally.
    pub source_weights: Option<Vec<f64>>,
    /// The most rows returned; `None` returns every row.
    pub limit: Option<usize>,
    /// Rows scoring below this are dropped; default `0`. (Personalized
    /// PageRank never returns the nodes its sources cannot reach, whatever
    /// this is.)
    pub min_score: f64,
}

impl PageRankOptions {
    /// The options of a call without a config map.
    ///
    /// ```
    /// use drevo::algorithms::PageRankConfig;
    /// use drevo::cypher::pagerank_config::{PageRankOptions, PageRankProcedure};
    ///
    /// let global = PageRankOptions::defaults(PageRankProcedure::Global);
    /// assert_eq!(global.algorithm_config(None), PageRankConfig::default());
    /// ```
    #[must_use]
    pub fn defaults(procedure: PageRankProcedure) -> Self {
        let base = PageRankConfig::default();
        Self {
            damping: base.damping,
            max_iterations: match procedure {
                PageRankProcedure::Global => base.max_iterations,
                PageRankProcedure::Personalized => 20,
            },
            tolerance: base.tolerance,
            direction: Direction::Outgoing,
            rel_types: Vec::new(),
            source_weights: None,
            limit: None,
            min_score: 0.0,
        }
    }

    /// Parse the optional config argument: `null` gives the defaults, a map
    /// sets the keys it names (a `null` value keeps that key's default).
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing the accepted keys) or a value has the wrong type
    /// or range.
    pub fn from_value(value: &Value, procedure: PageRankProcedure) -> Result<Self, String> {
        let map = match value {
            Value::Null => return Ok(Self::defaults(procedure)),
            Value::Map(map) => map,
            other => {
                return Err(format!(
                    "the config must be a map, got {}",
                    other.type_name()
                ))
            }
        };
        let mut options = Self::defaults(procedure);
        for (key, v) in map {
            if matches!(v, Value::Null) {
                continue;
            }
            match key.as_str() {
                "damping" => {
                    options.damping =
                        number(v).filter(|d| *d > 0.0 && *d < 1.0).ok_or_else(|| {
                            format!(
                                "damping must be a number in the open interval (0, 1), got {}",
                                describe(v)
                            )
                        })?;
                }
                "maxIterations" => {
                    options.max_iterations = match v {
                        Value::Integer(n) if *n >= 1 => usize::try_from(*n).unwrap_or(usize::MAX),
                        _ => {
                            return Err(format!(
                                "maxIterations must be an integer ≥ 1, got {}",
                                describe(v)
                            ))
                        }
                    };
                }
                "tolerance" => {
                    options.tolerance = number(v).filter(|t| *t >= 0.0).ok_or_else(|| {
                        format!("tolerance must be a finite number ≥ 0, got {}", describe(v))
                    })?;
                }
                "direction" => {
                    options.direction = match v {
                        Value::String(d) => match d.to_ascii_lowercase().as_str() {
                            "outgoing" => Some(Direction::Outgoing),
                            "incoming" => Some(Direction::Incoming),
                            "both" => Some(Direction::Both),
                            _ => None,
                        },
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "direction must be 'outgoing', 'incoming' or 'both', got {}",
                            describe(v)
                        )
                    })?;
                }
                "relTypes" => options.rel_types = rel_types(v)?,
                "sourceWeights" if procedure == PageRankProcedure::Global => {
                    return Err(
                        "sourceWeights applies only to drevo.pagerank.personalized".to_string()
                    )
                }
                "sourceWeights" => options.source_weights = Some(source_weights(v)?),
                "limit" => {
                    options.limit = match v {
                        Value::Integer(n) if *n >= 0 => {
                            Some(usize::try_from(*n).unwrap_or(usize::MAX))
                        }
                        _ => {
                            return Err(format!(
                                "limit must be an integer ≥ 0, got {}",
                                describe(v)
                            ))
                        }
                    };
                }
                "minScore" => {
                    options.min_score = number(v).ok_or_else(|| {
                        format!("minScore must be a finite number, got {}", describe(v))
                    })?;
                }
                other => {
                    let keys = match procedure {
                        PageRankProcedure::Global => GLOBAL_KEYS,
                        PageRankProcedure::Personalized => PERSONALIZED_KEYS,
                    };
                    return Err(format!("unknown config key `{other}`; expected {keys}"));
                }
            }
        }
        Ok(options)
    }

    /// The algorithm config these options run with.
    #[must_use]
    pub fn algorithm_config(&self, personalization: Option<Personalization>) -> PageRankConfig {
        PageRankConfig {
            damping: self.damping,
            max_iterations: self.max_iterations,
            tolerance: self.tolerance,
            personalization,
        }
    }

    /// The teleport set for `sources` (as returned by [`source_ids`]),
    /// weighted by `sourceWeights` or equally.
    ///
    /// # Errors
    ///
    /// When `sourceWeights` does not have one entry per source, or every
    /// weight is zero.
    pub fn personalization(&self, sources: Vec<u64>) -> Result<Personalization, String> {
        let weights = match &self.source_weights {
            Some(weights) if weights.len() != sources.len() => {
                return Err(format!(
                    "sourceWeights has {} entries but sourceNodes has {}",
                    weights.len(),
                    sources.len()
                ))
            }
            Some(weights) if weights.iter().all(|&w| w == 0.0) => {
                return Err("sourceWeights must not all be zero".to_string())
            }
            Some(weights) => weights.clone(),
            None => vec![1.0; sources.len()],
        };
        Personalization::new(sources, weights).map_err(|e| e.to_string())
    }

    /// The rows kept from a best-first ranking: those scoring at least
    /// `minScore`, then the first `limit` of them.
    #[must_use]
    pub fn select(&self, ranked: Vec<(u64, f64)>) -> Vec<(u64, f64)> {
        ranked
            .into_iter()
            .filter(|&(_, score)| score >= self.min_score)
            .take(self.limit.unwrap_or(usize::MAX))
            .collect()
    }
}

/// The node ids named by a `sourceNodes` argument: a node, a node id, or a
/// list mixing both. Ids are returned in order, repeats kept; whether each
/// id is a node in the graph is for the caller to check.
///
/// ```
/// use drevo::cypher::executor::Value;
/// use drevo::cypher::pagerank_config::source_ids;
///
/// let ids = source_ids(&Value::List(vec![Value::Integer(3), Value::Integer(8)]));
/// assert_eq!(ids, Ok(vec![3, 8]));
/// ```
///
/// # Errors
///
/// When the list is empty, an id is negative, or a value is neither a node
/// nor an integer.
pub fn source_ids(value: &Value) -> Result<Vec<u64>, String> {
    let items = match value {
        Value::List(items) => items.as_slice(),
        single => std::slice::from_ref(single),
    };
    if items.is_empty() {
        return Err("sourceNodes must name at least one node".to_string());
    }
    items
        .iter()
        .map(|item| match item {
            Value::Node(node) => Ok(node.id),
            Value::Integer(id) => {
                u64::try_from(*id).map_err(|_| format!("sourceNodes: {id} is not a node id"))
            }
            other => Err(format!(
                "sourceNodes must be a node, a node id or a list of them, got {}",
                other.type_name()
            )),
        })
        .collect()
}

/// `value` as a finite number.
fn number(value: &Value) -> Option<f64> {
    let n = match value {
        Value::Integer(i) => *i as f64,
        Value::Float(f) => *f,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// A value as quoted in an error: strings in quotes, integers as written,
/// floats with their decimal point, anything else by its type.
fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        other => other.type_name().to_string(),
    }
}

/// The `relTypes` list: relationship type names.
fn rel_types(value: &Value) -> Result<Vec<String>, String> {
    let Value::List(items) = value else {
        return Err(format!(
            "relTypes must be a list of relationship types, got {}",
            value.type_name()
        ));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(name) => Ok(name.clone()),
            other => Err(format!(
                "relTypes must list relationship types as strings, got {}",
                other.type_name()
            )),
        })
        .collect()
}

/// The `sourceWeights` list: finite numbers `≥ 0`.
fn source_weights(value: &Value) -> Result<Vec<f64>, String> {
    let Value::List(items) = value else {
        return Err(format!(
            "sourceWeights must be a list of numbers, got {}",
            value.type_name()
        ));
    };
    items
        .iter()
        .map(|item| {
            number(item).filter(|w| *w >= 0.0).ok_or_else(|| {
                format!(
                    "sourceWeights must be finite numbers ≥ 0, got {}",
                    describe(item)
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::cypher::executor::NodeValue;

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn parse(entries: &[(&str, Value)]) -> Result<PageRankOptions, String> {
        PageRankOptions::from_value(&map(entries), PageRankProcedure::Personalized)
    }

    fn node(id: u64) -> Value {
        Value::Node(std::sync::Arc::new(NodeValue {
            id,
            uuid: [0; 16],
            labels: vec!["N".into()],
            properties: BTreeMap::new(),
        }))
    }

    #[test]
    fn defaults_match_the_documented_table() {
        let ppr = PageRankOptions::defaults(PageRankProcedure::Personalized);
        assert_eq!(ppr.damping, 0.85);
        assert_eq!(ppr.max_iterations, 20);
        assert_eq!(ppr.tolerance, 1e-6);
        assert_eq!(ppr.direction, Direction::Outgoing);
        assert!(ppr.rel_types.is_empty());
        assert_eq!(ppr.source_weights, None);
        assert_eq!(ppr.limit, None);
        assert_eq!(ppr.min_score, 0.0);
        assert_eq!(
            PageRankOptions::from_value(&Value::Null, PageRankProcedure::Personalized),
            Ok(ppr)
        );
    }

    #[test]
    fn global_defaults_are_those_of_page_rank_config_default() {
        let global = PageRankOptions::defaults(PageRankProcedure::Global);
        assert_eq!(global.algorithm_config(None), PageRankConfig::default());
        assert_eq!(
            PageRankOptions::from_value(&map(&[]), PageRankProcedure::Global),
            Ok(global)
        );
    }

    #[test]
    fn every_key_is_read() {
        let o = parse(&[
            ("damping", Value::Float(0.5)),
            ("maxIterations", Value::Integer(7)),
            ("tolerance", Value::Integer(0)),
            ("direction", Value::String("Incoming".into())),
            (
                "relTypes",
                Value::List(vec![Value::String("A".into()), Value::String("B".into())]),
            ),
            (
                "sourceWeights",
                Value::List(vec![Value::Integer(2), Value::Float(0.5)]),
            ),
            ("limit", Value::Integer(3)),
            ("minScore", Value::Float(0.01)),
        ])
        .unwrap();
        assert_eq!(o.damping, 0.5);
        assert_eq!(o.max_iterations, 7);
        assert_eq!(o.tolerance, 0.0);
        assert_eq!(o.direction, Direction::Incoming);
        assert_eq!(o.rel_types, ["A", "B"]);
        assert_eq!(o.source_weights, Some(vec![2.0, 0.5]));
        assert_eq!(o.limit, Some(3));
        assert_eq!(o.min_score, 0.01);
        let cfg = o.algorithm_config(None);
        assert_eq!(
            (cfg.damping, cfg.max_iterations, cfg.tolerance),
            (0.5, 7, 0.0)
        );
    }

    #[test]
    fn null_values_keep_defaults() {
        assert_eq!(
            parse(&[("damping", Value::Null), ("limit", Value::Null)]),
            Ok(PageRankOptions::defaults(PageRankProcedure::Personalized))
        );
    }

    #[test]
    fn errors_name_the_key() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("damping", Value::Float(0.0), "damping must be a number in the open interval (0, 1), got 0.0"),
            ("damping", Value::String("x".into()), "damping must be a number in the open interval (0, 1), got 'x'"),
            ("maxIterations", Value::Integer(0), "maxIterations must be an integer ≥ 1, got 0"),
            ("maxIterations", Value::Float(2.0), "maxIterations must be an integer ≥ 1, got 2.0"),
            ("tolerance", Value::Float(f64::NAN), "tolerance must be a finite number ≥ 0, got NaN"),
            ("direction", Value::String("up".into()), "direction must be 'outgoing', 'incoming' or 'both', got 'up'"),
            ("direction", Value::Integer(1), "direction must be 'outgoing', 'incoming' or 'both', got 1"),
            ("relTypes", Value::String("A".into()), "relTypes must be a list of relationship types, got String"),
            ("relTypes", Value::List(vec![Value::Integer(1)]), "relTypes must list relationship types as strings, got Integer"),
            ("sourceWeights", Value::Float(1.0), "sourceWeights must be a list of numbers, got Float"),
            ("sourceWeights", Value::List(vec![Value::Float(-0.5)]), "sourceWeights must be finite numbers ≥ 0, got -0.5"),
            ("sourceWeights", Value::List(vec![Value::String("a".into())]), "sourceWeights must be finite numbers ≥ 0, got 'a'"),
            ("limit", Value::Integer(-1), "limit must be an integer ≥ 0, got -1"),
            ("minScore", Value::Float(f64::INFINITY), "minScore must be a finite number, got inf"),
            ("top", Value::Integer(3), "unknown config key `top`; expected damping, maxIterations, tolerance, direction, relTypes, sourceWeights, limit, minScore"),
        ];
        for (key, value, message) in cases {
            assert_eq!(
                parse(&[(key, value.clone())]),
                Err(message.to_string()),
                "{key}: {value:?}"
            );
        }
        assert_eq!(
            PageRankOptions::from_value(&Value::Integer(1), PageRankProcedure::Personalized),
            Err("the config must be a map, got Integer".to_string())
        );
    }

    #[test]
    fn the_global_procedure_refuses_source_weights() {
        let weights = map(&[("sourceWeights", Value::List(vec![Value::Integer(1)]))]);
        assert_eq!(
            PageRankOptions::from_value(&weights, PageRankProcedure::Global),
            Err("sourceWeights applies only to drevo.pagerank.personalized".to_string())
        );
        assert_eq!(
            PageRankOptions::from_value(&map(&[("x", Value::Null)]), PageRankProcedure::Global),
            Ok(PageRankOptions::defaults(PageRankProcedure::Global)),
            "a null value is ignored even under an unknown key, as in drevo.search.hybrid"
        );
        assert_eq!(
            PageRankOptions::from_value(&map(&[("x", Value::Integer(1))]), PageRankProcedure::Global),
            Err("unknown config key `x`; expected damping, maxIterations, tolerance, direction, relTypes, limit, minScore".to_string())
        );
    }

    #[test]
    fn source_ids_accept_nodes_ids_and_lists() {
        assert_eq!(source_ids(&node(4)), Ok(vec![4]));
        assert_eq!(source_ids(&Value::Integer(9)), Ok(vec![9]));
        assert_eq!(
            source_ids(&Value::List(vec![node(4), Value::Integer(9), node(4)])),
            Ok(vec![4, 9, 4])
        );
    }

    #[test]
    fn source_ids_refuse_empty_negative_and_other_values() {
        assert_eq!(
            source_ids(&Value::List(vec![])),
            Err("sourceNodes must name at least one node".to_string())
        );
        assert_eq!(
            source_ids(&Value::Null),
            Err("sourceNodes must be a node, a node id or a list of them, got Null".to_string())
        );
        assert_eq!(
            source_ids(&Value::Integer(-3)),
            Err("sourceNodes: -3 is not a node id".to_string())
        );
        assert_eq!(
            source_ids(&Value::List(vec![node(1), Value::List(vec![])])),
            Err("sourceNodes must be a node, a node id or a list of them, got List".to_string())
        );
        assert_eq!(
            source_ids(&Value::List(vec![Value::Null])),
            Err("sourceNodes must be a node, a node id or a list of them, got Null".to_string())
        );
    }

    #[test]
    fn personalization_checks_the_weights_against_the_sources() {
        let defaults = PageRankOptions::defaults(PageRankProcedure::Personalized);
        let p = defaults.personalization(vec![3, 5]).unwrap();
        assert_eq!((p.nodes(), p.weights()), (&[3, 5][..], &[1.0, 1.0][..]));

        let weighted = PageRankOptions {
            source_weights: Some(vec![1.0]),
            ..defaults.clone()
        };
        assert_eq!(
            weighted.personalization(vec![3, 5]),
            Err("sourceWeights has 1 entries but sourceNodes has 2".to_string())
        );
        let zero = PageRankOptions {
            source_weights: Some(vec![0.0, 0.0]),
            ..defaults.clone()
        };
        assert_eq!(
            zero.personalization(vec![3, 5]),
            Err("sourceWeights must not all be zero".to_string())
        );
        let ok = PageRankOptions {
            source_weights: Some(vec![2.0, 0.0]),
            ..defaults
        };
        assert_eq!(
            ok.personalization(vec![3, 5]).unwrap().weights(),
            &[2.0, 0.0]
        );
    }

    #[test]
    fn select_applies_min_score_then_limit_in_rank_order() {
        let ranked = vec![(1, 0.5), (2, 0.3), (3, 0.2), (4, 0.0)];
        let mut o = PageRankOptions::defaults(PageRankProcedure::Personalized);
        assert_eq!(
            o.select(ranked.clone()),
            ranked,
            "minScore 0 keeps zero rows"
        );
        o.min_score = 0.25;
        assert_eq!(o.select(ranked.clone()), vec![(1, 0.5), (2, 0.3)]);
        o.limit = Some(1);
        assert_eq!(o.select(ranked.clone()), vec![(1, 0.5)]);
        o.min_score = 0.0;
        o.limit = Some(0);
        assert!(o.select(ranked).is_empty());
    }
}
