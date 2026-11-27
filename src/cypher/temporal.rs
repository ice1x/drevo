//! Dates, datetimes and durations of Cypher (issue #866).
//!
//! `date('2026-11-27')`, `datetime('2026-11-27T10:00:00+02:00')` and
//! `duration('P1D')` are values of their own kind ([`Temporal`]): they
//! compare and sort as moments in time, have components (`.year`,
//! `.epochMillis`), and a duration shifts a date or a datetime.
//!
//! ```
//! use drevo::cypher::temporal::{Date, Duration, Temporal};
//!
//! let day = Date::parse("2026-11-27").unwrap();
//! let week = Duration::parse("P7D").unwrap();
//! assert_eq!(day.minus(&week).unwrap().to_string(), "2026-11-20");
//! assert_eq!(Temporal::Date(day).component("dayOfWeek"), Some(5));
//! ```
//!
//! The calendar is the proleptic Gregorian one, computed here with no date
//! library. A datetime carries a fixed offset from UTC (`Z`, `+02:00`);
//! named time zones (`[Europe/Kyiv]`) are not supported.
//!
//! Outside a query a temporal value is its ISO 8601 text: that is what a
//! client receives and what a property stores. The text of a datetime
//! always has three digits of milliseconds (more when it has a finer part),
//! so stored datetimes of one offset also sort as text.

use std::cmp::Ordering;
use std::fmt;

const SECS_PER_DAY: i64 = 86_400;
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// The length a month counts for when durations are put in order: a
/// Gregorian year of 365.2425 days, in twelfths.
const SECS_PER_MONTH: i64 = 2_629_746;

/// A date, a datetime or a duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Temporal {
    /// A calendar date.
    Date(Date),
    /// A moment in time with an offset from UTC.
    DateTime(DateTime),
    /// A length of time in months, days and seconds.
    Duration(Duration),
}

/// A calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    /// Days since 1970-01-01.
    days: i64,
}

/// A moment in time with an offset from UTC.
///
/// Two datetimes are equal, and ordered, by the moment: `10:00Z` equals
/// `12:00+02:00`.
#[derive(Debug, Clone, Copy)]
pub struct DateTime {
    /// Seconds since 1970-01-01T00:00:00Z.
    secs: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    nanos: u32,
    /// Seconds east of UTC the datetime is written in.
    offset: i32,
}

/// A length of time: months, days and seconds are kept apart, because a
/// month has no fixed number of days and a day no fixed number of seconds
/// on every calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Duration {
    months: i64,
    days: i64,
    secs: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    nanos: u32,
}

// ===== Calendar =============================================================

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year, month and day of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// The years a date may have: wide enough for any real use, narrow enough
/// that no arithmetic here overflows.
const YEARS: std::ops::RangeInclusive<i64> = -999_999..=999_999;

/// Move a day count by whole months, keeping the day of the month or, when
/// the month is shorter, its last day (31 January + 1 month = 28 February).
fn add_months(days: i64, months: i64) -> Result<i64, String> {
    let (year, month, day) = civil_from_days(days);
    let total = year
        .checked_mul(12)
        .and_then(|m| m.checked_add(i64::from(month) - 1))
        .and_then(|m| m.checked_add(months))
        .ok_or("the date is out of range")?;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    if !YEARS.contains(&y) {
        return Err("the date is out of range".into());
    }
    Ok(days_from_civil(y, m, day.min(days_in_month(y, m))))
}

// ===== Text helpers =========================================================

/// Exactly `len` ASCII digits at the start of `text`, as a number, and the
/// rest.
fn digits(text: &str, len: usize) -> Option<(i64, &str)> {
    let head = text.get(..len)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, &text[len..]))
}

/// A UTC offset as text: `Z`, `+02:00`, `-0530`, `+02`. Seconds east.
fn parse_offset(text: &str) -> Option<i32> {
    if text.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, rest) = digits(&text[1..], 2)?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let minutes = if rest.is_empty() {
        0
    } else {
        let (minutes, tail) = digits(rest, 2)?;
        if !tail.is_empty() {
            return None;
        }
        minutes
    };
    if hours > 18 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60) as i32)
}

fn format_offset(offset: i32) -> String {
    if offset == 0 {
        return "Z".into();
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60)
}

// ===== Date =================================================================

impl Date {
    /// The date `year`-`month`-`day`, or why there is no such date.
    pub fn from_ymd(year: i64, month: i64, day: i64) -> Result<Self, String> {
        if !YEARS.contains(&year) {
            return Err(format!("year {year} is out of range"));
        }
        if !(1..=12).contains(&month) {
            return Err(format!("month {month} is not in 1..=12"));
        }
        let last = i64::from(days_in_month(year, month as u32));
        if !(1..=last).contains(&day) {
            return Err(format!("day {day} is not in 1..={last}"));
        }
        Ok(Self {
            days: days_from_civil(year, month as u32, day as u32),
        })
    }

    /// Today's date in UTC.
    #[must_use]
    pub fn today() -> Self {
        DateTime::now().date()
    }

    /// Read `YYYY-MM-DD`, `YYYYMMDD` or `YYYY-MM` (the first of the month).
    /// A datetime text gives its date.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if let Some((date, _)) = text.split_once(['T', 't']) {
            // The date part of a datetime; the rest must be a valid time.
            DateTime::parse(text)?;
            return Self::parse(date);
        }
        let bad = || format!("`{text}` is not a date (expected YYYY-MM-DD)");
        let (year, rest) = digits(text, 4).ok_or_else(bad)?;
        let (month, day) = if let Some(rest) = rest.strip_prefix('-') {
            let (month, rest) = digits(rest, 2).ok_or_else(bad)?;
            match rest.strip_prefix('-') {
                Some(rest) => {
                    let (day, tail) = digits(rest, 2).ok_or_else(bad)?;
                    if !tail.is_empty() {
                        return Err(bad());
                    }
                    (month, day)
                }
                None if rest.is_empty() => (month, 1),
                None => return Err(bad()),
            }
        } else {
            let (month, rest) = digits(rest, 2).ok_or_else(bad)?;
            let (day, tail) = digits(rest, 2).ok_or_else(bad)?;
            if !tail.is_empty() {
                return Err(bad());
            }
            (month, day)
        };
        Self::from_ymd(year, month, day).map_err(|why| format!("`{text}` is not a date: {why}"))
    }

    /// The date `duration` later: its months first (keeping the day of the
    /// month, or the month's last day), then its days. Whole days of its
    /// seconds count; a shorter part does not move a date.
    pub fn plus(&self, duration: &Duration) -> Result<Self, String> {
        let days = add_months(self.days, duration.months)?
            .checked_add(duration.days)
            .and_then(|d| d.checked_add(duration.secs / SECS_PER_DAY))
            .ok_or("the date is out of range")?;
        Self::from_days(days)
    }

    /// The date `duration` earlier.
    pub fn minus(&self, duration: &Duration) -> Result<Self, String> {
        self.plus(&duration.negated()?)
    }

    fn from_days(days: i64) -> Result<Self, String> {
        let range = days_from_civil(*YEARS.start(), 1, 1)..=days_from_civil(*YEARS.end(), 12, 31);
        if !range.contains(&days) {
            return Err("the date is out of range".into());
        }
        Ok(Self { days })
    }

    fn ymd(&self) -> (i64, u32, u32) {
        civil_from_days(self.days)
    }

    /// Monday = 1 ... Sunday = 7.
    fn day_of_week(&self) -> i64 {
        // 1970-01-01 was a Thursday.
        (self.days + 3).rem_euclid(7) + 1
    }

    fn ordinal_day(&self) -> i64 {
        let (year, _, _) = self.ymd();
        self.days - days_from_civil(year, 1, 1) + 1
    }

    /// The ISO 8601 week-numbering year and week.
    fn iso_week(&self) -> (i64, i64) {
        let (year, _, _) = self.ymd();
        let weeks_in = |y: i64| {
            // A year has 53 weeks when it starts on a Thursday, or on a
            // Wednesday and is a leap year.
            let first = Self {
                days: days_from_civil(y, 1, 1),
            }
            .day_of_week();
            if first == 4 || (first == 3 && is_leap(y)) {
                53
            } else {
                52
            }
        };
        let week = (self.ordinal_day() - self.day_of_week() + 10) / 7;
        if week < 1 {
            (year - 1, weeks_in(year - 1))
        } else if week > weeks_in(year) {
            (year + 1, 1)
        } else {
            (year, week)
        }
    }

    fn component(&self, name: &str) -> Option<i64> {
        let (year, month, day) = self.ymd();
        Some(match name {
            "year" => year,
            "quarter" => (i64::from(month) - 1) / 3 + 1,
            "month" => i64::from(month),
            "week" => self.iso_week().1,
            "weekYear" => self.iso_week().0,
            "day" => i64::from(day),
            "dayOfWeek" | "weekDay" => self.day_of_week(),
            "ordinalDay" | "dayOfYear" => self.ordinal_day(),
            "epochDays" => self.days,
            _ => return None,
        })
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.ymd();
        if year < 0 {
            write!(f, "-{:04}-{month:02}-{day:02}", -year)
        } else {
            write!(f, "{year:04}-{month:02}-{day:02}")
        }
    }
}

// ===== DateTime =============================================================

/// The parts of a datetime that [`DateTime::from_parts`] puts together.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeOfDay {
    /// Hour, `0..=23`.
    pub hour: i64,
    /// Minute, `0..=59`.
    pub minute: i64,
    /// Second, `0..=59`.
    pub second: i64,
    /// Nanoseconds within the second.
    pub nanos: i64,
}

impl DateTime {
    /// The current moment, in UTC, to the millisecond.
    #[must_use]
    pub fn now() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self::from_epoch_millis(millis)
    }

    /// The moment `millis` milliseconds after 1970-01-01T00:00:00Z, in UTC.
    #[must_use]
    pub fn from_epoch_millis(millis: i64) -> Self {
        Self {
            secs: millis.div_euclid(1000),
            nanos: (millis.rem_euclid(1000) * 1_000_000) as u32,
            offset: 0,
        }
    }

    /// The moment `secs` seconds and `nanos` nanoseconds after the epoch.
    pub fn from_epoch(secs: i64, nanos: i64) -> Result<Self, String> {
        let secs = secs
            .checked_add(nanos.div_euclid(NANOS_PER_SEC))
            .ok_or("the datetime is out of range")?;
        Self::checked(secs, nanos.rem_euclid(NANOS_PER_SEC) as u32, 0)
    }

    /// `date` at `time` as written at `offset` seconds east of UTC.
    pub fn from_parts(date: Date, time: TimeOfDay, offset: i32) -> Result<Self, String> {
        let check = |name: &str, value: i64, max: i64| {
            if (0..=max).contains(&value) {
                Ok(())
            } else {
                Err(format!("{name} {value} is not in 0..={max}"))
            }
        };
        check("hour", time.hour, 23)?;
        check("minute", time.minute, 59)?;
        check("second", time.second, 59)?;
        check("nanosecond", time.nanos, NANOS_PER_SEC - 1)?;
        let local = date.days * SECS_PER_DAY + time.hour * 3600 + time.minute * 60 + time.second;
        Self::checked(local - i64::from(offset), time.nanos as u32, offset)
    }

    /// Read `YYYY-MM-DDThh:mm[:ss[.fraction]][offset]`. Without an offset
    /// the text is in UTC; a date alone is its midnight in UTC.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let Some((date, rest)) = text.split_once(['T', 't']) else {
            let date = Date::parse(text).map_err(|_| {
                format!("`{text}` is not a datetime (expected YYYY-MM-DDThh:mm:ss, with an offset such as Z or +02:00)")
            })?;
            return Self::from_parts(date, TimeOfDay::default(), 0);
        };
        let bad = |why: &str| format!("`{text}` is not a datetime: {why}");
        if rest.contains('[') {
            return Err(bad(
                "named time zones are not supported, write an offset such as +02:00",
            ));
        }
        let date = Date::parse(date).map_err(|why| bad(&why))?;
        // The offset starts at `Z` or at the first sign after the time.
        let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
        let (clock, zone) = rest.split_at(zone_at);
        let offset = if zone.is_empty() {
            0
        } else {
            parse_offset(zone).ok_or_else(|| bad("the offset is not Z or +hh:mm"))?
        };
        let (clock, fraction) = match clock.split_once(['.', ',']) {
            Some((clock, fraction)) => (clock, Some(fraction)),
            None => (clock, None),
        };
        let time_bad = || bad("the time is not hh:mm[:ss]");
        let colons = clock.contains(':');
        let mut fields = [0_i64; 3];
        let mut rest = clock;
        let mut read = 0;
        while !rest.is_empty() && read < 3 {
            let (value, tail) = digits(rest, 2).ok_or_else(time_bad)?;
            fields[read] = value;
            read += 1;
            rest = if colons && !tail.is_empty() {
                tail.strip_prefix(':').ok_or_else(time_bad)?
            } else {
                tail
            };
        }
        if read == 0 || !rest.is_empty() {
            return Err(time_bad());
        }
        let nanos = match fraction {
            None => 0,
            Some(f) if read == 3 && (1..=9).contains(&f.len()) => {
                let (value, _) = digits(f, f.len()).ok_or_else(time_bad)?;
                value * 10_i64.pow(9 - f.len() as u32)
            }
            Some(_) => return Err(bad("a fraction needs seconds and at most nine digits")),
        };
        let time = TimeOfDay {
            hour: fields[0],
            minute: fields[1],
            second: fields[2],
            nanos,
        };
        Self::from_parts(date, time, offset).map_err(|why| bad(&why))
    }

    /// Seconds east of UTC this datetime is written in.
    #[must_use]
    pub fn offset_seconds(&self) -> i32 {
        self.offset
    }

    /// The same moment written at another offset from UTC.
    pub fn at_offset(&self, offset: i32) -> Result<Self, String> {
        Self::checked(self.secs, self.nanos, offset)
    }

    /// The date this datetime is written on, in its own offset.
    #[must_use]
    pub fn date(&self) -> Date {
        Date {
            days: self.local_secs().div_euclid(SECS_PER_DAY),
        }
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub fn epoch_millis(&self) -> i64 {
        self.secs
            .saturating_mul(1000)
            .saturating_add(i64::from(self.nanos / 1_000_000))
    }

    /// The moment `duration` later: its months and days move the date as
    /// written (the time of day stays), then its seconds are added.
    pub fn plus(&self, duration: &Duration) -> Result<Self, String> {
        let local = self.local_secs();
        let days = add_months(local.div_euclid(SECS_PER_DAY), duration.months)?
            .checked_add(duration.days)
            .ok_or("the datetime is out of range")?;
        let total = i64::from(self.nanos) + i64::from(duration.nanos);
        let secs = days
            .checked_mul(SECS_PER_DAY)
            .and_then(|s| s.checked_add(local.rem_euclid(SECS_PER_DAY)))
            .and_then(|s| s.checked_sub(i64::from(self.offset)))
            .and_then(|s| s.checked_add(duration.secs))
            .and_then(|s| s.checked_add(total / NANOS_PER_SEC))
            .ok_or("the datetime is out of range")?;
        Self::checked(secs, (total % NANOS_PER_SEC) as u32, self.offset)
    }

    /// The moment `duration` earlier.
    pub fn minus(&self, duration: &Duration) -> Result<Self, String> {
        self.plus(&duration.negated()?)
    }

    fn checked(secs: i64, nanos: u32, offset: i32) -> Result<Self, String> {
        let limit = (days_from_civil(*YEARS.end(), 12, 31) + 1) * SECS_PER_DAY;
        if !(-limit..limit).contains(&secs) {
            return Err("the datetime is out of range".into());
        }
        Ok(Self {
            secs,
            nanos,
            offset,
        })
    }

    /// Seconds since the epoch of the clock reading in the datetime's own
    /// offset.
    fn local_secs(&self) -> i64 {
        self.secs + i64::from(self.offset)
    }

    fn component(&self, name: &str) -> Option<i64> {
        let of_day = self.local_secs().rem_euclid(SECS_PER_DAY);
        let nanos = i64::from(self.nanos);
        Some(match name {
            "hour" => of_day / 3600,
            "minute" => of_day % 3600 / 60,
            "second" => of_day % 60,
            "millisecond" => nanos / 1_000_000,
            "microsecond" => nanos / 1000,
            "nanosecond" => nanos,
            "epochSeconds" => self.secs,
            "epochMillis" => self.epoch_millis(),
            "offsetSeconds" => i64::from(self.offset),
            "offsetMinutes" => i64::from(self.offset) / 60,
            _ => return self.date().component(name),
        })
    }
}

impl PartialEq for DateTime {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for DateTime {}

impl PartialOrd for DateTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DateTime {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.secs, self.nanos).cmp(&(other.secs, other.nanos))
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let of_day = self.local_secs().rem_euclid(SECS_PER_DAY);
        write!(
            f,
            "{}T{:02}:{:02}:{:02}",
            self.date(),
            of_day / 3600,
            of_day % 3600 / 60,
            of_day % 60
        )?;
        // Always milliseconds, so datetimes of one offset sort as text.
        if self.nanos % 1_000_000 == 0 {
            write!(f, ".{:03}", self.nanos / 1_000_000)?;
        } else {
            let fraction = format!("{:09}", self.nanos);
            write!(f, ".{}", fraction.trim_end_matches('0'))?;
        }
        f.write_str(&format_offset(self.offset))
    }
}

// ===== Duration =============================================================

impl Duration {
    /// A duration of `months`, `days`, `secs` and `nanos`; each may be
    /// negative.
    pub fn new(months: i64, days: i64, secs: i64, nanos: i64) -> Result<Self, String> {
        let secs = secs
            .checked_add(nanos.div_euclid(NANOS_PER_SEC))
            .ok_or("the duration is out of range")?;
        // Bounds that keep every sum and comparison below within `i64`.
        const MONTHS: i64 = 12 * 2_000_000;
        const DAYS: i64 = 366 * 2_000_000;
        if months.abs() > MONTHS || days.abs() > DAYS || secs.abs() > DAYS * SECS_PER_DAY {
            return Err("the duration is out of range".into());
        }
        Ok(Self {
            months,
            days,
            secs,
            nanos: nanos.rem_euclid(NANOS_PER_SEC) as u32,
        })
    }

    /// Read an ISO 8601 duration: `P1Y2M3W4DT5H6M7.5S`, any part optional,
    /// a part may be negative (`P-1D`, `-PT1H`). Only seconds may have a
    /// fraction.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let bad = |why: &str| format!("`{trimmed}` is not a duration: {why}");
        let (negative, body) = match trimmed.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let body = body
            .strip_prefix(['P', 'p'])
            .ok_or_else(|| bad("expected a text such as P1D or PT1H30M"))?;
        if body.is_empty() {
            return Err(bad("it has no parts"));
        }
        let (mut months, mut days, mut secs, mut nanos) = (0_i64, 0_i64, 0_i64, 0_i64);
        let mut in_time = false;
        let mut parts = 0;
        let mut rest = body;
        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix(['T', 't']) {
                if in_time {
                    return Err(bad("T is written twice"));
                }
                in_time = true;
                rest = tail;
                continue;
            }
            let end = rest
                .find(|c: char| c.is_ascii_alphabetic())
                .ok_or_else(|| bad("a number has no unit"))?;
            let (number, tail) = rest.split_at(end);
            let unit = tail.as_bytes()[0].to_ascii_uppercase();
            rest = &tail[1..];
            parts += 1;
            let whole = |n: &str| {
                n.parse::<i64>()
                    .map_err(|_| bad("a part is not a whole number"))
            };
            let add = |total: i64, n: i64, scale: i64| {
                n.checked_mul(scale)
                    .and_then(|v| total.checked_add(v))
                    .ok_or_else(|| bad("it is out of range"))
            };
            match (in_time, unit) {
                (false, b'Y') => months = add(months, whole(number)?, 12)?,
                (false, b'M') => months = add(months, whole(number)?, 1)?,
                (false, b'W') => days = add(days, whole(number)?, 7)?,
                (false, b'D') => days = add(days, whole(number)?, 1)?,
                (true, b'H') => secs = add(secs, whole(number)?, 3600)?,
                (true, b'M') => secs = add(secs, whole(number)?, 60)?,
                (true, b'S') => {
                    let (int, fraction) = match number.split_once(['.', ',']) {
                        Some((int, fraction)) => (int, fraction),
                        None => (number, ""),
                    };
                    if fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(bad("seconds have at most nine digits of fraction"));
                    }
                    let sign = if int.starts_with('-') { -1 } else { 1 };
                    let int = if int.is_empty() || int == "-" {
                        0
                    } else {
                        whole(int)?
                    };
                    secs = add(secs, int, 1)?;
                    if !fraction.is_empty() {
                        let scale = 10_i64.pow(9 - fraction.len() as u32);
                        nanos += sign * whole(fraction)? * scale;
                    }
                }
                _ => return Err(bad("a unit is unknown or on the wrong side of T")),
            }
        }
        if parts == 0 {
            return Err(bad("it has no parts"));
        }
        let duration = Self::new(months, days, secs, nanos).map_err(|why| bad(&why))?;
        if negative {
            duration.negated().map_err(|why| bad(&why))
        } else {
            Ok(duration)
        }
    }

    /// The duration from `from` to `to` in months, days and a rest of
    /// seconds: adding it to `from` gives `to`. A date counts as its
    /// midnight in UTC.
    #[must_use]
    pub fn between(from: &Temporal, to: &Temporal) -> Option<Self> {
        let (a, b) = (from.as_datetime()?, to.as_datetime()?);
        // Whole months first, never stepping past `to`.
        let (ad, bd) = (
            a.secs.div_euclid(SECS_PER_DAY),
            b.secs.div_euclid(SECS_PER_DAY),
        );
        let ((ay, am, _), (by, bm, _)) = (civil_from_days(ad), civil_from_days(bd));
        let mut months = (by - ay) * 12 + i64::from(bm) - i64::from(am);
        let step =
            |months: i64| -> Option<DateTime> { a.plus(&Self::new(months, 0, 0, 0).ok()?).ok() };
        let forward = b >= a;
        while (forward && step(months)? > b) || (!forward && step(months)? < b) {
            months += if forward { -1 } else { 1 };
        }
        let rest = Self::in_seconds_between(&step(months)?, &b);
        // `rest` has the sign of the direction, so the division rounds
        // towards zero and the remainder keeps the sign.
        let (days, secs) = (rest.secs / SECS_PER_DAY, rest.secs % SECS_PER_DAY);
        let nanos = i64::from(rest.nanos);
        Self::new(months, days, secs, nanos).ok()
    }

    /// The duration from `from` to `to` in days and a rest of seconds.
    #[must_use]
    pub fn in_days(from: &Temporal, to: &Temporal) -> Option<Self> {
        let rest = Self::in_seconds(from, to)?;
        Self::new(
            0,
            rest.secs / SECS_PER_DAY,
            rest.secs % SECS_PER_DAY,
            i64::from(rest.nanos),
        )
        .ok()
    }

    /// The whole months from `from` to `to`.
    #[must_use]
    pub fn in_months(from: &Temporal, to: &Temporal) -> Option<Self> {
        let whole = Self::between(from, to)?;
        Self::new(whole.months, 0, 0, 0).ok()
    }

    /// The duration from `from` to `to` in seconds.
    #[must_use]
    pub fn in_seconds(from: &Temporal, to: &Temporal) -> Option<Self> {
        Some(Self::in_seconds_between(
            &from.as_datetime()?,
            &to.as_datetime()?,
        ))
    }

    fn in_seconds_between(a: &DateTime, b: &DateTime) -> Self {
        let nanos = i64::from(b.nanos) - i64::from(a.nanos);
        Self {
            months: 0,
            days: 0,
            secs: b.secs - a.secs + nanos.div_euclid(NANOS_PER_SEC),
            nanos: nanos.rem_euclid(NANOS_PER_SEC) as u32,
        }
    }

    /// The sum of two durations.
    pub fn plus(&self, other: &Self) -> Result<Self, String> {
        let sum = |a: i64, b: i64| a.checked_add(b).ok_or("the duration is out of range");
        Self::new(
            sum(self.months, other.months)?,
            sum(self.days, other.days)?,
            sum(self.secs, other.secs)?,
            i64::from(self.nanos) + i64::from(other.nanos),
        )
    }

    /// This duration less `other`.
    pub fn minus(&self, other: &Self) -> Result<Self, String> {
        self.plus(&other.negated()?)
    }

    /// The duration of the opposite direction.
    pub fn negated(&self) -> Result<Self, String> {
        Self::new(-self.months, -self.days, -self.secs, -i64::from(self.nanos))
    }

    /// The duration `factor` times as long. A fraction of a month becomes
    /// days and a fraction of a day seconds, a month counting as 30.436875
    /// days.
    pub fn times(&self, factor: f64) -> Result<Self, String> {
        if !factor.is_finite() {
            return Err("a duration cannot be scaled by a number that is not finite".into());
        }
        let months = self.months as f64 * factor;
        let days = self.days as f64 * factor + months.fract() * (SECS_PER_MONTH as f64 / 86_400.0);
        let secs = (self.secs as f64 + f64::from(self.nanos) / 1e9) * factor
            + days.fract() * SECS_PER_DAY as f64;
        let whole = secs.floor();
        if months.abs() > 1e12 || days.abs() > 1e12 || whole.abs() > 1e16 {
            return Err("the duration is out of range".into());
        }
        Self::new(
            months.trunc() as i64,
            days.trunc() as i64,
            whole as i64,
            ((secs - whole) * 1e9).round() as i64,
        )
    }

    /// Seconds and nanoseconds this duration counts for when durations are
    /// put in order: a month as a twelfth of a Gregorian year, a day as 24
    /// hours.
    fn approximate(&self) -> (i64, u32) {
        (
            self.months * SECS_PER_MONTH + self.days * SECS_PER_DAY + self.secs,
            self.nanos,
        )
    }

    fn component(&self, name: &str) -> Option<i64> {
        let nanos = i64::from(self.nanos);
        Some(match name {
            "years" => self.months / 12,
            "quarters" => self.months / 3,
            "months" => self.months,
            "weeks" => self.days / 7,
            "days" => self.days,
            "hours" => self.secs / 3600,
            "minutes" => self.secs / 60,
            "seconds" => self.secs,
            "milliseconds" => self
                .secs
                .saturating_mul(1000)
                .saturating_add(nanos / 1_000_000),
            "microseconds" => self
                .secs
                .saturating_mul(1_000_000)
                .saturating_add(nanos / 1000),
            "nanoseconds" => self
                .secs
                .saturating_mul(NANOS_PER_SEC)
                .saturating_add(nanos),
            "quartersOfYear" => self.months % 12 / 3,
            "monthsOfQuarter" => self.months % 3,
            "monthsOfYear" => self.months % 12,
            "daysOfWeek" => self.days % 7,
            "minutesOfHour" => self.secs % 3600 / 60,
            "secondsOfMinute" => self.secs % 60,
            "millisecondsOfSecond" => nanos / 1_000_000,
            "microsecondsOfSecond" => nanos / 1000,
            "nanosecondsOfSecond" => nanos,
            _ => return None,
        })
    }
}

impl PartialOrd for Duration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Duration {
    /// By approximate length; two durations of one length that are not the
    /// same (`P1M` and `P30DT10H29M6S`) are ordered by their parts, so the
    /// order agrees with equality.
    fn cmp(&self, other: &Self) -> Ordering {
        self.approximate().cmp(&other.approximate()).then_with(|| {
            (self.months, self.days, self.secs).cmp(&(other.months, other.days, other.secs))
        })
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P")?;
        let mut written = false;
        for (value, unit) in [
            (self.months / 12, 'Y'),
            (self.months % 12, 'M'),
            (self.days, 'D'),
        ] {
            if value != 0 {
                write!(f, "{value}{unit}")?;
                written = true;
            }
        }
        // The time as one signed count of nanoseconds, so -1.5 s is
        // written `-1.5S`, not `-2S` and a positive fraction.
        let total = i128::from(self.secs) * i128::from(NANOS_PER_SEC) + i128::from(self.nanos);
        if total == 0 {
            return if written { Ok(()) } else { f.write_str("T0S") };
        }
        f.write_str("T")?;
        let sign = if total < 0 { "-" } else { "" };
        let abs = total.unsigned_abs();
        let per_sec = NANOS_PER_SEC as u128;
        let (secs, nanos) = (abs / per_sec, abs % per_sec);
        let (hours, minutes, seconds) = (secs / 3600, secs % 3600 / 60, secs % 60);
        if hours != 0 {
            write!(f, "{sign}{hours}H")?;
        }
        if minutes != 0 {
            write!(f, "{sign}{minutes}M")?;
        }
        if seconds != 0 || nanos != 0 {
            write!(f, "{sign}{seconds}")?;
            if nanos != 0 {
                let fraction = format!("{nanos:09}");
                write!(f, ".{}", fraction.trim_end_matches('0'))?;
            }
            f.write_str("S")?;
        }
        Ok(())
    }
}

// ===== Temporal =============================================================

impl Temporal {
    /// `Date`, `DateTime` or `Duration`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Date(_) => "Date",
            Self::DateTime(_) => "DateTime",
            Self::Duration(_) => "Duration",
        }
    }

    /// A whole-number part of the value: `year`, `month`, `day`, `hour`,
    /// `epochMillis` and the like of a date or datetime, `months`, `days`,
    /// `seconds` and the like of a duration. `None` for a name the value
    /// does not have.
    #[must_use]
    pub fn component(&self, name: &str) -> Option<i64> {
        match self {
            Self::Date(d) => d.component(name),
            Self::DateTime(d) => d.component(name),
            Self::Duration(d) => d.component(name),
        }
    }

    /// The offset of a datetime as text (`Z`, `+02:00`): its `timezone` and
    /// `offset` components.
    #[must_use]
    pub fn offset_text(&self) -> Option<String> {
        match self {
            Self::DateTime(d) => Some(format_offset(d.offset)),
            _ => None,
        }
    }

    /// Read `text` as a value of the same kind as `self`: how a stored ISO
    /// text is compared with a temporal value.
    #[must_use]
    pub fn parse_same_kind(&self, text: &str) -> Option<Self> {
        match self {
            Self::Date(_) => Date::parse(text).ok().map(Self::Date),
            Self::DateTime(_) => DateTime::parse(text).ok().map(Self::DateTime),
            Self::Duration(_) => Duration::parse(text).ok().map(Self::Duration),
        }
    }

    /// How two values of one kind are ordered; `None` for two kinds.
    #[must_use]
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Date(a), Self::Date(b)) => Some(a.cmp(b)),
            (Self::DateTime(a), Self::DateTime(b)) => Some(a.cmp(b)),
            (Self::Duration(a), Self::Duration(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// The moment a date (its midnight in UTC) or a datetime stands for.
    fn as_datetime(&self) -> Option<DateTime> {
        match self {
            Self::Date(d) => DateTime::from_parts(*d, TimeOfDay::default(), 0).ok(),
            Self::DateTime(d) => Some(*d),
            Self::Duration(_) => None,
        }
    }
}

impl fmt::Display for Temporal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Date(d) => d.fmt(f),
            Self::DateTime(d) => d.fmt(f),
            Self::Duration(d) => d.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Date {
        Date::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn datetime(text: &str) -> DateTime {
        DateTime::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn duration(text: &str) -> Duration {
        Duration::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_calendar_round_trips_across_centuries() {
        for (y, m, d) in [
            (1970, 1, 1),
            (1969, 12, 31),
            (2000, 2, 29),
            (1900, 3, 1),
            (2026, 11, 27),
            (-1, 12, 31),
            (9999, 12, 31),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 11, 27), 20_784);
    }

    #[test]
    fn a_date_reads_its_three_forms_and_prints_one() {
        assert_eq!(date("2026-11-27").to_string(), "2026-11-27");
        assert_eq!(date("20261127").to_string(), "2026-11-27");
        assert_eq!(date("2026-11").to_string(), "2026-11-01");
        assert_eq!(
            date(" 2026-11-27T23:30:00+02:00 ").to_string(),
            "2026-11-27"
        );
    }

    #[test]
    fn a_date_that_does_not_exist_is_refused_with_the_text() {
        for text in [
            "2026-02-30",
            "2026-13-01",
            "2025-02-29",
            "2026-00-10",
            "2026-1-1",
            "today",
            "",
        ] {
            let err = Date::parse(text).expect_err(text);
            assert!(err.contains(text), "{err}");
        }
        assert!(Date::parse("2024-02-29").is_ok());
    }

    #[test]
    fn date_components_follow_iso_8601() {
        let friday = Temporal::Date(date("2026-11-27"));
        for (name, value) in [
            ("year", 2026),
            ("quarter", 4),
            ("month", 11),
            ("week", 48),
            ("weekYear", 2026),
            ("day", 27),
            ("dayOfWeek", 5),
            ("ordinalDay", 331),
            ("epochDays", 20_784),
        ] {
            assert_eq!(friday.component(name), Some(value), "{name}");
        }
        assert_eq!(friday.component("hour"), None);
        // 1 January 2027 is a Friday of the last week of 2026; 2026 has 53.
        let new_year = Temporal::Date(date("2027-01-01"));
        assert_eq!(new_year.component("week"), Some(53));
        assert_eq!(new_year.component("weekYear"), Some(2026));
        // 29 December 2025 is the Monday of week 1 of 2026.
        let monday = Temporal::Date(date("2025-12-29"));
        assert_eq!(monday.component("week"), Some(1));
        assert_eq!(monday.component("weekYear"), Some(2026));
        assert_eq!(monday.component("dayOfWeek"), Some(1));
    }

    #[test]
    fn a_datetime_reads_offsets_and_fractions() {
        let utc = datetime("2026-11-27T10:00:00Z");
        assert_eq!(utc.epoch_millis(), 1_795_773_600_000);
        assert_eq!(datetime("2026-11-27T12:00:00+02:00"), utc);
        assert_eq!(datetime("2026-11-27T04:30-0530"), utc);
        assert_eq!(datetime("2026-11-27T10:00"), utc);
        assert_eq!(datetime("2026-11-27T1000"), utc);
        assert_eq!(datetime("2026-11-27T100000"), utc);
        assert_eq!(datetime("2026-11-27").epoch_millis(), 1_795_737_600_000);
        let fine = datetime("2026-11-27T10:00:00.123456789Z");
        assert_eq!(Temporal::DateTime(fine).component("millisecond"), Some(123));
        assert_eq!(
            Temporal::DateTime(fine).component("nanosecond"),
            Some(123_456_789)
        );
        assert_eq!(fine.to_string(), "2026-11-27T10:00:00.123456789Z");
    }

    #[test]
    fn a_datetime_prints_milliseconds_and_its_offset() {
        assert_eq!(
            datetime("2026-11-27T10:00:00Z").to_string(),
            "2026-11-27T10:00:00.000Z"
        );
        assert_eq!(
            datetime("2026-11-27T23:30:05.5+02:00").to_string(),
            "2026-11-27T23:30:05.500+02:00"
        );
        assert_eq!(
            DateTime::from_epoch_millis(-1).to_string(),
            "1969-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn datetime_components_are_read_in_its_own_offset() {
        let late = Temporal::DateTime(datetime("2026-11-27T23:30:05+02:00"));
        for (name, value) in [
            ("year", 2026),
            ("day", 27),
            ("hour", 23),
            ("minute", 30),
            ("second", 5),
            ("offsetSeconds", 7200),
            ("offsetMinutes", 120),
            ("epochSeconds", 1_795_815_005),
        ] {
            assert_eq!(late.component(name), Some(value), "{name}");
        }
        assert_eq!(late.offset_text().as_deref(), Some("+02:00"));
        assert_eq!(
            Temporal::DateTime(datetime("2026-11-28T01:30:05+02:00")).component("day"),
            Some(28)
        );
    }

    #[test]
    fn a_bad_datetime_is_refused_with_the_text() {
        for text in [
            "2026-11-27T25:00:00Z",
            "2026-11-27T10:61:00Z",
            "2026-11-27T10:00:00+25:00",
            "2026-11-27T10:00:00[Europe/Kyiv]",
            "2026-11-27T",
            "2026-11-27T10:00:00.Z",
            "2026-02-30T10:00:00Z",
            "noon",
        ] {
            let err = DateTime::parse(text).expect_err(text);
            assert!(err.contains(text), "{err}");
        }
    }

    #[test]
    fn a_duration_reads_and_prints_iso_8601() {
        for (text, printed) in [
            ("P1D", "P1D"),
            ("P1Y2M3DT4H5M6S", "P1Y2M3DT4H5M6S"),
            ("P2W", "P14D"),
            ("PT90M", "PT1H30M"),
            ("PT1.5S", "PT1.5S"),
            ("PT0.000000001S", "PT0.000000001S"),
            ("P14M", "P1Y2M"),
            ("PT0S", "PT0S"),
            ("P0D", "PT0S"),
            ("P-1D", "P-1D"),
            ("-P1D", "P-1D"),
            ("PT-1.5S", "PT-1.5S"),
            ("-PT1H30M", "PT-1H-30M"),
        ] {
            assert_eq!(duration(text).to_string(), printed, "{text}");
        }
    }

    #[test]
    fn a_bad_duration_is_refused_with_the_text() {
        for text in [
            "1D",
            "P",
            "PT",
            "P1",
            "P1H",
            "PT1D",
            "P1.5D",
            "PT1S2",
            "P1DT1HT1M",
            "week",
        ] {
            let err = Duration::parse(text).expect_err(text);
            assert!(err.contains(text), "{err}");
        }
    }

    #[test]
    fn a_duration_shifts_a_date_by_months_then_days() {
        let d = date("2026-01-31");
        assert_eq!(d.plus(&duration("P1M")).unwrap().to_string(), "2026-02-28");
        assert_eq!(
            d.plus(&duration("P1M1D")).unwrap().to_string(),
            "2026-03-01"
        );
        assert_eq!(
            d.plus(&duration("P1Y1M")).unwrap().to_string(),
            "2027-02-28"
        );
        assert_eq!(d.minus(&duration("P2M")).unwrap().to_string(), "2025-11-30");
        assert_eq!(
            date("2024-02-29")
                .plus(&duration("P1Y"))
                .unwrap()
                .to_string(),
            "2025-02-28"
        );
        // Hours do not move a date until they make a day.
        assert_eq!(d.plus(&duration("PT23H")).unwrap(), d);
        assert_eq!(
            d.plus(&duration("PT48H")).unwrap().to_string(),
            "2026-02-02"
        );
    }

    #[test]
    fn a_duration_shifts_a_datetime_and_keeps_its_offset() {
        let t = datetime("2026-01-31T22:30:00+02:00");
        assert_eq!(
            t.plus(&duration("P1MT2H")).unwrap().to_string(),
            "2026-03-01T00:30:00.000+02:00"
        );
        assert_eq!(
            t.minus(&duration("PT0.5S")).unwrap().to_string(),
            "2026-01-31T22:29:59.500+02:00"
        );
    }

    #[test]
    fn between_counts_months_days_and_a_rest() {
        let between = |a: &str, b: &str| {
            Duration::between(
                &Temporal::Date(date(a)).parse_same_kind(a).unwrap(),
                &Temporal::Date(date(b)),
            )
            .unwrap()
            .to_string()
        };
        assert_eq!(between("2026-01-01", "2026-11-27"), "P10M26D");
        assert_eq!(between("2026-01-31", "2026-03-01"), "P1M1D");
        assert_eq!(between("2026-11-27", "2026-01-01"), "P-10M-26D");
        assert_eq!(between("2026-11-27", "2026-11-27"), "PT0S");
        let a = Temporal::DateTime(datetime("2026-11-27T10:00:00Z"));
        let b = Temporal::DateTime(datetime("2026-11-28T12:30:00.5+02:00"));
        assert_eq!(
            Duration::between(&a, &b).unwrap().to_string(),
            "P1DT30M0.5S"
        );
        assert_eq!(
            Duration::in_seconds(&a, &b).unwrap().to_string(),
            "PT24H30M0.5S"
        );
        assert_eq!(
            Duration::in_days(&a, &b).unwrap().to_string(),
            "P1DT30M0.5S"
        );
        let year = Temporal::Date(date("2027-12-01"));
        assert_eq!(Duration::in_months(&a, &year).unwrap().to_string(), "P1Y");
        assert_eq!(
            Duration::in_days(
                &Temporal::Date(date("2026-01-01")),
                &Temporal::Date(date("2026-11-27"))
            )
            .unwrap()
            .component("days"),
            Some(330)
        );
    }

    #[test]
    fn duration_components_are_totals_and_remainders() {
        let d = Temporal::Duration(duration("P1Y2M10DT1H30M15.25S"));
        for (name, value) in [
            ("years", 1),
            ("months", 14),
            ("monthsOfYear", 2),
            ("weeks", 1),
            ("days", 10),
            ("daysOfWeek", 3),
            ("hours", 1),
            ("minutes", 90),
            ("minutesOfHour", 30),
            ("seconds", 5415),
            ("secondsOfMinute", 15),
            ("milliseconds", 5_415_250),
            ("millisecondsOfSecond", 250),
        ] {
            assert_eq!(d.component(name), Some(value), "{name}");
        }
        assert_eq!(d.component("year"), None);
    }

    #[test]
    fn durations_add_scale_and_order() {
        let sum = duration("P1DT12H").plus(&duration("PT12H0.5S")).unwrap();
        assert_eq!(sum.to_string(), "P1DT24H0.5S");
        assert_eq!(
            duration("P1D").minus(&duration("P1D")).unwrap().to_string(),
            "PT0S"
        );
        assert_eq!(duration("PT1H").times(1.5).unwrap().to_string(), "PT1H30M");
        assert_eq!(duration("P1D").times(0.5).unwrap().to_string(), "PT12H");
        assert_eq!(duration("P2D").times(-1.0).unwrap().to_string(), "P-2D");
        assert!(duration("P1D").times(f64::NAN).is_err());
        assert!(duration("P1D") < duration("PT25H"));
        assert!(duration("P1M") > duration("P30D"));
        assert_eq!(duration("PT60M"), duration("PT1H"));
        assert_ne!(duration("P1D"), duration("PT24H"));
    }

    #[test]
    fn kinds_compare_with_their_own_kind_only() {
        let day = Temporal::Date(date("2026-11-27"));
        let moment = Temporal::DateTime(datetime("2026-11-27T00:00:00Z"));
        assert_eq!(
            day.compare(&Temporal::Date(date("2026-11-28"))),
            Some(Ordering::Less)
        );
        assert_eq!(day.compare(&moment), None);
        assert_eq!(
            moment.parse_same_kind("2026-11-27T02:00:00+02:00"),
            Some(moment)
        );
        assert_eq!(day.parse_same_kind("soon"), None);
    }

    #[test]
    fn out_of_range_values_are_errors_not_panics() {
        assert!(Date::from_ymd(1_000_000, 1, 1).is_err());
        assert!(date("9999-12-31").plus(&duration("P990001Y")).is_err());
        assert!(Duration::new(i64::MAX, 0, 0, 0).is_err());
        assert!(Duration::parse("P99999999999999999999D").is_err());
        assert!(DateTime::from_epoch(i64::MAX, 0).is_err());
        assert!(datetime("2026-11-27T10:00:00Z")
            .plus(&Duration::new(0, 0, 366 * 2_000_000 * 86_400, 0).unwrap())
            .is_err());
    }
}
