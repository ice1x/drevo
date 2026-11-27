//! Date, datetime and duration values (#866).
//!
//! `date('2026-11-27')`, `datetime('…')` and `duration('P1D')` are values of
//! their own kind: they compare and sort as moments in time, have components
//! (`.year`), and a duration shifts a date.
//!
//! The scenario is a CBT journal asking for the entries of the last week.

use std::collections::HashMap;

use drevo::cypher::executor::{execute_on_engine as execute, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

fn try_run(source: &str, db: &NativeGraph) -> Result<ExecResult, String> {
    let query = parse(source).map_err(|e| e.to_string())?;
    execute(&query, db, HashMap::new()).map_err(|e| e.to_string())
}

fn run(source: &str, db: &NativeGraph) -> ExecResult {
    try_run(source, db).unwrap_or_else(|e| panic!("`{source}` failed: {e}"))
}

fn rows(source: &str, db: &NativeGraph) -> Vec<Vec<Value>> {
    run(source, db).rows
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn i(n: i64) -> Value {
    Value::Integer(n)
}

fn one(source: &str) -> Value {
    let db = NativeGraph::new();
    let mut got = rows(source, &db);
    assert_eq!(got.len(), 1, "`{source}` returns one row");
    got.remove(0).remove(0)
}

#[test]
fn a_date_is_built_from_a_string_and_has_components() {
    assert_eq!(
        one("RETURN toString(date('2026-11-27')) AS d"),
        s("2026-11-27")
    );
    assert_eq!(one("RETURN date('2026-11-27').year AS y"), i(2026));
    assert_eq!(one("RETURN date('2026-11-27').month AS m"), i(11));
    assert_eq!(one("RETURN date('2026-11-27').day AS d"), i(27));
}

#[test]
fn a_date_is_built_from_a_map() {
    assert_eq!(
        one("RETURN toString(date({year: 2026, month: 11, day: 27})) AS d"),
        s("2026-11-27")
    );
}

#[test]
fn a_datetime_is_built_from_a_string() {
    assert_eq!(
        one("RETURN datetime('2026-11-27T10:00:00Z').epochMillis AS ms"),
        i(1_795_773_600_000)
    );
    assert_eq!(
        one("RETURN datetime('2026-11-27T12:00:00+02:00').epochMillis AS ms"),
        i(1_795_773_600_000)
    );
    assert_eq!(
        one("RETURN datetime('2026-11-27T10:00:00Z').hour AS h"),
        i(10)
    );
}

#[test]
fn now_has_components() {
    match one("RETURN datetime().year AS y") {
        Value::Integer(year) => assert!(year >= 2026, "the current year, got {year}"),
        other => panic!("datetime().year is an integer, got {other:?}"),
    }
    match one("RETURN date().year AS y") {
        Value::Integer(year) => assert!(year >= 2026),
        other => panic!("date().year is an integer, got {other:?}"),
    }
}

#[test]
fn a_duration_shifts_a_date() {
    assert_eq!(
        one("RETURN toString(date('2026-11-27') + duration('P5D')) AS d"),
        s("2026-12-02")
    );
    assert_eq!(
        one("RETURN toString(date('2026-03-01') - duration({days: 1})) AS d"),
        s("2026-02-28")
    );
}

#[test]
fn dates_compare_as_dates() {
    assert_eq!(
        one("RETURN date('2026-11-20') < date('2026-11-27') AS earlier"),
        Value::Bool(true)
    );
    assert_eq!(
        one("RETURN date('2026-11-27') = date({year: 2026, month: 11, day: 27}) AS same"),
        Value::Bool(true)
    );
}

#[test]
fn the_difference_of_two_dates_is_a_duration() {
    assert_eq!(
        one("RETURN duration.between(date('2026-01-01'), date('2026-11-27')).months AS m"),
        i(10)
    );
    assert_eq!(
        one("RETURN duration.between(date('2026-01-01'), date('2026-11-27')).days AS d"),
        i(26)
    );
}

#[test]
fn journal_entries_filter_and_sort_by_date() {
    let db = NativeGraph::new();
    run(
        "CREATE (:Entry {title: 'old', day: '2026-11-02'}), \
                (:Entry {title: 'mid', day: '2026-11-21'}), \
                (:Entry {title: 'new', day: '2026-11-26'})",
        &db,
    );
    let got = rows(
        "MATCH (e:Entry) \
         WHERE date(e.day) >= date('2026-11-27') - duration('P7D') \
         RETURN e.title AS t ORDER BY date(e.day) DESC",
        &db,
    );
    assert_eq!(got, vec![vec![s("new")], vec![s("mid")]]);
}

#[test]
fn arithmetic_on_a_datetime_with_a_number_is_an_error() {
    let db = NativeGraph::new();
    let got = try_run("RETURN datetime() + 1 AS wrong", &db);
    assert!(
        got.is_err(),
        "not a string concatenation: {:?}",
        got.map(|r| r.rows)
    );
}

#[test]
fn a_date_that_does_not_exist_is_an_error_naming_the_value() {
    let db = NativeGraph::new();
    let err = try_run("RETURN date('2026-02-30') AS d", &db).expect_err("no 30 February");
    assert!(
        err.contains("2026-02-30"),
        "the error names the value: {err}"
    );
}

#[test]
fn a_datetime_is_built_from_a_map_or_an_epoch() {
    assert_eq!(
        one(
            "RETURN toString(datetime({year: 2026, month: 11, day: 27, hour: 12, \
             timezone: '+02:00'})) AS d"
        ),
        s("2026-11-27T12:00:00.000+02:00")
    );
    assert_eq!(
        one("RETURN toString(datetime({epochMillis: 1795773600000})) AS d"),
        s("2026-11-27T10:00:00.000Z")
    );
    assert_eq!(one("RETURN datetime({epochSeconds: 0}).year AS y"), i(1970));
}

#[test]
fn a_duration_is_built_from_a_map_and_has_components() {
    assert_eq!(
        one("RETURN toString(duration({days: 1, hours: 2, minutes: 30})) AS d"),
        s("P1DT2H30M")
    );
    assert_eq!(
        one("RETURN toString(duration({weeks: 1.5})) AS d"),
        s("P10DT12H")
    );
    assert_eq!(one("RETURN duration('P1Y2M').months AS m"), i(14));
    assert_eq!(one("RETURN duration('PT90M').minutesOfHour AS m"), i(30));
    assert_eq!(one("RETURN duration('PT90M').year AS none"), Value::Null);
}

#[test]
fn durations_add_and_scale() {
    assert_eq!(
        one("RETURN toString(duration('P1D') + duration('PT12H')) AS d"),
        s("P1DT12H")
    );
    assert_eq!(one("RETURN toString(duration('PT1H') * 3) AS d"), s("PT3H"));
    assert_eq!(
        one("RETURN toString(duration('PT1H') / 2) AS d"),
        s("PT30M")
    );
    assert_eq!(
        one("RETURN duration('P1D') < duration('PT25H') AS shorter"),
        Value::Bool(true)
    );
}

#[test]
fn a_datetime_shifts_and_compares_across_offsets() {
    assert_eq!(
        one("RETURN toString(datetime('2026-01-31T22:30:00+02:00') + duration('P1MT2H')) AS d"),
        s("2026-03-01T00:30:00.000+02:00")
    );
    assert_eq!(
        one("RETURN datetime('2026-11-27T10:00:00Z') = datetime('2026-11-27T12:00:00+02:00') AS same"),
        Value::Bool(true)
    );
    assert_eq!(
        one("RETURN datetime('2026-11-27T10:00:00Z').timezone AS z"),
        s("Z")
    );
    assert_eq!(
        one(
            "RETURN duration.inSeconds(datetime('2026-11-27T10:00:00Z'), \
             datetime('2026-11-27T12:30:00+02:00')).minutes AS m"
        ),
        i(30)
    );
    assert_eq!(
        one("RETURN duration.inDays(date('2026-01-01'), date('2026-11-27')).days AS d"),
        i(330)
    );
}

#[test]
fn a_stored_datetime_is_text_and_still_compares_as_a_moment() {
    let db = NativeGraph::new();
    run(
        "CREATE (:Task {title: 'old', created_at: datetime('2026-11-20T09:00:00Z'), \
                        due: date('2026-11-25')}), \
                (:Task {title: 'new', created_at: datetime('2026-11-27T09:00:00+02:00'), \
                        due: date('2026-12-05')})",
        &db,
    );
    // The property holds the ISO 8601 text.
    let got = rows(
        "MATCH (t:Task {title: 'old'}) RETURN t.created_at AS at, t.due AS due",
        &db,
    );
    assert_eq!(
        got,
        vec![vec![s("2026-11-20T09:00:00.000Z"), s("2026-11-25")]]
    );
    // A datetime compares with the stored text as a moment, whatever the
    // offset it was written in.
    let got = rows(
        "MATCH (t:Task) WHERE t.created_at > datetime('2026-11-27T06:00:00Z') \
         RETURN t.title AS title",
        &db,
    );
    assert_eq!(got, vec![vec![s("new")]]);
    let got = rows(
        "MATCH (t:Task) WHERE t.due = date('2026-11-25') RETURN t.title AS title",
        &db,
    );
    assert_eq!(got, vec![vec![s("old")]]);
    let got = rows(
        "MATCH (t:Task) WHERE date('2026-12-01') < t.due RETURN t.title AS title",
        &db,
    );
    assert_eq!(got, vec![vec![s("new")]]);
    let got = rows(
        "MATCH (t:Task) \
         RETURN t.title AS title, \
                duration.between(t.created_at, datetime('2026-11-27T09:00:00Z')).days AS age \
         ORDER BY datetime(t.created_at)",
        &db,
    );
    assert_eq!(got, vec![vec![s("old"), i(7)], vec![s("new"), i(0)]]);
}

#[test]
fn text_functions_take_a_temporal_value_as_its_text() {
    assert_eq!(
        one("RETURN substring(toString(date('2026-11-27')), 0, 7) AS month"),
        s("2026-11")
    );
    assert_eq!(
        one("RETURN left(datetime('2026-11-27T10:00:00Z'), 10) AS day"),
        s("2026-11-27")
    );
    assert_eq!(
        one("RETURN datetime('2026-11-27T10:00:00Z') STARTS WITH '2026' AS this_year"),
        Value::Bool(true)
    );
    assert_eq!(
        one("RETURN 'due ' + date('2026-11-27') AS label"),
        s("due 2026-11-27")
    );
    assert_eq!(one("RETURN size(datetime()) AS length"), i(24));
}

#[test]
fn dates_group_sort_and_deduplicate() {
    let db = NativeGraph::new();
    run(
        "UNWIND ['2026-11-27', '2026-11-20', '2026-11-27'] AS day CREATE (:Entry {day: day})",
        &db,
    );
    let got = rows(
        "MATCH (e:Entry) WITH date(e.day) AS day, count(*) AS entries ORDER BY date(e.day) \
         RETURN toString(day) AS day, entries",
        &db,
    );
    assert_eq!(
        got,
        vec![vec![s("2026-11-20"), i(1)], vec![s("2026-11-27"), i(2)]]
    );
    let got = rows(
        "MATCH (e:Entry) WITH DISTINCT date(e.day) AS day \
         RETURN toString(max(day)) AS last, count(day) AS days",
        &db,
    );
    assert_eq!(got, vec![vec![s("2026-11-27"), i(2)]]);
    let got = rows(
        "MATCH (e:Entry) WHERE date(e.day) IN [date('2026-11-20')] RETURN count(e) AS c",
        &db,
    );
    assert_eq!(got, vec![vec![i(1)]]);
}

#[test]
fn null_and_wrong_arguments() {
    assert_eq!(one("RETURN date(null) AS d"), Value::Null);
    assert_eq!(one("RETURN date('2026-11-27') + null AS d"), Value::Null);
    assert_eq!(one("RETURN date('2026-11-27').hour AS none"), Value::Null);
    let db = NativeGraph::new();
    for (query, part) in [
        ("RETURN date(5) AS d", "expected a String"),
        (
            "RETURN date({year: 2026, hour: 1}) AS d",
            "unknown key `hour`",
        ),
        ("RETURN date({month: 1}) AS d", "`year`"),
        (
            "RETURN datetime('2026-11-27T10:00:00[Europe/Kyiv]') AS d",
            "named time zones",
        ),
        (
            "RETURN datetime({year: 2026, timezone: 'Europe/Kyiv'}) AS d",
            "offset",
        ),
        ("RETURN duration('1 day') AS d", "1 day"),
        ("RETURN duration({days: 'one'}) AS d", "`days`"),
        ("RETURN duration({months: 1.5}) AS d", "`months`"),
        (
            "RETURN date('2026-11-27') - date('2026-11-20') AS d",
            "duration.between",
        ),
        ("RETURN date('2026-11-27') * 2 AS d", "Duration"),
        ("RETURN date('2026-11-27') < datetime() AS d", "comparable"),
        (
            "RETURN duration.between(date('2026-01-01'), 5) AS d",
            "a Date or a DateTime",
        ),
        (
            "RETURN date('9999-12-31') + duration({years: 999999}) AS d",
            "range",
        ),
    ] {
        let err = try_run(query, &db).expect_err(query);
        assert!(err.contains(part), "`{query}`: {err}");
    }
}

mod through_the_service {
    //! The same comparisons where the property index narrows the scan.
    use super::*;

    use drevo::native_service::NativeService;

    fn titles(svc: &NativeService, query: &str) -> Vec<Value> {
        let parsed = parse(query).unwrap_or_else(|e| panic!("`{query}`: {e}"));
        svc.execute(&parsed, HashMap::new())
            .unwrap_or_else(|e| panic!("`{query}`: {e}"))
            .rows
            .into_iter()
            .map(|mut row| row.remove(0))
            .collect()
    }

    fn tasks() -> NativeService {
        let svc = NativeService::in_memory();
        titles(
            &svc,
            "CREATE (:Task {title: 'utc', at: '2026-11-27T10:00:00.000Z', due: '2026-11-25'}), \
                    (:Task {title: 'kyiv', at: '2026-11-27T12:00:00+02:00', due: '20261125'}), \
                    (:Task {title: 'later', at: datetime('2026-11-28T08:00:00Z'), \
                            due: date('2026-12-05')})",
        );
        svc
    }

    #[test]
    fn equality_finds_every_text_that_reads_as_the_value() {
        let svc = tasks();
        let moment = "datetime('2026-11-27T10:00:00Z')";
        for query in [
            format!("MATCH (t:Task) WHERE t.at = {moment} RETURN t.title ORDER BY t.title"),
            format!("MATCH (t:Task {{at: {moment}}}) RETURN t.title ORDER BY t.title"),
            format!("MATCH (t:Task) WHERE t.at IN [{moment}] RETURN t.title ORDER BY t.title"),
        ] {
            assert_eq!(titles(&svc, &query), vec![s("kyiv"), s("utc")], "{query}");
        }
        assert_eq!(
            titles(
                &svc,
                "MATCH (t:Task) WHERE t.due = date('2026-11-25') RETURN t.title ORDER BY t.title"
            ),
            vec![s("kyiv"), s("utc")]
        );
    }

    #[test]
    fn a_range_over_stored_texts_compares_moments() {
        let svc = tasks();
        assert_eq!(
            titles(
                &svc,
                "MATCH (t:Task) WHERE t.at > datetime('2026-11-27T10:00:00Z') RETURN t.title"
            ),
            vec![s("later")]
        );
        assert_eq!(
            titles(
                &svc,
                "MATCH (t:Task) WHERE t.due <= date('2026-11-25') + duration('P1D') \
                 RETURN t.title ORDER BY t.title"
            ),
            vec![s("kyiv"), s("utc")]
        );
    }

    #[test]
    fn the_stored_value_is_the_iso_text() {
        let svc = tasks();
        assert_eq!(
            titles(&svc, "MATCH (t:Task {title: 'later'}) RETURN t.at"),
            vec![s("2026-11-28T08:00:00.000Z")]
        );
        assert_eq!(
            titles(
                &svc,
                "MATCH (t:Task {title: 'later'}) SET t.done = date('2026-12-01') RETURN t.done"
            ),
            vec![s("2026-12-01")]
        );
    }
}

#[test]
fn a_procedure_takes_a_temporal_value_as_its_text() {
    let db = NativeGraph::new();
    run(
        "CREATE (a:Person {name: 'Alice'})-[:RELATED_TO {valid_from: '2020-01-01T00:00:00.000Z'}]->\
                (:Org {name: 'Acme'})",
        &db,
    );
    // `asOf` is read as a timestamp text; an expression of temporal values
    // is computed first and handed over as text.
    for as_of in [
        "datetime()",
        "datetime() - duration('PT1H')",
        "datetime('2026-11-27T10:00:00Z')",
    ] {
        let got = rows(
            &format!(
                "MATCH (a {{name: 'Alice'}}) \
                 CALL drevo.graph.expand(a, {{relTypes: ['RELATED_TO'], asOf: {as_of}, \
                 includeSeeds: false}}) YIELD node RETURN node.name AS name"
            ),
            &db,
        );
        assert_eq!(got, vec![vec![s("Acme")]], "{as_of}");
    }
    // After the call the query has temporal values again.
    let got = rows(
        "CALL db.labels() YIELD label WITH count(label) AS labels \
         RETURN date('2026-11-27').year + labels AS n",
        &db,
    );
    assert_eq!(got, vec![vec![i(2028)]]);
}

#[test]
fn temporal_values_pass_through_a_subquery_and_name_their_columns() {
    let db = NativeGraph::new();
    let got = run(
        "UNWIND ['2026-11-27', '2026-12-31'] AS day \
         CALL (day) { RETURN date(day) + duration('P1D') AS next } \
         RETURN next.year, toString(next) ORDER BY next",
        &db,
    );
    assert_eq!(got.columns, ["next.year", "toString(next)"]);
    assert_eq!(
        got.rows,
        vec![
            vec![i(2026), s("2026-11-28")],
            vec![i(2027), s("2027-01-01")]
        ]
    );
    // A procedure inside a subquery still gets text; the query around it
    // keeps its values.
    let got = rows(
        "WITH datetime('2026-11-27T10:00:00Z') AS at \
         CALL (at) { CALL db.labels() YIELD label RETURN count(label) AS labels } \
         RETURN at.hour + labels AS n",
        &db,
    );
    assert_eq!(got, vec![vec![i(10)]]);
}
