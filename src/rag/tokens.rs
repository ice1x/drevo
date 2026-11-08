//! Token estimates for a context's budget: no tokenizer table, just a rule
//! of thumb that is cheap and stable.
//!
//! ```
//! use drevo::rag::tokens::Tokenizer;
//!
//! // `approx`: one token per four characters, rounded up.
//! assert_eq!(Tokenizer::Approx.count("abcdefgh"), 2);
//! // `words`: whitespace-separated words × 1.3, rounded up.
//! assert_eq!(Tokenizer::Words.count("one two three"), 4);
//! ```

/// How a context's size in tokens is estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tokenizer {
    /// `⌈characters / 4⌉`, counting Unicode characters — an estimate close to
    /// common BPE tokenizers on English prose.
    Approx,
    /// `⌈whitespace-separated words × 1.3⌉`.
    Words,
}

impl Tokenizer {
    /// The estimated token count of `text`.
    ///
    /// ```
    /// use drevo::rag::tokens::Tokenizer;
    /// assert_eq!(Tokenizer::Approx.count(""), 0);
    /// ```
    #[must_use]
    pub fn count(self, text: &str) -> usize {
        Measure::of(text).tokens(self)
    }

    /// The tokenizer's name as written in a config map.
    ///
    /// ```
    /// use drevo::rag::tokens::Tokenizer;
    /// assert_eq!(Tokenizer::Words.as_str(), "words");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approx => "approx",
            Self::Words => "words",
        }
    }

    /// The tokenizer named `name`, if any.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "approx" => Some(Self::Approx),
            "words" => Some(Self::Words),
            _ => None,
        }
    }
}

/// `⌈characters / 4⌉` of `text`.
///
/// ```
/// assert_eq!(drevo::rag::tokens::approx(&"abcd".repeat(10)), 10);
/// ```
#[must_use]
pub fn approx(text: &str) -> usize {
    Tokenizer::Approx.count(text)
}

/// `⌈whitespace-separated words × 1.3⌉` of `text`.
///
/// ```
/// assert_eq!(drevo::rag::tokens::words("a b c d e f g h i j"), 13);
/// ```
#[must_use]
pub fn words(text: &str) -> usize {
    Tokenizer::Words.count(text)
}

/// What both estimates need to know about a piece of text, in a form that
/// adds up: the measure of `a + b` follows from the measures of `a` and `b`,
/// so a growing context is measured without re-reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Measure {
    /// Unicode characters.
    chars: usize,
    /// Whitespace-separated words.
    words: usize,
    /// Whether the text starts inside a word (with a non-whitespace char).
    starts_in_word: bool,
    /// Whether the text ends inside a word.
    ends_in_word: bool,
}

impl Measure {
    /// The measure of `text`.
    pub(crate) fn of(text: &str) -> Self {
        let mut m = Self::default();
        let mut in_word = false;
        for (i, c) in text.chars().enumerate() {
            let word_char = !c.is_whitespace();
            if i == 0 {
                m.starts_in_word = word_char;
            }
            if word_char && !in_word {
                m.words += 1;
            }
            in_word = word_char;
            m.chars += 1;
        }
        m.ends_in_word = in_word;
        m
    }

    /// The measure of this text followed by `next`'s.
    #[must_use]
    pub(crate) fn then(self, next: Self) -> Self {
        if self.chars == 0 {
            return next;
        }
        if next.chars == 0 {
            return self;
        }
        let joined = usize::from(self.ends_in_word && next.starts_in_word);
        Self {
            chars: self.chars + next.chars,
            words: self.words + next.words - joined,
            starts_in_word: self.starts_in_word,
            ends_in_word: next.ends_in_word,
        }
    }

    /// The estimated token count under `tokenizer`.
    pub(crate) fn tokens(self, tokenizer: Tokenizer) -> usize {
        match tokenizer {
            Tokenizer::Approx => self.chars.div_ceil(4),
            Tokenizer::Words => (self.words * 13).div_ceil(10),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_is_characters_over_four_rounded_up() {
        assert_eq!(approx(&"abcd".repeat(10)), 10);
        assert_eq!(approx(""), 0);
        assert_eq!(approx("a"), 1);
        assert_eq!(approx("abcde"), 2);
        // Characters, not bytes: six Cyrillic letters are twelve bytes.
        assert_eq!(approx("привет"), 2);
    }

    #[test]
    fn words_is_whitespace_words_times_1_3_rounded_up() {
        assert_eq!(words(""), 0);
        assert_eq!(words("   \n\t "), 0);
        assert_eq!(words("one"), 2);
        assert_eq!(words("a b  c\nd"), 6);
        assert_eq!(words("a b c d e f g h i j"), 13);
        assert_eq!(words("Сегодня было тяжело"), 4);
    }

    #[test]
    fn the_names_round_trip() {
        for t in [Tokenizer::Approx, Tokenizer::Words] {
            assert_eq!(Tokenizer::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tokenizer::parse("tiktoken"), None);
    }

    #[test]
    fn measures_add_up_like_the_joined_text() {
        let pieces = [
            "",
            " ",
            "word",
            " lead",
            "trail ",
            "two words",
            "\n",
            "Пятница",
            "- **x** (Y): z\n",
            "},{\"id\":1",
        ];
        for a in pieces {
            for b in pieces {
                let joined = format!("{a}{b}");
                assert_eq!(
                    Measure::of(a).then(Measure::of(b)),
                    Measure::of(&joined),
                    "{a:?} + {b:?}"
                );
                for t in [Tokenizer::Approx, Tokenizer::Words] {
                    assert_eq!(
                        Measure::of(a).then(Measure::of(b)).tokens(t),
                        t.count(&joined),
                        "{a:?} + {b:?} under {t:?}"
                    );
                }
            }
        }
    }
}
