//! The `asOf` instant of `drevo.graph.expand`, `drevo.memory.factsAt` and
//! `drevo.rag.*`: parsing an RFC 3339 timestamp, normalising it to UTC in
//! the format `datetime()` writes (`YYYY-MM-DDThh:mm:ss.sssZ`), and checking
//! whether a fact's `valid_from` / `valid_until` hold at it.
//!
//! Nothing here touches the graph and no date library is involved: the
//! calendar arithmetic is Howard Hinnant's `days_from_civil` over the
//! proleptic Gregorian calendar, the inverse of the executor's formatter.

use std::cmp::Ordering;

use crate::cypher::executor::iso8601_utc;

/// Milliseconds since the Unix epoch of an RFC 3339 timestamp
/// (`2024-03-01T02:00:00+02:00`, `2024-03-01T00:00:00.250Z`), or of a bare
/// RFC 3339 date (`2024-03-01`, read as midnight UTC). `None` for anything
/// else. Digits beyond milliseconds are truncated.
pub(crate) fn parse_instant(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let year = digits(b, 0, 4)?;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if b.len() == 10 {
        return Some(days * 86_400_000);
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let hour = digits(b, 11, 2)?;
    let minute = digits(b, 14, 2)?;
    let second = digits(b, 17, 2)?;
    if b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
        return None;
    }
    // RFC 3339 allows a leap second (`:60`).
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0_i64;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            if at - start < 3 {
                millis = millis * 10 + i64::from(b[at] - b'0');
            }
            at += 1;
        }
        if at == start {
            return None;
        }
        for _ in (at - start)..3 {
            millis *= 10;
        }
    }
    let offset_minutes = match b.get(at) {
        Some(b'Z' | b'z') if b.len() == at + 1 => 0,
        Some(sign @ (b'+' | b'-')) if b.len() == at + 6 => {
            let oh = digits(b, at + 1, 2)?;
            let om = digits(b, at + 4, 2)?;
            if b[at + 3] != b':' || oh > 23 || om > 59 {
                return None;
            }
            let minutes = oh * 60 + om;
            if *sign == b'-' {
                -minutes
            } else {
                minutes
            }
        }
        _ => return None,
    };
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

/// `asOf` checked and normalised to UTC as `datetime()` writes it, so
/// `'2024-03-01T02:00:00+02:00'` becomes `'2024-03-01T00:00:00.000Z'`.
///
/// # Errors
///
/// A message naming `asOf`, with an example, when `text` is not an RFC 3339
/// timestamp or date.
pub(crate) fn normalise_as_of(text: &str) -> Result<String, String> {
    parse_instant(text).map(iso8601_utc).ok_or_else(|| {
        format!(
            "asOf must be an RFC 3339 timestamp such as '2024-03-01T00:00:00Z' or \
             '2024-03-01T02:00:00+02:00' (or a date such as '2024-03-01'), got '{text}'"
        )
    })
}

/// Whether a fact with `valid_from` / `valid_until` holds at `as_of`:
/// `valid_from <= as_of < valid_until`, a missing bound always passing.
/// Bounds that parse as RFC 3339 are compared as instants, so offsets and
/// fractional seconds compare correctly; a bound that does not parse is
/// compared as a string, as before.
pub(crate) fn holds_at(valid_from: Option<&str>, valid_until: Option<&str>, as_of: &str) -> bool {
    let at = parse_instant(as_of);
    let compare = |bound: &str| match (parse_instant(bound), at) {
        (Some(b), Some(a)) => b.cmp(&a),
        _ => bound.cmp(as_of),
    };
    let started = valid_from.is_none_or(|from| compare(from) != Ordering::Greater);
    let still = valid_until.is_none_or(|until| compare(until) == Ordering::Greater);
    started && still
}

/// The `len` ASCII digits of `b` at `at`, as a number.
fn digits(b: &[u8], at: usize, len: usize) -> Option<i64> {
    let slice = b.get(at..at + len)?;
    slice.iter().try_fold(0_i64, |n, c| {
        c.is_ascii_digit().then(|| n * 10 + i64::from(c - b'0'))
    })
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(text: &str) -> Result<String, String> {
        normalise_as_of(text)
    }

    #[test]
    fn utc_z_is_kept_with_milliseconds() {
        assert_eq!(
            norm("2024-03-01T00:30:00Z").as_deref(),
            Ok("2024-03-01T00:30:00.000Z")
        );
        assert_eq!(
            norm("1970-01-01t00:00:00z").as_deref(),
            Ok("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(parse_instant("1970-01-01T00:00:01Z"), Some(1_000));
    }

    #[test]
    fn positive_offset_moves_back_to_utc() {
        assert_eq!(
            norm("2024-03-01T02:00:00+02:00").as_deref(),
            Ok("2024-03-01T00:00:00.000Z")
        );
        // Across midnight and a month end.
        assert_eq!(
            norm("2024-03-01T01:00:00+02:00").as_deref(),
            Ok("2024-02-29T23:00:00.000Z")
        );
    }

    #[test]
    fn negative_half_hour_offset_moves_forward_to_utc() {
        assert_eq!(
            norm("2024-03-01T20:00:00-05:30").as_deref(),
            Ok("2024-03-02T01:30:00.000Z")
        );
    }

    #[test]
    fn fractional_seconds_keep_milliseconds() {
        assert_eq!(
            norm("2024-03-01T00:00:00.5Z").as_deref(),
            Ok("2024-03-01T00:00:00.500Z")
        );
        assert_eq!(
            norm("2024-03-01T00:00:00.123456789Z").as_deref(),
            Ok("2024-03-01T00:00:00.123Z")
        );
        assert!(norm("2024-03-01T00:00:00.Z").is_err());
    }

    #[test]
    fn a_bare_date_is_midnight_utc() {
        assert_eq!(
            norm("2024-03-01").as_deref(),
            Ok("2024-03-01T00:00:00.000Z")
        );
    }

    #[test]
    fn words_empty_and_malformed_values_are_refused_naming_as_of() {
        for bad in [
            "yesterday",
            "",
            "2024-3-1",
            "2024-02-30",
            "2023-02-29T00:00:00Z",
            "2024-03-01T24:00:00Z",
            "2024-03-01T00:00:00",
            "2024-03-01T00:00:00+0200",
            "2024-03-01T00:00:00Z junk",
            "2024-13-01T00:00:00Z",
        ] {
            let message = norm(bad).expect_err(bad);
            assert!(message.contains("asOf"), "{message}");
            assert!(message.contains("2024-03-01T00:00:00Z"), "{message}");
        }
    }

    #[test]
    fn round_trips_through_the_datetime_formatter() {
        for ms in [0_i64, 951_782_400_000, 1_709_251_200_123, -86_400_000] {
            assert_eq!(parse_instant(&iso8601_utc(ms)), Some(ms));
        }
    }

    #[test]
    fn holds_at_compares_instants_not_strings() {
        let at = "2024-03-01T00:00:00.000Z";
        // As strings '.000Z' < '00Z…', but as instants they are equal.
        assert!(holds_at(Some("2024-03-01T00:00:00Z"), None, at));
        assert!(!holds_at(None, Some("2024-03-01T00:00:00Z"), at));
        assert!(!holds_at(Some("2024-03-01T00:30:00Z"), None, at));
        assert!(holds_at(Some("2024-03-01T01:00:00+02:00"), None, at));
        assert!(holds_at(Some("2024-01-01"), Some("2025-01-01"), at));
        assert!(holds_at(None, None, at));
    }

    #[test]
    fn holds_at_falls_back_to_strings_for_unparsed_bounds() {
        assert!(holds_at(Some("2024"), None, "2024-03-01T00:00:00.000Z"));
        assert!(!holds_at(Some("zzz"), None, "2024-03-01T00:00:00.000Z"));
    }
}
