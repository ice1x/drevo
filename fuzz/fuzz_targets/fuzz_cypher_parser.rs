//! Coverage-guided fuzz target for [`drevo::cypher::parser::parse`].
//!
//! Phase 15 task `00099`. The parser must be **total** — it never panics on
//! any `&str`, returning either an [`ast::Query`] or a recoverable
//! `ParseError`. Two further invariants are asserted on success:
//!
//! - **Non-empty query.** A successfully-parsed [`ast::Query`] always has at
//!   least one UNION part, and each part has at least one clause — the
//!   parser never returns a structurally empty tree.
//! - **Lex-then-parse agreement.** If `parse` succeeds, `tokenize` on the
//!   same source must also succeed (the parser consumes the lexer, so it
//!   cannot accept input the lexer rejects).
//!
//! Inputs are decoded as `&str` by `libfuzzer-sys`. The same assertions are
//! mirrored in `tests/cypher_fuzz_harness_tests.rs::assert_parser_invariants`.

#![no_main]

use drevo::cypher::lexer::tokenize;
use drevo::cypher::parser::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let Ok(query) = parse(input) else {
        // A recoverable ParseError is a valid outcome; only a panic fails.
        return;
    };

    assert!(
        !query.parts.is_empty(),
        "parser returned a query with no parts for {input:?}"
    );
    for part in &query.parts {
        assert!(
            !part.query.clauses.is_empty(),
            "parser returned a UNION part with no clauses for {input:?}"
        );
    }

    assert!(
        tokenize(input).is_ok(),
        "parser accepted input the lexer rejects: {input:?}"
    );
});
