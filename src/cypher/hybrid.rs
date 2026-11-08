//! Hybrid retrieval: Reciprocal Rank Fusion of a BM25 and a vector ranking,
//! and the config map of `drevo.search.hybrid`.
//!
//! [`rrf_fuse`] merges ranked candidate lists ("legs") into one ranking where
//! each candidate scores `Σ weight / (rrfK + rank)` over the legs that
//! returned it:
//!
//! ```
//! use drevo::cypher::hybrid::{rrf_fuse, Leg, LegWeights};
//!
//! let fts = [7_u64, 3];
//! let vector = [3_u64, 9];
//! let fused = rrf_fuse(
//!     &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
//!     60.0,
//!     LegWeights::default(),
//! );
//! // 3 is in both legs, so it beats 7, which tops only one.
//! assert_eq!(fused[0].id, 3);
//! assert_eq!((fused[0].fts_rank, fused[0].vector_rank), (Some(2), Some(1)));
//! ```
//!
//! Nothing here touches the graph: the executor runs the legs and hands
//! their ids over.

use std::collections::HashMap;
use std::hash::Hash;

use crate::cypher::executor::Value;

/// The RRF constant used when the config names none.
const DEFAULT_RRF_K: f64 = 60.0;

/// The fewest candidates each leg fetches when the config names no
/// `candidates`: `max(k, 20)`.
const MIN_CANDIDATES: usize = 20;

/// The keys a `drevo.search.hybrid` config map accepts, as listed in errors.
pub const CONFIG_KEYS: &str = "candidates, rrfK, mode, ftsFilter, weights";

/// One retriever whose ranking is fused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    /// The BM25 full-text ranking.
    Fts,
    /// The cosine-similarity ranking over an embedding property.
    Vector,
}

/// The factor each leg's reciprocal rank is multiplied by (weighted RRF).
///
/// ```
/// use drevo::cypher::hybrid::LegWeights;
///
/// let w = LegWeights { fts: 0.5, vector: 1.0 };
/// assert_eq!(LegWeights::default(), LegWeights { fts: 1.0, vector: 1.0 });
/// assert!(w.fts < w.vector);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegWeights {
    /// Weight of the BM25 leg.
    pub fts: f64,
    /// Weight of the vector leg.
    pub vector: f64,
}

impl Default for LegWeights {
    fn default() -> Self {
        Self {
            fts: 1.0,
            vector: 1.0,
        }
    }
}

impl LegWeights {
    /// The weight of `leg`.
    #[must_use]
    pub fn of(&self, leg: Leg) -> f64 {
        match leg {
            Leg::Fts => self.fts,
            Leg::Vector => self.vector,
        }
    }
}

/// One fused candidate: its id, fused score and its 1-based rank in each leg
/// (`None` when that leg did not return it).
#[derive(Debug, Clone, PartialEq)]
pub struct Fused<Id> {
    /// The candidate (a node or relationship id).
    pub id: Id,
    /// `Σ weight / (rrfK + rank)` over the legs that returned it.
    pub score: f64,
    /// 1-based rank in the BM25 leg.
    pub fts_rank: Option<usize>,
    /// 1-based rank in the vector leg.
    pub vector_rank: Option<usize>,
}

/// Fuse ranked candidate lists with (weighted) Reciprocal Rank Fusion.
///
/// Each leg lists ids best first; an id's rank in a leg is its 1-based
/// position (a repeated id keeps its first, best rank). The result is ordered
/// by descending score, then candidates present in both legs first, then
/// ascending id, so equal inputs always give the same order. A candidate
/// whose score is zero — returned only by legs weighted `0` — is left out.
///
/// ```
/// use drevo::cypher::hybrid::{rrf_fuse, Leg, LegWeights};
///
/// let fused = rrf_fuse(&[(Leg::Fts, &[1_u64, 2][..])], 60.0, LegWeights::default());
/// assert_eq!(fused.iter().map(|f| f.id).collect::<Vec<_>>(), [1, 2]);
/// assert_eq!(fused[0].score, 1.0 / 61.0);
/// ```
#[must_use]
pub fn rrf_fuse<Id: Copy + Eq + Hash + Ord>(
    legs: &[(Leg, &[Id])],
    rrf_k: f64,
    weights: LegWeights,
) -> Vec<Fused<Id>> {
    let mut fused: Vec<Fused<Id>> = Vec::new();
    let mut slot: HashMap<Id, usize> = HashMap::new();
    for (leg, ids) in legs {
        let weight = weights.of(*leg);
        for (position, id) in ids.iter().enumerate() {
            let rank = position + 1;
            let index = *slot.entry(*id).or_insert_with(|| {
                fused.push(Fused {
                    id: *id,
                    score: 0.0,
                    fts_rank: None,
                    vector_rank: None,
                });
                fused.len() - 1
            });
            let entry = &mut fused[index];
            let leg_rank = match leg {
                Leg::Fts => &mut entry.fts_rank,
                Leg::Vector => &mut entry.vector_rank,
            };
            if leg_rank.is_some() {
                continue; // a repeated id keeps its first, best rank
            }
            *leg_rank = Some(rank);
            entry.score += weight / (rrf_k + rank as f64);
        }
    }
    fused.retain(|f| f.score > 0.0);
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| in_both(b).cmp(&in_both(a)))
            .then_with(|| a.id.cmp(&b.id))
    });
    fused
}

/// Whether both legs returned the candidate.
fn in_both<Id>(f: &Fused<Id>) -> bool {
    f.fts_rank.is_some() && f.vector_rank.is_some()
}

/// Which legs `drevo.search.hybrid` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridMode {
    /// Both legs when an embedder is installed and a vector target is named,
    /// else BM25 only.
    Auto,
    /// Both legs; an error when no embedder is installed.
    Hybrid,
    /// BM25 only.
    Fts,
    /// Vector only.
    Vector,
}

impl HybridMode {
    /// The mode's name as written in the config and reported per row.
    ///
    /// ```
    /// use drevo::cypher::hybrid::HybridMode;
    /// assert_eq!(HybridMode::Fts.as_str(), "fts");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hybrid => "hybrid",
            Self::Fts => "fts",
            Self::Vector => "vector",
        }
    }
}

/// Why the requested mode cannot run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeUnavailable {
    /// A vector leg was asked for but no label / relationship type was given.
    NoVectorTarget,
    /// A vector leg was asked for but no text embedder is installed.
    NoEmbedder,
}

/// The mode that actually runs: `auto` becomes `hybrid` when a vector target
/// is named and an embedder is installed, else `fts`; an explicit `hybrid` or
/// `vector` needs both.
///
/// ```
/// use drevo::cypher::hybrid::{resolve_mode, HybridMode};
///
/// assert_eq!(resolve_mode(HybridMode::Auto, true, false), Ok(HybridMode::Fts));
/// assert_eq!(resolve_mode(HybridMode::Auto, true, true), Ok(HybridMode::Hybrid));
/// ```
///
/// # Errors
///
/// [`ModeUnavailable`] when an explicit `hybrid` / `vector` lacks a vector
/// target or an embedder (the target is checked first).
pub fn resolve_mode(
    requested: HybridMode,
    has_vector_target: bool,
    has_embedder: bool,
) -> Result<HybridMode, ModeUnavailable> {
    match requested {
        HybridMode::Fts => Ok(HybridMode::Fts),
        HybridMode::Auto if has_vector_target && has_embedder => Ok(HybridMode::Hybrid),
        HybridMode::Auto => Ok(HybridMode::Fts),
        HybridMode::Hybrid | HybridMode::Vector if !has_vector_target => {
            Err(ModeUnavailable::NoVectorTarget)
        }
        HybridMode::Hybrid | HybridMode::Vector if !has_embedder => {
            Err(ModeUnavailable::NoEmbedder)
        }
        mode => Ok(mode),
    }
}

/// The parsed config map of `drevo.search.hybrid`.
///
/// ```
/// use drevo::cypher::hybrid::{HybridConfig, HybridMode};
///
/// let config = HybridConfig::default();
/// assert_eq!(config.mode, HybridMode::Auto);
/// assert_eq!(config.depth(5), 20);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct HybridConfig {
    /// How deep each leg goes before fusion; `None` = `max(k, 20)`.
    pub candidates: Option<usize>,
    /// The RRF constant.
    pub rrf_k: f64,
    /// The requested mode.
    pub mode: HybridMode,
    /// A label (or relationship type) the BM25 leg is restricted to.
    pub fts_filter: Option<String>,
    /// Per-leg weights.
    pub weights: LegWeights,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            candidates: None,
            rrf_k: DEFAULT_RRF_K,
            mode: HybridMode::Auto,
            fts_filter: None,
            weights: LegWeights::default(),
        }
    }
}

impl HybridConfig {
    /// Parse the optional config argument: `null` gives the defaults, a map
    /// sets the keys it names (a `null` value keeps that key's default).
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use drevo::cypher::executor::Value;
    /// use drevo::cypher::hybrid::{HybridConfig, HybridMode};
    ///
    /// let map = BTreeMap::from([("mode".to_string(), Value::String("fts".into()))]);
    /// let config = HybridConfig::from_value(&Value::Map(map)).unwrap();
    /// assert_eq!(config.mode, HybridMode::Fts);
    /// ```
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing [`CONFIG_KEYS`]) or a value has the wrong type or
    /// range.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let map = match value {
            Value::Null => return Ok(Self::default()),
            Value::Map(map) => map,
            other => {
                return Err(format!(
                    "the config must be a map, got {}",
                    other.type_name()
                ))
            }
        };
        let mut config = Self::default();
        for (key, v) in map {
            if matches!(v, Value::Null) {
                continue;
            }
            match key.as_str() {
                "candidates" => match v {
                    Value::Integer(n) if *n >= 1 => {
                        config.candidates = Some(usize::try_from(*n).unwrap_or(usize::MAX));
                    }
                    other => {
                        return Err(format!(
                            "candidates must be a positive integer, got {}",
                            describe(other)
                        ))
                    }
                },
                "rrfK" => {
                    config.rrf_k = non_negative(v)
                        .ok_or_else(|| format!("rrfK must be a number ≥ 0, got {}", describe(v)))?;
                }
                "mode" => {
                    config.mode = match v {
                        Value::String(name) => HybridMode::parse(name),
                        _ => None,
                    }
                    .ok_or_else(|| {
                        format!(
                            "mode must be 'auto', 'hybrid', 'fts' or 'vector', got {}",
                            describe(v)
                        )
                    })?;
                }
                "ftsFilter" => match v {
                    Value::String(label) => config.fts_filter = Some(label.clone()),
                    other => {
                        return Err(format!(
                            "ftsFilter must be a label or relationship type, got {}",
                            describe(other)
                        ))
                    }
                },
                "weights" => config.weights = parse_weights(v)?,
                other => {
                    return Err(format!(
                        "unknown config key `{other}`; expected {CONFIG_KEYS}"
                    ))
                }
            }
        }
        Ok(config)
    }

    /// How many candidates each leg fetches for `k` rows: `candidates`
    /// (default `max(k, 20)`), never fewer than `k`.
    #[must_use]
    pub fn depth(&self, k: usize) -> usize {
        self.candidates.unwrap_or(MIN_CANDIDATES).max(k)
    }
}

impl HybridMode {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "hybrid" => Some(Self::Hybrid),
            "fts" => Some(Self::Fts),
            "vector" => Some(Self::Vector),
            _ => None,
        }
    }
}

/// `value` as a finite number ≥ 0.
fn non_negative(value: &Value) -> Option<f64> {
    let n = match value {
        Value::Integer(i) => *i as f64,
        Value::Float(f) => *f,
        _ => return None,
    };
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// A value as quoted in an error: strings in quotes, numbers as written,
/// anything else by its type.
fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{s}'"),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => other.type_name().to_string(),
    }
}

/// The `weights` map: `fts` and `vector`, each a finite number ≥ 0.
fn parse_weights(value: &Value) -> Result<LegWeights, String> {
    let Value::Map(map) = value else {
        return Err(format!(
            "weights must be a map like {{fts: 1.0, vector: 1.0}}, got {}",
            value.type_name()
        ));
    };
    let mut weights = LegWeights::default();
    for (key, v) in map {
        if matches!(v, Value::Null) {
            continue;
        }
        let slot = match key.as_str() {
            "fts" => &mut weights.fts,
            "vector" => &mut weights.vector,
            other => {
                return Err(format!(
                    "unknown weights key `{other}`; expected fts, vector"
                ))
            }
        };
        *slot = non_negative(v).ok_or_else(|| {
            format!(
                "weights.{key} must be a finite number ≥ 0, got {}",
                describe(v)
            )
        })?;
    }
    Ok(weights)
}

/// Parse the `k` argument: a positive integer.
///
/// ```
/// use drevo::cypher::executor::Value;
/// use drevo::cypher::hybrid::parse_k;
///
/// assert_eq!(parse_k(&Value::Integer(5)), Ok(5));
/// assert!(parse_k(&Value::Integer(0)).is_err());
/// ```
///
/// # Errors
///
/// A message naming `k` when the value is not an integer ≥ 1.
pub fn parse_k(value: &Value) -> Result<usize, String> {
    match value {
        Value::Integer(n) if *n >= 1 => Ok(usize::try_from(*n).unwrap_or(usize::MAX)),
        other => Err(format!(
            "k must be a positive integer, got {}",
            describe(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn ids(fused: &[Fused<u64>]) -> Vec<u64> {
        fused.iter().map(|f| f.id).collect()
    }

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

    // ── rrf_fuse (ports drevo-mcp `test_rrf_fuse_*`) ──────────────────────

    #[test]
    fn rrf_orders_by_reciprocal_rank() {
        let fts = [1_u64, 2, 3];
        let fused = rrf_fuse(&[(Leg::Fts, &fts[..])], 60.0, LegWeights::default());
        assert_eq!(ids(&fused), [1, 2, 3]);
        assert!((fused[0].score - 1.0 / 61.0).abs() < 1e-12);
        assert!((fused[2].score - 1.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_a_node_in_both_legs_beats_one_that_tops_a_single_leg() {
        let fts = [10_u64, 20];
        let vector = [30_u64, 20];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            60.0,
            LegWeights::default(),
        );
        assert_eq!(fused[0].id, 20, "{fused:?}");
        assert!((fused[0].score - 2.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_dedups_and_records_source_ranks() {
        let fts = [5_u64, 6, 5];
        let vector = [6_u64];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            60.0,
            LegWeights::default(),
        );
        assert_eq!(fused.len(), 2, "5 appears once: {fused:?}");
        let six = fused.iter().find(|f| f.id == 6).cloned();
        let five = fused.iter().find(|f| f.id == 5).cloned();
        assert_eq!(
            six.map(|f| (f.fts_rank, f.vector_rank)),
            Some((Some(2), Some(1)))
        );
        assert_eq!(
            five.map(|f| (f.fts_rank, f.vector_rank)),
            Some((Some(1), None)),
            "a repeated id keeps its first rank"
        );
    }

    #[test]
    fn rrf_larger_k_compresses_the_gap() {
        let fts = [1_u64, 2];
        let gap = |k: f64| {
            let fused = rrf_fuse(&[(Leg::Fts, &fts[..])], k, LegWeights::default());
            fused[0].score - fused[1].score
        };
        assert!(gap(600.0) < gap(60.0));
        assert!(gap(60.0) < gap(1.0));
    }

    #[test]
    fn rrf_empty_input_gives_empty_output() {
        assert!(rrf_fuse::<u64>(&[], 60.0, LegWeights::default()).is_empty());
        let none: [u64; 0] = [];
        assert!(rrf_fuse(
            &[(Leg::Fts, &none[..]), (Leg::Vector, &none[..])],
            60.0,
            LegWeights::default()
        )
        .is_empty());
    }

    #[test]
    fn rrf_weight_zero_on_fts_equals_the_vector_ranking() {
        let fts = [1_u64, 2, 3, 4];
        let vector = [4_u64, 9, 2];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            60.0,
            LegWeights {
                fts: 0.0,
                vector: 1.0,
            },
        );
        assert_eq!(ids(&fused), [4, 9, 2]);
        assert_eq!(fused[0].fts_rank, Some(4), "ranks are still recorded");
    }

    #[test]
    fn rrf_weights_scale_each_leg() {
        let fts = [1_u64];
        let vector = [2_u64];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            60.0,
            LegWeights {
                fts: 1.0,
                vector: 2.0,
            },
        );
        assert_eq!(ids(&fused), [2, 1]);
        assert!((fused[0].score - 2.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_ties_break_on_both_legs_then_id() {
        // 8 and 3 each top one leg: equal scores, lower id first.
        let fts = [8_u64];
        let vector = [3_u64];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            60.0,
            LegWeights::default(),
        );
        assert_eq!(ids(&fused), [3, 8]);
        // Equal score, but 9 is in both legs: it goes first.
        // 9: 1/(0+2) + 1/(0+2) = 1.0; 4: 1/(0+1) = 1.0.
        let fts = [4_u64, 9];
        let vector = [7_u64, 9];
        let fused = rrf_fuse(
            &[(Leg::Fts, &fts[..]), (Leg::Vector, &vector[..])],
            0.0,
            LegWeights::default(),
        );
        assert_eq!(ids(&fused), [9, 4, 7]);
    }

    // ── resolve_mode ──────────────────────────────────────────────────────

    #[test]
    fn auto_degrades_to_fts_without_embedder_or_target() {
        assert_eq!(
            resolve_mode(HybridMode::Auto, true, true),
            Ok(HybridMode::Hybrid)
        );
        assert_eq!(
            resolve_mode(HybridMode::Auto, true, false),
            Ok(HybridMode::Fts)
        );
        assert_eq!(
            resolve_mode(HybridMode::Auto, false, true),
            Ok(HybridMode::Fts)
        );
        assert_eq!(
            resolve_mode(HybridMode::Fts, false, false),
            Ok(HybridMode::Fts)
        );
    }

    #[test]
    fn explicit_vector_legs_need_target_and_embedder() {
        for mode in [HybridMode::Hybrid, HybridMode::Vector] {
            assert_eq!(resolve_mode(mode, true, true), Ok(mode));
            assert_eq!(
                resolve_mode(mode, true, false),
                Err(ModeUnavailable::NoEmbedder)
            );
            assert_eq!(
                resolve_mode(mode, false, false),
                Err(ModeUnavailable::NoVectorTarget)
            );
        }
    }

    // ── config ────────────────────────────────────────────────────────────

    #[test]
    fn config_null_gives_defaults() {
        assert_eq!(
            HybridConfig::from_value(&Value::Null),
            Ok(HybridConfig::default())
        );
        assert_eq!(
            HybridConfig::from_value(&map(&[("mode", Value::Null)])),
            Ok(HybridConfig::default())
        );
    }

    #[test]
    fn config_sets_every_key() {
        let config = HybridConfig::from_value(&map(&[
            ("candidates", Value::Integer(50)),
            ("rrfK", Value::Integer(10)),
            ("mode", s("vector")),
            ("ftsFilter", s("Doc")),
            (
                "weights",
                map(&[("fts", Value::Float(0.5)), ("vector", Value::Integer(2))]),
            ),
        ]));
        assert_eq!(
            config,
            Ok(HybridConfig {
                candidates: Some(50),
                rrf_k: 10.0,
                mode: HybridMode::Vector,
                fts_filter: Some("Doc".to_string()),
                weights: LegWeights {
                    fts: 0.5,
                    vector: 2.0
                },
            })
        );
        for (name, mode) in [
            ("auto", HybridMode::Auto),
            ("hybrid", HybridMode::Hybrid),
            ("fts", HybridMode::Fts),
        ] {
            let parsed = HybridConfig::from_value(&map(&[("mode", s(name))]));
            assert_eq!(parsed.map(|c| c.mode), Ok(mode));
        }
    }

    fn config_error(value: &Value) -> String {
        match HybridConfig::from_value(value) {
            Err(message) => message,
            Ok(config) => panic!("expected an error, got {config:?}"),
        }
    }

    #[test]
    fn config_unknown_key_lists_the_accepted_keys() {
        let message = config_error(&map(&[("topK", Value::Integer(3))]));
        assert!(message.contains("`topK`"), "{message}");
        assert!(message.contains(CONFIG_KEYS), "{message}");
    }

    #[test]
    fn config_must_be_a_map() {
        let message = config_error(&Value::Integer(3));
        assert!(message.contains("config must be a map"), "{message}");
        assert!(message.contains("Integer"), "{message}");
    }

    #[test]
    fn config_rejects_bad_values_naming_the_key() {
        for (value, key) in [
            (map(&[("rrfK", Value::Integer(-1))]), "rrfK"),
            (map(&[("rrfK", s("60"))]), "rrfK"),
            (map(&[("candidates", Value::Integer(0))]), "candidates"),
            (map(&[("candidates", Value::Float(2.5))]), "candidates"),
            (map(&[("mode", s("bm25"))]), "mode"),
            (map(&[("mode", Value::Integer(1))]), "mode"),
            (map(&[("ftsFilter", Value::Integer(1))]), "ftsFilter"),
            (map(&[("weights", Value::Integer(1))]), "weights"),
            (
                map(&[("weights", map(&[("fts", Value::Float(-0.5))]))]),
                "weights.fts",
            ),
            (
                map(&[("weights", map(&[("vector", Value::Float(f64::NAN))]))]),
                "weights.vector",
            ),
            (
                map(&[("weights", map(&[("graph", Value::Float(1.0))]))]),
                "`graph`",
            ),
        ] {
            let message = config_error(&value);
            assert!(message.contains(key), "{key}: {message}");
        }
    }

    #[test]
    fn mode_error_lists_the_modes() {
        let message = config_error(&map(&[("mode", s("bm25"))]));
        assert!(
            message.contains("'auto', 'hybrid', 'fts' or 'vector'"),
            "{message}"
        );
    }

    #[test]
    fn depth_defaults_to_max_k_20_and_is_clamped_to_k() {
        let default = HybridConfig::default();
        assert_eq!(default.depth(5), 20);
        assert_eq!(default.depth(30), 30);
        let shallow = HybridConfig {
            candidates: Some(3),
            ..HybridConfig::default()
        };
        assert_eq!(shallow.depth(10), 10, "candidates < k is clamped to k");
        let deep = HybridConfig {
            candidates: Some(100),
            ..HybridConfig::default()
        };
        assert_eq!(deep.depth(10), 100);
    }

    #[test]
    fn k_must_be_a_positive_integer() {
        assert_eq!(parse_k(&Value::Integer(7)), Ok(7));
        for bad in [Value::Integer(0), Value::Integer(-2), s("3"), Value::Null] {
            let message = parse_k(&bad).err().unwrap_or_default();
            assert!(message.starts_with("k must be"), "{bad:?}: {message}");
        }
    }
}
