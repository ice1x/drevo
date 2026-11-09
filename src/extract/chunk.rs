//! Splitting a text into overlapping passages for extraction.
//!
//! [`split`] cuts a text into chunks of at most `chunk_chars` characters,
//! preferring a paragraph break, then a sentence end, then a space as the
//! place to cut, and starts each chunk about a tenth of `chunk_chars` before
//! the previous one ended, so a sentence cut in two is still read whole by
//! one of them. Offsets and lengths count characters (Unicode scalar values),
//! not bytes, so they agree with Cypher's `substring`:
//!
//! ```
//! use drevo::extract::chunk::split;
//!
//! let text = "Dana joined Acme. Acme printed the book. Dana left in May.";
//! let chunks = split(text, 30);
//! assert!(chunks.len() > 1);
//! for c in &chunks {
//!     let slice: String = text.chars().skip(c.offset).take(c.length).collect();
//!     assert_eq!(slice, c.text);
//!     assert!(c.length <= 30);
//! }
//! ```

/// One passage of a text, as [`split`] cuts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position in reading order, from 0.
    pub order: usize,
    /// Character offset of the passage in the text.
    pub offset: usize,
    /// Length of the passage in characters.
    pub length: usize,
    /// The passage itself.
    pub text: String,
}

/// Cut `text` into passages of at most `chunk_chars` characters (at least 1)
/// that overlap by about a tenth of `chunk_chars`. An empty text gives no
/// chunks; a text that fits gives one.
#[must_use]
pub fn split(text: &str, chunk_chars: usize) -> Vec<Chunk> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let size = chunk_chars.max(1);
    let overlap = size / 10;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < total {
        let end = if start + size >= total {
            total
        } else {
            cut_point(&chars, start, start + size)
        };
        chunks.push(Chunk {
            order: chunks.len(),
            offset: start,
            length: end - start,
            text: chars[start..end].iter().collect(),
        });
        if end == total {
            break;
        }
        start = next_start(&chars, start, end, overlap);
    }
    chunks
}

/// Where to end a chunk that starts at `start` and may run to `limit`
/// (exclusive, `limit < chars.len()`): after the last paragraph break, else
/// the last sentence end, else the last whitespace in the second half of
/// the window; at `limit` when there is none.
fn cut_point(chars: &[char], start: usize, limit: usize) -> usize {
    let floor = start + (limit - start) / 2;
    let mut sentence = None;
    let mut space = None;
    // A cut at `i` ends the chunk before `chars[i]`.
    for i in (floor.max(start + 1)..=limit).rev() {
        let prev = chars[i - 1];
        if i >= 2 && prev == '\n' && chars[i - 2] == '\n' {
            return i;
        }
        if sentence.is_none()
            && prev.is_whitespace()
            && i >= 2
            && matches!(chars[i - 2], '.' | '!' | '?' | '…' | '。' | '！' | '？')
        {
            sentence = Some(i);
        }
        if space.is_none() && prev.is_whitespace() {
            space = Some(i);
        }
    }
    sentence.or(space).unwrap_or(limit)
}

/// Where the chunk after `[start, end)` starts: `overlap` characters before
/// `end`, moved past the next whitespace so it begins on a word, and always
/// after `start`.
fn next_start(chars: &[char], start: usize, end: usize, overlap: usize) -> usize {
    let mut next = end.saturating_sub(overlap).max(start + 1);
    if let Some(ws) = (next..end).find(|&i| chars[i].is_whitespace()) {
        if ws + 1 < end {
            next = ws + 1;
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(text: &str, c: &Chunk) -> String {
        text.chars().skip(c.offset).take(c.length).collect()
    }

    /// The invariants every split keeps.
    fn check(text: &str, size: usize) -> Vec<Chunk> {
        let chunks = split(text, size);
        let total = text.chars().count();
        if total == 0 {
            assert!(chunks.is_empty());
            return chunks;
        }
        assert_eq!(chunks[0].offset, 0, "the first chunk starts the text");
        let last = &chunks[chunks.len() - 1];
        assert_eq!(last.offset + last.length, total, "the last chunk ends it");
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.order, i);
            assert!(c.length >= 1 && c.length <= size.max(1), "{c:?}");
            assert_eq!(c.length, c.text.chars().count());
            assert_eq!(slice(text, c), c.text, "offsets are exact");
        }
        for w in chunks.windows(2) {
            assert!(w[1].offset > w[0].offset, "chunks move forward");
            assert!(
                w[1].offset <= w[0].offset + w[0].length,
                "no gap between {:?} and {:?}",
                w[0],
                w[1]
            );
        }
        chunks
    }

    #[test]
    fn an_empty_text_has_no_chunks() {
        assert!(split("", 100).is_empty());
    }

    #[test]
    fn a_short_text_is_one_chunk() {
        let chunks = check("Dana joined Acme.", 1500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Dana joined Acme.");
    }

    #[test]
    fn chunks_respect_the_size_and_overlap() {
        let text = "Dana joined Acme in May. ".repeat(40);
        let chunks = check(&text, 200);
        assert!(chunks.len() >= 5, "{}", chunks.len());
        for w in chunks.windows(2) {
            let overlap = w[0].offset + w[0].length - w[1].offset;
            assert!(overlap > 0, "consecutive chunks overlap: {w:?}");
            assert!(overlap <= 20, "the overlap is about a tenth: {overlap}");
        }
    }

    #[test]
    fn a_cut_prefers_a_sentence_end() {
        let text = "First sentence is here. Second sentence follows it. Third one ends.";
        let chunks = check(text, 40);
        assert!(
            chunks[0].text.ends_with(". "),
            "cut after a sentence: {:?}",
            chunks[0].text
        );
    }

    #[test]
    fn a_cut_prefers_a_paragraph_break_over_a_sentence_end() {
        let text = "One. Two three.\n\nFour five six. Seven eight nine ten.";
        let chunks = check(text, 30);
        assert_eq!(chunks[0].text, "One. Two three.\n\n");
    }

    #[test]
    fn a_text_without_spaces_is_cut_hard() {
        let text = "x".repeat(95);
        let chunks = check(&text, 30);
        assert!(chunks.iter().all(|c| c.length <= 30));
    }

    #[test]
    fn cyrillic_offsets_count_characters() {
        let text = "Дана пришла в «Акме». Акме напечатала книгу. Дана ушла в мае. ".repeat(6);
        let chunks = check(&text, 50);
        assert!(chunks.len() > 3);
        // Offsets are character positions, not bytes.
        assert!(chunks[1].offset < text.len() / 2);
    }

    #[test]
    fn emoji_are_never_split() {
        let text = "🚀 Dana 👩‍🚀 joined Acme 🏢. ".repeat(10);
        let chunks = check(&text, 25);
        for c in &chunks {
            // `text` is a valid String; re-slicing by chars rebuilt it exactly.
            assert_eq!(slice(&text, c), c.text);
        }
    }

    #[test]
    fn a_size_of_zero_is_treated_as_one() {
        let chunks = check("abc", 0);
        assert_eq!(chunks.len(), 3);
    }
}
