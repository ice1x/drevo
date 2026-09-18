//! An in-memory full-text index that tails a
//! [`NativeGraph`](crate::native::NativeGraph)'s change-feed
//! (RFC `docs/rfc-native-core.md`, #307, Phase 6.3).
//!
//! The native graph engine keeps only the core graph (nodes, edges, adjacency);
//! secondary indexes live off it and stay current by **tailing the change-feed**
//! rather than coupling to the write path (see
//! [`NativeGraph::changes_since`](crate::native::NativeGraph::changes_since)).
//! This is the first such consumer: a trigram BM25 index, matching the KV
//! store's full-text semantics (`k1 = 1.2`, `b = 0.75`, IDF
//! `ln(1 + (N − df + 0.5) / (df + 0.5))`, over each node's title + body + string
//! properties) so `fts.search` can be answered on the native engine.
//!
//! # Usage
//!
//! Snapshot-then-tail: build the index, then
//! [`sync`](crate::native_fts::NativeFtsIndex::sync) periodically (or after each
//! batch of writes). `sync` applies every change
//! since the last cursor; if the feed was trimmed past the cursor it transparently
//! rebuilds from a fresh snapshot.
//!
//! ```
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_fts::NativeFtsIndex;
//! use drevo_core::engine::GraphEngine; // brings `create_node` into scope
//! use drevo_core::model::NewNode;
//!
//! # fn main() -> drevo_core::error::Result<()> {
//! let g = NativeGraph::new();
//! g.create_node(NewNode { kind: "doc".into(), title: "the quick brown fox".into(),
//!     body: String::new(), body_html: String::new(), properties: Default::default() })?;
//!
//! let mut fts = NativeFtsIndex::new();
//! fts.sync(&g);
//! let hits = fts.search("quick", 10);
//! assert_eq!(hits.len(), 1);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;

use crate::bm25::bm25_idf;
use crate::engine::GraphEngine;
use crate::model::{Edge, Node, Properties};
use crate::native::{NativeGraph, WalOp};
use crate::tokenizer;

/// BM25 term-frequency saturation, matching the KV store's `FtsRanking::default`.
const K1: f32 = 1.2;
/// BM25 length-normalisation, matching the KV store's `FtsRanking::default`.
const B: f32 = 0.75;

/// A trigram BM25 full-text index maintained by tailing a [`NativeGraph`]'s
/// change-feed. See the module docs.
#[derive(Default)]
pub struct NativeFtsIndex {
    /// trigram → (node id → term frequency in that node).
    postings: HashMap<String, HashMap<u64, u32>>,
    /// node id → its trigram frequencies (the forward index, so a node can be
    /// removed or re-indexed without scanning every posting list).
    docs: HashMap<u64, HashMap<String, u32>>,
    /// node id → document length (total trigram occurrences), for BM25.
    doc_len: HashMap<u64, u32>,
    /// Sum of every document length, so `avgdl = total_len / docs.len()`.
    total_len: u64,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl NativeFtsIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this index has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of nodes currently indexed.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The number of indexed documents (corpus `N`) — the IDF denominator for
    /// keyword extraction (`drevo.keywords`, #447 native port).
    pub fn doc_count(&self) -> u64 {
        self.docs.len() as u64
    }

    /// Estimate a term's document frequency from its trigrams: the number of
    /// indexed docs containing **all** of them. Mirrors the KV FTS
    /// `intersect_trigrams(...).len()` used by keyword extraction. An empty
    /// trigram set, or any trigram absent from the corpus, yields 0.
    pub fn trigram_df(&self, term_trigrams: &[String]) -> u64 {
        if term_trigrams.is_empty() {
            return 0;
        }
        // Gather each trigram's posting list; a missing trigram means no doc can
        // contain all of them.
        let mut lists: Vec<&HashMap<u64, u32>> = Vec::with_capacity(term_trigrams.len());
        for tg in term_trigrams {
            match self.postings.get(tg) {
                Some(list) => lists.push(list),
                None => return 0,
            }
        }
        // Intersect from the rarest list outward.
        lists.sort_by_key(|l| l.len());
        let Some((first, rest)) = lists.split_first() else {
            return 0;
        };
        first
            .keys()
            .filter(|id| rest.iter().all(|l| l.contains_key(*id)))
            .count() as u64
    }

    /// Bring the index up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor).
    ///
    /// If the feed was trimmed past this index's cursor (a `lagged` batch), the
    /// index is rebuilt from a fresh snapshot of every node — the standard
    /// re-snapshot recovery for a consumer that fell behind the retention window.
    pub fn sync(&mut self, graph: &NativeGraph) {
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => self.remove_node(id),
                // Edges carry no full-text content; embeddings are a separate store.
                WalOp::UpsertEdge(_)
                | WalOp::DeleteEdge(_)
                | WalOp::SetEmbedding(..)
                | WalOp::DeleteEmbedding(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }

    /// Search for `query`, returning up to `limit` `(node_id, score)` pairs
    /// ranked by descending BM25 score.
    ///
    /// The query is trigram-tokenised the same way documents are, and each
    /// query trigram contributes its BM25 weight to every node whose text
    /// contains it — so a longer, more specific query concentrates score on the
    /// nodes that match the most of it.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(u64, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        let n = self.docs.len() as u64;
        if n == 0 {
            return Vec::new();
        }
        let avgdl = self.total_len as f32 / n as f32;

        // Query trigrams, normalised + deduped exactly as the KV store does
        // (`extract_trigrams`), so candidate selection and scoring line up.
        let q_trigrams: Vec<String> = tokenizer::extract_trigrams(query, "");
        if q_trigrams.is_empty() {
            return Vec::new();
        }

        // Conjunctive candidate selection: a document is a candidate only if it
        // contains *every* query trigram (the intersection of the posting
        // lists), matching the KV `intersect_trigrams` rule — this approximates
        // substring matching, so sharing a single incidental trigram does not
        // make a document match.
        let mut candidates: Option<Vec<u64>> = None;
        for trigram in &q_trigrams {
            let Some(posting) = self.postings.get(trigram) else {
                return Vec::new(); // a missing trigram → empty intersection
            };
            candidates = Some(match candidates {
                None => posting.keys().copied().collect(),
                Some(prev) => prev
                    .into_iter()
                    .filter(|id| posting.contains_key(id))
                    .collect(),
            });
            if candidates.as_ref().is_some_and(|c| c.is_empty()) {
                return Vec::new();
            }
        }
        let candidates = candidates.unwrap_or_default();

        // BM25 over the candidates, summing each query trigram's weight.
        let mut scores: HashMap<u64, f32> = HashMap::new();
        for trigram in &q_trigrams {
            let posting = &self.postings[trigram];
            let df = posting.len() as u64;
            let idf = bm25_idf(n, df);
            for &id in &candidates {
                let tf = *posting.get(&id).unwrap_or(&0) as f32;
                let dl = *self.doc_len.get(&id).unwrap_or(&0) as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl.max(f32::MIN_POSITIVE));
                let contribution = idf * (tf * (K1 + 1.0)) / denom.max(f32::MIN_POSITIVE);
                *scores.entry(id).or_insert(0.0) += contribution;
            }
        }

        let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
        // Highest score first; ties broken by ascending id for determinism.
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(limit);
        ranked
    }

    // ----- maintenance -------------------------------------------------------

    /// Discard everything and re-index every node in `graph`.
    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.postings.clear();
        self.docs.clear();
        self.doc_len.clear();
        self.total_len = 0;
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// The full-text fields of a node: title, body, and every string property
    /// value (matching the KV store's property FTS, #227).
    fn node_trigrams(node: &Node) -> Vec<String> {
        let mut fields: Vec<&str> = vec![node.title.as_str(), node.body.as_str()];
        for value in node.properties.0.values() {
            if let Some(s) = value.as_str() {
                fields.push(s);
            }
        }
        tokenizer::extract_raw_trigrams_fields(&fields)
    }

    /// Insert or replace a node's postings (create and update both route here).
    fn index_node(&mut self, node: &Node) {
        self.remove_node(node.id);
        let trigrams = Self::node_trigrams(node);
        if trigrams.is_empty() {
            // Still track the (empty) document so counts stay consistent.
            self.docs.insert(node.id, HashMap::new());
            self.doc_len.insert(node.id, 0);
            return;
        }
        let mut freqs: HashMap<String, u32> = HashMap::new();
        for t in &trigrams {
            *freqs.entry(t.clone()).or_insert(0) += 1;
        }
        for (t, &tf) in &freqs {
            self.postings
                .entry(t.clone())
                .or_default()
                .insert(node.id, tf);
        }
        let dl = trigrams.len() as u32;
        self.total_len += u64::from(dl);
        self.doc_len.insert(node.id, dl);
        self.docs.insert(node.id, freqs);
    }

    /// Remove a node's postings, if present.
    fn remove_node(&mut self, id: u64) {
        let Some(freqs) = self.docs.remove(&id) else {
            return;
        };
        for trigram in freqs.keys() {
            if let Some(posting) = self.postings.get_mut(trigram) {
                posting.remove(&id);
                if posting.is_empty() {
                    self.postings.remove(trigram);
                }
            }
        }
        if let Some(dl) = self.doc_len.remove(&id) {
            self.total_len -= u64::from(dl);
        }
    }
}

/// A trigram BM25 full-text index over **relationships**, maintained by tailing
/// a [`NativeGraph`]'s change-feed. The edge companion of [`NativeFtsIndex`],
/// matching the KV store's `efts:` edge index (#227-B / #229): an edge's
/// full-text document is its **string property values** (and the string
/// elements of any array-valued property), so `fts.searchRelationships` can be
/// answered on the native engine. Edges carry no title/body, so — unlike nodes
/// — only properties contribute.
#[derive(Default)]
pub struct NativeFtsRelIndex {
    /// trigram → (edge id → term frequency in that edge).
    postings: HashMap<String, HashMap<u64, u32>>,
    /// edge id → its trigram frequencies (the forward index).
    docs: HashMap<u64, HashMap<String, u32>>,
    /// edge id → document length (total trigram occurrences), for BM25.
    doc_len: HashMap<u64, u32>,
    /// Sum of every document length, so `avgdl = total_len / docs.len()`.
    total_len: u64,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl NativeFtsRelIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// The change-feed cursor this index has consumed up to.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The number of relationships currently indexed.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether the index holds no relationships.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Bring the index up to date with `graph` by consuming its change-feed
    /// since the last [`cursor`](Self::cursor); re-snapshots on a lagged batch,
    /// exactly like [`NativeFtsIndex::sync`].
    pub fn sync(&mut self, graph: &NativeGraph) {
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertEdge(edge) => self.index_edge(&edge),
                WalOp::DeleteEdge(id) => self.remove_edge(id),
                // Nodes and embeddings are indexed elsewhere.
                WalOp::UpsertNode(_)
                | WalOp::DeleteNode(_)
                | WalOp::SetEmbedding(..)
                | WalOp::DeleteEmbedding(_) => {}
            }
        }
        self.cursor = batch.cursor;
    }

    /// Search for `query`, returning up to `limit` `(edge_id, score)` pairs
    /// ranked by descending BM25 score. Identical ranking to
    /// [`NativeFtsIndex::search`], over the edge corpus.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(u64, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        let n = self.docs.len() as u64;
        if n == 0 {
            return Vec::new();
        }
        let avgdl = self.total_len as f32 / n as f32;

        let q_trigrams: Vec<String> = tokenizer::extract_trigrams(query, "");
        if q_trigrams.is_empty() {
            return Vec::new();
        }

        // Conjunctive candidate selection: an edge is a candidate only if it
        // contains every query trigram (matches the KV `intersect_trigrams`).
        let mut candidates: Option<Vec<u64>> = None;
        for trigram in &q_trigrams {
            let Some(posting) = self.postings.get(trigram) else {
                return Vec::new();
            };
            candidates = Some(match candidates {
                None => posting.keys().copied().collect(),
                Some(prev) => prev
                    .into_iter()
                    .filter(|id| posting.contains_key(id))
                    .collect(),
            });
            if candidates.as_ref().is_some_and(|c| c.is_empty()) {
                return Vec::new();
            }
        }
        let candidates = candidates.unwrap_or_default();

        let mut scores: HashMap<u64, f32> = HashMap::new();
        for trigram in &q_trigrams {
            let posting = &self.postings[trigram];
            let df = posting.len() as u64;
            let idf = bm25_idf(n, df);
            for &id in &candidates {
                let tf = *posting.get(&id).unwrap_or(&0) as f32;
                let dl = *self.doc_len.get(&id).unwrap_or(&0) as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl.max(f32::MIN_POSITIVE));
                let contribution = idf * (tf * (K1 + 1.0)) / denom.max(f32::MIN_POSITIVE);
                *scores.entry(id).or_insert(0.0) += contribution;
            }
        }

        let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(limit);
        ranked
    }

    // ----- maintenance -------------------------------------------------------

    /// Discard everything and re-index every edge in `graph`.
    fn rebuild_from(&mut self, graph: &NativeGraph) {
        self.postings.clear();
        self.docs.clear();
        self.doc_len.clear();
        self.total_len = 0;
        if let Ok(edges) = graph.all_edges() {
            for edge in &edges {
                self.index_edge(edge);
            }
        }
    }

    /// The full-text fields of an edge: its string property values and the
    /// string elements of any array-valued property, in key order — matching
    /// the KV store's `collect_property_text` (#227-B). Edges have no
    /// title/body.
    fn edge_trigrams(edge: &Edge) -> Vec<String> {
        let text = collect_edge_property_text(&edge.properties);
        let fields: Vec<&str> = text.iter().map(String::as_str).collect();
        tokenizer::extract_raw_trigrams_fields(&fields)
    }

    /// Insert or replace an edge's postings (create and update both route here).
    fn index_edge(&mut self, edge: &Edge) {
        self.remove_edge(edge.id);
        let trigrams = Self::edge_trigrams(edge);
        if trigrams.is_empty() {
            self.docs.insert(edge.id, HashMap::new());
            self.doc_len.insert(edge.id, 0);
            return;
        }
        let mut freqs: HashMap<String, u32> = HashMap::new();
        for t in &trigrams {
            *freqs.entry(t.clone()).or_insert(0) += 1;
        }
        for (t, &tf) in &freqs {
            self.postings
                .entry(t.clone())
                .or_default()
                .insert(edge.id, tf);
        }
        let dl = trigrams.len() as u32;
        self.total_len += u64::from(dl);
        self.doc_len.insert(edge.id, dl);
        self.docs.insert(edge.id, freqs);
    }

    /// Remove an edge's postings, if present.
    fn remove_edge(&mut self, id: u64) {
        let Some(freqs) = self.docs.remove(&id) else {
            return;
        };
        for trigram in freqs.keys() {
            if let Some(posting) = self.postings.get_mut(trigram) {
                posting.remove(&id);
                if posting.is_empty() {
                    self.postings.remove(trigram);
                }
            }
        }
        if let Some(dl) = self.doc_len.remove(&id) {
            self.total_len -= u64::from(dl);
        }
    }
}

/// Gather an edge's full-text source strings: string property values plus the
/// string elements of array-valued properties, in sorted-key order. Mirrors the
/// KV store's `collect_property_text` so native and KV edge-FTS index the same
/// text.
fn collect_edge_property_text(properties: &Properties) -> Vec<String> {
    let mut keys: Vec<&String> = properties.0.keys().collect();
    keys.sort();
    let mut out = Vec::new();
    for key in keys {
        match properties.0.get(key) {
            Some(serde_json::Value::String(s)) => out.push(s.clone()),
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    if let serde_json::Value::String(s) = item {
                        out.push(s.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod rel_tests {
    use super::NativeFtsRelIndex;
    use crate::engine::GraphEngine;
    use crate::model::{NewEdge, NewNode, Properties};
    use crate::native::NativeGraph;

    fn node(g: &NativeGraph, title: &str) -> u64 {
        g.create_node(NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .expect("node")
        .id
    }

    fn edge(g: &NativeGraph, from: u64, to: u64, note: &str) -> u64 {
        let mut props = std::collections::HashMap::new();
        props.insert("note".to_string(), serde_json::Value::String(note.into()));
        g.create_edge(NewEdge {
            from_id: from,
            to_id: to,
            kind: "MENTIONS".into(),
            weight: 1.0,
            properties: Properties(props),
        })
        .expect("edge")
        .id
    }

    #[test]
    fn indexes_edge_string_properties_and_ranks_by_bm25() {
        let g = NativeGraph::new();
        let a = node(&g, "a");
        let b = node(&g, "b");
        let zebra = edge(&g, a, b, "the quick brown zebra");
        let _dog = edge(&g, a, b, "a lazy dog sleeps");

        let mut fts = NativeFtsRelIndex::new();
        fts.sync(&g);
        assert_eq!(fts.len(), 2, "both edges indexed");

        let hits = fts.search("zebra", 10);
        assert_eq!(hits.len(), 1, "only the zebra edge matches");
        assert_eq!(hits[0].0, zebra);
        assert!(hits[0].1 > 0.0, "positive BM25 score");
    }

    #[test]
    fn edges_carry_no_title_or_body_only_properties() {
        // The edge kind ("MENTIONS") is NOT full-text; a query for it finds
        // nothing, matching the KV edge index (properties only).
        let g = NativeGraph::new();
        let a = node(&g, "a");
        let b = node(&g, "b");
        edge(&g, a, b, "hello world");

        let mut fts = NativeFtsRelIndex::new();
        fts.sync(&g);
        assert!(fts.search("mentions", 10).is_empty(), "kind is not indexed");
        assert_eq!(fts.search("hello", 10).len(), 1, "property text is indexed");
    }

    #[test]
    fn a_deleted_edge_leaves_the_index() {
        let g = NativeGraph::new();
        let a = node(&g, "a");
        let b = node(&g, "b");
        let e = edge(&g, a, b, "unique zebra");

        let mut fts = NativeFtsRelIndex::new();
        fts.sync(&g);
        assert_eq!(fts.search("zebra", 10).len(), 1);

        GraphEngine::delete_edge(&g, e).expect("delete");
        fts.sync(&g);
        assert!(fts.search("zebra", 10).is_empty(), "removed after delete");
    }
}
