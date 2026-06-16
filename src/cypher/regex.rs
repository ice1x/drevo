//! A small, dependency-free regular-expression engine for the Cypher
//! `=~` operator.
//!
//! # Why hand-rolled?
//!
//! drevo deliberately keeps its production dependency tree minimal — every
//! algorithmic building block so far (the FTS trigram tokenizer, the Porter
//! stemmer, BM25 ranking, the HNSW vector index, the cost-based planner) is
//! written from scratch rather than pulled from crates.io. The `Cargo.toml`
//! even notes that the `regex` crate is intentionally kept *out* of the
//! production tree. Supporting `=~` by adding a third-party regex engine
//! would cut against that grain, so this module provides the small subset of
//! Java/Neo4j regular-expression syntax that real Cypher queries use.
//!
//! # Semantics
//!
//! Neo4j's `=~` matches the **entire** string (it behaves like
//! `java.util.regex.Matcher::matches`, not `find`), so
//! [`Regex::is_match`](crate::cypher::regex::Regex::is_match) is anchored at
//! both ends regardless of explicit `^` / `$`.
//!
//! # Supported syntax
//!
//! * Literal characters (Unicode).
//! * `.` — any character except a newline.
//! * Quantifiers `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}`, greedy by default,
//!   lazy when suffixed with `?` (e.g. `a+?`). A possessive `+` suffix is
//!   accepted but treated as greedy.
//! * Character classes `[...]`, including ranges (`a-z`), negation (`[^...]`),
//!   and the predefined shortcuts below.
//! * Predefined classes `\d \D \w \W \s \S` (ASCII semantics).
//! * Escapes: `\n \t \r \f \0` and `\<metachar>` for a literal metacharacter.
//! * Anchors `^` and `$` (zero-width; redundant under full-match anchoring).
//! * Alternation `a|b`.
//! * Grouping `(...)` and non-capturing `(?:...)` — capture groups are parsed
//!   but not captured, which is all `=~` (a boolean predicate) needs.
//! * The inline case-insensitive flag `(?i)`. Because scoped flag groups add
//!   significant complexity for little Cypher value, a `(?i)` anywhere in the
//!   pattern turns case-insensitivity on for the **whole** expression.
//!
//! # Robustness
//!
//! The matcher is a backtracking engine. To keep a maliciously crafted
//! pattern from triggering catastrophic backtracking on the query path, every
//! matching step decrements a fixed budget; exhausting it yields
//! [`RegexError::Complexity`](crate::cypher::regex::RegexError::Complexity)
//! rather than hanging.

use std::cell::Cell;

/// Maximum number of matcher steps before [`RegexError::Complexity`] is
/// returned. Generous enough that any realistic property string / pattern
/// completes, small enough that a pathological `(a+)+` style blow-up is
/// bounded to a few hundred milliseconds.
const MATCH_BUDGET: u64 = 2_000_000;

/// Errors produced while compiling or evaluating a [`Regex`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegexError {
    /// The pattern is not valid regular-expression syntax.
    #[error("invalid regular expression: {0}")]
    Syntax(String),
    /// Matching exceeded the backtracking budget — the pattern is too complex
    /// to evaluate against this input.
    #[error("regular expression too complex to evaluate")]
    Complexity,
}

/// A single item inside a character class (or a standalone predefined class).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

impl ClassItem {
    fn matches(&self, c: char, ci: bool) -> bool {
        match self {
            ClassItem::Char(x) => char_eq(*x, c, ci),
            ClassItem::Range(a, b) => {
                let inr = |x: char| x >= *a && x <= *b;
                inr(c) || (ci && (inr(c.to_ascii_lowercase()) || inr(c.to_ascii_uppercase())))
            }
            ClassItem::Digit => c.is_ascii_digit(),
            ClassItem::NotDigit => !c.is_ascii_digit(),
            ClassItem::Word => c.is_ascii_alphanumeric() || c == '_',
            ClassItem::NotWord => !(c.is_ascii_alphanumeric() || c == '_'),
            ClassItem::Space => c.is_ascii_whitespace(),
            ClassItem::NotSpace => !c.is_ascii_whitespace(),
        }
    }
}

/// The parsed regular-expression tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// The empty pattern — matches zero-width.
    Empty,
    /// A single literal character.
    Literal(char),
    /// `.` — any character except `\n`.
    AnyChar,
    /// `^` — start-of-input assertion.
    Start,
    /// `$` — end-of-input assertion.
    End,
    /// A `[...]` character class (or a standalone `\d` style shortcut).
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
    /// A sequence of sub-patterns matched in order.
    Concat(Vec<Node>),
    /// Alternation — the first branch that matches wins.
    Alternate(Vec<Node>),
    /// A quantified sub-pattern: `node` repeated between `min` and `max`
    /// times (`max == None` means unbounded), greedy or lazy.
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
}

fn char_eq(pat: char, c: char, ci: bool) -> bool {
    if ci {
        pat.eq_ignore_ascii_case(&c)
    } else {
        pat == c
    }
}

/// A compiled regular expression, ready to match against any number of
/// inputs.
#[derive(Debug, Clone)]
pub struct Regex {
    root: Node,
    case_insensitive: bool,
}

impl Regex {
    /// Compile `pattern` into a [`Regex`].
    ///
    /// # Errors
    ///
    /// Returns [`RegexError::Syntax`] if the pattern is malformed.
    pub fn compile(pattern: &str) -> Result<Self, RegexError> {
        let mut parser = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            ci: false,
        };
        let root = parser.parse_alternation()?;
        if parser.pos != parser.chars.len() {
            return Err(RegexError::Syntax(format!(
                "unexpected `{}` at position {}",
                parser.chars[parser.pos], parser.pos
            )));
        }
        Ok(Regex {
            root,
            case_insensitive: parser.ci,
        })
    }

    /// Return `true` if the **entire** `input` is matched by this expression,
    /// mirroring Neo4j `=~` (Java `Matcher::matches`) semantics.
    ///
    /// # Errors
    ///
    /// Returns [`RegexError::Complexity`] if matching exhausts the
    /// backtracking budget.
    pub fn is_match(&self, input: &str) -> Result<bool, RegexError> {
        let chars: Vec<char> = input.chars().collect();
        let ctx = MatchCtx {
            chars: &chars,
            ci: self.case_insensitive,
            budget: Cell::new(MATCH_BUDGET),
            overflowed: Cell::new(false),
        };
        let len = chars.len();
        let matched = ctx.m(&self.root, 0, &|p| p == len);
        if ctx.overflowed.get() {
            return Err(RegexError::Complexity);
        }
        Ok(matched)
    }
}

/// Convenience: compile `pattern` and test `input` against it in one call.
///
/// # Errors
///
/// Propagates [`RegexError::Syntax`] from compilation and
/// [`RegexError::Complexity`] from matching.
pub fn full_match(pattern: &str, input: &str) -> Result<bool, RegexError> {
    Regex::compile(pattern)?.is_match(input)
}

// ===== Matching =============================================================

struct MatchCtx<'a> {
    chars: &'a [char],
    ci: bool,
    budget: Cell<u64>,
    overflowed: Cell<bool>,
}

impl MatchCtx<'_> {
    /// Charge one step against the budget. Returns `false` (and latches the
    /// overflow flag) once the budget is exhausted.
    fn tick(&self) -> bool {
        let b = self.budget.get();
        if b == 0 {
            self.overflowed.set(true);
            return false;
        }
        self.budget.set(b - 1);
        true
    }

    /// Try to match `node` starting at character index `pos`; on success call
    /// the continuation `k` with the index past the match. The continuation
    /// lets quantifiers and groups backtrack with full knowledge of what must
    /// follow them.
    fn m(&self, node: &Node, pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
        if !self.tick() {
            return false;
        }
        match node {
            Node::Empty => k(pos),
            Node::Literal(c) => {
                pos < self.chars.len() && char_eq(*c, self.chars[pos], self.ci) && k(pos + 1)
            }
            Node::AnyChar => pos < self.chars.len() && self.chars[pos] != '\n' && k(pos + 1),
            Node::Start => pos == 0 && k(pos),
            Node::End => pos == self.chars.len() && k(pos),
            Node::Class { negated, items } => {
                if pos >= self.chars.len() {
                    return false;
                }
                let c = self.chars[pos];
                let hit = items.iter().any(|it| it.matches(c, self.ci));
                (hit != *negated) && k(pos + 1)
            }
            Node::Concat(nodes) => self.m_concat(nodes, pos, k),
            Node::Alternate(alts) => alts.iter().any(|a| self.m(a, pos, k)),
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => self.m_repeat(node, *min, *max, *greedy, 0, pos, k),
        }
    }

    fn m_concat(&self, nodes: &[Node], pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
        match nodes.split_first() {
            None => k(pos),
            Some((first, rest)) => self.m(first, pos, &|p| self.m_concat(rest, p, k)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn m_repeat(
        &self,
        inner: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        count: usize,
        pos: usize,
        k: &dyn Fn(usize) -> bool,
    ) -> bool {
        let can_more = max.is_none_or(|m| count < m);
        let try_more = || -> bool {
            can_more
                && self.m(inner, pos, &|p| {
                    if p == pos {
                        // Zero-width match — repeating again cannot advance, so
                        // stop here to avoid an infinite loop. Accept only if
                        // this iteration satisfies the lower bound.
                        count + 1 >= min && k(p)
                    } else {
                        self.m_repeat(inner, min, max, greedy, count + 1, p, k)
                    }
                })
        };
        let try_stop = || -> bool { count >= min && k(pos) };
        // Greedy prefers consuming another repetition first; lazy prefers
        // stopping first. `||` short-circuits, so whichever is tried first
        // wins, preserving the quantifier's preference under backtracking.
        let (first, second): (&dyn Fn() -> bool, &dyn Fn() -> bool) = if greedy {
            (&try_more, &try_stop)
        } else {
            (&try_stop, &try_more)
        };
        first() || second()
    }
}

// ===== Parsing ==============================================================

struct Parser {
    chars: Vec<char>,
    pos: usize,
    ci: bool,
}

/// One element of a character class as read from the source — either a
/// predefined shortcut or a plain character (which may begin a range).
enum ClassAtom {
    Shortcut(ClassItem),
    Ch(char),
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn parse_alternation(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            match branches.pop() {
                Some(node) => Ok(node),
                None => Ok(Node::Empty),
            }
        } else {
            Ok(Node::Alternate(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        match items.pop() {
            None => Ok(Node::Empty),
            Some(only) if items.is_empty() => Ok(only),
            Some(last) => {
                items.push(last);
                Ok(Node::Concat(items))
            }
        }
    }

    fn parse_repeat(&mut self) -> Result<Node, RegexError> {
        let atom = self.parse_atom()?;
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(self.finish_quant(atom, 0, None))
            }
            Some('+') => {
                self.pos += 1;
                Ok(self.finish_quant(atom, 1, None))
            }
            Some('?') => {
                self.pos += 1;
                Ok(self.finish_quant(atom, 0, Some(1)))
            }
            Some('{') => {
                if let Some((min, max)) = self.try_parse_brace() {
                    Ok(self.finish_quant(atom, min, max))
                } else {
                    // Not a well-formed quantifier — leave `{` for the next
                    // iteration to consume as a literal atom.
                    Ok(atom)
                }
            }
            _ => Ok(atom),
        }
    }

    /// Consume an optional laziness/possessive suffix and build a `Repeat`.
    fn finish_quant(&mut self, atom: Node, min: usize, max: Option<usize>) -> Node {
        let greedy = match self.peek() {
            // lazy
            Some('?') => {
                self.pos += 1;
                false
            }
            // possessive — accepted, evaluated as greedy
            Some('+') => {
                self.pos += 1;
                true
            }
            _ => true,
        };
        Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        }
    }

    /// Attempt to parse a `{n}` / `{n,}` / `{n,m}` brace quantifier. Returns
    /// `None` (without advancing) if the text after `{` is not a well-formed
    /// quantifier, so the caller can treat `{` as a literal.
    fn try_parse_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let start = self.pos;
        debug_assert_eq!(self.peek(), Some('{'));
        self.pos += 1;
        let min = self.read_number();
        let result = match self.peek() {
            Some('}') => match min {
                Some(n) => {
                    self.pos += 1;
                    Some((n, Some(n)))
                }
                None => None,
            },
            Some(',') => {
                self.pos += 1;
                let max = self.read_number();
                match (min, self.peek()) {
                    (Some(n), Some('}')) => {
                        self.pos += 1;
                        Some((n, max))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if result.is_none() {
            self.pos = start;
        }
        result
    }

    fn read_number(&mut self) -> Option<usize> {
        let start = self.pos;
        let mut val: usize = 0;
        while let Some(c) = self.peek() {
            if let Some(d) = c.to_digit(10) {
                val = val.saturating_mul(10).saturating_add(d as usize);
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(val)
        }
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let c = self
            .peek()
            .ok_or_else(|| RegexError::Syntax("unexpected end of pattern".into()))?;
        match c {
            '(' => self.parse_group(),
            '[' => self.parse_class(),
            '.' => {
                self.pos += 1;
                Ok(Node::AnyChar)
            }
            '^' => {
                self.pos += 1;
                Ok(Node::Start)
            }
            '$' => {
                self.pos += 1;
                Ok(Node::End)
            }
            '\\' => self.parse_escape(),
            '*' | '+' | '?' => Err(RegexError::Syntax(format!(
                "dangling quantifier `{c}` with nothing to repeat"
            ))),
            _ => {
                self.pos += 1;
                Ok(Node::Literal(c))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Node, RegexError> {
        self.pos += 1; // consume '('
        if self.peek() == Some('?') {
            self.pos += 1;
            match self.peek() {
                Some(':') => {
                    self.pos += 1; // non-capturing group
                }
                Some('i') => {
                    self.pos += 1;
                    self.ci = true;
                    match self.peek() {
                        // `(?i)` — a flag toggle; matches zero-width.
                        Some(')') => {
                            self.pos += 1;
                            return Ok(Node::Empty);
                        }
                        // `(?i:...)` — scoped form; the flag is applied
                        // globally (documented simplification), the group is
                        // otherwise non-capturing.
                        Some(':') => {
                            self.pos += 1;
                        }
                        _ => {
                            return Err(RegexError::Syntax("unsupported inline flag group".into()));
                        }
                    }
                }
                _ => {
                    return Err(RegexError::Syntax(
                        "unsupported `(?...)` group construct".into(),
                    ));
                }
            }
        }
        let inner = self.parse_alternation()?;
        if self.peek() != Some(')') {
            return Err(RegexError::Syntax("unclosed group `(`".into()));
        }
        self.pos += 1; // consume ')'
        Ok(inner)
    }

    fn parse_escape(&mut self) -> Result<Node, RegexError> {
        self.pos += 1; // consume '\'
        let c = self
            .peek()
            .ok_or_else(|| RegexError::Syntax("dangling escape `\\` at end of pattern".into()))?;
        self.pos += 1;
        let node = match c {
            'd' => Node::Class {
                negated: false,
                items: vec![ClassItem::Digit],
            },
            'D' => Node::Class {
                negated: false,
                items: vec![ClassItem::NotDigit],
            },
            'w' => Node::Class {
                negated: false,
                items: vec![ClassItem::Word],
            },
            'W' => Node::Class {
                negated: false,
                items: vec![ClassItem::NotWord],
            },
            's' => Node::Class {
                negated: false,
                items: vec![ClassItem::Space],
            },
            'S' => Node::Class {
                negated: false,
                items: vec![ClassItem::NotSpace],
            },
            other => Node::Literal(escape_char(other)),
        };
        Ok(node)
    }

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        self.pos += 1; // consume '['
        let negated = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(RegexError::Syntax("unclosed character class `[`".into())),
                // A `]` immediately after `[` or `[^` is a literal `]`.
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }
            first = false;
            let atom = self.read_class_atom()?;
            match atom {
                ClassAtom::Shortcut(s) => items.push(s),
                ClassAtom::Ch(lo) => {
                    // A `-` that is followed by another class atom (and not the
                    // closing `]`) forms a range.
                    if self.peek() == Some('-')
                        && self.peek_at(1).is_some()
                        && self.peek_at(1) != Some(']')
                    {
                        self.pos += 1; // consume '-'
                        match self.read_class_atom()? {
                            ClassAtom::Ch(hi) => {
                                if lo > hi {
                                    return Err(RegexError::Syntax(format!(
                                        "character class range out of order: {lo}-{hi}"
                                    )));
                                }
                                items.push(ClassItem::Range(lo, hi));
                            }
                            // `[a-\d]` and similar: treat `-` literally.
                            ClassAtom::Shortcut(s) => {
                                items.push(ClassItem::Char(lo));
                                items.push(ClassItem::Char('-'));
                                items.push(s);
                            }
                        }
                    } else {
                        items.push(ClassItem::Char(lo));
                    }
                }
            }
        }
        Ok(Node::Class { negated, items })
    }

    fn read_class_atom(&mut self) -> Result<ClassAtom, RegexError> {
        let c = self
            .peek()
            .ok_or_else(|| RegexError::Syntax("unclosed character class `[`".into()))?;
        if c == '\\' {
            self.pos += 1;
            let e = self
                .peek()
                .ok_or_else(|| RegexError::Syntax("dangling escape in character class".into()))?;
            self.pos += 1;
            Ok(match e {
                'd' => ClassAtom::Shortcut(ClassItem::Digit),
                'D' => ClassAtom::Shortcut(ClassItem::NotDigit),
                'w' => ClassAtom::Shortcut(ClassItem::Word),
                'W' => ClassAtom::Shortcut(ClassItem::NotWord),
                's' => ClassAtom::Shortcut(ClassItem::Space),
                'S' => ClassAtom::Shortcut(ClassItem::NotSpace),
                other => ClassAtom::Ch(escape_char(other)),
            })
        } else {
            self.pos += 1;
            Ok(ClassAtom::Ch(c))
        }
    }
}

/// Map a backslash-escaped character to the character it denotes. Recognised
/// control escapes expand; everything else (including metacharacters) is the
/// literal character itself.
fn escape_char(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'f' => '\u{0C}',
        '0' => '\0',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, input: &str) -> bool {
        full_match(pat, input).unwrap_or_else(|e| panic!("pattern `{pat}` failed: {e}"))
    }

    // ----- literals & full-match anchoring ---------------------------------

    #[test]
    fn literal_requires_full_match() {
        assert!(m("abc", "abc"));
        assert!(!m("abc", "abcd"));
        assert!(!m("abc", "xabc"));
        assert!(!m("abc", "ab"));
    }

    #[test]
    fn empty_pattern_matches_only_empty_string() {
        assert!(m("", ""));
        assert!(!m("", "a"));
    }

    #[test]
    fn unicode_literals_match() {
        assert!(m("café", "café"));
        assert!(m("日本語", "日本語"));
        assert!(m("🎉", "🎉"));
        assert!(!m("café", "cafe"));
    }

    // ----- dot -------------------------------------------------------------

    #[test]
    fn dot_matches_any_char_except_newline() {
        assert!(m("a.c", "abc"));
        assert!(m("a.c", "a c"));
        assert!(m("...", "xyz"));
        assert!(!m("a.c", "a\nc"));
        assert!(!m(".", ""));
    }

    // ----- quantifiers -----------------------------------------------------

    #[test]
    fn star_matches_zero_or_more() {
        assert!(m("ab*", "a"));
        assert!(m("ab*", "ab"));
        assert!(m("ab*", "abbbb"));
        assert!(!m("ab*", "abx"));
    }

    #[test]
    fn plus_matches_one_or_more() {
        assert!(!m("ab+", "a"));
        assert!(m("ab+", "ab"));
        assert!(m("ab+", "abbb"));
    }

    #[test]
    fn question_matches_zero_or_one() {
        assert!(m("ab?c", "ac"));
        assert!(m("ab?c", "abc"));
        assert!(!m("ab?c", "abbc"));
    }

    #[test]
    fn brace_quantifiers() {
        assert!(m("a{3}", "aaa"));
        assert!(!m("a{3}", "aa"));
        assert!(!m("a{3}", "aaaa"));
        assert!(m("a{2,4}", "aa"));
        assert!(m("a{2,4}", "aaaa"));
        assert!(!m("a{2,4}", "a"));
        assert!(!m("a{2,4}", "aaaaa"));
        assert!(m("a{2,}", "aaaaa"));
        assert!(!m("a{2,}", "a"));
    }

    #[test]
    fn malformed_brace_is_literal() {
        assert!(m("a{b", "a{b"));
        assert!(m("a{,3}x", "a{,3}x"));
    }

    #[test]
    fn lazy_quantifier_still_full_matches() {
        // Laziness changes which split wins, but full-match makes both ends
        // anchored so the whole string must still be consumed.
        assert!(m("a+?", "aaa"));
        assert!(m("a.*?b", "axxxb"));
        assert!(!m("a.*?b", "axxx"));
    }

    // ----- character classes ----------------------------------------------

    #[test]
    fn class_membership_and_ranges() {
        assert!(m("[abc]", "b"));
        assert!(!m("[abc]", "d"));
        assert!(m("[a-z]+", "hello"));
        assert!(!m("[a-z]+", "Hello"));
        assert!(m("[0-9a-fA-F]+", "DeadBeef00"));
    }

    #[test]
    fn negated_class() {
        assert!(m("[^0-9]+", "abc"));
        assert!(!m("[^0-9]+", "ab9"));
        assert!(m("a[^b]c", "axc"));
        assert!(!m("a[^b]c", "abc"));
    }

    #[test]
    fn class_special_chars() {
        // A leading `]` is a literal; `-` at the end is literal too.
        assert!(m("[]a]+", "]a]a"));
        assert!(m("[a-]+", "a-a-"));
    }

    #[test]
    fn class_with_shortcuts() {
        assert!(m("[\\d.]+", "3.14"));
        assert!(!m("[\\d.]+", "3,14"));
    }

    // ----- predefined classes ---------------------------------------------

    #[test]
    fn digit_word_space_shortcuts() {
        assert!(m("\\d{4}", "2026"));
        assert!(!m("\\d{4}", "20x6"));
        assert!(m("\\w+", "hello_World99"));
        assert!(!m("\\w+", "no spaces"));
        assert!(m("a\\sb", "a b"));
        assert!(m("a\\sb", "a\tb"));
        assert!(m("\\S+", "nogaps"));
        assert!(m("\\D+", "letters"));
        assert!(m("\\W+", "!@#"));
    }

    // ----- anchors ---------------------------------------------------------

    #[test]
    fn explicit_anchors_are_consistent_with_full_match() {
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "xabc"));
        assert!(m("^$", ""));
    }

    // ----- alternation & groups --------------------------------------------

    #[test]
    fn alternation() {
        assert!(m("cat|dog", "cat"));
        assert!(m("cat|dog", "dog"));
        assert!(!m("cat|dog", "cow"));
    }

    #[test]
    fn groups_and_repetition() {
        assert!(m("(ab)+", "ababab"));
        assert!(!m("(ab)+", "aba"));
        assert!(m("(cat|dog)s?", "cats"));
        assert!(m("(cat|dog)s?", "dog"));
        assert!(m("(?:foo)+bar", "foofoobar"));
    }

    #[test]
    fn nested_groups() {
        assert!(m("((a|b)c)+", "acbcac"));
        assert!(!m("((a|b)c)+", "acb"));
    }

    // ----- escapes ---------------------------------------------------------

    #[test]
    fn escaped_metacharacters_are_literal() {
        assert!(m("a\\.b", "a.b"));
        assert!(!m("a\\.b", "axb"));
        assert!(m("\\(\\)", "()"));
        assert!(m("a\\+", "a+"));
        assert!(m("c:\\\\tmp", "c:\\tmp"));
    }

    #[test]
    fn control_escapes() {
        assert!(m("a\\tb", "a\tb"));
        assert!(m("a\\nb", "a\nb"));
    }

    // ----- case-insensitive flag -------------------------------------------

    #[test]
    fn inline_case_insensitive_flag() {
        assert!(m("(?i)hello", "HELLO"));
        assert!(m("(?i)hello", "Hello"));
        assert!(m("(?i)[a-z]+", "ABC"));
        assert!(!m("hello", "HELLO"));
    }

    #[test]
    fn scoped_case_insensitive_flag_applies_globally() {
        assert!(m("(?i:foo)bar", "FOObar"));
        // documented simplification: (?i:) turns on CI for the whole pattern
        assert!(m("(?i:foo)bar", "FOOBAR"));
    }

    // ----- realistic Cypher predicates -------------------------------------

    #[test]
    fn email_like_pattern() {
        let pat = "[\\w.]+@[\\w.]+";
        assert!(m(pat, "alice@example.com"));
        assert!(!m(pat, "not-an-email"));
    }

    #[test]
    fn case_insensitive_prefix() {
        assert!(m("(?i)alice.*", "ALICE Smith"));
        assert!(!m("(?i)alice.*", "Bob"));
    }

    // ----- errors ----------------------------------------------------------

    #[test]
    fn invalid_patterns_report_syntax_error() {
        assert!(matches!(Regex::compile("(abc"), Err(RegexError::Syntax(_))));
        assert!(matches!(Regex::compile("[a-"), Err(RegexError::Syntax(_))));
        assert!(matches!(Regex::compile("*abc"), Err(RegexError::Syntax(_))));
        assert!(matches!(
            Regex::compile("[z-a]"),
            Err(RegexError::Syntax(_))
        ));
        assert!(matches!(Regex::compile("a\\"), Err(RegexError::Syntax(_))));
    }

    #[test]
    fn catastrophic_backtracking_is_bounded() {
        // The classic exponential blow-up: matching `(a+)+$` against a long
        // run of `a` followed by a non-matching char. The budget must catch
        // it instead of hanging.
        let pat = "(a+)+b";
        let input = "a".repeat(40); // no trailing 'b' → forces full backtrack
        match full_match(pat, &input) {
            Ok(false) => {}                   // engine was fast enough to prove no-match
            Err(RegexError::Complexity) => {} // or bailed out on the budget
            other => panic!("expected bounded result, got {other:?}"),
        }
    }

    #[test]
    fn zero_width_repeat_terminates() {
        // `(a?)*` can match the empty string infinitely many ways; the
        // zero-width guard must terminate.
        assert!(m("(a?)*", "aaa"));
        assert!(m("(a?)*", ""));
        assert!(m("(.*)*", "abc"));
    }
}
