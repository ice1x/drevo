//! Node titles are not unique (issue #595): any number of nodes may share a
//! title, as in Neo4j and Memgraph. Covered here across the surfaces that
//! used to reject a repeat — Cypher, the service API, JSON and GraphML
//! import, full-text search and a durable reopen — plus graphbook's case of an
//! embedded note named like an external one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::model::NewNode;
use drevo::native_service::NativeService;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        HashMap::new(),
    )
    .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn count(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("expected an integer, got {other:?}"),
    }
}

fn note(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: body.into(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

#[test]
fn cypher_create_and_set_accept_a_repeated_title() {
    let svc = NativeService::in_memory();
    run_ok(&svc, "CREATE (:Person {title: 'Ann'})");
    run_ok(
        &svc,
        "CREATE (:Person {title: 'Ann'}), (:Pet {title: 'Ann'})",
    );
    assert_eq!(count(&svc, "MATCH (n {title: 'Ann'}) RETURN count(n)"), 3);
    run_ok(&svc, "CREATE (:Person {title: 'Bob'})");
    run_ok(&svc, "MATCH (n:Person {title: 'Bob'}) SET n.title = 'Ann'");
    assert_eq!(
        count(&svc, "MATCH (n:Person {title: 'Ann'}) RETURN count(n)"),
        3
    );
}

#[test]
fn merge_matches_existing_nodes_instead_of_failing() {
    // MERGE on a title creates once, then matches every node with it.
    let svc = NativeService::in_memory();
    run_ok(&svc, "MERGE (:Tag {title: 'rust'})");
    run_ok(&svc, "MERGE (:Tag {title: 'rust'})");
    assert_eq!(count(&svc, "MATCH (t:Tag) RETURN count(t)"), 1);
    // A note named like the tag is a different node; MERGE on the tag
    // label still finds only the tag.
    run_ok(&svc, "CREATE (:Note {title: 'rust'})");
    run_ok(&svc, "MERGE (:Tag {title: 'rust'})");
    assert_eq!(count(&svc, "MATCH (n {title: 'rust'}) RETURN count(n)"), 2);
}

#[test]
fn graphbook_embedded_note_shares_the_external_note_name() {
    // graphbook keeps an external note and an embedded copy under the same
    // name, linked to each other.
    let svc = NativeService::in_memory();
    let external = svc
        .create_node(note("note", "Project X", "external text"))
        .unwrap();
    let embedded = svc
        .create_node(note("embedded", "Project X", "embedded text"))
        .unwrap();
    run_ok(
        &svc,
        &format!(
            "MATCH (a), (b) WHERE id(a) = {} AND id(b) = {} CREATE (a)-[:EMBEDS]->(b)",
            external.id, embedded.id
        ),
    );
    let ids: Vec<u64> = svc
        .nodes_by_title("Project X")
        .iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(ids, vec![external.id, embedded.id]);
    assert_eq!(
        svc.get_node_by_title("Project X").map(|n| n.id),
        Some(external.id)
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (:note {title: 'Project X'})-[:EMBEDS]->(:embedded {title: 'Project X'}) \
             RETURN count(*)"
        ),
        1
    );
    // Full-text search finds both.
    let hits: Vec<u64> = svc
        .search_fts("Project X", 10)
        .iter()
        .map(|s| s.node.id)
        .collect();
    assert!(
        hits.contains(&external.id) && hits.contains(&embedded.id),
        "{hits:?}"
    );
    // Removing the external note leaves the embedded one findable by name.
    run_ok(
        &svc,
        &format!("MATCH (n) WHERE id(n) = {} DETACH DELETE n", external.id),
    );
    assert_eq!(
        svc.get_node_by_title("Project X").map(|n| n.id),
        Some(embedded.id)
    );
}

#[test]
fn json_and_graphml_round_trip_keep_both_nodes() {
    let src = NativeService::in_memory();
    src.create_node(note("note", "Same", "one")).unwrap();
    src.create_node(note("note", "Same", "two")).unwrap();

    let json = src.export_json().unwrap();
    let dst = NativeService::in_memory();
    dst.import_json(&json).unwrap();
    assert_eq!(dst.nodes_by_title("Same").len(), 2);

    let graphml = src.export_graphml().unwrap();
    let dst = NativeService::in_memory();
    dst.import_graphml(&graphml).unwrap();
    assert_eq!(dst.nodes_by_title("Same").len(), 2);
}

#[test]
fn importing_a_title_that_already_exists_adds_a_node() {
    // The dump's node has id 2; the target already holds id 1 with the same
    // title. Ids do not collide, titles do — and that is fine.
    let src = NativeService::in_memory();
    let filler = src.create_node(note("note", "filler", "")).unwrap();
    src.create_node(note("note", "Shared", "from the dump"))
        .unwrap();
    src.delete_node(filler.id).unwrap();
    let json = src.export_json().unwrap();

    let dst = NativeService::in_memory();
    dst.create_node(note("note", "Shared", "already here"))
        .unwrap();
    dst.import_json(&json).unwrap();
    let bodies: Vec<String> = dst
        .nodes_by_title("Shared")
        .into_iter()
        .map(|n| n.body)
        .collect();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies.contains(&"already here".to_string()));
}

#[test]
fn shared_titles_survive_a_durable_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_shared_titles_svc_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE (:Entry {title: 'Monday'}), (:Entry {title: 'Monday'})",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(svc.nodes_by_title("Monday").len(), 2);
    assert_eq!(
        count(&svc, "MATCH (e:Entry {title: 'Monday'}) RETURN count(e)"),
        2
    );
    let _ = std::fs::remove_dir_all(&dir);
}
