//! Reads filter instead of failing, as in Neo4j: a node or relationship the
//! user may not traverse is not matched, expanded, counted, searched or
//! returned, and a property without `READ` reads as `null`.
//!
//! The scenarios: a bug tracker with `:Secret` issues hidden from `rita`, and
//! an ERP where `hank` (HR clerk) may not read salaries or payments.
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

struct Server {
    app: axum::Router,
    users: Arc<AuthManager>,
}

impl Server {
    /// A server with `admin`, `rita` (reads, but no `:Secret` nodes), `hank`
    /// (reads, but no salaries and no `PAYS` relationships) and `rex` (a plain
    /// reader).
    async fn new() -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [
            ("admin", "admin"),
            ("rita", "reader"),
            ("hank", "reader"),
            ("rex", "reader"),
        ] {
            users
                .create_user(NewUser::new(name, "long-password").change_required(false))
                .unwrap();
            users.grant_role(role, name).unwrap();
        }
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(Arc::clone(&users)));
        let s = Self {
            app: build_native_router(state),
            users,
        };
        for q in [
            // Bug tracker.
            "CREATE (a:Bug {title: 'crash on save', severity: 1})",
            "CREATE (b:Bug:Secret {title: 'crash exploit in auth', severity: 0})",
            "CREATE (c:Bug {title: 'slow search', severity: 3})",
            "MATCH (a:Bug {title: 'crash on save'}), (b:Bug {title: 'crash exploit in auth'}) \
             CREATE (a)-[:BLOCKS]->(b)",
            "MATCH (b:Bug {title: 'crash exploit in auth'}), (c:Bug {title: 'slow search'}) \
             CREATE (b)-[:BLOCKS]->(c)",
            "MATCH (a:Bug {title: 'crash on save'}), (c:Bug {title: 'slow search'}) \
             CREATE (a)-[:RELATES]->(c)",
            // ERP.
            "CREATE (e:Employee {title: 'Ann', dept: 'ops', salary: 5000})",
            "CREATE (e:Employee {title: 'Bob', dept: 'it', salary: 7000})",
            "CREATE (co:Company {title: 'Acme'})",
            "MATCH (co:Company), (e:Employee {title: 'Ann'}) CREATE (co)-[:PAYS {amount: 5000}]->(e)",
            "MATCH (co:Company), (e:Employee {title: 'Ann'}) CREATE (co)-[:EMPLOYS {since: 2020}]->(e)",
            // Roles.
            "CREATE ROLE no_secrets",
            "DENY TRAVERSE ON GRAPH * NODES Secret TO no_secrets",
            "GRANT ROLE no_secrets TO rita",
            "CREATE ROLE hr_clerk",
            "DENY READ {salary} ON GRAPH * NODES Employee TO hr_clerk",
            "DENY TRAVERSE ON GRAPH * RELATIONSHIPS PAYS TO hr_clerk",
            "DENY READ {since} ON GRAPH * RELATIONSHIPS EMPLOYS TO hr_clerk",
            "GRANT ROLE hr_clerk TO hank",
        ] {
            s.rows("admin", q).await;
        }
        s
    }

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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn rows(&self, user: &str, query: &str) -> serde_json::Value {
        let (status, json) = self.run(user, query).await;
        assert_eq!(status, StatusCode::OK, "{user}: {query}: {json}");
        json["rows"].clone()
    }

    async fn single(&self, user: &str, query: &str) -> serde_json::Value {
        self.rows(user, query).await[0][0].clone()
    }
}

fn json(v: serde_json::Value) -> serde_json::Value {
    v
}

// ── Hidden nodes ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_hidden_node_is_not_matched_or_counted() {
    let s = Server::new().await;
    for (user, bugs) in [("admin", 3), ("rex", 3), ("rita", 2)] {
        assert_eq!(
            s.single(user, "MATCH (b:Bug) RETURN count(b)").await,
            bugs,
            "{user}"
        );
        // The count shortcut must not see them either.
        assert_eq!(
            s.single(user, "MATCH (b:Bug) RETURN count(*)").await,
            bugs,
            "{user}"
        );
    }
    assert_eq!(s.single("rita", "MATCH (n) RETURN count(*)").await, 5);
    assert_eq!(s.single("admin", "MATCH (n) RETURN count(*)").await, 6);
    let titles = s
        .rows("rita", "MATCH (b:Bug) RETURN b.title ORDER BY b.title")
        .await;
    assert_eq!(
        titles,
        json(serde_json::json!([["crash on save"], ["slow search"]]))
    );
    assert!(s
        .rows("rita", "MATCH (s:Secret) RETURN s")
        .await
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_hidden_node_is_not_reached_by_expanding() {
    let s = Server::new().await;
    // One hop: the BLOCKS edge into the secret bug is not followed.
    assert_eq!(
        s.single(
            "rita",
            "MATCH (:Bug {title: 'crash on save'})-[:BLOCKS]->(b) RETURN count(b)"
        )
        .await,
        0
    );
    assert_eq!(
        s.single(
            "rex",
            "MATCH (:Bug {title: 'crash on save'})-[:BLOCKS]->(b) RETURN count(b)"
        )
        .await,
        1
    );
    // Variable length: slow search is two BLOCKS hops away, through the secret.
    let reach = "MATCH (:Bug {title: 'crash on save'})-[:BLOCKS*1..3]->(b) RETURN count(b)";
    assert_eq!(s.single("rex", reach).await, 2);
    assert_eq!(s.single("rita", reach).await, 0);
    // Shortest path: the direct RELATES edge, never the route through it.
    let shortest = "MATCH p = shortestPath((a:Bug {title: 'crash on save'})-[*]-(c:Bug {title: 'slow search'})) \
                    RETURN length(p)";
    assert_eq!(s.single("rita", shortest).await, 1);
    // Relationships touching a hidden node are hidden too.
    assert_eq!(
        s.single("rita", "MATCH (:Bug)-[r]->(:Bug) RETURN count(r)")
            .await,
        1
    );
    assert_eq!(
        s.single("rex", "MATCH (:Bug)-[r]->(:Bug) RETURN count(r)")
            .await,
        3
    );
    // Pattern predicates and subqueries see the same graph.
    assert_eq!(
        s.single(
            "rita",
            "MATCH (b:Bug) WHERE EXISTS { (b)-[:BLOCKS]->() } RETURN count(b)"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn search_skips_hidden_nodes() {
    let s = Server::new().await;
    let crashes = "CALL fts.search('crash', 10) YIELD node RETURN node.title ORDER BY node.title";
    assert_eq!(s.rows("rex", crashes).await.as_array().unwrap().len(), 2);
    assert_eq!(
        s.rows("rita", crashes).await,
        json(serde_json::json!([["crash on save"]]))
    );
}

#[tokio::test]
async fn end_nodes_of_a_relationship_are_hidden_too() {
    let s = Server::new().await;
    // The secret bug is the start of a BLOCKS edge and the end of another;
    // neither edge is visible, so neither endpoint leaks.
    let ends = "MATCH ()-[r:BLOCKS]->() RETURN startNode(r).title, endNode(r).title";
    assert!(s.rows("rita", ends).await.as_array().unwrap().is_empty());
}

// ── Hidden relationships ─────────────────────────────────────────────

#[tokio::test]
async fn a_hidden_relationship_type_is_not_matched() {
    let s = Server::new().await;
    assert_eq!(
        s.single("rex", "MATCH ()-[r:PAYS]->() RETURN count(r)")
            .await,
        1
    );
    assert_eq!(
        s.single("hank", "MATCH ()-[r:PAYS]->() RETURN count(r)")
            .await,
        0
    );
    assert_eq!(
        s.single("hank", "MATCH (:Company)-->(e) RETURN count(e)")
            .await,
        1,
        "only EMPLOYS"
    );
}

// ── Hidden properties ────────────────────────────────────────────────

#[tokio::test]
async fn a_property_without_read_is_null() {
    let s = Server::new().await;
    let salaries = "MATCH (e:Employee) RETURN e.title, e.salary ORDER BY e.title";
    assert_eq!(
        s.rows("rex", salaries).await,
        json(serde_json::json!([["Ann", 5000], ["Bob", 7000]]))
    );
    assert_eq!(
        s.rows("hank", salaries).await,
        json(serde_json::json!([["Ann", null], ["Bob", null]]))
    );
    // Other properties read as usual.
    assert_eq!(
        s.single("hank", "MATCH (e:Employee {title: 'Bob'}) RETURN e.dept")
            .await,
        "it"
    );
}

#[tokio::test]
async fn a_hidden_property_cannot_be_listed_filtered_or_returned() {
    let s = Server::new().await;
    let keys = s
        .single("hank", "MATCH (e:Employee {title: 'Ann'}) RETURN keys(e)")
        .await;
    assert!(!keys.to_string().contains("salary"), "{keys}");
    let props = s
        .single(
            "hank",
            "MATCH (e:Employee {title: 'Ann'}) RETURN properties(e)",
        )
        .await;
    assert!(!props.to_string().contains("salary"), "{props}");
    let whole = s
        .rows("hank", "MATCH (e:Employee {title: 'Ann'}) RETURN e")
        .await;
    assert!(!whole.to_string().contains("5000"), "{whole}");
    // A hidden property cannot narrow a match either.
    assert_eq!(
        s.single(
            "hank",
            "MATCH (e:Employee) WHERE e.salary > 0 RETURN count(e)"
        )
        .await,
        0
    );
    assert_eq!(
        s.single("hank", "MATCH (e:Employee {salary: 5000}) RETURN count(e)")
            .await,
        0
    );
    assert_eq!(
        s.single(
            "rex",
            "MATCH (e:Employee) WHERE e.salary > 0 RETURN count(e)"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn a_hidden_relationship_property_is_null_and_cannot_narrow_a_match() {
    let s = Server::new().await;
    assert_eq!(
        s.single("rex", "MATCH ()-[r:EMPLOYS]->() RETURN r.since")
            .await,
        2020
    );
    assert_eq!(
        s.single("hank", "MATCH ()-[r:EMPLOYS]->() RETURN r.since")
            .await,
        serde_json::Value::Null
    );
    assert_eq!(
        s.single(
            "hank",
            "MATCH ()-[r:EMPLOYS {since: 2020}]->() RETURN count(r)"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn a_reader_with_no_deny_sees_everything() {
    let s = Server::new().await;
    assert_eq!(s.single("rex", "MATCH (n) RETURN count(n)").await, 6);
    assert_eq!(s.single("rex", "MATCH ()-[r]->() RETURN count(r)").await, 5);
}
