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
//! use drevo::native::NativeGraph;
//! use drevo::native_fts::NativeFtsIndex;
//! use drevo::engine::GraphEngine; // brings `create_node` into scope
//! use drevo::model::NewNode;
//!
//! # fn main() -> drevo::error::Result<()> {
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

use crate::engine::GraphEngine;
use crate::fts::index::bm25_idf;
use crate::fts::tokenizer;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};

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
                // Edges carry no full-text content in this index.
                WalOp::UpsertEdge(_) | WalOp::DeleteEdge(_) => {}
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
