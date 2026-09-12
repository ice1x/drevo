//! `CALL drevo.cypher.fromText(prompt)` — text-to-Cypher proxy procedure
//! (issue #429), exercised end-to-end through the Cypher executor with a **fake**
//! generator (no network / no LLM).
//!
//! Verifies the two paths a client sees: a clear "not configured" error when no
//! upstream is installed, and — once a generator is installed — a single
//! `cypher` row carrying the generated query, with the live graph schema handed
//! to the generator so the model queries the graph that actually exists.

use std::collections::HashMap;
use std::sync::Arc;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::text2cypher::{install, CypherGenerator, Text2CypherError};

/// A network-free generator: echoes what it was handed so the test can assert
/// both that the prompt was forwarded and that the schema reached the model.
struct FakeGenerator;
impl CypherGenerator for FakeGenerator {
    fn generate(&self, system: &str, question: &str) -> Result<String, Text2CypherError> {
        Ok(format!(
            "rels_livesin={};has_schema={};q={question}",
            system.contains("LIVES_IN"),
            system.contains("Schema:"),
        ))
    }
}

fn seed_schema(d: &Drevo) {
    let q = parse("CREATE (:Person {name: 'ada'})-[:LIVES_IN]->(:City {name: 'nyc'})")
        .expect("parse seed");
    execute(&q, d, HashMap::new()).expect("seed");
}

const CALL: &str = "CALL drevo.cypher.fromText('find all people') YIELD cypher RETURN cypher";

#[test]
fn from_text_is_a_known_procedure_but_reports_not_configured_until_installed() {
    // NOTE: the generator is a process-global. This is the only test in this
    // binary that touches it, and it asserts the not-configured path *before*
    // installing, so there is no intra-binary ordering hazard.
    let d = Drevo::open_in_memory().expect("open");
    seed_schema(&d);

    let q = parse(CALL).expect("parse call");
    let err = execute(&q, &d, HashMap::new()).expect_err("must error when not configured");
    assert!(
        err.to_string().to_lowercase().contains("not configured"),
        "expected a clear not-configured error, got: {err}"
    );

    // Install the fake generator and re-run: one `cypher` row with the schema
    // forwarded to the generator.
    install(Arc::new(FakeGenerator));
    let result = execute(&q, &d, HashMap::new()).expect("configured call succeeds");
    assert_eq!(result.columns, vec!["cypher".to_string()]);
    assert_eq!(result.rows.len(), 1, "exactly one cypher row");
    match &result.rows[0][0] {
        Value::String(s) => {
            assert!(
                s.contains("rels_livesin=true"),
                "the live relationship type must be in the schema prompt: {s}"
            );
            assert!(
                s.contains("has_schema=true"),
                "a schema block must be sent: {s}"
            );
            assert!(
                s.contains("q=find all people"),
                "the NL prompt must be forwarded verbatim: {s}"
            );
        }
        other => panic!("expected a cypher string, got {other:?}"),
    }
}
