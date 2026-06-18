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
            | Self::UnexpectedEof { span } => Some(*span),
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
    let mut parser = Parser::new(tokens);
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
    }
}

/// Internal parser state.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
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
                let s = format!("{kind}");
                // The Display impl for keywords uses upper-case canonical
                // form. For property names we want what the user wrote;
                // but since the lexer is case-insensitive on keywords, the
                // canonical form is fine — Neo4j itself reports the same.
                // (Soft-keyword normalization is the parser's choice.)
                self.consume();
                Ok((s.to_lowercase(), tok.span))
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
            _ => Err(ParseError::Expected {
                expected: "clause keyword (MATCH, CREATE, MERGE, DELETE, SET, REMOVE, WITH, RETURN, UNWIND, FOREACH, OPTIONAL, DETACH)".to_string(),
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
        let path = self.parse_path_pattern()?;
        Ok(NamedPattern { variable, path })
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
    fn parse_expression_bp(&mut self, min_bp: u8) -> ParseResult<Expression> {
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
                self.consume();
                let inner = self.parse_expression()?;
                self.eat(&TokenKind::RParen, "`)` to close grouped expression")?;
                inner
            }
            TokenKind::LBracket => self.parse_list_literal()?,
            TokenKind::LBrace => Expression::Map(self.parse_map_literal()?),
            TokenKind::Case => self.parse_case()?,
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
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_list_literal(&mut self) -> ParseResult<Expression> {
        let span = self.peek_span();
        self.consume(); // [
        let mut items = Vec::new();
        if !matches!(self.peek_kind(), TokenKind::RBracket) {
            items.push(self.parse_expression()?);
            while matches!(self.peek_kind(), TokenKind::Comma) {
                self.consume();
                items.push(self.parse_expression()?);
            }
        }
        self.eat(&TokenKind::RBracket, "`]` to close list literal")?;
        Ok(Expression::List { items, span })
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
}
