//! Trigram text indexes through Cypher (issue #589):
//! `CREATE TEXT INDEX … FOR (n:Label) ON (n.prop)`, `SHOW INDEXES`,
//! `DROP INDEX`, and `WHERE n.prop CONTAINS / STARTS WITH / ENDS WITH …`
//! served by the index.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo::cypher::executor::{
    execute_on_engine_with_context, ExecError, ExecResult, NativeQueryContext, Value,
};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;
use drevo::native_service::NativeService;
use drevo::native_text_index::{NativeTextIndex, TextIndexSpec};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn run_with(
    svc: &NativeService,
    q: &str,
    params: HashMap<String, Value>,
) -> Result<ExecResult, ExecError> {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        params,
    )
}

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    run_with(svc, q, HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn strings(res: &ExecResult) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// IT task manager: tickets with a key and a summary.
fn tracker() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Ticket {title: 'Crash on save', key: 'CORE-101'}), \
                (:Ticket {title: 'Save button misaligned', key: 'UI-7'}), \
                (:Ticket {title: 'autosave loses edits', key: 'CORE-102'}), \
                (:Note {title: 'save the date', key: 'CORE-1'})",
    );
    svc
}

// ----- results through the index ---------------------------------------------

#[test]
fn text_index_answers_contains_starts_and_ends_with() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE TEXT INDEX ticket_title FOR (t:Ticket) ON (t.title)",
    );
    run_ok(
        &svc,
        "CREATE TEXT INDEX ticket_key FOR (t:Ticket) ON (t.key)",
    );
    let titles = |q: &str| strings(&run_ok(&svc, q));
    assert_eq!(
        titles("MATCH (t:Ticket) WHERE t.title CONTAINS 'save' RETURN t.title AS x ORDER BY x"),
        ["Crash on save", "autosave loses edits"]
    );
    assert_eq!(
        titles("MATCH (t:Ticket) WHERE t.key STARTS WITH 'CORE-' RETURN t.title AS x ORDER BY x"),
        ["Crash on save", "autosave loses edits"]
    );
    assert_eq!(
        titles("MATCH (t:Ticket) WHERE t.key ENDS WITH '7' RETURN t.title AS x"),
        ["Save button misaligned"]
    );
    let by_param = run_with(
        &svc,
        "MATCH (t:Ticket) WHERE t.title CONTAINS $q RETURN t.title AS x",
        HashMap::from([("q".to_string(), s("misaligned"))]),
    )
    .expect("param needle");
    assert_eq!(strings(&by_param), ["Save button misaligned"]);
}

#[test]
fn results_after_writes_include_the_changes() {
    let svc = tracker();
    run_ok(&svc, "CREATE TEXT INDEX FOR (t:Ticket) ON (t.title)");
    run_ok(
        &svc,
        "CREATE (:Ticket {title: 'save dialog freezes', key: 'UI-8'})",
    );
    run_ok(
        &svc,
        "MATCH (t:Ticket {key: 'CORE-101'}) SET t.title = 'Crash on load'",
    );
    run_ok(&svc, "MATCH (t:Ticket {key: 'CORE-102'}) DETACH DELETE t");
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (t:Ticket) WHERE t.title CONTAINS 'save' RETURN t.title AS x"
        )),
        ["save dialog freezes"]
    );
}

#[test]
fn a_non_string_value_raises_the_same_error_as_without_the_index() {
    let svc = tracker();
    run_ok(&svc, "CREATE TEXT INDEX FOR (t:Ticket) ON (t.key)");
    run_ok(&svc, "CREATE (:Ticket {title: 'numeric key', key: 404})");
    let plain = tracker();
    run_ok(&plain, "CREATE (:Ticket {title: 'numeric key', key: 404})");
    let q = "MATCH (t:Ticket) WHERE t.key CONTAINS 'CORE' RETURN t.title AS x";
    let with_index = run(&svc, q).expect_err("type error");
    let without = run(&plain, q).expect_err("type error");
    assert_eq!(with_index.to_string(), without.to_string());
}

// ----- the index is really used ----------------------------------------------

/// A graph plus a text index synced *before* one more matching node is
/// created: a query that narrows through the index misses that node, a scan
/// finds it. That difference is how these tests see the index being used.
struct StaleProbe {
    graph: NativeGraph,
    texts: NativeTextIndex,
}

impl StaleProbe {
    fn new(specs: Vec<TextIndexSpec>) -> Self {
        let mut probe = Self {
            graph: NativeGraph::new(),
            texts: NativeTextIndex::new(specs),
        };
        probe.exec("CREATE (:Doc {title: 'indexed chapter', code: 'CH-1'})");
        probe.texts.sync(&probe.graph);
        probe.exec("CREATE (:Doc {title: 'late chapter', code: 'CH-1'})");
        probe
    }

    fn exec(&self, q: &str) -> ExecResult {
        let ctx = NativeQueryContext {
            texts: Some(&self.texts),
            ..NativeQueryContext::default()
        };
        execute_on_engine_with_context(&parse(q).expect("parse"), &self.graph, &ctx, HashMap::new())
            .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
    }

    fn titles(&self, q: &str) -> Vec<String> {
        let mut t = strings(&self.exec(q));
        t.sort();
        t
    }
}

fn spec(label: Option<&str>, path: &[&str]) -> TextIndexSpec {
    TextIndexSpec::new(
        label.map(str::to_string),
        path.iter().map(|p| (*p).to_string()).collect(),
    )
    .expect("valid spec")
}

#[test]
fn substring_predicates_narrow_through_the_index() {
    let p = StaleProbe::new(vec![spec(Some("Doc"), &["code"])]);
    for q in [
        "MATCH (n:Doc) WHERE n.code CONTAINS 'H-1' RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.code STARTS WITH 'C' RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.code ENDS WITH '1' RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.code CONTAINS 'CH-' AND n.title <> 'x' RETURN n.title AS t",
    ] {
        assert_eq!(
            p.titles(q),
            ["indexed chapter"],
            "{q} should narrow through the index"
        );
    }
}

#[test]
fn uncovered_shapes_scan() {
    let p = StaleProbe::new(vec![spec(Some("Doc"), &["code"])]);
    for q in [
        // No label: the Doc-only index has not seen other nodes.
        "MATCH (n) WHERE n.code CONTAINS 'CH-' RETURN n.title AS t",
        // A property the index does not declare.
        "MATCH (n:Doc) WHERE n.title CONTAINS 'chapter' RETURN n.title AS t",
        // Under OR / NOT the term is not required.
        "MATCH (n:Doc) WHERE n.code CONTAINS 'CH-' OR n.title = 'x' RETURN n.title AS t",
        "MATCH (n:Doc) WHERE NOT n.code CONTAINS 'zz' RETURN n.title AS t",
        // Too short to form a trigram.
        "MATCH (n:Doc) WHERE n.code CONTAINS 'CH' RETURN n.title AS t",
        // The property is the needle, not the haystack.
        "MATCH (n:Doc) WHERE 'CH-1 and more' CONTAINS n.code RETURN n.title AS t",
    ] {
        assert_eq!(
            p.titles(q),
            ["indexed chapter", "late chapter"],
            "{q} must not narrow"
        );
    }
}

// ----- DDL ---------------------------------------------------------------------

#[test]
fn show_indexes_lists_text_indexes() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE TEXT INDEX ticket_title IF NOT EXISTS FOR (t:Ticket) ON (t.title)",
    );
    run_ok(&svc, "CREATE TEXT INDEX FOR (n) ON (n.meta.author)");
    run_ok(
        &svc,
        "CREATE INDEX nested FOR (n:Ticket) ON (n.meta.severity)",
    );
    let res = run_ok(
        &svc,
        "SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, properties, indexProvider \
         RETURN name, type, entityType, labelsOrTypes, properties, indexProvider ORDER BY name",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("nested"),
                s("RANGE"),
                s("NODE"),
                Value::List(vec![s("Ticket")]),
                Value::List(vec![s("meta.severity")]),
                s("range-1.0"),
            ],
            vec![
                s("text_index_all_meta_author"),
                s("TEXT"),
                s("NODE"),
                Value::List(vec![]),
                Value::List(vec![s("meta.author")]),
                s("text-2.0"),
            ],
            vec![
                s("ticket_title"),
                s("TEXT"),
                s("NODE"),
                Value::List(vec![s("Ticket")]),
                Value::List(vec![s("title")]),
                s("text-2.0"),
            ],
        ]
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "SHOW TEXT INDEXES YIELD name RETURN name ORDER BY name"
        )),
        ["text_index_all_meta_author", "ticket_title"]
    );
    assert_eq!(
        strings(&run_ok(&svc, "SHOW RANGE INDEXES YIELD name RETURN name")),
        ["nested"]
    );
}

#[test]
fn names_are_unique_across_index_kinds() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE INDEX shared FOR (n:Ticket) ON (n.meta.severity)",
    );
    let err =
        run(&svc, "CREATE TEXT INDEX shared FOR (t:Ticket) ON (t.title)").expect_err("name taken");
    assert!(err.to_string().contains("already exists"), "{err}");
    run_ok(
        &svc,
        "CREATE TEXT INDEX shared IF NOT EXISTS FOR (t:Ticket) ON (t.title)",
    );
    run_ok(&svc, "CREATE TEXT INDEX FOR (t:Ticket) ON (t.title)");
    let err = run(
        &svc,
        "CREATE VECTOR INDEX text_index_Ticket_title FOR (c:Chunk) ON (c.e)",
    )
    .expect_err("name taken by the text index");
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn drop_index_removes_a_text_index() {
    let svc = tracker();
    run_ok(&svc, "CREATE TEXT INDEX titles FOR (t:Ticket) ON (t.title)");
    run_ok(&svc, "DROP INDEX titles");
    assert!(run_ok(&svc, "SHOW INDEXES YIELD name RETURN name")
        .rows
        .is_empty());
    // Still answered, now by scanning.
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (t:Ticket) WHERE t.title ENDS WITH 'edits' RETURN t.title AS x"
        )),
        ["autosave loses edits"]
    );
}

#[test]
fn invalid_text_index_definitions_are_errors() {
    let svc = tracker();
    for q in [
        "CREATE TEXT INDEX two FOR (t:Ticket) ON (t.title, t.key)",
        "CREATE TEXT INDEX star FOR (t:Ticket) ON (t.meta.*)",
        "CREATE TEXT INDEX other FOR (t:Ticket) ON (x.title)",
    ] {
        assert!(parse(q).is_err(), "{q} should not parse");
    }
    // Relationship text indexes are accepted and ignored, as before.
    run_ok(&svc, "CREATE TEXT INDEX FOR ()-[r:LINKS]-() ON (r.note)");
    assert!(run_ok(&svc, "SHOW INDEXES YIELD name RETURN name")
        .rows
        .is_empty());
}

#[test]
fn text_index_ddl_accepts_options() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE TEXT INDEX opts FOR (t:Ticket) ON (t.title) OPTIONS {indexProvider: 'text-2.0'}",
    );
    assert_eq!(
        strings(&run_ok(&svc, "SHOW TEXT INDEXES YIELD name RETURN name")),
        ["opts"]
    );
}

#[test]
fn definitions_survive_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_text_idx_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE TEXT INDEX chapters FOR (c:Chapter) ON (c.text)",
        );
        run_ok(
            &svc,
            "CREATE (:Chapter {title: 'one', text: 'It was a bright cold day in April'}), \
                    (:Chapter {title: 'two', text: 'The clocks were striking thirteen'})",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(
        strings(&run_ok(&svc, "SHOW TEXT INDEXES YIELD name RETURN name")),
        ["chapters"]
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (c:Chapter) WHERE c.text CONTAINS 'clocks' RETURN c.title AS t"
        )),
        ["two"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- integration: a book editor ----------------------------------------------

#[test]
fn book_editor_finds_phrases_across_chapters() {
    // A story editor keeps scenes with their text; the author searches for a
    // character's name and for scenes opening or closing with a line.
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE TEXT INDEX scene_text FOR (s:Scene) ON (s.text)",
    );
    run_ok(
        &svc,
        "UNWIND range(1, 120) AS i \
         CREATE (:Scene {title: 'scene-' + toString(i), n: i, \
                 text: CASE i % 4 \
                   WHEN 0 THEN 'Marta opened the door. The end.' \
                   WHEN 1 THEN 'Rain again; Oleg waited.' \
                   WHEN 2 THEN 'Marta and Oleg argued until dawn' \
                   ELSE 'Nobody spoke' END})",
    );
    let count = |q: &str| run_ok(&svc, q).rows[0][0].clone();
    assert_eq!(
        count("MATCH (s:Scene) WHERE s.text CONTAINS 'Marta' RETURN count(s)"),
        Value::Integer(60)
    );
    assert_eq!(
        count("MATCH (s:Scene) WHERE s.text STARTS WITH 'Rain' RETURN count(s)"),
        Value::Integer(30)
    );
    assert_eq!(
        count("MATCH (s:Scene) WHERE s.text ENDS WITH 'The end.' AND s.n > 100 RETURN count(s)"),
        Value::Integer(5)
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (s:Scene) WHERE s.text CONTAINS 'Oleg' AND s.text CONTAINS 'dawn' \
             AND s.n < 10 RETURN s.title AS t ORDER BY s.n"
        )),
        ["scene-2", "scene-6"]
    );
}

// ----- differential: indexed answers equal scanned answers --------------------

mod differential {
    use super::*;
    use proptest::prelude::*;

    /// A value: mostly strings over a tiny alphabet so substrings collide,
    /// sometimes null, an integer or a list.
    fn value() -> impl Strategy<Value = String> {
        prop_oneof![
            8 => "[abё ]{0,7}".prop_map(|s| format!("'{s}'")),
            1 => Just("null".to_string()),
            1 => (0i64..3).prop_map(|i| i.to_string()),
            1 => Just("['ab']".to_string()),
        ]
    }

    fn rows(svc: &NativeService, q: &str) -> Result<Vec<String>, String> {
        match run(svc, q) {
            Ok(res) => {
                let mut v: Vec<String> = res.rows.iter().map(|r| format!("{r:?}")).collect();
                v.sort();
                Ok(v)
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn indexed_and_scanned_answers_agree(
            docs in proptest::collection::vec((any::<bool>(), value(), value()), 1..14),
            needle in "[abё ]{0,4}",
        ) {
            let indexed = NativeService::in_memory();
            let plain = NativeService::in_memory();
            run_ok(&indexed, "CREATE TEXT INDEX FOR (n:Doc) ON (n.code)");
            run_ok(&indexed, "CREATE TEXT INDEX FOR (n) ON (n.meta.v)");
            for (i, (is_doc, code, v)) in docs.iter().enumerate() {
                let label = if *is_doc { "Doc" } else { "Other" };
                let q = format!(
                    "CREATE (:{label} {{title: 't{i}', code: {code}, meta: {{v: {v}}}}})"
                );
                run_ok(&indexed, &q);
                run_ok(&plain, &q);
            }
            for op in ["CONTAINS", "STARTS WITH", "ENDS WITH"] {
                for q in [
                    format!("MATCH (n:Doc) WHERE n.code {op} '{needle}' RETURN n.title"),
                    format!("MATCH (n) WHERE n.meta.v {op} '{needle}' RETURN n.title"),
                    format!(
                        "MATCH (n:Doc) WHERE n.code {op} '{needle}' AND n.meta.v IS NOT NULL \
                         RETURN n.title"
                    ),
                ] {
                    // Non-string values make the index decline, so even the
                    // type errors match the scan exactly.
                    prop_assert_eq!(rows(&indexed, &q), rows(&plain, &q), "{}", q);
                }
            }
        }
    }
}
