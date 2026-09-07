# Changelog

All notable changes to drevo are recorded here. This complements the git
history and the in-repo issue tracker; format loosely follows
[Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- **CSR adjacency snapshot** (`drevo_core::csr::CsrAdjacency`, issue #382,
  Phase 8): a Compressed-Sparse-Row view of a `GraphSnapshot`'s out-adjacency
  (`GraphSnapshot::csr_out`) and in-adjacency (`GraphSnapshot::csr_in`) — a
  cache-friendly, contiguous layout with dense vertex indices, built off the
  immutable MVCC snapshot. The substrate for whole-graph parallel algorithms
  (#408, #412).
- **Morsel-driven parallel scan** (`CsrAdjacency::par_map`, issue #382): runs a
  per-vertex closure across worker threads over disjoint output slices — no
  locking, no contention, no new dependency (`std::thread::scope`). The
  parallel-execution primitive the analytics can fan out over (#411).

### Removed

- **Duplicate CSR PageRank and weakly-connected-components** that had been added
  to `drevo_core::csr` in #409 and #410. **This was my (the assistant's)
  mistake:** I built second implementations of PageRank and WCC over the CSR
  layout without first checking that the crate already ships a complete graph
  analytics suite in `src/algorithms/` — `pagerank` (serial, `pagerank_parallel`,
  and `pagerank_native` over the native MVCC snapshot), `louvain`, `wcc`, `scc`,
  `triangles`, `betweenness`, `closeness` — the canonical, more capable versions
  (weighted edges, tolerance-based convergence, configurable damping), exposed as
  `CALL drevo.*` procedures. The csr.rs copies were weaker (unweighted,
  fixed-iteration), unused by anything, and pure duplication, so they are
  removed. Lesson recorded: grep `src/algorithms/` before implementing a graph
  algorithm; #382's actual intent is to give the *existing* algorithms a CSR
  layout for a real parallel speedup (as the `pagerank_native` doc comment
  already noted), not to reimplement them. The CSR layout and `par_map` above
  remain — those are the genuinely new, on-target contribution.
