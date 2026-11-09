//! Checking where a model says a passage is.
//!
//! A model asked to cite its source returns the words it read (`surface` or
//! `quote`) and their character offset. [`verify_or_repair`] accepts the
//! pair when the words stand at that offset, moves it to the nearest place
//! they do stand when the offset is wrong, tolerates a surface that differs
//! from the text only in whitespace, and rejects words the text does not
//! contain:
//!
//! ```
//! use drevo::extract::offsets::verify_or_repair;
//!
//! let text = "Dana joined Acme in May.";
//! assert_eq!(verify_or_repair(text, "Acme", Some(12)).unwrap().offset, 12);
//! assert_eq!(verify_or_repair(text, "Acme", Some(3)).unwrap().offset, 12);
//! assert!(verify_or_repair(text, "Globex", Some(12)).is_none());
//! ```

/// Where a surface was found: its character offset and length in the text,
/// and the text's own words there (which differ from what the model sent
/// only when whitespace was repaired).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// Character offset in the text.
    pub offset: usize,
    /// Length in characters.
    pub length: usize,
    /// The characters of the text at that place.
    pub surface: String,
}

/// Locate `surface` (trimmed) in `text`, preferring `claimed` (a character
/// offset): the surface there, else its occurrence nearest to `claimed`
/// (the first one without a claim), else a place where the text holds its
/// words separated by other whitespace.
/// `None` when the text does not contain it or it is blank.
#[must_use]
pub fn verify_or_repair(text: &str, surface: &str, claimed: Option<usize>) -> Option<Located> {
    let surface = surface.trim();
    if surface.is_empty() {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let wanted: Vec<char> = surface.chars().collect();
    if let Some(at) = claimed {
        if chars.get(at..at + wanted.len()) == Some(wanted.as_slice()) {
            return Some(located(&chars, at, wanted.len()));
        }
    }
    let found: Vec<usize> = (0..=chars.len().saturating_sub(wanted.len()))
        .filter(|&i| chars.get(i..i + wanted.len()) == Some(wanted.as_slice()))
        .collect();
    if let Some(at) = nearest(&found, claimed) {
        return Some(located(&chars, at, wanted.len()));
    }
    let words: Vec<Vec<char>> = surface
        .split_whitespace()
        .map(|w| w.chars().collect())
        .collect();
    let matches: Vec<(usize, usize)> = (0..chars.len())
        .filter_map(|i| match_words(&chars, i, &words).map(|end| (i, end)))
        .collect();
    let starts: Vec<usize> = matches.iter().map(|(s, _)| *s).collect();
    let at = nearest(&starts, claimed)?;
    let end = matches.iter().find(|(s, _)| *s == at).map(|(_, e)| *e)?;
    Some(located(&chars, at, end - at))
}

fn located(chars: &[char], offset: usize, length: usize) -> Located {
    Located {
        offset,
        length,
        surface: chars[offset..offset + length].iter().collect(),
    }
}

/// The candidate closest to `claimed` (the first on a tie or without one).
fn nearest(candidates: &[usize], claimed: Option<usize>) -> Option<usize> {
    let claimed = claimed.unwrap_or(0);
    candidates
        .iter()
        .copied()
        .min_by_key(|&c| c.abs_diff(claimed))
}

/// The end of `words` matched at `start` with any run of whitespace between
/// them, or `None`.
fn match_words(chars: &[char], start: usize, words: &[Vec<char>]) -> Option<usize> {
    let mut at = start;
    for (n, word) in words.iter().enumerate() {
        if n > 0 {
            let gap = chars[at..].iter().take_while(|c| c.is_whitespace()).count();
            if gap == 0 {
                return None;
            }
            at += gap;
        }
        if chars.get(at..at + word.len()) != Some(word.as_slice()) {
            return None;
        }
        at += word.len();
    }
    (!words.is_empty()).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, l: &Located) -> String {
        text.chars().skip(l.offset).take(l.length).collect()
    }

    #[test]
    fn an_exact_offset_is_accepted() {
        let text = "Dana joined Acme in May.";
        let l = verify_or_repair(text, "Dana", Some(0)).unwrap();
        assert_eq!((l.offset, l.length, l.surface.as_str()), (0, 4, "Dana"));
    }

    #[test]
    fn a_wrong_offset_moves_to_the_nearest_occurrence() {
        let text = "Acme hired Dana. Later Acme fired Dana.";
        let l = verify_or_repair(text, "Acme", Some(20)).unwrap();
        assert_eq!(l.offset, 23);
        let l = verify_or_repair(text, "Acme", Some(2)).unwrap();
        assert_eq!(l.offset, 0);
    }

    #[test]
    fn a_missing_offset_takes_the_first_occurrence() {
        let l = verify_or_repair("a Dana b Dana", "Dana", None).unwrap();
        assert_eq!(l.offset, 2);
    }

    #[test]
    fn surrounding_whitespace_is_repaired() {
        let text = "Dana joined Acme in May.";
        let l = verify_or_repair(text, " Acme ", Some(11)).unwrap();
        assert_eq!((l.offset, l.length, l.surface.as_str()), (12, 4, "Acme"));
    }

    #[test]
    fn inner_whitespace_differences_are_repaired() {
        let text = "Dana joined\n  Acme Corp in May.";
        let l = verify_or_repair(text, "joined Acme Corp", Some(5)).unwrap();
        assert_eq!(l.offset, 5);
        assert_eq!(l.surface, "joined\n  Acme Corp");
        assert_eq!(at(text, &l), l.surface);
    }

    #[test]
    fn an_unlocatable_surface_is_dropped() {
        assert!(verify_or_repair("Dana joined Acme.", "Globex", Some(0)).is_none());
        assert!(verify_or_repair("Dana joined Acme.", "dana", Some(0)).is_none());
        assert!(verify_or_repair("Dana joined Acme.", "   ", Some(0)).is_none());
        assert!(verify_or_repair("Dana joined Acme.", "", None).is_none());
    }

    #[test]
    fn a_claim_past_the_end_still_finds_the_surface() {
        let l = verify_or_repair("Dana joined Acme.", "Acme", Some(999)).unwrap();
        assert_eq!(l.offset, 12);
    }

    #[test]
    fn cyrillic_and_emoji_offsets_count_characters() {
        let text = "🚀 Дана пришла в «Акме».";
        let l = verify_or_repair(text, "Акме", Some(17)).unwrap();
        assert_eq!(l.offset, 17);
        assert_eq!(at(text, &l), "Акме");
        let l = verify_or_repair(text, "Дана", Some(0)).unwrap();
        assert_eq!((l.offset, l.length), (2, 4));
    }
}
