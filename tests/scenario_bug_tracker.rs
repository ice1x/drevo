//! Scenario integration test: Bug Tracker / Control System
//!
//! This test validates GraphNote DB against a real-world bug-tracker use case.
//!
//! Domain model:
//! - Node kinds: `bug`, `feature`, `release`, `test_case`, `assignee`
//! - Edge kinds: `reported_in`, `fixed_by`, `verified_by`, `blocks_release`,
//!   plus `part_of` for feature → release delivery links
//!
//! Tested workflows:
//! - Building a bug board with assignees, releases, features, test cases, and bugs
//! - Bug → Release (reported_in): finding all bugs reported in a release
//! - Bug → Assignee (fixed_by): developer workload and fix attribution
//! - Bug → TestCase (verified_by): coverage / regression linkage
//! - Bug → Release (blocks_release): release-blocker queries
//! - Feature → Release (part_of): release contents
//! - Kind index: list views (all bugs, all releases, ...)
//! - FTS: searching bugs / features by title or description
//! - Subgraph: full context of a bug (release, fixer, test, blocked release)
//! - Shortest path: bug → release via different relations
//! - Edge properties: severity, priority, resolved-at timestamps
//! - Transactional updates: status transitions, reassignment
//! - Cascade delete: removing a release cleans up all incident edges

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::*;
use std::collections::HashMap;

// =========================================================================
// Test helpers
// =========================================================================

fn memory_db() -> GraphNoteDb {
    GraphNoteDb::open_in_memory().expect("open in-memory DB")
}

#[cfg(feature = "redb-backend")]
fn redb_db(dir: &tempfile::TempDir) -> GraphNoteDb {
    let path = dir.path().join("bug_tracker_test.db");
    GraphNoteDb::open(&path).expect("open redb DB")
}

fn make_node_with_props(
    kind: &str,
    title: &str,
    body: &str,
    props: HashMap<String, serde_json::Value>,
) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::from(props),
    }
}

fn make_edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn make_edge_with_props(
    from_id: u64,
    to_id: u64,
    kind: &str,
    props: HashMap<String, serde_json::Value>,
) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::from(props),
    }
}

fn bug_props(severity: &str, status: &str, priority: u32) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("severity".to_string(), serde_json::json!(severity));
    p.insert("status".to_string(), serde_json::json!(status));
    p.insert("priority".to_string(), serde_json::json!(priority));
    p
}

fn release_props(version: &str, status: &str) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("version".to_string(), serde_json::json!(version));
    p.insert("status".to_string(), serde_json::json!(status));
    p
}

fn assignee_props(role: &str, email: &str) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("role".to_string(), serde_json::json!(role));
    p.insert("email".to_string(), serde_json::json!(email));
    p
}

fn feature_props(area: &str, state: &str) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("area".to_string(), serde_json::json!(area));
    p.insert("state".to_string(), serde_json::json!(state));
    p
}

fn test_case_props(kind: &str, automated: bool) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("kind".to_string(), serde_json::json!(kind));
    p.insert("automated".to_string(), serde_json::json!(automated));
    p
}

fn fixed_by_props(resolved_at: i64, commits: u32) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("resolved_at".to_string(), serde_json::json!(resolved_at));
    p.insert("commits".to_string(), serde_json::json!(commits));
    p
}

fn verified_by_props(run_count: u32, last_status: &str) -> HashMap<String, serde_json::Value> {
    let mut p = HashMap::new();
    p.insert("run_count".to_string(), serde_json::json!(run_count));
    p.insert("last_status".to_string(), serde_json::json!(last_status));
    p
}

// =========================================================================
// Domain: Bug Tracker board
// =========================================================================

/// A complete bug-tracker graph with all created node IDs.
///
/// ```text
/// releases:  v1.0.0, v1.1.0, v2.0.0
/// assignees: Alice, Bob, Carol
/// features:  Login, Search, Export, Settings
/// tests:     TC-LOGIN-1, TC-SEARCH-1, TC-EXPORT-1, TC-LOGIN-2
/// bugs:      BUG-1 .. BUG-5
///
/// part_of (feature → release):
///   Login    → v1.0.0
///   Search   → v1.1.0
///   Export   → v1.1.0
///   Settings → v2.0.0
///
/// reported_in (bug → release):
///   BUG-1 → v1.0.0    BUG-2 → v1.1.0
///   BUG-3 → v1.1.0    BUG-4 → v1.0.0
///   BUG-5 → v1.0.0
///
/// fixed_by (bug → assignee):
///   BUG-1 → Alice     BUG-2 → Bob
///   BUG-3 → Carol     BUG-4 → Alice
///   BUG-5 → Bob
///
/// verified_by (bug → test_case):
///   BUG-1 → TC-LOGIN-1
///   BUG-2 → TC-SEARCH-1
///   BUG-3 → TC-EXPORT-1
///   BUG-4 → TC-LOGIN-2
///   (BUG-5 has no verifying test)
///
/// blocks_release (bug → release):
///   BUG-1 → v1.1.0
///   BUG-3 → v2.0.0
/// ```
struct BugBoard {
    // Releases
    rel_v1_0: u64,
    rel_v1_1: u64,
    rel_v2_0: u64,
    // Assignees
    dev_alice: u64,
    dev_bob: u64,
    dev_carol: u64,
    // Features
    feat_login: u64,
    feat_search: u64,
    feat_export: u64,
    feat_settings: u64,
    // Test cases
    tc_login_1: u64,
    tc_search_1: u64,
    tc_export_1: u64,
    tc_login_2: u64,
    // Bugs
    bug_1: u64,
    bug_2: u64,
    bug_3: u64,
    bug_4: u64,
    bug_5: u64,
}

fn build_bug_board(db: &GraphNoteDb) -> BugBoard {
    // --- Releases ---
    let rel_v1_0 = db
        .create_node(make_node_with_props(
            "release",
            "Release v1.0.0",
            "Initial public release with core authentication and settings functionality",
            release_props("1.0.0", "released"),
        ))
        .unwrap();
    let rel_v1_1 = db
        .create_node(make_node_with_props(
            "release",
            "Release v1.1.0",
            "Minor release adding full-text search and PDF export capabilities",
            release_props("1.1.0", "released"),
        ))
        .unwrap();
    let rel_v2_0 = db
        .create_node(make_node_with_props(
            "release",
            "Release v2.0.0",
            "Major release introducing redesigned settings and mobile support",
            release_props("2.0.0", "planned"),
        ))
        .unwrap();

    // --- Assignees ---
    let dev_alice = db
        .create_node(make_node_with_props(
            "assignee",
            "Alice",
            "Senior frontend engineer focused on authentication flows",
            assignee_props("frontend", "alice@example.com"),
        ))
        .unwrap();
    let dev_bob = db
        .create_node(make_node_with_props(
            "assignee",
            "Bob",
            "Backend engineer maintaining search and settings services",
            assignee_props("backend", "bob@example.com"),
        ))
        .unwrap();
    let dev_carol = db
        .create_node(make_node_with_props(
            "assignee",
            "Carol",
            "QA specialist covering export pipelines and document rendering",
            assignee_props("qa", "carol@example.com"),
        ))
        .unwrap();

    // --- Features ---
    let feat_login = db
        .create_node(make_node_with_props(
            "feature",
            "Login",
            "User authentication via email and password with session cookies",
            feature_props("auth", "shipped"),
        ))
        .unwrap();
    let feat_search = db
        .create_node(make_node_with_props(
            "feature",
            "Search",
            "Full-text search across notes and attachments",
            feature_props("search", "shipped"),
        ))
        .unwrap();
    let feat_export = db
        .create_node(make_node_with_props(
            "feature",
            "Export",
            "Export notes to PDF with embedded fonts and images",
            feature_props("export", "shipped"),
        ))
        .unwrap();
    let feat_settings = db
        .create_node(make_node_with_props(
            "feature",
            "Settings",
            "User-level and workspace-level settings with persistence",
            feature_props("settings", "in_progress"),
        ))
        .unwrap();

    // --- Test cases ---
    let tc_login_1 = db
        .create_node(make_node_with_props(
            "test_case",
            "TC-LOGIN-1",
            "Regression: login with empty password must return a validation error",
            test_case_props("regression", true),
        ))
        .unwrap();
    let tc_search_1 = db
        .create_node(make_node_with_props(
            "test_case",
            "TC-SEARCH-1",
            "Regression: search returns unique results without duplicates",
            test_case_props("regression", true),
        ))
        .unwrap();
    let tc_export_1 = db
        .create_node(make_node_with_props(
            "test_case",
            "TC-EXPORT-1",
            "Regression: PDF export produces a valid non-corrupt file",
            test_case_props("regression", true),
        ))
        .unwrap();
    let tc_login_2 = db
        .create_node(make_node_with_props(
            "test_case",
            "TC-LOGIN-2",
            "UI: login error message spelling and punctuation",
            test_case_props("ui", false),
        ))
        .unwrap();

    // --- Bugs ---
    let bug_1 = db
        .create_node(make_node_with_props(
            "bug",
            "BUG-1",
            "Login crashes when password field is empty on Safari mobile",
            bug_props("critical", "fixed", 1),
        ))
        .unwrap();
    let bug_2 = db
        .create_node(make_node_with_props(
            "bug",
            "BUG-2",
            "Search returns duplicate results for tagged notes in the sidebar",
            bug_props("major", "fixed", 2),
        ))
        .unwrap();
    let bug_3 = db
        .create_node(make_node_with_props(
            "bug",
            "BUG-3",
            "PDF export corrupts files when note contains embedded SVG images",
            bug_props("critical", "fixed", 1),
        ))
        .unwrap();
    let bug_4 = db
        .create_node(make_node_with_props(
            "bug",
            "BUG-4",
            "Typo in login error message: 'passowrd' should be 'password'",
            bug_props("trivial", "fixed", 4),
        ))
        .unwrap();
    let bug_5 = db
        .create_node(make_node_with_props(
            "bug",
            "BUG-5",
            "Settings page occasionally fails to persist user preferences on logout",
            bug_props("major", "open", 2),
        ))
        .unwrap();

    // --- Edges: feature part_of release ---
    db.create_edge(make_edge(feat_login.id, rel_v1_0.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(feat_search.id, rel_v1_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(feat_export.id, rel_v1_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(feat_settings.id, rel_v2_0.id, "part_of"))
        .unwrap();

    // --- Edges: bug reported_in release ---
    db.create_edge(make_edge(bug_1.id, rel_v1_0.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(bug_2.id, rel_v1_1.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(bug_3.id, rel_v1_1.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(bug_4.id, rel_v1_0.id, "reported_in"))
        .unwrap();
    db.create_edge(make_edge(bug_5.id, rel_v1_0.id, "reported_in"))
        .unwrap();

    // --- Edges: bug fixed_by assignee (with resolved_at + commits) ---
    db.create_edge(make_edge_with_props(
        bug_1.id,
        dev_alice.id,
        "fixed_by",
        fixed_by_props(1_700_000_000, 3),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_2.id,
        dev_bob.id,
        "fixed_by",
        fixed_by_props(1_700_100_000, 2),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_3.id,
        dev_carol.id,
        "fixed_by",
        fixed_by_props(1_700_200_000, 5),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_4.id,
        dev_alice.id,
        "fixed_by",
        fixed_by_props(1_700_300_000, 1),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_5.id,
        dev_bob.id,
        "fixed_by",
        fixed_by_props(0, 0),
    ))
    .unwrap();

    // --- Edges: bug verified_by test_case ---
    db.create_edge(make_edge_with_props(
        bug_1.id,
        tc_login_1.id,
        "verified_by",
        verified_by_props(12, "pass"),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_2.id,
        tc_search_1.id,
        "verified_by",
        verified_by_props(7, "pass"),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_3.id,
        tc_export_1.id,
        "verified_by",
        verified_by_props(4, "pass"),
    ))
    .unwrap();
    db.create_edge(make_edge_with_props(
        bug_4.id,
        tc_login_2.id,
        "verified_by",
        verified_by_props(1, "pass"),
    ))
    .unwrap();

    // --- Edges: bug blocks_release ---
    db.create_edge(make_edge(bug_1.id, rel_v1_1.id, "blocks_release"))
        .unwrap();
    db.create_edge(make_edge(bug_3.id, rel_v2_0.id, "blocks_release"))
        .unwrap();

    BugBoard {
        rel_v1_0: rel_v1_0.id,
        rel_v1_1: rel_v1_1.id,
        rel_v2_0: rel_v2_0.id,
        dev_alice: dev_alice.id,
        dev_bob: dev_bob.id,
        dev_carol: dev_carol.id,
        feat_login: feat_login.id,
        feat_search: feat_search.id,
        feat_export: feat_export.id,
        feat_settings: feat_settings.id,
        tc_login_1: tc_login_1.id,
        tc_search_1: tc_search_1.id,
        tc_export_1: tc_export_1.id,
        tc_login_2: tc_login_2.id,
        bug_1: bug_1.id,
        bug_2: bug_2.id,
        bug_3: bug_3.id,
        bug_4: bug_4.id,
        bug_5: bug_5.id,
    }
}

// =========================================================================
// Tests — run against both backends via macro
// =========================================================================

macro_rules! bug_tracker_tests {
    ($db_expr:expr, $mod_name:ident) => {
        mod $mod_name {
            use super::*;

            // -----------------------------------------------------------------
            // 1. Basic graph construction
            // -----------------------------------------------------------------

            #[test]
            fn build_complete_bug_board() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                assert!(db.get_node(g.rel_v1_0).unwrap().is_some());
                assert!(db.get_node(g.rel_v1_1).unwrap().is_some());
                assert!(db.get_node(g.rel_v2_0).unwrap().is_some());
                assert!(db.get_node(g.dev_alice).unwrap().is_some());
                assert!(db.get_node(g.dev_bob).unwrap().is_some());
                assert!(db.get_node(g.dev_carol).unwrap().is_some());
                assert!(db.get_node(g.feat_login).unwrap().is_some());
                assert!(db.get_node(g.feat_search).unwrap().is_some());
                assert!(db.get_node(g.feat_export).unwrap().is_some());
                assert!(db.get_node(g.feat_settings).unwrap().is_some());
                assert!(db.get_node(g.tc_login_1).unwrap().is_some());
                assert!(db.get_node(g.tc_search_1).unwrap().is_some());
                assert!(db.get_node(g.tc_export_1).unwrap().is_some());
                assert!(db.get_node(g.tc_login_2).unwrap().is_some());
                assert!(db.get_node(g.bug_1).unwrap().is_some());
                assert!(db.get_node(g.bug_2).unwrap().is_some());
                assert!(db.get_node(g.bug_3).unwrap().is_some());
                assert!(db.get_node(g.bug_4).unwrap().is_some());
                assert!(db.get_node(g.bug_5).unwrap().is_some());
            }

            #[test]
            fn node_kinds_are_correct() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                assert_eq!(db.get_node(g.rel_v1_0).unwrap().unwrap().kind, "release");
                assert_eq!(db.get_node(g.dev_alice).unwrap().unwrap().kind, "assignee");
                assert_eq!(db.get_node(g.feat_login).unwrap().unwrap().kind, "feature");
                assert_eq!(
                    db.get_node(g.tc_login_1).unwrap().unwrap().kind,
                    "test_case"
                );
                assert_eq!(db.get_node(g.bug_1).unwrap().unwrap().kind, "bug");
            }

            #[test]
            fn bug_properties_stored() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let bug = db.get_node(g.bug_1).unwrap().unwrap();
                assert_eq!(
                    bug.properties.get("severity").unwrap(),
                    &serde_json::json!("critical")
                );
                assert_eq!(
                    bug.properties.get("status").unwrap(),
                    &serde_json::json!("fixed")
                );
                assert_eq!(
                    bug.properties.get("priority").unwrap(),
                    &serde_json::json!(1)
                );
            }

            #[test]
            fn release_properties_stored() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let rel = db.get_node(g.rel_v1_0).unwrap().unwrap();
                assert_eq!(
                    rel.properties.get("version").unwrap(),
                    &serde_json::json!("1.0.0")
                );
                assert_eq!(
                    rel.properties.get("status").unwrap(),
                    &serde_json::json!("released")
                );
            }

            #[test]
            fn assignee_properties_stored() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let alice = db.get_node(g.dev_alice).unwrap().unwrap();
                assert_eq!(
                    alice.properties.get("role").unwrap(),
                    &serde_json::json!("frontend")
                );
                assert_eq!(
                    alice.properties.get("email").unwrap(),
                    &serde_json::json!("alice@example.com")
                );
            }

            #[test]
            fn test_case_properties_stored() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let tc = db.get_node(g.tc_login_1).unwrap().unwrap();
                assert_eq!(
                    tc.properties.get("kind").unwrap(),
                    &serde_json::json!("regression")
                );
                assert_eq!(
                    tc.properties.get("automated").unwrap(),
                    &serde_json::json!(true)
                );
            }

            // -----------------------------------------------------------------
            // 2. Bug → Release (reported_in)
            // -----------------------------------------------------------------

            #[test]
            fn bug_reported_in_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let releases = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("reported_in"))
                    .unwrap();
                assert_eq!(releases.len(), 1);
                assert_eq!(releases[0].id, g.rel_v1_0);
            }

            #[test]
            fn release_has_multiple_reported_bugs() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v1.0.0 has BUG-1, BUG-4, BUG-5 reported against it
                let bugs = db
                    .neighbors(g.rel_v1_0, Direction::Incoming, Some("reported_in"))
                    .unwrap();
                assert_eq!(bugs.len(), 3);
                let ids: Vec<u64> = bugs.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_4));
                assert!(ids.contains(&g.bug_5));
            }

            #[test]
            fn release_with_two_reported_bugs() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let bugs = db
                    .neighbors(g.rel_v1_1, Direction::Incoming, Some("reported_in"))
                    .unwrap();
                assert_eq!(bugs.len(), 2);
                let ids: Vec<u64> = bugs.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.bug_2));
                assert!(ids.contains(&g.bug_3));
            }

            // -----------------------------------------------------------------
            // 3. Bug → Assignee (fixed_by)
            // -----------------------------------------------------------------

            #[test]
            fn bug_fixed_by_assignee() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let fixers = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("fixed_by"))
                    .unwrap();
                assert_eq!(fixers.len(), 1);
                assert_eq!(fixers[0].id, g.dev_alice);
            }

            #[test]
            fn assignee_fix_workload() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Alice fixed BUG-1 and BUG-4
                let bugs = db
                    .neighbors(g.dev_alice, Direction::Incoming, Some("fixed_by"))
                    .unwrap();
                assert_eq!(bugs.len(), 2);
                let ids: Vec<u64> = bugs.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_4));
            }

            #[test]
            fn fixed_by_edge_metadata() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let edges = db.edges_of(g.bug_3, Direction::Outgoing).unwrap();
                let fix = edges.iter().find(|e| e.kind == "fixed_by").unwrap();
                assert_eq!(fix.to_id, g.dev_carol);
                assert_eq!(
                    fix.properties.get("commits").unwrap(),
                    &serde_json::json!(5)
                );
                assert_eq!(
                    fix.properties.get("resolved_at").unwrap(),
                    &serde_json::json!(1_700_200_000i64)
                );
            }

            #[test]
            fn open_bug_has_no_resolution_time() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BUG-5 is still "open" — its fixed_by edge has resolved_at = 0
                let edges = db.edges_of(g.bug_5, Direction::Outgoing).unwrap();
                let fix = edges.iter().find(|e| e.kind == "fixed_by").unwrap();
                assert_eq!(
                    fix.properties.get("resolved_at").unwrap(),
                    &serde_json::json!(0)
                );
            }

            // -----------------------------------------------------------------
            // 4. Bug → TestCase (verified_by)
            // -----------------------------------------------------------------

            #[test]
            fn bug_verified_by_test_case() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let tests = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("verified_by"))
                    .unwrap();
                assert_eq!(tests.len(), 1);
                assert_eq!(tests[0].id, g.tc_login_1);
            }

            #[test]
            fn test_case_covers_bug() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // TC-LOGIN-1 verifies exactly BUG-1
                let covered = db
                    .neighbors(g.tc_login_1, Direction::Incoming, Some("verified_by"))
                    .unwrap();
                assert_eq!(covered.len(), 1);
                assert_eq!(covered[0].id, g.bug_1);
            }

            #[test]
            fn bug_without_verifying_test() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BUG-5 has no verified_by edge
                let tests = db
                    .neighbors(g.bug_5, Direction::Outgoing, Some("verified_by"))
                    .unwrap();
                assert!(tests.is_empty());
            }

            #[test]
            fn verified_by_edge_metadata() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let edges = db.edges_of(g.bug_1, Direction::Outgoing).unwrap();
                let verify = edges.iter().find(|e| e.kind == "verified_by").unwrap();
                assert_eq!(
                    verify.properties.get("run_count").unwrap(),
                    &serde_json::json!(12)
                );
                assert_eq!(
                    verify.properties.get("last_status").unwrap(),
                    &serde_json::json!("pass")
                );
            }

            // -----------------------------------------------------------------
            // 5. Bug → Release (blocks_release)
            // -----------------------------------------------------------------

            #[test]
            fn bug_blocks_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BUG-1 blocks v1.1.0
                let blocked = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("blocks_release"))
                    .unwrap();
                assert_eq!(blocked.len(), 1);
                assert_eq!(blocked[0].id, g.rel_v1_1);
            }

            #[test]
            fn release_with_blockers() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v2.0.0 is blocked by BUG-3
                let blockers = db
                    .neighbors(g.rel_v2_0, Direction::Incoming, Some("blocks_release"))
                    .unwrap();
                assert_eq!(blockers.len(), 1);
                assert_eq!(blockers[0].id, g.bug_3);
            }

            #[test]
            fn release_without_blockers() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v1.0.0 was the initial release — nothing blocks it
                let blockers = db
                    .neighbors(g.rel_v1_0, Direction::Incoming, Some("blocks_release"))
                    .unwrap();
                assert!(blockers.is_empty());
            }

            // -----------------------------------------------------------------
            // 6. Feature → Release (part_of)
            // -----------------------------------------------------------------

            #[test]
            fn feature_part_of_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let releases = db
                    .neighbors(g.feat_login, Direction::Outgoing, Some("part_of"))
                    .unwrap();
                assert_eq!(releases.len(), 1);
                assert_eq!(releases[0].id, g.rel_v1_0);
            }

            #[test]
            fn release_contents() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v1.1.0 contains Search and Export
                let features = db
                    .neighbors(g.rel_v1_1, Direction::Incoming, Some("part_of"))
                    .unwrap();
                assert_eq!(features.len(), 2);
                let ids: Vec<u64> = features.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.feat_search));
                assert!(ids.contains(&g.feat_export));
            }

            #[test]
            fn planned_release_features() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v2.0.0 ships with Settings
                let features = db
                    .neighbors(g.rel_v2_0, Direction::Incoming, Some("part_of"))
                    .unwrap();
                assert_eq!(features.len(), 1);
                assert_eq!(features[0].id, g.feat_settings);
            }

            // -----------------------------------------------------------------
            // 7. Kind index — list views
            // -----------------------------------------------------------------

            #[test]
            fn kind_index_lists_all_bugs() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                assert_eq!(bugs.len(), 5);
            }

            #[test]
            fn kind_index_lists_all_releases() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let releases = db.list_nodes_by_kind("release", 100, 0).unwrap();
                assert_eq!(releases.len(), 3);
            }

            #[test]
            fn kind_index_lists_all_assignees() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let devs = db.list_nodes_by_kind("assignee", 100, 0).unwrap();
                assert_eq!(devs.len(), 3);
            }

            #[test]
            fn kind_index_lists_all_features() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let features = db.list_nodes_by_kind("feature", 100, 0).unwrap();
                assert_eq!(features.len(), 4);
            }

            #[test]
            fn kind_index_lists_all_test_cases() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let tests = db.list_nodes_by_kind("test_case", 100, 0).unwrap();
                assert_eq!(tests.len(), 4);
            }

            #[test]
            fn kind_index_pagination() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let page1 = db.list_nodes_by_kind("bug", 2, 0).unwrap();
                assert_eq!(page1.len(), 2);
                let page2 = db.list_nodes_by_kind("bug", 2, 2).unwrap();
                assert_eq!(page2.len(), 2);
                let page3 = db.list_nodes_by_kind("bug", 2, 4).unwrap();
                assert_eq!(page3.len(), 1);
            }

            // -----------------------------------------------------------------
            // 8. FTS — search by title / description
            // -----------------------------------------------------------------

            #[test]
            fn fts_find_bug_by_title() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let results = db.search_fts("BUG-1", 10).unwrap();
                assert!(!results.is_empty());
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                assert!(titles.contains(&"BUG-1"));
            }

            #[test]
            fn fts_find_bug_by_description() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                // BUG-3 body mentions "SVG"
                let results = db.search_fts("SVG", 10).unwrap();
                assert!(!results.is_empty());
                let found = results.iter().any(|r| r.node.title == "BUG-3");
                assert!(found);
            }

            #[test]
            fn fts_find_bug_by_keyword_corrupts() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let results = db.search_fts("corrupts", 10).unwrap();
                assert!(!results.is_empty());
                let found = results.iter().any(|r| r.node.title == "BUG-3");
                assert!(found);
            }

            #[test]
            fn fts_find_feature_by_description() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let results = db.search_fts("authentication", 10).unwrap();
                assert!(!results.is_empty());
                let found = results.iter().any(|r| r.node.title == "Login");
                assert!(found);
            }

            #[test]
            fn fts_find_test_case_by_keyword() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let results = db.search_fts("regression", 10).unwrap();
                assert!(!results.is_empty());
                // All regression test cases should surface
                let matched: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                assert!(matched.iter().any(|t| t == &"TC-LOGIN-1"));
            }

            // -----------------------------------------------------------------
            // 9. Subgraph — full context of a bug
            // -----------------------------------------------------------------

            #[test]
            fn subgraph_of_bug_depth_1() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Depth 1 around BUG-1: reported_in release, fixed_by Alice,
                // verified_by TC-LOGIN-1, blocks_release v1.1.0.
                let sg = db.subgraph(g.bug_1, 1).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.rel_v1_0));
                assert!(ids.contains(&g.rel_v1_1));
                assert!(ids.contains(&g.dev_alice));
                assert!(ids.contains(&g.tc_login_1));
            }

            #[test]
            fn subgraph_of_release_sees_features_and_bugs() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Depth 1 around v1.0.0: Login feature (part_of),
                // BUG-1, BUG-4, BUG-5 (reported_in).
                let sg = db.subgraph(g.rel_v1_0, 1).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.rel_v1_0));
                assert!(ids.contains(&g.feat_login));
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_4));
                assert!(ids.contains(&g.bug_5));
            }

            #[test]
            fn subgraph_of_assignee_sees_fixed_bugs() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let sg = db.subgraph(g.dev_alice, 1).unwrap();
                let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(ids.contains(&g.dev_alice));
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_4));
            }

            // -----------------------------------------------------------------
            // 10. Traversals — BFS / DFS / shortest path
            // -----------------------------------------------------------------

            #[test]
            fn bfs_from_bug_all_outgoing() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BFS depth 1 from BUG-1 outgoing: release, assignee, test_case, blocked release
                let reached = db.bfs(g.bug_1, 1, Direction::Outgoing, None).unwrap();
                let ids: Vec<u64> = reached.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.rel_v1_0));
                assert!(ids.contains(&g.dev_alice));
                assert!(ids.contains(&g.tc_login_1));
                assert!(ids.contains(&g.rel_v1_1));
            }

            #[test]
            fn bfs_from_release_incoming_shows_bugs_and_features() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let reached = db.bfs(g.rel_v1_1, 1, Direction::Incoming, None).unwrap();
                let ids: Vec<u64> = reached.iter().map(|n| n.id).collect();
                // BUG-2, BUG-3 reported_in + BUG-1 blocks_release + Search/Export part_of
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_2));
                assert!(ids.contains(&g.bug_3));
                assert!(ids.contains(&g.feat_search));
                assert!(ids.contains(&g.feat_export));
            }

            #[test]
            fn bfs_filter_by_edge_kind() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Only reported_in edges from BUG-1 — one release, v1.0.0
                let reached = db
                    .bfs(g.bug_1, 1, Direction::Outgoing, Some("reported_in"))
                    .unwrap();
                let ids: Vec<u64> = reached.iter().map(|n| n.id).collect();
                assert_eq!(ids.len(), 1);
                assert!(ids.contains(&g.rel_v1_0));
                assert!(!ids.contains(&g.rel_v1_1));
            }

            #[test]
            fn dfs_from_assignee_fixed_bugs_only() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let bugs = db
                    .dfs(g.dev_alice, 10, Direction::Incoming, Some("fixed_by"))
                    .unwrap();
                let ids: Vec<u64> = bugs.iter().map(|n| n.id).collect();
                assert_eq!(bugs.len(), 2);
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_4));
            }

            #[test]
            fn shortest_path_bug_to_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let path = db.shortest_path(g.bug_1, g.rel_v1_0).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 2);
                assert_eq!(path[0], g.bug_1);
                assert_eq!(path[1], g.rel_v1_0);
            }

            #[test]
            fn shortest_path_bug_to_assignee() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let path = db.shortest_path(g.bug_3, g.dev_carol).unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 2);
                assert_eq!(path[0], g.bug_3);
                assert_eq!(path[1], g.dev_carol);
            }

            // -----------------------------------------------------------------
            // 11. Edge kind index
            // -----------------------------------------------------------------

            #[test]
            fn edge_kind_index_reported_in() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let edges = db.list_edges_by_kind("reported_in", 100, 0).unwrap();
                assert_eq!(edges.len(), 5);
            }

            #[test]
            fn edge_kind_index_fixed_by() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let edges = db.list_edges_by_kind("fixed_by", 100, 0).unwrap();
                assert_eq!(edges.len(), 5);
            }

            #[test]
            fn edge_kind_index_verified_by() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                // 4 bugs verified, BUG-5 has no test
                let edges = db.list_edges_by_kind("verified_by", 100, 0).unwrap();
                assert_eq!(edges.len(), 4);
            }

            #[test]
            fn edge_kind_index_blocks_release() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let edges = db.list_edges_by_kind("blocks_release", 100, 0).unwrap();
                assert_eq!(edges.len(), 2);
            }

            #[test]
            fn edge_kind_index_part_of() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let edges = db.list_edges_by_kind("part_of", 100, 0).unwrap();
                assert_eq!(edges.len(), 4);
            }

            // -----------------------------------------------------------------
            // 12. Transactional updates — status transitions, reassignment
            // -----------------------------------------------------------------

            #[test]
            fn mark_bug_as_verified() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let mut new_props = HashMap::new();
                new_props.insert("severity".to_string(), serde_json::json!("major"));
                new_props.insert("status".to_string(), serde_json::json!("verified"));
                new_props.insert("priority".to_string(), serde_json::json!(2));

                let updated = db
                    .update_node(
                        g.bug_5,
                        NodePatch {
                            kind: None,
                            title: None,
                            body: None,
                            body_html: None,
                            properties: Some(Properties::from(new_props)),
                        },
                    )
                    .unwrap();

                assert_eq!(
                    updated.properties.get("status").unwrap(),
                    &serde_json::json!("verified")
                );
            }

            #[test]
            fn reassign_bug_to_different_developer() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Remove existing fixed_by on BUG-5 and create a new one pointing to Carol
                let edges = db.edges_of(g.bug_5, Direction::Outgoing).unwrap();
                for e in edges.iter().filter(|e| e.kind == "fixed_by") {
                    db.delete_edge(e.id).unwrap();
                }
                db.create_edge(make_edge_with_props(
                    g.bug_5,
                    g.dev_carol,
                    "fixed_by",
                    fixed_by_props(1_700_400_000, 1),
                ))
                .unwrap();

                let fixers = db
                    .neighbors(g.bug_5, Direction::Outgoing, Some("fixed_by"))
                    .unwrap();
                assert_eq!(fixers.len(), 1);
                assert_eq!(fixers[0].id, g.dev_carol);
            }

            #[test]
            fn report_new_bug_against_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let new_bug = db
                    .create_node(make_node_with_props(
                        "bug",
                        "BUG-6",
                        "Export dialog fails to remember last chosen directory",
                        bug_props("minor", "open", 3),
                    ))
                    .unwrap();

                db.create_edge(make_edge(new_bug.id, g.rel_v1_1, "reported_in"))
                    .unwrap();

                // v1.1.0 now has 3 reported bugs (BUG-2, BUG-3, BUG-6)
                let bugs = db
                    .neighbors(g.rel_v1_1, Direction::Incoming, Some("reported_in"))
                    .unwrap();
                assert_eq!(bugs.len(), 3);
            }

            #[test]
            fn add_blocker_to_planned_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BUG-5 becomes a blocker for v2.0.0 during triage
                db.create_edge(make_edge(g.bug_5, g.rel_v2_0, "blocks_release"))
                    .unwrap();

                let blockers = db
                    .neighbors(g.rel_v2_0, Direction::Incoming, Some("blocks_release"))
                    .unwrap();
                assert_eq!(blockers.len(), 2);
                let ids: Vec<u64> = blockers.iter().map(|n| n.id).collect();
                assert!(ids.contains(&g.bug_3));
                assert!(ids.contains(&g.bug_5));
            }

            #[test]
            fn update_verification_run_count() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let edges = db.edges_of(g.bug_1, Direction::Outgoing).unwrap();
                let verify = edges.iter().find(|e| e.kind == "verified_by").unwrap();

                let mut new_props = HashMap::new();
                new_props.insert("run_count".to_string(), serde_json::json!(20));
                new_props.insert("last_status".to_string(), serde_json::json!("pass"));

                let updated = db
                    .update_edge(
                        verify.id,
                        EdgePatch {
                            kind: None,
                            weight: None,
                            properties: Some(Properties::from(new_props)),
                        },
                    )
                    .unwrap();

                assert_eq!(
                    updated.properties.get("run_count").unwrap(),
                    &serde_json::json!(20)
                );
            }

            // -----------------------------------------------------------------
            // 13. Cascade delete
            // -----------------------------------------------------------------

            #[test]
            fn delete_release_cascades_edges() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                db.delete_node(g.rel_v1_1).unwrap();
                assert!(db.get_node(g.rel_v1_1).unwrap().is_none());

                // BUG-1 no longer has a blocks_release edge to v1.1.0
                let blocked = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("blocks_release"))
                    .unwrap();
                assert!(blocked.is_empty());

                // BUG-2 and BUG-3 no longer have any reported_in edges
                let bug2_rel = db
                    .neighbors(g.bug_2, Direction::Outgoing, Some("reported_in"))
                    .unwrap();
                assert!(bug2_rel.is_empty());
                let bug3_rel = db
                    .neighbors(g.bug_3, Direction::Outgoing, Some("reported_in"))
                    .unwrap();
                assert!(bug3_rel.is_empty());

                // Search and Export no longer have part_of edges
                let search_rel = db
                    .neighbors(g.feat_search, Direction::Outgoing, Some("part_of"))
                    .unwrap();
                assert!(search_rel.is_empty());
            }

            #[test]
            fn delete_assignee_cascades_fix_edges() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                db.delete_node(g.dev_alice).unwrap();

                // BUG-1 and BUG-4 no longer have fixed_by edges
                let bug1_fix = db
                    .neighbors(g.bug_1, Direction::Outgoing, Some("fixed_by"))
                    .unwrap();
                assert!(bug1_fix.is_empty());
                let bug4_fix = db
                    .neighbors(g.bug_4, Direction::Outgoing, Some("fixed_by"))
                    .unwrap();
                assert!(bug4_fix.is_empty());
            }

            #[test]
            fn unblock_release_via_edge_deletion() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Remove the blocks_release edge from BUG-3 to v2.0.0
                let edges = db.edges_of(g.bug_3, Direction::Outgoing).unwrap();
                for e in edges.iter().filter(|e| e.kind == "blocks_release") {
                    db.delete_edge(e.id).unwrap();
                }

                let blockers = db
                    .neighbors(g.rel_v2_0, Direction::Incoming, Some("blocks_release"))
                    .unwrap();
                assert!(blockers.is_empty());
                // Bug itself still exists
                assert!(db.get_node(g.bug_3).unwrap().is_some());
            }

            // -----------------------------------------------------------------
            // 14. Advanced queries
            // -----------------------------------------------------------------

            #[test]
            fn find_bugs_by_status() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                let open: Vec<&Node> = bugs
                    .iter()
                    .filter(|b| {
                        b.properties
                            .get("status")
                            .map(|v| v == &serde_json::json!("open"))
                            .unwrap_or(false)
                    })
                    .collect();
                assert_eq!(open.len(), 1);
                assert_eq!(open[0].title, "BUG-5");
            }

            #[test]
            fn find_critical_bugs() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                let critical: Vec<&Node> = bugs
                    .iter()
                    .filter(|b| {
                        b.properties
                            .get("severity")
                            .map(|v| v == &serde_json::json!("critical"))
                            .unwrap_or(false)
                    })
                    .collect();
                assert_eq!(critical.len(), 2);
            }

            #[test]
            fn find_bugs_blocking_any_release() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                let blockers: Vec<&Node> = bugs
                    .iter()
                    .filter(|b| {
                        !db.neighbors(b.id, Direction::Outgoing, Some("blocks_release"))
                            .unwrap()
                            .is_empty()
                    })
                    .collect();
                assert_eq!(blockers.len(), 2);
                let ids: Vec<u64> = blockers.iter().map(|b| b.id).collect();
                assert!(ids.contains(&g.bug_1));
                assert!(ids.contains(&g.bug_3));
            }

            #[test]
            fn find_bugs_without_verifying_test() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                let uncovered: Vec<&Node> = bugs
                    .iter()
                    .filter(|b| {
                        db.neighbors(b.id, Direction::Outgoing, Some("verified_by"))
                            .unwrap()
                            .is_empty()
                    })
                    .collect();
                assert_eq!(uncovered.len(), 1);
                assert_eq!(uncovered[0].id, g.bug_5);
            }

            #[test]
            fn developer_resolution_commit_total() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Sum of commits across Alice's fixed_by edges: 3 (BUG-1) + 1 (BUG-4) = 4
                let edges = db.edges_of(g.dev_alice, Direction::Incoming).unwrap();
                let total: u64 = edges
                    .iter()
                    .filter(|e| e.kind == "fixed_by")
                    .map(|e| {
                        e.properties
                            .get("commits")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                    })
                    .sum();
                assert_eq!(total, 4);
            }

            // -----------------------------------------------------------------
            // 15. Node lookup
            // -----------------------------------------------------------------

            #[test]
            fn find_bug_by_title() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let node = db.get_node_by_title("BUG-1").unwrap();
                assert!(node.is_some());
                assert_eq!(node.unwrap().id, g.bug_1);
            }

            #[test]
            fn find_release_by_uuid() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let original = db.get_node(g.rel_v1_0).unwrap().unwrap();
                let by_uuid = db.get_node_by_uuid(&original.uuid).unwrap();
                assert!(by_uuid.is_some());
                assert_eq!(by_uuid.unwrap().id, g.rel_v1_0);
            }

            #[test]
            fn list_recent_returns_latest() {
                let db = $db_expr;
                let _g = build_bug_board(&db);

                let recent = db.list_recent(5).unwrap();
                assert_eq!(recent.len(), 5);
            }

            // -----------------------------------------------------------------
            // 16. Edge direction filtering
            // -----------------------------------------------------------------

            #[test]
            fn edges_of_bug_both_directions() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // BUG-1 outgoing: reported_in, fixed_by, verified_by, blocks_release (4)
                // BUG-1 incoming: none
                let all = db.edges_of(g.bug_1, Direction::Both).unwrap();
                assert_eq!(all.len(), 4);
            }

            #[test]
            fn edges_of_release_incoming() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // v1.1.0 incoming: 2 reported_in (BUG-2, BUG-3) + 1 blocks_release (BUG-1)
                //                  + 2 part_of (Search, Export) = 5
                let incoming = db.edges_of(g.rel_v1_1, Direction::Incoming).unwrap();
                assert_eq!(incoming.len(), 5);
                assert!(incoming.iter().all(|e| e.to_id == g.rel_v1_1));
            }

            #[test]
            fn edges_of_assignee_only_incoming_fixes() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                // Alice has no outgoing edges
                let outgoing = db.edges_of(g.dev_alice, Direction::Outgoing).unwrap();
                assert!(outgoing.is_empty());

                // Incoming: 2 fixed_by edges
                let incoming = db.edges_of(g.dev_alice, Direction::Incoming).unwrap();
                assert_eq!(incoming.len(), 2);
                assert!(incoming.iter().all(|e| e.kind == "fixed_by"));
            }

            // -----------------------------------------------------------------
            // 17. Edge weight on severity
            // -----------------------------------------------------------------

            #[test]
            fn bump_edge_weight_for_critical_blocker() {
                let db = $db_expr;
                let g = build_bug_board(&db);

                let edges = db.edges_of(g.bug_3, Direction::Outgoing).unwrap();
                let block = edges
                    .iter()
                    .find(|e| e.kind == "blocks_release" && e.to_id == g.rel_v2_0)
                    .unwrap();

                let updated = db
                    .update_edge(
                        block.id,
                        EdgePatch {
                            kind: None,
                            weight: Some(100.0),
                            properties: None,
                        },
                    )
                    .unwrap();
                assert_eq!(updated.weight, 100.0);
            }
        }
    };
}

// Run all tests against MemoryBackend
bug_tracker_tests!(memory_db(), memory);

// Run a smoke-test subset against RedbBackend
#[cfg(feature = "redb-backend")]
mod redb {
    use super::*;

    macro_rules! bug_tracker_redb_tests {
        ($mod_name:ident) => {
            mod $mod_name {
                use super::*;
                use tempfile::TempDir;

                fn setup() -> (TempDir, GraphNoteDb) {
                    let dir = TempDir::new().expect("create temp dir");
                    let db = redb_db(&dir);
                    (dir, db)
                }

                #[test]
                fn build_complete_bug_board() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    assert!(db.get_node(g.bug_1).unwrap().is_some());
                    assert!(db.get_node(g.rel_v2_0).unwrap().is_some());
                }

                #[test]
                fn assignee_workload() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let bugs = db
                        .neighbors(g.dev_alice, Direction::Incoming, Some("fixed_by"))
                        .unwrap();
                    assert_eq!(bugs.len(), 2);
                }

                #[test]
                fn release_reported_bugs() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let bugs = db
                        .neighbors(g.rel_v1_0, Direction::Incoming, Some("reported_in"))
                        .unwrap();
                    assert_eq!(bugs.len(), 3);
                }

                #[test]
                fn release_blockers() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let blockers = db
                        .neighbors(g.rel_v2_0, Direction::Incoming, Some("blocks_release"))
                        .unwrap();
                    assert_eq!(blockers.len(), 1);
                    assert_eq!(blockers[0].id, g.bug_3);
                }

                #[test]
                fn feature_release_contents() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let features = db
                        .neighbors(g.rel_v1_1, Direction::Incoming, Some("part_of"))
                        .unwrap();
                    assert_eq!(features.len(), 2);
                }

                #[test]
                fn kind_index_bugs() {
                    let (_dir, db) = setup();
                    let _g = build_bug_board(&db);
                    let bugs = db.list_nodes_by_kind("bug", 100, 0).unwrap();
                    assert_eq!(bugs.len(), 5);
                }

                #[test]
                fn fts_search_bug() {
                    let (_dir, db) = setup();
                    let _g = build_bug_board(&db);
                    let results = db.search_fts("BUG-1", 10).unwrap();
                    assert!(!results.is_empty());
                }

                #[test]
                fn shortest_path_bug_to_release() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let path = db.shortest_path(g.bug_1, g.rel_v1_0).unwrap();
                    assert!(path.is_some());
                    assert_eq!(path.unwrap().len(), 2);
                }

                #[test]
                fn subgraph_extraction() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let sg = db.subgraph(g.bug_1, 1).unwrap();
                    let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();
                    assert!(ids.contains(&g.dev_alice));
                    assert!(ids.contains(&g.tc_login_1));
                }

                #[test]
                fn delete_release_cascades() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    db.delete_node(g.rel_v1_1).unwrap();
                    let bugs = db
                        .neighbors(g.bug_2, Direction::Outgoing, Some("reported_in"))
                        .unwrap();
                    assert!(bugs.is_empty());
                }

                #[test]
                fn edge_properties_persist() {
                    let (_dir, db) = setup();
                    let g = build_bug_board(&db);
                    let edges = db.edges_of(g.bug_1, Direction::Outgoing).unwrap();
                    let fix = edges.iter().find(|e| e.kind == "fixed_by").unwrap();
                    assert_eq!(
                        fix.properties.get("commits").unwrap(),
                        &serde_json::json!(3)
                    );
                }
            }
        };
    }

    bug_tracker_redb_tests!(redb_backend);
}
