# AUDIT-fts — Phase 8.5 task `00108`

**Scope.** `src/fts/*` (~540 LOC across `tokenizer.rs`, `index.rs`,
`mod.rs`) plus the FTS-touching paths in `src/db.rs`:

- `tokenizer::normalize`, `tokenizer::trigrams`,
  `tokenizer::extract_trigrams`, `tokenizer::is_cjk`.
- `index::index_node`, `index::deindex_node`,
  `index::node_ids_for_trigram`, `index::posting_list_len`,
  `index::intersect_trigrams`.
- `db.rs` — `Drevo::create_node` / `update_node` / `delete_node` FTS
  hooks, `fts_node_ids_for_trigram`, `fts_intersect_trigrams`,
  `search_fts`, `list_recent` + `updated_idx` maintenance.

**Rules verified against.**

- `drevo-database` §"FTS index" — trigram inverted index;
  `lowercase + strip punctuation; CJK → bigrams`;
  *intersect posting lists, rank by TF-IDF*.
- `drevo-database` §"updated_idx" — every node mutation updates the
  inverted-timestamp index.
- `drevo-database` §"Performance Watch List" — `search_fts` on broad
  queries ~800 ms vs 50 ms target; documents the gap and the
  candidate mitigations.
- `drevo-database` §"Invariants" #2 (cascading delete must remove
  FTS entries) and #3 (FTS reindex on update).
- `drevo-tdd` §"Property-based tests for invariants" — algorithmic
  code (tokenizer) gets property-style coverage.
- `drevo-tdd` §"Edge cases mandatory" — Unicode (CJK, emoji,
  Cyrillic).
- `drevo-rust` §"Error Handling" — no `unwrap()` / `expect()` in
  library code (`#[cfg(test)]` exempt); `Result<T, DrevoError>`
  through every fallible path.
- `drevo-rust` common pitfalls #1 (cascade-delete must visit
  every index) and #2 (FTS reindex on title/body update).

**Test baseline at audit start.** 1154 passing (post-`00107`,
all features).

**Test baseline at audit end.** 1169 (+15 in
`tests/fts_audit_tests.rs`).

**Cross-links closed.**

- `00106` invariant #4 (`updated_idx` parity) — extended here with
  a *FTS-adjacent* mutation stream that specifically exercises
  title / body updates, the same call sites that touch
  `fts_index::deindex_node` / `index_node`.

---

## Findings

### F1 — `search_fts` rustdoc disagrees with the implementation   ❌ → ✅ fixed in this PR

`Drevo::search_fts` rustdoc described the IDF as:

```text
IDF (inverse document frequency): ln(N / df) where N is the
total number of nodes and df is the number of nodes containing
the trigram.
```

The code computes:

```rust
let idf = if df > 0.0 { (1.0 + total_nodes / df).ln() } else { 0.0 };
```

That is `ln(1 + N/df)` — a smoothed variant that keeps the IDF
strictly positive when every node contains the trigram (`df == N`).
The plain `ln(N/df)` would collapse to `ln(1) = 0` in that case and
silently drop *every* result, breaking the principle of least
astonishment for narrow corpora.

**Action.** Rustdoc rewritten to describe the *implemented* formula
([src/db.rs:621-655](src/db.rs:621)), include the smoothing
rationale (`df == N` case), and forward-link to the audit's
performance section. Behaviour is unchanged — this is a documentation
fix, not a ranking change.

**Pinned by test.**

- `search_fts_smoothed_idf_is_positive_when_df_equals_n` —
  3 nodes that all contain "rust", search for "rust", expect 3
  positive-score results. The naive-IDF semantics would silently
  return zero results; the smoothed semantics return 3.

A follow-up refactor (`#follow-up-bm25`) is recorded for swapping in
BM25 — that change SHOULD update this test as part of the deliberate
ranking-formula migration.

---

### F2 — Tokenizer is NFC-naive on combining diacritics   ❌ → 📝 pinned + flagged for follow-up

The `drevo-database` skill specifies *"lowercase, strip punctuation;
CJK characters become bigrams"* but does not require Unicode
normalization (NFC / NFKC). The current implementation is byte-exact
on the input: `"résumé"` (precomposed `U+00E9`) and `"re\u{0301}sume\u{0301}"`
(decomposed `e` + combining acute) produce **different** trigram
sets. A user typing one form will not find a node indexed under the
other form.

Existing coverage (`tests/fts_recall_tests.rs::search_fts_combining_diacritics`)
already exercised this case but only asserted "no crash"; it did
not lock the divergence between the two forms.

**Action.** Behaviour pinned by
`tokenizer_combining_diacritics_decomposed_form` in
`tests/fts_audit_tests.rs`:

```rust
assert_ne!(precomposed, decomposed,
    "tokenizer pins NFC-naive behaviour; see audit/AUDIT-fts.md F2");
```

A future tokenizer refactor that adopts NFC will flip the assertion,
which is the correct signal: ranking-correctness changes should be
visible, deliberate code diffs — never silent.

**No-refactor — reason.** NFC normalisation requires an additional
~150 KB of Unicode tables (via `unicode-normalization`) and a per-token
allocation. Both costs are acceptable in principle but materially
larger than the audit task's scope. The `00108` task spec calls out
tokenizer changes as *flagged for a separate refactor* (overlaps with
Phase 9 task `00058` fuzz target). Logging here, not landing.

---

### F3 — `search_fts` performance gap on broad queries   ⏸ documented, deferred per task scope

`drevo-database` §"Performance Watch List" cites the measured 800 ms
vs 50 ms target on broad single-token queries against ~10k nodes and
identifies the bottleneck as `scan_prefix` on large posting lists.
The `00108` task spec explicitly states: *"landing the fix is out of
scope for the audit task — flag for a separate refactor."*

**Bottleneck breakdown** (verified by reading
[src/db.rs:637-712](src/db.rs:637)):

1. **`scan_prefix(PREFIX_NODE)` per query** to compute `total_nodes`
   — O(N) on a B-tree scan of every node bytes payload, just to get
   a count. For 10k nodes with ~1 KB Markdown bodies this is the
   dominant cost on a cold cache.
2. **`extract_trigrams(&node.title, &node.body)` per candidate** to
   compute TF — runs the full normalise → trigram pipeline twice
   per node (once at index time, again at score time).
3. **`fts_index::posting_list_len` per query trigram** — re-issues
   `scan_prefix(fts:{trigram}:)` even though the same prefix was
   already walked during `intersect_trigrams`.
4. **`ids.contains(id)` in `intersect_trigrams`** is O(|ids|) per
   element — overall O(k · |posting|²) intersection. A `HashSet`
   over the smallest list would be O(k · |posting|).

**Mitigations (recorded as `#follow-up-fts-perf`):**

| Mitigation | Expected gain | Risk |
|------------|---------------|------|
| Persisted `meta:node_count` updated on create / delete | Replaces O(N) `scan_prefix` with O(1) `get` | Must be transactional with the `nodes` write — crash-safety story to design |
| Cached node-trigram set on the `Node` struct (or separate `node_trigrams:{id}` value) | Skips second `extract_trigrams` per candidate; ~2× speedup | +O(node_size) storage per node |
| `intersect_trigrams` via smallest-list-first + `HashSet` | Reduces intersection from O(k · M²) to O(k · M) | Pure code change |
| Cached `posting_list_len` per trigram in a meta column | Removes per-query DF scan | Must invalidate on every index_node / deindex_node |
| Inverted-index compaction at write-quiesce time | Removes empty / single-id posting lists | Background job; orthogonal |

The intersection-optimisation (last row of low-hanging fruit) is
small enough to land in a follow-up PR without protocol changes;
the rest will be scoped after Phase 13 (MVCC) since their
crash-safety stories change under MVCC.

---

### F4 — Posting-list intersection AND semantics are uncovered by direct tests   ❌ → ✅ fixed in this PR

`drevo-database` §"FTS index" specifies *"intersect posting lists,
rank by TF-IDF"* — i.e., **AND** semantics across query trigrams.
The intersection lives in `fts_index::intersect_trigrams` and is
correct, but the existing test suite only exercised it through
`search_fts` end-to-end; the AND-semantics, idempotence, and
input-order-commutativity invariants had no targeted assertions.

**Action.** Three new tests in `tests/fts_audit_tests.rs`:

| Test | Verifies |
|------|----------|
| `intersect_trigrams_is_idempotent` | Same query twice → same result |
| `intersect_trigrams_is_commutative_in_input_order` | `[a, b]` and `[b, a]` produce the same posting list |
| `intersect_trigrams_empty_when_any_trigram_misses` | One missing trigram makes the AND empty |

These tests pin the set-algebra contract so a future BM25 / OR-mode
swap (`#follow-up-bm25`) is a visible diff, not silent.

---

### F5 — Tokenizer Unicode-class property coverage was incomplete   ❌ → ✅ fixed in this PR

The `00108` task spec calls for *"property-test on Unicode classes
(CJK / Cyrillic / emoji / combining diacritics / RTL)"*.

**Pre-audit coverage** (grep over `tests/fts_*.rs`):

| Class | Covered before audit |
|-------|----------------------|
| Latin (mixed case, punctuation, digits) | ✅ — `fts_tokenizer_tests.rs` |
| CJK (Han / Hiragana / Katakana / Hangul) | ✅ — `fts_tokenizer_tests.rs` + `fts_recall_tests.rs` |
| Arabic | ✅ — `fts_recall_tests.rs::search_fts_arabic_text` |
| Korean (Hangul) | ✅ — `fts_recall_tests.rs::search_fts_korean_query` |
| Combining diacritics | partial (no-crash only) — `fts_recall_tests.rs` |
| ZWJ / ZWNJ | partial (no-crash only) — `fts_recall_tests.rs` |
| Emoji | partial (emoji-only query) — `fts_recall_tests.rs` |
| **Cyrillic** | ❌ — none |
| **Hebrew (RTL)** | ❌ — none |
| **Tokenizer determinism property** | ❌ — none |

**Action.** Six new Unicode-class tests in `tests/fts_audit_tests.rs`:

| Test | Verifies |
|------|----------|
| `tokenizer_cyrillic_lowercase_and_trigrams` | Cyrillic lowercases and trigrams correctly |
| `tokenizer_hebrew_rtl_is_kept` | Hebrew RTL passes through; trigrams non-empty and length-3 |
| `tokenizer_combining_diacritics_decomposed_form` | Pins F2 (NFC-naive divergence) |
| `tokenizer_emoji_are_stripped` | Emoji become spaces; emoji-only input → zero trigrams |
| `tokenizer_zero_width_chars_become_word_breaks` | ZWJ / ZWNJ act as word boundaries (not preserved) |
| `tokenizer_cjk_bigrams_only_between_consecutive_cjk` | Bigrams only fire for CJK + CJK pairs, not at Latin/CJK boundaries |
| `tokenizer_extract_trigrams_is_deterministic_on_random_unicode` | Four `xorshift32` seeds × 200-char strings — `extract_trigrams` is a pure function; all outputs are length 2 (CJK bigram) or 3 |

---

### F6 — FTS reindex invariant had no property-style fuzzer   ❌ → ✅ fixed in this PR

`drevo-database` invariant #3 — *"FTS reindex on update: changing
`title` or `body` requires deindexing the old text and indexing the
new"* — and `drevo-rust` common pitfall #2 *"forgetting to reindex
on update"* are tested at fixed points (single update, then read)
but never under a *random mutation stream*. A regression that
leaked stale trigrams from a deleted node, or missed an entry after
an in-place update, would slip past the current tests.

**Action.** New `fts_index_matches_node_text_under_random_mutations`
test in `tests/fts_audit_tests.rs`:

- 3 deterministic `xorshift32` seeds × 30 random ops each.
- Each op picks `create` / `update` / `delete` uniformly.
- After every op, re-derives the *expected* posting-list membership
  by enumerating every live node's `extract_trigrams(title, body)`
  and compares it to the *observed* posting lists scanned out of the
  storage layer.
- The assertion is symmetric: it catches both
  (a) missing entries (forgot to index) and
  (b) stale entries (forgot to deindex).

The 90-mutation-per-run fuzz is small enough to keep test latency
sub-50 ms on this codebase but large enough to catch the historical
regression class (cascade-delete forgetting an index, in-place
update reindexing only one half of the title-body pair).

---

### F7 — `updated_idx` parity under FTS-touching mutations   ❌ → ✅ fixed in this PR

`00106` introduced `Drevo::verify_invariants()` which already checks
the four storage-layer invariants including `updated_idx` parity.
The audit verified that *every* FTS-touching mutation
(`create_node` / `update_node` / `delete_node`) maintains this
invariant by adding a dedicated fuzzer that targets the same
mutation surface from the FTS side:

| Code path | `updated_idx` action | Verified by |
|-----------|----------------------|-------------|
| `create_node` → `updated_key(node.updated_at, id)` | put | `updated_idx_parity_under_random_mutations` (op `create`) |
| `update_node` → delete old key, put new key | update | same test (ops `update title` / `update body`) |
| `delete_node` → delete by `(old_updated_at, id)` | delete | same test (op `delete`) |

The test runs 3 seeds × 25 ops each and asserts
`verify_invariants()` returns an empty vector after **every** op.
A regression in `update_node` (e.g. forgetting to update
`updated_at` before re-keying) would surface as a duplicate /
orphaned entry on the very next assertion.

**No code change.** The path is already correct; the test pins it
against a regression class that the existing single-shot tests
would not catch.

---

### F8 — `search_fts` ordering determinism + tie-break stability   ❌ → ✅ fixed in this PR

The existing search tests assert membership (`results.len() == N`,
`results.contains(...)`) but do not lock the *exact* ordering of
the result vector. The implementation has a documented tie-break
(`score desc, then id asc`) — a future change to that tie-break,
say `id desc` for "most recently inserted wins", would slip past
the current suite.

**Action.** Two new tests in `tests/fts_audit_tests.rs`:

- `search_fts_results_are_deterministic_across_runs` — same corpus +
  same query produces byte-identical `ScoredNode` vectors across
  repeated calls, including bit-identical `f32` scores
  (`score.to_bits() == score.to_bits()`).
- `search_fts_ordering_stable_on_tied_scores` — two nodes with
  identical TF-IDF scores resolve to ascending node id, in
  insertion order.

---

## Universal-rule compliance for `src/fts/*`

Per the Phase 8.5 cross-cutting acceptance criteria, every audit
task verifies six universal rules. Status for the FTS module:

| Rule | Source | Status |
|------|--------|--------|
| No `unwrap()` / `expect()` in library code | `drevo-rust` §"Error Handling" | ✅ — only `#[cfg(test)]` blocks use `unwrap()` (notably `tests/fts_audit_tests.rs` per the same exemption) |
| No `unsafe` without justification | `drevo-rust` §"Code Style" | ✅ — zero `unsafe` in `src/fts/*` |
| Every `pub` item documented | `drevo-rust` §"Code Style" | ✅ — `tokenizer.rs` and `index.rs` document every `pub` / `pub(crate)` item; `search_fts` rustdoc rewritten in F1 |
| Max 3 levels of indentation per fn | `drevo-rust` §"Code Style" | ✅ — deepest function is `intersect_trigrams` at 3 levels |
| Every fallible `pub fn` returns `Result<T, DrevoError>` | `drevo-rust` §"Error Handling" | ✅ — all FTS-index functions return `Result<_, DrevoError>` |
| Test data in English (or documented Unicode test case) | `drevo-tdd` §"Conventions"; `CLAUDE.md` | ✅ — non-English data is confined to Unicode-class tests with a `// Unicode test case` comment per call site |

---

## Refactor follow-ups not landed in this PR

These are NOT failures of the audit — they are deliberate deferrals
per the `00108` task spec.

### `#follow-up-bm25` — strategy trait for ranking

Task spec: *"extract scoring into a strategy trait so BM25 (`drevo-database`
'Optional Phase 2') can swap in"*.

Sketch:

```rust
pub trait Scorer {
    fn score(&self, query: &[String], node_trigrams: &[String], df_by_trigram: &[(String, usize)],
             total_nodes: usize) -> f32;
}

pub struct TfIdfSmoothed;  // current implementation
pub struct Bm25 { pub k1: f32, pub b: f32 }  // future
```

`Drevo::search_fts` would accept a `&dyn Scorer` (or a generic
`S: Scorer`) and delegate the per-candidate scoring. The closed-form
fallout: every test that currently asserts a specific score value
would need a `TfIdfSmoothed` parameterisation.

**Not landed** because:

1. It introduces a public surface (`pub trait Scorer`) that
   `00109` (HTTP API audit) and `00111` (WASM audit) will need to
   either expose or wrap. Better to wait for those audits to declare
   their boundary preferences first.
2. The single-scorer implementation today is correct and pinned by
   F1 / F8. The cost of introducing the trait without a second
   scorer is YAGNI per `drevo-architecture` anti-pattern #2.

### `#follow-up-fts-perf` — broad-query latency

Task spec: *"document the gap and propose mitigation [...]; landing
the fix is out of scope for the audit task"*. Captured in F3 above
with a five-row mitigation table.

### `#follow-up-tokenizer-fuzz` — fuzz target

Task spec: *"tokenizer fuzz target (overlaps with Phase 9 task
`00058` — clarify division of labour)"*.

**Division of labour clarified.** `00058` owns the `cargo fuzz`
harness (afl-style coverage-guided fuzzer with `libFuzzer`). The
property-style fuzzer added by `00108` in
`tests/fts_audit_tests.rs::tokenizer_extract_trigrams_is_deterministic_on_random_unicode`
uses an `xorshift32` seed — same precedent as `00106` (DB invariant
fuzzer) and `00107` (traversal cross-algorithm fuzzer). `00058` will
upgrade these to coverage-guided fuzzing without changing the
existing property assertions.

### `#follow-up-nfc` — Unicode normalization

Pinned by F2's `assert_ne!` test. The `unicode-normalization` crate
would be the natural dependency; +150 KB compiled. Not landed —
flagged in F2.

---

## Test additions summary

`tests/fts_audit_tests.rs` (new file, 15 tests, ~450 LOC):

| Test | Finding | Verifies |
|------|---------|----------|
| `tokenizer_cyrillic_lowercase_and_trigrams` | F5 | Cyrillic class |
| `tokenizer_hebrew_rtl_is_kept` | F5 | Hebrew RTL |
| `tokenizer_combining_diacritics_decomposed_form` | F2, F5 | NFC-naive pin |
| `tokenizer_emoji_are_stripped` | F5 | Emoji stripping |
| `tokenizer_zero_width_chars_become_word_breaks` | F5 | ZWJ/ZWNJ semantics |
| `tokenizer_cjk_bigrams_only_between_consecutive_cjk` | F5 | CJK bigram boundary |
| `tokenizer_extract_trigrams_is_deterministic_on_random_unicode` | F5 | Pure function on random Unicode |
| `intersect_trigrams_is_idempotent` | F4 | Determinism |
| `intersect_trigrams_is_commutative_in_input_order` | F4 | Set-AND commutativity |
| `intersect_trigrams_empty_when_any_trigram_misses` | F4 | Missing trigram → empty |
| `fts_index_matches_node_text_under_random_mutations` | F6 | Index symmetric to live text under random mutations |
| `updated_idx_parity_under_random_mutations` | F7 | `verify_invariants` clean across FTS-touching mutation streams |
| `search_fts_results_are_deterministic_across_runs` | F8 | Same query → bit-identical score vector |
| `search_fts_ordering_stable_on_tied_scores` | F8 | Tie-break: `score desc, id asc` |
| `search_fts_smoothed_idf_is_positive_when_df_equals_n` | F1 | Smoothed-IDF behaviour pin |

---

## Definition of done — `00108`

| Acceptance criterion | Status |
|----------------------|--------|
| `audit/AUDIT-fts.md` exists, citing the skill rules verified | ✅ this file |
| Every rule violation either fixed by PR or recorded as accepted exception | ✅ F1 fixed; F2 / F3 / `#follow-up-*` deferred with rationale |
| `cargo test --all-features` ≥ 1092 passing tests | ✅ 1169 passing (+15) |
| `cargo clippy --all-targets --all-features -- -D warnings` clean | ✅ verified |
| `cargo clippy --target wasm32-unknown-unknown --no-default-features --features wasm -- -D warnings` clean | ✅ verified |
| `cargo fmt --check` clean | ✅ verified |
| No public API breakage without `BREAKING:` | ✅ — rustdoc-only edit in F1; no signature change |
