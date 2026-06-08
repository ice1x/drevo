//! Porter stemmer (M.F. Porter, 1980) — pure-Rust, dependency-free.
//!
//! Keyword extraction ([`crate::fts::keywords`]) uses this *optionally* to
//! collapse morphological variants of a term ("running", "runs", "ran"→no,
//! but "running"/"runs"→"run") onto a common stem before ranking. Collapsing
//! variants both sharpens term-frequency counts and feeds the keyword
//! similarity grouping that builds on this task (`00133`).
//!
//! This is the classic 1980 algorithm (the original suffix list, not the
//! later Snowball/"Porter2" revision), operating on lowercase ASCII words.
//! Non-ASCII or non-alphabetic tokens (CJK, anything with digits) are
//! returned unchanged — the algorithm's vowel/consonant rules are defined
//! only over `a`–`z`. The canonical Porter test vocabulary is exercised in
//! the unit tests.

/// Stem a single lowercase token using the Porter algorithm.
///
/// Returns the input unchanged when it is shorter than three characters or
/// contains any non-`a`–`z` byte (the algorithm is ASCII-alphabetic only).
pub(crate) fn stem(word: &str) -> String {
    // Porter is defined over ASCII letters; bail out for anything else so we
    // never mangle digits, CJK, or accented text.
    if !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    // The algorithm is a no-op (and several rules under-flow) for length <= 2.
    if word.len() <= 2 {
        return word.to_string();
    }

    let mut b: Vec<u8> = word.bytes().collect();
    step1a(&mut b);
    step1b(&mut b);
    step1c(&mut b);
    step2(&mut b);
    step3(&mut b);
    step4(&mut b);
    step5(&mut b);

    // Safe: every byte started as ASCII lowercase and rules only ever delete
    // letters or append `e`/`i`, so the buffer is still valid UTF-8.
    String::from_utf8(b).unwrap_or_else(|_| word.to_string())
}

/// A consonant is a letter other than `a,e,i,o,u`, and other than `y`
/// preceded by a consonant. Defined recursively for `y`.
fn is_consonant(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => {
            if i == 0 {
                true
            } else {
                !is_consonant(b, i - 1)
            }
        }
        _ => true,
    }
}

/// The "measure" `m` of a word: the number of vowel→consonant transitions in
/// the form `[C](VC)^m[V]`.
fn measure(b: &[u8]) -> usize {
    let len = b.len();
    let mut i = 0;
    // Skip leading consonants.
    while i < len && is_consonant(b, i) {
        i += 1;
    }
    let mut n = 0;
    while i < len {
        // Skip the vowel run.
        while i < len && !is_consonant(b, i) {
            i += 1;
        }
        if i >= len {
            break;
        }
        // We are at a consonant after at least one vowel → one VC unit.
        n += 1;
        while i < len && is_consonant(b, i) {
            i += 1;
        }
    }
    n
}

/// `*v*`: the stem contains a vowel.
fn has_vowel(b: &[u8]) -> bool {
    (0..b.len()).any(|i| !is_consonant(b, i))
}

/// `*d`: the stem ends with a double consonant (two identical consonants).
fn double_consonant_end(b: &[u8]) -> bool {
    let len = b.len();
    len >= 2 && b[len - 1] == b[len - 2] && is_consonant(b, len - 1)
}

/// `*o`: the stem ends consonant-vowel-consonant where the final consonant
/// is not `w`, `x`, or `y`.
fn cvc(b: &[u8]) -> bool {
    let len = b.len();
    if len < 3 {
        return false;
    }
    if is_consonant(b, len - 1) && !is_consonant(b, len - 2) && is_consonant(b, len - 3) {
        let c = b[len - 1];
        c != b'w' && c != b'x' && c != b'y'
    } else {
        false
    }
}

fn ends(b: &[u8], suffix: &str) -> bool {
    b.ends_with(suffix.as_bytes())
}

fn step1a(b: &mut Vec<u8>) {
    if ends(b, "sses") {
        b.truncate(b.len() - 2); // sses -> ss
    } else if ends(b, "ies") {
        b.truncate(b.len() - 2); // ies -> i
    } else if ends(b, "ss") {
        // ss -> ss (no change)
    } else if ends(b, "s") {
        b.pop(); // s -> ""
    }
}

fn step1b(b: &mut Vec<u8>) {
    let mut flag = false;
    if ends(b, "eed") {
        // (m>0) EED -> EE : measure the part before "eed".
        if measure(&b[..b.len() - 3]) > 0 {
            b.pop(); // drop the trailing 'd', leaving "ee"
        }
    } else if ends(b, "ed") && has_vowel(&b[..b.len() - 2]) {
        b.truncate(b.len() - 2);
        flag = true;
    } else if ends(b, "ing") && has_vowel(&b[..b.len() - 3]) {
        b.truncate(b.len() - 3);
        flag = true;
    }

    if flag {
        if ends(b, "at") || ends(b, "bl") || ends(b, "iz") {
            b.push(b'e');
        } else if double_consonant_end(b) {
            let last = b[b.len() - 1];
            if last != b'l' && last != b's' && last != b'z' {
                b.pop();
            }
        } else if measure(b) == 1 && cvc(b) {
            b.push(b'e');
        }
    }
}

fn step1c(b: &mut [u8]) {
    // (*v*) Y -> I
    let len = b.len();
    if len > 0 && b[len - 1] == b'y' && has_vowel(&b[..len - 1]) {
        b[len - 1] = b'i';
    }
}

/// Replace `suffix` with `repl` when the word ends in `suffix` and the
/// measure of the part before the suffix is greater than `min_m`. Returns
/// `true` when the suffix matched (whether or not the measure gate fired) so
/// the caller stops trying further suffixes for this step — matching the
/// single-branch structure of Porter's original switch.
fn replace(b: &mut Vec<u8>, suffix: &str, repl: &str, min_m: usize) -> bool {
    if ends(b, suffix) {
        let stem_len = b.len() - suffix.len();
        if measure(&b[..stem_len]) > min_m {
            b.truncate(stem_len);
            b.extend_from_slice(repl.as_bytes());
        }
        true
    } else {
        false
    }
}

fn step2(b: &mut Vec<u8>) {
    // Order matters: longer/more-specific suffixes first (a word ending
    // "ational" also ends "tional"). All gated on m>0.
    const RULES: &[(&str, &str)] = &[
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("abli", "able"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
    ];
    for (suffix, repl) in RULES {
        if replace(b, suffix, repl, 0) {
            break;
        }
    }
}

fn step3(b: &mut Vec<u8>) {
    const RULES: &[(&str, &str)] = &[
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (suffix, repl) in RULES {
        if replace(b, suffix, repl, 0) {
            break;
        }
    }
}

fn step4(b: &mut Vec<u8>) {
    // (m>1) suffix removals. "ion" carries the extra condition that the
    // preceding letter is `s` or `t`, so it is handled specially.
    const RULES: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent",
    ];
    // Longest-first ordering within the shared-tail families (ement/ment/ent)
    // is preserved by the array order above.
    for suffix in RULES {
        if ends(b, suffix) {
            let stem_len = b.len() - suffix.len();
            if measure(&b[..stem_len]) > 1 {
                b.truncate(stem_len);
            }
            return;
        }
    }
    // (m>1 and (*S or *T)) ION ->
    if ends(b, "ion") {
        let stem_len = b.len() - 3;
        if stem_len > 0 {
            let prev = b[stem_len - 1];
            if (prev == b's' || prev == b't') && measure(&b[..stem_len]) > 1 {
                b.truncate(stem_len);
            }
        }
        return;
    }
    const RULES2: &[&str] = &["ou", "ism", "ate", "iti", "ous", "ive", "ize"];
    for suffix in RULES2 {
        if ends(b, suffix) {
            let stem_len = b.len() - suffix.len();
            if measure(&b[..stem_len]) > 1 {
                b.truncate(stem_len);
            }
            return;
        }
    }
}

fn step5(b: &mut Vec<u8>) {
    // Step 5a: (m>1) E -> ; (m=1 and not *o) E ->
    if b.last() == Some(&b'e') {
        let stem = &b[..b.len() - 1];
        let m = measure(stem);
        if m > 1 || (m == 1 && !cvc(stem)) {
            b.pop();
        }
    }
    // Step 5b: (m>1 and *d and *L) -> single letter
    if measure(b) > 1 && double_consonant_end(b) && b.last() == Some(&b'l') {
        b.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(pairs: &[(&str, &str)]) {
        for (input, expected) in pairs {
            assert_eq!(
                stem(input),
                *expected,
                "stem({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn step1a_plurals() {
        check(&[
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("caress", "caress"),
            ("cats", "cat"),
        ]);
    }

    #[test]
    fn step1b_past_and_gerund() {
        check(&[
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("bled", "bled"),
            ("motoring", "motor"),
            ("sing", "sing"),
            ("conflated", "conflat"),
            ("troubled", "troubl"),
            ("sized", "size"),
            ("hopping", "hop"),
            ("falling", "fall"),
            ("filing", "file"),
        ]);
    }

    #[test]
    fn step1c_terminal_y() {
        check(&[("happy", "happi"), ("sky", "sky")]);
    }

    #[test]
    fn step2_double_suffixes() {
        // Expected values are FULL-algorithm outputs (the paper's step-2
        // examples are intermediate; later steps reduce them further —
        // e.g. step-2 "relate" then loses its final `e` in step 5).
        check(&[
            ("relational", "relat"),
            ("conditional", "condit"),
            ("rational", "ration"),
            ("predication", "predic"),
            ("operator", "oper"),
            ("feudalism", "feudal"),
            ("decisiveness", "decis"),
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            ("formaliti", "formal"),
            ("sensitiviti", "sensit"),
            ("sensibiliti", "sensibl"),
        ]);
    }

    #[test]
    fn step3_more_suffixes() {
        check(&[
            ("triplicate", "triplic"),
            ("formative", "form"),
            ("formalize", "formal"),
            ("electriciti", "electr"),
            ("electrical", "electr"),
            ("hopeful", "hope"),
            ("goodness", "good"),
        ]);
    }

    #[test]
    fn step4_suffix_stripping() {
        check(&[
            ("revival", "reviv"),
            ("allowance", "allow"),
            ("inference", "infer"),
            ("airliner", "airlin"),
            ("gyroscopic", "gyroscop"),
            ("adjustable", "adjust"),
            ("defensible", "defens"),
            ("irritant", "irrit"),
            ("replacement", "replac"),
            ("adjustment", "adjust"),
            ("dependent", "depend"),
            ("adoption", "adopt"),
            ("homologou", "homolog"),
            ("communism", "commun"),
            ("activate", "activ"),
            ("angulariti", "angular"),
            ("homologous", "homolog"),
            ("effective", "effect"),
            ("bowdlerize", "bowdler"),
        ]);
    }

    #[test]
    fn step5_final_e_and_double_l() {
        check(&[
            ("probate", "probat"),
            ("rate", "rate"),
            ("cease", "ceas"),
            ("controll", "control"),
            ("roll", "roll"),
        ]);
    }

    #[test]
    fn short_and_non_ascii_unchanged() {
        check(&[
            ("a", "a"),
            ("at", "at"),
            ("go", "go"),
            ("世界", "世界"),
            ("test123", "test123"),
            ("straße", "straße"),
        ]);
    }

    #[test]
    fn morphological_variants_collapse() {
        // The practical payoff for keyword grouping: variants share a stem.
        assert_eq!(stem("running"), stem("runs"));
        assert_eq!(stem("connection"), stem("connections"));
        assert_eq!(stem("argue"), stem("argued"));
    }
}
