//! Cypher lexer — integration tests.
//!
//! Exercises the lexer on realistic, full-query Cypher snippets drawn
//! from each of the five drevo scenarios (CBT journal, story editor,
//! task manager, ERP, bug tracker). The goal here is *not* to re-test
//! every token kind in isolation — that is what the inline unit-test
//! module in `src/cypher/lexer.rs` does — but to confirm that the
//! lexer composes correctly when feeding it whole queries the parser
//! (task `00062`) will eventually have to handle.
//!
//! Naming follows the project's behavior-named convention: each test
//! describes the *behavior* being verified, not the function under
//! test.

use drevo::cypher::lexer::{tokenize, LexError, Span, TokenKind};

// ---------- helpers --------------------------------------------------

/// Return only the token kinds (dropping the trailing EOF), so tests can
/// assert against compact `vec![...]` patterns.
fn kinds(source: &str) -> Vec<TokenKind> {
    tokenize(source)
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .filter(|k| !matches!(k, TokenKind::Eof))
        .collect()
}

/// Slice the source between two adjacent token positions to confirm the
/// span actually points at the matching substring.
fn span_text(source: &str, span: Span) -> &str {
    &source[span.start..span.end]
}

// ---------- CBT journal queries --------------------------------------

#[test]
fn cbt_journal_create_entry() {
    // Recording a new journal entry with a list of cognitive distortions.
    let src = "CREATE (e:Entry {title: 'Anxious thought', \
               distortions: ['catastrophizing', 'mind reading']}) RETURN e";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Create));
    assert!(tokens.contains(&TokenKind::Colon));
    assert!(tokens.contains(&TokenKind::Identifier("Entry".to_string())));
    assert!(tokens.contains(&TokenKind::LBracket));
    assert!(tokens.contains(&TokenKind::RBracket));
    assert!(tokens.contains(&TokenKind::String("catastrophizing".to_string())));
    assert!(tokens.contains(&TokenKind::String("mind reading".to_string())));
    assert!(tokens.ends_with(&[TokenKind::Return, TokenKind::Identifier("e".to_string()),]));
}

#[test]
fn cbt_journal_match_by_mood_score_range() {
    let src = "MATCH (e:Entry) WHERE e.mood >= 3 AND e.mood <= 7 RETURN e.title, e.mood";
    let tokens = kinds(src);
    assert_eq!(tokens[0], TokenKind::Match);
    assert!(tokens.contains(&TokenKind::Where));
    assert!(tokens.contains(&TokenKind::Ge));
    assert!(tokens.contains(&TokenKind::Le));
    assert!(tokens.contains(&TokenKind::And));
    assert!(tokens.contains(&TokenKind::Integer(3)));
    assert!(tokens.contains(&TokenKind::Integer(7)));
}

#[test]
fn cbt_journal_link_thought_to_emotion() {
    let src = "MATCH (t:Thought), (em:Emotion) \
               WHERE t.id = $thought_id AND em.id = $emotion_id \
               CREATE (t)-[:TRIGGERS {weight: 0.8}]->(em)";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Parameter("thought_id".to_string())));
    assert!(tokens.contains(&TokenKind::Parameter("emotion_id".to_string())));
    assert!(tokens.contains(&TokenKind::Arrow));
    assert!(tokens.contains(&TokenKind::Identifier("TRIGGERS".to_string())));
    assert!(tokens.contains(&TokenKind::Float(0.8)));
}

// ---------- story editor queries -------------------------------------

#[test]
fn story_editor_chapter_with_scenes() {
    let src = "MATCH (c:Chapter)-[:HAS_SCENE]->(s:Scene) \
               WHERE c.book_id = $book RETURN c.title, collect(s.title) AS scenes";
    let tokens = kinds(src);
    assert_eq!(tokens[0], TokenKind::Match);
    assert!(tokens.contains(&TokenKind::Identifier("HAS_SCENE".to_string())));
    assert!(tokens.contains(&TokenKind::Parameter("book".to_string())));
    assert!(tokens.contains(&TokenKind::As));
    assert!(tokens.contains(&TokenKind::Identifier("collect".to_string())));
    assert!(tokens.contains(&TokenKind::Identifier("scenes".to_string())));
}

#[test]
fn story_editor_variable_length_path() {
    // Variable-length paths use `*` and `..`. The lexer emits the
    // building blocks; assembling them into [*1..3] is the parser's
    // job (task `00069`).
    let src = "MATCH (a:Character)-[:KNOWS*1..3]->(b:Character) RETURN a, b";
    let tokens = kinds(src);
    let star_pos = tokens.iter().position(|t| matches!(t, TokenKind::Star));
    let dotdot_pos = tokens.iter().position(|t| matches!(t, TokenKind::DotDot));
    assert!(star_pos.is_some());
    assert!(dotdot_pos.is_some());
    assert!(star_pos.unwrap() < dotdot_pos.unwrap());
    assert!(tokens.contains(&TokenKind::Integer(1)));
    assert!(tokens.contains(&TokenKind::Integer(3)));
}

#[test]
fn story_editor_unicode_characters_in_strings() {
    // CJK characters in strings — important because drevo's FTS layer
    // supports CJK and Cypher will be the query layer over the FTS.
    let src = r#"CREATE (c:Character {name: '李白', era: 'Tang dynasty'})"#;
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::String("李白".to_string())));
    assert!(tokens.contains(&TokenKind::String("Tang dynasty".to_string())));
}

// ---------- task manager queries -------------------------------------

#[test]
fn task_manager_filter_open_high_priority_tickets() {
    let src = "MATCH (t:Task) WHERE t.status <> 'done' AND t.priority IN ['P0', 'P1'] \
               RETURN t ORDER BY t.created_at DESC LIMIT 25";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Ne));
    assert!(tokens.contains(&TokenKind::In));
    assert!(tokens.contains(&TokenKind::String("done".to_string())));
    assert!(tokens.contains(&TokenKind::String("P0".to_string())));
    assert!(tokens.contains(&TokenKind::String("P1".to_string())));
    assert!(tokens.contains(&TokenKind::Order));
    assert!(tokens.contains(&TokenKind::By));
    assert!(tokens.contains(&TokenKind::Desc));
    assert!(tokens.contains(&TokenKind::Limit));
    assert!(tokens.contains(&TokenKind::Integer(25)));
}

#[test]
fn task_manager_assign_to_user_with_merge() {
    let src = "MERGE (t:Task {id: $task_id})-[:ASSIGNED_TO]->(u:User {handle: $handle}) \
               ON CREATE SET t.created_at = $now \
               ON MATCH SET t.updated_at = $now";
    let tokens = kinds(src);
    assert_eq!(tokens[0], TokenKind::Merge);
    let on_count = tokens.iter().filter(|t| matches!(t, TokenKind::On)).count();
    assert_eq!(
        on_count, 2,
        "ON keyword must appear twice (ON CREATE / ON MATCH)"
    );
    assert!(tokens.contains(&TokenKind::Parameter("task_id".to_string())));
    assert!(tokens.contains(&TokenKind::Parameter("handle".to_string())));
    assert!(tokens.contains(&TokenKind::Parameter("now".to_string())));
}

#[test]
fn task_manager_optional_match_assignee() {
    let src = "MATCH (t:Task {id: $id}) \
               OPTIONAL MATCH (t)-[:ASSIGNED_TO]->(u:User) \
               RETURN t, u";
    let tokens = kinds(src);
    let opt_pos = tokens
        .iter()
        .position(|t| matches!(t, TokenKind::Optional))
        .unwrap();
    assert_eq!(tokens[opt_pos + 1], TokenKind::Match);
}

// ---------- ERP queries ---------------------------------------------

#[test]
fn erp_purchase_order_aggregate_total() {
    let src = "MATCH (o:Order)-[:CONTAINS]->(li:LineItem) \
               WHERE o.supplier = 'Acme Corp' \
               RETURN o.id, sum(li.qty * li.unit_price) AS total";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Identifier("sum".to_string())));
    assert!(tokens.contains(&TokenKind::Star));
    assert!(tokens.contains(&TokenKind::As));
    assert!(tokens.contains(&TokenKind::String("Acme Corp".to_string())));
    assert!(tokens.contains(&TokenKind::Identifier("total".to_string())));
}

#[test]
fn erp_invoice_payment_with_chain() {
    let src = "MATCH (inv:Invoice {id: $id}) \
               WITH inv \
               MATCH (inv)<-[:PAID_BY]-(pay:Payment) \
               RETURN inv, collect(pay) AS payments";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::With));
    assert!(tokens.contains(&TokenKind::LArrow));
    assert!(tokens.contains(&TokenKind::Identifier("PAID_BY".to_string())));
}

#[test]
fn erp_decimal_amounts_in_floats() {
    let src = "CREATE (li:LineItem {qty: 200, unit_price: 15.00, discount: 0.05})";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Integer(200)));
    assert!(tokens.contains(&TokenKind::Float(15.0)));
    assert!(tokens.contains(&TokenKind::Float(0.05)));
}

// ---------- bug tracker queries --------------------------------------

#[test]
fn bug_tracker_search_recent_open_bugs() {
    let src = "MATCH (b:Bug) \
               WHERE b.status = 'open' AND b.severity >= 3 \
               AND b.title CONTAINS 'crash' \
               RETURN DISTINCT b ORDER BY b.severity DESC, b.created_at DESC";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Where));
    assert!(tokens.contains(&TokenKind::Contains));
    assert!(tokens.contains(&TokenKind::Distinct));
    assert!(tokens.contains(&TokenKind::String("crash".to_string())));
    assert!(tokens.contains(&TokenKind::Comma));
}

#[test]
fn bug_tracker_starts_with_predicate() {
    let src = "MATCH (b:Bug) WHERE b.module STARTS WITH 'auth.' RETURN b";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Starts));
    // Inside this lexer, WITH is a keyword (used in `WITH` clauses);
    // disambiguation between "WITH clause" and "STARTS WITH" is the
    // parser's responsibility.
    assert!(tokens.contains(&TokenKind::With));
    assert!(tokens.contains(&TokenKind::String("auth.".to_string())));
}

#[test]
fn bug_tracker_ends_with_and_regex() {
    let src = "MATCH (b:Bug) \
               WHERE b.file ENDS WITH '.rs' AND b.title =~ '(?i).*timeout.*' \
               RETURN b";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Ends));
    assert!(tokens.contains(&TokenKind::RegexMatch));
    assert!(tokens.contains(&TokenKind::String("(?i).*timeout.*".to_string())));
}

#[test]
fn bug_tracker_delete_with_detach() {
    let src = "MATCH (b:Bug {id: $id}) DETACH DELETE b";
    let tokens = kinds(src);
    let detach_pos = tokens
        .iter()
        .position(|t| matches!(t, TokenKind::Detach))
        .unwrap();
    assert_eq!(tokens[detach_pos + 1], TokenKind::Delete);
}

// ---------- positional / multi-line queries --------------------------

#[test]
fn multi_line_query_tracks_line_numbers() {
    let src = "MATCH (n)\nWHERE n.x > 5\nRETURN n";
    let tokens = tokenize(src).unwrap();
    let where_tok = tokens
        .iter()
        .find(|t| matches!(t.kind, TokenKind::Where))
        .unwrap();
    let return_tok = tokens
        .iter()
        .find(|t| matches!(t.kind, TokenKind::Return))
        .unwrap();
    assert_eq!(where_tok.span.line, 2);
    assert_eq!(return_tok.span.line, 3);
}

#[test]
fn line_comment_inside_query_skipped() {
    let src = "MATCH (n) // this is a node\nRETURN n";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::Match,
            TokenKind::LParen,
            TokenKind::Identifier("n".to_string()),
            TokenKind::RParen,
            TokenKind::Return,
            TokenKind::Identifier("n".to_string()),
        ]
    );
}

#[test]
fn block_comment_inside_query_skipped() {
    let src = "MATCH (n) /* TODO: add filter */ RETURN n";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::Match,
            TokenKind::LParen,
            TokenKind::Identifier("n".to_string()),
            TokenKind::RParen,
            TokenKind::Return,
            TokenKind::Identifier("n".to_string()),
        ]
    );
}

#[test]
fn span_round_trip_for_identifier() {
    let src = "MATCH (user)";
    let tokens = tokenize(src).unwrap();
    let user = tokens
        .iter()
        .find(|t| matches!(&t.kind, TokenKind::Identifier(name) if name == "user"))
        .unwrap();
    assert_eq!(span_text(src, user.span), "user");
}

// ---------- error reporting ----------------------------------------

#[test]
fn error_unterminated_string_in_query() {
    let src = "MATCH (n) WHERE n.title = 'oops";
    let err = tokenize(src).unwrap_err();
    assert!(matches!(err, LexError::UnterminatedString { .. }));
    // The error span should point at the opening quote.
    let span = err.span();
    assert_eq!(span.line, 1);
    // Source index of `'` is 26 (after "MATCH (n) WHERE n.title = ").
    assert_eq!(span.start, 26);
}

#[test]
fn error_unterminated_block_comment_in_query() {
    let src = "/* never closes\nMATCH (n)";
    let err = tokenize(src).unwrap_err();
    assert!(matches!(err, LexError::UnterminatedBlockComment { .. }));
}

#[test]
fn error_unexpected_char_carries_position() {
    let src = "MATCH (n) @";
    let err = tokenize(src).unwrap_err();
    let span = err.span();
    assert_eq!(span.line, 1);
    assert_eq!(span.column, 11);
}

// ---------- regression: keyword adjacency --------------------------

#[test]
fn is_null_lexes_as_two_tokens() {
    // Lexer must NOT collapse `IS NULL` into a single token — that
    // composition is the parser's job.
    let src = "WHERE n.foo IS NULL";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::Where,
            TokenKind::Identifier("n".to_string()),
            TokenKind::Dot,
            TokenKind::Identifier("foo".to_string()),
            TokenKind::Is,
            TokenKind::Null,
        ]
    );
}

#[test]
fn is_not_null_lexes_as_three_tokens() {
    let src = "WHERE n.foo IS NOT NULL";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::Where,
            TokenKind::Identifier("n".to_string()),
            TokenKind::Dot,
            TokenKind::Identifier("foo".to_string()),
            TokenKind::Is,
            TokenKind::Not,
            TokenKind::Null,
        ]
    );
}

#[test]
fn return_star_lexes_as_star_token() {
    // `RETURN *` is a common projection; the lexer should emit it as
    // RETURN, Star — not as a single composite token.
    let src = "MATCH (n) RETURN *";
    let tokens = kinds(src);
    assert_eq!(tokens.last(), Some(&TokenKind::Star));
}

#[test]
fn union_all_lexes_as_two_tokens() {
    // UNION ALL is two adjacent keyword tokens.
    let src = "RETURN 1 UNION ALL RETURN 2";
    let tokens = kinds(src);
    assert!(tokens.contains(&TokenKind::Union));
    assert!(tokens.contains(&TokenKind::All));
}

// ---------- regression: pattern adjacency ---------------------------

#[test]
fn directed_edge_pattern_lexes_correctly() {
    let src = "(a)-[:KNOWS]->(b)";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::LParen,
            TokenKind::Identifier("a".to_string()),
            TokenKind::RParen,
            TokenKind::Minus,
            TokenKind::LBracket,
            TokenKind::Colon,
            TokenKind::Identifier("KNOWS".to_string()),
            TokenKind::RBracket,
            TokenKind::Arrow,
            TokenKind::LParen,
            TokenKind::Identifier("b".to_string()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn reverse_directed_edge_pattern_lexes_correctly() {
    let src = "(a)<-[:KNOWS]-(b)";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::LParen,
            TokenKind::Identifier("a".to_string()),
            TokenKind::RParen,
            TokenKind::LArrow,
            TokenKind::LBracket,
            TokenKind::Colon,
            TokenKind::Identifier("KNOWS".to_string()),
            TokenKind::RBracket,
            TokenKind::Minus,
            TokenKind::LParen,
            TokenKind::Identifier("b".to_string()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn undirected_edge_pattern_lexes_as_two_minus_tokens() {
    // `--` is the undirected-edge syntax in Cypher. The lexer emits
    // two `Minus` tokens; the parser recognises the composite shape.
    let src = "(a)--(b)";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::LParen,
            TokenKind::Identifier("a".to_string()),
            TokenKind::RParen,
            TokenKind::Minus,
            TokenKind::Minus,
            TokenKind::LParen,
            TokenKind::Identifier("b".to_string()),
            TokenKind::RParen,
        ]
    );
}

#[test]
fn label_disjunction_in_pattern() {
    // (n:Foo|Bar) — node matching either label.
    let src = "(n:Foo|Bar)";
    let tokens = kinds(src);
    assert_eq!(
        tokens,
        vec![
            TokenKind::LParen,
            TokenKind::Identifier("n".to_string()),
            TokenKind::Colon,
            TokenKind::Identifier("Foo".to_string()),
            TokenKind::Pipe,
            TokenKind::Identifier("Bar".to_string()),
            TokenKind::RParen,
        ]
    );
}
