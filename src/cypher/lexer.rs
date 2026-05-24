//! Cypher lexer — converts a Cypher source string into a stream of
//! `Token`s for the parser.
//!
//! The lexer is the entry point of Phase 10 (task `00061`). It recognises
//! the lexical surface that the upcoming parser (`00062`) and executor
//! (`00063`) layers consume:
//!
//! - **Reserved keywords** (case-insensitive): `MATCH`, `CREATE`, `MERGE`,
//!   `DELETE`, `DETACH`, `SET`, `REMOVE`, `WHERE`, `RETURN`, `WITH`,
//!   `ORDER`, `BY`, `ASC`/`ASCENDING`, `DESC`/`DESCENDING`, `LIMIT`,
//!   `SKIP`, `OPTIONAL`, `UNION`, `ALL`, `AS`, `DISTINCT`, `AND`, `OR`,
//!   `XOR`, `NOT`, `IN`, `IS`, `STARTS`, `ENDS`, `CONTAINS`, `CASE`,
//!   `WHEN`, `THEN`, `ELSE`, `END`, `UNWIND`, `CALL`, `YIELD`, `FOREACH`,
//!   `ON`. The boolean / null literals `TRUE`, `FALSE`, `NULL` are
//!   matched here too.
//! - **Identifiers**: ASCII or Unicode letters (incl. underscore) followed
//!   by letters / digits / underscore, OR backtick-quoted segments that
//!   may contain arbitrary characters except an unescaped backtick.
//! - **Literals**: signed-by-context integers (`123`, `0xff`, `0o17`),
//!   floats (`1.5`, `1.5e10`, `.5`, `5.`), and strings (single- or
//!   double-quoted, with the escape sequences `\\`, `\'`, `\"`, `\n`,
//!   `\r`, `\t`, `\b`, `\f`, `\u{XXXX}`).
//! - **Operators**: `=`, `<>`, `<`, `<=`, `>`, `>=`, `=~`, `+`, `-`, `*`,
//!   `/`, `%`, `^`, `.`, `..`.
//! - **Pattern syntax**: `->`, `<-`, `|`, `:`, plus the bracketing tokens
//!   `(`, `)`, `[`, `]`, `{`, `}`, `,`, `;`.
//! - **Parameters**: `$name` (identifier-style) or `$0` (numeric).
//! - **Comments**: `// ...\n` line comments and `/* ... */` block
//!   comments. Comments are *skipped* — they never appear as tokens.
//!
//! The lexer never panics. Every error is returned via `LexError` with
//! a `Span` indicating the offending byte range. Position tracking is
//! 1-based line and 1-based column (character — not byte — column),
//! matching how editors render error markers.
//!
//! Downstream consumers (`00062` parser onwards) get a `Vec<Token>` from
//! the `tokenize` entry point; a `TokenKind::Eof` sentinel is appended
//! so look-ahead parsers don't need to special-case the end of the
//! stream.

use std::fmt;

/// A source location covered by a token, in both byte offsets (for slicing
/// the original source) and 1-based `(line, column)` (for editor-friendly
/// diagnostics).
///
/// `column` counts Unicode scalar values, not bytes, so a multi-byte
/// character occupies a single column — consistent with how editors and
/// LSP clients report positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Inclusive byte offset where the token starts.
    pub start: usize,
    /// Exclusive byte offset where the token ends.
    pub end: usize,
    /// 1-based line number of the token's start.
    pub line: u32,
    /// 1-based column number (character count) of the token's start.
    pub column: u32,
}

impl Span {
    /// Build a new [`Span`].
    pub fn new(start: usize, end: usize, line: u32, column: u32) -> Self {
        Self {
            start,
            end,
            line,
            column,
        }
    }
}

/// A single lexical token: a [`TokenKind`] plus the source [`Span`] it
/// occupied.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Lexical category and (where applicable) the parsed value.
    pub kind: TokenKind,
    /// Source location the token was lexed from.
    pub span: Span,
}

impl Token {
    /// Build a new [`Token`].
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// The lexical category of a [`Token`].
///
/// Literal-bearing variants (`Integer`, `Float`, `String`, `Identifier`,
/// `Parameter`) carry the already-parsed value so the parser does not
/// need to re-walk the source slice.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // ---- Literals --------------------------------------------------
    /// Integer literal: `42`, `0`, `0xff`, `0o17`. Parsed into `i64`.
    Integer(i64),
    /// Floating-point literal: `1.5`, `1.5e10`, `.5`, `5.`. Parsed into
    /// `f64` (`NaN` / `Inf` are never produced — the lexer rejects them
    /// upstream).
    Float(f64),
    /// String literal — single- or double-quoted in source, escape
    /// sequences resolved.
    String(String),
    /// The `TRUE` keyword.
    True,
    /// The `FALSE` keyword.
    False,
    /// The `NULL` keyword.
    Null,

    // ---- Identifiers and parameters --------------------------------
    /// Plain identifier (`foo`, `name`) or backtick-quoted identifier
    /// (`` `foo bar` ``). The contained string is the *resolved* name —
    /// backticks and their escapes have been stripped.
    Identifier(String),
    /// `$name` or `$0` parameter reference. The contained string is the
    /// name without the leading `$`.
    Parameter(String),

    // ---- Statement / clause keywords -------------------------------
    /// `MATCH`.
    Match,
    /// `OPTIONAL`. The grammar parses `OPTIONAL MATCH` as two
    /// consecutive tokens.
    Optional,
    /// `CREATE`.
    Create,
    /// `MERGE`.
    Merge,
    /// `DELETE`.
    Delete,
    /// `DETACH`.
    Detach,
    /// `SET`.
    Set,
    /// `REMOVE`.
    Remove,
    /// `WHERE`.
    Where,
    /// `RETURN`.
    Return,
    /// `WITH`.
    With,
    /// `ORDER`.
    Order,
    /// `BY`.
    By,
    /// `ASC` or `ASCENDING`.
    Asc,
    /// `DESC` or `DESCENDING`.
    Desc,
    /// `LIMIT`.
    Limit,
    /// `SKIP`.
    Skip,
    /// `UNION`.
    Union,
    /// `ALL`.
    All,
    /// `AS`.
    As,
    /// `DISTINCT`.
    Distinct,
    /// `UNWIND`.
    Unwind,
    /// `CALL`.
    Call,
    /// `YIELD`.
    Yield,
    /// `FOREACH`.
    Foreach,
    /// `ON`.
    On,

    // ---- Boolean operators -----------------------------------------
    /// `AND`.
    And,
    /// `OR`.
    Or,
    /// `XOR`.
    Xor,
    /// `NOT`.
    Not,

    // ---- Pattern / string predicates -------------------------------
    /// `IN`.
    In,
    /// `IS`. The grammar parses `IS NULL` / `IS NOT NULL` as separate
    /// token sequences.
    Is,
    /// `STARTS` — paired with `WITH` by the grammar.
    Starts,
    /// `ENDS` — paired with `WITH` by the grammar.
    Ends,
    /// `CONTAINS`.
    Contains,
    /// `EXISTS`.
    Exists,

    // ---- Control flow ----------------------------------------------
    /// `CASE`.
    Case,
    /// `WHEN`.
    When,
    /// `THEN`.
    Then,
    /// `ELSE`.
    Else,
    /// `END`.
    End,

    // ---- Comparison operators --------------------------------------
    /// `=`. In Cypher, `=` is both equality (in `WHERE`) and assignment
    /// (in `SET`). The parser disambiguates by context.
    Eq,
    /// `<>` — not-equals.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `=~` — regex match.
    RegexMatch,

    // ---- Arithmetic operators --------------------------------------
    /// `+`.
    Plus,
    /// `-`. Doubles as unary negation and as an undirected-edge dash in
    /// pattern syntax; the parser disambiguates by context.
    Minus,
    /// `*`. Doubles as the wildcard projection in `RETURN *` and as
    /// arithmetic multiplication.
    Star,
    /// `/`.
    Slash,
    /// `%`.
    Percent,
    /// `^`.
    Caret,

    // ---- Property access / ranges ----------------------------------
    /// `.` — property access.
    Dot,
    /// `..` — range, e.g. in variable-length paths `[*1..3]`.
    DotDot,

    // ---- Pattern syntax --------------------------------------------
    /// `->` — directed-edge arrow.
    Arrow,
    /// `<-` — directed-edge arrow (left).
    LArrow,
    /// `|` — relationship-type alternation.
    Pipe,
    /// `:` — label / type separator.
    Colon,

    // ---- Brackets and punctuation ----------------------------------
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `[`.
    LBracket,
    /// `]`.
    RBracket,
    /// `{`.
    LBrace,
    /// `}`.
    RBrace,
    /// `,`.
    Comma,
    /// `;`.
    Semicolon,

    // ---- End of input ----------------------------------------------
    /// Sentinel emitted at the end of [`tokenize`]'s output so look-ahead
    /// parsers do not need to bound-check.
    Eof,
}

impl TokenKind {
    /// Returns `true` if this token is a reserved keyword (including
    /// boolean / null literals).
    ///
    /// Useful for diagnostics: a parser that allows a keyword to act as
    /// an identifier (the Cypher spec allows this in some positions —
    /// see task `00062`) can check this flag to flag the case.
    pub fn is_keyword(&self) -> bool {
        matches!(
            self,
            Self::Match
                | Self::Optional
                | Self::Create
                | Self::Merge
                | Self::Delete
                | Self::Detach
                | Self::Set
                | Self::Remove
                | Self::Where
                | Self::Return
                | Self::With
                | Self::Order
                | Self::By
                | Self::Asc
                | Self::Desc
                | Self::Limit
                | Self::Skip
                | Self::Union
                | Self::All
                | Self::As
                | Self::Distinct
                | Self::Unwind
                | Self::Call
                | Self::Yield
                | Self::Foreach
                | Self::On
                | Self::And
                | Self::Or
                | Self::Xor
                | Self::Not
                | Self::In
                | Self::Is
                | Self::Starts
                | Self::Ends
                | Self::Contains
                | Self::Exists
                | Self::Case
                | Self::When
                | Self::Then
                | Self::Else
                | Self::End
                | Self::True
                | Self::False
                | Self::Null
        )
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s:?}"),
            Self::Identifier(s) => f.write_str(s),
            Self::Parameter(s) => write!(f, "${s}"),
            Self::Match => f.write_str("MATCH"),
            Self::Optional => f.write_str("OPTIONAL"),
            Self::Create => f.write_str("CREATE"),
            Self::Merge => f.write_str("MERGE"),
            Self::Delete => f.write_str("DELETE"),
            Self::Detach => f.write_str("DETACH"),
            Self::Set => f.write_str("SET"),
            Self::Remove => f.write_str("REMOVE"),
            Self::Where => f.write_str("WHERE"),
            Self::Return => f.write_str("RETURN"),
            Self::With => f.write_str("WITH"),
            Self::Order => f.write_str("ORDER"),
            Self::By => f.write_str("BY"),
            Self::Asc => f.write_str("ASC"),
            Self::Desc => f.write_str("DESC"),
            Self::Limit => f.write_str("LIMIT"),
            Self::Skip => f.write_str("SKIP"),
            Self::Union => f.write_str("UNION"),
            Self::All => f.write_str("ALL"),
            Self::As => f.write_str("AS"),
            Self::Distinct => f.write_str("DISTINCT"),
            Self::Unwind => f.write_str("UNWIND"),
            Self::Call => f.write_str("CALL"),
            Self::Yield => f.write_str("YIELD"),
            Self::Foreach => f.write_str("FOREACH"),
            Self::On => f.write_str("ON"),
            Self::And => f.write_str("AND"),
            Self::Or => f.write_str("OR"),
            Self::Xor => f.write_str("XOR"),
            Self::Not => f.write_str("NOT"),
            Self::In => f.write_str("IN"),
            Self::Is => f.write_str("IS"),
            Self::Starts => f.write_str("STARTS"),
            Self::Ends => f.write_str("ENDS"),
            Self::Contains => f.write_str("CONTAINS"),
            Self::Exists => f.write_str("EXISTS"),
            Self::Case => f.write_str("CASE"),
            Self::When => f.write_str("WHEN"),
            Self::Then => f.write_str("THEN"),
            Self::Else => f.write_str("ELSE"),
            Self::End => f.write_str("END"),
            Self::True => f.write_str("TRUE"),
            Self::False => f.write_str("FALSE"),
            Self::Null => f.write_str("NULL"),
            Self::Eq => f.write_str("="),
            Self::Ne => f.write_str("<>"),
            Self::Lt => f.write_str("<"),
            Self::Le => f.write_str("<="),
            Self::Gt => f.write_str(">"),
            Self::Ge => f.write_str(">="),
            Self::RegexMatch => f.write_str("=~"),
            Self::Plus => f.write_str("+"),
            Self::Minus => f.write_str("-"),
            Self::Star => f.write_str("*"),
            Self::Slash => f.write_str("/"),
            Self::Percent => f.write_str("%"),
            Self::Caret => f.write_str("^"),
            Self::Dot => f.write_str("."),
            Self::DotDot => f.write_str(".."),
            Self::Arrow => f.write_str("->"),
            Self::LArrow => f.write_str("<-"),
            Self::Pipe => f.write_str("|"),
            Self::Colon => f.write_str(":"),
            Self::LParen => f.write_str("("),
            Self::RParen => f.write_str(")"),
            Self::LBracket => f.write_str("["),
            Self::RBracket => f.write_str("]"),
            Self::LBrace => f.write_str("{"),
            Self::RBrace => f.write_str("}"),
            Self::Comma => f.write_str(","),
            Self::Semicolon => f.write_str(";"),
            Self::Eof => f.write_str("<eof>"),
        }
    }
}

/// A lexical error — produced when [`tokenize`] encounters source that is
/// not a valid Cypher token.
///
/// Each variant carries the [`Span`] of the offending input so callers can
/// render `^^^` markers under the source.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LexError {
    /// A string literal opened with `'` or `"` but the matching quote was
    /// never found before end-of-input.
    #[error("unterminated string literal at line {}, column {}", .span.line, .span.column)]
    UnterminatedString {
        /// Source span of the offending opening quote.
        span: Span,
    },

    /// A backtick-quoted identifier opened with `` ` `` but the matching
    /// backtick was never found before end-of-input.
    #[error("unterminated backtick identifier at line {}, column {}", .span.line, .span.column)]
    UnterminatedBacktick {
        /// Source span of the offending opening backtick.
        span: Span,
    },

    /// A `/*` block-comment opener was not balanced by a closing `*/`
    /// before end-of-input.
    #[error("unterminated block comment at line {}, column {}", .span.line, .span.column)]
    UnterminatedBlockComment {
        /// Source span of the offending `/*`.
        span: Span,
    },

    /// A string-literal escape sequence (e.g. `\z`) is not recognised.
    #[error("invalid escape sequence `\\{ch}` at line {}, column {}", .span.line, .span.column)]
    InvalidEscape {
        /// The character following the backslash.
        ch: char,
        /// Source span of the offending escape.
        span: Span,
    },

    /// A `\u{XXXX}` Unicode escape did not parse — either the braces are
    /// missing, the hex digits are missing, or the code point is not a
    /// valid Unicode scalar value.
    #[error("invalid unicode escape at line {}, column {}", .span.line, .span.column)]
    InvalidUnicodeEscape {
        /// Source span of the offending escape.
        span: Span,
    },

    /// A `$` parameter prefix was not followed by an identifier or
    /// integer.
    #[error("invalid parameter at line {}, column {}", .span.line, .span.column)]
    InvalidParameter {
        /// Source span of the offending `$`.
        span: Span,
    },

    /// A numeric literal could not be parsed as an `i64` or `f64`
    /// (for example, an integer that overflows `i64::MAX`).
    #[error("invalid number `{text}` at line {}, column {}", .span.line, .span.column)]
    InvalidNumber {
        /// The raw source text of the offending number.
        text: String,
        /// Source span of the offending number.
        span: Span,
    },

    /// A character does not begin any valid Cypher token.
    #[error("unexpected character `{ch}` at line {}, column {}", .span.line, .span.column)]
    UnexpectedChar {
        /// The offending character.
        ch: char,
        /// Source span of the offending character.
        span: Span,
    },
}

impl LexError {
    /// Returns the [`Span`] of the offending source.
    pub fn span(&self) -> Span {
        match self {
            Self::UnterminatedString { span }
            | Self::UnterminatedBacktick { span }
            | Self::UnterminatedBlockComment { span }
            | Self::InvalidEscape { span, .. }
            | Self::InvalidUnicodeEscape { span }
            | Self::InvalidParameter { span }
            | Self::InvalidNumber { span, .. }
            | Self::UnexpectedChar { span, .. } => *span,
        }
    }
}

/// Result alias for lexer operations.
pub type LexResult<T> = std::result::Result<T, LexError>;

/// Lex `source` into a stream of [`Token`]s terminated by
/// [`TokenKind::Eof`].
///
/// # Errors
///
/// Returns the first [`LexError`] encountered. The lexer is fail-fast
/// because subsequent tokens after a malformed one are very rarely
/// meaningful; the parser layer (task `00062`) is the right place for
/// error recovery.
///
/// # Example
///
/// ```
/// use drevo::cypher::lexer::{tokenize, TokenKind};
///
/// let tokens = tokenize("MATCH (n) RETURN n").unwrap();
/// assert_eq!(tokens[0].kind, TokenKind::Match);
/// assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
/// ```
pub fn tokenize(source: &str) -> LexResult<Vec<Token>> {
    let mut lexer = Lexer::new(source);
    let mut tokens = Vec::new();
    while let Some(tok) = lexer.next_token()? {
        tokens.push(tok);
    }
    tokens.push(Token::new(
        TokenKind::Eof,
        Span::new(source.len(), source.len(), lexer.line, lexer.column),
    ));
    Ok(tokens)
}

/// Hand-rolled, character-at-a-time lexer.
///
/// Implementation detail — exposed only via the free-function [`tokenize`].
struct Lexer<'src> {
    source: &'src str,
    /// Byte cursor into `source`.
    cursor: usize,
    /// 1-based line of the next character.
    line: u32,
    /// 1-based column (character count) of the next character.
    column: u32,
}

impl<'src> Lexer<'src> {
    fn new(source: &'src str) -> Self {
        Self {
            source,
            cursor: 0,
            line: 1,
            column: 1,
        }
    }

    /// Look at the next character without consuming it.
    fn peek(&self) -> Option<char> {
        self.source[self.cursor..].chars().next()
    }

    /// Look at the character after `peek()`.
    fn peek2(&self) -> Option<char> {
        let mut it = self.source[self.cursor..].chars();
        it.next()?;
        it.next()
    }

    /// Consume and return the next character, advancing the line/column
    /// counters.
    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.cursor += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    /// Skip whitespace and comments.
    ///
    /// Returns `Err` if a block comment is unterminated.
    fn skip_trivia(&mut self) -> LexResult<()> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.advance();
                }
                Some('/') if self.peek2() == Some('/') => {
                    // line comment: //...\n
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }
                Some('/') if self.peek2() == Some('*') => {
                    let start = self.cursor;
                    let line = self.line;
                    let column = self.column;
                    self.advance(); // /
                    self.advance(); // *
                    loop {
                        match self.peek() {
                            None => {
                                return Err(LexError::UnterminatedBlockComment {
                                    span: Span::new(start, start + 2, line, column),
                                });
                            }
                            Some('*') if self.peek2() == Some('/') => {
                                self.advance(); // *
                                self.advance(); // /
                                break;
                            }
                            Some(_) => {
                                self.advance();
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn next_token(&mut self) -> LexResult<Option<Token>> {
        self.skip_trivia()?;
        let start = self.cursor;
        let line = self.line;
        let column = self.column;
        let c = match self.peek() {
            None => return Ok(None),
            Some(c) => c,
        };

        // Multi-char operators and punctuation first, then single-char.
        let kind = match c {
            '(' => {
                self.advance();
                TokenKind::LParen
            }
            ')' => {
                self.advance();
                TokenKind::RParen
            }
            '[' => {
                self.advance();
                TokenKind::LBracket
            }
            ']' => {
                self.advance();
                TokenKind::RBracket
            }
            '{' => {
                self.advance();
                TokenKind::LBrace
            }
            '}' => {
                self.advance();
                TokenKind::RBrace
            }
            ',' => {
                self.advance();
                TokenKind::Comma
            }
            ';' => {
                self.advance();
                TokenKind::Semicolon
            }
            ':' => {
                self.advance();
                TokenKind::Colon
            }
            '|' => {
                self.advance();
                TokenKind::Pipe
            }
            '+' => {
                self.advance();
                TokenKind::Plus
            }
            '*' => {
                self.advance();
                TokenKind::Star
            }
            '/' => {
                self.advance();
                TokenKind::Slash
            }
            '%' => {
                self.advance();
                TokenKind::Percent
            }
            '^' => {
                self.advance();
                TokenKind::Caret
            }
            '-' => {
                self.advance();
                if self.peek() == Some('>') {
                    self.advance();
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '<' => {
                self.advance();
                match self.peek() {
                    Some('=') => {
                        self.advance();
                        TokenKind::Le
                    }
                    Some('>') => {
                        self.advance();
                        TokenKind::Ne
                    }
                    Some('-') => {
                        self.advance();
                        TokenKind::LArrow
                    }
                    _ => TokenKind::Lt,
                }
            }
            '>' => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::Ge
                } else {
                    TokenKind::Gt
                }
            }
            '=' => {
                self.advance();
                if self.peek() == Some('~') {
                    self.advance();
                    TokenKind::RegexMatch
                } else {
                    TokenKind::Eq
                }
            }
            '.' => {
                if matches!(self.peek2(), Some(c) if c.is_ascii_digit()) {
                    self.read_number(start, line, column)?
                } else {
                    self.advance();
                    if self.peek() == Some('.') {
                        self.advance();
                        TokenKind::DotDot
                    } else {
                        TokenKind::Dot
                    }
                }
            }
            '\'' | '"' => self.read_string(c, start, line, column)?,
            '`' => self.read_backtick_identifier(start, line, column)?,
            '$' => self.read_parameter(start, line, column)?,
            c if c.is_ascii_digit() => self.read_number(start, line, column)?,
            c if is_identifier_start(c) => self.read_identifier_or_keyword(),
            c => {
                self.advance();
                return Err(LexError::UnexpectedChar {
                    ch: c,
                    span: Span::new(start, self.cursor, line, column),
                });
            }
        };
        let span = Span::new(start, self.cursor, line, column);
        Ok(Some(Token::new(kind, span)))
    }

    fn read_identifier_or_keyword(&mut self) -> TokenKind {
        let start = self.cursor;
        while let Some(c) = self.peek() {
            if is_identifier_continue(c) {
                self.advance();
            } else {
                break;
            }
        }
        let raw = &self.source[start..self.cursor];
        keyword_or_identifier(raw)
    }

    fn read_backtick_identifier(
        &mut self,
        start: usize,
        line: u32,
        column: u32,
    ) -> LexResult<TokenKind> {
        self.advance(); // opening backtick
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(LexError::UnterminatedBacktick {
                        span: Span::new(start, start + 1, line, column),
                    });
                }
                Some('`') => {
                    self.advance();
                    // doubled backtick escapes to one literal backtick
                    if self.peek() == Some('`') {
                        value.push('`');
                        self.advance();
                    } else {
                        return Ok(TokenKind::Identifier(value));
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.advance();
                }
            }
        }
    }

    fn read_parameter(&mut self, start: usize, line: u32, column: u32) -> LexResult<TokenKind> {
        self.advance(); // $
        let name_start = self.cursor;
        // Numeric parameter: $0, $42, ...
        if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            let name = self.source[name_start..self.cursor].to_string();
            return Ok(TokenKind::Parameter(name));
        }
        // Named parameter: $name. The opener may also be backtick-quoted: $`weird name`.
        if self.peek() == Some('`') {
            // Reuse backtick logic for the name portion.
            let backtick_kind =
                self.read_backtick_identifier(self.cursor, self.line, self.column)?;
            if let TokenKind::Identifier(name) = backtick_kind {
                return Ok(TokenKind::Parameter(name));
            }
            unreachable!("read_backtick_identifier returns Identifier or errors");
        }
        if matches!(self.peek(), Some(c) if is_identifier_start(c)) {
            while let Some(c) = self.peek() {
                if is_identifier_continue(c) {
                    self.advance();
                } else {
                    break;
                }
            }
            let name = self.source[name_start..self.cursor].to_string();
            return Ok(TokenKind::Parameter(name));
        }
        Err(LexError::InvalidParameter {
            span: Span::new(start, self.cursor, line, column),
        })
    }

    fn read_string(
        &mut self,
        quote: char,
        start: usize,
        line: u32,
        column: u32,
    ) -> LexResult<TokenKind> {
        self.advance(); // opening quote
        let mut value = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(LexError::UnterminatedString {
                        span: Span::new(start, start + 1, line, column),
                    });
                }
                Some(c) if c == quote => {
                    self.advance();
                    return Ok(TokenKind::String(value));
                }
                Some('\\') => {
                    let esc_start = self.cursor;
                    let esc_line = self.line;
                    let esc_col = self.column;
                    self.advance(); // backslash
                    match self.peek() {
                        Some('\\') => {
                            value.push('\\');
                            self.advance();
                        }
                        Some('\'') => {
                            value.push('\'');
                            self.advance();
                        }
                        Some('"') => {
                            value.push('"');
                            self.advance();
                        }
                        Some('n') => {
                            value.push('\n');
                            self.advance();
                        }
                        Some('r') => {
                            value.push('\r');
                            self.advance();
                        }
                        Some('t') => {
                            value.push('\t');
                            self.advance();
                        }
                        Some('b') => {
                            value.push('\u{0008}');
                            self.advance();
                        }
                        Some('f') => {
                            value.push('\u{000C}');
                            self.advance();
                        }
                        Some('0') => {
                            value.push('\0');
                            self.advance();
                        }
                        Some('`') => {
                            value.push('`');
                            self.advance();
                        }
                        Some('u') => {
                            self.advance(); // u
                            value.push(self.read_unicode_escape(esc_start, esc_line, esc_col)?);
                        }
                        Some(ch) => {
                            return Err(LexError::InvalidEscape {
                                ch,
                                span: Span::new(
                                    esc_start,
                                    self.cursor + ch.len_utf8(),
                                    esc_line,
                                    esc_col,
                                ),
                            });
                        }
                        None => {
                            return Err(LexError::UnterminatedString {
                                span: Span::new(start, start + 1, line, column),
                            });
                        }
                    }
                }
                Some(c) => {
                    value.push(c);
                    self.advance();
                }
            }
        }
    }

    fn read_unicode_escape(
        &mut self,
        esc_start: usize,
        esc_line: u32,
        esc_col: u32,
    ) -> LexResult<char> {
        // Two accepted forms:
        //   \uXXXX          — exactly 4 hex digits (Java/Cypher classic)
        //   \u{XXXX}        — braces with 1..=6 hex digits (Rust-style)
        if self.peek() == Some('{') {
            self.advance();
            let digits_start = self.cursor;
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    self.advance();
                } else {
                    break;
                }
            }
            let digits = &self.source[digits_start..self.cursor];
            if digits.is_empty() || self.peek() != Some('}') {
                return Err(LexError::InvalidUnicodeEscape {
                    span: Span::new(esc_start, self.cursor, esc_line, esc_col),
                });
            }
            self.advance(); // }
            let code =
                u32::from_str_radix(digits, 16).map_err(|_| LexError::InvalidUnicodeEscape {
                    span: Span::new(esc_start, self.cursor, esc_line, esc_col),
                })?;
            char::from_u32(code).ok_or(LexError::InvalidUnicodeEscape {
                span: Span::new(esc_start, self.cursor, esc_line, esc_col),
            })
        } else {
            let digits_start = self.cursor;
            for _ in 0..4 {
                match self.peek() {
                    Some(c) if c.is_ascii_hexdigit() => {
                        self.advance();
                    }
                    _ => {
                        return Err(LexError::InvalidUnicodeEscape {
                            span: Span::new(esc_start, self.cursor, esc_line, esc_col),
                        });
                    }
                }
            }
            let digits = &self.source[digits_start..self.cursor];
            let code =
                u32::from_str_radix(digits, 16).map_err(|_| LexError::InvalidUnicodeEscape {
                    span: Span::new(esc_start, self.cursor, esc_line, esc_col),
                })?;
            char::from_u32(code).ok_or(LexError::InvalidUnicodeEscape {
                span: Span::new(esc_start, self.cursor, esc_line, esc_col),
            })
        }
    }

    fn read_number(&mut self, start: usize, line: u32, column: u32) -> LexResult<TokenKind> {
        // Hex / octal integers: 0x... / 0o... (no float counterpart).
        if self.peek() == Some('0') {
            match self.peek2() {
                Some('x') | Some('X') => {
                    self.advance(); // 0
                    self.advance(); // x
                    let hex_start = self.cursor;
                    while let Some(c) = self.peek() {
                        if c.is_ascii_hexdigit() {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    let text = &self.source[start..self.cursor];
                    let digits = &self.source[hex_start..self.cursor];
                    if digits.is_empty() {
                        return Err(LexError::InvalidNumber {
                            text: text.to_string(),
                            span: Span::new(start, self.cursor, line, column),
                        });
                    }
                    return i64::from_str_radix(digits, 16)
                        .map(TokenKind::Integer)
                        .map_err(|_| LexError::InvalidNumber {
                            text: text.to_string(),
                            span: Span::new(start, self.cursor, line, column),
                        });
                }
                Some('o') | Some('O') => {
                    self.advance();
                    self.advance();
                    let oct_start = self.cursor;
                    while let Some(c) = self.peek() {
                        if ('0'..='7').contains(&c) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    let text = &self.source[start..self.cursor];
                    let digits = &self.source[oct_start..self.cursor];
                    if digits.is_empty() {
                        return Err(LexError::InvalidNumber {
                            text: text.to_string(),
                            span: Span::new(start, self.cursor, line, column),
                        });
                    }
                    return i64::from_str_radix(digits, 8)
                        .map(TokenKind::Integer)
                        .map_err(|_| LexError::InvalidNumber {
                            text: text.to_string(),
                            span: Span::new(start, self.cursor, line, column),
                        });
                }
                _ => {}
            }
        }

        let mut is_float = false;

        // Integer / pre-dot part.
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }

        // Fractional part: `.digit*`. A bare trailing `.` (e.g. `5.`) is
        // accepted as a float. A `..` after a digit (`1..3`) must NOT be
        // consumed — it is the range operator.
        if self.peek() == Some('.') && self.peek2() != Some('.') {
            // Don't consume if `.` is followed by an identifier-start
            // (property access like `n.foo`) — but `read_number` is only
            // entered from a digit start, so `.` after the integer is
            // unambiguously a fractional point unless `..` (range).
            self.advance();
            is_float = true;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        } else if self.cursor == start && self.peek() == Some('.') {
            // Leading-dot float: `.5`. Only reachable if read_number was
            // invoked because peek2() was a digit at the top-level
            // dispatcher.
            self.advance();
            is_float = true;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Exponent part: `e` / `E` `[+-]?` digits.
        if matches!(self.peek(), Some('e') | Some('E')) {
            let save = self.cursor;
            let save_line = self.line;
            let save_col = self.column;
            self.advance();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.advance();
            }
            let exp_start = self.cursor;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            if self.cursor == exp_start {
                // No digits in exponent — roll back.
                self.cursor = save;
                self.line = save_line;
                self.column = save_col;
            } else {
                is_float = true;
            }
        }

        let text = &self.source[start..self.cursor];
        if is_float {
            text.parse::<f64>()
                .map(TokenKind::Float)
                .map_err(|_| LexError::InvalidNumber {
                    text: text.to_string(),
                    span: Span::new(start, self.cursor, line, column),
                })
        } else {
            text.parse::<i64>()
                .map(TokenKind::Integer)
                .map_err(|_| LexError::InvalidNumber {
                    text: text.to_string(),
                    span: Span::new(start, self.cursor, line, column),
                })
        }
    }
}

fn is_identifier_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_identifier_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

fn keyword_or_identifier(raw: &str) -> TokenKind {
    // Cypher keywords are case-insensitive. Use to_ascii_uppercase so the
    // lookup is allocation-free for the common ASCII case.
    let mut upper = String::with_capacity(raw.len());
    for c in raw.chars() {
        upper.push(c.to_ascii_uppercase());
    }
    match upper.as_str() {
        "MATCH" => TokenKind::Match,
        "OPTIONAL" => TokenKind::Optional,
        "CREATE" => TokenKind::Create,
        "MERGE" => TokenKind::Merge,
        "DELETE" => TokenKind::Delete,
        "DETACH" => TokenKind::Detach,
        "SET" => TokenKind::Set,
        "REMOVE" => TokenKind::Remove,
        "WHERE" => TokenKind::Where,
        "RETURN" => TokenKind::Return,
        "WITH" => TokenKind::With,
        "ORDER" => TokenKind::Order,
        "BY" => TokenKind::By,
        "ASC" | "ASCENDING" => TokenKind::Asc,
        "DESC" | "DESCENDING" => TokenKind::Desc,
        "LIMIT" => TokenKind::Limit,
        "SKIP" => TokenKind::Skip,
        "UNION" => TokenKind::Union,
        "ALL" => TokenKind::All,
        "AS" => TokenKind::As,
        "DISTINCT" => TokenKind::Distinct,
        "UNWIND" => TokenKind::Unwind,
        "CALL" => TokenKind::Call,
        "YIELD" => TokenKind::Yield,
        "FOREACH" => TokenKind::Foreach,
        "ON" => TokenKind::On,
        "AND" => TokenKind::And,
        "OR" => TokenKind::Or,
        "XOR" => TokenKind::Xor,
        "NOT" => TokenKind::Not,
        "IN" => TokenKind::In,
        "IS" => TokenKind::Is,
        "STARTS" => TokenKind::Starts,
        "ENDS" => TokenKind::Ends,
        "CONTAINS" => TokenKind::Contains,
        "EXISTS" => TokenKind::Exists,
        "CASE" => TokenKind::Case,
        "WHEN" => TokenKind::When,
        "THEN" => TokenKind::Then,
        "ELSE" => TokenKind::Else,
        "END" => TokenKind::End,
        "TRUE" => TokenKind::True,
        "FALSE" => TokenKind::False,
        "NULL" => TokenKind::Null,
        _ => TokenKind::Identifier(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Eof))
            .collect()
    }

    // ---- keywords --------------------------------------------------

    #[test]
    fn keyword_match_uppercase() {
        assert_eq!(kinds("MATCH"), vec![TokenKind::Match]);
    }

    #[test]
    fn keyword_match_lowercase() {
        assert_eq!(kinds("match"), vec![TokenKind::Match]);
    }

    #[test]
    fn keyword_match_mixed_case() {
        assert_eq!(kinds("MaTcH"), vec![TokenKind::Match]);
    }

    #[test]
    fn keyword_all_statement_keywords_recognized() {
        let source =
            "MATCH OPTIONAL CREATE MERGE DELETE DETACH SET REMOVE WHERE RETURN WITH ORDER BY \
             ASC DESC LIMIT SKIP UNION ALL AS DISTINCT UNWIND CALL YIELD FOREACH ON";
        let kinds = kinds(source);
        assert_eq!(
            kinds,
            vec![
                TokenKind::Match,
                TokenKind::Optional,
                TokenKind::Create,
                TokenKind::Merge,
                TokenKind::Delete,
                TokenKind::Detach,
                TokenKind::Set,
                TokenKind::Remove,
                TokenKind::Where,
                TokenKind::Return,
                TokenKind::With,
                TokenKind::Order,
                TokenKind::By,
                TokenKind::Asc,
                TokenKind::Desc,
                TokenKind::Limit,
                TokenKind::Skip,
                TokenKind::Union,
                TokenKind::All,
                TokenKind::As,
                TokenKind::Distinct,
                TokenKind::Unwind,
                TokenKind::Call,
                TokenKind::Yield,
                TokenKind::Foreach,
                TokenKind::On,
            ]
        );
    }

    #[test]
    fn keyword_ascending_descending_aliases() {
        assert_eq!(
            kinds("ASCENDING DESCENDING"),
            vec![TokenKind::Asc, TokenKind::Desc]
        );
    }

    #[test]
    fn keyword_boolean_string_predicates() {
        assert_eq!(
            kinds("AND OR XOR NOT IN IS STARTS ENDS CONTAINS EXISTS"),
            vec![
                TokenKind::And,
                TokenKind::Or,
                TokenKind::Xor,
                TokenKind::Not,
                TokenKind::In,
                TokenKind::Is,
                TokenKind::Starts,
                TokenKind::Ends,
                TokenKind::Contains,
                TokenKind::Exists,
            ]
        );
    }

    #[test]
    fn keyword_case_when_then_else_end() {
        assert_eq!(
            kinds("CASE WHEN THEN ELSE END"),
            vec![
                TokenKind::Case,
                TokenKind::When,
                TokenKind::Then,
                TokenKind::Else,
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn keyword_literals_true_false_null() {
        assert_eq!(
            kinds("TRUE FALSE NULL"),
            vec![TokenKind::True, TokenKind::False, TokenKind::Null]
        );
    }

    // ---- identifiers -----------------------------------------------

    #[test]
    fn identifier_simple() {
        assert_eq!(kinds("foo"), vec![TokenKind::Identifier("foo".to_string())]);
    }

    #[test]
    fn identifier_with_underscore_and_digits() {
        assert_eq!(
            kinds("foo_bar123"),
            vec![TokenKind::Identifier("foo_bar123".to_string())]
        );
    }

    #[test]
    fn identifier_leading_underscore() {
        assert_eq!(kinds("_x"), vec![TokenKind::Identifier("_x".to_string())]);
    }

    #[test]
    fn identifier_cannot_start_with_digit() {
        let kinds = kinds("3foo");
        assert!(matches!(kinds[0], TokenKind::Integer(3)));
        assert!(matches!(kinds[1], TokenKind::Identifier(ref s) if s == "foo"));
    }

    #[test]
    fn identifier_unicode_letter() {
        assert_eq!(kinds("Π"), vec![TokenKind::Identifier("Π".to_string())]);
    }

    #[test]
    fn backtick_identifier_basic() {
        assert_eq!(
            kinds("`foo bar`"),
            vec![TokenKind::Identifier("foo bar".to_string())]
        );
    }

    #[test]
    fn backtick_identifier_contains_keyword() {
        // Inside backticks, `MATCH` is just an identifier name.
        assert_eq!(
            kinds("`MATCH`"),
            vec![TokenKind::Identifier("MATCH".to_string())]
        );
    }

    #[test]
    fn backtick_identifier_doubled_escapes() {
        assert_eq!(
            kinds("`a``b`"),
            vec![TokenKind::Identifier("a`b".to_string())]
        );
    }

    #[test]
    fn backtick_identifier_unterminated() {
        let err = tokenize("`foo").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedBacktick { .. }));
    }

    // ---- string literals -------------------------------------------

    #[test]
    fn string_single_quoted() {
        assert_eq!(
            kinds("'hello'"),
            vec![TokenKind::String("hello".to_string())]
        );
    }

    #[test]
    fn string_double_quoted() {
        assert_eq!(
            kinds("\"hello\""),
            vec![TokenKind::String("hello".to_string())]
        );
    }

    #[test]
    fn string_empty() {
        assert_eq!(kinds("''"), vec![TokenKind::String(String::new())]);
    }

    #[test]
    fn string_escape_quote() {
        assert_eq!(
            kinds("'it\\'s'"),
            vec![TokenKind::String("it's".to_string())]
        );
    }

    #[test]
    fn string_escape_double_quote_inside_double() {
        assert_eq!(
            kinds("\"a\\\"b\""),
            vec![TokenKind::String("a\"b".to_string())]
        );
    }

    #[test]
    fn string_escape_backslash() {
        assert_eq!(
            kinds("'a\\\\b'"),
            vec![TokenKind::String("a\\b".to_string())]
        );
    }

    #[test]
    fn string_escape_n_r_t() {
        assert_eq!(
            kinds("'\\n\\r\\t'"),
            vec![TokenKind::String("\n\r\t".to_string())]
        );
    }

    #[test]
    fn string_escape_unicode_braced() {
        assert_eq!(
            kinds(r#"'\u{2764}'"#),
            vec![TokenKind::String("❤".to_string())]
        );
    }

    #[test]
    fn string_escape_unicode_classic_4_hex() {
        assert_eq!(kinds(r#"'❤'"#), vec![TokenKind::String("❤".to_string())]);
    }

    #[test]
    fn string_unterminated() {
        let err = tokenize("'foo").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedString { .. }));
    }

    #[test]
    fn string_invalid_escape() {
        let err = tokenize(r"'\z'").unwrap_err();
        match err {
            LexError::InvalidEscape { ch, .. } => assert_eq!(ch, 'z'),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn string_invalid_unicode_escape() {
        let err = tokenize(r"'\u{ZZZZ}'").unwrap_err();
        assert!(matches!(err, LexError::InvalidUnicodeEscape { .. }));
    }

    #[test]
    fn string_invalid_unicode_escape_no_braces_short() {
        let err = tokenize(r"'\u12'").unwrap_err();
        assert!(matches!(err, LexError::InvalidUnicodeEscape { .. }));
    }

    // ---- numeric literals ------------------------------------------

    #[test]
    fn integer_zero() {
        assert_eq!(kinds("0"), vec![TokenKind::Integer(0)]);
    }

    #[test]
    fn integer_basic() {
        assert_eq!(kinds("42"), vec![TokenKind::Integer(42)]);
    }

    #[test]
    fn integer_max_i64() {
        assert_eq!(
            kinds("9223372036854775807"),
            vec![TokenKind::Integer(i64::MAX)]
        );
    }

    #[test]
    fn integer_overflow_errors() {
        let err = tokenize("99999999999999999999").unwrap_err();
        assert!(matches!(err, LexError::InvalidNumber { .. }));
    }

    #[test]
    fn integer_hex_lower_x() {
        assert_eq!(kinds("0xff"), vec![TokenKind::Integer(255)]);
    }

    #[test]
    fn integer_hex_upper_x() {
        assert_eq!(kinds("0XFF"), vec![TokenKind::Integer(255)]);
    }

    #[test]
    fn integer_octal() {
        assert_eq!(kinds("0o17"), vec![TokenKind::Integer(15)]);
    }

    #[test]
    fn float_basic() {
        assert_eq!(kinds("1.5"), vec![TokenKind::Float(1.5)]);
    }

    #[test]
    fn float_leading_dot() {
        assert_eq!(kinds(".5"), vec![TokenKind::Float(0.5)]);
    }

    #[test]
    fn float_trailing_dot() {
        assert_eq!(kinds("5."), vec![TokenKind::Float(5.0)]);
    }

    #[test]
    fn float_exponent_positive() {
        assert_eq!(kinds("1.5e10"), vec![TokenKind::Float(1.5e10)]);
    }

    #[test]
    fn float_exponent_negative() {
        assert_eq!(kinds("1.5E-10"), vec![TokenKind::Float(1.5e-10)]);
    }

    #[test]
    fn float_exponent_no_dot() {
        assert_eq!(kinds("3e4"), vec![TokenKind::Float(3e4)]);
    }

    #[test]
    fn number_followed_by_range_operator() {
        // 1..3 must be Integer(1), DotDot, Integer(3) — NOT Float(1.)
        // followed by Dot, Integer(3).
        assert_eq!(
            kinds("1..3"),
            vec![
                TokenKind::Integer(1),
                TokenKind::DotDot,
                TokenKind::Integer(3),
            ]
        );
    }

    #[test]
    fn number_property_access() {
        // n.foo is identifier, dot, identifier. Verify the dot doesn't
        // merge into a float when adjacent to an identifier.
        assert_eq!(
            kinds("n.foo"),
            vec![
                TokenKind::Identifier("n".to_string()),
                TokenKind::Dot,
                TokenKind::Identifier("foo".to_string()),
            ]
        );
    }

    // ---- parameters ------------------------------------------------

    #[test]
    fn parameter_named() {
        assert_eq!(
            kinds("$name"),
            vec![TokenKind::Parameter("name".to_string())]
        );
    }

    #[test]
    fn parameter_numeric() {
        assert_eq!(kinds("$0"), vec![TokenKind::Parameter("0".to_string())]);
    }

    #[test]
    fn parameter_backtick_quoted() {
        assert_eq!(
            kinds("$`weird name`"),
            vec![TokenKind::Parameter("weird name".to_string())]
        );
    }

    #[test]
    fn parameter_dangling_dollar() {
        let err = tokenize("$").unwrap_err();
        assert!(matches!(err, LexError::InvalidParameter { .. }));
    }

    #[test]
    fn parameter_invalid_char_after_dollar() {
        let err = tokenize("$+").unwrap_err();
        assert!(matches!(err, LexError::InvalidParameter { .. }));
    }

    // ---- operators -------------------------------------------------

    #[test]
    fn op_comparison_set() {
        assert_eq!(
            kinds("= <> < <= > >= =~"),
            vec![
                TokenKind::Eq,
                TokenKind::Ne,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::RegexMatch,
            ]
        );
    }

    #[test]
    fn op_arithmetic_set() {
        assert_eq!(
            kinds("+ - * / % ^"),
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Caret,
            ]
        );
    }

    #[test]
    fn op_dot_and_range() {
        assert_eq!(
            kinds(". ..  ..."),
            vec![
                TokenKind::Dot,
                TokenKind::DotDot,
                TokenKind::DotDot,
                TokenKind::Dot,
            ]
        );
    }

    #[test]
    fn op_arrows() {
        assert_eq!(kinds("-> <-"), vec![TokenKind::Arrow, TokenKind::LArrow]);
    }

    #[test]
    fn op_pipe_and_colon() {
        assert_eq!(kinds("| :"), vec![TokenKind::Pipe, TokenKind::Colon]);
    }

    #[test]
    fn punctuation_brackets() {
        assert_eq!(
            kinds("()[]{},;"),
            vec![
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::Comma,
                TokenKind::Semicolon,
            ]
        );
    }

    // ---- comments --------------------------------------------------

    #[test]
    fn comment_line_skipped() {
        assert_eq!(kinds("// comment\nMATCH"), vec![TokenKind::Match]);
    }

    #[test]
    fn comment_line_until_eof() {
        // No trailing newline — the comment still ends at EOF.
        assert_eq!(kinds("MATCH // tail"), vec![TokenKind::Match]);
    }

    #[test]
    fn comment_block_skipped() {
        assert_eq!(kinds("/* block */ MATCH"), vec![TokenKind::Match]);
    }

    #[test]
    fn comment_block_multiline() {
        assert_eq!(
            kinds("MATCH /*\nstill in comment\n*/ RETURN"),
            vec![TokenKind::Match, TokenKind::Return]
        );
    }

    #[test]
    fn comment_block_unterminated() {
        let err = tokenize("/* no end").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedBlockComment { .. }));
    }

    // ---- whitespace ------------------------------------------------

    #[test]
    fn whitespace_collapsed() {
        assert_eq!(
            kinds("MATCH   \t\r\n  RETURN"),
            vec![TokenKind::Match, TokenKind::Return]
        );
    }

    // ---- position tracking -----------------------------------------

    #[test]
    fn span_byte_offsets() {
        let tokens = tokenize("MATCH (n)").unwrap();
        assert_eq!(tokens[0].span, Span::new(0, 5, 1, 1));
        assert_eq!(tokens[1].span, Span::new(6, 7, 1, 7));
        assert_eq!(tokens[2].span, Span::new(7, 8, 1, 8));
        assert_eq!(tokens[3].span, Span::new(8, 9, 1, 9));
    }

    #[test]
    fn span_tracks_line_advance() {
        let tokens = tokenize("MATCH\nRETURN").unwrap();
        assert_eq!(tokens[0].span.line, 1);
        assert_eq!(tokens[0].span.column, 1);
        assert_eq!(tokens[1].span.line, 2);
        assert_eq!(tokens[1].span.column, 1);
    }

    #[test]
    fn span_column_counts_characters_not_bytes() {
        // 'Π' is 2 bytes UTF-8 but 1 column.
        let tokens = tokenize("Π x").unwrap();
        assert_eq!(tokens[0].span.column, 1);
        assert_eq!(tokens[1].span.column, 3);
    }

    #[test]
    fn span_eof_at_end() {
        let tokens = tokenize("MATCH").unwrap();
        let eof = tokens.last().unwrap();
        assert_eq!(eof.kind, TokenKind::Eof);
        assert_eq!(eof.span.start, 5);
        assert_eq!(eof.span.end, 5);
    }

    #[test]
    fn empty_input_produces_only_eof() {
        let tokens = tokenize("").unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn whitespace_only_produces_only_eof() {
        let tokens = tokenize("   \n\t  ").unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    // ---- error reporting -------------------------------------------

    #[test]
    fn unexpected_character_emits_error() {
        let err = tokenize("@").unwrap_err();
        match err {
            LexError::UnexpectedChar { ch, .. } => assert_eq!(ch, '@'),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lex_error_span_points_to_offender() {
        let err = tokenize("MATCH @").unwrap_err();
        let span = err.span();
        assert_eq!(span.line, 1);
        assert_eq!(span.column, 7);
    }

    // ---- TokenKind helper ------------------------------------------

    #[test]
    fn is_keyword_classifies_correctly() {
        assert!(TokenKind::Match.is_keyword());
        assert!(TokenKind::True.is_keyword());
        assert!(TokenKind::Null.is_keyword());
        assert!(!TokenKind::Identifier("foo".to_string()).is_keyword());
        assert!(!TokenKind::Integer(42).is_keyword());
        assert!(!TokenKind::LParen.is_keyword());
    }

    #[test]
    fn token_kind_display_round_trip_for_keywords() {
        // Spot-check a few that should render to their canonical
        // uppercase form.
        assert_eq!(TokenKind::Match.to_string(), "MATCH");
        assert_eq!(TokenKind::Return.to_string(), "RETURN");
        assert_eq!(TokenKind::Eq.to_string(), "=");
        assert_eq!(TokenKind::Arrow.to_string(), "->");
        assert_eq!(TokenKind::DotDot.to_string(), "..");
    }
}
