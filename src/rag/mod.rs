//! Graph-RAG retrieval: a question in, the part of the graph that answers it
//! out — as ranked rows (`drevo.rag.retrieve`) or as LLM-ready text within a
//! token budget (`drevo.rag.context`, also `POST /rag/context`).
//!
//! The pipeline runs inside one statement:
//!
//! 1. **Seeds** — the hybrid BM25 + vector search of `drevo.search.hybrid`.
//! 2. **Expand** — the breadth-first walk of `drevo.graph.expand` from the
//!    seeds.
//! 3. **Edges** — the relationships between the nodes found.
//! 4. **Rank** — seeds first, then Personalized PageRank over just those
//!    nodes and relationships ([`personalized_scores`](crate::rag::personalized_scores)), or distance, or the
//!    walk's own order ([`order`](crate::rag::order)).
//! 5. **Serialise** — [`serialize::render`](crate::rag::serialize::render) fills the budget best-ranked
//!    first.
//!
//! The stages after the graph reads are pure functions over a [`Subgraph`](crate::rag::Subgraph),
//! so they are tested without an engine:
//!
//! ```
//! use std::collections::BTreeMap;
//! use std::sync::Arc;
//! use drevo::cypher::executor::NodeValue;
//! use drevo::rag::config::RankMode;
//! use drevo::rag::{order, RankedNode};
//!
//! let node = |id: u64| Arc::new(NodeValue {
//!     id,
//!     uuid: [0; 16],
//!     labels: vec!["Note".to_string()],
//!     properties: BTreeMap::new(),
//! });
//! let reached = |id: u64, distance: usize| RankedNode {
//!     seed_score: None,
//!     distance,
//!     ..RankedNode::seed(node(id), 0.0)
//! };
//! let seed = RankedNode::seed(node(5), 0.9);
//! let ranked = order(vec![seed, reached(3, 2), reached(7, 1)], RankMode::Distance);
//! let ids: Vec<u64> = ranked.iter().map(|n| n.node.id).collect();
//! assert_eq!(ids, [5, 7, 3]);
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::algorithms::{oriented_edges, pagerank, AdjacencyList};
use crate::cypher::executor::{NodeValue, RelationshipValue, Value};
use crate::cypher::pagerank_config::PageRankOptions;
use crate::traversal::ExpandVia;

pub mod config;
pub mod eval;
pub mod serialize;
pub mod tokens;
pub(crate) mod trace;

use config::RankMode;

/// One node of a retrieved subgraph with what ranked it.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedNode {
    /// The node, as the caller may see it.
    pub node: Arc<NodeValue>,
    /// Its fused search score when it is a seed; `None` otherwise.
    pub seed_score: Option<f64>,
    /// How many relationships away from the nearest seed it is; `0` for a
    /// seed.
    pub distance: usize,
    /// Its Personalized PageRank score when ranked by `'ppr'`.
    pub ppr_score: Option<f64>,
    /// The relationship that first reached it; `None` for a seed.
    pub via: Option<ExpandVia>,
}

impl RankedNode {
    /// A seed found with search score `score`.
    #[must_use]
    pub fn seed(node: Arc<NodeValue>, score: f64) -> Self {
        Self {
            node,
            seed_score: Some(score),
            distance: 0,
            ppr_score: None,
            via: None,
        }
    }
}

/// The nodes a question retrieved, best first, and the relationships
/// between them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Subgraph {
    /// The nodes in rank order.
    pub nodes: Vec<RankedNode>,
    /// The relationships whose two ends are both in [`nodes`](Self::nodes),
    /// by id.
    pub rels: Vec<Arc<RelationshipValue>>,
    /// Whether the expansion stopped at its node cap with more to return.
    pub truncated: bool,
}

/// `nodes` (seeds first, as the expansion returns them) in rank order: the
/// seeds keep their search order and come first; the rest follow by `mode` —
/// best PageRank score, nearest, or as given — ties broken by node id.
#[must_use]
pub fn order(nodes: Vec<RankedNode>, mode: RankMode) -> Vec<RankedNode> {
    let (mut ranked, mut rest): (Vec<RankedNode>, Vec<RankedNode>) =
        nodes.into_iter().partition(|n| n.seed_score.is_some());
    match mode {
        RankMode::Ppr => rest.sort_by(|a, b| {
            let score = |n: &RankedNode| n.ppr_score.unwrap_or(0.0);
            score(b)
                .total_cmp(&score(a))
                .then(a.node.id.cmp(&b.node.id))
        }),
        RankMode::Distance => rest.sort_by(|a, b| {
            let score = |n: &RankedNode| n.seed_score.unwrap_or(f64::NEG_INFINITY);
            a.distance
                .cmp(&b.distance)
                .then(score(b).total_cmp(&score(a)))
                .then(a.node.id.cmp(&b.node.id))
        }),
        RankMode::None => {}
    }
    ranked.append(&mut rest);
    ranked
}

/// Personalized PageRank over the subgraph of `node_ids` and the `edges`
/// (`from, to, weight`) between them, teleporting to `seeds` in proportion
/// to their weights; `options` gives the damping, iterations, tolerance and
/// direction. Edges with an end outside `node_ids` are ignored, so nothing
/// outside the subgraph moves a score. Every id gets a score; they sum to 1.
///
/// # Errors
///
/// When `seeds` is empty or names a node outside `node_ids`, or every
/// weight is zero.
pub fn personalized_scores(
    node_ids: &[u64],
    edges: &[(u64, u64, f32)],
    seeds: &[(u64, f64)],
    options: &PageRankOptions,
) -> Result<HashMap<u64, f64>, String> {
    if seeds.is_empty() {
        return Err("sourceNodes must name at least one node".to_string());
    }
    let inside: HashSet<u64> = node_ids.iter().copied().collect();
    if let Some((outside, _)) = seeds.iter().find(|(id, _)| !inside.contains(id)) {
        return Err(format!("seed {outside} is not in the expanded subgraph"));
    }
    let edges = edges
        .iter()
        .filter(|(from, to, _)| inside.contains(from) && inside.contains(to))
        .copied();
    let graph =
        AdjacencyList::from_parts(node_ids.to_vec(), oriented_edges(edges, options.direction));
    let options = PageRankOptions {
        source_weights: Some(seeds.iter().map(|&(_, w)| w).collect()),
        ..options.clone()
    };
    let personalization = options.personalization(seeds.iter().map(|&(id, _)| id).collect())?;
    let result = pagerank(&graph, &options.algorithm_config(Some(personalization)));
    Ok(result.ranked().into_iter().collect())
}

/// Whether an element with `props` passes a `source` filter: always without
/// one, else when it has no `source_id` or its `source_id` (a string, or a
/// list of strings) names one of `sources`.
#[must_use]
pub fn source_admits(props: &BTreeMap<String, Value>, sources: Option<&[String]>) -> bool {
    let Some(sources) = sources else {
        return true;
    };
    match props.get("source_id") {
        Some(Value::String(id)) => sources.contains(id),
        Some(Value::List(ids)) => ids
            .iter()
            .any(|id| matches!(id, Value::String(id) if sources.contains(id))),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::pagerank_config::PageRankProcedure;
    use crate::model::Direction;

    fn node(id: u64) -> Arc<NodeValue> {
        Arc::new(NodeValue {
            id,
            uuid: [0; 16],
            labels: vec!["Note".to_string()],
            properties: BTreeMap::new(),
        })
    }

    fn reached(id: u64, distance: usize, ppr: Option<f64>) -> RankedNode {
        RankedNode {
            node: node(id),
            seed_score: None,
            distance,
            ppr_score: ppr,
            via: None,
        }
    }

    fn ids(nodes: &[RankedNode]) -> Vec<u64> {
        nodes.iter().map(|n| n.node.id).collect()
    }

    /// Two seeds (9 found first, then 4) and four reached nodes in walk
    /// order.
    fn found() -> Vec<RankedNode> {
        vec![
            RankedNode {
                ppr_score: Some(0.1),
                ..RankedNode::seed(node(9), 0.03)
            },
            RankedNode {
                ppr_score: Some(0.4),
                ..RankedNode::seed(node(4), 0.02)
            },
            reached(8, 1, Some(0.05)),
            reached(2, 1, Some(0.2)),
            reached(6, 2, Some(0.2)),
            reached(1, 2, Some(0.05)),
        ]
    }

    #[test]
    fn seeds_come_first_in_search_order_in_every_mode() {
        for mode in [RankMode::Ppr, RankMode::Distance, RankMode::None] {
            assert_eq!(ids(&order(found(), mode))[..2], [9, 4], "{mode:?}");
        }
    }

    #[test]
    fn ppr_orders_by_score_then_id() {
        assert_eq!(ids(&order(found(), RankMode::Ppr)), [9, 4, 2, 6, 1, 8]);
    }

    #[test]
    fn distance_orders_by_distance_then_id() {
        assert_eq!(ids(&order(found(), RankMode::Distance)), [9, 4, 2, 8, 1, 6]);
    }

    #[test]
    fn none_keeps_the_walk_order() {
        assert_eq!(ids(&order(found(), RankMode::None)), [9, 4, 8, 2, 6, 1]);
    }

    fn ppr_options() -> PageRankOptions {
        PageRankOptions {
            direction: Direction::Both,
            ..PageRankOptions::defaults(PageRankProcedure::Personalized)
        }
    }

    #[test]
    fn ppr_is_restricted_to_the_subgraph() {
        // 1 - 2 - 3 inside; 99 is a hub outside, linked to 2 and 3.
        let inside = [(1, 2, 1.0_f32), (2, 3, 1.0)];
        let with_hub = [
            (1, 2, 1.0_f32),
            (2, 3, 1.0),
            (99, 2, 1.0),
            (99, 3, 1.0),
            (2, 99, 1.0),
        ];
        let seeds = [(1, 1.0)];
        let alone = personalized_scores(&[1, 2, 3], &inside, &seeds, &ppr_options()).unwrap();
        let hubbed = personalized_scores(&[1, 2, 3], &with_hub, &seeds, &ppr_options()).unwrap();
        assert_eq!(alone, hubbed);
        assert!(!hubbed.contains_key(&99));
        assert_eq!(hubbed.len(), 3);
        let total: f64 = hubbed.values().sum();
        assert!((total - 1.0).abs() < 1e-6, "{total}");
        // The far end of the path, away from the seed, scores least.
        assert!(
            hubbed[&1] > hubbed[&3] && hubbed[&2] > hubbed[&3],
            "{hubbed:?}"
        );
    }

    #[test]
    fn ppr_weights_the_seeds() {
        let edges = [(1, 2, 1.0_f32), (3, 4, 1.0)];
        let heavy_one =
            personalized_scores(&[1, 2, 3, 4], &edges, &[(1, 0.9), (3, 0.1)], &ppr_options())
                .unwrap();
        assert!(heavy_one[&1] > heavy_one[&3], "{heavy_one:?}");
        assert!(heavy_one[&2] > heavy_one[&4], "{heavy_one:?}");
    }

    #[test]
    fn ppr_refuses_no_seeds_and_outside_seeds() {
        assert_eq!(
            personalized_scores(&[1, 2], &[], &[], &ppr_options()),
            Err("sourceNodes must name at least one node".to_string())
        );
        assert_eq!(
            personalized_scores(&[1, 2], &[], &[(7, 1.0)], &ppr_options()),
            Err("seed 7 is not in the expanded subgraph".to_string())
        );
    }

    fn props(source: Option<Value>) -> BTreeMap<String, Value> {
        source
            .map(|s| BTreeMap::from([("source_id".to_string(), s)]))
            .unwrap_or_default()
    }

    #[test]
    fn source_filter_keeps_elements_without_provenance() {
        let wanted = ["journal-1".to_string()];
        let s = |t: &str| Value::String(t.to_string());
        assert!(source_admits(&props(None), Some(&wanted)));
        assert!(source_admits(&props(Some(s("journal-1"))), Some(&wanted)));
        assert!(!source_admits(&props(Some(s("journal-2"))), Some(&wanted)));
        assert!(source_admits(
            &props(Some(Value::List(vec![s("x"), s("journal-1")]))),
            Some(&wanted)
        ));
        assert!(!source_admits(
            &props(Some(Value::List(vec![s("x")]))),
            Some(&wanted)
        ));
        assert!(source_admits(&props(Some(s("journal-2"))), None));
    }
}
