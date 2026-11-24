//! In-memory BM25 full-text search over a
//! [`NativeGraph`], kept current from its change
//! feed.
//!
//! The graph engine keeps only the core graph (nodes, edges, adjacency);
//! secondary indexes live off it and stay current by **tailing the change-feed**
//! rather than coupling to the write path (see
//! [`NativeGraph::changes_since`](crate::native::NativeGraph::changes_since)).
//! [`NativeFtsIndex`] indexes each node's
//! title, body and string properties as character trigrams and ranks matches
//! with Okapi BM25 (`k1 = 1.2`, `b = 0.75`, IDF
//! `ln(1 + (N − df + 0.5) / (df + 0.5))`);
//! [`NativeFtsRelIndex`] does the same
//! for edges.
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
use crate::native::{Change, NativeGraph, WalOp};
use crate::tokenizer;

/// BM25 term-frequency saturation, as in [`FtsRanking::default`](crate::model::FtsRanking::default).
const K1: f32 = 1.2;
/// BM25 length-normalisation, as in [`FtsRanking::default`](crate::model::FtsRanking::default).
const B: f32 = 0.75;

/// Which documents a query selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FtsMatch {
    /// A document holding all of the query.
    #[default]
    All,
    /// A document holding any word of the query.
    Any,
}

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
    /// A node carrying one of these labels is left out
    /// ([`without_labels`](Self::without_labels)).
    skipped_labels: Vec<String>,
}

impl NativeFtsIndex {
    /// Create an empty index positioned before any change.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty index that leaves out every node carrying one of
    /// `labels`, as its [`kind`](Node::kind) or as a
    /// [secondary label](crate::labels::secondary_labels).
    ///
    /// Such a node is never a hit and does not count towards the corpus
    /// statistics, so the other nodes rank exactly as if it were not in the
    /// graph. Use it to keep an application's bookkeeping records (job
    /// queues, audit trails) out of its search.
    ///
    /// ```
    /// # use drevo_core::engine::GraphEngine;
    /// # use drevo_core::model::{NewNode, Properties};
    /// # use drevo_core::native::NativeGraph;
    /// # use drevo_core::native_fts::NativeFtsIndex;
    /// # fn node(kind: &str, title: &str) -> NewNode {
    /// #     NewNode { kind: kind.into(), title: title.into(), body: String::new(),
    /// #               body_html: String::new(), properties: Properties::default() }
    /// # }
    /// let graph = NativeGraph::new();
    /// let recipe = graph.create_node(node("Recipe", "rye bread")).unwrap();
    /// graph.create_node(node("ImportJob", "import of rye bread")).unwrap();
    ///
    /// let mut fts = NativeFtsIndex::without_labels(["ImportJob"]);
    /// fts.sync(&graph);
    /// let hits = fts.search("rye", 10);
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].0, recipe.id);
    /// ```
    pub fn without_labels<I, S>(labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            skipped_labels: labels.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// Whether `node` carries a label this index leaves out.
    fn skips(&self, node: &Node) -> bool {
        !self.skipped_labels.is_empty()
            && (self.skipped_labels.contains(&node.kind)
                || crate::labels::secondary_labels(node)
                    .iter()
                    .any(|label| self.skipped_labels.contains(label)))
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
    /// keyword extraction (`drevo.keywords`).
    pub fn doc_count(&self) -> u64 {
        self.docs.len() as u64
    }

    /// Estimate a term's document frequency from its trigrams: the number of
    /// indexed docs containing **all** of them, as keyword extraction needs.
    /// An empty
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
            // `batch.cursor` is the head before the rebuild's snapshot, which
            // holds every change up to it; a later change is replayed next.
            self.rebuild_from(graph);
            self.cursor = batch.cursor;
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
                | WalOp::DeleteEmbedding(_)
                // Two-phase commit records (#556) never reach the feed as
                // such: a commit publishes its expanded write set, and only
                // the open-time seed can carry an unresolved `Prepare`, which
                // is invisible state.
                | WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
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
        self.corpus()
            .rank(query, limit, FtsMatch::All, &HashMap::new())
    }

    /// [`search`](Self::search) as it will answer once `changes` are
    /// committed and indexed: the hits and scores a transaction sees for its
    /// own view, before it commits.
    ///
    /// `changes` are the nodes the transaction has created, updated or
    /// deleted so far ([`NativeGraph::tx_changes`]). A created or updated
    /// node is matched and ranked by its text in the transaction, a deleted
    /// node is left out, and the corpus statistics count each of them once,
    /// as they will after the commit. With no changes this is
    /// [`search`](Self::search).
    ///
    /// ```
    /// use drevo_core::engine::GraphEngine;
    /// use drevo_core::model::{NewNode, Properties};
    /// use drevo_core::native::NativeGraph;
    /// use drevo_core::native_fts::NativeFtsIndex;
    ///
    /// let note = |title: &str| NewNode {
    ///     kind: "note".to_string(),
    ///     title: title.to_string(),
    ///     body: String::new(),
    ///     body_html: String::new(),
    ///     properties: Properties::default(),
    /// };
    /// let g = NativeGraph::new();
    /// g.create_node(note("sourdough bread")).unwrap();
    /// let mut fts = NativeFtsIndex::new();
    /// fts.sync(&g);
    ///
    /// let tx = g.tx_begin();
    /// let rye = g.tx_engine(tx).unwrap().create_node(note("rye bread")).unwrap();
    /// // The index holds committed nodes only …
    /// assert_eq!(fts.search("rye", 10), vec![]);
    /// // … and the transaction's changes complete it.
    /// let changes = g.tx_changes(tx).unwrap();
    /// let hits = fts.search_with_changes("rye", 10, &changes.nodes);
    /// assert_eq!(hits.len(), 1);
    /// assert_eq!(hits[0].0, rye.id);
    /// ```
    pub fn search_with_changes(
        &self,
        query: &str,
        limit: usize,
        changes: &[Change<Node>],
    ) -> Vec<(u64, f32)> {
        let changed = changed_docs(changes, |n| n.id, |n| !self.skips(n), Self::node_trigrams);
        self.corpus().rank(query, limit, FtsMatch::All, &changed)
    }

    /// [`search`](Self::search) with the choice of which nodes match:
    /// [`FtsMatch::All`] is `search`; [`FtsMatch::Any`] takes a node holding
    /// any word of `query` and ranks it by the BM25 weight of the words it
    /// holds, so the node sharing more, and rarer, words comes first. Use
    /// it for a natural-language question, which no node holds in full.
    ///
    /// A word counts when the node holds every trigram of it; a word of
    /// fewer than three characters has none and is ignored.
    ///
    /// ```
    /// # use drevo_core::engine::GraphEngine;
    /// # use drevo_core::model::{NewNode, Properties};
    /// # use drevo_core::native::NativeGraph;
    /// # use drevo_core::native_fts::{FtsMatch, NativeFtsIndex};
    /// # fn chapter(title: &str) -> NewNode {
    /// #     NewNode { kind: "Chapter".into(), title: title.into(), body: String::new(),
    /// #               body_html: String::new(), properties: Properties::default() }
    /// # }
    /// let graph = NativeGraph::new();
    /// let storm = graph
    ///     .create_node(chapter("The lighthouse keeper watched the storm"))
    ///     .unwrap();
    /// graph.create_node(chapter("Prices at the harbour market")).unwrap();
    /// let mut fts = NativeFtsIndex::new();
    /// fts.sync(&graph);
    ///
    /// let question = "who watched the storm from the lighthouse";
    /// assert!(fts.search(question, 5).is_empty());
    /// let hits = fts.search_matching(question, 5, FtsMatch::Any);
    /// assert_eq!(hits[0].0, storm.id);
    /// ```
    pub fn search_matching(&self, query: &str, limit: usize, mode: FtsMatch) -> Vec<(u64, f32)> {
        self.corpus().rank(query, limit, mode, &HashMap::new())
    }

    /// [`search_with_changes`](Self::search_with_changes) with the choice of
    /// which nodes match, as in [`search_matching`](Self::search_matching).
    pub fn search_matching_with_changes(
        &self,
        query: &str,
        limit: usize,
        mode: FtsMatch,
        changes: &[Change<Node>],
    ) -> Vec<(u64, f32)> {
        let changed = changed_docs(changes, |n| n.id, |n| !self.skips(n), Self::node_trigrams);
        self.corpus().rank(query, limit, mode, &changed)
    }

    /// The indexed documents, for ranking.
    fn corpus(&self) -> Corpus<'_> {
        Corpus {
            postings: &self.postings,
            docs: &self.docs,
            doc_len: &self.doc_len,
            total_len: self.total_len,
        }
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
    /// value.
    fn node_trigrams(node: &Node) -> Vec<String> {
        let mut fields: Vec<&str> = vec![node.title.as_str(), node.body.as_str()];
        for value in node.properties.values() {
            if let Some(s) = value.as_str() {
                fields.push(s);
            }
        }
        tokenizer::extract_raw_trigrams_fields(&fields)
    }

    /// Insert or replace a node's postings (create and update both route here).
    fn index_node(&mut self, node: &Node) {
        self.remove_node(node.id);
        if self.skips(node) {
            return;
        }
        let trigrams = Self::node_trigrams(node);
        if trigrams.is_empty() {
            // Still track the (empty) document so counts stay consistent.
            self.docs.insert(node.id, HashMap::new());
            self.doc_len.insert(node.id, 0);
            return;
        }
        let (freqs, dl) = term_frequencies(&trigrams);
        for (t, &tf) in &freqs {
            self.postings
                .entry(t.clone())
                .or_default()
                .insert(node.id, tf);
        }
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
/// a [`NativeGraph`]'s change-feed. The edge companion of [`NativeFtsIndex`]:
/// an edge's
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
            // `batch.cursor` is the head before the rebuild's snapshot, which
            // holds every change up to it; a later change is replayed next.
            self.rebuild_from(graph);
            self.cursor = batch.cursor;
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
                | WalOp::DeleteEmbedding(_)
                // Two-phase commit records (#556) never reach the feed as
                // such: a commit publishes its expanded write set, and only
                // the open-time seed can carry an unresolved `Prepare`, which
                // is invisible state.
                | WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
            }
        }
        self.cursor = batch.cursor;
    }

    /// Search for `query`, returning up to `limit` `(edge_id, score)` pairs
    /// ranked by descending BM25 score. Identical ranking to
    /// [`NativeFtsIndex::search`], over the edge corpus.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(u64, f32)> {
        self.corpus()
            .rank(query, limit, FtsMatch::All, &HashMap::new())
    }

    /// [`search`](Self::search) as it will answer once `changes` are
    /// committed and indexed — the relationship twin of
    /// [`NativeFtsIndex::search_with_changes`]. `changes` are the
    /// relationships the transaction has created, updated or deleted so far
    /// ([`NativeGraph::tx_changes`]), those removed with a deleted node
    /// included.
    pub fn search_with_changes(
        &self,
        query: &str,
        limit: usize,
        changes: &[Change<Edge>],
    ) -> Vec<(u64, f32)> {
        let changed = changed_docs(changes, |e| e.id, |_| true, Self::edge_trigrams);
        self.corpus().rank(query, limit, FtsMatch::All, &changed)
    }

    /// [`search`](Self::search) with the choice of which relationships
    /// match, as in [`NativeFtsIndex::search_matching`].
    pub fn search_matching(&self, query: &str, limit: usize, mode: FtsMatch) -> Vec<(u64, f32)> {
        self.corpus().rank(query, limit, mode, &HashMap::new())
    }

    /// [`search_with_changes`](Self::search_with_changes) with the choice of
    /// which relationships match, as in
    /// [`NativeFtsIndex::search_matching`].
    pub fn search_matching_with_changes(
        &self,
        query: &str,
        limit: usize,
        mode: FtsMatch,
        changes: &[Change<Edge>],
    ) -> Vec<(u64, f32)> {
        let changed = changed_docs(changes, |e| e.id, |_| true, Self::edge_trigrams);
        self.corpus().rank(query, limit, mode, &changed)
    }

    /// The indexed documents, for ranking.
    fn corpus(&self) -> Corpus<'_> {
        Corpus {
            postings: &self.postings,
            docs: &self.docs,
            doc_len: &self.doc_len,
            total_len: self.total_len,
        }
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
    /// string elements of any array-valued property, in key order. Edges have
    /// no title/body.
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
        let (freqs, dl) = term_frequencies(&trigrams);
        for (t, &tf) in &freqs {
            self.postings
                .entry(t.clone())
                .or_default()
                .insert(edge.id, tf);
        }
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

/// Each trigram's number of occurrences in a document, and the document's
/// length (every occurrence counted).
fn term_frequencies(trigrams: &[String]) -> (HashMap<String, u32>, u32) {
    let mut freqs: HashMap<String, u32> = HashMap::new();
    for t in trigrams {
        *freqs.entry(t.clone()).or_insert(0) += 1;
    }
    (freqs, trigrams.len() as u32)
}

/// A document as a transaction left it: its trigram frequencies and length,
/// or `None` once deleted.
type ChangedDoc = Option<(HashMap<String, u32>, u32)>;

/// The documents of the records in `changes`, by record id.
fn changed_docs<T>(
    changes: &[Change<T>],
    id: impl Fn(&T) -> u64,
    indexed: impl Fn(&T) -> bool,
    trigrams: impl Fn(&T) -> Vec<String>,
) -> HashMap<u64, ChangedDoc> {
    let mut changed = HashMap::with_capacity(changes.len());
    for change in changes {
        match (&change.after, &change.before) {
            (Some(record), _) if indexed(record) => {
                changed.insert(id(record), Some(term_frequencies(&trigrams(record))));
            }
            // A record the index leaves out is, to a search, a deleted one.
            (Some(record), _) => {
                changed.insert(id(record), None);
            }
            (None, Some(record)) => {
                changed.insert(id(record), None);
            }
            (None, None) => {}
        }
    }
    changed
}

/// The documents of one index, borrowed for a search.
struct Corpus<'a> {
    /// trigram → (document id → term frequency).
    postings: &'a HashMap<String, HashMap<u64, u32>>,
    /// document id → its trigram frequencies.
    docs: &'a HashMap<u64, HashMap<String, u32>>,
    /// document id → document length.
    doc_len: &'a HashMap<u64, u32>,
    /// Sum of every document length.
    total_len: u64,
}

impl Corpus<'_> {
    /// Up to `limit` `(id, score)` pairs for `query`, by descending BM25
    /// score, over this corpus with every document in `changed` replaced by
    /// its entry (or removed, for `None`).
    ///
    /// The query is trigram-tokenised the same way documents are. With
    /// [`FtsMatch::All`] a document matches only if it contains *every*
    /// query trigram — this approximates substring matching, so sharing a
    /// single incidental trigram does not make a document match — and each
    /// query trigram then contributes its BM25 weight to the document's
    /// score. With [`FtsMatch::Any`] the same rule is applied to each word
    /// of the query on its own, and a document's score is the sum over the
    /// words it matches.
    fn rank(
        &self,
        query: &str,
        limit: usize,
        mode: FtsMatch,
        changed: &HashMap<u64, ChangedDoc>,
    ) -> Vec<(u64, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        // Corpus size and total length, each changed document counted as the
        // transaction left it.
        let mut n = self.docs.len() as u64;
        let mut total_len = self.total_len;
        for (id, doc) in changed {
            if self.docs.contains_key(id) {
                n -= 1;
                total_len -= u64::from(self.doc_len.get(id).copied().unwrap_or(0));
            }
            if let Some((_, dl)) = doc {
                n += 1;
                total_len += u64::from(*dl);
            }
        }
        if n == 0 {
            return Vec::new();
        }
        let avgdl = total_len as f32 / n as f32;

        // Query trigrams, normalised + deduped exactly as the KV store does
        // (`extract_trigrams`), so candidate selection and scoring line up:
        // one set for the whole query, or one per distinct word.
        let terms: Vec<Vec<String>> = match mode {
            FtsMatch::All => vec![tokenizer::extract_trigrams(query, "")],
            FtsMatch::Any => {
                let mut words = tokenizer::words(query);
                words.sort_unstable();
                words.dedup();
                words.iter().map(|w| tokenizer::trigrams(w)).collect()
            }
        };
        let mut scores: HashMap<u64, f32> = HashMap::new();
        for q_trigrams in terms.iter().filter(|t| !t.is_empty()) {
            self.score(q_trigrams, n, avgdl, changed, &mut scores);
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

    /// Add to `scores` the BM25 weight of `q_trigrams` for every document
    /// that holds all of them, in a corpus of `n` documents of average
    /// length `avgdl` (both counted with `changed` applied).
    fn score(
        &self,
        q_trigrams: &[String],
        n: u64,
        avgdl: f32,
        changed: &HashMap<u64, ChangedDoc>,
        scores: &mut HashMap<u64, f32>,
    ) {
        let postings: Vec<Option<&HashMap<u64, u32>>> =
            q_trigrams.iter().map(|t| self.postings.get(t)).collect();

        // Candidates: the indexed documents holding every query trigram (the
        // intersection of the posting lists; a trigram with no list leaves
        // none), less the changed ones, plus the changed documents that hold
        // every query trigram.
        let indexed: Option<Vec<&HashMap<u64, u32>>> = postings.iter().copied().collect();
        let mut candidates: Vec<u64> = match indexed.as_deref().and_then(<[_]>::split_first) {
            Some((first, rest)) => first
                .keys()
                .copied()
                .filter(|id| rest.iter().all(|list| list.contains_key(id)))
                .filter(|id| !changed.contains_key(id))
                .collect(),
            None => Vec::new(),
        };
        candidates.extend(changed.iter().filter_map(|(id, doc)| {
            let (freqs, _) = doc.as_ref()?;
            q_trigrams
                .iter()
                .all(|t| freqs.contains_key(t))
                .then_some(*id)
        }));
        if candidates.is_empty() {
            return;
        }

        // BM25 over the candidates, summing each query trigram's weight.
        for (trigram, posting) in q_trigrams.iter().zip(&postings) {
            // Documents holding the trigram: the indexed ones that did not
            // change, plus the changed ones that hold it now.
            let mut df = posting.map_or(0, |list| list.len() as u64);
            for (id, doc) in changed {
                if posting.is_some_and(|list| list.contains_key(id)) {
                    df -= 1;
                }
                if doc
                    .as_ref()
                    .is_some_and(|(freqs, _)| freqs.contains_key(trigram))
                {
                    df += 1;
                }
            }
            let idf = bm25_idf(n, df);
            for &id in &candidates {
                let (tf, dl) = match changed.get(&id) {
                    Some(Some((freqs, dl))) => (freqs.get(trigram).copied().unwrap_or(0), *dl),
                    _ => (
                        posting.and_then(|list| list.get(&id)).copied().unwrap_or(0),
                        self.doc_len.get(&id).copied().unwrap_or(0),
                    ),
                };
                let (tf, dl) = (tf as f32, dl as f32);
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl.max(f32::MIN_POSITIVE));
                let contribution = idf * (tf * (K1 + 1.0)) / denom.max(f32::MIN_POSITIVE);
                *scores.entry(id).or_insert(0.0) += contribution;
            }
        }
    }
}

/// Gather an edge's full-text source strings: string property values plus the
/// string elements of array-valued properties, in sorted-key order, so the
/// same edge always yields the same document.
fn collect_edge_property_text(properties: &Properties) -> Vec<String> {
    // Keys come in sorted order.
    let mut out = Vec::new();
    for key in properties.keys() {
        match properties.get(key) {
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
            properties: Properties::from(props),
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
