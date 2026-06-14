# drevo-fuzz — coverage-guided fuzzing

[`cargo fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets driven by
[`libFuzzer`](https://llvm.org/docs/LibFuzzer.html) against drevo's
algorithmic surfaces. Two families:

## FTS tokenizer (Phase 9 task `00058`)

Three targets against the public surface of
[`drevo::fts::tokenizer`](../src/fts/tokenizer.rs):

| Target | Tokenizer function | Invariants asserted |
|---|---|---|
| `fuzz_normalize` | `normalize(&str) -> String` | total, idempotent, no leading/trailing/consecutive spaces, ASCII alphabetics lowercase |
| `fuzz_trigrams` | `trigrams(&str) -> Vec<String>` | total, strictly ascending, char-length ∈ {2, 3}, empty when `normalize(input)` has < 2 chars |
| `fuzz_extract_trigrams` | `extract_trigrams(&str, &str) -> Vec<String>` | equal to `trigrams(joined)` (documented join rule), strictly ascending |

The exact same assertions are mirrored in
[`tests/fts_tokenizer_fuzz_harness_tests.rs`](../tests/fts_tokenizer_fuzz_harness_tests.rs)
so the stable `cargo test` matrix exercises them on every PR; the nightly
fuzz job extends coverage via libFuzzer's branch-feedback mutator.

## Cypher front end (Phase 15 task `00099`)

Three targets against the lexer/parser of
[`drevo::cypher`](../src/cypher/), including a **grammar-aware** generator:

| Target | Surface | Input | Invariants asserted |
|---|---|---|---|
| `fuzz_cypher_lexer` | `lexer::tokenize(&str)` | arbitrary `&str` | total (never panics); spans in-bounds (`start ≤ end ≤ len`) and monotonic by start offset |
| `fuzz_cypher_parser` | `parser::parse(&str)` | arbitrary `&str` | total; on success the `Query` is structurally non-empty and the lexer also accepts the source |
| `fuzz_cypher_grammar` | `parse(generate_query(&[u8]))` | byte **choice stream** | generation is total + non-empty; every generated query lexes **and** parses (the grammar is the supported subset) |

The first two throw arbitrary bytes at the front end (mostly the error
paths). The third interprets the libFuzzer input as a *choice stream* and
[`cypher_grammar.rs`](cypher_grammar.rs) turns it into a syntactically
well-formed query drawn from the parser's supported subset — driving the
mutator deep into the *accepting* branches a random byte stream rarely
reaches. That generator is `include!`d by **both** the fuzz target and the
stable harness so the two cannot drift.

All Cypher invariants — and a deterministic ~3800-input sweep of the
generator — are replayed under stable `cargo test` in
[`tests/cypher_fuzz_harness_tests.rs`](../tests/cypher_fuzz_harness_tests.rs).

## Prerequisites

This crate requires a nightly toolchain and the `cargo-fuzz` subcommand:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

The fuzz crate is a **separate workspace** from the parent `drevo` crate
(`fuzz/Cargo.toml` declares `[workspace] members = ["."]`). It is therefore
invisible to `cargo build` / `cargo test` / `cargo clippy` from the
repository root — those commands do not need the nightly toolchain.

## Run a target

```sh
# Run forever (Ctrl+C to stop), seeded with fuzz/seed_corpus/<target>/
cargo +nightly fuzz run fuzz_normalize

# Time-boxed (60s), useful in CI
cargo +nightly fuzz run fuzz_normalize -- -max_total_time=60

# Replay a single corpus file
cargo +nightly fuzz run fuzz_normalize fuzz/corpus/fuzz_normalize/<id>

# Same for the other FTS targets
cargo +nightly fuzz run fuzz_trigrams         -- -max_total_time=60
cargo +nightly fuzz run fuzz_extract_trigrams -- -max_total_time=60

# Cypher front-end targets
cargo +nightly fuzz run fuzz_cypher_lexer     -- -max_total_time=60
cargo +nightly fuzz run fuzz_cypher_parser    -- -max_total_time=60
cargo +nightly fuzz run fuzz_cypher_grammar   -- -max_total_time=60
```

`cargo-fuzz` will copy `seed_corpus/<target>/` into `corpus/<target>/` on
the first run, then evolve the corpus from there. Crashes are written to
`artifacts/<target>/` as standalone reproducers.

## Inspect coverage

```sh
cargo +nightly fuzz coverage fuzz_normalize
# Then point any LLVM coverage viewer at
# fuzz/coverage/fuzz_normalize/coverage.profdata
```

## Add a new fuzz target

1. Add a `fuzz_targets/fuzz_<name>.rs` file with a `fuzz_target!` macro
   invocation.
2. Append a matching `[[bin]]` entry to `fuzz/Cargo.toml`.
3. Create `seed_corpus/fuzz_<name>/` and add at least one seed file.
4. Mirror the invariant assertions into
   `tests/fts_tokenizer_fuzz_harness_tests.rs` so the stable CI matrix
   re-runs them on every PR. The `every_fuzz_target_has_a_seed_corpus`
   and `fuzz_cargo_toml_declares_every_target` regression tests will
   catch missed wiring.

## Division of labour with other test layers

| Layer | Location | Input strategy |
|---|---|---|
| Hand-written examples | `src/fts/tokenizer.rs::tests`, `tests/fts_*` | curated edge cases (CJK, RTL, emoji, combining diacritics) |
| Property-based | `tests/proptest_fts_tokenizer.rs` (task `00057`) | proptest mixed-script generator (512 cases × 8 properties) |
| xorshift32 fuzz | `tests/fts_audit_tests.rs` (task `00108`) | seeded xorshift32 PRNG, deterministic on CI |
| Coverage-guided | `fuzz/` (this directory, task `00058`) | libFuzzer branch-feedback mutator, nightly only |

`00058` adds the coverage-guided layer without removing the others. All
three random-input strategies assert the same set of tokenizer
invariants, exercised over different input distributions; the fuzz harness
extends coverage along execution branches the proptest generator does not
reach.
