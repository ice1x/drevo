//! The config maps of `drevo.entity.candidates` and `drevo.entity.merge`,
//! and the resolution keys `drevo.memory.rememberEntity` accepts in its
//! options map. A `null` map, or a `null` value for a key, keeps the
//! defaults; an unknown key is refused with the accepted keys listed.

use std::collections::BTreeMap;

use super::blocking::Blocking;
use super::merge::Strategy;
use super::score::Weights;
use crate::cypher::executor::Value;
use crate::cypher::provenance::describe;

/// The keys a `drevo.entity.candidates` config accepts, as listed in errors.
pub(crate) const CANDIDATES_KEYS: &str = "nameProperty, typeProperty, embeddingProperty, \
     threshold, nameWeight, vectorWeight, blocking, maxPairs, normalise";

/// The keys a `drevo.entity.merge` config accepts, as listed in errors.
pub(crate) const MERGE_KEYS: &str = "strategy, force, nameProperty, typeProperty";

/// The resolution keys of the `drevo.memory.rememberEntity` options map.
pub(crate) const RESOLVE_KEYS: &str = "resolve, threshold";

/// The score a pair needs by default.
pub(crate) const DEFAULT_THRESHOLD: f64 = 0.9;

/// The most pairs compared by default.
pub(crate) const DEFAULT_MAX_PAIRS: u64 = 100_000;

/// The parsed `drevo.entity.candidates` config.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CandidatesConfig {
    /// The property holding a node's name.
    pub(crate) name_property: String,
    /// The property holding a node's type; pairs of different types are
    /// never candidates.
    pub(crate) type_property: String,
    /// The property holding a node's embedding, if vectors are compared.
    pub(crate) embedding_property: Option<String>,
    /// The score a pair needs (inclusive).
    pub(crate) threshold: f64,
    /// How much the name and vector scores count.
    pub(crate) weights: Weights,
    /// Which pairs are compared.
    pub(crate) blocking: Blocking,
    /// The most pairs compared.
    pub(crate) max_pairs: u64,
    /// Whether names are compared in normalised form.
    pub(crate) normalise: bool,
}

impl Default for CandidatesConfig {
    fn default() -> Self {
        Self {
            name_property: "name".into(),
            type_property: "type".into(),
            embedding_property: None,
            threshold: DEFAULT_THRESHOLD,
            weights: Weights::default(),
            blocking: Blocking::Prefix3,
            max_pairs: DEFAULT_MAX_PAIRS,
            normalise: true,
        }
    }
}

impl CandidatesConfig {
    /// Parse a `drevo.entity.candidates` config.
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown, a value has the wrong type or range, or both weights are 0.
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut config = Self::default();
        for (key, v) in entries(value)? {
            match key {
                "nameProperty" => config.name_property = string(key, v)?,
                "typeProperty" => config.type_property = string(key, v)?,
                "embeddingProperty" => config.embedding_property = Some(string(key, v)?),
                "threshold" => config.threshold = unit(key, v)?,
                "nameWeight" => config.weights.name = weight(key, v)?,
                "vectorWeight" => config.weights.vector = weight(key, v)?,
                "blocking" => {
                    config.blocking = match v {
                        Value::String(s) if s == "prefix3" => Blocking::Prefix3,
                        Value::String(s) if s == "token" => Blocking::Token,
                        Value::String(s) if s == "none" => Blocking::None,
                        other => {
                            return Err(format!(
                                "blocking must be 'prefix3', 'token' or 'none', got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "maxPairs" => {
                    config.max_pairs = match v {
                        Value::Integer(n) if *n >= 1 => n.unsigned_abs(),
                        other => {
                            return Err(format!(
                                "maxPairs must be an integer ≥ 1, got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "normalise" => config.normalise = boolean(key, v)?,
                other => {
                    return Err(format!(
                        "unknown config key `{other}`; expected {CANDIDATES_KEYS}"
                    ))
                }
            }
        }
        if config.weights.name == 0.0 && config.weights.vector == 0.0 {
            return Err("nameWeight and vectorWeight cannot both be 0".into());
        }
        Ok(config)
    }
}

/// The parsed `drevo.entity.merge` config.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MergeConfig {
    /// How conflicting properties are combined.
    pub(crate) strategy: Strategy,
    /// Merge nodes of different types anyway.
    pub(crate) force: bool,
    /// The property holding a node's name (`name` when `None`).
    pub(crate) name_property: Option<String>,
    /// The property holding a node's type (`type` when `None`).
    pub(crate) type_property: Option<String>,
}

impl MergeConfig {
    /// The property holding a node's name.
    pub(crate) fn name_property(&self) -> &str {
        self.name_property.as_deref().unwrap_or("name")
    }

    /// The property holding a node's type.
    pub(crate) fn type_property(&self) -> &str {
        self.type_property.as_deref().unwrap_or("type")
    }

    /// Parse a `drevo.entity.merge` config.
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown or a value has the wrong type.
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut config = Self::default();
        for (key, v) in entries(value)? {
            match key {
                "strategy" => {
                    config.strategy = match v {
                        Value::String(s) if s == "keepFirst" => Strategy::KeepFirst,
                        Value::String(s) if s == "newest" => Strategy::Newest,
                        Value::String(s) if s == "collect" => Strategy::Collect,
                        other => {
                            return Err(format!(
                                "strategy must be 'keepFirst', 'newest' or 'collect', got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "force" => config.force = boolean(key, v)?,
                "nameProperty" => config.name_property = Some(string(key, v)?),
                "typeProperty" => config.type_property = Some(string(key, v)?),
                other => {
                    return Err(format!(
                        "unknown config key `{other}`; expected {MERGE_KEYS}"
                    ))
                }
            }
        }
        Ok(config)
    }
}

/// How `drevo.memory.rememberEntity` looks for an entity it already knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Resolve {
    /// The same name and type.
    #[default]
    Exact,
    /// As `Exact`, then an `:Alias` whose normalised name matches.
    Alias,
    /// As `Alias`, then the one entity of the type whose name scores at
    /// least the threshold.
    Fuzzy,
}

/// The resolution keys of a `drevo.memory.rememberEntity` options map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResolveOptions {
    /// How a known entity is found.
    pub(crate) resolve: Resolve,
    /// The name score a fuzzy match needs.
    pub(crate) threshold: f64,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            resolve: Resolve::Exact,
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

impl ResolveOptions {
    /// Take the resolution keys out of a `rememberEntity` options map,
    /// leaving the provenance keys in it.
    ///
    /// # Errors
    ///
    /// A message when `resolve` is not `'exact'`, `'alias'` or `'fuzzy'`,
    /// `threshold` is not a number in `[0, 1]`, or a threshold is given
    /// without `resolve: 'fuzzy'`.
    pub(crate) fn take_from(map: &mut BTreeMap<String, Value>) -> Result<Self, String> {
        let mut options = Self::default();
        if let Some(v) = map.remove("resolve") {
            options.resolve = match &v {
                Value::Null => Resolve::Exact,
                Value::String(s) if s == "exact" => Resolve::Exact,
                Value::String(s) if s == "alias" => Resolve::Alias,
                Value::String(s) if s == "fuzzy" => Resolve::Fuzzy,
                other => {
                    return Err(format!(
                        "resolve must be 'exact', 'alias' or 'fuzzy', got {}",
                        describe(other)
                    ))
                }
            };
        }
        match map.remove("threshold") {
            None | Some(Value::Null) => {}
            Some(v) => {
                if options.resolve != Resolve::Fuzzy {
                    return Err("threshold only applies with resolve: 'fuzzy'".into());
                }
                options.threshold = unit("threshold", &v)?;
            }
        }
        Ok(options)
    }
}

/// The non-null entries of a config map; `null` is an empty map.
fn entries(value: &Value) -> Result<Vec<(&str, &Value)>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Map(map) => Ok(map
            .iter()
            .filter(|(_, v)| !matches!(v, Value::Null))
            .map(|(k, v)| (k.as_str(), v))
            .collect()),
        other => Err(format!("the config must be a map, got {}", describe(other))),
    }
}

/// `value` as a number in `[0, 1]`, or an error naming `key`.
fn unit(key: &str, value: &Value) -> Result<f64, String> {
    match number(value) {
        Some(n) if (0.0..=1.0).contains(&n) => Ok(n),
        _ => Err(format!(
            "{key} must be a number between 0 and 1, got {}",
            describe(value)
        )),
    }
}

/// `value` as a number ≥ 0, or an error naming `key`.
fn weight(key: &str, value: &Value) -> Result<f64, String> {
    match number(value) {
        Some(n) if n >= 0.0 && n.is_finite() => Ok(n),
        _ => Err(format!(
            "{key} must be a number ≥ 0, got {}",
            describe(value)
        )),
    }
}

/// An integer or float as `f64`.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// `value` as a string, or an error naming `key`.
fn string(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Err(format!("{key} must be a string, got {}", describe(other))),
    }
}

/// `value` as a boolean, or an error naming `key`.
fn boolean(key: &str, value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(format!("{key} must be a boolean, got {}", describe(other))),
    }
}

/// The node id of `drevo.entity.merge`'s `keepId`: a node or a node id.
///
/// # Errors
///
/// A message when the value is something else or a negative id.
pub(crate) fn keep_id(value: &Value) -> Result<u64, String> {
    match value {
        Value::Node(n) => Ok(n.id),
        Value::Integer(id) => {
            u64::try_from(*id).map_err(|_| format!("keepId: {id} is not a node id"))
        }
        other => Err(format!(
            "keepId must be a node or a node id, got {}",
            other.type_name()
        )),
    }
}

/// The node ids of `drevo.entity.merge`'s `dropIds`: a node, a node id, a
/// list mixing both (`null` entries are skipped), or `null` for none.
///
/// # Errors
///
/// A message when the value or an entry is something else or a negative id.
pub(crate) fn drop_ids(value: &Value) -> Result<Vec<u64>, String> {
    let one = |item: &Value, in_list: bool| -> Result<Option<u64>, String> {
        match item {
            Value::Node(n) => Ok(Some(n.id)),
            Value::Integer(id) => u64::try_from(*id)
                .map(Some)
                .map_err(|_| format!("dropIds: {id} is not a node id")),
            Value::Null => Ok(None),
            other => Err(format!(
                "dropIds must be a node, a node id or a list of them, got {}{}",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn candidates_err(entries: &[(&str, Value)]) -> String {
        CandidatesConfig::from_value(&map(entries)).unwrap_err()
    }

    // ----- candidates ----------------------------------------------------------

    #[test]
    fn candidates_defaults() {
        let c = CandidatesConfig::from_value(&Value::Null).unwrap();
        assert_eq!(c, CandidatesConfig::default());
        assert_eq!(c.name_property, "name");
        assert_eq!(c.type_property, "type");
        assert_eq!(c.embedding_property, None);
        assert_eq!(c.threshold, 0.9);
        assert_eq!(
            c.weights,
            Weights {
                name: 0.5,
                vector: 0.5
            }
        );
        assert_eq!(c.blocking, Blocking::Prefix3);
        assert_eq!(c.max_pairs, 100_000);
        assert!(c.normalise);
        assert_eq!(
            CandidatesConfig::from_value(&map(&[("threshold", Value::Null)])).unwrap(),
            CandidatesConfig::default()
        );
    }

    #[test]
    fn candidates_keys_are_read() {
        let c = CandidatesConfig::from_value(&map(&[
            ("nameProperty", s("title")),
            ("typeProperty", s("kind")),
            ("embeddingProperty", s("embedding")),
            ("threshold", Value::Float(0.75)),
            ("nameWeight", Value::Integer(1)),
            ("vectorWeight", Value::Float(0.25)),
            ("blocking", s("token")),
            ("maxPairs", Value::Integer(10)),
            ("normalise", Value::Bool(false)),
        ]))
        .unwrap();
        assert_eq!(
            c,
            CandidatesConfig {
                name_property: "title".into(),
                type_property: "kind".into(),
                embedding_property: Some("embedding".into()),
                threshold: 0.75,
                weights: Weights {
                    name: 1.0,
                    vector: 0.25
                },
                blocking: Blocking::Token,
                max_pairs: 10,
                normalise: false,
            }
        );
        let none = CandidatesConfig::from_value(&map(&[("blocking", s("none"))])).unwrap();
        assert_eq!(none.blocking, Blocking::None);
        let one = CandidatesConfig::from_value(&map(&[("threshold", Value::Integer(1))])).unwrap();
        assert_eq!(one.threshold, 1.0);
    }

    #[test]
    fn candidates_unknown_key_lists_the_accepted_keys() {
        let err = candidates_err(&[("cutoff", Value::Float(0.5))]);
        assert_eq!(
            err,
            format!("unknown config key `cutoff`; expected {CANDIDATES_KEYS}")
        );
    }

    #[test]
    fn candidates_wrong_values_are_refused() {
        assert_eq!(
            CandidatesConfig::from_value(&s("x")).unwrap_err(),
            "the config must be a map, got 'x'"
        );
        assert_eq!(
            candidates_err(&[("threshold", Value::Float(1.5))]),
            "threshold must be a number between 0 and 1, got 1.5"
        );
        assert_eq!(
            candidates_err(&[("threshold", s("high"))]),
            "threshold must be a number between 0 and 1, got 'high'"
        );
        assert_eq!(
            candidates_err(&[("nameWeight", Value::Integer(-1))]),
            "nameWeight must be a number ≥ 0, got -1"
        );
        assert_eq!(
            candidates_err(&[
                ("nameWeight", Value::Integer(0)),
                ("vectorWeight", Value::Integer(0))
            ]),
            "nameWeight and vectorWeight cannot both be 0"
        );
        assert_eq!(
            candidates_err(&[("blocking", s("soundex"))]),
            "blocking must be 'prefix3', 'token' or 'none', got 'soundex'"
        );
        assert_eq!(
            candidates_err(&[("maxPairs", Value::Integer(0))]),
            "maxPairs must be an integer ≥ 1, got 0"
        );
        assert_eq!(
            candidates_err(&[("normalise", s("yes"))]),
            "normalise must be a boolean, got 'yes'"
        );
        assert_eq!(
            candidates_err(&[("nameProperty", Value::Integer(3))]),
            "nameProperty must be a string, got 3"
        );
    }

    // ----- merge ---------------------------------------------------------------

    #[test]
    fn merge_defaults_and_keys() {
        let d = MergeConfig::from_value(&Value::Null).unwrap();
        assert_eq!(d, MergeConfig::default());
        assert_eq!((d.name_property(), d.type_property()), ("name", "type"));
        assert_eq!(d.strategy, Strategy::KeepFirst);
        assert!(!d.force);
        let c = MergeConfig::from_value(&map(&[
            ("strategy", s("collect")),
            ("force", Value::Bool(true)),
            ("nameProperty", s("title")),
            ("typeProperty", s("kind")),
        ]))
        .unwrap();
        assert_eq!(c.strategy, Strategy::Collect);
        assert!(c.force);
        assert_eq!((c.name_property(), c.type_property()), ("title", "kind"));
        let n = MergeConfig::from_value(&map(&[("strategy", s("newest"))])).unwrap();
        assert_eq!(n.strategy, Strategy::Newest);
        let k = MergeConfig::from_value(&map(&[("strategy", s("keepFirst"))])).unwrap();
        assert_eq!(k.strategy, Strategy::KeepFirst);
    }

    #[test]
    fn merge_wrong_values_are_refused() {
        let err = |e: &[(&str, Value)]| MergeConfig::from_value(&map(e)).unwrap_err();
        assert_eq!(
            err(&[("strategy", s("oldest"))]),
            "strategy must be 'keepFirst', 'newest' or 'collect', got 'oldest'"
        );
        assert_eq!(
            err(&[("force", s("yes"))]),
            "force must be a boolean, got 'yes'"
        );
        assert_eq!(
            err(&[("dryRun", Value::Bool(true))]),
            format!("unknown config key `dryRun`; expected {MERGE_KEYS}")
        );
        assert_eq!(
            MergeConfig::from_value(&Value::Integer(1)).unwrap_err(),
            "the config must be a map, got 1"
        );
    }

    // ----- merge ids -----------------------------------------------------------

    #[test]
    fn merge_ids_are_nodes_or_ids() {
        assert_eq!(keep_id(&Value::Integer(4)), Ok(4));
        assert_eq!(
            keep_id(&s("Пушкин")),
            Err("keepId must be a node or a node id, got String".into())
        );
        assert_eq!(
            keep_id(&Value::Integer(-1)),
            Err("keepId: -1 is not a node id".into())
        );
        assert_eq!(drop_ids(&Value::Integer(5)), Ok(vec![5]));
        assert_eq!(drop_ids(&Value::Null), Ok(vec![]));
        assert_eq!(
            drop_ids(&Value::List(vec![
                Value::Integer(5),
                Value::Null,
                Value::Integer(6)
            ])),
            Ok(vec![5, 6])
        );
        assert_eq!(
            drop_ids(&Value::List(vec![s("x")])),
            Err(
                "dropIds must be a node, a node id or a list of them, got a list holding String"
                    .into()
            )
        );
        assert_eq!(
            drop_ids(&Value::Bool(true)),
            Err("dropIds must be a node, a node id or a list of them, got Boolean".into())
        );
        assert_eq!(
            drop_ids(&Value::List(vec![Value::Integer(-3)])),
            Err("dropIds: -3 is not a node id".into())
        );
    }

    // ----- resolve -------------------------------------------------------------

    fn take(entries: &[(&str, Value)]) -> (Result<ResolveOptions, String>, Vec<String>) {
        let Value::Map(mut m) = map(entries) else {
            unreachable!()
        };
        let r = ResolveOptions::take_from(&mut m);
        (r, m.into_keys().collect())
    }

    #[test]
    fn resolve_keys_are_taken_out_and_the_rest_stays() {
        let (r, rest) = take(&[
            ("resolve", s("fuzzy")),
            ("threshold", Value::Float(0.8)),
            ("chunk", s("book-1#0")),
        ]);
        assert_eq!(
            r.unwrap(),
            ResolveOptions {
                resolve: Resolve::Fuzzy,
                threshold: 0.8
            }
        );
        assert_eq!(rest, ["chunk"]);
        let (r, rest) = take(&[("resolve", s("alias"))]);
        assert_eq!(r.unwrap().resolve, Resolve::Alias);
        assert!(rest.is_empty());
        let (r, _) = take(&[("resolve", s("exact")), ("threshold", Value::Null)]);
        assert_eq!(r.unwrap(), ResolveOptions::default());
        let (r, _) = take(&[]);
        assert_eq!(r.unwrap(), ResolveOptions::default());
    }

    #[test]
    fn resolve_wrong_values_are_refused() {
        assert_eq!(
            take(&[("resolve", s("soundex"))]).0.unwrap_err(),
            "resolve must be 'exact', 'alias' or 'fuzzy', got 'soundex'"
        );
        assert_eq!(
            take(&[("resolve", s("fuzzy")), ("threshold", Value::Integer(2))])
                .0
                .unwrap_err(),
            "threshold must be a number between 0 and 1, got 2"
        );
        assert_eq!(
            take(&[("threshold", Value::Float(0.8))]).0.unwrap_err(),
            "threshold only applies with resolve: 'fuzzy'"
        );
        assert_eq!(
            take(&[("resolve", s("alias")), ("threshold", Value::Float(0.8))])
                .0
                .unwrap_err(),
            "threshold only applies with resolve: 'fuzzy'"
        );
    }
}
