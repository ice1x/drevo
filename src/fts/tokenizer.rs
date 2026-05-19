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
