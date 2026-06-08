/// Returns `true` if the character is in a CJK Unified Ideographs range.
const fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{20000}'..='\u{2A6DF}' // CJK Unified Ideographs Extension B
        | '\u{2A700}'..='\u{2B73F}' // Extension C
        | '\u{2B740}'..='\u{2B81F}' // Extension D
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

/// Returns `true` if the character should be kept during normalization
/// (alphanumeric or CJK).
fn is_keepable(c: char) -> bool {
    c.is_alphanumeric() || is_cjk(c)
}

/// Normalize text: lowercase, strip punctuation (replace with space),
/// collapse whitespace, trim.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut result = String::with_capacity(lowered.len());
    let mut prev_space = true; // skip leading spaces

    for c in lowered.chars() {
        if is_keepable(c) {
            result.push(c);
            prev_space = false;
        } else if !prev_space {
            result.push(' ');
            prev_space = true;
        }
    }

    // trim trailing space
    if result.ends_with(' ') {
        result.pop();
    }

    result
}

/// Extract trigrams from text. Normalizes first, then produces
/// a deduplicated, sorted list of 3-character sliding windows.
///
/// For CJK characters, bigrams (2-char windows) are also produced
/// since CJK characters are semantically dense.
pub fn trigrams(text: &str) -> Vec<String> {
    let normalized = normalize(text);
    let chars: Vec<char> = normalized.chars().collect();

    if chars.len() < 2 {
        return Vec::new();
    }

    let mut set = std::collections::BTreeSet::new();

    // Standard trigrams (3-char sliding window)
    if chars.len() >= 3 {
        for window in chars.windows(3) {
            set.insert(window.iter().collect::<String>());
        }
    }

    // CJK bigrams: for consecutive CJK characters, add 2-char windows
    for window in chars.windows(2) {
        if is_cjk(window[0]) && is_cjk(window[1]) {
            set.insert(window.iter().collect::<String>());
        }
    }

    set.into_iter().collect()
}

/// Extract trigrams from title and body fields combined.
/// The fields are joined with a space separator before tokenization.
pub fn extract_trigrams(title: &str, body: &str) -> Vec<String> {
    let combined = if body.is_empty() {
        title.to_string()
    } else if title.is_empty() {
        body.to_string()
    } else {
        format!("{} {}", title, body)
    };

    trigrams(&combined)
}

/// Extract **raw** trigrams from text — like [`trigrams`] but **without
/// deduplication**, preserving every sliding-window occurrence in document
/// order.
///
/// Where [`trigrams`] returns the *set* of distinct trigrams (which the
/// inverted index and document-frequency counts need), `raw_trigrams`
/// returns the *bag* of trigrams. This is what term-frequency-aware
/// ranking (BM25, task `00131`) consumes: the number of times a trigram
/// occurs in a document drives the `k1` saturation term, and the total
/// number of trigram tokens is the document length `|d|` used for the `b`
/// length-normalization term.
///
/// CJK bigrams are emitted with the same rule as [`trigrams`].
pub fn raw_trigrams(text: &str) -> Vec<String> {
    let normalized = normalize(text);
    let chars: Vec<char> = normalized.chars().collect();

    if chars.len() < 2 {
        return Vec::new();
    }

    let mut out = Vec::new();

    // Standard trigrams (3-char sliding window), with repetition.
    if chars.len() >= 3 {
        for window in chars.windows(3) {
            out.push(window.iter().collect::<String>());
        }
    }

    // CJK bigrams: for consecutive CJK characters, add 2-char windows.
    for window in chars.windows(2) {
        if is_cjk(window[0]) && is_cjk(window[1]) {
            out.push(window.iter().collect::<String>());
        }
    }

    out
}

/// Extract raw (non-deduplicated) trigrams from the title and body fields
/// combined, mirroring [`extract_trigrams`] but keeping every occurrence.
///
/// Used by BM25 ranking (task `00131`) to compute per-document term
/// frequencies and document length `|d|`.
pub fn extract_raw_trigrams(title: &str, body: &str) -> Vec<String> {
    let combined = if body.is_empty() {
        title.to_string()
    } else if title.is_empty() {
        body.to_string()
    } else {
        format!("{} {}", title, body)
    };

    raw_trigrams(&combined)
}

/// Split text into lowercase **word** tokens, preserving document order and
/// repetition.
///
/// This is the *word-level* tokenizer used by keyword extraction (task
/// `00132`), deliberately distinct from the character-trigram tokenizer that
/// powers the FTS inverted index. A token is a maximal run of alphanumeric or
/// CJK characters; everything else (whitespace, punctuation) is a separator.
/// Tokens shorter than two characters are dropped — single letters and stray
/// digits carry no keyword signal and only add noise to the ranking.
///
/// Unlike [`trigrams`], occurrences are **not** deduplicated: the caller needs
/// per-word term frequencies, so "graph graph" yields `["graph", "graph"]`.
pub fn words(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    let mut out = Vec::new();
    let mut current = String::new();
    for c in lowered.chars() {
        if is_keepable(c) {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.retain(|w| w.chars().count() >= 2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize tests ---

    #[test]
    fn normalize_lowercase() {
        assert_eq!(normalize("Hello World"), "hello world");
    }

    #[test]
    fn normalize_strip_punctuation() {
        assert_eq!(normalize("hello, world!"), "hello world");
    }

    #[test]
    fn normalize_collapse_whitespace() {
        assert_eq!(normalize("hello   world"), "hello world");
    }

    #[test]
    fn normalize_unicode_lowercase() {
        assert_eq!(normalize("Straße"), "straße");
    }

    #[test]
    fn normalize_trim() {
        assert_eq!(normalize("  hello  "), "hello");
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_only_punctuation() {
        assert_eq!(normalize("!@#$%^&*()"), "");
    }

    #[test]
    fn normalize_mixed_cjk_latin() {
        let result = normalize("Hello 世界!");
        assert_eq!(result, "hello 世界");
    }

    #[test]
    fn normalize_preserves_digits() {
        assert_eq!(normalize("test123"), "test123");
    }

    // --- trigrams tests ---

    #[test]
    fn trigrams_basic() {
        let result = trigrams("hello");
        assert_eq!(result, vec!["ell", "hel", "llo"]); // sorted
    }

    #[test]
    fn trigrams_short_text_single_char() {
        assert!(trigrams("h").is_empty());
    }

    #[test]
    fn trigrams_short_text_two_chars() {
        // "hi" — too short for trigrams, no CJK bigrams
        assert!(trigrams("hi").is_empty());
    }

    #[test]
    fn trigrams_exact_three() {
        assert_eq!(trigrams("abc"), vec!["abc"]);
    }

    #[test]
    fn trigrams_with_space() {
        let result = trigrams("ab cd");
        assert!(result.contains(&" cd".to_string()));
        assert!(result.contains(&"ab ".to_string()));
        assert!(result.contains(&"b c".to_string()));
    }

    #[test]
    fn trigrams_dedup() {
        let result = trigrams("aaaa");
        assert_eq!(result, vec!["aaa"]);
    }

    #[test]
    fn trigrams_empty() {
        assert!(trigrams("").is_empty());
    }

    #[test]
    fn trigrams_cjk_bigrams() {
        // "世界你好" -> CJK bigrams: "世界", "界你", "你好"
        // also trigrams: "世界你", "界你好"
        let result = trigrams("世界你好");
        assert!(result.contains(&"世界".to_string()));
        assert!(result.contains(&"界你".to_string()));
        assert!(result.contains(&"你好".to_string()));
        assert!(result.contains(&"世界你".to_string()));
        assert!(result.contains(&"界你好".to_string()));
    }

    #[test]
    fn trigrams_cjk_two_chars() {
        // "你好" -> one CJK bigram, no trigrams (only 2 chars)
        let result = trigrams("你好");
        assert_eq!(result, vec!["你好"]);
    }

    #[test]
    fn trigrams_normalizes_input() {
        let result = trigrams("HELLO");
        assert_eq!(result, vec!["ell", "hel", "llo"]);
    }

    // --- extract_trigrams tests ---

    #[test]
    fn extract_trigrams_combines_fields() {
        let result = extract_trigrams("abc", "def");
        // "abc def" -> trigrams include parts from both title and body
        assert!(result.contains(&"abc".to_string()));
        assert!(result.contains(&"def".to_string()));
    }

    #[test]
    fn extract_trigrams_empty_body() {
        let result = extract_trigrams("hello", "");
        assert_eq!(result, trigrams("hello"));
    }

    #[test]
    fn extract_trigrams_empty_title() {
        let result = extract_trigrams("", "hello");
        assert_eq!(result, trigrams("hello"));
    }

    #[test]
    fn extract_trigrams_both_empty() {
        assert!(extract_trigrams("", "").is_empty());
    }

    // --- raw_trigrams tests ---

    #[test]
    fn raw_trigrams_preserves_repetition() {
        // "aaaa" -> windows: "aaa", "aaa" (two occurrences, not deduped)
        let result = raw_trigrams("aaaa");
        assert_eq!(result, vec!["aaa", "aaa"]);
    }

    #[test]
    fn raw_trigrams_basic_order() {
        // Document order, not sorted (contrast with `trigrams`).
        assert_eq!(raw_trigrams("hello"), vec!["hel", "ell", "llo"]);
    }

    #[test]
    fn raw_trigrams_count_matches_window_count() {
        // "abcabc" normalized has 6 chars -> 4 trigram windows.
        assert_eq!(raw_trigrams("abcabc").len(), 4);
    }

    #[test]
    fn raw_trigrams_term_frequency() {
        // "rust rust" contains the trigram "rus" twice.
        let raw = raw_trigrams("rust rust");
        let rus = raw.iter().filter(|t| *t == "rus").count();
        assert_eq!(rus, 2, "tf of 'rus' must reflect repetition");
    }

    #[test]
    fn raw_trigrams_too_short() {
        assert!(raw_trigrams("hi").is_empty());
        assert!(raw_trigrams("").is_empty());
    }

    #[test]
    fn raw_trigrams_cjk_bigrams_with_repetition() {
        // "你好你好" -> bigrams "你好","好你","你好" + trigrams "你好你","好你好"
        let raw = raw_trigrams("你好你好");
        let nihao = raw.iter().filter(|t| *t == "你好").count();
        assert_eq!(nihao, 2);
    }

    #[test]
    fn raw_trigrams_superset_of_distinct() {
        // The distinct set is exactly the dedup of the raw bag.
        let raw = raw_trigrams("programming programming");
        let mut distinct: Vec<String> = raw.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct, trigrams("programming programming"));
        assert!(raw.len() > distinct.len());
    }

    #[test]
    fn extract_raw_trigrams_combines_fields() {
        let result = extract_raw_trigrams("abc", "abc");
        // "abc abc" keeps both "abc" windows.
        let abc = result.iter().filter(|t| *t == "abc").count();
        assert_eq!(abc, 2);
    }

    #[test]
    fn extract_raw_trigrams_empty_body() {
        assert_eq!(extract_raw_trigrams("hello", ""), raw_trigrams("hello"));
    }

    // --- words tests ---

    #[test]
    fn words_basic_split() {
        assert_eq!(words("hello world"), vec!["hello", "world"]);
    }

    #[test]
    fn words_lowercases() {
        assert_eq!(words("Graph DATABASE"), vec!["graph", "database"]);
    }

    #[test]
    fn words_strips_punctuation() {
        assert_eq!(
            words("anxiety, depression; cbt!"),
            vec!["anxiety", "depression", "cbt"]
        );
    }

    #[test]
    fn words_preserves_repetition_and_order() {
        // Contrast with `trigrams`, which dedups and sorts.
        assert_eq!(words("graph graph node"), vec!["graph", "graph", "node"]);
    }

    #[test]
    fn words_drops_single_chars() {
        // "a" and "I" carry no keyword signal.
        assert_eq!(words("a graph i node"), vec!["graph", "node"]);
    }

    #[test]
    fn words_keeps_digits_in_tokens() {
        assert_eq!(words("neo4j v2 release"), vec!["neo4j", "v2", "release"]);
    }

    #[test]
    fn words_empty_and_punctuation_only() {
        assert!(words("").is_empty());
        assert!(words("!@#$ %^&*").is_empty());
    }

    #[test]
    fn words_cjk_runs() {
        // CJK runs are kept as tokens (English-first; CJK word segmentation
        // is a follow-up). A 2+ char run survives the length filter.
        assert_eq!(words("hello 世界"), vec!["hello", "世界"]);
    }

    // --- is_cjk tests ---

    #[test]
    fn is_cjk_chinese() {
        assert!(is_cjk('中'));
        assert!(is_cjk('世'));
    }

    #[test]
    fn is_cjk_hiragana() {
        assert!(is_cjk('あ'));
    }

    #[test]
    fn is_cjk_katakana() {
        assert!(is_cjk('ア'));
    }

    #[test]
    fn is_cjk_hangul() {
        assert!(is_cjk('한'));
    }

    #[test]
    fn is_cjk_latin_false() {
        assert!(!is_cjk('a'));
        assert!(!is_cjk('Z'));
        assert!(!is_cjk('5'));
    }
}
