//! The form names are compared in: case-folded, without diacritics or
//! punctuation, words separated by single spaces. Initials written with dots
//! become one-letter words, so «А. С. Пушкин» and «А.С.Пушкин» both read
//! «а с пушкин».

/// `name` in compared form: lower case, Latin letters without accents
/// (`é` → `e`, `ß` → `ss`), `ё` → `е`, every run of characters that are not
/// letters or digits one space, no leading or trailing space. Normalising a
/// normalised name changes nothing.
pub(crate) fn normalise(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut gap = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if is_combining_mark(c) {
            continue;
        }
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            match unaccented(c) {
                Some(plain) => out.push_str(plain),
                None => out.push(c),
            }
        } else {
            gap = true;
        }
    }
    out
}

/// A combining diacritical mark (U+0300–U+036F), as left by a decomposed
/// accent.
fn is_combining_mark(c: char) -> bool {
    ('\u{0300}'..='\u{036f}').contains(&c)
}

/// The plain letters of an accented lower-case Latin letter (Latin-1 and
/// Latin Extended-A), and `ё` → `е`; `None` for any other character.
fn unaccented(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ð' | 'ď' | 'đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĳ' => "ij",
        'ĵ' => "j",
        'ķ' | 'ĸ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ſ' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ё' => "е",
        _ => return None,
    })
}

/// The words of a compared name.
pub(crate) fn words(form: &str) -> impl Iterator<Item = &str> {
    form.split_whitespace()
}

/// The initials of a compared name (its one-character words) and its other
/// words, each in order.
pub(crate) fn split_initials(form: &str) -> (Vec<&str>, Vec<&str>) {
    words(form).partition(|w| w.chars().nth(1).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn cyrillic_initials_with_dots_become_words() {
        assert_eq!(normalise("А. С. Пушкин"), "а с пушкин");
        assert_eq!(normalise("А.С.Пушкин"), "а с пушкин");
        assert_eq!(normalise("Пушкин А.С."), "пушкин а с");
    }

    #[test]
    fn case_is_folded() {
        assert_eq!(normalise("ПУШКИН"), "пушкин");
        assert_eq!(normalise("Pushkin"), "pushkin");
        assert_eq!(normalise("İstanbul"), "istanbul");
    }

    #[test]
    fn diacritics_are_stripped() {
        assert_eq!(normalise("Émile Zola"), "emile zola");
        assert_eq!(normalise("Łódź"), "lodz");
        assert_eq!(normalise("Straße"), "strasse");
        assert_eq!(normalise("Ёлкин"), "елкин");
        assert_eq!(normalise("Dvořák"), "dvorak");
        // A decomposed accent (e + U+0301) goes too.
        assert_eq!(normalise("Cafe\u{301}"), "cafe");
    }

    #[test]
    fn punctuation_and_spaces_collapse() {
        assert_eq!(normalise("  Pushkin,  Alexander!! "), "pushkin alexander");
        assert_eq!(normalise("O'Brien"), "o brien");
        assert_eq!(normalise("R2-D2"), "r2 d2");
        assert_eq!(normalise("«Пушкин»"), "пушкин");
        assert_eq!(normalise(""), "");
        assert_eq!(normalise(" ... "), "");
    }

    #[test]
    fn normalising_twice_changes_nothing() {
        for name in [
            "А. С. Пушкин",
            "Émile Zola",
            "İstanbul",
            "Straße",
            "Ǆemal",
            "ﬁne",
            "Cafe\u{301}",
        ] {
            let once = normalise(name);
            assert_eq!(normalise(&once), once, "{name}");
        }
    }

    proptest! {
        #[test]
        fn normalise_is_idempotent(chars in prop::collection::vec(any::<char>(), 0..32)) {
            let name: String = chars.into_iter().collect();
            let once = normalise(&name);
            prop_assert_eq!(normalise(&once), once);
        }
    }

    #[test]
    fn initials_are_the_one_letter_words() {
        assert_eq!(
            split_initials("а с пушкин"),
            (vec!["а", "с"], vec!["пушкин"])
        );
        assert_eq!(split_initials("пушкин"), (vec![], vec!["пушкин"]));
        assert_eq!(split_initials(""), (vec![], vec![]));
        assert_eq!(
            words("а с пушкин").collect::<Vec<_>>(),
            ["а", "с", "пушкин"]
        );
    }
}
