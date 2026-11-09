//! Community summaries: the graph cut into communities (Louvain or weakly
//! connected components), each written back as a `:Community` node with a
//! summary of its members, so a corpus-level question ("what is this about?")
//! can be answered from a handful of summaries instead of a k-hop walk.
//!
//! `drevo.community.summarize` writes
//!
//! ```text
//! (:Community {id, layer, size, modularity, summary, keywords, mode, computed_at, member_ids_hash})
//! (member)-[:MEMBER_OF {layer, rank}]->(:Community)      // rank = in-community PageRank
//! (:Community)-[:RELATED_COMMUNITY {weight}]->(:Community) // relationships between the two
//! ```
//!
//! This module holds the pure parts — the config maps, the partition, the
//! in-community ranking, the weights between communities, the member-set
//! hash and the summarisers — tested without an engine; the procedures in
//! the executor read the graph, hand it over and write the result.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::cypher::executor::Value;
use crate::cypher::hybrid::HybridConfig;

pub(crate) mod extractive;

/// The label of a community node.
pub(crate) const COMMUNITY_LABEL: &str = "Community";

/// The relationship from a member to its community.
pub(crate) const MEMBER_OF: &str = "MEMBER_OF";

/// The relationship between two communities linked by relationships.
pub(crate) const RELATED_COMMUNITY: &str = "RELATED_COMMUNITY";

/// The layer a config names when it names none.
pub(crate) const DEFAULT_LAYER: &str = "default";

/// The property community embeddings are read from by `drevo.community.query`
/// and the global scope of `drevo.rag.context`.
pub(crate) const DEFAULT_EMBEDDING_PROPERTY: &str = "embedding";

/// The labels left out of the partition unless `labels` names others:
/// conversation and bookkeeping, not knowledge — communities included.
pub(crate) const DEFAULT_EXCLUDED_LABELS: [&str; 9] = [
    "Message",
    "ReasoningTrace",
    "ContextTrace",
    "Community",
    "Alias",
    "Source",
    "Chunk",
    "IngestJob",
    "IngestItem",
];

/// The keys a `drevo.community.summarize` config map accepts, as listed in
/// errors.
pub(crate) const SUMMARIZE_KEYS: &str = "algorithm, resolution, labels, relTypes, minSize, \
     maxMembers, mode, model, tokenBudget, replace, layer, async";

/// The keys a `drevo.community.query` config map accepts, as listed in
/// errors.
pub(crate) const QUERY_KEYS: &str = "layer, embeddingProperty, candidates, rrfK, mode";

/// The smallest `tokenBudget` a summary may be given.
pub(crate) const MIN_SUMMARY_BUDGET: usize = 20;

/// How many keywords a community keeps.
pub(crate) const KEYWORDS_PER_COMMUNITY: usize = 10;

/// How many relationship types a summary names.
pub(crate) const REL_TYPES_PER_SUMMARY: usize = 3;

/// How the graph is cut into communities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    /// Louvain modularity optimisation.
    Louvain,
    /// Weakly connected components.
    Wcc,
}

/// How a community's summary is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryMode {
    /// By a model when one is configured, else extractively.
    Auto,
    /// By the chat model; an error when none is configured.
    Llm,
    /// From the members' names, keywords and relationship types.
    Extractive,
}

impl SummaryMode {
    /// The mode's name as written in a config map and on a community.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Llm => "llm",
            Self::Extractive => "extractive",
        }
    }
}

/// The parsed config map of `drevo.community.summarize`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummarizeConfig {
    pub(crate) algorithm: Algorithm,
    pub(crate) resolution: f64,
    pub(crate) labels: Option<Vec<String>>,
    pub(crate) rel_types: Option<Vec<String>>,
    pub(crate) min_size: usize,
    pub(crate) max_members: usize,
    pub(crate) mode: SummaryMode,
    pub(crate) model: Option<String>,
    pub(crate) token_budget: usize,
    pub(crate) replace: bool,
    pub(crate) layer: String,
    pub(crate) run_async: bool,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Louvain,
            resolution: 1.0,
            labels: None,
            rel_types: None,
            min_size: 3,
            max_members: 40,
            mode: SummaryMode::Auto,
            model: None,
            token_budget: 300,
            replace: true,
            layer: DEFAULT_LAYER.to_string(),
            run_async: false,
        }
    }
}

impl SummarizeConfig {
    /// Parse the optional config argument: `null` gives the defaults, a map
    /// sets the keys it names (a `null` value keeps that key's default).
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing [`SUMMARIZE_KEYS`]) or a value has the wrong type
    /// or range.
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut config = Self::default();
        for (key, v) in &config_map(value)? {
            if matches!(v, Value::Null) {
                if SUMMARIZE_KEYS.split(", ").any(|k| k == key) {
                    continue;
                }
                return Err(unknown(key, SUMMARIZE_KEYS));
            }
            match key.as_str() {
                "algorithm" => {
                    config.algorithm = match v {
                        Value::String(name) if name == "louvain" => Algorithm::Louvain,
                        Value::String(name) if name == "wcc" => Algorithm::Wcc,
                        other => {
                            return Err(format!(
                                "algorithm must be 'louvain' or 'wcc', got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "resolution" => config.resolution = non_negative("resolution", v)?,
                "labels" => config.labels = Some(strings("labels", v)?),
                "relTypes" => config.rel_types = Some(strings("relTypes", v)?),
                "minSize" => config.min_size = whole("minSize", v, 1)?,
                "maxMembers" => config.max_members = whole("maxMembers", v, 1)?,
                "mode" => {
                    config.mode = match v {
                        Value::String(name) if name == "auto" => SummaryMode::Auto,
                        Value::String(name) if name == "llm" => SummaryMode::Llm,
                        Value::String(name) if name == "extractive" => SummaryMode::Extractive,
                        other => {
                            return Err(format!(
                                "mode must be 'auto', 'llm' or 'extractive', got {}",
                                describe(other)
                            ))
                        }
                    }
                }
                "model" => match v {
                    Value::String(model) => config.model = Some(model.clone()),
                    other => {
                        return Err(format!(
                            "model must be a string or null, got {}",
                            describe(other)
                        ))
                    }
                },
                "tokenBudget" => {
                    config.token_budget = whole("tokenBudget", v, MIN_SUMMARY_BUDGET)?;
                }
                "replace" => config.replace = boolean("replace", v)?,
                "layer" => config.layer = layer(v)?,
                "async" => config.run_async = boolean("async", v)?,
                other => return Err(unknown(other, SUMMARIZE_KEYS)),
            }
        }
        Ok(config)
    }

    /// The `labels` and `relTypes` the partition was computed with, as the
    /// JSON object stored on each community.
    pub(crate) fn scope_json(&self) -> String {
        serde_json::json!({ "labels": self.labels, "relTypes": self.rel_types }).to_string()
    }

    /// The config whose `labels` and `relTypes` are read back from `json`
    /// ([`Self::scope_json`]); the defaults when it is missing or unreadable.
    pub(crate) fn from_scope_json(json: Option<&str>) -> Self {
        let parsed = json.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok());
        let list = |key: &str| -> Option<Vec<String>> {
            let items = parsed.as_ref()?.get(key)?.as_array()?;
            items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        Self {
            labels: list("labels"),
            rel_types: list("relTypes"),
            ..Self::default()
        }
    }

    /// Whether a node with `labels` is partitioned: one of `labels` when the
    /// config names them, else none of [`DEFAULT_EXCLUDED_LABELS`];
    /// communities never.
    pub(crate) fn admits_labels(&self, labels: &[String]) -> bool {
        if labels.iter().any(|l| l == COMMUNITY_LABEL) {
            return false;
        }
        match &self.labels {
            Some(wanted) => labels.iter().any(|l| wanted.contains(l)),
            None => !labels
                .iter()
                .any(|l| DEFAULT_EXCLUDED_LABELS.contains(&l.as_str())),
        }
    }

    /// Whether a relationship of `kind` is partitioned: one of `relTypes`
    /// when the config names them; the community bookkeeping never.
    pub(crate) fn admits_rel_type(&self, kind: &str) -> bool {
        if kind == MEMBER_OF || kind == RELATED_COMMUNITY {
            return false;
        }
        self.rel_types
            .as_ref()
            .is_none_or(|wanted| wanted.iter().any(|t| t == kind))
    }
}

/// The parsed config map of `drevo.community.query`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueryConfig {
    pub(crate) layer: String,
    pub(crate) embedding_property: String,
    pub(crate) hybrid: HybridConfig,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            layer: DEFAULT_LAYER.to_string(),
            embedding_property: DEFAULT_EMBEDDING_PROPERTY.to_string(),
            hybrid: HybridConfig::default(),
        }
    }
}

impl QueryConfig {
    /// Parse the optional config argument: `null` gives the defaults; the
    /// search keys mean what they mean in `drevo.search.hybrid`.
    ///
    /// # Errors
    ///
    /// A message naming the offending key when the value is not a map, a key
    /// is unknown (listing [`QUERY_KEYS`]) or a value has the wrong type or
    /// range.
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut config = Self::default();
        let mut search = BTreeMap::new();
        for (key, v) in &config_map(value)? {
            match key.as_str() {
                "candidates" | "rrfK" | "mode" => {
                    search.insert(key.clone(), v.clone());
                }
                _ if matches!(v, Value::Null) => {
                    if !QUERY_KEYS.split(", ").any(|k| k == key) {
                        return Err(unknown(key, QUERY_KEYS));
                    }
                }
                "layer" => config.layer = layer(v)?,
                "embeddingProperty" => match v {
                    Value::String(p) => config.embedding_property = p.clone(),
                    other => {
                        return Err(format!(
                            "embeddingProperty must be a string, got {}",
                            describe(other)
                        ))
                    }
                },
                other => return Err(unknown(other, QUERY_KEYS)),
            }
        }
        config.hybrid = HybridConfig::from_value(&Value::Map(search))?;
        Ok(config)
    }
}

/// The mode a summary is written in: `requested`, with `'auto'` resolved by
/// whether a model is available. `Err` when `'llm'` is asked for and none is.
pub(crate) fn effective_mode(
    requested: SummaryMode,
    llm_available: bool,
) -> Result<SummaryMode, ()> {
    match (requested, llm_available) {
        (SummaryMode::Extractive, _) | (SummaryMode::Auto, false) => Ok(SummaryMode::Extractive),
        (SummaryMode::Llm | SummaryMode::Auto, true) => Ok(SummaryMode::Llm),
        (SummaryMode::Llm, false) => Err(()),
    }
}

/// A graph cut into communities.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Partition {
    /// The communities' members, each ascending; the communities largest
    /// first, then by smallest member id.
    pub(crate) groups: Vec<Vec<u64>>,
    /// The modularity of the partition.
    pub(crate) modularity: f64,
}

/// Cut the graph of `node_ids` and `edges` (`from, to, weight`) into
/// communities with `algorithm` (`resolution` is Louvain's γ).
pub(crate) fn partition(
    node_ids: Vec<u64>,
    edges: &[(u64, u64, f32)],
    algorithm: Algorithm,
    resolution: f64,
) -> Result<Partition, String> {
    use crate::algorithms::{louvain, wcc, AdjacencyList, LouvainConfig};
    if !(resolution.is_finite() && resolution >= 0.0) {
        return Err(format!("resolution must be a number ≥ 0, got {resolution}"));
    }
    let graph = AdjacencyList::from_parts(node_ids, edges.iter().copied());
    let assignment = match algorithm {
        Algorithm::Louvain => {
            let config = LouvainConfig::new(resolution, LouvainConfig::default().max_levels)
                .map_err(|e| e.to_string())?;
            louvain(&graph, &config).communities
        }
        Algorithm::Wcc => wcc(&graph).components,
    };
    let mut by_community: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    for (id, community) in assignment {
        by_community.entry(community).or_default().push(id);
    }
    let mut groups: Vec<Vec<u64>> = by_community
        .into_values()
        .map(|mut members| {
            members.sort_unstable();
            members
        })
        .collect();
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then(a.first().cmp(&b.first())));
    let modularity = modularity(&groups, edges, resolution);
    Ok(Partition { groups, modularity })
}

/// The modularity (γ = `resolution`) of `groups` over the undirected,
/// weighted `edges`: `Σ_c L_c / m − γ (d_c / 2m)²`, where `m` is the total
/// weight, `L_c` the weight inside community `c` and `d_c` the weighted
/// degree of its members. Edges with an end in no group count towards `m`
/// and the degrees; no edges at all give `0`.
pub(crate) fn modularity(groups: &[Vec<u64>], edges: &[(u64, u64, f32)], resolution: f64) -> f64 {
    let group_of: HashMap<u64, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(i, members)| members.iter().map(move |&id| (id, i)))
        .collect();
    let mut total = 0.0;
    let mut inside = vec![0.0; groups.len()];
    let mut degree = vec![0.0; groups.len()];
    for &(from, to, weight) in edges {
        let w = f64::from(weight);
        total += w;
        let (a, b) = (group_of.get(&from), group_of.get(&to));
        if let Some(&a) = a {
            degree[a] += w;
        }
        if let Some(&b) = b {
            degree[b] += w;
        }
        if let (Some(a), Some(b)) = (a, b) {
            if a == b {
                inside[*a] += w;
            }
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    inside
        .iter()
        .zip(&degree)
        .map(|(l, d)| l / total - resolution * (d / (2.0 * total)).powi(2))
        .sum()
}

/// PageRank inside one community: `members` and the `edges` between them
/// (others are ignored), undirected; best first, ties by id.
pub(crate) fn member_ranks(members: &[u64], edges: &[(u64, u64, f32)]) -> Vec<(u64, f64)> {
    use crate::algorithms::{oriented_edges, pagerank, AdjacencyList, PageRankConfig};
    let inside: HashSet<u64> = members.iter().copied().collect();
    let edges = edges
        .iter()
        .filter(|(from, to, _)| inside.contains(from) && inside.contains(to))
        .copied();
    let graph = AdjacencyList::from_parts(
        members.to_vec(),
        oriented_edges(edges, crate::model::Direction::Both),
    );
    pagerank(&graph, &PageRankConfig::default()).ranked()
}

/// How many `edges` link each pair of `groups`, as `(a, b, count)` with
/// `a < b` group indexes, in index order; pairs with none are left out.
pub(crate) fn related_weights(
    groups: &[Vec<u64>],
    edges: &[(u64, u64, f32)],
) -> Vec<(usize, usize, i64)> {
    let group_of: HashMap<u64, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(i, members)| members.iter().map(move |&id| (id, i)))
        .collect();
    let mut counts: BTreeMap<(usize, usize), i64> = BTreeMap::new();
    for (from, to, _) in edges {
        if let (Some(&a), Some(&b)) = (group_of.get(from), group_of.get(to)) {
            if a != b {
                *counts.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
    }
    counts.into_iter().map(|((a, b), n)| (a, b, n)).collect()
}

/// A stable fingerprint of a member set, whatever the order of `ids`: the
/// 64-bit FNV-1a hash of the sorted ids, as 16 hex digits.
pub(crate) fn member_ids_hash(ids: &[u64]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hash = OFFSET;
    for byte in (sorted.len() as u64)
        .to_le_bytes()
        .into_iter()
        .chain(sorted.iter().flat_map(|id| id.to_le_bytes()))
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// The id of the `index`-th community of `layer`.
pub(crate) fn community_id(layer: &str, index: usize) -> String {
    format!("{layer}-{index}")
}

/// The `n` most frequent relationship types of `kinds`, most frequent
/// first, ties by name.
pub(crate) fn heaviest_rel_types<'a>(
    kinds: impl IntoIterator<Item = &'a str>,
    n: usize,
) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for kind in kinds {
        *counts.entry(kind).or_insert(0) += 1;
    }
    let mut sorted: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(kind, count)| (kind.to_string(), count))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted.truncate(n);
    sorted
}

/// The top `k` keywords of the members' `texts` (`keywords()`'s ranking:
/// stopwords dropped, tf·idf with `n` documents and `df` per term).
///
/// # Errors
///
/// A failing `df` lookup.
pub(crate) fn community_keywords<F>(
    texts: &[String],
    k: usize,
    n: u64,
    df: &F,
) -> crate::error::Result<Vec<String>>
where
    F: Fn(&[String]) -> crate::error::Result<u64>,
{
    crate::fts::keywords::extract_keywords_scored(&texts.join("\n"), k, false, n, df)
}

/// The document statistics keywords are ranked by: one document per
/// partitioned node (its text properties), indexed by trigram — so a
/// community's keywords depend on the graph being partitioned, not on
/// whatever else (earlier summaries, traces) the full-text index holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrigramCorpus {
    docs: u64,
    postings: HashMap<String, Vec<u32>>,
}

impl TrigramCorpus {
    /// The corpus of `docs`.
    pub(crate) fn new(docs: impl IntoIterator<Item = String>) -> Self {
        let mut corpus = Self::default();
        for (index, doc) in docs.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            for trigram in crate::fts::tokenizer::trigrams(&doc) {
                corpus.postings.entry(trigram).or_default().push(index);
            }
            corpus.docs += 1;
        }
        corpus
    }

    /// How many documents there are.
    pub(crate) fn doc_count(&self) -> u64 {
        self.docs
    }

    /// How many documents contain every one of `trigrams`.
    pub(crate) fn df(&self, trigrams: &[String]) -> u64 {
        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for trigram in trigrams {
            match self.postings.get(trigram) {
                Some(list) => lists.push(list),
                None => return 0,
            }
        }
        lists.sort_by_key(|l| l.len());
        let Some((first, rest)) = lists.split_first() else {
            return 0;
        };
        first
            .iter()
            .filter(|doc| rest.iter().all(|l| l.binary_search(doc).is_ok()))
            .count() as u64
    }
}

/// The members of each community now: `stored` (community → its
/// `MEMBER_OF` members) plus each unassigned node of `links` (`node,
/// neighbour`) given to the community it has most links into, ties to the
/// lowest community key. Nodes linking into no community are left out.
pub(crate) fn current_members(
    stored: &BTreeMap<u64, BTreeSet<u64>>,
    links: &[(u64, u64)],
) -> BTreeMap<u64, BTreeSet<u64>> {
    let community_of: HashMap<u64, u64> = stored
        .iter()
        .flat_map(|(&c, members)| members.iter().map(move |&m| (m, c)))
        .collect();
    let mut votes: BTreeMap<u64, BTreeMap<u64, usize>> = BTreeMap::new();
    for (node, neighbour) in links {
        if community_of.contains_key(node) {
            continue;
        }
        if let Some(&c) = community_of.get(neighbour) {
            *votes.entry(*node).or_default().entry(c).or_insert(0) += 1;
        }
    }
    let mut now = stored.clone();
    for (node, counts) in votes {
        // Most links first; on a tie the lowest community key (the map is
        // ordered, so the first maximum seen wins).
        let mut best: Option<(u64, usize)> = None;
        for (c, n) in counts {
            if best.is_none_or(|(_, m)| n > m) {
                best = Some((c, n));
            }
        }
        if let Some((c, _)) = best {
            now.entry(c).or_default().insert(node);
        }
    }
    now
}

/// One member as a summariser sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    /// Its `name`, else `title`, else `#id`.
    pub(crate) name: String,
    /// Its first label, `Node` when it has none.
    pub(crate) label: String,
    /// Its in-community PageRank.
    pub(crate) rank: f64,
}

/// What a summariser is given for one community.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummaryInput<'a> {
    /// How many members the community has.
    pub(crate) size: usize,
    /// The members, best-ranked first.
    pub(crate) members: &'a [Member],
    /// The community's keywords.
    pub(crate) keywords: &'a [String],
    /// The heaviest relationship types inside it, with their counts.
    pub(crate) rel_types: &'a [(String, usize)],
    /// How many members the summary may name.
    pub(crate) max_members: usize,
    /// The most tokens the summary may take.
    pub(crate) token_budget: usize,
}

/// Writes a community's summary.
pub(crate) trait CommunitySummarizer {
    /// The mode recorded on the communities it summarises.
    fn mode(&self) -> SummaryMode;
    /// The summary of one community.
    fn summarize(&self, input: &SummaryInput<'_>) -> Result<String, String>;
}

/// `node`'s name in a summary: its `name`, else `title`, else `#id`.
pub(crate) fn member_name(node: &crate::cypher::executor::NodeValue) -> String {
    ["name", "title"]
        .iter()
        .find_map(|key| match node.properties.get(*key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| format!("#{}", node.id))
}

/// `subgraph` as the global scope of `drevo.rag.context` writes it: each
/// `:Community` node named `community <id>` and described by its summary;
/// every other node as it is.
pub(crate) fn display_subgraph(subgraph: &crate::rag::Subgraph) -> crate::rag::Subgraph {
    let mut view = subgraph.clone();
    for ranked in &mut view.nodes {
        if !ranked.node.labels.iter().any(|l| l == COMMUNITY_LABEL) {
            continue;
        }
        let mut node = (*ranked.node).clone();
        let id = match node.properties.get("id") {
            Some(Value::String(id)) => id.clone(),
            _ => node.id.to_string(),
        };
        node.properties
            .insert("name".to_string(), Value::String(format!("community {id}")));
        match node.properties.get("summary").cloned() {
            Some(summary @ Value::String(_)) => {
                node.properties.insert("description".to_string(), summary);
            }
            _ => {
                node.properties.remove("description");
            }
        }
        ranked.node = std::sync::Arc::new(node);
    }
    view
}

/// `value` as a config map: `null` is empty.
fn config_map(value: &Value) -> Result<BTreeMap<String, Value>, String> {
    match value {
        Value::Null => Ok(BTreeMap::new()),
        Value::Map(map) => Ok(map.clone()),
        other => Err(format!(
            "the config must be a map, got {}",
            other.type_name()
        )),
    }
}

/// The error for an unknown key of a map accepting `keys`.
fn unknown(key: &str, keys: &str) -> String {
    format!("unknown config key `{key}`; expected {keys}")
}

/// A config value as an error message shows it.
fn describe(value: &Value) -> String {
    crate::cypher::provenance::describe(value)
}

/// `value` as a whole number of at least `min`, for the key `key`.
fn whole(key: &str, value: &Value, min: usize) -> Result<usize, String> {
    match value {
        Value::Integer(n) => usize::try_from(*n).ok().filter(|n| *n >= min),
        _ => None,
    }
    .ok_or_else(|| format!("{key} must be an integer ≥ {min}, got {}", describe(value)))
}

/// `value` as a finite number ≥ 0, for the key `key`.
fn non_negative(key: &str, value: &Value) -> Result<f64, String> {
    let n = match value {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    n.filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| format!("{key} must be a number ≥ 0, got {}", describe(value)))
}

/// `value` as a boolean, for the key `key`.
fn boolean(key: &str, value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(format!("{key} must be a boolean, got {}", describe(other))),
    }
}

/// `value` as a layer name.
fn layer(value: &Value) -> Result<String, String> {
    match value {
        Value::String(layer) if !layer.is_empty() => Ok(layer.clone()),
        other => Err(format!(
            "layer must be a non-empty string, got {}",
            describe(other)
        )),
    }
}

/// `value` as a list of strings, for the key `key`.
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use super::extractive::ExtractiveSummarizer;
    use super::*;
    use crate::cypher::executor::NodeValue;
    use crate::cypher::hybrid::HybridMode;
    use crate::rag::{RankedNode, Subgraph};

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    fn s(text: &str) -> Value {
        Value::String(text.to_string())
    }

    fn strings(items: &[&str]) -> Value {
        Value::List(items.iter().map(|i| s(i)).collect())
    }

    // ── summarize config ─────────────────────────────────────────────────

    #[test]
    fn summarize_defaults_are_the_documented_ones() {
        for value in [Value::Null, map(&[])] {
            let config = SummarizeConfig::from_value(&value).unwrap();
            assert_eq!(config, SummarizeConfig::default());
        }
        let d = SummarizeConfig::default();
        assert_eq!(d.algorithm, Algorithm::Louvain);
        assert_eq!(d.resolution, 1.0);
        assert_eq!((d.labels.clone(), d.rel_types.clone()), (None, None));
        assert_eq!((d.min_size, d.max_members), (3, 40));
        assert_eq!(d.mode, SummaryMode::Auto);
        assert_eq!(d.model, None);
        assert_eq!(d.token_budget, 300);
        assert!(d.replace);
        assert_eq!(d.layer, "default");
        assert!(!d.run_async);
    }

    #[test]
    fn every_summarize_key_is_read() {
        let config = SummarizeConfig::from_value(&map(&[
            ("algorithm", s("wcc")),
            ("resolution", Value::Integer(2)),
            ("labels", strings(&["Ticket", "Person"])),
            ("relTypes", strings(&["WORKS_ON"])),
            ("minSize", Value::Integer(2)),
            ("maxMembers", Value::Integer(5)),
            ("mode", s("extractive")),
            ("model", s("qwen2.5:14b-instruct")),
            ("tokenBudget", Value::Integer(120)),
            ("replace", Value::Bool(false)),
            ("layer", s("fine")),
            ("async", Value::Bool(true)),
        ]))
        .unwrap();
        assert_eq!(config.algorithm, Algorithm::Wcc);
        assert_eq!(config.resolution, 2.0);
        assert_eq!(
            config.labels,
            Some(vec!["Ticket".to_string(), "Person".to_string()])
        );
        assert_eq!(config.rel_types, Some(vec!["WORKS_ON".to_string()]));
        assert_eq!((config.min_size, config.max_members), (2, 5));
        assert_eq!(config.mode, SummaryMode::Extractive);
        assert_eq!(config.model.as_deref(), Some("qwen2.5:14b-instruct"));
        assert_eq!(config.token_budget, 120);
        assert!(!config.replace);
        assert_eq!(config.layer, "fine");
        assert!(config.run_async);
        let llm = SummarizeConfig::from_value(&map(&[("mode", s("llm"))])).unwrap();
        assert_eq!(llm.mode, SummaryMode::Llm);
        let float =
            SummarizeConfig::from_value(&map(&[("resolution", Value::Float(0.5))])).unwrap();
        assert_eq!(float.resolution, 0.5);
        let nulls = SummarizeConfig::from_value(&map(&[
            ("labels", Value::Null),
            ("model", Value::Null),
            ("layer", Value::Null),
        ]))
        .unwrap();
        assert_eq!(nulls, SummarizeConfig::default());
    }

    #[test]
    fn every_summarize_error_names_its_key() {
        let cases: Vec<(Vec<(&str, Value)>, &str)> = vec![
            (
                vec![("levels", Value::Integer(2))],
                "unknown config key `levels`; expected algorithm, resolution, labels, relTypes, \
                 minSize, maxMembers, mode, model, tokenBudget, replace, layer, async",
            ),
            (
                vec![("algorithm", s("leiden"))],
                "algorithm must be 'louvain' or 'wcc', got 'leiden'",
            ),
            (
                vec![("resolution", Value::Float(-1.0))],
                "resolution must be a number ≥ 0, got -1",
            ),
            (
                vec![("resolution", s("high"))],
                "resolution must be a number ≥ 0, got 'high'",
            ),
            (
                vec![("labels", s("Ticket"))],
                "labels must be a list of strings, got 'Ticket'",
            ),
            (
                vec![("relTypes", Value::List(vec![Value::Integer(1)]))],
                "relTypes must be a list of strings, got a list holding 1",
            ),
            (
                vec![("minSize", Value::Integer(0))],
                "minSize must be an integer ≥ 1, got 0",
            ),
            (
                vec![("maxMembers", Value::Integer(0))],
                "maxMembers must be an integer ≥ 1, got 0",
            ),
            (
                vec![("mode", s("abstractive"))],
                "mode must be 'auto', 'llm' or 'extractive', got 'abstractive'",
            ),
            (
                vec![("model", Value::Integer(3))],
                "model must be a string or null, got 3",
            ),
            (
                vec![("tokenBudget", Value::Integer(10))],
                "tokenBudget must be an integer ≥ 20, got 10",
            ),
            (
                vec![("replace", s("yes"))],
                "replace must be a boolean, got 'yes'",
            ),
            (
                vec![("layer", s(""))],
                "layer must be a non-empty string, got ''",
            ),
            (
                vec![("layer", Value::Integer(1))],
                "layer must be a non-empty string, got 1",
            ),
            (
                vec![("async", s("later"))],
                "async must be a boolean, got 'later'",
            ),
        ];
        for (entries, expected) in cases {
            let message = SummarizeConfig::from_value(&map(&entries)).unwrap_err();
            assert_eq!(message, expected, "{entries:?}");
        }
        assert_eq!(
            SummarizeConfig::from_value(&s("fast")).unwrap_err(),
            "the config must be a map, got String"
        );
    }

    #[test]
    fn the_default_partition_leaves_bookkeeping_out() {
        let config = SummarizeConfig::default();
        let labels = |l: &[&str]| l.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert!(config.admits_labels(&labels(&["Ticket"])));
        assert!(config.admits_labels(&labels(&[])));
        for bookkeeping in DEFAULT_EXCLUDED_LABELS {
            assert!(
                !config.admits_labels(&labels(&["Ticket", bookkeeping])),
                "{bookkeeping}"
            );
        }
        assert!(config.admits_rel_type("WORKS_ON"));
        assert!(!config.admits_rel_type(MEMBER_OF));
        assert!(!config.admits_rel_type(RELATED_COMMUNITY));

        let only = SummarizeConfig::from_value(&map(&[
            ("labels", strings(&["Ticket"])),
            ("relTypes", strings(&["BLOCKS"])),
        ]))
        .unwrap();
        assert!(only.admits_labels(&labels(&["Ticket", "Urgent"])));
        assert!(!only.admits_labels(&labels(&["Person"])));
        // Communities are never partitioned again, whatever `labels` says.
        let odd =
            SummarizeConfig::from_value(&map(&[("labels", strings(&["Community"]))])).unwrap();
        assert!(!odd.admits_labels(&labels(&["Community"])));
        assert!(only.admits_rel_type("BLOCKS"));
        assert!(!only.admits_rel_type("WORKS_ON"));
    }

    #[test]
    fn the_partition_scope_round_trips_through_json() {
        let scoped = SummarizeConfig::from_value(&map(&[
            ("labels", strings(&["Task"])),
            ("relTypes", strings(&["BLOCKS", "OWNS"])),
            ("layer", s("tasks")),
        ]))
        .unwrap();
        let json = scoped.scope_json();
        assert_eq!(json, r#"{"labels":["Task"],"relTypes":["BLOCKS","OWNS"]}"#);
        let back = SummarizeConfig::from_scope_json(Some(&json));
        assert_eq!(back.labels, scoped.labels);
        assert_eq!(back.rel_types, scoped.rel_types);
        let all = SummarizeConfig::default().scope_json();
        assert_eq!(all, r#"{"labels":null,"relTypes":null}"#);
        // Missing or unreadable: the defaults.
        for missing in [None, Some("not json"), Some("[1]")] {
            let d = SummarizeConfig::from_scope_json(missing);
            assert_eq!((d.labels, d.rel_types), (None, None), "{missing:?}");
        }
    }

    // ── query config ─────────────────────────────────────────────────────

    #[test]
    fn query_config_reads_its_keys_and_names_bad_ones() {
        assert_eq!(
            QueryConfig::from_value(&Value::Null).unwrap(),
            QueryConfig::default()
        );
        let config = QueryConfig::from_value(&map(&[
            ("layer", s("fine")),
            ("embeddingProperty", s("vec")),
            ("candidates", Value::Integer(50)),
            ("rrfK", Value::Integer(10)),
            ("mode", s("fts")),
        ]))
        .unwrap();
        assert_eq!(config.layer, "fine");
        assert_eq!(config.embedding_property, "vec");
        assert_eq!(config.hybrid.candidates, Some(50));
        assert_eq!(config.hybrid.rrf_k, 10.0);
        assert_eq!(config.hybrid.mode, HybridMode::Fts);
        let cases: Vec<(Vec<(&str, Value)>, &str)> = vec![
            (
                vec![("k", Value::Integer(2))],
                "unknown config key `k`; expected layer, embeddingProperty, candidates, rrfK, mode",
            ),
            (
                vec![("layer", s(""))],
                "layer must be a non-empty string, got ''",
            ),
            (
                vec![("embeddingProperty", Value::Bool(true))],
                "embeddingProperty must be a string, got Boolean",
            ),
            (
                vec![("mode", s("magic"))],
                "mode must be 'auto', 'hybrid', 'fts' or 'vector', got 'magic'",
            ),
        ];
        for (entries, expected) in cases {
            let message = QueryConfig::from_value(&map(&entries)).unwrap_err();
            assert!(message.starts_with(expected), "{entries:?}: {message}");
        }
        assert_eq!(
            QueryConfig::from_value(&Value::Integer(1)).unwrap_err(),
            "the config must be a map, got Integer"
        );
    }

    #[test]
    fn auto_mode_degrades_to_extractive_without_a_model() {
        use SummaryMode::{Auto, Extractive, Llm};
        assert_eq!(effective_mode(Auto, false), Ok(Extractive));
        assert_eq!(effective_mode(Auto, true), Ok(Llm));
        assert_eq!(effective_mode(Extractive, true), Ok(Extractive));
        assert_eq!(effective_mode(Llm, true), Ok(Llm));
        assert_eq!(effective_mode(Llm, false), Err(()));
        assert_eq!(Extractive.as_str(), "extractive");
        assert_eq!(Llm.as_str(), "llm");
        assert_eq!(Auto.as_str(), "auto");
    }

    // ── partition ────────────────────────────────────────────────────────

    /// Two triangles {1, 2, 3} and {4, 5, 6} plus 7 hanging off 6, and one
    /// bridge 3 → 4.
    fn two_teams() -> (Vec<u64>, Vec<(u64, u64, f32)>) {
        let edges = vec![
            (1, 2, 1.0),
            (2, 3, 1.0),
            (1, 3, 1.0),
            (4, 5, 1.0),
            (5, 6, 1.0),
            (4, 6, 1.0),
            (6, 7, 1.0),
            (3, 4, 1.0),
        ];
        (vec![1, 2, 3, 4, 5, 6, 7], edges)
    }

    #[test]
    fn louvain_finds_the_two_teams_largest_first() {
        let (nodes, edges) = two_teams();
        let p = partition(nodes, &edges, Algorithm::Louvain, 1.0).unwrap();
        assert_eq!(p.groups, vec![vec![4, 5, 6, 7], vec![1, 2, 3]]);
        assert!(p.modularity > 0.3, "{}", p.modularity);
        // The reported modularity is the partition's.
        let recomputed = modularity(&p.groups, &edges, 1.0);
        assert!(
            (p.modularity - recomputed).abs() < 1e-9,
            "{p:?} vs {recomputed}"
        );
    }

    #[test]
    fn wcc_keeps_connected_nodes_together() {
        let (mut nodes, edges) = two_teams();
        nodes.push(9);
        let p = partition(nodes, &edges, Algorithm::Wcc, 1.0).unwrap();
        assert_eq!(p.groups, vec![vec![1, 2, 3, 4, 5, 6, 7], vec![9]]);
        assert!((p.modularity - modularity(&p.groups, &edges, 1.0)).abs() < 1e-12);
    }

    #[test]
    fn partition_refuses_a_bad_resolution_and_handles_an_empty_graph() {
        let (nodes, edges) = two_teams();
        assert_eq!(
            partition(nodes, &edges, Algorithm::Louvain, f64::NAN).unwrap_err(),
            "resolution must be a number ≥ 0, got NaN"
        );
        let empty = partition(Vec::new(), &[], Algorithm::Louvain, 1.0).unwrap();
        assert!(empty.groups.is_empty());
        assert_eq!(empty.modularity, 0.0);
    }

    #[test]
    fn modularity_of_known_partitions() {
        // One edge, both ends in one community: L_c/m − (d_c/2m)² = 1 − 1 = 0.
        assert_eq!(modularity(&[vec![1, 2]], &[(1, 2, 1.0)], 1.0), 0.0);
        // Two disjoint edges, one community each: 2 × (1/2 − (2/4)²) = 0.5.
        let q = modularity(&[vec![1, 2], vec![3, 4]], &[(1, 2, 1.0), (3, 4, 1.0)], 1.0);
        assert!((q - 0.5).abs() < 1e-12, "{q}");
        // No edges at all: 0.
        assert_eq!(modularity(&[vec![1], vec![2]], &[], 1.0), 0.0);
    }

    #[test]
    fn the_hub_ranks_first_inside_its_community() {
        // 1 is linked to 2, 3 and 4; 2–3 too; 9 is outside and ignored.
        let edges = [
            (1, 2, 1.0),
            (1, 3, 1.0),
            (1, 4, 1.0),
            (2, 3, 1.0),
            (4, 9, 1.0),
            (9, 1, 1.0),
        ];
        let ranks = member_ranks(&[1, 2, 3, 4], &edges);
        assert_eq!(ranks.len(), 4);
        assert_eq!(ranks[0].0, 1);
        assert_eq!(ranks[3].0, 4);
        let total: f64 = ranks.iter().map(|(_, r)| r).sum();
        assert!((total - 1.0).abs() < 1e-6, "{total}");
        // Ties are broken by id: 2 and 3 are symmetric.
        assert_eq!((ranks[1].0, ranks[2].0), (2, 3));
        assert_eq!(member_ranks(&[1, 2, 3, 4], &edges), ranks);
    }

    #[test]
    fn related_weights_count_the_edges_between_communities() {
        let groups = vec![vec![1, 2], vec![3, 4], vec![5]];
        let edges = [
            (1, 2, 1.0),
            (2, 3, 1.0),
            (4, 1, 1.0),
            (3, 4, 1.0),
            (5, 4, 1.0),
            (7, 1, 1.0), // 7 is in no community
        ];
        assert_eq!(related_weights(&groups, &edges), vec![(0, 1, 2), (1, 2, 1)]);
        assert!(related_weights(&groups, &[(1, 2, 1.0)]).is_empty());
    }

    #[test]
    fn the_member_hash_ignores_order_and_sees_change() {
        let a = member_ids_hash(&[3, 1, 2]);
        assert_eq!(a, member_ids_hash(&[1, 2, 3]));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, member_ids_hash(&[1, 2, 3, 4]));
        assert_ne!(a, member_ids_hash(&[1, 2]));
        assert_ne!(member_ids_hash(&[]), member_ids_hash(&[0]));
    }

    #[test]
    fn community_ids_are_layer_and_index() {
        assert_eq!(community_id("default", 0), "default-0");
        assert_eq!(community_id("fine", 12), "fine-12");
    }

    #[test]
    fn heaviest_rel_types_by_count_then_name() {
        let kinds = [
            "WORKS_ON", "BLOCKS", "WORKS_ON", "MENTORS", "BLOCKS", "WORKS_ON", "ABOUT",
        ];
        assert_eq!(
            heaviest_rel_types(kinds, 3),
            vec![
                ("WORKS_ON".to_string(), 3),
                ("BLOCKS".to_string(), 2),
                ("ABOUT".to_string(), 1),
            ]
        );
        assert!(heaviest_rel_types([], 3).is_empty());
    }

    #[test]
    fn keywords_are_the_salient_terms_of_the_members() {
        let texts = vec![
            "Release the mobile app".to_string(),
            "Mobile release notes for the app store".to_string(),
            "Crash on startup".to_string(),
        ];
        // Every term in one document: tf decides, ties alphabetical.
        let df = |_: &[String]| Ok(1);
        let top = community_keywords(&texts, 3, 10, &df).unwrap();
        assert_eq!(top, ["app", "mobile", "release"]);
        let all = community_keywords(&texts, 10, 10, &df).unwrap();
        assert!(!all.contains(&"the".to_string()), "{all:?}");
        assert!(all.len() <= 10);
        assert_eq!(community_keywords(&texts, 10, 10, &df).unwrap(), all);
        assert!(community_keywords(&[], 10, 10, &df).unwrap().is_empty());
    }

    #[test]
    fn the_corpus_counts_documents_holding_every_trigram() {
        let corpus = TrigramCorpus::new([
            "Mobile release notes".to_string(),
            "iOS release build".to_string(),
            "Login token".to_string(),
        ]);
        assert_eq!(corpus.doc_count(), 3);
        let tri = |w: &str| crate::fts::tokenizer::trigrams(w);
        assert_eq!(corpus.df(&tri("release")), 2);
        assert_eq!(corpus.df(&tri("mobile")), 1);
        assert_eq!(corpus.df(&tri("zebra")), 0);
        assert_eq!(corpus.df(&[]), 0);
        // Keywords ranked over it: the rarer of two equally frequent terms wins.
        let texts = vec!["release mobile".to_string()];
        let top = community_keywords(&texts, 1, corpus.doc_count(), &|t| Ok(corpus.df(t))).unwrap();
        assert_eq!(top, ["mobile"]);
    }

    #[test]
    fn unassigned_nodes_join_the_community_they_link_into_most() {
        let stored = BTreeMap::from([
            (100, BTreeSet::from([1, 2, 3])),
            (200, BTreeSet::from([4, 5])),
        ]);
        // 7 links twice into 100 and once into 200; 8 once each (tie → 100);
        // 9 links only to another unassigned node.
        let links = [(7, 1), (7, 2), (7, 4), (8, 5), (8, 3), (9, 7)];
        let now = current_members(&stored, &links);
        assert_eq!(now[&100], BTreeSet::from([1, 2, 3, 7, 8]));
        assert_eq!(now[&200], BTreeSet::from([4, 5]));
        assert_eq!(current_members(&stored, &[]), stored);
    }

    // ── extractive summaries ─────────────────────────────────────────────

    fn member(_id: u64, name: &str, label: &str, rank: f64) -> Member {
        Member {
            name: name.to_string(),
            label: label.to_string(),
            rank,
        }
    }

    fn backend() -> Vec<Member> {
        vec![
            member(1, "Alice", "Person", 0.3),
            member(4, "BE-142 Login token expires early", "Ticket", 0.2),
            member(2, "Bob", "Person", 0.15),
            member(5, "BE-101 API rate limiter", "Ticket", 0.15),
            member(3, "Carol", "Person", 0.1),
            member(6, "BE-150 Database migration", "Ticket", 0.1),
        ]
    }

    fn input<'a>(
        members: &'a [Member],
        keywords: &'a [String],
        rels: &'a [(String, usize)],
        max_members: usize,
        token_budget: usize,
    ) -> SummaryInput<'a> {
        SummaryInput {
            size: members.len(),
            members,
            keywords,
            rel_types: rels,
            max_members,
            token_budget,
        }
    }

    #[test]
    fn an_extractive_summary_groups_members_by_label_with_keywords_and_relations() {
        let members = backend();
        let keywords = vec!["token".to_string(), "api".to_string()];
        let rels = vec![("WORKS_ON".to_string(), 7), ("MENTORS".to_string(), 2)];
        let text = ExtractiveSummarizer
            .summarize(&input(&members, &keywords, &rels, 40, 300))
            .unwrap();
        assert_eq!(
            text,
            "6 members. Person: Alice, Bob, Carol. Ticket: BE-142 Login token expires early, \
             BE-101 API rate limiter, BE-150 Database migration. Keywords: token, api. \
             Relations: WORKS_ON (7), MENTORS (2)."
        );
        assert_eq!(ExtractiveSummarizer.mode(), SummaryMode::Extractive);
        // Deterministic.
        let again = ExtractiveSummarizer
            .summarize(&input(&members, &keywords, &rels, 40, 300))
            .unwrap();
        assert_eq!(again, text);
    }

    #[test]
    fn an_extractive_summary_names_at_most_max_members_best_first() {
        let members = backend();
        let text = ExtractiveSummarizer
            .summarize(&input(&members, &[], &[], 2, 300))
            .unwrap();
        assert_eq!(
            text,
            "6 members. Person: Alice. Ticket: BE-142 Login token expires early."
        );
    }

    #[test]
    fn an_extractive_summary_stays_within_its_budget() {
        let many: Vec<Member> = (0..40)
            .map(|i| {
                member(
                    i,
                    &format!("Ticket number {i} with a long title"),
                    "Ticket",
                    1.0 / (f64::from(i as u32) + 1.0),
                )
            })
            .collect();
        let keywords: Vec<String> = (0..10).map(|i| format!("keyword{i}")).collect();
        let rels = vec![("BLOCKS".to_string(), 30)];
        for budget in [20, 40, 80, 300] {
            let text = ExtractiveSummarizer
                .summarize(&input(&many, &keywords, &rels, 40, budget))
                .unwrap();
            let tokens = crate::rag::tokens::approx(&text);
            assert!(tokens <= budget, "{budget}: {tokens} tokens: {text}");
            assert!(text.starts_with("40 members."), "{text}");
        }
        // A bigger budget names more members, best first.
        let small = ExtractiveSummarizer
            .summarize(&input(&many, &keywords, &rels, 40, 80))
            .unwrap();
        let big = ExtractiveSummarizer
            .summarize(&input(&many, &keywords, &rels, 40, 300))
            .unwrap();
        assert!(small.contains("Ticket number 0 with"), "{small}");
        assert!(big.len() > small.len());
        assert!(big.contains("Ticket number 1 with"), "{big}");
    }

    #[test]
    fn member_names_come_from_name_then_title() {
        let node = |props: &[(&str, &str)]| NodeValue {
            id: 42,
            uuid: [0; 16],
            labels: vec!["Ticket".to_string()],
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), s(v)))
                .collect(),
        };
        assert_eq!(
            member_name(&node(&[("name", "Alice"), ("title", "x")])),
            "Alice"
        );
        assert_eq!(member_name(&node(&[("title", "BE-1 Crash")])), "BE-1 Crash");
        assert_eq!(member_name(&node(&[("body", "text")])), "#42");
    }

    #[test]
    fn the_global_view_names_communities_and_describes_them_by_summary() {
        let community = NodeValue {
            id: 7,
            uuid: [0; 16],
            labels: vec!["Community".to_string()],
            properties: BTreeMap::from([
                ("id".to_string(), s("default-0")),
                ("summary".to_string(), s("6 members. Person: Alice.")),
            ]),
        };
        let alice = NodeValue {
            id: 1,
            uuid: [0; 16],
            labels: vec!["Person".to_string()],
            properties: BTreeMap::from([("name".to_string(), s("Alice"))]),
        };
        let subgraph = Subgraph {
            nodes: vec![
                RankedNode::seed(Arc::new(community), 0.03),
                RankedNode {
                    seed_score: None,
                    distance: 1,
                    ppr_score: Some(0.3),
                    via: None,
                    node: Arc::new(alice.clone()),
                },
            ],
            rels: Vec::new(),
            truncated: false,
        };
        let view = display_subgraph(&subgraph);
        let shown = &view.nodes[0].node;
        assert_eq!(shown.id, 7);
        assert_eq!(
            shown.properties.get("name"),
            Some(&s("community default-0"))
        );
        assert_eq!(
            shown.properties.get("description"),
            Some(&s("6 members. Person: Alice."))
        );
        assert_eq!(*view.nodes[1].node, alice);
        assert_eq!(view.nodes[0].seed_score, Some(0.03));
        let text =
            crate::rag::serialize::render(&view, &crate::rag::serialize::RenderOptions::default())
                .text;
        assert!(
            text.starts_with(
                "## Entities\n- **community default-0** (Community): 6 members. Person: Alice.\n"
            ),
            "{text}"
        );
    }
}
