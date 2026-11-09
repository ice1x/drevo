//! The arguments of `drevo.graph.expand`: its seeds, its config map, and the
//! server's ceilings on how far and how wide one call may walk.
//!
//! [`expand_options`] turns the config map into the
//! [`ExpandOptions`] of
//! [`expand_bfs`](crate::traversal::expand_bfs), refusing an unknown key or a
//! value out of range by name:
//!
//! ```
//! use std::collections::BTreeMap;
//! use drevo::cypher::executor::Value;
//! use drevo::cypher::expand::{expand_options, ExpandCeilings};
//! use drevo::model::Direction;
//!
//! let config = Value::Map(BTreeMap::from([
//!     ("hops".to_string(), Value::Integer(2)),
//!     ("direction".to_string(), Value::String("outgoing".to_string())),
//! ]));
//! let options = expand_options(&config, ExpandCeilings::default()).unwrap();
//! assert_eq!((options.hops, options.direction), (2, Direction::Outgoing));
//!
//! let too_far = Value::Map(BTreeMap::from([("hops".to_string(), Value::Integer(7))]));
//! let refused = expand_options(&too_far, ExpandCeilings::default()).unwrap_err();
//! assert!(refused.contains("DREVO_EXPAND_MAX_HOPS"));
//! ```
//!
//! Nothing here touches the graph.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::cypher::executor::Value;
use crate::model::Direction;
use crate::traversal::ExpandOptions;

/// The most hops one call may ask for unless `DREVO_EXPAND_MAX_HOPS` says
/// otherwise.
pub(crate) const DEFAULT_EXPAND_MAX_HOPS: usize = 6;

/// The most nodes one call may ask for unless `DREVO_EXPAND_MAX_NODES` says
/// otherwise.
pub(crate) const DEFAULT_EXPAND_MAX_NODES: usize = 5000;

/// The keys a `drevo.graph.expand` config map accepts, as listed in errors.
pub const CONFIG_KEYS: &str =
    "hops, direction, relTypes, labels, excludeLabels, perNodeLimit, maxNodes, asOf, includeSeeds";

/// The server's limits on one `drevo.graph.expand` call: `hops` and
/// `maxNodes` above them are refused.
///
/// ```
/// use drevo::cypher::expand::ExpandCeilings;
///
/// let c = ExpandCeilings::default();
/// assert_eq!((c.max_hops, c.max_nodes), (6, 5000));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpandCeilings {
    /// The most `hops` allowed (`DREVO_EXPAND_MAX_HOPS`).
    pub max_hops: usize,
    /// The largest `maxNodes` allowed (`DREVO_EXPAND_MAX_NODES`).
    pub max_nodes: usize,
}

impl Default for ExpandCeilings {
    fn default() -> Self {
        Self {
            max_hops: DEFAULT_EXPAND_MAX_HOPS,
            max_nodes: DEFAULT_EXPAND_MAX_NODES,
        }
    }
}

/// The ceilings `drevo.graph.expand` calls are held to in this process: the
/// defaults until [`set_ceilings`] is called (the server does at start, from
/// `DREVO_EXPAND_MAX_HOPS` / `DREVO_EXPAND_MAX_NODES`).
#[must_use]
pub fn ceilings() -> ExpandCeilings {
    ExpandCeilings {
        max_hops: MAX_HOPS.load(Ordering::Relaxed),
        max_nodes: MAX_NODES.load(Ordering::Relaxed),
    }
}

/// Hold `drevo.graph.expand` calls in this process to `limits` from now on.
pub fn set_ceilings(limits: ExpandCeilings) {
    MAX_HOPS.store(limits.max_hops, Ordering::Relaxed);
    MAX_NODES.store(limits.max_nodes, Ordering::Relaxed);
}

static MAX_HOPS: AtomicUsize = AtomicUsize::new(DEFAULT_EXPAND_MAX_HOPS);
static MAX_NODES: AtomicUsize = AtomicUsize::new(DEFAULT_EXPAND_MAX_NODES);

/// The seeds argument as node ids, in the order given: a node, a node id, a
/// list mixing both (`null` entries are skipped), or `null` for none.
///
/// # Errors
///
/// A message when the value is something else or an id is negative.
pub fn seed_ids(value: &Value) -> Result<Vec<u64>, String> {
    let one = |item: &Value, in_list: bool| -> Result<Option<u64>, String> {
        match item {
            Value::Node(n) => Ok(Some(n.id)),
            Value::Integer(id) => u64::try_from(*id)
                .map(Some)
                .map_err(|_| format!("seeds: {id} is not a node id")),
            Value::Null => Ok(None),
            other => Err(format!(
                "seeds must be a node, a node id or a list of them, got {}{}",
                if in_list { "a list holding " } else { "" },
                other.type_name()
            )),
        }
    };
    match value {
        Value::List(items) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                ids.extend(one(item, true)?);
            }
            Ok(ids)
        }
        other => Ok(one(other, false)?.into_iter().collect()),
    }
}

/// The config map of `drevo.graph.expand` as walk options; `null` (or a
/// `null` value for a key) keeps the defaults of
/// [`ExpandOptions::default`], except that the default `maxNodes` is lowered
/// to `ceilings.max_nodes` when that is smaller.
///
/// # Errors
///
/// A message naming the offending key when the value is not a map, a key is
/// unknown (listing [`CONFIG_KEYS`]), a value has the wrong type or range,
/// or `hops` / `maxNodes` exceed `ceilings`.
pub fn expand_options(value: &Value, ceilings: ExpandCeilings) -> Result<ExpandOptions, String> {
    // The default node cap, held to a server ceiling below it.
    let mut options = ExpandOptions::default();
    options.max_nodes = options.max_nodes.min(ceilings.max_nodes);
    let map = match value {
        Value::Null => return Ok(options),
        Value::Map(map) => map,
        other => {
            return Err(format!(
                "the config must be a map, got {}",
                other.type_name()
            ))
        }
    };
    for (key, v) in map {
        if matches!(v, Value::Null) {
            continue;
        }
        match key.as_str() {
            "hops" => {
                let hops = whole(v, 0)
                    .ok_or_else(|| format!("hops must be an integer ≥ 0, got {}", describe(v)))?;
                if hops > ceilings.max_hops {
                    return Err(format!(
                        "hops {hops} is above the server's limit of {} (DREVO_EXPAND_MAX_HOPS)",
                        ceilings.max_hops
                    ));
                }
                options.hops = hops;
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
            "relTypes" => options.rel_types = Some(strings("relTypes", v)?),
            "labels" => options.labels = Some(strings("labels", v)?),
            "excludeLabels" => options.exclude_labels = strings("excludeLabels", v)?,
            "perNodeLimit" => {
                options.per_node_limit = whole(v, 1).ok_or_else(|| {
                    format!("perNodeLimit must be an integer ≥ 1, got {}", describe(v))
                })?;
            }
            "maxNodes" => {
                let max = whole(v, 1).ok_or_else(|| {
                    format!("maxNodes must be an integer ≥ 1, got {}", describe(v))
                })?;
                if max > ceilings.max_nodes {
                    return Err(format!(
                        "maxNodes {max} is above the server's limit of {} (DREVO_EXPAND_MAX_NODES)",
                        ceilings.max_nodes
                    ));
                }
                options.max_nodes = max;
            }
            "asOf" => match v {
                Value::String(t) => {
                    options.as_of = Some(crate::cypher::timestamp::normalise_as_of(t)?);
                }
                other => {
                    return Err(format!(
                        "asOf must be an RFC 3339 timestamp string such as \
                         '2024-03-01T00:00:00Z', got {}",
                        describe(other)
                    ))
                }
            },
            "includeSeeds" => match v {
                Value::Bool(b) => options.include_seeds = *b,
                other => {
                    return Err(format!(
                        "includeSeeds must be a boolean, got {}",
                        describe(other)
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
    Ok(options)
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
    use std::collections::BTreeMap;
    use std::sync::Arc;

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

    fn s(text: &str) -> Value {
        Value::String(text.to_string())
    }

    fn strings(items: &[&str]) -> Value {
        Value::List(items.iter().map(|i| s(i)).collect())
    }

    fn parse(entries: &[(&str, Value)]) -> Result<ExpandOptions, String> {
        expand_options(&map(entries), ExpandCeilings::default())
    }

    fn refused(entries: &[(&str, Value)]) -> String {
        parse(entries).expect_err("expected the config to be refused")
    }

    fn node(id: u64) -> Value {
        Value::Node(Arc::new(NodeValue {
            id,
            uuid: [0; 16],
            labels: vec!["N".to_string()],
            properties: BTreeMap::new(),
        }))
    }

    #[test]
    fn null_and_empty_configs_are_the_defaults() {
        let defaults = ExpandOptions::default();
        assert_eq!(
            expand_options(&Value::Null, ExpandCeilings::default()),
            Ok(defaults.clone())
        );
        assert_eq!(parse(&[]), Ok(defaults.clone()));
        assert_eq!(parse(&[("hops", Value::Null)]), Ok(defaults));
    }

    #[test]
    fn every_key_is_read() {
        let options = parse(&[
            ("hops", Value::Integer(3)),
            ("direction", s("INCOMING")),
            ("relTypes", strings(&["KNOWS", "OWNS"])),
            ("labels", strings(&["Doc"])),
            ("excludeLabels", strings(&["Message", "ReasoningTrace"])),
            ("perNodeLimit", Value::Integer(4)),
            ("maxNodes", Value::Integer(50)),
            ("asOf", s("2024-03-01T00:00:00Z")),
            ("includeSeeds", Value::Bool(false)),
        ])
        .unwrap();
        assert_eq!(
            options,
            ExpandOptions {
                hops: 3,
                direction: Direction::Incoming,
                rel_types: Some(vec!["KNOWS".to_string(), "OWNS".to_string()]),
                labels: Some(vec!["Doc".to_string()]),
                exclude_labels: vec!["Message".to_string(), "ReasoningTrace".to_string()],
                per_node_limit: 4,
                max_nodes: 50,
                as_of: Some("2024-03-01T00:00:00.000Z".to_string()),
                include_seeds: false,
            }
        );
        assert_eq!(
            parse(&[("direction", s("outgoing"))]).unwrap().direction,
            Direction::Outgoing
        );
        assert_eq!(
            parse(&[("direction", s("both"))]).unwrap().direction,
            Direction::Both
        );
        assert_eq!(parse(&[("hops", Value::Integer(0))]).unwrap().hops, 0);
    }

    #[test]
    fn the_ceilings_are_inclusive() {
        let options = parse(&[
            ("hops", Value::Integer(6)),
            ("maxNodes", Value::Integer(5000)),
        ])
        .unwrap();
        assert_eq!((options.hops, options.max_nodes), (6, 5000));
    }

    #[test]
    fn the_default_max_nodes_is_held_to_a_lower_ceiling() {
        let low = ExpandCeilings {
            max_hops: 6,
            max_nodes: 50,
        };
        assert_eq!(
            expand_options(&Value::Null, low).map(|o| o.max_nodes),
            Ok(50)
        );
        assert_eq!(expand_options(&map(&[]), low).map(|o| o.max_nodes), Ok(50));
    }

    #[test]
    fn hops_above_the_ceiling_are_refused() {
        assert_eq!(
            refused(&[("hops", Value::Integer(7))]),
            "hops 7 is above the server's limit of 6 (DREVO_EXPAND_MAX_HOPS)"
        );
        let low = ExpandCeilings {
            max_hops: 2,
            max_nodes: 10,
        };
        assert_eq!(
            expand_options(&map(&[("hops", Value::Integer(3))]), low),
            Err("hops 3 is above the server's limit of 2 (DREVO_EXPAND_MAX_HOPS)".to_string())
        );
    }

    #[test]
    fn max_nodes_above_the_ceiling_is_refused() {
        assert_eq!(
            refused(&[("maxNodes", Value::Integer(5001))]),
            "maxNodes 5001 is above the server's limit of 5000 (DREVO_EXPAND_MAX_NODES)"
        );
    }

    #[test]
    fn negative_and_wrong_typed_numbers_are_refused() {
        assert_eq!(
            refused(&[("hops", Value::Integer(-1))]),
            "hops must be an integer ≥ 0, got -1"
        );
        assert_eq!(
            refused(&[("hops", Value::Float(1.5))]),
            "hops must be an integer ≥ 0, got 1.5"
        );
        assert_eq!(
            refused(&[("perNodeLimit", Value::Integer(0))]),
            "perNodeLimit must be an integer ≥ 1, got 0"
        );
        assert_eq!(
            refused(&[("perNodeLimit", Value::Integer(-5))]),
            "perNodeLimit must be an integer ≥ 1, got -5"
        );
        assert_eq!(
            refused(&[("maxNodes", Value::Integer(-1))]),
            "maxNodes must be an integer ≥ 1, got -1"
        );
        assert_eq!(
            refused(&[("maxNodes", s("many"))]),
            "maxNodes must be an integer ≥ 1, got 'many'"
        );
    }

    #[test]
    fn unknown_keys_are_refused_by_name() {
        assert_eq!(
            refused(&[("depth", Value::Integer(2))]),
            "unknown config key `depth`; expected hops, direction, relTypes, labels, \
             excludeLabels, perNodeLimit, maxNodes, asOf, includeSeeds"
        );
    }

    #[test]
    fn a_config_that_is_not_a_map_is_refused() {
        assert_eq!(
            expand_options(&Value::Integer(2), ExpandCeilings::default()),
            Err("the config must be a map, got Integer".to_string())
        );
    }

    #[test]
    fn wrong_typed_values_are_refused_by_key() {
        assert_eq!(
            refused(&[("direction", s("sideways"))]),
            "direction must be 'outgoing', 'incoming' or 'both', got 'sideways'"
        );
        assert_eq!(
            refused(&[("direction", Value::Integer(1))]),
            "direction must be 'outgoing', 'incoming' or 'both', got 1"
        );
        assert_eq!(
            refused(&[("relTypes", s("KNOWS"))]),
            "relTypes must be a list of strings, got 'KNOWS'"
        );
        assert_eq!(
            refused(&[("labels", Value::List(vec![Value::Integer(1)]))]),
            "labels must be a list of strings, got a list holding 1"
        );
        assert_eq!(
            refused(&[("excludeLabels", Value::Bool(true))]),
            "excludeLabels must be a list of strings, got Boolean"
        );
        assert_eq!(
            refused(&[("asOf", Value::Integer(2024))]),
            "asOf must be an RFC 3339 timestamp string such as '2024-03-01T00:00:00Z', got 2024"
        );
        let message = refused(&[("asOf", s("yesterday"))]);
        assert!(
            message.contains("asOf") && message.contains("'yesterday'"),
            "{message}"
        );
        assert_eq!(
            refused(&[("includeSeeds", s("yes"))]),
            "includeSeeds must be a boolean, got 'yes'"
        );
    }

    #[test]
    fn seeds_accept_a_node_an_id_or_a_list_of_both() {
        assert_eq!(seed_ids(&node(4)), Ok(vec![4]));
        assert_eq!(seed_ids(&Value::Integer(7)), Ok(vec![7]));
        assert_eq!(
            seed_ids(&Value::List(vec![
                node(3),
                Value::Integer(1),
                Value::Null,
                node(3)
            ])),
            Ok(vec![3, 1, 3])
        );
        assert_eq!(seed_ids(&Value::Null), Ok(vec![]));
        assert_eq!(seed_ids(&Value::List(vec![])), Ok(vec![]));
    }

    #[test]
    fn seeds_of_another_kind_are_refused() {
        assert_eq!(
            seed_ids(&Value::Integer(-3)),
            Err("seeds: -3 is not a node id".to_string())
        );
        assert_eq!(
            seed_ids(&s("alice")),
            Err("seeds must be a node, a node id or a list of them, got String".to_string())
        );
        assert_eq!(
            seed_ids(&Value::List(vec![node(1), s("bob")])),
            Err(
                "seeds must be a node, a node id or a list of them, got a list holding String"
                    .to_string()
            )
        );
    }
}
