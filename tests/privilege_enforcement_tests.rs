//! Privileges are enforced on writes, schema commands, procedures and
//! database access: without the privilege a statement fails with
//! `Neo.ClientError.Security.Forbidden` and writes nothing.
//!
//! The scenario is a bug tracker. `admin` sets it up; `rita` is a reader,
//! `ed` an editor, `archie` an architect, `tina` triages bugs with an
//! editor's rights minus a few, and `pat` has only `PUBLIC`.
#![cfg(all(feature = "http", feature = "auth"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::Security;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, NewUser};
use tower::ServiceExt;

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

struct Tracker {
    app: axum::Router,
    users: Arc<AuthManager>,
}

impl Tracker {
    fn new() -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [
            ("admin", Some("admin")),
            ("rita", Some("reader")),
            ("ed", Some("editor")),
            ("archie", Some("architect")),
            ("tina", None),
            ("pat", None),
        ] {
            users
                .create_user(NewUser::new(name, "long-password").change_required(false))
                .unwrap();
            if let Some(role) = role {
                users.grant_role(role, name).unwrap();
            }
        }
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(Arc::clone(&users)));
        Self {
            app: build_native_router(state),
            users,
        }
    }

    /// POST /cypher as `user`; (status, body).
    async fn run(&self, user: &str, query: &str) -> (StatusCode, serde_json::Value) {
        let token = self.users.issue_session(user).unwrap();
        let body = serde_json::json!({ "query": query }).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/cypher")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn ok(&self, user: &str, query: &str) -> serde_json::Value {
        let (status, json) = self.run(user, query).await;
        assert_eq!(status, StatusCode::OK, "{user}: {query}: {json}");
        json
    }

    async fn forbidden(&self, user: &str, query: &str) {
        let (status, json) = self.run(user, query).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user}: {query}: {json}");
        assert_eq!(json["code"], FORBIDDEN, "{user}: {query}: {json}");
    }

    async fn count(&self, query: &str) -> i64 {
        let json = self.ok("admin", query).await;
        json["rows"][0][0].as_i64().unwrap()
    }

    /// Seed two bugs and a triage role for `tina`.
    async fn seeded() -> Self {
        let t = Self::new();
        for q in [
            "CREATE (:Bug {title: 'crash on save', reporter: 'zoe'})",
            "CREATE (:Bug {title: 'slow search', reporter: 'max'})",
            "CREATE ROLE triage AS COPY OF editor",
            "DENY CREATE ON GRAPH * NODES Secret TO triage",
            "DENY SET PROPERTY {reporter} ON GRAPH * NODES Bug TO triage",
            "DENY DELETE ON GRAPH * NODES Bug TO triage",
            "DENY SET LABEL Closed ON GRAPH * TO triage",
            "GRANT ROLE triage TO tina",
        ] {
            t.ok("admin", q).await;
        }
        t
    }
}

// ── Writes ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_reader_cannot_write() {
    let t = Tracker::seeded().await;
    for q in [
        "CREATE (:Bug {title: 'new'})",
        "MATCH (b:Bug) SET b.status = 'open'",
        "MATCH (b:Bug) DETACH DELETE b",
        "MATCH (b:Bug) SET b:Urgent",
        "MERGE (:Bug {title: 'merged'})",
        "MATCH (a:Bug), (b:Bug) WHERE a <> b CREATE (a)-[:DUPLICATES]->(b)",
    ] {
        t.forbidden("rita", q).await;
    }
    assert_eq!(t.count("MATCH (b:Bug) RETURN count(b)").await, 2);
    assert_eq!(t.count("MATCH ()-[r]->() RETURN count(r)").await, 0);
    // Reads still work.
    t.ok("rita", "MATCH (b:Bug) RETURN b.title").await;
}

#[tokio::test]
async fn an_editor_writes() {
    let t = Tracker::seeded().await;
    t.ok("ed", "CREATE (:Bug {title: 'new'})").await;
    t.ok(
        "ed",
        "MATCH (b:Bug {title: 'new'}) SET b.status = 'open', b:Urgent",
    )
    .await;
    t.ok("ed", "MATCH (b:Bug {title: 'new'}) DETACH DELETE b")
        .await;
    assert_eq!(t.count("MATCH (b:Bug) RETURN count(b)").await, 2);
}

#[tokio::test]
async fn a_denied_write_fails_the_whole_statement() {
    let t = Tracker::seeded().await;
    // The Bug is allowed, the Secret is not: neither is created.
    t.forbidden(
        "tina",
        "CREATE (:Bug {title: 'ok'}), (:Secret {title: 'no'})",
    )
    .await;
    assert_eq!(t.count("MATCH (n) RETURN count(n)").await, 2);
    // A later, allowed write is not blocked by the earlier refusal.
    t.ok("tina", "CREATE (:Bug {title: 'triaged'})").await;
    assert_eq!(t.count("MATCH (b:Bug) RETURN count(b)").await, 3);
}

#[tokio::test]
async fn denied_properties_labels_and_deletes_are_refused() {
    let t = Tracker::seeded().await;
    t.ok("tina", "MATCH (b:Bug) SET b.status = 'triaged'").await;
    t.forbidden("tina", "MATCH (b:Bug) SET b.reporter = 'someone else'")
        .await;
    t.forbidden("tina", "MATCH (b:Bug) REMOVE b.reporter").await;
    t.forbidden("tina", "MATCH (b:Bug) SET b = {title: 'replaced'}")
        .await;
    t.forbidden("tina", "MATCH (b:Bug) SET b:Closed").await;
    t.ok("tina", "MATCH (b:Bug) SET b:Triaged").await;
    t.forbidden("tina", "MATCH (b:Bug) DELETE b").await;
    let json = t
        .ok(
            "admin",
            "MATCH (b:Bug) RETURN b.reporter, b.status ORDER BY b.reporter",
        )
        .await;
    assert_eq!(
        json["rows"],
        serde_json::json!([["max", "triaged"], ["zoe", "triaged"]]),
        "only the allowed SET happened"
    );
}

// ── Schema ───────────────────────────────────────────────────────────

#[tokio::test]
async fn index_and_constraint_commands_need_their_management_privilege() {
    let t = Tracker::seeded().await;
    t.forbidden("ed", "CREATE INDEX FOR (b:Bug) ON (b.title)")
        .await;
    t.forbidden(
        "ed",
        "CREATE CONSTRAINT bug_title FOR (b:Bug) REQUIRE b.title IS UNIQUE",
    )
    .await;
    t.ok(
        "archie",
        "CREATE INDEX bug_title_idx FOR (b:Bug) ON (b.title)",
    )
    .await;
    t.ok(
        "archie",
        "CREATE CONSTRAINT bug_title FOR (b:Bug) REQUIRE b.title IS UNIQUE",
    )
    .await;
    t.forbidden("ed", "DROP CONSTRAINT bug_title").await;
    t.forbidden("ed", "DROP INDEX bug_title_idx").await;
    t.ok("archie", "DROP CONSTRAINT bug_title").await;
    // Listing the schema needs only access.
    t.ok("rita", "SHOW INDEXES").await;
    t.ok("rita", "SHOW CONSTRAINTS").await;
}

// ── Procedures ───────────────────────────────────────────────────────

#[tokio::test]
async fn procedures_need_execute() {
    let t = Tracker::seeded().await;
    // PUBLIC may run every procedure.
    t.ok("rita", "CALL db.labels()").await;
    t.ok("admin", "DENY EXECUTE PROCEDURE db.* ON DBMS TO reader")
        .await;
    t.forbidden("rita", "CALL db.labels()").await;
    t.ok("ed", "CALL db.labels()").await;
}

#[tokio::test]
async fn administration_procedures_need_execute_admin_procedures() {
    let t = Tracker::seeded().await;
    t.forbidden("ed", "CALL drevo.tx.listPrepared()").await;
    t.ok("admin", "CALL drevo.tx.listPrepared()").await;
}

#[tokio::test]
async fn writing_procedures_need_write() {
    let t = Tracker::seeded().await;
    t.forbidden(
        "rita",
        "CALL drevo.memory.addMessage('triage-1', 'user', 'hello')",
    )
    .await;
    t.ok(
        "ed",
        "CALL drevo.memory.addMessage('triage-1', 'user', 'hello')",
    )
    .await;
}

// ── Database access ──────────────────────────────────────────────────

#[tokio::test]
async fn switching_a_database_read_only_needs_database_management() {
    let t = Tracker::seeded().await;
    t.ok("admin", "CREATE DATABASE erp").await;
    t.forbidden("ed", "ALTER DATABASE erp SET ACCESS READ ONLY")
        .await;
    t.ok("admin", "ALTER DATABASE erp SET ACCESS READ ONLY")
        .await;
    let rows = t.ok("admin", "SHOW DATABASES").await["rows"].to_string();
    assert!(rows.contains(r#"["erp","read-only"]"#), "{rows}");
}

#[tokio::test]
async fn a_database_without_access_cannot_be_used() {
    let t = Tracker::seeded().await;
    t.ok("admin", "CREATE DATABASE erp").await;
    // pat has PUBLIC only: the home database, nothing else.
    t.ok("pat", "RETURN 1").await;
    t.forbidden("pat", "USE erp RETURN 1").await;
    // SHOW DATABASES lists only what the user may use.
    let listed = |json: serde_json::Value| json["rows"].to_string();
    assert!(!listed(t.ok("pat", "SHOW DATABASES").await).contains("erp"));
    assert!(listed(t.ok("rita", "SHOW DATABASES").await).contains("erp"));
    // A reader may use every database.
    t.ok("rita", "USE erp RETURN 1").await;
    t.ok("admin", "DENY ACCESS ON DATABASE erp TO reader").await;
    t.forbidden("rita", "USE erp RETURN 1").await;
}

// ── Without logins nothing is checked ────────────────────────────────

#[tokio::test]
async fn a_server_that_needs_no_login_checks_nothing() {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(users).without_login());
    let app = build_native_router(state);
    let body = serde_json::json!({ "query": "CREATE (:Bug) RETURN 1" }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Role changes apply at once ───────────────────────────────────────

#[tokio::test]
async fn a_granted_role_applies_to_the_next_statement() {
    let t = Tracker::seeded().await;
    t.forbidden("rita", "CREATE (:Bug {title: 'x'})").await;
    t.ok("admin", "GRANT ROLE editor TO rita").await;
    t.ok("rita", "CREATE (:Bug {title: 'x'})").await;
    t.ok("admin", "REVOKE ROLE editor FROM rita").await;
    t.forbidden("rita", "CREATE (:Bug {title: 'y'})").await;
}

// ── Bolt ─────────────────────────────────────────────────────────────

mod bolt {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use drevo::bolt::packstream::Value;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use drevo::native_service::NativeService;
    use drevo::security::Security;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};

    fn dict<I: IntoIterator<Item = (&'static str, Value)>>(e: I) -> BTreeMap<String, Value> {
        e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn session_as(role: &str) -> Session<'static> {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        users
            .create_user(NewUser::new("u", "long-password").change_required(false))
            .unwrap();
        users.grant_role(role, "u").unwrap();
        let mut s = Session::new_durable(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(users));
        let replies = s.handle(ClientMessage::Hello {
            extra: dict([
                ("scheme", Value::String("basic".into())),
                ("principal", Value::String("u".into())),
                ("credentials", Value::String("long-password".into())),
            ]),
        });
        assert!(matches!(replies[0], ServerMessage::Success { .. }));
        s
    }

    fn run(s: &mut Session<'_>, query: &str) -> Vec<ServerMessage> {
        let mut replies = s.handle(ClientMessage::Run {
            query: query.to_string(),
            parameters: dict([]),
            extra: dict([]),
        });
        if matches!(replies.first(), Some(ServerMessage::Success { .. })) {
            replies.extend(s.handle(ClientMessage::Pull {
                extra: dict([("n", Value::Integer(-1))]),
            }));
        }
        replies
    }

    fn code(replies: &[ServerMessage]) -> Option<String> {
        replies.iter().find_map(|m| match m {
            ServerMessage::Failure { metadata } => match metadata.get("code") {
                Some(Value::String(c)) => Some(c.clone()),
                _ => None,
            },
            _ => None,
        })
    }

    #[test]
    fn a_reader_gets_forbidden_in_auto_commit_and_in_a_transaction() {
        let mut s = session_as("reader");
        assert_eq!(
            code(&run(&mut s, "CREATE (:Bug)")).as_deref(),
            Some(super::FORBIDDEN)
        );
        s.handle(ClientMessage::Reset);
        s.handle(ClientMessage::Begin { extra: dict([]) });
        assert_eq!(
            code(&run(&mut s, "CREATE (:Bug)")).as_deref(),
            Some(super::FORBIDDEN)
        );
    }

    #[test]
    fn an_editor_writes_in_a_transaction() {
        let mut s = session_as("editor");
        s.handle(ClientMessage::Begin { extra: dict([]) });
        assert!(code(&run(&mut s, "CREATE (:Bug)")).is_none());
        let replies = s.handle(ClientMessage::Commit);
        assert!(
            matches!(replies[0], ServerMessage::Success { .. }),
            "{replies:?}"
        );
    }
}

// ── Triggers ─────────────────────────────────────────────────────────

#[tokio::test]
async fn changing_triggers_needs_admin_procedures() {
    let t = Tracker::seeded().await;
    for user in ["ed", "archie", "rita"] {
        t.forbidden(
            user,
            "CALL apoc.trigger.add('audit', 'RETURN 1', {phase: 'after'})",
        )
        .await;
    }
    t.ok("admin", "CALL apoc.trigger.add('audit', 'UNWIND $createdNodes AS n CREATE (:Audit)', {phase: 'after'})")
        .await;
    for user in ["ed", "archie"] {
        t.forbidden(user, "CALL apoc.trigger.pause('audit')").await;
        t.forbidden(user, "CALL apoc.trigger.remove('audit')").await;
    }
    // Listing is not administration.
    let listed = t
        .ok("ed", "CALL apoc.trigger.list() YIELD name RETURN name")
        .await;
    assert_eq!(listed["rows"][0][0], "audit");
}

#[tokio::test]
async fn a_trigger_runs_with_its_own_rights_for_any_writer() {
    let t = Tracker::seeded().await;
    t.ok(
        "admin",
        "CALL apoc.trigger.add('audit', 'UNWIND $createdNodes AS n CREATE (:Audit {of: n.title})', {phase: 'after'})",
    )
    .await;
    // The editor may create bugs but has no grant for Audit nodes beyond
    // that; the trigger's write is the database's, not the editor's.
    t.ok("ed", "CREATE (:Bug {title: 'flaky test'})").await;
    assert_eq!(
        t.count("MATCH (a:Audit {of: 'flaky test'}) RETURN count(a)")
            .await,
        1
    );
}

// ── Sequences ────────────────────────────────────────────────────────

#[tokio::test]
async fn changing_sequences_needs_admin_procedures() {
    let t = Tracker::seeded().await;
    for user in ["ed", "archie", "rita"] {
        t.forbidden(user, "CALL drevo.sequence.create('bug', {start: 1000})")
            .await;
    }
    t.ok("admin", "CALL drevo.sequence.create('bug', {start: 1000})")
        .await;
    t.forbidden("ed", "CALL drevo.sequence.drop('bug')").await;
    // Drawing numbers and listing are not administration.
    let drawn = t
        .ok(
            "ed",
            "CREATE (b:Bug {title: 'numbered', n: drevo.sequence.next('bug')}) RETURN b.n",
        )
        .await;
    assert_eq!(drawn["rows"][0][0], 1000);
    let listed = t
        .ok("rita", "CALL drevo.sequence.list() YIELD name RETURN name")
        .await;
    assert_eq!(listed["rows"][0][0], "bug");
}

// ── Secondary indexes ────────────────────────────────────────────────

#[tokio::test]
async fn dropping_secondary_indexes_needs_index_management() {
    let t = Tracker::seeded().await;
    for user in ["ed", "rita"] {
        t.forbidden(user, "CALL drevo.indexes.setSecondary(false)")
            .await;
    }
    t.ok("archie", "CALL drevo.indexes.setSecondary(false)")
        .await;
    let state = t
        .ok(
            "rita",
            "CALL drevo.indexes.secondary() YIELD enabled RETURN enabled",
        )
        .await;
    assert_eq!(state["rows"][0][0], false);
}
