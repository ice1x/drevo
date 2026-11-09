//! What a `:ContextTrace` records about one `drevo.rag.context` call: which
//! nodes went into the text (`INCLUDED`, with what ranked them and the tokens
//! each took), which were retrieved but left out (`SKIPPED`, with why), and
//! when traces are old enough to prune.
//!
//! The executor writes the trace through its regular `CREATE` path; this
//! module holds the pure parts, tested without an engine.

use std::collections::HashMap;
use std::sync::Arc;

use crate::cypher::executor::{NodeValue, Value};
use crate::rag::serialize::Rendered;
use crate::rag::{RankedNode, Subgraph};

/// The label of a context trace node.
pub(crate) const TRACE_LABEL: &str = "ContextTrace";

/// The relationship from a trace to a node written into its text.
pub(crate) const INCLUDED: &str = "INCLUDED";

/// The relationship from a trace to a node retrieved but not written.
pub(crate) const SKIPPED: &str = "SKIPPED";

/// One day in milliseconds.
#[cfg(any(test, feature = "http"))]
pub(crate) const MILLIS_PER_DAY: i64 = 86_400_000;

/// Why a retrieved node was left out of the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// It did not fit the token budget.
    Budget,
    /// The expansion reached it after it had already returned `maxNodes`.
    MaxNodes,
}

impl SkipReason {
    /// The reason as stored on a `SKIPPED` relationship.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::MaxNodes => "maxNodes",
        }
    }
}

/// One node a trace links to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraceEntry {
    /// The node.
    pub(crate) node: Arc<NodeValue>,
    /// Its place in the trace, from 1: the written nodes in the order
    /// written, then the skipped ones in rank order.
    pub(crate) rank: usize,
    /// `None` when written; why it was left out otherwise.
    pub(crate) skipped: Option<SkipReason>,
    /// How far from the nearest seed it is.
    pub(crate) distance: usize,
    /// Its search score when it is a seed.
    pub(crate) seed_score: Option<f64>,
    /// Its Personalized PageRank score when ranked by `'ppr'`.
    pub(crate) ppr_score: Option<f64>,
    /// The tokens it added to the text; `0` when skipped.
    pub(crate) tokens: usize,
}

/// The nodes a trace links to: those `rendered` wrote, then those it
/// skipped, then `cut` — the node the expansion stopped at — when there is
/// one.
pub(crate) fn entries(
    subgraph: &Subgraph,
    rendered: &Rendered,
    cut: Option<&Arc<NodeValue>>,
) -> Vec<TraceEntry> {
    let by_id: HashMap<u64, &RankedNode> = subgraph.nodes.iter().map(|n| (n.node.id, n)).collect();
    let entry = |id: &u64, tokens: usize, skipped: Option<SkipReason>| {
        by_id.get(id).map(|n| TraceEntry {
            node: Arc::clone(&n.node),
            rank: 0,
            skipped,
            distance: n.distance,
            seed_score: n.seed_score,
            ppr_score: n.ppr_score,
            tokens,
        })
    };
    let written = rendered
        .node_ids
        .iter()
        .zip(&rendered.node_tokens)
        .filter_map(|(id, tokens)| entry(id, *tokens, None));
    let skipped = rendered
        .skipped_ids
        .iter()
        .filter_map(|id| entry(id, 0, Some(SkipReason::Budget)));
    let stopped = cut.map(|node| TraceEntry {
        node: Arc::clone(node),
        rank: 0,
        skipped: Some(SkipReason::MaxNodes),
        distance: 0,
        seed_score: None,
        ppr_score: None,
        tokens: 0,
    });
    written
        .chain(skipped)
        .chain(stopped)
        .enumerate()
        .map(|(i, e)| TraceEntry { rank: i + 1, ..e })
        .collect()
}

/// The layout of a UTC date: `YYYY-MM-DD`.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// Whether `text` is a UTC date, or a date and a time ending in `Z`.
fn is_utc_instant(text: &str) -> bool {
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    is_date(date)
        && time.is_none_or(|t| {
            t.strip_suffix('Z').is_some_and(|clock| {
                clock.len() >= 5
                    && clock
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
            })
        })
}

/// `value` as a UTC cut-off for pruning: a date (`2025-01-01`) or a
/// timestamp ending in `Z` (`2025-01-01T12:00:00Z`).
pub(crate) fn parse_cutoff(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) if is_utc_instant(s) => Ok(s.clone()),
        other => {
            let got = match other {
                Value::String(s) => format!("'{s}'"),
                v => v.type_name().to_string(),
            };
            Err(format!(
                "olderThan must be an ISO-8601 UTC date or timestamp \
                 ('2025-01-01' or '2025-01-01T00:00:00Z'), got {got}"
            ))
        }
    }
}

/// Whether a trace created at `created_at` is older than `cutoff`. Both are
/// ISO-8601 UTC strings, whose fixed layout orders them in time; a
/// `created_at` that is not one is never older.
pub(crate) fn is_older(created_at: &str, cutoff: &str) -> bool {
    is_utc_instant(created_at) && created_at < cutoff
}

/// The cut-off that keeps `days` days of traces at `now_ms` (the server's
/// `DREVO_RAG_TRACE_RETENTION_DAYS`).
#[cfg(any(test, feature = "http"))]
pub(crate) fn retention_cutoff(now_ms: i64, days: u64) -> String {
    let span = i64::try_from(days)
        .ok()
        .and_then(|d| d.checked_mul(MILLIS_PER_DAY))
        .unwrap_or(i64::MAX);
    crate::cypher::executor::iso8601_utc(now_ms.saturating_sub(span).max(0))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::rag::RankedNode;

    fn node(id: u64) -> Arc<NodeValue> {
        Arc::new(NodeValue {
            id,
            uuid: [0; 16],
            labels: vec!["Note".to_string()],
            properties: BTreeMap::new(),
        })
    }

    fn reached(id: u64, distance: usize, ppr: f64) -> RankedNode {
        RankedNode {
            seed_score: None,
            distance,
            ppr_score: Some(ppr),
            ..RankedNode::seed(node(id), 0.0)
        }
    }

    /// A seed (1) and three reached nodes, of which 3 did not fit.
    fn retrieved() -> (Subgraph, Rendered) {
        let subgraph = Subgraph {
            nodes: vec![
                RankedNode {
                    ppr_score: Some(0.5),
                    ..RankedNode::seed(node(1), 0.03)
                },
                reached(2, 1, 0.3),
                reached(3, 1, 0.15),
                reached(4, 2, 0.05),
            ],
            rels: Vec::new(),
            truncated: true,
        };
        let rendered = Rendered {
            node_ids: vec![1, 2, 4],
            node_tokens: vec![12, 7, 5],
            tokens: 24,
            skipped: 1,
            skipped_ids: vec![3],
            ..Rendered::default()
        };
        (subgraph, rendered)
    }

    #[test]
    fn written_nodes_rank_first_in_written_order_then_the_skipped() {
        let (subgraph, rendered) = retrieved();
        let cut = node(9);
        let entries = entries(&subgraph, &rendered, Some(&cut));
        let summary: Vec<(u64, usize, Option<SkipReason>, usize)> = entries
            .iter()
            .map(|e| (e.node.id, e.rank, e.skipped, e.tokens))
            .collect();
        assert_eq!(
            summary,
            [
                (1, 1, None, 12),
                (2, 2, None, 7),
                (4, 3, None, 5),
                (3, 4, Some(SkipReason::Budget), 0),
                (9, 5, Some(SkipReason::MaxNodes), 0),
            ]
        );
        assert_eq!(entries[0].seed_score, Some(0.03));
        assert_eq!(entries[0].ppr_score, Some(0.5));
        assert_eq!(entries[0].distance, 0);
        assert_eq!(entries[2].seed_score, None);
        assert_eq!(entries[2].distance, 2);
        assert_eq!(entries[3].distance, 1);
        let written: usize = entries.iter().map(|e| e.tokens).sum();
        assert_eq!(written, rendered.tokens);
    }

    #[test]
    fn nothing_retrieved_gives_no_entries() {
        assert!(entries(&Subgraph::default(), &Rendered::default(), None).is_empty());
    }

    #[test]
    fn skip_reasons_have_their_stored_names() {
        assert_eq!(SkipReason::Budget.as_str(), "budget");
        assert_eq!(SkipReason::MaxNodes.as_str(), "maxNodes");
    }

    #[test]
    fn a_cutoff_is_a_utc_date_or_timestamp() {
        let s = |t: &str| Value::String(t.to_string());
        for ok in [
            "2025-01-01",
            "2025-01-01T12:00:00Z",
            "2025-01-01T12:00:00.123Z",
        ] {
            assert_eq!(parse_cutoff(&s(ok)), Ok(ok.to_string()));
        }
        let message = |got: &str| {
            format!(
                "olderThan must be an ISO-8601 UTC date or timestamp \
                 ('2025-01-01' or '2025-01-01T00:00:00Z'), got {got}"
            )
        };
        for bad in [
            "yesterday",
            "2025-1-01",
            "2025-01-01T12:00:00+02:00",
            "2025-01-01 12:00",
            "",
        ] {
            assert_eq!(parse_cutoff(&s(bad)), Err(message(&format!("'{bad}'"))));
        }
        assert_eq!(parse_cutoff(&Value::Integer(5)), Err(message("Integer")));
        assert_eq!(parse_cutoff(&Value::Null), Err(message("Null")));
    }

    #[test]
    fn older_compares_instants() {
        assert!(is_older("2024-12-31T23:59:59.999Z", "2025-01-01"));
        assert!(!is_older("2025-01-01T00:00:00.000Z", "2025-01-01"));
        assert!(!is_older("2025-03-01T00:00:00.000Z", "2025-01-01"));
        assert!(is_older("2025-01-01T11:59:59.000Z", "2025-01-01T12:00:00Z"));
        assert!(!is_older(
            "2025-01-01T12:00:01.000Z",
            "2025-01-01T12:00:00Z"
        ));
        // Something that is not a timestamp is never pruned.
        assert!(!is_older("", "2025-01-01"));
        assert!(!is_older("soon", "2025-01-01"));
    }

    #[test]
    fn retention_keeps_the_last_days() {
        // 2025-01-10T00:00:00Z.
        let now = 1_736_467_200_000;
        assert_eq!(retention_cutoff(now, 1), "2025-01-09T00:00:00.000Z");
        assert_eq!(retention_cutoff(now, 9), "2025-01-01T00:00:00.000Z");
        assert!(is_older(
            "2025-01-08T23:00:00.000Z",
            &retention_cutoff(now, 1)
        ));
        assert!(!is_older(
            "2025-01-09T01:00:00.000Z",
            &retention_cutoff(now, 1)
        ));
        // An absurd retention clamps to the epoch rather than overflowing.
        assert_eq!(retention_cutoff(now, u64::MAX), "1970-01-01T00:00:00.000Z");
    }
}
