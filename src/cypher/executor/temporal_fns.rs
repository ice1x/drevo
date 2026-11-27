//! The temporal functions of Cypher (`date`, `datetime`, `duration`,
//! `duration.between` and its relatives) and arithmetic on their values
//! (issue #866). The calendar itself is [`crate::cypher::temporal`]; this
//! module reads the arguments and reports the errors.

use std::collections::BTreeMap;

use super::{fn_err, ExecError, ExecResultT, Value};
use crate::cypher::ast::BinaryOp;
use crate::cypher::lexer::Span;
use crate::cypher::temporal::{Date, DateTime, Duration, Temporal, TimeOfDay};

/// `date()`, `date('2026-11-27')`, `date({year, month, day})`, or the date
/// of a datetime.
pub(super) fn scalar_date(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    let err = |why: String| fn_err("date", why, span);
    let date = match one_or_none("date", args, span)? {
        None => Date::today(),
        Some(Value::String(text)) => Date::parse(&text).map_err(err)?,
        Some(Value::Map(map)) => date_from_map("date", &map, span)?,
        Some(Value::Temporal(Temporal::Date(date))) => date,
        Some(Value::Temporal(Temporal::DateTime(moment))) => moment.date(),
        Some(other) => return Err(err(not_a(&other, "a String, a Map, a Date or a DateTime"))),
    };
    Ok(Value::Temporal(Temporal::Date(date)))
}

/// `datetime()`, `datetime('2026-11-27T10:00:00Z')`, `datetime({year, ...,
/// timezone})`, `datetime({epochMillis})`, or the midnight of a date.
pub(super) fn scalar_datetime(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    let err = |why: String| fn_err("datetime", why, span);
    let moment = match one_or_none("datetime", args, span)? {
        None => DateTime::now(),
        Some(Value::String(text)) => DateTime::parse(&text).map_err(err)?,
        Some(Value::Map(map)) => datetime_from_map(&map, span)?,
        Some(Value::Temporal(Temporal::DateTime(moment))) => moment,
        Some(Value::Temporal(Temporal::Date(date))) => {
            DateTime::from_parts(date, TimeOfDay::default(), 0).map_err(err)?
        }
        Some(other) => return Err(err(not_a(&other, "a String, a Map, a Date or a DateTime"))),
    };
    Ok(Value::Temporal(Temporal::DateTime(moment)))
}

/// `duration('P1DT2H')` or `duration({days: 1, hours: 2})`.
pub(super) fn scalar_duration(args: Vec<Value>, span: Span) -> ExecResultT<Value> {
    let err = |why: String| fn_err("duration", why, span);
    let duration = match one_or_none("duration", args, span)? {
        Some(Value::String(text)) => Duration::parse(&text).map_err(err)?,
        Some(Value::Map(map)) => duration_from_map(&map, span)?,
        Some(Value::Temporal(Temporal::Duration(duration))) => duration,
        Some(other) => return Err(err(not_a(&other, "a String, a Map or a Duration"))),
        None => return Err(err("expected 1 argument(s), got 0".into())),
    };
    Ok(Value::Temporal(Temporal::Duration(duration)))
}

/// `duration.between(a, b)`, `duration.inMonths`, `duration.inDays`,
/// `duration.inSeconds`: the time from `a` to `b`; `name` is the function as
/// written, `how` the calculation.
pub(super) fn scalar_duration_between(
    name: &str,
    how: fn(&Temporal, &Temporal) -> Option<Duration>,
    args: Vec<Value>,
    span: Span,
) -> ExecResultT<Value> {
    let [from, to] = args.as_slice() else {
        return Err(fn_err(
            name,
            format!("expected 2 argument(s), got {}", args.len()),
            span,
        ));
    };
    let moment = |value: &Value| match value {
        Value::Temporal(t @ (Temporal::Date(_) | Temporal::DateTime(_))) => Ok(*t),
        // A stored datetime or date is its ISO text.
        Value::String(text) => DateTime::parse(text)
            .map(Temporal::DateTime)
            .map_err(|why| fn_err(name, why, span)),
        other => Err(fn_err(name, not_a(other, "a Date or a DateTime"), span)),
    };
    how(&moment(from)?, &moment(to)?)
        .map(|d| Value::Temporal(Temporal::Duration(d)))
        .ok_or_else(|| fn_err(name, "the duration is out of range", span))
}

/// A component of a temporal value read as a property: `.year`,
/// `.epochMillis`, `.days`. `null` for a name the value does not have, like
/// a property a node does not have.
pub(super) fn component(value: &Temporal, name: &str) -> Value {
    if matches!(name, "timezone" | "offset") {
        return value.offset_text().map_or(Value::Null, Value::String);
    }
    value.component(name).map_or(Value::Null, Value::Integer)
}

/// `+`, `-`, `*` and `/` with a temporal value on either side. `None` when
/// neither side is one, so the caller goes on with numbers.
pub(super) fn arith(
    op: BinaryOp,
    lhs: &Value,
    rhs: &Value,
    span: Span,
) -> Option<ExecResultT<Value>> {
    use Temporal::{Date as D, DateTime as T, Duration as P};
    let (a, b) = (as_temporal(lhs), as_temporal(rhs));
    if a.is_none() && b.is_none() {
        return None;
    }
    let number = |v: &Value| match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    let result: Option<Result<Temporal, String>> = match (op, a, b) {
        (BinaryOp::Add, Some(D(d)), Some(P(p))) | (BinaryOp::Add, Some(P(p)), Some(D(d))) => {
            Some(d.plus(&p).map(D))
        }
        (BinaryOp::Sub, Some(D(d)), Some(P(p))) => Some(d.minus(&p).map(D)),
        (BinaryOp::Add, Some(T(t)), Some(P(p))) | (BinaryOp::Add, Some(P(p)), Some(T(t))) => {
            Some(t.plus(&p).map(T))
        }
        (BinaryOp::Sub, Some(T(t)), Some(P(p))) => Some(t.minus(&p).map(T)),
        (BinaryOp::Add, Some(P(x)), Some(P(y))) => Some(x.plus(&y).map(P)),
        (BinaryOp::Sub, Some(P(x)), Some(P(y))) => Some(x.minus(&y).map(P)),
        (BinaryOp::Mul, Some(P(p)), None) => number(rhs).map(|n| p.times(n).map(P)),
        (BinaryOp::Mul, None, Some(P(p))) => number(lhs).map(|n| p.times(n).map(P)),
        (BinaryOp::Div, Some(P(p)), None) => number(rhs).map(|n| {
            if n == 0.0 {
                Err("a duration cannot be divided by zero".into())
            } else {
                p.times(1.0 / n).map(P)
            }
        }),
        _ => None,
    };
    Some(match result {
        Some(Ok(value)) => Ok(Value::Temporal(value)),
        Some(Err(why)) => Err(ExecError::TypeMismatch {
            expected: "a result within the range of dates and durations".into(),
            got: why,
            span,
        }),
        None => {
            let between = matches!(
                (op, a, b),
                (BinaryOp::Sub, Some(D(_) | T(_)), Some(D(_) | T(_)))
            );
            Err(ExecError::TypeMismatch {
                expected: if between {
                    "duration.between(a, b) for the time between two dates".into()
                } else {
                    "a Date or DateTime with a Duration (+, -), two Durations (+, -), \
                     or a Duration with a number (*, /)"
                        .into()
                },
                got: format!("{} {op:?} {}", lhs.type_name(), rhs.type_name()),
                span,
            })
        }
    })
}

/// How a temporal value and another value are ordered. A string on the
/// other side is read as the same kind of value (a stored datetime is its
/// ISO text); a text that is not one is compared as text, as before
/// temporal values existed. `None` when the two cannot be compared.
pub(super) fn compare(lhs: &Value, rhs: &Value) -> Option<std::cmp::Ordering> {
    match (lhs, rhs) {
        (Value::Temporal(a), Value::Temporal(b)) => a.compare(b),
        (Value::Temporal(a), Value::String(text)) => Some(match a.parse_same_kind(text) {
            Some(b) => a.compare(&b)?,
            None => a.to_string().as_str().cmp(text.as_str()),
        }),
        (Value::String(_), Value::Temporal(_)) => {
            compare(rhs, lhs).map(std::cmp::Ordering::reverse)
        }
        _ => None,
    }
}

fn as_temporal(value: &Value) -> Option<Temporal> {
    match value {
        Value::Temporal(t) => Some(*t),
        _ => None,
    }
}

fn not_a(value: &Value, wanted: &str) -> String {
    format!("expected {wanted}, got {}", value.type_name())
}

fn one_or_none(name: &str, mut args: Vec<Value>, span: Span) -> ExecResultT<Option<Value>> {
    if args.len() > 1 {
        return Err(fn_err(
            name,
            format!("expected at most 1 argument, got {}", args.len()),
            span,
        ));
    }
    Ok(args.pop())
}

/// The whole number under `key`, `default` when the map has no such key.
fn whole(
    func: &str,
    map: &BTreeMap<String, Value>,
    key: &str,
    default: i64,
    span: Span,
) -> ExecResultT<i64> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Integer(i)) => Ok(*i),
        Some(other) => Err(fn_err(
            func,
            format!("`{key}` must be an Integer, got {}", other.type_name()),
            span,
        )),
    }
}

/// Refuse a key the constructor does not know, by name.
fn only_keys(
    func: &str,
    map: &BTreeMap<String, Value>,
    known: &[&str],
    span: Span,
) -> ExecResultT<()> {
    match map.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(fn_err(
            func,
            format!("unknown key `{key}`; the keys are {}", known.join(", ")),
            span,
        )),
        None => Ok(()),
    }
}

const DATE_KEYS: [&str; 3] = ["year", "month", "day"];

/// The date of `{year, month, day}`; `month` and `day` default to 1.
/// `datetime` checks the keys of its own map itself.
fn date_from_map(func: &str, map: &BTreeMap<String, Value>, span: Span) -> ExecResultT<Date> {
    if func == "date" {
        only_keys(func, map, &DATE_KEYS, span)?;
    }
    if !map.contains_key("year") {
        return Err(fn_err(func, "the map needs a `year`", span));
    }
    Date::from_ymd(
        whole(func, map, "year", 0, span)?,
        whole(func, map, "month", 1, span)?,
        whole(func, map, "day", 1, span)?,
    )
    .map_err(|why| fn_err(func, why, span))
}

fn datetime_from_map(map: &BTreeMap<String, Value>, span: Span) -> ExecResultT<DateTime> {
    const FUNC: &str = "datetime";
    let err = |why: String| fn_err(FUNC, why, span);
    let nanos = whole(FUNC, map, "millisecond", 0, span)?
        .checked_mul(1_000_000)
        .and_then(|n| {
            n.checked_add(
                whole(FUNC, map, "microsecond", 0, span)
                    .ok()?
                    .checked_mul(1000)?,
            )
        })
        .and_then(|n| n.checked_add(whole(FUNC, map, "nanosecond", 0, span).ok()?))
        .ok_or_else(|| err("the fraction of a second is out of range".into()))?;
    let offset = match map.get("timezone") {
        None | Some(Value::Null) => 0,
        Some(Value::String(zone)) => {
            // An offset is read the way it is written after a time.
            DateTime::parse(&format!("1970-01-01T00:00{zone}"))
                .map_err(|_| {
                    err(format!(
                        "`timezone` must be an offset such as +02:00 or Z, got `{zone}`"
                    ))
                })?
                .offset_seconds()
        }
        Some(other) => return Err(err(not_a(other, "a String for `timezone`"))),
    };
    if map.contains_key("epochSeconds") || map.contains_key("epochMillis") {
        only_keys(
            FUNC,
            map,
            &[
                "epochSeconds",
                "epochMillis",
                "millisecond",
                "microsecond",
                "nanosecond",
                "timezone",
            ],
            span,
        )?;
        let secs = whole(FUNC, map, "epochSeconds", 0, span)?;
        let millis = whole(FUNC, map, "epochMillis", 0, span)?;
        let moment = DateTime::from_epoch(
            secs.checked_add(millis.div_euclid(1000))
                .ok_or_else(|| err("the datetime is out of range".into()))?,
            millis.rem_euclid(1000) * 1_000_000 + nanos,
        )
        .map_err(err)?;
        return moment.at_offset(offset).map_err(err);
    }
    only_keys(
        FUNC,
        map,
        &[
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "millisecond",
            "microsecond",
            "nanosecond",
            "timezone",
        ],
        span,
    )?;
    let date = date_from_map(FUNC, map, span)?;
    let time = TimeOfDay {
        hour: whole(FUNC, map, "hour", 0, span)?,
        minute: whole(FUNC, map, "minute", 0, span)?,
        second: whole(FUNC, map, "second", 0, span)?,
        nanos,
    };
    DateTime::from_parts(date, time, offset).map_err(err)
}

fn duration_from_map(map: &BTreeMap<String, Value>, span: Span) -> ExecResultT<Duration> {
    const FUNC: &str = "duration";
    /// Each key with what one of it is: months, days, or nanoseconds.
    const UNITS: [(&str, i64, i64, i64); 10] = [
        ("years", 12, 0, 0),
        ("months", 1, 0, 0),
        ("weeks", 0, 7, 0),
        ("days", 0, 1, 0),
        ("hours", 0, 0, 3_600_000_000_000),
        ("minutes", 0, 0, 60_000_000_000),
        ("seconds", 0, 0, 1_000_000_000),
        ("milliseconds", 0, 0, 1_000_000),
        ("microseconds", 0, 0, 1000),
        ("nanoseconds", 0, 0, 1),
    ];
    let known: Vec<&str> = UNITS.iter().map(|unit| unit.0).collect();
    only_keys(FUNC, map, &known, span)?;
    let range = || fn_err(FUNC, "the duration is out of range", span);
    let (mut months, mut days, mut nanos) = (0_i64, 0_i64, 0_i128);
    for (key, in_months, in_days, in_nanos) in UNITS {
        match map.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Integer(n)) => {
                let add = |total: i64, scale: i64| {
                    n.checked_mul(scale).and_then(|v| total.checked_add(v))
                };
                months = add(months, in_months).ok_or_else(range)?;
                days = add(days, in_days).ok_or_else(range)?;
                nanos += i128::from(*n) * i128::from(in_nanos);
            }
            // A fraction of a day or of a shorter unit goes into the seconds.
            Some(Value::Float(f)) if in_months == 0 && f.is_finite() => {
                let (whole_days, in_nanos) = if in_days != 0 {
                    let in_all = f * in_days as f64;
                    (in_all.trunc(), in_all.fract() * 86_400e9)
                } else {
                    (0.0, f * in_nanos as f64)
                };
                if whole_days.abs() > 1e12 || in_nanos.abs() > 1e27 {
                    return Err(range());
                }
                days = days.checked_add(whole_days as i64).ok_or_else(range)?;
                nanos += in_nanos.round() as i128;
            }
            Some(other) => {
                return Err(fn_err(
                    FUNC,
                    format!(
                        "`{key}` must be {}, got {}",
                        if in_months == 0 {
                            "a number"
                        } else {
                            "an Integer"
                        },
                        match other {
                            Value::Float(_) => "a Float".to_string(),
                            other => other.type_name().to_string(),
                        }
                    ),
                    span,
                ))
            }
        }
    }
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000)).map_err(|_| range())?;
    let rest = nanos.rem_euclid(1_000_000_000) as i64;
    Duration::new(months, days, secs, rest).map_err(|why| fn_err(FUNC, why, span))
}
