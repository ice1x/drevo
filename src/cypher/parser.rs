//! Cypher parser — Phase 10 task `00062`.
//!
//! The parser consumes the `Vec<Token>` produced by
//! [`crate::cypher::lexer::tokenize`] and produces a
//! [`crate::cypher::ast::Query`]. It is hand-written, recursive-descent
//! at the clause level, and uses [Pratt parsing][pratt] for expressions
//! so operator precedence and associativity are encoded in a single
//! table rather than mirrored across mutually-recursive functions.
//!
//! [pratt]: https://en.wikipedia.org/wiki/Operator-precedence_parser#Pratt_parsing
//!
//! ## Surface
//!
//! Only the free function `parse` is public. It returns a `ParseError` on the first
//! syntactic problem; the lexer is fail-fast by design and the parser
//! follows the same convention for the initial Phase 10 cut. (A
//! recovering parser that collects multiple errors per source is
//! tracked as a follow-up — for the executor and Bolt drivers, a single
//! good error message at the first failure point is sufficient.)
//!
//! ## Keyword-as-identifier policy
//!
//! Cypher's grammar allows most keywords to appear as ordinary names in
//! certain positions:
//!
//! - After `.` — property names (`n.in`, `n.start`).
//! - After `:` — label and relationship-type names (`MATCH (n:Match)`).
//! - In map keys (`{end: 1}`).
//! - As function names (`count(...)`).
//!
//! The parser accepts both [`crate::cypher::lexer::TokenKind::Identifier`]
//! and any keyword in those positions via a single internal helper. Bare
//! variable positions (e.g. `MATCH (foo)`) still require a real
//! identifier — using a clause keyword as a variable would create
//! ambiguity the parser can't resolve without unbounded look-ahead.

use crate::cypher::ast::*;
use crate::cypher::lexer::{tokenize, LexError, Span, Token, TokenKind};

/// A parse error.
///
/// Carries the source [`Span`] of the offending input so callers can
/// render `^^^` markers under the source — the same convention as
/// [`LexError`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    /// A token of one kind was expected but a different one was found.
    #[error(
        "expected {expected} at line {}, column {}, found {found}",
        .span.line, .span.column
    )]
    Expected {
        /// Human description of what was expected.
        expected: String,
        /// What was found instead (display form of the token).
        found: String,
        /// Span of the offending token.
        span: Span,
    },

    /// A required name (identifier) was missing.
    #[error("expected identifier at line {}, column {}", .span.line, .span.column)]
    ExpectedIdentifier {
        /// Span of the offending token.
        span: Span,
    },

    /// A required expression was missing.
    #[error("expected expression at line {}, column {}", .span.line, .span.column)]
    ExpectedExpression {
        /// Span of the offending token.
        span: Span,
    },

    /// A clause was malformed.
    #[error("{message} at line {}, column {}", .span.line, .span.column)]
    Malformed {
        /// Diagnostic message.
        message: String,
        /// Span of the offending construct.
        span: Span,
    },

    /// The source ended unexpectedly.
    #[error("unexpected end of input at line {}, column {}", .span.line, .span.column)]
    UnexpectedEof {
        /// Span of the EOF token.
        span: Span,
    },

    /// The input was empty (no clauses at all).
    #[error("empty Cypher query")]
    Empty,

    /// Expression nesting exceeded the parser's fixed depth limit
    /// (`MAX_EXPRESSION_DEPTH`). Recursive-descent
    /// parsing of arbitrarily deep input (`((((…))))`, `NOT NOT NOT …`,
    /// `[[[[…]]]]`) would otherwise overflow the stack; this recoverable error
    /// keeps the parser *total* (it never panics / aborts) on adversarial
    /// input. The bound is far beyond any hand-written query.
    #[error(
        "expression nests too deeply (limit {MAX_EXPRESSION_DEPTH}) at line {}, column {}",
        .span.line, .span.column
    )]
    NestingTooDeep {
        /// Span at which the depth limit was hit.
        span: Span,
    },

    /// A lexical error surfaced through the parser.
    #[error(transparent)]
    Lex(#[from] LexError),
}

impl ParseError {
    /// Return the [`Span`] of the offending source, when available.
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Expected { span, .. }
            | Self::ExpectedIdentifier { span }
            | Self::ExpectedExpression { span }
            | Self::Malformed { span, .. }
            | Self::UnexpectedEof { span }
            | Self::NestingTooDeep { span } => Some(*span),
            Self::Empty => None,
            Self::Lex(e) => Some(e.span()),
        }
    }
}

/// Result alias for the parser.
pub type ParseResult<T> = std::result::Result<T, ParseError>;

/// Parse a Cypher source string into a [`Query`].
///
/// # Errors
///
/// Returns the first lexical or syntactic problem encountered.
///
/// # Example
///
/// ```
/// use drevo::cypher::parser::parse;
/// let q = parse("MATCH (n) RETURN n").unwrap();
/// assert_eq!(q.parts.len(), 1);
/// ```
pub fn parse(source: &str) -> ParseResult<Query> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(source, tokens);
    let query = parser.parse_query()?;
    parser.ensure_eof()?;
    Ok(query)
}

/// `true` when `clause` is an update clause permitted inside a
/// `FOREACH` body (`CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, or a
/// nested `FOREACH`). Read clauses are excluded.
fn is_update_clause(clause: &Clause) -> bool {
    matches!(
        clause,
        Clause::Create(_)
            | Clause::Merge(_)
            | Clause::Set(_)
            | Clause::Remove(_)
            | Clause::Delete(_)
            | Clause::Foreach(_)
    )
}

/// The keyword that introduced `clause`, for diagnostics.
fn clause_keyword(clause: &Clause) -> &'static str {
    match clause {
        Clause::Match(_) => "MATCH",
        Clause::Create(_) => "CREATE",
        Clause::Merge(_) => "MERGE",
        Clause::Delete(_) => "DELETE",
        Clause::Set(_) => "SET",
        Clause::Remove(_) => "REMOVE",
        Clause::With(_) => "WITH",
        Clause::Return(_) => "RETURN",
        Clause::Unwind(_) => "UNWIND",
        Clause::Foreach(_) => "FOREACH",
        Clause::Call(_) => "CALL",
    }
}

/// Map a bare identifier to a list predicate quantifier, case-insensitively.
///
/// Only `any` / `none` / `single` are matched here; `all` is a dedicated
/// keyword token and is dispatched separately in `parse_prefix`.
fn list_predicate_kind(name: &str) -> Option<ListPredicateKind> {
    match name.to_ascii_lowercase().as_str() {
        "any" => Some(ListPredicateKind::Any),
        "none" => Some(ListPredicateKind::None),
        "single" => Some(ListPredicateKind::Single),
        _ => None,
    }
}

/// Internal parser state.
/// Maximum expression-nesting depth before the recursive-descent parser bails
/// with a recoverable [`ParseError::NestingTooDeep`] rather than overflowing
/// the stack. Each nesting level (`(`, a prefix operator, a list element, a
/// subquery predicate) costs one frame through [`Parser::parse_expression_bp`],
/// and the speculative pattern-parsing paths make those frames stack-hungry, so
/// adversarial input like `(((…)))` would abort the process around a few
/// hundred levels. `64` is orders of magnitude beyond any hand-written query
/// yet leaves ample head-room on the small (2 MiB) stacks used by test threads
/// and the libFuzzer harness.
const MAX_EXPRESSION_DEPTH: usize = 64;

struct Parser {
    /// The original query text, kept so identifier-position keywords can be
    /// recovered with their *written* casing via their token span (the
    /// lexer is case-insensitive on keywords, so the `TokenKind` alone has
    /// lost the original casing). See [`Parser::consume_name`].
    source: String,
    tokens: Vec<Token>,
    pos: usize,
    /// Current expression-recursion depth, bounded by [`MAX_EXPRESSION_DEPTH`].
    /// Incremented on entry to [`Parser::parse_expression_bp`] and decremented
    /// on exit, so it tracks live recursion regardless of the `?` early-return
    /// paths.
    depth: usize,
}

impl Parser {
    fn new(source: &str, tokens: Vec<Token>) -> Self {
        Self {
            source: source.to_string(),
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    // ---- Token-stream primitives ----------------------------------------

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        let idx = self.pos + offset;
        if idx < self.tokens.len() {
            &self.tokens[idx].kind
        } else {
            // Always-present Eof sentinel makes this branch unreachable in
            // practice, but defensively return Eof anyway.
            &self.tokens[self.tokens.len() - 1].kind
        }
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn consume(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if !matches!(tok.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        tok
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek_kind()) == std::mem::discriminant(kind)
    }

    fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.consume();
            true
        } else {
            false
        }
    }

    fn eat(&mut self, kind: &TokenKind, what: &str) -> ParseResult<Token> {
        if self.check(kind) {
            Ok(self.consume())
        } else {
            Err(ParseError::Expected {
                expected: what.to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            })
        }
    }

    fn ensure_eof(&self) -> ParseResult<()> {
        if self.at_eof() {
            Ok(())
        } else {
            Err(ParseError::Expected {
                expected: "end of input".to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            })
        }
    }

    // ---- Identifier extraction with soft-keyword policy -----------------

    /// Consume an identifier. Does NOT accept keywords.
    fn consume_strict_identifier(&mut self) -> ParseResult<(String, Span)> {
        match self.peek_kind() {
            TokenKind::Identifier(_) => {
                let tok = self.consume();
                let name = match tok.kind {
                    TokenKind::Identifier(s) => s,
                    _ => unreachable!(),
                };
                Ok((name, tok.span))
            }
            _ => Err(ParseError::ExpectedIdentifier {
                span: self.peek_span(),
            }),
        }
    }

    /// Consume a "name" — an identifier or any keyword acting as an
    /// identifier. Used for property names, label names, relationship
    /// types, map keys, and function names.
    fn consume_name(&mut self) -> ParseResult<(String, Span)> {
        let tok = self.tokens[self.pos].clone();
        match &tok.kind {
            TokenKind::Identifier(s) => {
                let s = s.clone();
                self.consume();
                Ok((s, tok.span))
            }
            kind if kind.is_keyword() => {
                // A reserved keyword used in an identifier position (label,
                // relationship type, property / map key). The lexer matches
                // keywords case-insensitively, so the `TokenKind` has lost
                // the original casing — recover the *written* text from the
                // source by the token's byte span. This keeps `[:CONTAINS]`
                // a `CONTAINS` relationship type and `n.In` an `In` property
                // (Neo4j preserves the written casing of labels / types /
                // property keys; only keyword *recognition* is
                // case-insensitive). Spans are byte offsets and keywords are
                // ASCII, so the slice is always on a char boundary.
                let text = self.source[tok.span.start..tok.span.end].to_string();
                self.consume();
                Ok((text, tok.span))
            }
            _ => Err(ParseError::ExpectedIdentifier { span: tok.span }),
        }
    }

    // ---- Query / UNION --------------------------------------------------

    fn parse_query(&mut self) -> ParseResult<Query> {
        if self.at_eof() {
            return Err(ParseError::Empty);
        }
        let mut parts = Vec::new();
        let first = self.parse_single_query()?;
        parts.push(UnionPart {
            union: None,
            query: first,
        });
        while matches!(self.peek_kind(), TokenKind::Union) {
            self.consume(); // UNION
            let kind = if matches!(self.peek_kind(), TokenKind::All) {
                self.consume();
                UnionKind::All
            } else {
                UnionKind::Distinct
            };
            let next = self.parse_single_query()?;
            parts.push(UnionPart {
                union: Some(kind),
                query: next,
            });
        }
        // Optional trailing semicolon — Cypher allows one.
        let _ = self.match_kind(&TokenKind::Semicolon);
        Ok(Query { parts })
    }

    fn parse_single_query(&mut self) -> ParseResult<SingleQuery> {
        let mut clauses = Vec::new();
        loop {
            if self.at_eof() || matches!(self.peek_kind(), TokenKind::Union | TokenKind::Semicolon)
            {
                break;
            }
            let clause = self.parse_clause()?;
            clauses.push(clause);
        }
        if clauses.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(SingleQuery { clauses })
    }

    // ---- Clauses --------------------------------------------------------

    fn parse_clause(&mut self) -> ParseResult<Clause> {
        match self.peek_kind() {
            TokenKind::Match => self.parse_match(false),
            TokenKind::Optional => {
                // Lexer emits OPTIONAL and MATCH as separate tokens.
                self.consume();
                if !matches!(self.peek_kind(), TokenKind::Match) {
                    return Err(ParseError::Expected {
                        expected: "MATCH after OPTIONAL".to_string(),
                        found: format!("{}", self.peek_kind()),
                        span: self.peek_span(),
                    });
                }
                self.parse_match(true)
            }
            TokenKind::Create => self.parse_create(),
            TokenKind::Merge => self.parse_merge(),
            TokenKind::Delete => self.parse_delete(false),
            TokenKind::Detach => {
                let span = self.peek_span();
                self.consume();
                if !matches!(self.peek_kind(), TokenKind::Delete) {
                    return Err(ParseError::Expected {
                        expected: "DELETE after DETACH".to_string(),
                        found: format!("{}", self.peek_kind()),
                        span: self.peek_span(),
                    });
                }
                self.parse_delete_with_span(true, span)
            }
            TokenKind::Set => self.parse_set(),
            TokenKind::Remove => self.parse_remove(),
            TokenKind::With => self.parse_with(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Unwind => self.parse_unwind(),
            TokenKind::Foreach => self.parse_foreach(),
            TokenKind::Call => self.parse_call(),
            _ => Err(ParseError::Expected {
                expected: "clause keyword (MATCH, CREATE, MERGE, DELETE, SET, REMOVE, WITH, RETURN, UNWIND, FOREACH, CALL, OPTIONAL, DETACH)".to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            }),
        }
    }

    fn parse_match(&mut self, optional: bool) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume(); // MATCH
        let patterns = self.parse_pattern_list()?;
        let where_clause = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(Clause::Match(MatchClause {
            optional,
            patterns,
            where_clause,
            span,
        }))
    }

    fn parse_create(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let patterns = self.parse_pattern_list()?;
        Ok(Clause::Create(CreateClause { patterns, span }))
    }

    fn parse_merge(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let pattern = self.parse_named_pattern()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while matches!(self.peek_kind(), TokenKind::On) {
            self.consume();
            match self.peek_kind() {
                TokenKind::Create => {
                    self.consume();
                    self.eat(&TokenKind::Set, "SET after ON CREATE")?;
                    let items = self.parse_set_items()?;
                    on_create.extend(items);
                }
                TokenKind::Match => {
                    self.consume();
                    self.eat(&TokenKind::Set, "SET after ON MATCH")?;
                    let items = self.parse_set_items()?;
                    on_match.extend(items);
                }
                _ => {
                    return Err(ParseError::Expected {
                        expected: "CREATE or MATCH after ON".to_string(),
                        found: format!("{}", self.peek_kind()),
                        span: self.peek_span(),
                    });
                }
            }
        }
        Ok(Clause::Merge(MergeClause {
            pattern,
            on_create,
            on_match,
            span,
        }))
    }

    fn parse_delete(&mut self, detach: bool) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.parse_delete_with_span(detach, span)
    }

    fn parse_delete_with_span(&mut self, detach: bool, span: Span) -> ParseResult<Clause> {
        self.consume(); // DELETE (DETACH was already consumed if detach)
        let targets = self.parse_expression_list()?;
        Ok(Clause::Delete(DeleteClause {
            detach,
            targets,
            span,
        }))
    }

    fn parse_set(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let items = self.parse_set_items()?;
        Ok(Clause::Set(SetClause { items, span }))
    }

    fn parse_set_items(&mut self) -> ParseResult<Vec<SetItem>> {
        let mut items = Vec::new();
        items.push(self.parse_set_item()?);
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            items.push(self.parse_set_item()?);
        }
        Ok(items)
    }

    fn parse_set_item(&mut self) -> ParseResult<SetItem> {
        // The LHS of a SET item is a property-access chain or a bare
        // variable — never a full expression with arithmetic / comparison
        // (those operators would swallow the `=` we need for the
        // assignment). Use [`parse_postfix_chain`] for that.
        let target = self.parse_postfix_chain()?;
        match self.peek_kind() {
            TokenKind::Eq => {
                self.consume();
                let value = self.parse_expression()?;
                // Distinguish Replace (target is a bare variable) from
                // Property (target is a Property expression). Both kinds
                // hit this branch.
                let item = if matches!(target, Expression::Property { .. }) {
                    SetItem::Property { target, value }
                } else {
                    SetItem::Replace { target, value }
                };
                Ok(item)
            }
            TokenKind::Plus if matches!(self.peek_at(1), TokenKind::Eq) => {
                self.consume(); // +
                self.consume(); // =
                let value = self.parse_expression()?;
                Ok(SetItem::Merge { target, value })
            }
            TokenKind::Colon => {
                let labels = self.parse_label_chain()?;
                Ok(SetItem::Labels { target, labels })
            }
            _ => Err(ParseError::Expected {
                expected: "`=`, `+=`, or `:Label` in SET item".to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            }),
        }
    }

    fn parse_remove(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let mut items = Vec::new();
        items.push(self.parse_remove_item()?);
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            items.push(self.parse_remove_item()?);
        }
        Ok(Clause::Remove(RemoveClause { items, span }))
    }

    fn parse_remove_item(&mut self) -> ParseResult<RemoveItem> {
        let target = self.parse_postfix_chain()?;
        if matches!(self.peek_kind(), TokenKind::Colon) {
            let labels = self.parse_label_chain()?;
            Ok(RemoveItem::Labels { target, labels })
        } else if matches!(target, Expression::Property { .. }) {
            Ok(RemoveItem::Property(target))
        } else {
            Err(ParseError::Malformed {
                message: "REMOVE expects a property or labels".to_string(),
                span: target.span(),
            })
        }
    }

    fn parse_with(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let distinct = self.match_kind(&TokenKind::Distinct);
        let items = self.parse_projection_list()?;
        let (order_by, skip, limit) = self.parse_post_projection()?;
        let where_clause = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(Clause::With(WithClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
            where_clause,
            span,
        }))
    }

    fn parse_return(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let distinct = self.match_kind(&TokenKind::Distinct);
        let items = self.parse_projection_list()?;
        let (order_by, skip, limit) = self.parse_post_projection()?;
        Ok(Clause::Return(ReturnClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
            span,
        }))
    }

    fn parse_unwind(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume();
        let expression = self.parse_expression()?;
        self.eat(&TokenKind::As, "AS in UNWIND")?;
        let (alias, _) = self.consume_strict_identifier()?;
        Ok(Clause::Unwind(UnwindClause {
            expression,
            alias,
            span,
        }))
    }

    /// `FOREACH (variable IN list | update_clause [update_clause …])`.
    ///
    /// The body is one or more update clauses (`CREATE` / `MERGE` / `SET`
    /// / `REMOVE` / `DELETE` / nested `FOREACH`). Read clauses (`MATCH`,
    /// `RETURN`, `WITH`, `UNWIND`) are rejected here at parse time so the
    /// grammar mirrors Neo4j, which only permits updates inside `FOREACH`.
    fn parse_foreach(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume(); // FOREACH
        self.eat(&TokenKind::LParen, "( after FOREACH")?;
        let (variable, _) = self.consume_strict_identifier()?;
        self.eat(&TokenKind::In, "IN in FOREACH")?;
        let list = self.parse_expression()?;
        self.eat(&TokenKind::Pipe, "| in FOREACH")?;
        let mut clauses = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen) {
            if matches!(self.peek_kind(), TokenKind::Eof) {
                return Err(ParseError::Expected {
                    expected: ") to close FOREACH body".to_string(),
                    found: format!("{}", self.peek_kind()),
                    span: self.peek_span(),
                });
            }
            let clause = self.parse_clause()?;
            if !is_update_clause(&clause) {
                return Err(ParseError::Expected {
                    expected:
                        "update clause inside FOREACH (CREATE, MERGE, SET, REMOVE, DELETE, FOREACH)"
                            .to_string(),
                    found: clause_keyword(&clause).to_string(),
                    span,
                });
            }
            clauses.push(clause);
        }
        self.eat(&TokenKind::RParen, ") to close FOREACH")?;
        if clauses.is_empty() {
            return Err(ParseError::Expected {
                expected: "at least one update clause inside FOREACH".to_string(),
                found: ")".to_string(),
                span,
            });
        }
        Ok(Clause::Foreach(ForeachClause {
            variable,
            list,
            clauses,
            span,
        }))
    }

    /// `CALL proc.name(arg, …) [YIELD col [AS alias], … [WHERE pred]]`.
    ///
    /// The procedure name is one or more dot-separated identifiers
    /// (`db.labels`). Arguments are an ordinary comma-separated expression
    /// list (empty for the built-in introspection procedures). `YIELD`,
    /// when present, names the output columns to bring into scope, each
    /// optionally renamed with `AS`, and may be followed by a `WHERE`
    /// predicate that filters the yielded rows.
    fn parse_call(&mut self) -> ParseResult<Clause> {
        let span = self.peek_span();
        self.consume(); // CALL
        let (first, _) = self.consume_strict_identifier()?;
        let mut name = vec![first];
        while matches!(self.peek_kind(), TokenKind::Dot) {
            self.consume(); // .
            let (segment, _) = self.consume_strict_identifier()?;
            name.push(segment);
        }
        self.eat(&TokenKind::LParen, "( after procedure name")?;
        let mut args = Vec::new();
        if !matches!(self.peek_kind(), TokenKind::RParen) {
            args.push(self.parse_expression()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                args.push(self.parse_expression()?);
            }
        }
        self.eat(&TokenKind::RParen, ") to close procedure arguments")?;

        let (yields, where_clause) = if matches!(self.peek_kind(), TokenKind::Yield) {
            self.consume(); // YIELD
            let mut items = Vec::new();
            items.push(self.parse_yield_item()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                items.push(self.parse_yield_item()?);
            }
            let where_clause = if matches!(self.peek_kind(), TokenKind::Where) {
                self.consume();
                Some(self.parse_expression()?)
            } else {
                None
            };
            (Some(items), where_clause)
        } else {
            (None, None)
        };

        Ok(Clause::Call(CallClause {
            name,
            args,
            yields,
            where_clause,
            span,
        }))
    }

    /// One `YIELD` item: `col [AS alias]`.
    fn parse_yield_item(&mut self) -> ParseResult<YieldItem> {
        let (name, span) = self.consume_strict_identifier()?;
        let alias = if matches!(self.peek_kind(), TokenKind::As) {
            self.consume();
            let (alias, _) = self.consume_strict_identifier()?;
            Some(alias)
        } else {
            None
        };
        Ok(YieldItem { name, alias, span })
    }

    // ---- Projection helpers --------------------------------------------

    fn parse_projection_list(&mut self) -> ParseResult<Vec<ProjectionItem>> {
        let mut items = Vec::new();
        items.push(self.parse_projection_item()?);
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            items.push(self.parse_projection_item()?);
        }
        Ok(items)
    }

    fn parse_projection_item(&mut self) -> ParseResult<ProjectionItem> {
        if matches!(self.peek_kind(), TokenKind::Star) {
            // `*` alone is the wildcard. But `*` can also legally appear
            // inside an expression as multiplication — except as the FIRST
            // token of a projection it is always the wildcard (Cypher
            // grammar). The look-ahead-1 check is enough.
            self.consume();
            return Ok(ProjectionItem::Star);
        }
        let expr = self.parse_expression()?;
        let alias = if matches!(self.peek_kind(), TokenKind::As) {
            self.consume();
            let (name, _) = self.consume_strict_identifier()?;
            Some(name)
        } else {
            None
        };
        Ok(ProjectionItem::Expression { expr, alias })
    }

    fn parse_post_projection(
        &mut self,
    ) -> ParseResult<(Vec<OrderItem>, Option<Expression>, Option<Expression>)> {
        let mut order_by = Vec::new();
        if matches!(self.peek_kind(), TokenKind::Order) {
            self.consume();
            self.eat(&TokenKind::By, "BY after ORDER")?;
            order_by.push(self.parse_order_item()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                order_by.push(self.parse_order_item()?);
            }
        }
        let skip = if matches!(self.peek_kind(), TokenKind::Skip) {
            self.consume();
            Some(self.parse_expression()?)
        } else {
            None
        };
        let limit = if matches!(self.peek_kind(), TokenKind::Limit) {
            self.consume();
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok((order_by, skip, limit))
    }

    fn parse_order_item(&mut self) -> ParseResult<OrderItem> {
        let expression = self.parse_expression()?;
        let direction = match self.peek_kind() {
            TokenKind::Asc => {
                self.consume();
                OrderDirection::Asc
            }
            TokenKind::Desc => {
                self.consume();
                OrderDirection::Desc
            }
            _ => OrderDirection::Asc,
        };
        Ok(OrderItem {
            expression,
            direction,
        })
    }

    fn parse_expression_list(&mut self) -> ParseResult<Vec<Expression>> {
        let mut exprs = Vec::new();
        exprs.push(self.parse_expression()?);
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            exprs.push(self.parse_expression()?);
        }
        Ok(exprs)
    }

    // ---- Patterns -------------------------------------------------------

    fn parse_pattern_list(&mut self) -> ParseResult<Vec<NamedPattern>> {
        let mut patterns = Vec::new();
        patterns.push(self.parse_named_pattern()?);
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            patterns.push(self.parse_named_pattern()?);
        }
        Ok(patterns)
    }

    fn parse_named_pattern(&mut self) -> ParseResult<NamedPattern> {
        // Optional `name =` prefix for path-binding.
        let variable = if matches!(self.peek_kind(), TokenKind::Identifier(_))
            && matches!(self.peek_at(1), TokenKind::Eq)
        {
            let (name, _) = self.consume_strict_identifier()?;
            self.consume(); // =
            Some(name)
        } else {
            None
        };
        // Optional `shortestPath( … )` / `allShortestPaths( … )` wrapper.
        // Both are ordinary identifiers (not keywords); we claim the
        // wrapper form only on the exact name immediately followed by `(`,
        // so a node variable or label literally named `shortestpath` is
        // unaffected.
        let shortest = self.peek_shortest_kind();
        let path = if shortest.is_some() {
            self.consume(); // shortestPath / allShortestPaths
            self.eat(
                &TokenKind::LParen,
                "`(` after shortestPath/allShortestPaths",
            )?;
            let path = self.parse_path_pattern()?;
            self.eat(
                &TokenKind::RParen,
                "`)` to close shortestPath/allShortestPaths",
            )?;
            path
        } else {
            self.parse_path_pattern()?
        };
        Ok(NamedPattern {
            variable,
            path,
            shortest,
        })
    }

    /// Peek for a `shortestPath(` / `allShortestPaths(` wrapper at the
    /// current position without consuming anything. Matching is
    /// case-insensitive (Cypher function names are) and requires the name
    /// to be immediately followed by `(`.
    fn peek_shortest_kind(&self) -> Option<ShortestKind> {
        if let TokenKind::Identifier(name) = self.peek_kind() {
            if matches!(self.peek_at(1), TokenKind::LParen) {
                return match name.to_ascii_lowercase().as_str() {
                    "shortestpath" => Some(ShortestKind::Single),
                    "allshortestpaths" => Some(ShortestKind::All),
                    _ => None,
                };
            }
        }
        None
    }

    fn parse_path_pattern(&mut self) -> ParseResult<PathPattern> {
        let head = self.parse_node_pattern()?;
        let mut tail = Vec::new();
        while self.at_relationship_start() {
            let relationship = self.parse_relationship_pattern()?;
            let node = self.parse_node_pattern()?;
            tail.push(PathSegment { relationship, node });
        }
        Ok(PathPattern { head, tail })
    }

    fn at_relationship_start(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Minus | TokenKind::LArrow)
    }

    fn parse_node_pattern(&mut self) -> ParseResult<NodePattern> {
        let span = self.peek_span();
        self.eat(&TokenKind::LParen, "`(` to open node pattern")?;
        let variable = if matches!(self.peek_kind(), TokenKind::Identifier(_)) {
            let (name, _) = self.consume_strict_identifier()?;
            Some(name)
        } else {
            None
        };
        let labels = if matches!(self.peek_kind(), TokenKind::Colon) {
            self.parse_label_chain()?
        } else {
            Vec::new()
        };
        let properties = if matches!(self.peek_kind(), TokenKind::LBrace) {
            Some(self.parse_map_literal()?)
        } else {
            None
        };
        self.eat(&TokenKind::RParen, "`)` to close node pattern")?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
            span,
        })
    }

    fn parse_relationship_pattern(&mut self) -> ParseResult<RelationshipPattern> {
        // Direction prefix: `-` or `<-`.
        let span = self.peek_span();
        let left_incoming = match self.peek_kind() {
            TokenKind::LArrow => {
                self.consume();
                true
            }
            TokenKind::Minus => {
                self.consume();
                false
            }
            _ => unreachable!("at_relationship_start guards entry"),
        };

        let mut variable = None;
        let mut types = Vec::new();
        let mut length = None;
        let mut properties = None;

        // Optional detail in [...]
        if matches!(self.peek_kind(), TokenKind::LBracket) {
            self.consume();
            // Variable
            if matches!(self.peek_kind(), TokenKind::Identifier(_)) {
                let (name, _) = self.consume_strict_identifier()?;
                variable = Some(name);
            }
            // Types
            if matches!(self.peek_kind(), TokenKind::Colon) {
                self.consume();
                let (t, _) = self.consume_name()?;
                types.push(t);
                while matches!(self.peek_kind(), TokenKind::Pipe) {
                    self.consume();
                    // Optional leading `:` after `|`.
                    let _ = self.match_kind(&TokenKind::Colon);
                    let (t, _) = self.consume_name()?;
                    types.push(t);
                }
            }
            // Range (`*N` / `*N..M`)
            if matches!(self.peek_kind(), TokenKind::Star) {
                self.consume();
                length = Some(self.parse_rel_length()?);
            }
            // Properties
            if matches!(self.peek_kind(), TokenKind::LBrace) {
                properties = Some(self.parse_map_literal()?);
            }
            self.eat(&TokenKind::RBracket, "`]` to close relationship detail")?;
        }

        // Suffix: `-` or `->`.
        let right_outgoing = match self.peek_kind() {
            TokenKind::Minus => {
                self.consume();
                false
            }
            TokenKind::Arrow => {
                self.consume();
                true
            }
            _ => {
                return Err(ParseError::Expected {
                    expected: "`-` or `->` to close relationship pattern".to_string(),
                    found: format!("{}", self.peek_kind()),
                    span: self.peek_span(),
                });
            }
        };

        let direction = match (left_incoming, right_outgoing) {
            (false, true) => Direction::Outgoing,
            (true, false) => Direction::Incoming,
            (false, false) => Direction::Undirected,
            (true, true) => {
                return Err(ParseError::Malformed {
                    message: "relationship has both `<-` and `->`".to_string(),
                    span,
                });
            }
        };

        Ok(RelationshipPattern {
            direction,
            variable,
            types,
            length,
            properties,
            span,
        })
    }

    fn parse_rel_length(&mut self) -> ParseResult<RelLength> {
        // Cursor is positioned right after `*`. Cases:
        //   `]`            → Any
        //   `N`            → Exact(N) (unless followed by `..`)
        //   `N..M`         → Range
        //   `N..`          → Range from N to unbounded
        //   `..M`          → Range from unbounded to M
        //   `..`           → equivalent to Any with both ends loose
        match self.peek_kind() {
            TokenKind::RBracket | TokenKind::LBrace => Ok(RelLength::Any),
            TokenKind::DotDot => {
                self.consume();
                let to = self.try_parse_int_literal()?;
                Ok(RelLength::Range { from: None, to })
            }
            TokenKind::Integer(n) => {
                let from_val = *n;
                self.consume();
                if matches!(self.peek_kind(), TokenKind::DotDot) {
                    self.consume();
                    let to = self.try_parse_int_literal()?;
                    Ok(RelLength::Range {
                        from: Some(from_val),
                        to,
                    })
                } else {
                    Ok(RelLength::Exact(from_val))
                }
            }
            _ => Err(ParseError::Expected {
                expected: "integer, `..`, or `]` after `*` in relationship length".to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            }),
        }
    }

    fn try_parse_int_literal(&mut self) -> ParseResult<Option<i64>> {
        if let TokenKind::Integer(n) = *self.peek_kind() {
            self.consume();
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }

    fn parse_label_chain(&mut self) -> ParseResult<Vec<String>> {
        let mut labels = Vec::new();
        // Caller has *not* consumed the colon yet.
        while matches!(self.peek_kind(), TokenKind::Colon) {
            self.consume();
            let (l, _) = self.consume_name()?;
            labels.push(l);
        }
        if labels.is_empty() {
            return Err(ParseError::Expected {
                expected: "at least one `:Label`".to_string(),
                found: format!("{}", self.peek_kind()),
                span: self.peek_span(),
            });
        }
        Ok(labels)
    }

    fn parse_map_literal(&mut self) -> ParseResult<MapLiteral> {
        let span = self.peek_span();
        self.eat(&TokenKind::LBrace, "`{` to open map literal")?;
        let mut entries = Vec::new();
        if !matches!(self.peek_kind(), TokenKind::RBrace) {
            entries.push(self.parse_map_entry()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                entries.push(self.parse_map_entry()?);
            }
        }
        self.eat(&TokenKind::RBrace, "`}` to close map literal")?;
        Ok(MapLiteral { entries, span })
    }

    fn parse_map_entry(&mut self) -> ParseResult<(String, Expression)> {
        let (key, _) = self.consume_name()?;
        self.eat(&TokenKind::Colon, "`:` between map key and value")?;
        let value = self.parse_expression()?;
        Ok((key, value))
    }

    // ---- Expressions (Pratt) -------------------------------------------

    fn parse_expression(&mut self) -> ParseResult<Expression> {
        self.parse_expression_bp(0)
    }

    /// Parse a prefix expression plus the postfix chain (`.name`,
    /// `[index]`, `[from..to]`) but NO infix operators. Used as the LHS
    /// of `SET` / `REMOVE` items where the trailing `=` / `+=` / `:Label`
    /// must not be consumed.
    fn parse_postfix_chain(&mut self) -> ParseResult<Expression> {
        // parse_prefix already wraps in the postfix loop, but it also
        // bottoms out on identifier-or-call. That's exactly what we want
        // for an assignment LHS, so we can reuse it directly.
        self.parse_prefix()
    }

    /// Pratt parser. `min_bp` is the minimum binding power the right side
    /// must beat to continue absorbing operators.
    ///
    /// This is the single funnel for all expression recursion — grouped
    /// expressions `(…)`, prefix operators, list elements, subquery
    /// predicates, and infix right-hand sides all re-enter here — so the
    /// [`MAX_EXPRESSION_DEPTH`] guard is applied at this one place. The body
    /// lives in [`Parser::parse_expression_bp_inner`]; this wrapper only
    /// maintains the depth counter so every `?` early-return still decrements.
    fn parse_expression_bp(&mut self, min_bp: u8) -> ParseResult<Expression> {
        self.depth += 1;
        if self.depth > MAX_EXPRESSION_DEPTH {
            self.depth -= 1;
            return Err(ParseError::NestingTooDeep {
                span: self.peek_span(),
            });
        }
        let result = self.parse_expression_bp_inner(min_bp);
        self.depth -= 1;
        result
    }

    fn parse_expression_bp_inner(&mut self, min_bp: u8) -> ParseResult<Expression> {
        let mut lhs = self.parse_prefix()?;

        // Postfix forms with high precedence (`.`, `[`, `(`) are handled
        // inside parse_prefix's call chain. This loop handles binary infix
        // and the trailing `IS NULL` / `IN` / `STARTS WITH` predicates.
        while let Some((op, lbp, rbp)) = self.peek_infix_op() {
            if lbp < min_bp {
                break;
            }

            let op_span = self.peek_span();
            // STARTS WITH / ENDS WITH need a two-token match.
            match op {
                InfixOp::Binary(BinaryOp::StartsWith) | InfixOp::Binary(BinaryOp::EndsWith) => {
                    // Consume `STARTS` or `ENDS`, then expect `WITH`.
                    self.consume();
                    self.eat(&TokenKind::With, "WITH after STARTS/ENDS")?;
                }
                InfixOp::IsNullPrefix => {
                    // Consume IS, then optional NOT, then NULL.
                    self.consume();
                    let negated = self.match_kind(&TokenKind::Not);
                    self.eat(&TokenKind::Null, "NULL after IS [NOT]")?;
                    lhs = Expression::IsNull {
                        expr: Box::new(lhs),
                        negated,
                        span: op_span,
                    };
                    continue;
                }
                InfixOp::InPrefix => {
                    self.consume(); // IN
                    let list = self.parse_expression_bp(rbp)?;
                    lhs = Expression::In {
                        expr: Box::new(lhs),
                        list: Box::new(list),
                        span: op_span,
                    };
                    continue;
                }
                _ => {
                    self.consume();
                }
            }

            let rhs = self.parse_expression_bp(rbp)?;
            let op = match op {
                InfixOp::Binary(b) => b,
                _ => unreachable!("non-binary handled above"),
            };
            lhs = Expression::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span: op_span,
            };
        }

        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> ParseResult<Expression> {
        let tok_span = self.peek_span();
        let mut expr = match self.peek_kind().clone() {
            TokenKind::Integer(n) => {
                self.consume();
                Expression::Integer(n, tok_span)
            }
            TokenKind::Float(n) => {
                self.consume();
                Expression::Float(n, tok_span)
            }
            TokenKind::String(s) => {
                self.consume();
                Expression::String(s, tok_span)
            }
            TokenKind::True => {
                self.consume();
                Expression::True(tok_span)
            }
            TokenKind::False => {
                self.consume();
                Expression::False(tok_span)
            }
            TokenKind::Null => {
                self.consume();
                Expression::Null(tok_span)
            }
            TokenKind::Parameter(p) => {
                self.consume();
                Expression::Parameter(p, tok_span)
            }
            TokenKind::Star => {
                self.consume();
                Expression::Star(tok_span)
            }
            TokenKind::Minus => {
                self.consume();
                let rhs = self.parse_expression_bp(PREFIX_BP)?;
                Expression::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(rhs),
                    span: tok_span,
                }
            }
            TokenKind::Plus => {
                self.consume();
                let rhs = self.parse_expression_bp(PREFIX_BP)?;
                Expression::Unary {
                    op: UnaryOp::Plus,
                    expr: Box::new(rhs),
                    span: tok_span,
                }
            }
            TokenKind::Not => {
                self.consume();
                let rhs = self.parse_expression_bp(NOT_BP)?;
                Expression::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(rhs),
                    span: tok_span,
                }
            }
            TokenKind::LParen => {
                // A `(` opens either a grouped expression (`(a + 1)`) or a
                // pattern predicate (`(a)-[:R]->(b)` — an existence test). Both
                // start with `(`, so we *speculatively* parse a path pattern and
                // only commit when it has at least one relationship; otherwise
                // the cursor is rolled back and the `(` falls through to the
                // ordinary grouped-expression path.
                if let Some(expr) = self.try_parse_pattern_predicate(tok_span)? {
                    expr
                } else {
                    self.consume();
                    let inner = self.parse_expression()?;
                    self.eat(&TokenKind::RParen, "`)` to close grouped expression")?;
                    inner
                }
            }
            TokenKind::LBracket => self.parse_list_literal()?,
            TokenKind::LBrace => Expression::Map(self.parse_map_literal()?),
            TokenKind::Case => self.parse_case()?,
            // `all(x IN list WHERE pred)` — `ALL` is a keyword token (it also
            // appears in `UNION ALL`), so the identifier path below never sees
            // it; the predicate form is the only use of `ALL` in expression
            // position.
            TokenKind::All if matches!(self.peek_at(1), TokenKind::LParen) => {
                self.consume(); // ALL
                self.parse_list_predicate(ListPredicateKind::All, tok_span)?
            }
            // `EXISTS { [MATCH] pattern [WHERE pred] }` — an existential
            // subquery. `EXISTS` is a reserved keyword token, so it never
            // reaches the identifier path; the brace form is its only use in
            // expression position (the deprecated `exists(n.prop)` function form
            // is replaced by `n.prop IS NOT NULL`).
            TokenKind::Exists => {
                self.consume(); // EXISTS
                self.parse_exists_subquery(tok_span)?
            }
            TokenKind::Identifier(_) => self.parse_identifier_or_call()?,
            TokenKind::Eof => {
                return Err(ParseError::ExpectedExpression { span: tok_span });
            }
            _ => {
                return Err(ParseError::ExpectedExpression { span: tok_span });
            }
        };

        // Postfix: `.name`, `[index]`, `[from..to]`, ... .
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.consume();
                    let (name, name_span) = self.consume_name()?;
                    expr = Expression::Property {
                        base: Box::new(expr),
                        name,
                        span: name_span,
                    };
                }
                TokenKind::LBracket => {
                    let lb_span = self.peek_span();
                    self.consume();
                    // Slice forms: `..to`, `from..to`, `from..`, or
                    // index form: `from`.
                    if matches!(self.peek_kind(), TokenKind::DotDot) {
                        self.consume();
                        let to = if matches!(self.peek_kind(), TokenKind::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.parse_expression()?))
                        };
                        self.eat(&TokenKind::RBracket, "`]` to close slice")?;
                        expr = Expression::Slice {
                            base: Box::new(expr),
                            from: None,
                            to,
                            span: lb_span,
                        };
                    } else {
                        let first = self.parse_expression()?;
                        if matches!(self.peek_kind(), TokenKind::DotDot) {
                            self.consume();
                            let to = if matches!(self.peek_kind(), TokenKind::RBracket) {
                                None
                            } else {
                                Some(Box::new(self.parse_expression()?))
                            };
                            self.eat(&TokenKind::RBracket, "`]` to close slice")?;
                            expr = Expression::Slice {
                                base: Box::new(expr),
                                from: Some(Box::new(first)),
                                to,
                                span: lb_span,
                            };
                        } else {
                            self.eat(&TokenKind::RBracket, "`]` to close index")?;
                            expr = Expression::Index {
                                base: Box::new(expr),
                                index: Box::new(first),
                                span: lb_span,
                            };
                        }
                    }
                }
                // `base { .key, .*, key: expr, var }` — a map projection. A
                // `{` only reaches the postfix loop *after* a primary; a
                // standalone map literal is consumed by `parse_prefix`'s
                // `LBrace` arm before this loop runs, so claiming `{` here is
                // purely additive (an expression followed by `{` was a parse
                // error before this task).
                TokenKind::LBrace => {
                    expr = self.parse_map_projection(expr)?;
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    /// Parse a map projection `base { selector, … }` once `base` has been
    /// parsed and the cursor sits on the opening `{`.
    ///
    /// Selectors are, in any mix: `.key` (property), `.*` (all properties),
    /// `key: expr` (literal entry), or a bare `var` (shorthand for `var: var`).
    /// An empty `{}` projects to an empty map.
    fn parse_map_projection(&mut self, base: Expression) -> ParseResult<Expression> {
        let span = self.peek_span();
        self.eat(&TokenKind::LBrace, "`{` to open map projection")?;
        let mut selectors = Vec::new();
        if !matches!(self.peek_kind(), TokenKind::RBrace) {
            selectors.push(self.parse_map_projection_selector()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                selectors.push(self.parse_map_projection_selector()?);
            }
        }
        self.eat(&TokenKind::RBrace, "`}` to close map projection")?;
        Ok(Expression::MapProjection {
            base: Box::new(base),
            selectors,
            span,
        })
    }

    fn parse_map_projection_selector(&mut self) -> ParseResult<MapProjectionSelector> {
        // `.key` (property) or `.*` (all properties).
        if matches!(self.peek_kind(), TokenKind::Dot) {
            self.consume();
            if matches!(self.peek_kind(), TokenKind::Star) {
                self.consume();
                return Ok(MapProjectionSelector::AllProperties);
            }
            let (name, _) = self.consume_name()?;
            return Ok(MapProjectionSelector::Property(name));
        }
        // `key: expr` (literal entry) or bare `var` (variable shorthand).
        let (name, _) = self.consume_name()?;
        if matches!(self.peek_kind(), TokenKind::Colon) {
            self.consume();
            let value = self.parse_expression()?;
            Ok(MapProjectionSelector::Literal(name, value))
        } else {
            Ok(MapProjectionSelector::Variable(name))
        }
    }

    fn parse_list_literal(&mut self) -> ParseResult<Expression> {
        let span = self.peek_span();
        self.consume(); // [
        if matches!(self.peek_kind(), TokenKind::RBracket) {
            self.consume();
            return Ok(Expression::List {
                items: Vec::new(),
                span,
            });
        }
        // Pattern comprehension `[ (a)-[:R]->(b) WHERE pred | proj ]` — its first
        // token is `(`, the start of a node pattern. A list literal whose first
        // element is parenthesised (`[(1+2)]`, `[(a)]`, `[(a {x:1})]`) also opens
        // with `(`, so we *speculatively* parse a path pattern and only commit
        // when it has at least one relationship and is followed by `WHERE` / `|`;
        // otherwise the cursor is restored and the bracket falls through to the
        // ordinary expression / list-literal path below.
        if matches!(self.peek_kind(), TokenKind::LParen) {
            if let Some(expr) = self.try_parse_pattern_comprehension(span)? {
                return Ok(expr);
            }
        }
        // Parse the first element. For a list comprehension this naturally
        // parses the `variable IN list` prefix as an `In` expression (IN binds
        // tighter than the `WHERE` / `|` that follow); `parse_expression`
        // stops at `WHERE` / `|` since neither is an expression operator.
        let first = self.parse_expression()?;
        // `[var IN list WHERE pred | proj]` — a list comprehension is signalled
        // by a `WHERE` or `|` immediately after the `var IN list` prefix.
        if matches!(self.peek_kind(), TokenKind::Where | TokenKind::Pipe) {
            return self.finish_list_comprehension(first, span);
        }
        let mut items = vec![first];
        while matches!(self.peek_kind(), TokenKind::Comma) {
            self.consume();
            items.push(self.parse_expression()?);
        }
        self.eat(&TokenKind::RBracket, "`]` to close list literal")?;
        Ok(Expression::List { items, span })
    }

    /// Finish parsing a list comprehension once the leading `var IN list`
    /// prefix (`first`) has been recognised by a trailing `WHERE` / `|`.
    ///
    /// `first` must be `variable IN listExpr` with `variable` a bare
    /// identifier; otherwise the bracket form is malformed.
    fn finish_list_comprehension(
        &mut self,
        first: Expression,
        span: Span,
    ) -> ParseResult<Expression> {
        let Expression::In { expr, list, .. } = first else {
            return Err(ParseError::Malformed {
                message: "list comprehension must start with `variable IN list`".to_string(),
                span,
            });
        };
        let Expression::Variable(variable, _) = *expr else {
            return Err(ParseError::Malformed {
                message: "list comprehension variable must be a simple identifier".to_string(),
                span,
            });
        };
        let predicate = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        let projection = if matches!(self.peek_kind(), TokenKind::Pipe) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.eat(&TokenKind::RBracket, "`]` to close list comprehension")?;
        Ok(Expression::ListComprehension {
            variable,
            list,
            predicate,
            projection,
            span,
        })
    }

    /// Speculatively parse a pattern comprehension `[ pattern WHERE? | proj ]`
    /// when the cursor (just past the opening `[`) sits on `(`.
    ///
    /// The bracket is only a pattern comprehension when the leading `(`
    /// introduces a genuine **path** — a node pattern followed by at least one
    /// relationship — that is in turn followed by `WHERE` or `|`. A
    /// parenthesised expression element of a list literal (`[(1+2)]`, `[(a)]`,
    /// `[(a {x:1})]`) parses as a node pattern with no relationship, so it fails
    /// the "at least one relationship" test; on any non-match the speculative
    /// cursor is rolled back to `checkpoint` and `Ok(None)` is returned so the
    /// caller resumes ordinary list-literal parsing. Once the commit point
    /// (a valid path + `WHERE` / `|`) is reached, a later syntax error is a real
    /// error rather than a roll-back signal — `| projection` is mandatory.
    fn try_parse_pattern_comprehension(&mut self, span: Span) -> ParseResult<Option<Expression>> {
        let checkpoint = self.pos;
        let pattern = match self.parse_path_pattern() {
            Ok(p) => p,
            Err(_) => {
                self.pos = checkpoint;
                return Ok(None);
            }
        };
        // A pattern comprehension needs an actual relationship and a trailing
        // `WHERE` / `|`; anything else is a parenthesised list-literal element.
        if pattern.tail.is_empty()
            || !matches!(self.peek_kind(), TokenKind::Where | TokenKind::Pipe)
        {
            self.pos = checkpoint;
            return Ok(None);
        }
        let predicate = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.eat(
            &TokenKind::Pipe,
            "`|` before the pattern-comprehension projection",
        )?;
        let projection = Box::new(self.parse_expression()?);
        self.eat(&TokenKind::RBracket, "`]` to close pattern comprehension")?;
        Ok(Some(Expression::PatternComprehension {
            pattern: Box::new(pattern),
            predicate,
            projection,
            span,
        }))
    }

    /// Speculatively parse a pattern predicate `(a)-[:R]->(b)` when the cursor
    /// sits on `(` in expression position.
    ///
    /// A `(` opens both a grouped expression (`(a + 1)`, `(a)`, `(a).name`) and
    /// a pattern predicate (an existence test over a path). Only a path with at
    /// least one **relationship** is a predicate; a bare parenthesised node —
    /// which is what a grouped expression looks like to [`parse_path_pattern`] —
    /// fails the "at least one relationship" test, so on any non-match the
    /// speculative cursor is rolled back to `checkpoint` and `Ok(None)` is
    /// returned so the caller resumes ordinary grouped-expression parsing. This
    /// mirrors [`try_parse_pattern_comprehension`](Self::try_parse_pattern_comprehension)'s
    /// commit rule, keeping the change purely additive: any `(` that previously
    /// parsed as grouping still does.
    fn try_parse_pattern_predicate(&mut self, span: Span) -> ParseResult<Option<Expression>> {
        let checkpoint = self.pos;
        let pattern = match self.parse_path_pattern() {
            Ok(p) => p,
            Err(_) => {
                self.pos = checkpoint;
                return Ok(None);
            }
        };
        // Only a genuine path (≥ 1 relationship) is a predicate; a bare
        // parenthesised node is grouping, so roll back and fall through.
        if pattern.tail.is_empty() {
            self.pos = checkpoint;
            return Ok(None);
        }
        Ok(Some(Expression::PatternPredicate {
            pattern: Box::new(pattern),
            span,
        }))
    }

    /// Parse an existential subquery `EXISTS { [MATCH] pattern [WHERE pred] }`
    /// once the `EXISTS` keyword has been consumed.
    ///
    /// The braces are mandatory (the deprecated `exists(n.prop)` function form
    /// is not supported — `n.prop IS NOT NULL` replaces it). Inside, a leading
    /// `MATCH` keyword is optional and equivalent, a single path pattern is
    /// required, and an optional `WHERE` filters the matches before the
    /// existence test. Because the braces already delimit the pattern, a bare
    /// node (`EXISTS { (n) }`) is legal here — there is no grouping ambiguity to
    /// resolve as there is for a bare [pattern predicate](Self::try_parse_pattern_predicate).
    fn parse_exists_subquery(&mut self, span: Span) -> ParseResult<Expression> {
        self.eat(&TokenKind::LBrace, "`{` after EXISTS")?;
        // An optional leading `MATCH` keyword — `EXISTS { MATCH (a)-->(b) }` is
        // equivalent to `EXISTS { (a)-->(b) }`.
        if matches!(self.peek_kind(), TokenKind::Match) {
            self.consume();
        }
        let pattern = self.parse_path_pattern()?;
        let predicate = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.eat(&TokenKind::RBrace, "`}` to close EXISTS subquery")?;
        Ok(Expression::ExistsSubquery {
            pattern: Box::new(pattern),
            predicate,
            span,
        })
    }

    /// Parse a counting subquery `COUNT { [MATCH] pattern [WHERE pred] }` once
    /// the `COUNT` identifier has been consumed and the cursor sits on `{`.
    ///
    /// Structurally identical to [`parse_exists_subquery`](Self::parse_exists_subquery)
    /// — the braces are mandatory and disambiguate the pattern from a grouped
    /// expression (so a bare node `COUNT { (n) }` is legal), a leading `MATCH`
    /// keyword is optional and equivalent, a single path pattern is required,
    /// and an optional inner `WHERE` filters the matches before they are
    /// counted. The only difference from `EXISTS` is the produced AST node and
    /// its runtime value (an integer count rather than a boolean).
    fn parse_count_subquery(&mut self, span: Span) -> ParseResult<Expression> {
        self.eat(&TokenKind::LBrace, "`{` after COUNT")?;
        // An optional leading `MATCH` keyword — `COUNT { MATCH (a)-->(b) }` is
        // equivalent to `COUNT { (a)-->(b) }`.
        if matches!(self.peek_kind(), TokenKind::Match) {
            self.consume();
        }
        let pattern = self.parse_path_pattern()?;
        let predicate = if matches!(self.peek_kind(), TokenKind::Where) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.eat(&TokenKind::RBrace, "`}` to close COUNT subquery")?;
        Ok(Expression::CountSubquery {
            pattern: Box::new(pattern),
            predicate,
            span,
        })
    }

    /// Parse a list predicate function `kind(var IN list WHERE pred)` once the
    /// function-name token has been consumed and the cursor sits on `(`.
    ///
    /// The `WHERE predicate` is mandatory (unlike a list comprehension's
    /// optional filter), matching Neo4j — `all(x IN list)` is a parse error.
    fn parse_list_predicate(
        &mut self,
        kind: ListPredicateKind,
        span: Span,
    ) -> ParseResult<Expression> {
        self.eat(&TokenKind::LParen, "`(` after list predicate function")?;
        let (variable, _) = self.consume_strict_identifier()?;
        self.eat(&TokenKind::In, "`IN` in list predicate")?;
        let list = self.parse_expression()?;
        self.eat(&TokenKind::Where, "`WHERE` in list predicate")?;
        let predicate = self.parse_expression()?;
        self.eat(&TokenKind::RParen, "`)` to close list predicate")?;
        Ok(Expression::ListPredicate {
            kind,
            variable,
            list: Box::new(list),
            predicate: Box::new(predicate),
            span,
        })
    }

    /// Parse a `reduce(acc = init, var IN list | expr)` fold once the `reduce`
    /// name has been consumed and the cursor sits on `(`.
    ///
    /// The accumulator and loop variables must be bare identifiers; the `=`,
    /// `,`, `IN` and `|` separators are all mandatory, matching Neo4j — any
    /// missing piece surfaces as a parse error.
    fn parse_reduce(&mut self, span: Span) -> ParseResult<Expression> {
        self.eat(&TokenKind::LParen, "`(` after `reduce`")?;
        let (accumulator, _) = self.consume_strict_identifier()?;
        self.eat(&TokenKind::Eq, "`=` after the reduce accumulator")?;
        let init = self.parse_expression()?;
        self.eat(&TokenKind::Comma, "`,` after the reduce initial value")?;
        let (variable, _) = self.consume_strict_identifier()?;
        self.eat(&TokenKind::In, "`IN` in reduce")?;
        let list = self.parse_expression()?;
        self.eat(&TokenKind::Pipe, "`|` before the reduce expression")?;
        let expr = self.parse_expression()?;
        self.eat(&TokenKind::RParen, "`)` to close reduce")?;
        Ok(Expression::Reduce {
            accumulator,
            init: Box::new(init),
            variable,
            list: Box::new(list),
            expr: Box::new(expr),
            span,
        })
    }

    fn parse_case(&mut self) -> ParseResult<Expression> {
        let span = self.peek_span();
        self.consume(); // CASE
        let scrutinee = if matches!(self.peek_kind(), TokenKind::When) {
            None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        let mut arms = Vec::new();
        while matches!(self.peek_kind(), TokenKind::When) {
            self.consume();
            let cond = self.parse_expression()?;
            self.eat(&TokenKind::Then, "THEN after WHEN condition")?;
            let val = self.parse_expression()?;
            arms.push((cond, val));
        }
        if arms.is_empty() {
            return Err(ParseError::Malformed {
                message: "CASE requires at least one WHEN arm".to_string(),
                span,
            });
        }
        let else_branch = if matches!(self.peek_kind(), TokenKind::Else) {
            self.consume();
            Some(Box::new(self.parse_expression()?))
        } else {
            None
        };
        self.eat(&TokenKind::End, "END to close CASE")?;
        Ok(Expression::Case {
            scrutinee,
            arms,
            else_branch,
            span,
        })
    }

    fn parse_identifier_or_call(&mut self) -> ParseResult<Expression> {
        let (first, first_span) = self.consume_strict_identifier()?;
        // `COUNT { [MATCH] pattern [WHERE pred] }` — a counting subquery. Unlike
        // `EXISTS`, `count` is not a reserved keyword token (it is the ordinary
        // aggregation identifier `count(*)`), so the brace form is detected here
        // by a bare `count` name immediately followed by `{`. A `count(` call
        // (the aggregation) and a bare `count` variable both keep the `(` /
        // non-`{` paths below, so claiming `count {` is purely additive — an
        // identifier followed by `{` was a parse error before this task.
        if first.eq_ignore_ascii_case("count") && matches!(self.peek_kind(), TokenKind::LBrace) {
            return self.parse_count_subquery(first_span);
        }
        // List predicate functions `any` / `none` / `single` (the `all`
        // variant is a keyword token, handled in `parse_prefix`). They take
        // the `var IN list WHERE pred` form rather than ordinary comma-
        // separated arguments, so they are dispatched before the generic
        // call path. Detection is by bare name immediately followed by `(`;
        // `any`/`none`/`single` are not otherwise valid drevo functions, so
        // there is no ambiguity with a scalar call.
        if matches!(self.peek_kind(), TokenKind::LParen) {
            if let Some(kind) = list_predicate_kind(&first) {
                return self.parse_list_predicate(kind, first_span);
            }
            // `reduce(acc = init, var IN list | expr)` — like the list
            // predicates, this is a bare name immediately followed by `(`
            // that takes a bespoke form rather than comma-separated arguments,
            // so it is dispatched before the generic call path. `reduce` is not
            // otherwise a valid drevo function, so there is no ambiguity.
            if first.eq_ignore_ascii_case("reduce") {
                return self.parse_reduce(first_span);
            }
        }
        // Dotted function name: name `.` name `.` name ... `(`
        // We use a peek to decide: if the next non-dotted-name token is `(`
        // we treat this as a function call; otherwise this is a variable
        // (and the postfix loop will handle `.` for property access).
        // Look ahead through alternating Dot + Identifier.
        let mut probe = 0usize;
        let mut name_segments = vec![first.clone()];
        loop {
            if matches!(self.peek_at(probe), TokenKind::Dot)
                && matches!(self.peek_at(probe + 1), TokenKind::Identifier(_))
            {
                if let TokenKind::Identifier(s) = self.peek_at(probe + 1) {
                    name_segments.push(s.clone());
                }
                probe += 2;
            } else {
                break;
            }
        }
        if matches!(self.peek_at(probe), TokenKind::LParen) && !name_segments.is_empty() {
            // It's a function call only if there's at least one dot (so
            // multi-segment name) OR the bare name is immediately followed
            // by `(` with no dotting at all.
            let bare_call = probe == 0;
            if bare_call || name_segments.len() > 1 {
                // Consume the segments we've already peeked.
                for _ in 0..probe {
                    self.consume();
                }
                self.consume(); // (
                let distinct = self.match_kind(&TokenKind::Distinct);
                let mut args = Vec::new();
                if !matches!(self.peek_kind(), TokenKind::RParen) {
                    args.push(self.parse_expression()?);
                    while matches!(self.peek_kind(), TokenKind::Comma) {
                        self.consume();
                        args.push(self.parse_expression()?);
                    }
                }
                self.eat(&TokenKind::RParen, "`)` to close function call")?;
                return Ok(Expression::FunctionCall {
                    name: name_segments,
                    distinct,
                    args,
                    span: first_span,
                });
            }
        }
        Ok(Expression::Variable(first, first_span))
    }

    fn peek_infix_op(&self) -> Option<(InfixOp, u8, u8)> {
        // Returns (op, lbp, rbp). lbp == rbp for left-assoc, rbp == lbp - 1
        // for right-assoc (Pratt convention with `>` for continue).
        match self.peek_kind() {
            TokenKind::Or => Some((InfixOp::Binary(BinaryOp::Or), 10, 11)),
            TokenKind::Xor => Some((InfixOp::Binary(BinaryOp::Xor), 12, 13)),
            TokenKind::And => Some((InfixOp::Binary(BinaryOp::And), 14, 15)),
            TokenKind::Eq => Some((InfixOp::Binary(BinaryOp::Eq), 20, 21)),
            TokenKind::Ne => Some((InfixOp::Binary(BinaryOp::Ne), 20, 21)),
            TokenKind::Lt => Some((InfixOp::Binary(BinaryOp::Lt), 20, 21)),
            TokenKind::Le => Some((InfixOp::Binary(BinaryOp::Le), 20, 21)),
            TokenKind::Gt => Some((InfixOp::Binary(BinaryOp::Gt), 20, 21)),
            TokenKind::Ge => Some((InfixOp::Binary(BinaryOp::Ge), 20, 21)),
            TokenKind::RegexMatch => Some((InfixOp::Binary(BinaryOp::RegexMatch), 20, 21)),
            TokenKind::Is => Some((InfixOp::IsNullPrefix, 20, 21)),
            TokenKind::In => Some((InfixOp::InPrefix, 20, 21)),
            TokenKind::Starts => Some((InfixOp::Binary(BinaryOp::StartsWith), 20, 21)),
            TokenKind::Ends => Some((InfixOp::Binary(BinaryOp::EndsWith), 20, 21)),
            TokenKind::Contains => Some((InfixOp::Binary(BinaryOp::Contains), 20, 21)),
            TokenKind::Plus => Some((InfixOp::Binary(BinaryOp::Add), 30, 31)),
            TokenKind::Minus => Some((InfixOp::Binary(BinaryOp::Sub), 30, 31)),
            TokenKind::Star => Some((InfixOp::Binary(BinaryOp::Mul), 40, 41)),
            TokenKind::Slash => Some((InfixOp::Binary(BinaryOp::Div), 40, 41)),
            TokenKind::Percent => Some((InfixOp::Binary(BinaryOp::Mod), 40, 41)),
            // ^ is right-associative
            TokenKind::Caret => Some((InfixOp::Binary(BinaryOp::Pow), 51, 50)),
            _ => None,
        }
    }
}

/// Binding power for unary `+` / `-`. Per openCypher (and Neo4j 5.x)
/// `^` binds tighter than unary minus, so `-2^3` parses as `-(2^3)`.
/// Therefore `PREFIX_BP` sits between `*` (40) and `^` (51).
const PREFIX_BP: u8 = 45;
/// `NOT` binds tighter than the boolean infixes but looser than
/// comparison — so `NOT a = b` parses as `NOT (a = b)`.
const NOT_BP: u8 = 18;

#[derive(Debug, Clone, Copy)]
enum InfixOp {
    Binary(BinaryOp),
    IsNullPrefix,
    InPrefix,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Query {
        parse(src).unwrap()
    }

    #[test]
    fn parses_minimal_return() {
        let q = p("RETURN 1");
        assert_eq!(q.parts.len(), 1);
        assert_eq!(q.parts[0].query.clauses.len(), 1);
    }

    #[test]
    fn semicolon_terminator_is_accepted() {
        let q = p("RETURN 1;");
        assert_eq!(q.parts[0].query.clauses.len(), 1);
    }

    #[test]
    fn parses_empty_returns_empty_error() {
        let err = parse("").unwrap_err();
        assert!(matches!(err, ParseError::Empty));
    }

    #[test]
    fn lex_error_propagates() {
        let err = parse("RETURN 'oops").unwrap_err();
        assert!(matches!(err, ParseError::Lex(_)));
    }

    #[test]
    fn extra_tokens_after_query_fail() {
        let err = parse("RETURN 1 EXTRA").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[test]
    fn parses_property_in_where() {
        let q = p("MATCH (n) WHERE n.x = 1 RETURN n");
        match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => assert!(m.where_clause.is_some()),
            _ => panic!(),
        }
    }

    #[test]
    fn parses_node_pattern_anonymous_unlabeled() {
        let q = p("MATCH () RETURN 1");
        match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => {
                assert!(m.patterns[0].path.head.variable.is_none());
                assert!(m.patterns[0].path.head.labels.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn relationship_default_no_brackets() {
        let q = p("MATCH (a)-->(b) RETURN a, b");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        let rel = &m.patterns[0].path.tail[0].relationship;
        assert_eq!(rel.direction, Direction::Outgoing);
        assert!(rel.variable.is_none());
        assert!(rel.types.is_empty());
    }

    #[test]
    fn relationship_default_no_brackets_incoming() {
        let q = p("MATCH (a)<--(b) RETURN a");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            m.patterns[0].path.tail[0].relationship.direction,
            Direction::Incoming
        );
    }

    #[test]
    fn keyword_as_property_name() {
        // `IN` is a hard keyword, so it must work as a property name.
        let q = p("RETURN n.in");
        let r = match &q.parts[0].query.clauses[0] {
            Clause::Return(r) => r,
            _ => panic!(),
        };
        match &r.items[0] {
            ProjectionItem::Expression {
                expr: Expression::Property { name, .. },
                ..
            } => assert_eq!(name, "in"),
            _ => panic!(),
        }
    }

    #[test]
    fn unary_neg_precedence_against_pow() {
        // openCypher: `^` binds tighter than unary minus, so `-2 ^ 3`
        // is `-(2 ^ 3) = -8`, not `(-2) ^ 3 = -8`. (Same numeric value
        // here by coincidence, but the AST shape matters for the
        // executor and for non-cubic exponents.)
        let q = p("RETURN -2 ^ 3");
        let expr = match &q.parts[0].query.clauses[0] {
            Clause::Return(r) => match &r.items[0] {
                ProjectionItem::Expression { expr, .. } => expr,
                _ => panic!(),
            },
            _ => panic!(),
        };
        match expr {
            Expression::Unary {
                op: UnaryOp::Neg,
                expr,
                ..
            } => {
                assert!(matches!(
                    expr.as_ref(),
                    Expression::Binary {
                        op: BinaryOp::Pow,
                        ..
                    }
                ));
            }
            _ => panic!("expected unary neg, got {expr:?}"),
        }
    }

    #[test]
    fn rel_length_unbounded_lower() {
        let q = p("MATCH (a)-[*..3]->(b) RETURN b");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            m.patterns[0].path.tail[0].relationship.length,
            Some(RelLength::Range {
                from: None,
                to: Some(3)
            })
        );
    }

    #[test]
    fn rel_length_unbounded_upper() {
        let q = p("MATCH (a)-[*2..]->(b) RETURN b");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            m.patterns[0].path.tail[0].relationship.length,
            Some(RelLength::Range {
                from: Some(2),
                to: None
            })
        );
    }

    #[test]
    fn empty_node_with_only_labels() {
        let q = p("MATCH (:Person) RETURN 1");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        assert!(m.patterns[0].path.head.variable.is_none());
        assert_eq!(m.patterns[0].path.head.labels, vec!["Person".to_string()]);
    }

    #[test]
    fn empty_node_with_only_properties() {
        let q = p("MATCH ({id: 1}) RETURN 1");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        let props = m.patterns[0].path.head.properties.as_ref().unwrap();
        assert_eq!(props.entries.len(), 1);
    }

    // ---- FOREACH (00144) ------------------------------------------------

    #[test]
    fn parses_foreach_with_single_update_clause() {
        let q = p("FOREACH (x IN [1, 2] | CREATE (:Task {title: 'a'}))");
        let f = match &q.parts[0].query.clauses[0] {
            Clause::Foreach(f) => f,
            other => panic!("expected FOREACH, got {other:?}"),
        };
        assert_eq!(f.variable, "x");
        assert!(matches!(f.list, Expression::List { .. }));
        assert_eq!(f.clauses.len(), 1);
        assert!(matches!(f.clauses[0], Clause::Create(_)));
    }

    #[test]
    fn parses_foreach_with_multiple_update_clauses() {
        let q = p("FOREACH (x IN [1] | CREATE (n:Task {title: 'a'}) SET n.done = true)");
        let f = match &q.parts[0].query.clauses[0] {
            Clause::Foreach(f) => f,
            other => panic!("expected FOREACH, got {other:?}"),
        };
        assert_eq!(f.clauses.len(), 2);
        assert!(matches!(f.clauses[0], Clause::Create(_)));
        assert!(matches!(f.clauses[1], Clause::Set(_)));
    }

    #[test]
    fn parses_nested_foreach() {
        let q = p("FOREACH (r IN [[1]] | FOREACH (c IN r | CREATE (:Cell {title: 'a'})))");
        let f = match &q.parts[0].query.clauses[0] {
            Clause::Foreach(f) => f,
            other => panic!("expected FOREACH, got {other:?}"),
        };
        assert!(matches!(f.clauses[0], Clause::Foreach(_)));
    }

    #[test]
    fn foreach_rejects_read_clause_in_body() {
        let err = parse("FOREACH (x IN [1] | MATCH (n) SET n.done = true)").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }), "got {err:?}");
    }

    #[test]
    fn foreach_rejects_empty_body() {
        let err = parse("FOREACH (x IN [1] | )").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }), "got {err:?}");
    }

    #[test]
    fn foreach_requires_closing_paren() {
        let err = parse("FOREACH (x IN [1] | CREATE (:Task {title: 'a'})").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }), "got {err:?}");
    }

    #[test]
    fn foreach_requires_pipe_separator() {
        let err = parse("FOREACH (x IN [1] CREATE (:Task {title: 'a'}))").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }), "got {err:?}");
    }

    // ---- CALL / YIELD (00145) ---------------------------------------------

    fn first_call(src: &str) -> CallClause {
        match p(src).parts[0].query.clauses[0].clone() {
            Clause::Call(c) => c,
            other => panic!("expected CALL, got {other:?}"),
        }
    }

    #[test]
    fn parses_standalone_call_dotted_name() {
        let c = first_call("CALL db.labels()");
        assert_eq!(c.name, vec!["db", "labels"]);
        assert!(c.args.is_empty());
        assert!(c.yields.is_none());
        assert!(c.where_clause.is_none());
    }

    #[test]
    fn parses_call_with_yield_items() {
        let c = first_call("CALL db.labels() YIELD label RETURN label");
        let yields = c.yields.expect("yields");
        assert_eq!(yields.len(), 1);
        assert_eq!(yields[0].name, "label");
        assert!(yields[0].alias.is_none());
    }

    #[test]
    fn parses_call_yield_with_alias() {
        let c = first_call("CALL db.labels() YIELD label AS l RETURN l");
        let yields = c.yields.expect("yields");
        assert_eq!(yields[0].alias.as_deref(), Some("l"));
    }

    #[test]
    fn parses_call_yield_where() {
        let c = first_call("CALL db.labels() YIELD label WHERE label = 'X' RETURN label");
        assert!(c.where_clause.is_some());
    }

    #[test]
    fn parses_call_with_arguments() {
        // Arguments parse generically even though the built-ins take none;
        // arity is an executor-level concern.
        let c = first_call("CALL some.proc(1, 'two')");
        assert_eq!(c.args.len(), 2);
    }

    #[test]
    fn call_requires_parentheses() {
        let err = parse("CALL db.labels").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }), "got {err:?}");
    }

    // ---- keyword-as-identifier casing -------------------------------------
    // A reserved keyword used in an *identifier* position (label, rel-type,
    // property/map key) must round-trip with its written casing — keyword
    // recognition is case-insensitive, but the name is whatever the source
    // wrote. Regression: `consume_name` used to `.to_lowercase()` the
    // canonical keyword form, so `[:CONTAINS]` became `contains`.

    #[test]
    fn keyword_rel_type_preserves_written_casing() {
        let q = p("CREATE (a)-[:CONTAINS]->(b)");
        let c = match &q.parts[0].query.clauses[0] {
            Clause::Create(c) => c,
            _ => panic!(),
        };
        let rel = &c.patterns[0].path.tail[0].relationship;
        assert_eq!(rel.types, vec!["CONTAINS".to_string()]);
    }

    #[test]
    fn keyword_label_preserves_written_casing() {
        // `Contains` (mixed case of the CONTAINS keyword) as a label.
        let q = p("MATCH (n:Contains) RETURN n");
        let m = match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            _ => panic!(),
        };
        assert_eq!(m.patterns[0].path.head.labels, vec!["Contains".to_string()]);
    }

    #[test]
    fn keyword_property_name_preserves_mixed_casing() {
        // `In` (mixed case of the IN keyword) as a property name keeps
        // exactly what was written, not a lowercased form.
        let q = p("RETURN n.In");
        let r = match &q.parts[0].query.clauses[0] {
            Clause::Return(r) => r,
            _ => panic!(),
        };
        match &r.items[0] {
            ProjectionItem::Expression {
                expr: Expression::Property { name, .. },
                ..
            } => assert_eq!(name, "In"),
            _ => panic!(),
        }
    }

    /// Extract the first `RETURN` projection expression from a query.
    fn first_return_expr(q: &Query) -> &Expression {
        match &q.parts[0].query.clauses[0] {
            Clause::Return(r) => match &r.items[0] {
                ProjectionItem::Expression { expr, .. } => expr,
                _ => panic!("expected expression projection"),
            },
            _ => panic!("expected RETURN clause"),
        }
    }

    #[test]
    fn list_comprehension_filter_and_projection() {
        let q = p("RETURN [x IN [1, 2, 3] WHERE x > 1 | x * 10]");
        match first_return_expr(&q) {
            Expression::ListComprehension {
                variable,
                list,
                predicate,
                projection,
                ..
            } => {
                assert_eq!(variable, "x");
                assert!(matches!(list.as_ref(), Expression::List { .. }));
                assert!(predicate.is_some());
                assert!(projection.is_some());
            }
            other => panic!("expected list comprehension, got {other:?}"),
        }
    }

    #[test]
    fn list_comprehension_filter_only_has_no_projection() {
        let q = p("RETURN [x IN [1, 2, 3] WHERE x > 1]");
        match first_return_expr(&q) {
            Expression::ListComprehension {
                predicate,
                projection,
                ..
            } => {
                assert!(predicate.is_some());
                assert!(projection.is_none());
            }
            other => panic!("expected list comprehension, got {other:?}"),
        }
    }

    #[test]
    fn list_comprehension_projection_only_has_no_predicate() {
        let q = p("RETURN [x IN [1, 2, 3] | x + 1]");
        match first_return_expr(&q) {
            Expression::ListComprehension {
                predicate,
                projection,
                ..
            } => {
                assert!(predicate.is_none());
                assert!(projection.is_some());
            }
            other => panic!("expected list comprehension, got {other:?}"),
        }
    }

    #[test]
    fn bracket_in_expression_without_where_or_pipe_is_a_list_literal() {
        // `[x IN list]` with neither `WHERE` nor `|` is an ordinary list
        // literal whose single element is the membership test `x IN list`,
        // not a comprehension.
        let q = p("RETURN [1 IN [1, 2]]");
        match first_return_expr(&q) {
            Expression::List { items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0], Expression::In { .. }));
            }
            other => panic!("expected list literal, got {other:?}"),
        }
    }

    #[test]
    fn list_comprehension_non_identifier_variable_is_malformed() {
        // The element on the left of `IN` must be a bare identifier.
        let err = parse("RETURN [n.x IN [1, 2] | n.x]").unwrap_err();
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn list_predicate_all_parses_with_kind_and_parts() {
        // `all` is a keyword token, dispatched in `parse_prefix`.
        let q = p("RETURN all(x IN [1, 2, 3] WHERE x > 0)");
        match first_return_expr(&q) {
            Expression::ListPredicate {
                kind,
                variable,
                list,
                ..
            } => {
                assert_eq!(*kind, ListPredicateKind::All);
                assert_eq!(variable, "x");
                assert!(matches!(list.as_ref(), Expression::List { .. }));
            }
            other => panic!("expected list predicate, got {other:?}"),
        }
    }

    #[test]
    fn list_predicate_any_none_single_parse_kinds() {
        for (src, want) in [
            ("RETURN any(x IN xs WHERE x > 0)", ListPredicateKind::Any),
            ("RETURN none(x IN xs WHERE x > 0)", ListPredicateKind::None),
            (
                "RETURN single(x IN xs WHERE x > 0)",
                ListPredicateKind::Single,
            ),
        ] {
            let q = p(src);
            match first_return_expr(&q) {
                Expression::ListPredicate { kind, .. } => assert_eq!(*kind, want),
                other => panic!("expected list predicate for {src}, got {other:?}"),
            }
        }
    }

    #[test]
    fn list_predicate_name_is_case_insensitive() {
        for src in [
            "RETURN ALL(x IN xs WHERE x)",
            "RETURN Any(x IN xs WHERE x)",
            "RETURN NONE(x IN xs WHERE x)",
            "RETURN Single(x IN xs WHERE x)",
        ] {
            let q = p(src);
            assert!(
                matches!(first_return_expr(&q), Expression::ListPredicate { .. }),
                "expected list predicate for {src}"
            );
        }
    }

    #[test]
    fn list_predicate_where_is_mandatory() {
        // Unlike a list comprehension's optional filter, the predicate
        // functions require `WHERE`.
        assert!(parse("RETURN any(x IN [1, 2])").is_err());
        assert!(parse("RETURN all(x IN [1, 2])").is_err());
    }

    #[test]
    fn reduce_parses_all_parts() {
        let q = p("RETURN reduce(s = 0, x IN [1, 2, 3] | s + x)");
        match first_return_expr(&q) {
            Expression::Reduce {
                accumulator,
                init,
                variable,
                list,
                expr,
                ..
            } => {
                assert_eq!(accumulator, "s");
                assert!(matches!(init.as_ref(), Expression::Integer(0, _)));
                assert_eq!(variable, "x");
                assert!(matches!(list.as_ref(), Expression::List { .. }));
                assert!(matches!(expr.as_ref(), Expression::Binary { .. }));
            }
            other => panic!("expected reduce, got {other:?}"),
        }
    }

    #[test]
    fn reduce_name_is_case_insensitive() {
        for src in [
            "RETURN REDUCE(s = 0, x IN xs | s + x)",
            "RETURN Reduce(s = 0, x IN xs | s + x)",
        ] {
            let q = p(src);
            assert!(
                matches!(first_return_expr(&q), Expression::Reduce { .. }),
                "expected reduce for {src}"
            );
        }
    }

    #[test]
    fn reduce_requires_each_separator() {
        // `=`, `,`, `IN` and `|` are all mandatory.
        assert!(parse("RETURN reduce(s 0, x IN xs | s + x)").is_err());
        assert!(parse("RETURN reduce(s = 0 x IN xs | s + x)").is_err());
        assert!(parse("RETURN reduce(s = 0, x xs | s + x)").is_err());
        assert!(parse("RETURN reduce(s = 0, x IN xs s + x)").is_err());
        assert!(parse("RETURN reduce(s = 0, x IN xs | s + x").is_err());
    }

    #[test]
    fn bare_reduce_without_paren_is_a_variable() {
        // `reduce` only triggers the fold form when immediately followed by
        // `(`; otherwise it is an ordinary identifier.
        let q = p("RETURN reduce");
        assert!(matches!(
            first_return_expr(&q),
            Expression::Variable(name, _) if name == "reduce"
        ));
    }

    #[test]
    fn bare_any_without_paren_is_a_variable() {
        // `any` only triggers the predicate form when immediately followed by
        // `(`; otherwise it is an ordinary identifier (variable / property).
        let q = p("RETURN any");
        assert!(matches!(
            first_return_expr(&q),
            Expression::Variable(name, _) if name == "any"
        ));
    }

    #[test]
    fn empty_list_literal_still_parses() {
        let q = p("RETURN []");
        match first_return_expr(&q) {
            Expression::List { items, .. } => assert!(items.is_empty()),
            other => panic!("expected empty list literal, got {other:?}"),
        }
    }

    // ===== Map projection (`00149`) =====================================

    #[test]
    fn map_projection_parses_mixed_selectors() {
        let q = p("RETURN n {.name, .age, role: 'admin', extra} AS m");
        match first_return_expr(&q) {
            Expression::MapProjection {
                base, selectors, ..
            } => {
                assert!(matches!(base.as_ref(), Expression::Variable(v, _) if v == "n"));
                assert_eq!(selectors.len(), 4);
                assert!(matches!(&selectors[0], MapProjectionSelector::Property(k) if k == "name"));
                assert!(matches!(&selectors[1], MapProjectionSelector::Property(k) if k == "age"));
                match &selectors[2] {
                    MapProjectionSelector::Literal(k, expr) => {
                        assert_eq!(k, "role");
                        assert!(matches!(expr, Expression::String(s, _) if s == "admin"));
                    }
                    other => panic!("expected literal selector, got {other:?}"),
                }
                assert!(
                    matches!(&selectors[3], MapProjectionSelector::Variable(v) if v == "extra")
                );
            }
            other => panic!("expected map projection, got {other:?}"),
        }
    }

    #[test]
    fn map_projection_all_properties_selector() {
        let q = p("RETURN n {.*} AS m");
        match first_return_expr(&q) {
            Expression::MapProjection { selectors, .. } => {
                assert_eq!(selectors.len(), 1);
                assert!(matches!(selectors[0], MapProjectionSelector::AllProperties));
            }
            other => panic!("expected map projection, got {other:?}"),
        }
    }

    #[test]
    fn empty_map_projection_parses() {
        // `n {}` is a projection with no selectors (projects to an empty map);
        // a leading `{` is consumed by the prefix path, so the bare `{}` form
        // requires a preceding base — exactly this case.
        let q = p("RETURN n {} AS m");
        match first_return_expr(&q) {
            Expression::MapProjection { selectors, .. } => assert!(selectors.is_empty()),
            other => panic!("expected map projection, got {other:?}"),
        }
    }

    #[test]
    fn map_projection_chains_off_a_property_base() {
        // The base may itself be a postfix expression (`a.b { … }`).
        let q = p("RETURN a.inner {.x} AS m");
        match first_return_expr(&q) {
            Expression::MapProjection {
                base, selectors, ..
            } => {
                assert!(matches!(base.as_ref(), Expression::Property { .. }));
                assert_eq!(selectors.len(), 1);
            }
            other => panic!("expected map projection, got {other:?}"),
        }
    }

    #[test]
    fn standalone_brace_is_still_a_map_literal() {
        // A `{` in *prefix* position is a map literal, untouched by the new
        // postfix branch.
        let q = p("RETURN {a: 1, b: 2}");
        assert!(matches!(first_return_expr(&q), Expression::Map(_)));
    }

    #[test]
    fn map_projection_unclosed_brace_is_an_error() {
        assert!(parse("RETURN n {.name").is_err());
        assert!(parse("RETURN n {.name,}").is_err());
    }

    #[test]
    fn pattern_comprehension_parses_predicate_and_projection() {
        let q = p("RETURN [(p)-[:KNOWS]->(f) WHERE f.age > 30 | f.name] AS ns");
        match first_return_expr(&q) {
            Expression::PatternComprehension {
                pattern,
                predicate,
                projection,
                ..
            } => {
                assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                assert_eq!(pattern.head.variable.as_deref(), Some("p"));
                assert!(predicate.is_some());
                assert!(matches!(projection.as_ref(), Expression::Property { .. }));
            }
            other => panic!("expected pattern comprehension, got {other:?}"),
        }
    }

    #[test]
    fn pattern_comprehension_projection_only_has_no_predicate() {
        let q = p("RETURN [(p)-[:KNOWS]->(f) | f.name] AS ns");
        match first_return_expr(&q) {
            Expression::PatternComprehension { predicate, .. } => assert!(predicate.is_none()),
            other => panic!("expected pattern comprehension, got {other:?}"),
        }
    }

    #[test]
    fn pattern_comprehension_supports_a_multi_hop_path() {
        let q = p("RETURN [(p)-[:KNOWS]->()-[:KNOWS]->(ff) | ff.name] AS ns");
        match first_return_expr(&q) {
            Expression::PatternComprehension { pattern, .. } => {
                assert_eq!(pattern.tail.len(), 2, "two relationship segments");
            }
            other => panic!("expected pattern comprehension, got {other:?}"),
        }
    }

    #[test]
    fn bracket_with_parenthesised_element_is_a_list_literal_not_a_comprehension() {
        // A list literal whose single element is a parenthesised expression
        // also opens with `(`; without a relationship + `|` it stays a literal.
        let q = p("RETURN [(1 + 2)]");
        match first_return_expr(&q) {
            Expression::List { items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0], Expression::Binary { .. }));
            }
            other => panic!("expected list literal, got {other:?}"),
        }
    }

    #[test]
    fn bracket_with_lone_node_pattern_falls_through_to_variable_list() {
        // `[(a)]` is a list literal `[a]` — a bare node has no relationship, so
        // it is not a pattern comprehension; the speculative parse rolls back.
        let q = p("RETURN [(a)]");
        match first_return_expr(&q) {
            Expression::List { items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0], Expression::Variable(..)));
            }
            other => panic!("expected list literal, got {other:?}"),
        }
    }

    #[test]
    fn pattern_predicate_in_where_parses_as_a_path() {
        let q = p("MATCH (p) WHERE (p)-[:KNOWS]->(f) RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::PatternPredicate { pattern, .. } => {
                assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                assert_eq!(pattern.head.variable.as_deref(), Some("p"));
            }
            other => panic!("expected pattern predicate, got {other:?}"),
        }
    }

    #[test]
    fn negated_pattern_predicate_parses_under_not() {
        let q = p("MATCH (p) WHERE NOT (p)-[:KNOWS]->() RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => assert!(matches!(expr.as_ref(), Expression::PatternPredicate { .. })),
            other => panic!("expected NOT over a pattern predicate, got {other:?}"),
        }
    }

    #[test]
    fn multi_hop_pattern_predicate_parses() {
        let q = p("MATCH (p) WHERE (p)-[:KNOWS]->()-[:KNOWS]->() RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::PatternPredicate { pattern, .. } => {
                assert_eq!(pattern.tail.len(), 2, "two relationship segments");
            }
            other => panic!("expected pattern predicate, got {other:?}"),
        }
    }

    #[test]
    fn exists_subquery_in_where_parses_with_pattern_and_no_predicate() {
        let q = p("MATCH (p) WHERE EXISTS { (p)-[:KNOWS]->(f) } RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::ExistsSubquery {
                pattern, predicate, ..
            } => {
                assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                assert_eq!(pattern.head.variable.as_deref(), Some("p"));
                assert!(predicate.is_none(), "no inner WHERE");
            }
            other => panic!("expected existential subquery, got {other:?}"),
        }
    }

    #[test]
    fn exists_subquery_with_match_keyword_and_inner_where_parses() {
        let q = p("MATCH (p) WHERE EXISTS { MATCH (p)-[:KNOWS]->(f) WHERE f.age > 18 } RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::ExistsSubquery {
                pattern, predicate, ..
            } => {
                assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                assert!(predicate.is_some(), "inner WHERE present");
            }
            other => panic!("expected existential subquery, got {other:?}"),
        }
    }

    #[test]
    fn exists_subquery_accepts_a_bare_node_pattern() {
        // Unlike a bare pattern predicate, the braces disambiguate, so a single
        // node `EXISTS { (n) }` is a legal subquery (not grouping).
        let q = p("MATCH (n) WHERE EXISTS { (n) } RETURN n");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::ExistsSubquery { pattern, .. } => {
                assert!(pattern.tail.is_empty(), "bare node — no relationship");
                assert_eq!(pattern.head.variable.as_deref(), Some("n"));
            }
            other => panic!("expected existential subquery, got {other:?}"),
        }
    }

    #[test]
    fn negated_exists_subquery_parses_under_not() {
        let q = p("MATCH (p) WHERE NOT EXISTS { (p)-[:KNOWS]->() } RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => assert!(matches!(expr.as_ref(), Expression::ExistsSubquery { .. })),
            other => panic!("expected NOT over an existential subquery, got {other:?}"),
        }
    }

    #[test]
    fn exists_without_brace_is_a_parse_error() {
        // Only the brace form is supported; the deprecated `exists(...)`
        // function form is not.
        assert!(parse("MATCH (n) WHERE EXISTS (n)-[:R]->() RETURN n").is_err());
    }

    #[test]
    fn count_subquery_in_where_parses_with_pattern_and_no_predicate() {
        let q = p("MATCH (p) WHERE COUNT { (p)-[:KNOWS]->(f) } > 1 RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        // The predicate is `COUNT { … } > 1`; the left side is the subquery.
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::Binary { lhs, .. } => match lhs.as_ref() {
                Expression::CountSubquery {
                    pattern, predicate, ..
                } => {
                    assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                    assert_eq!(pattern.head.variable.as_deref(), Some("p"));
                    assert!(predicate.is_none(), "no inner WHERE");
                }
                other => panic!("expected counting subquery, got {other:?}"),
            },
            other => panic!("expected a comparison, got {other:?}"),
        }
    }

    #[test]
    fn count_subquery_with_match_keyword_and_inner_where_parses() {
        let q =
            p("MATCH (p) WHERE COUNT { MATCH (p)-[:KNOWS]->(f) WHERE f.age > 18 } > 0 RETURN p");
        let Clause::Match(m) = &q.parts[0].query.clauses[0] else {
            panic!("expected MATCH");
        };
        match m.where_clause.as_ref().expect("a WHERE predicate") {
            Expression::Binary { lhs, .. } => match lhs.as_ref() {
                Expression::CountSubquery {
                    pattern, predicate, ..
                } => {
                    assert_eq!(pattern.tail.len(), 1, "one relationship segment");
                    assert!(predicate.is_some(), "inner WHERE present");
                }
                other => panic!("expected counting subquery, got {other:?}"),
            },
            other => panic!("expected a comparison, got {other:?}"),
        }
    }

    #[test]
    fn count_subquery_accepts_a_bare_node_pattern_in_return() {
        // The braces disambiguate, so a single node `COUNT { (n) }` is a legal
        // subquery (not grouping). Used here directly as a RETURN column.
        let q = p("MATCH (n) RETURN COUNT { (n) }");
        let Clause::Return(r) = &q.parts[0].query.clauses[1] else {
            panic!("expected RETURN");
        };
        let ProjectionItem::Expression { expr, .. } = &r.items[0] else {
            panic!("expected an expression projection");
        };
        match expr {
            Expression::CountSubquery { pattern, .. } => {
                assert!(pattern.tail.is_empty(), "bare node — no relationship");
                assert_eq!(pattern.head.variable.as_deref(), Some("n"));
            }
            other => panic!("expected counting subquery, got {other:?}"),
        }
    }

    #[test]
    fn count_star_aggregation_still_parses_as_a_function_call() {
        // The brace form must not steal the `count(*)` / `count(x)` aggregation:
        // a `count` immediately followed by `(` stays an ordinary function call.
        let q = p("MATCH (n) RETURN count(*)");
        let Clause::Return(r) = &q.parts[0].query.clauses[1] else {
            panic!("expected RETURN");
        };
        let ProjectionItem::Expression { expr, .. } = &r.items[0] else {
            panic!("expected an expression projection");
        };
        match expr {
            Expression::FunctionCall { name, args, .. } => {
                assert_eq!(name, &["count".to_string()]);
                assert!(matches!(args.as_slice(), [Expression::Star(_)]));
            }
            other => panic!("expected count(*) function call, got {other:?}"),
        }
    }

    #[test]
    fn count_subquery_without_brace_is_a_bare_variable() {
        // `count` not followed by `{` or `(` is an ordinary identifier — neither
        // the aggregation nor the subquery is claimed.
        let q = p("MATCH (count) RETURN count");
        let Clause::Return(r) = &q.parts[0].query.clauses[1] else {
            panic!("expected RETURN");
        };
        let ProjectionItem::Expression { expr, .. } = &r.items[0] else {
            panic!("expected an expression projection");
        };
        assert!(matches!(
            expr,
            Expression::Variable(name, _) if name == "count"
        ));
    }

    #[test]
    fn parenthesised_node_without_relationship_is_still_grouping() {
        // `(a)` has no relationship, so the speculative pattern parse rolls back
        // and `(a).x` is an ordinary grouped variable with a property access.
        let q = p("RETURN (a).x");
        assert!(matches!(first_return_expr(&q), Expression::Property { .. }));
    }

    #[test]
    fn parenthesised_arithmetic_is_still_grouping() {
        let q = p("RETURN (1 + 2) * 3");
        // The grouped `(1 + 2)` is the LHS of a `*`, so the top node is a Binary
        // multiply — not a pattern predicate.
        match first_return_expr(&q) {
            Expression::Binary { op, .. } => assert_eq!(*op, BinaryOp::Mul),
            other => panic!("expected grouped arithmetic, got {other:?}"),
        }
    }

    #[test]
    fn pattern_comprehension_without_projection_pipe_is_an_error() {
        // The `| projection` is mandatory — once a path + `WHERE` commits to a
        // comprehension, a missing `|` is a real error (not a roll-back signal).
        assert!(parse("RETURN [(p)-[:KNOWS]->(f) WHERE f.age > 1]").is_err());
    }

    #[test]
    fn bracketed_path_without_pipe_is_a_list_of_a_pattern_predicate() {
        // `[(p)-[:KNOWS]->(f)]` has no `WHERE`/`|`, so the comprehension parse
        // rolls back; the element then parses as a *pattern predicate* (a path
        // is a boolean existence test in expression position), giving a
        // single-element list — additive on the prior parse-error behaviour.
        let q = p("RETURN [(p)-[:KNOWS]->(f)]");
        match first_return_expr(&q) {
            Expression::List { items, .. } => {
                assert_eq!(items.len(), 1);
                assert!(matches!(items[0], Expression::PatternPredicate { .. }));
            }
            other => panic!("expected list of a pattern predicate, got {other:?}"),
        }
    }

    // ---- shortestPath / allShortestPaths (00155) ------------------------

    fn first_match(q: &Query) -> &MatchClause {
        match &q.parts[0].query.clauses[0] {
            Clause::Match(m) => m,
            other => panic!("expected MATCH, got {other:?}"),
        }
    }

    #[test]
    fn parses_shortest_path_named_binding() {
        let q = p("MATCH (a), (b), p = shortestPath((a)-[*]-(b)) RETURN p");
        let m = first_match(&q);
        // Three comma-separated patterns; only the third is a shortest search.
        assert_eq!(m.patterns.len(), 3);
        assert_eq!(m.patterns[0].shortest, None);
        assert_eq!(m.patterns[1].shortest, None);
        let sp = &m.patterns[2];
        assert_eq!(sp.shortest, Some(ShortestKind::Single));
        assert_eq!(sp.variable.as_deref(), Some("p"));
        // The wrapped path is a single variable-length leg `(a)-[*]-(b)`.
        assert_eq!(sp.path.head.variable.as_deref(), Some("a"));
        assert_eq!(sp.path.tail.len(), 1);
        assert!(sp.path.tail[0].relationship.length.is_some());
        assert_eq!(sp.path.tail[0].node.variable.as_deref(), Some("b"));
    }

    #[test]
    fn parses_all_shortest_paths() {
        let q = p("MATCH (a), (b), p = allShortestPaths((a)-[:KNOWS*..5]-(b)) RETURN p");
        let sp = &first_match(&q).patterns[2];
        assert_eq!(sp.shortest, Some(ShortestKind::All));
        assert_eq!(
            sp.path.tail[0].relationship.types,
            vec!["KNOWS".to_string()]
        );
    }

    #[test]
    fn shortest_path_function_name_is_case_insensitive() {
        let q = p("MATCH p = SHORTESTPATH((a)-[*]-(b)) RETURN p");
        assert_eq!(
            first_match(&q).patterns[0].shortest,
            Some(ShortestKind::Single)
        );
    }

    #[test]
    fn shortest_path_without_binding_variable() {
        // The path variable is optional; `MATCH shortestPath(...)` parses too.
        let q = p("MATCH shortestPath((a)-[*]-(b)) RETURN a");
        let sp = &first_match(&q).patterns[0];
        assert_eq!(sp.shortest, Some(ShortestKind::Single));
        assert!(sp.variable.is_none());
    }

    #[test]
    fn bare_identifier_named_shortestpath_is_not_a_wrapper() {
        // A node variable that happens to be spelled `shortestpath` is an
        // ordinary variable — the wrapper is only claimed before a `(`.
        let q = p("MATCH (shortestpath) RETURN shortestpath");
        let sp = &first_match(&q).patterns[0];
        assert_eq!(sp.shortest, None);
        assert_eq!(sp.path.head.variable.as_deref(), Some("shortestpath"));
    }

    #[test]
    fn shortest_path_missing_close_paren_is_an_error() {
        assert!(parse("MATCH p = shortestPath((a)-[*]-(b) RETURN p").is_err());
    }
}
