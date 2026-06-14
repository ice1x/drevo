//! Grammar-aware coverage-guided fuzz target for the Cypher front end.
//!
//! Phase 15 task `00099`. Unlike [`fuzz_cypher_lexer`] / [`fuzz_cypher_parser`],
//! which throw *arbitrary* bytes at the lexer/parser (mostly exercising the
//! error paths), this target interprets the libFuzzer input as a **choice
//! stream** and `generate_query` (shared `cypher_grammar.rs`) turns it into a
//! syntactically well-formed Cypher query drawn from the subset drevo's parser
//! supports. That drives the mutator deep into the *accepting* paths of the
//! lexer and parser — the branches a purely-random byte stream almost never
//! reaches.
//!
//! Invariants asserted on every generated query:
//!
//! - **Generation is total.** `generate_query` never panics and always returns
//!   a non-empty string (guaranteed by the generator's exhausted-stream-reads-0
//!   contract; re-asserted here as a tripwire).
//! - **The grammar is accepted.** Every generated query lexes *and* parses —
//!   if `parse` ever rejects a generated query, either the generator drifted
//!   out of the supported subset or the parser regressed. Both are bugs this
//!   target exists to catch.
//! - **Structural soundness.** The parsed [`ast::Query`] has at least one UNION
//!   part and every part has at least one clause.
//!
//! The same generator + assertions are replayed under stable `cargo test` in
//! `tests/cypher_fuzz_harness_tests.rs::assert_grammar_invariants`, so the
//! grammar the nightly fuzzer explores is exactly the grammar the PR matrix
//! exercises.

#![no_main]

use drevo::cypher::lexer::tokenize;
use drevo::cypher::parser::parse;
use libfuzzer_sys::fuzz_target;

// Shared generator — `include!`d so the libFuzzer binary and the stable
// replay harness compile the *same* source (see the file header).
include!("../cypher_grammar.rs");

fuzz_target!(|data: &[u8]| {
    let query = generate_query(data);
    assert!(
        !query.is_empty(),
        "generate_query returned an empty string for {data:?}"
    );

    assert!(
        tokenize(&query).is_ok(),
        "generator produced a query the lexer rejects: {query:?}"
    );

    let parsed = parse(&query)
        .unwrap_or_else(|e| panic!("generator produced an unparseable query {query:?}: {e:?}"));

    assert!(
        !parsed.parts.is_empty(),
        "parsed generated query has no parts: {query:?}"
    );
    for part in &parsed.parts {
        assert!(
            !part.query.clauses.is_empty(),
            "parsed generated query has a UNION part with no clauses: {query:?}"
        );
    }
});
