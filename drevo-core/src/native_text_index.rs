//! Opt-in **text indexes**: case-sensitive character trigrams over the string
//! at a declared property path, kept current from a [`NativeGraph`]'s change
//! feed, serving `CONTAINS`, `STARTS WITH` and `ENDS WITH` lookups.
//!
//! Each indexed value is padded with two start markers and two end markers
//! and cut into every three-character window, so `"save"` yields `⟨⟨s`,
//! `⟨sa`, `sav`, `ave`, `ve⟩`, `e⟩⟩`. A lookup cuts the needle the same way —
//! unpadded for `CONTAINS`, start-padded for `STARTS WITH`, end-padded for
//! `ENDS WITH` — and intersects the posting lists of its trigrams. A string
//! that matches holds every one of those trigrams, so the result is a
//! **superset** of the matches; the caller still checks each candidate.
//!
//! A lookup returns `None` — scan instead — when:
//!
//! - no declared spec covers the path for the pattern's labels (a spec for
//!   label `L` has only seen nodes carrying `L`);
//! - the needle has no trigrams: an empty needle, or fewer than three
//!   characters for `CONTAINS`;
//! - an indexed node holds something other than a string or `null` on the path
//!   (a number, a list, a map, or a scalar where the path needs a map). A
//!   substring test on such a value is a type error, and narrowing would hide
//!   that error instead of raising it;
//! - the needle is too common: once at least [`COMMON_TRIGRAM_FLOOR`] values
//!   are indexed, a needle whose rarest trigram still appears in more than half
//!   of them narrows too little to pay for the intersection.
//!
//! A top-level `title` or `body` path reads the node's own field unless the
//! property map has an entry of that name, the same way Cypher shows them.
//!
//! ```
//! use drevo_core::engine::GraphEngine;
//! use drevo_core::model::NewNode;
//! use drevo_core::native::NativeGraph;
//! use drevo_core::native_text_index::{NativeTextIndex, TextIndexSpec, TextMatch};
//!
//! # fn main() -> drevo_core::error::Result<()> {
//! let graph = NativeGraph::new();
//! let bug = graph.create_node(NewNode {
//!     kind: "Bug".into(),
//!     title: "crash on save".into(),
//!     body: String::new(),
//!     body_html: String::new(),
//!     properties: Default::default(),
//! })?;
//!
//! let title = vec!["title".to_string()];
//! let spec = TextIndexSpec::new(Some("Bug".into()), title.clone()).expect("valid path");
//! let mut idx = NativeTextIndex::new(vec![spec]);
//! idx.sync(&graph);
//!
//! let bug_label = ["Bug".to_string()];
//! let hits = idx.candidates(&bug_label, &title, TextMatch::Contains, "save");
//! assert_eq!(hits, Some([bug.id].into()));
//! // Two characters cannot form a trigram: the caller scans.
//! assert_eq!(idx.candidates(&bug_label, &title, TextMatch::Contains, "sa"), None);
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use serde_json::Value as JsonValue;

use crate::engine::GraphEngine;
use crate::labels::secondary_labels;
use crate::model::Node;
use crate::native::{NativeGraph, WalOp};
use crate::native_path_index::PathError;

/// Padding before a value, so prefixes have their own trigrams.
const START: char = '\u{2}';
/// Padding after a value, so suffixes have their own trigrams.
const END: char = '\u{3}';

/// Three consecutive characters.
type Trigram = [char; 3];

/// Below this many postings for its rarest trigram, a needle is always looked
/// up: both a lookup and a scan are cheap at that size. Above it, a lookup is
/// declined when that trigram appears in more than half the indexed values.
pub const COMMON_TRIGRAM_FLOOR: usize = 1024;

/// The substring predicate a lookup serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextMatch {
    /// `value CONTAINS needle`.
    Contains,
    /// `value STARTS WITH needle`.
    StartsWith,
    /// `value ENDS WITH needle`.
    EndsWith,
}

/// One declared text index: the string at `path`, for nodes with `label` or,
/// when `label` is `None`, for every node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TextIndexSpec {
    label: Option<String>,
    path: Vec<String>,
}

impl TextIndexSpec {
    /// A spec over `path` — `["title"]` for `n.title`, `["meta", "author"]`
    /// for `n.meta.author`.
    ///
    /// # Errors
    /// [`PathError::Empty`] for an empty path, [`PathError::EmptySegment`] for
    /// an empty segment.
    pub fn new(label: Option<String>, path: Vec<String>) -> Result<Self, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if path.iter().any(String::is_empty) {
            return Err(PathError::EmptySegment);
        }
        Ok(Self { label, path })
    }

    /// The label indexed nodes must carry, if any.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The property path.
    #[must_use]
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// Whether a node with `labels` falls under this spec.
    fn applies_to(&self, labels: &[String]) -> bool {
        self.label
            .as_ref()
            .is_none_or(|l| labels.iter().any(|have| have == l))
    }

    /// Whether this spec has seen every node a pattern requiring `labels`
    /// can match, at `path`.
    fn covers(&self, labels: &[String], path: &[String]) -> bool {
        self.path == path && self.label.as_ref().is_none_or(|l| labels.contains(l))
    }
}

/// The trigram postings of one spec.
#[derive(Default)]
struct TrigramStore {
    /// trigram → ids of the nodes whose value contains it.
    postings: HashMap<Trigram, BTreeSet<u64>>,
    /// node id → its distinct trigrams, so the node can be removed.
    docs: HashMap<u64, Vec<Trigram>>,
    /// Nodes holding a value a substring test would reject with a type error.
    blocked: BTreeSet<u64>,
}

impl TrigramStore {
    fn remove(&mut self, id: u64) {
        self.blocked.remove(&id);
        let Some(trigrams) = self.docs.remove(&id) else {
            return;
        };
        for t in trigrams {
            if let Some(ids) = self.postings.get_mut(&t) {
                ids.remove(&id);
                if ids.is_empty() {
                    self.postings.remove(&t);
                }
            }
        }
    }

    fn insert(&mut self, id: u64, text: &str) {
        let mut padded = vec![START, START];
        padded.extend(text.chars());
        padded.extend([END, END]);
        let mut trigrams: Vec<Trigram> = padded.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        trigrams.sort_unstable();
        trigrams.dedup();
        for t in &trigrams {
            self.postings.entry(*t).or_default().insert(id);
        }
        self.docs.insert(id, trigrams);
    }

    /// Ids holding every trigram in `query` (non-empty), or `None` when the
    /// rarest of them is too common to be worth intersecting.
    fn intersect(&self, query: &[Trigram]) -> Option<BTreeSet<u64>> {
        let mut lists = Vec::with_capacity(query.len());
        for t in query {
            match self.postings.get(t) {
                Some(ids) => lists.push(ids),
                None => return Some(BTreeSet::new()),
            }
        }
        lists.sort_by_key(|ids| ids.len());
        let (first, rest) = lists.split_first()?;
        if first.len() >= COMMON_TRIGRAM_FLOOR && first.len() * 2 > self.docs.len() {
            return None;
        }
        Some(
            first
                .iter()
                .copied()
                .filter(|id| rest.iter().all(|ids| ids.contains(id)))
                .collect(),
        )
    }
}

/// Trigram postings for the declared [`TextIndexSpec`]s. See the
/// [module docs](self).
#[derive(Default)]
pub struct NativeTextIndex {
    specs: Vec<TextIndexSpec>,
    /// One store per spec, in the same order.
    stores: Vec<TrigramStore>,
    /// The change-feed cursor this index has consumed up to.
    cursor: u64,
}

impl fmt::Debug for NativeTextIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTextIndex")
            .field("specs", &self.specs)
            .field(
                "nodes",
                &self.stores.iter().map(|s| s.docs.len()).collect::<Vec<_>>(),
            )
            .field("cursor", &self.cursor)
            .finish()
    }
}

/// What a property path holds on one node.
enum Resolved<'n> {
    /// A string to index.
    Text(&'n str),
    /// Missing or `null` somewhere along the path: the predicate is `null`.
    Absent,
    /// A value a substring test rejects with a type error.
    Blocking,
}

impl NativeTextIndex {
    /// An index over `specs`, positioned before any change; call
    /// [`sync`](Self::sync) to populate it.
    #[must_use]
    pub fn new(specs: Vec<TextIndexSpec>) -> Self {
        let stores = specs.iter().map(|_| TrigramStore::default()).collect();
        Self {
            specs,
            stores,
            cursor: 0,
        }
    }

    /// The declared specs.
    #[must_use]
    pub fn specs(&self) -> &[TextIndexSpec] {
        &self.specs
    }

    /// The change-feed cursor this index has consumed up to.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Replace the declared specs and rebuild from `graph`.
    pub fn set_specs(&mut self, specs: Vec<TextIndexSpec>, graph: &NativeGraph) {
        self.stores = specs.iter().map(|_| TrigramStore::default()).collect();
        self.specs = specs;
        self.rebuild_from(graph);
        self.cursor = graph.change_head();
    }

    /// Ids of the nodes whose string at `path` may satisfy `op` with `needle`,
    /// for a pattern requiring `labels` — a superset of the matches. `None`
    /// when the index cannot narrow (see the [module docs](self)).
    #[must_use]
    pub fn candidates(
        &self,
        labels: &[String],
        path: &[String],
        op: TextMatch,
        needle: &str,
    ) -> Option<BTreeSet<u64>> {
        let query = query_trigrams(op, needle)?;
        self.specs
            .iter()
            .zip(&self.stores)
            .find(|(spec, store)| spec.covers(labels, path) && store.blocked.is_empty())
            .and_then(|(_, store)| store.intersect(&query))
    }

    /// Bring the index up to date with `graph` by consuming its change feed
    /// since the last [`cursor`](Self::cursor). A feed trimmed past the cursor
    /// triggers a rebuild.
    pub fn sync(&mut self, graph: &NativeGraph) {
        if self.specs.is_empty() {
            self.cursor = graph.change_head();
            return;
        }
        let batch = graph.changes_since(self.cursor);
        if batch.lagged {
            self.rebuild_from(graph);
            self.cursor = graph.change_head().max(batch.cursor);
            return;
        }
        for op in batch.ops {
            match op {
                WalOp::UpsertNode(node) => self.index_node(&node),
                WalOp::DeleteNode(id) => {
                    for store in &mut self.stores {
                        store.remove(id);
                    }
                }
                WalOp::UpsertEdge(_)
                | WalOp::DeleteEdge(_)
                | WalOp::SetEmbedding(..)
                | WalOp::DeleteEmbedding(_)
                | WalOp::Prepare { .. }
                | WalOp::CommitPrepared { .. }
                | WalOp::RollbackPrepared { .. } => {}
            }
        }
        self.cursor = batch.cursor;
    }

    fn rebuild_from(&mut self, graph: &NativeGraph) {
        for store in &mut self.stores {
            *store = TrigramStore::default();
        }
        if self.specs.is_empty() {
            return;
        }
        if let Ok(nodes) = graph.all_nodes() {
            for node in &nodes {
                self.index_node(node);
            }
        }
    }

    /// Replace `node`'s postings in every store.
    fn index_node(&mut self, node: &Node) {
        let mut labels = secondary_labels(node);
        labels.push(node.kind.clone());
        for (spec, store) in self.specs.iter().zip(&mut self.stores) {
            store.remove(node.id);
            if !spec.applies_to(&labels) {
                continue;
            }
            match resolve(node, &spec.path) {
                Resolved::Text(text) => store.insert(node.id, text),
                Resolved::Absent => {}
                Resolved::Blocking => {
                    store.blocked.insert(node.id);
                }
            }
        }
    }
}

/// The trigrams a value matching `op` with `needle` must contain, or `None`
/// when there are none to look up.
fn query_trigrams(op: TextMatch, needle: &str) -> Option<Vec<Trigram>> {
    if needle.is_empty() {
        return None;
    }
    let mut chars: Vec<char> = Vec::with_capacity(needle.len() + 2);
    match op {
        TextMatch::Contains => chars.extend(needle.chars()),
        TextMatch::StartsWith => {
            chars.extend([START, START]);
            chars.extend(needle.chars());
        }
        TextMatch::EndsWith => {
            chars.extend(needle.chars());
            chars.extend([END, END]);
        }
    }
    let mut trigrams: Vec<Trigram> = chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect();
    trigrams.sort_unstable();
    trigrams.dedup();
    (!trigrams.is_empty()).then_some(trigrams)
}

/// What `node` holds at `path`.
fn resolve<'n>(node: &'n Node, path: &[String]) -> Resolved<'n> {
    let props = &node.properties.0;
    let Some((first, rest)) = path.split_first() else {
        return Resolved::Absent;
    };
    let mut value = match props.get(first) {
        Some(v) => v,
        None if rest.is_empty() && first == "title" => return text_field(&node.title),
        None if rest.is_empty() && first == "body" => return text_field(&node.body),
        None => return Resolved::Absent,
    };
    for segment in rest {
        value = match value {
            JsonValue::Object(map) => match map.get(segment) {
                Some(next) => next,
                None => return Resolved::Absent,
            },
            JsonValue::Null => return Resolved::Absent,
            _ => return Resolved::Blocking,
        };
    }
    match value {
        JsonValue::String(s) => Resolved::Text(s),
        JsonValue::Null => Resolved::Absent,
        _ => Resolved::Blocking,
    }
}

fn text_field(field: &str) -> Resolved<'_> {
    if field.is_empty() {
        Resolved::Absent
    } else {
        Resolved::Text(field)
    }
}
