//! Keyword-similarity grouping & faceting (task `00133`).
//!
//! Exact-string `GROUP BY kw` over the `keywords()` extractor's
//! output keeps *anxiety* / *anxieties* / *organize* / *organizing* as
//! separate facets, which defeats the theme-discovery use case. This module
//! collapses near-duplicate keywords into one facet along one of two
//! independent, *non-conflated* similarity axes the database already owns:
//!
//! * **Lexical** ([`FacetCollapse::Lexical`]) — *form*-based and free.
//!   Two keywords merge when they share a Porter `stem`
//!   **or** when their character-trigram
//!   sets ([`crate::fts::tokenizer::trigrams`]) overlap above a Jaccard
//!   threshold. Stemming catches morphological variants
//!   (*organize*/*organizing*/*organized*); the trigram signal catches
//!   typos and spellings stemming misses (*databse* ↔ *database*).
//!   Dependency-free, and the default.
//! * **Semantic** ([`FacetCollapse::Semantic`]) — *meaning*-based. Two
//!   keywords merge when the cosine similarity of their embeddings
//!   ([`crate::vector::cosine_similarity`]) is at least a threshold, which
//!   catches synonyms with no shared characters (*anxiety* ↔ *worry* ↔
//!   *dread*). Opt-in, because it needs an embedder: the caller supplies a
//!   `keyword → Vector` map, keeping the database core embedder-agnostic
//!   (embedding text lives in the Python layer, tasks `00079`).
//!
//! The axes are deliberately kept separate (`drevo-architecture`
//! §"Anti-Patterns" — no premature merge of two similarity signals): a
//! single call picks exactly one mode.
//!
//! ## Output shape
//!
//! Grouping is single-linkage: a union-find merges keywords
//! pairwise, so a transitive chain (*a*~*b*, *b*~*c*) lands in one
//! [`Facet`]. Each facet reports:
//!
//! * `facet` — a representative label, the member present in the most
//!   distinct documents (ties broken alphabetically),
//! * `members` — every surface form that collapsed in, ordered the same
//!   way,
//! * `count` — the number of **distinct documents** containing *any*
//!   member (a union, so a document with two members of one facet counts
//!   once).
//!
//! Facets are returned sorted by `count` descending, then `facet`
//! ascending — fully deterministic regardless of hash-map iteration order,
//! which the integration/e2e suites depend on.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::fts::stemmer::stem;
use crate::fts::tokenizer::trigrams;
use crate::vector::{cosine_similarity, Vector};

/// Default trigram-set Jaccard threshold for [`FacetCollapse::Lexical`].
///
/// `0.34` means two keywords merge on the trigram signal when about a third
/// of their combined trigrams are shared — loose enough to fold a
/// single-edit / transposition typo (e.g. *databse* ↔ *database*, Jaccard
/// ≈ 0.38), tight enough that words sharing only a common prefix stay apart
/// (*anxiety* ↔ *anxious*, Jaccard ≈ 0.2 — a synonym-ish pair that is the
/// *semantic* axis's job, not the lexical one). Stemming (always applied in
/// lexical mode) handles the morphological families regardless of this
/// value.
pub const DEFAULT_TRIGRAM_THRESHOLD: f32 = 0.34;

/// Default cosine threshold for [`FacetCollapse::Semantic`].
///
/// `0.85` is a conservative synonym cutoff for unit-normalized sentence /
/// word embeddings; lower it to merge more aggressively.
pub const DEFAULT_COSINE_THRESHOLD: f32 = 0.85;

/// How near-duplicate keywords are collapsed into a single [`Facet`].
///
/// Exactly one axis is selected per faceting call; see the module docs for
/// the lexical-vs-semantic distinction.
pub enum FacetCollapse<'a> {
    /// No collapsing — each distinct keyword is its own facet (`GROUP BY
    /// kw` semantics).
    None,
    /// Lexical (form-based) collapse: shared Porter stem **or** trigram
    /// Jaccard ≥ `trigram_threshold`.
    Lexical {
        /// Trigram-set Jaccard similarity at or above which two keywords
        /// merge. See [`DEFAULT_TRIGRAM_THRESHOLD`].
        trigram_threshold: f32,
    },
    /// Semantic (meaning-based) collapse: cosine similarity of the two
    /// keyword embeddings ≥ `cosine_threshold`. A keyword absent from
    /// `embeddings` only ever forms a singleton facet.
    Semantic {
        /// `keyword → embedding` map supplied by the caller's embedder.
        embeddings: &'a HashMap<String, Vector>,
        /// Cosine similarity at or above which two keywords merge. See
        /// [`DEFAULT_COSINE_THRESHOLD`].
        cosine_threshold: f32,
    },
}

/// One collapsed keyword facet — a representative label, its member surface
/// forms, and the number of distinct documents it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Facet {
    /// Representative label: the member appearing in the most distinct
    /// documents (alphabetical tie-break).
    pub facet: String,
    /// Every surface form folded into this facet, ordered by descending
    /// document count then alphabetically. Always non-empty; the first
    /// element equals `facet`.
    pub members: Vec<String>,
    /// Number of distinct documents containing **any** member.
    pub count: u64,
}

/// Build facets from per-document keyword lists.
///
/// `per_doc_keywords` pairs a document id with the keywords extracted from
/// it (e.g. via the crate-private `keywords` extractor); the same
/// keyword may repeat across documents. The chosen `collapse` axis decides
/// which keywords merge. Returns facets sorted by descending `count`, then
/// ascending `facet` label.
///
/// This function is pure (no storage access) so the collapse logic is
/// unit-testable in isolation from keyword extraction.
pub fn build_facets(
    per_doc_keywords: &[(u64, Vec<String>)],
    collapse: &FacetCollapse<'_>,
) -> Vec<Facet> {
    // keyword → set of documents containing it. BTreeMap keeps keyword
    // iteration order stable (sorted), which makes every downstream index
    // assignment deterministic.
    let mut docs: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for (doc_id, keywords) in per_doc_keywords {
        for kw in keywords {
            docs.entry(kw.clone()).or_default().insert(*doc_id);
        }
    }
    collapse_keyword_docs(&docs, collapse)
}

/// Collapse a fully-accumulated `keyword → documents` map into facets.
fn collapse_keyword_docs(
    docs: &BTreeMap<String, BTreeSet<u64>>,
    collapse: &FacetCollapse<'_>,
) -> Vec<Facet> {
    let keywords: Vec<&String> = docs.keys().collect();
    let n = keywords.len();
    let mut uf = UnionFind::new(n);

    match collapse {
        FacetCollapse::None => {}
        FacetCollapse::Lexical { trigram_threshold } => {
            // 1. Merge keywords sharing a Porter stem (morphological family).
            let mut stem_first: HashMap<String, usize> = HashMap::new();
            for (i, kw) in keywords.iter().enumerate() {
                let s = stem(kw);
                match stem_first.get(&s) {
                    Some(&j) => uf.union(i, j),
                    None => {
                        stem_first.insert(s, i);
                    }
                }
            }
            // 2. Merge keywords whose trigram sets are close (typos /
            //    near-spellings stemming missed).
            let tris: Vec<BTreeSet<String>> = keywords
                .iter()
                .map(|kw| trigrams(kw).into_iter().collect())
                .collect();
            for i in 0..n {
                for j in (i + 1)..n {
                    if uf.find(i) == uf.find(j) {
                        continue;
                    }
                    if jaccard(&tris[i], &tris[j]) >= *trigram_threshold {
                        uf.union(i, j);
                    }
                }
            }
        }
        FacetCollapse::Semantic {
            embeddings,
            cosine_threshold,
        } => {
            for i in 0..n {
                for j in (i + 1)..n {
                    if uf.find(i) == uf.find(j) {
                        continue;
                    }
                    if let (Some(a), Some(b)) =
                        (embeddings.get(keywords[i]), embeddings.get(keywords[j]))
                    {
                        if let Ok(sim) = cosine_similarity(a.as_slice(), b.as_slice()) {
                            if sim >= *cosine_threshold {
                                uf.union(i, j);
                            }
                        }
                    }
                }
            }
        }
    }

    // Bucket members by their union-find root.
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    let mut facets: Vec<Facet> = groups
        .into_values()
        .map(|member_idx| {
            // Distinct-document union across all members.
            let mut union_docs: BTreeSet<u64> = BTreeSet::new();
            for &i in &member_idx {
                union_docs.extend(docs[keywords[i]].iter().copied());
            }
            // Members ranked by descending document count, then alpha.
            let mut members: Vec<String> =
                member_idx.iter().map(|&i| keywords[i].clone()).collect();
            members.sort_by(|a, b| docs[b].len().cmp(&docs[a].len()).then_with(|| a.cmp(b)));
            Facet {
                facet: members[0].clone(),
                members,
                count: union_docs.len() as u64,
            }
        })
        .collect();

    facets.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.facet.cmp(&b.facet)));
    facets
}

/// Jaccard similarity of two trigram sets: `|A ∩ B| / |A ∪ B|`.
///
/// Returns `0.0` when either set is empty (a token too short to have any
/// trigram has no usable lexical signal, so it never merges on this axis).
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f32 / union as f32
}

/// Minimal disjoint-set / union-find with path compression.
///
/// Used for single-linkage clustering of keywords: `union(i, j)` puts the
/// two keywords' clusters together, `find(i)` returns a stable cluster
/// root. Root identity is arbitrary, but facet output is re-sorted, so the
/// final result is deterministic.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        // Path compression.
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            // Attach the larger root index under the smaller for a stable
            // (deterministic) shape; the final sort makes this irrelevant
            // to output but keeps the structure predictable in tests.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi] = lo;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: u64, kws: &[&str]) -> (u64, Vec<String>) {
        (id, kws.iter().map(|s| s.to_string()).collect())
    }

    fn facet_labels(facets: &[Facet]) -> Vec<&str> {
        facets.iter().map(|f| f.facet.as_str()).collect()
    }

    #[test]
    fn none_keeps_every_keyword_separate() {
        let docs = [
            doc(1, &["anxiety"]),
            doc(2, &["anxieties"]),
            doc(3, &["graph"]),
        ];
        let facets = build_facets(&docs, &FacetCollapse::None);
        assert_eq!(facets.len(), 3);
        // All counts are 1; sorted alphabetically on the tie.
        assert_eq!(facet_labels(&facets), vec!["anxieties", "anxiety", "graph"]);
        assert!(facets.iter().all(|f| f.count == 1 && f.members.len() == 1));
    }

    #[test]
    fn count_is_distinct_documents_not_occurrences() {
        // "graph" appears in docs 1, 2, 3 → count 3, even repeated within a doc.
        let docs = [
            doc(1, &["graph", "graph"]),
            doc(2, &["graph"]),
            doc(3, &["graph"]),
        ];
        let facets = build_facets(&docs, &FacetCollapse::None);
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].count, 3);
    }

    #[test]
    fn lexical_merges_morphological_family_via_stem() {
        // organize / organizing / organized share a Porter stem.
        let docs = [
            doc(1, &["organize"]),
            doc(2, &["organizing"]),
            doc(3, &["organized"]),
        ];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Lexical {
                trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
            },
        );
        assert_eq!(
            facets.len(),
            1,
            "stemming should fold all three: {facets:?}"
        );
        let f = &facets[0];
        assert_eq!(f.count, 3);
        assert_eq!(f.members.len(), 3);
        assert!(f.members.contains(&"organize".to_string()));
        assert!(f.members.contains(&"organizing".to_string()));
        assert!(f.members.contains(&"organized".to_string()));
    }

    #[test]
    fn lexical_merges_plural_pair_via_stem() {
        let docs = [doc(1, &["anxiety"]), doc(2, &["anxieties"])];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Lexical {
                trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
            },
        );
        assert_eq!(
            facets.len(),
            1,
            "anxiety/anxieties share a stem: {facets:?}"
        );
        assert_eq!(facets[0].count, 2);
    }

    #[test]
    fn lexical_merges_typo_via_trigrams() {
        // "database" vs "databse" (transposed) do not share a Porter stem
        // but their trigram sets overlap heavily.
        let docs = [doc(1, &["database"]), doc(2, &["databse"])];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Lexical {
                trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
            },
        );
        assert_eq!(facets.len(), 1, "typo should merge on trigrams: {facets:?}");
        assert_eq!(facets[0].count, 2);
    }

    #[test]
    fn lexical_does_not_merge_unrelated_words() {
        let docs = [
            doc(1, &["graph"]),
            doc(2, &["database"]),
            doc(3, &["vector"]),
        ];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Lexical {
                trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
            },
        );
        assert_eq!(
            facets.len(),
            3,
            "unrelated words must stay separate: {facets:?}"
        );
    }

    #[test]
    fn representative_is_most_frequent_member() {
        // "running" in 3 docs, "runs" in 1 → stem-merged; label = running.
        let docs = [
            doc(1, &["running"]),
            doc(2, &["running"]),
            doc(3, &["running"]),
            doc(4, &["runs"]),
        ];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Lexical {
                trigram_threshold: DEFAULT_TRIGRAM_THRESHOLD,
            },
        );
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].facet, "running");
        assert_eq!(facets[0].members, vec!["running", "runs"]);
        assert_eq!(facets[0].count, 4);
    }

    #[test]
    fn facets_sorted_by_count_then_label() {
        let docs = [
            doc(1, &["graph"]),
            doc(2, &["graph"]),
            doc(3, &["alpha"]),
            doc(4, &["beta"]),
        ];
        // graph:2, alpha:1, beta:1 → graph first, then alpha/beta alpha-sorted.
        let facets = build_facets(&docs, &FacetCollapse::None);
        assert_eq!(facet_labels(&facets), vec!["graph", "alpha", "beta"]);
    }

    #[test]
    fn deterministic_across_input_order() {
        let a = [doc(1, &["zebra", "apple"]), doc(2, &["mango", "apple"])];
        let b = [doc(2, &["apple", "mango"]), doc(1, &["apple", "zebra"])];
        let fa = build_facets(&a, &FacetCollapse::None);
        let fb = build_facets(&b, &FacetCollapse::None);
        assert_eq!(fa, fb);
    }

    #[test]
    fn semantic_merges_synonyms_with_no_shared_characters() {
        // anxiety / worry / dread share no trigrams, so lexical leaves them
        // apart; close embeddings collapse them semantically.
        let mut emb = HashMap::new();
        emb.insert("anxiety".to_string(), Vector(vec![1.0, 0.0, 0.0]));
        emb.insert("worry".to_string(), Vector(vec![0.99, 0.14, 0.0]));
        emb.insert("dread".to_string(), Vector(vec![0.98, 0.0, 0.2]));
        emb.insert("budget".to_string(), Vector(vec![0.0, 0.0, 1.0]));
        let docs = [
            doc(1, &["anxiety"]),
            doc(2, &["worry"]),
            doc(3, &["dread"]),
            doc(4, &["budget"]),
        ];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Semantic {
                embeddings: &emb,
                cosine_threshold: DEFAULT_COSINE_THRESHOLD,
            },
        );
        // anxiety+worry+dread collapse (count 3); budget stands alone.
        assert_eq!(facets.len(), 2, "{facets:?}");
        assert_eq!(facets[0].count, 3);
        assert_eq!(facets[0].members.len(), 3);
        assert_eq!(facets[1].facet, "budget");
    }

    #[test]
    fn semantic_keyword_without_embedding_is_singleton() {
        let mut emb = HashMap::new();
        emb.insert("anxiety".to_string(), Vector(vec![1.0, 0.0]));
        emb.insert("worry".to_string(), Vector(vec![0.99, 0.14]));
        // "mystery" has no embedding → never merges.
        let docs = [
            doc(1, &["anxiety"]),
            doc(2, &["worry"]),
            doc(3, &["mystery"]),
        ];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Semantic {
                embeddings: &emb,
                cosine_threshold: DEFAULT_COSINE_THRESHOLD,
            },
        );
        assert_eq!(facets.len(), 2);
        assert!(facets.iter().any(|f| f.facet == "mystery" && f.count == 1));
    }

    #[test]
    fn single_linkage_chains_transitively() {
        // a~b and b~c (but a not directly ~c) still land in one cluster.
        let mut emb = HashMap::new();
        emb.insert("a".to_string(), Vector(vec![1.0, 0.0]));
        emb.insert("b".to_string(), Vector(vec![0.94, 0.34])); // ~a and ~c
        emb.insert("c".to_string(), Vector(vec![0.77, 0.64])); // ~b, not ~a
        let docs = [doc(1, &["a"]), doc(2, &["b"]), doc(3, &["c"])];
        let facets = build_facets(
            &docs,
            &FacetCollapse::Semantic {
                embeddings: &emb,
                cosine_threshold: 0.9,
            },
        );
        assert_eq!(
            facets.len(),
            1,
            "transitive chain should be one facet: {facets:?}"
        );
        assert_eq!(facets[0].count, 3);
    }

    #[test]
    fn empty_input_yields_no_facets() {
        let facets = build_facets(&[], &FacetCollapse::None);
        assert!(facets.is_empty());
    }
}
