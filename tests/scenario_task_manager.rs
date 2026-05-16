//! Scenario integration test: IT Task Manager
//!
//! This test validates drevo against a real-world IT task manager use case.
//!
//! Domain model:
//! - Node kinds: `task`, `epic`, `sprint`, `developer`, `component`
//! - Edge kinds: `assigned_to`, `blocks`, `part_of`, `depends_on`
//!
//! Tested workflows:
//! - Building a sprint board with tasks, epics, developers, and components
//! - Dependency chains: task A depends_on task B
//! - Blocking BFS: from a blocked task, find the full blocking chain
//! - Sprint board view: list all tasks in a sprint via kind_index
//! - Developer workload: find all tasks assigned to a developer
//! - Component ownership: tasks grouped by component
//! - FTS: searching for tasks by description
//! - Subgraph: extracting the full context of a task (deps, assignee, component)
//! - Shortest path: find the critical dependency path between two tasks
//! - Edge weight priorities and task status via properties

use drevo::db::Drevo;
use drevo::model::*;
use std::collections::HashMap;

// =========================================================================
// Test helpers
// =========================================================================

fn memory_db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory DB")
}

#[cfg(feature = "redb-backend")]
fn redb_db(dir: &tempfile::TempDir) -> Drevo {
    let path = dir.path().join("task_manager_test.db");
    Drevo::open(&path).expect("open redb DB")
}

fn make_node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
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

fn make_weighted_edge(from_id: u64, to_id: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight,
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

// =========================================================================
// Domain: Sprint board setup
// =========================================================================

/// A complete sprint board with all created node IDs.
///
/// Graph structure:
///
/// ```text
/// epic: "User Authentication"
///   ──contains──▶ task: "Design login page" (sprint-1, assigned to Alice, component: frontend)
///   ──contains──▶ task: "Implement OAuth backend" (sprint-1, assigned to Bob, component: backend)
///   ──contains──▶ task: "Write auth integration tests" (sprint-1, assigned to Alice, component: testing)
///   ──contains──▶ task: "Deploy auth service" (sprint-2, assigned to Charlie, component: devops)
///
/// epic: "Dashboard"
///   ──contains──▶ task: "Design dashboard layout" (sprint-1, assigned to Alice, component: frontend)
///   ──contains──▶ task: "Build analytics API" (sprint-2, assigned to Bob, component: backend)
///   ──contains──▶ task: "Integrate charts" (sprint-2, assigned to Charlie, component: frontend)
///
/// Dependencies:
///   "Implement OAuth backend" ──blocks──▶ "Write auth integration tests"
///   "Write auth integration tests" ──blocks──▶ "Deploy auth service"
///   "Design login page" ──depends_on──▶ "Implement OAuth backend"
///   "Design dashboard layout" ──depends_on──▶ "Build analytics API"
///   "Build analytics API" ──blocks──▶ "Integrate charts"
///
/// Developers: Alice, Bob, Charlie
/// Components: frontend, backend, testing, devops
/// Sprints: sprint-1, sprint-2
/// ```
struct SprintBoard {
    // Epics
    epic_auth: u64,
    epic_dashboard: u64,
    // Sprint-1 tasks
    task_design_login: u64,
    task_oauth_backend: u64,
    task_auth_tests: u64,
    // Sprint-2 tasks
    task_deploy_auth: u64,
    task_design_dashboard: u64,
    task_analytics_api: u64,
    task_integrate_charts: u64,
    // Developers
    dev_alice: u64,
    dev_bob: u64,
    dev_charlie: u64,
    // Components
    comp_frontend: u64,
    comp_backend: u64,
    comp_testing: u64,
    #[allow(dead_code)]
    comp_devops: u64,
    // Sprints
    sprint_1: u64,
    sprint_2: u64,
}

fn task_props(
    status: &str,
    priority: &str,
    story_points: u32,
) -> HashMap<String, serde_json::Value> {
    let mut props = HashMap::new();
    props.insert("status".to_string(), serde_json::json!(status));
    props.insert("priority".to_string(), serde_json::json!(priority));
    props.insert("story_points".to_string(), serde_json::json!(story_points));
    props
}

fn build_sprint_board(db: &Drevo) -> SprintBoard {
    // --- Sprints ---
    let sprint_1 = db
        .create_node(make_node(
            "sprint",
            "Sprint 1",
            "First sprint: auth + dashboard design",
        ))
        .unwrap();
    let sprint_2 = db
        .create_node(make_node(
            "sprint",
            "Sprint 2",
            "Second sprint: dashboard build + deploy",
        ))
        .unwrap();

    // --- Developers ---
    let dev_alice = db
        .create_node(make_node_with_props(
            "developer",
            "Alice",
            "Senior frontend engineer",
            {
                let mut p = HashMap::new();
                p.insert("team".to_string(), serde_json::json!("frontend"));
                p.insert("email".to_string(), serde_json::json!("alice@example.com"));
                p
            },
        ))
        .unwrap();
    let dev_bob = db
        .create_node(make_node_with_props(
            "developer",
            "Bob",
            "Backend engineer specializing in authentication systems",
            {
                let mut p = HashMap::new();
                p.insert("team".to_string(), serde_json::json!("backend"));
                p.insert("email".to_string(), serde_json::json!("bob@example.com"));
                p
            },
        ))
        .unwrap();
    let dev_charlie = db
        .create_node(make_node_with_props(
            "developer",
            "Charlie",
            "DevOps engineer handling deployments and infrastructure",
            {
                let mut p = HashMap::new();
                p.insert("team".to_string(), serde_json::json!("devops"));
                p.insert(
                    "email".to_string(),
                    serde_json::json!("charlie@example.com"),
                );
                p
            },
        ))
        .unwrap();

    // --- Components ---
    let comp_frontend = db
        .create_node(make_node(
            "component",
            "Frontend",
            "React-based web application",
        ))
        .unwrap();
    let comp_backend = db
        .create_node(make_node(
            "component",
            "Backend",
            "Rust API server with axum",
        ))
        .unwrap();
    let comp_testing = db
        .create_node(make_node(
            "component",
            "Testing",
            "Integration and e2e test suite",
        ))
        .unwrap();
    let comp_devops = db
        .create_node(make_node(
            "component",
            "DevOps",
            "CI/CD pipelines and deployment",
        ))
        .unwrap();

    // --- Epics ---
    let epic_auth = db
        .create_node(make_node(
            "epic",
            "User Authentication",
            "Complete authentication system with OAuth",
        ))
        .unwrap();
    let epic_dashboard = db
        .create_node(make_node(
            "epic",
            "Dashboard",
            "Analytics dashboard with charts and reports",
        ))
        .unwrap();

    // --- Tasks: Auth epic ---
    let task_design_login = db
        .create_node(make_node_with_props(
            "task",
            "Design login page",
            "Create mockup and implement login page with email/password and OAuth buttons",
            task_props("done", "high", 5),
        ))
        .unwrap();

    let task_oauth_backend = db
        .create_node(make_node_with_props(
            "task",
            "Implement OAuth backend",
            "Set up OAuth2 provider integration with Google and GitHub",
            task_props("in_progress", "critical", 8),
        ))
        .unwrap();

    let task_auth_tests = db
        .create_node(make_node_with_props(
            "task",
            "Write auth integration tests",
            "End-to-end tests for login flow, token refresh, and logout",
            task_props("blocked", "high", 5),
        ))
        .unwrap();

    let task_deploy_auth = db
        .create_node(make_node_with_props(
            "task",
            "Deploy auth service",
            "Deploy authentication microservice to production Kubernetes cluster",
            task_props("todo", "critical", 3),
        ))
        .unwrap();

    // --- Tasks: Dashboard epic ---
    let task_design_dashboard = db
        .create_node(make_node_with_props(
            "task",
            "Design dashboard layout",
            "Wireframe and implement the main dashboard layout with widget grid",
            task_props("done", "medium", 5),
        ))
        .unwrap();

    let task_analytics_api = db
        .create_node(make_node_with_props(
            "task",
            "Build analytics API",
            "REST endpoints for fetching analytics data aggregations",
            task_props("todo", "high", 8),
        ))
        .unwrap();

    let task_integrate_charts = db
        .create_node(make_node_with_props(
            "task",
            "Integrate charts",
            "Connect chart widgets to analytics API and add real-time updates",
            task_props("todo", "medium", 5),
        ))
        .unwrap();

    // --- Edges: Epic contains tasks ---
    db.create_edge(make_edge(epic_auth.id, task_design_login.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(epic_auth.id, task_oauth_backend.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(epic_auth.id, task_auth_tests.id, "contains"))
        .unwrap();
    db.create_edge(make_edge(epic_auth.id, task_deploy_auth.id, "contains"))
        .unwrap();

    db.create_edge(make_edge(
        epic_dashboard.id,
        task_design_dashboard.id,
        "contains",
    ))
    .unwrap();
    db.create_edge(make_edge(
        epic_dashboard.id,
        task_analytics_api.id,
        "contains",
    ))
    .unwrap();
    db.create_edge(make_edge(
        epic_dashboard.id,
        task_integrate_charts.id,
        "contains",
    ))
    .unwrap();

    // --- Edges: Tasks assigned to developers ---
    db.create_edge(make_edge(task_design_login.id, dev_alice.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(task_oauth_backend.id, dev_bob.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(task_auth_tests.id, dev_alice.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(
        task_deploy_auth.id,
        dev_charlie.id,
        "assigned_to",
    ))
    .unwrap();
    db.create_edge(make_edge(
        task_design_dashboard.id,
        dev_alice.id,
        "assigned_to",
    ))
    .unwrap();
    db.create_edge(make_edge(task_analytics_api.id, dev_bob.id, "assigned_to"))
        .unwrap();
    db.create_edge(make_edge(
        task_integrate_charts.id,
        dev_charlie.id,
        "assigned_to",
    ))
    .unwrap();

    // --- Edges: Tasks belong to components ---
    db.create_edge(make_edge(task_design_login.id, comp_frontend.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_oauth_backend.id, comp_backend.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_auth_tests.id, comp_testing.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_deploy_auth.id, comp_devops.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(
        task_design_dashboard.id,
        comp_frontend.id,
        "part_of",
    ))
    .unwrap();
    db.create_edge(make_edge(task_analytics_api.id, comp_backend.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(
        task_integrate_charts.id,
        comp_frontend.id,
        "part_of",
    ))
    .unwrap();

    // --- Edges: Tasks belong to sprints ---
    db.create_edge(make_edge(task_design_login.id, sprint_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_oauth_backend.id, sprint_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_auth_tests.id, sprint_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_design_dashboard.id, sprint_1.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_deploy_auth.id, sprint_2.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_analytics_api.id, sprint_2.id, "part_of"))
        .unwrap();
    db.create_edge(make_edge(task_integrate_charts.id, sprint_2.id, "part_of"))
        .unwrap();

    // --- Edges: Blocking / dependency chains ---
    // OAuth backend blocks auth tests (can't test without it)
    db.create_edge(make_weighted_edge(
        task_oauth_backend.id,
        task_auth_tests.id,
        "blocks",
        1.0,
    ))
    .unwrap();
    // Auth tests block deploy (can't deploy untested code)
    db.create_edge(make_weighted_edge(
        task_auth_tests.id,
        task_deploy_auth.id,
        "blocks",
        1.0,
    ))
    .unwrap();
    // Login page depends on OAuth backend (needs API endpoints)
    db.create_edge(make_edge(
        task_design_login.id,
        task_oauth_backend.id,
        "depends_on",
    ))
    .unwrap();
    // Dashboard layout depends on analytics API (needs data shape)
    db.create_edge(make_edge(
        task_design_dashboard.id,
        task_analytics_api.id,
        "depends_on",
    ))
    .unwrap();
    // Analytics API blocks chart integration
    db.create_edge(make_weighted_edge(
        task_analytics_api.id,
        task_integrate_charts.id,
        "blocks",
        1.0,
    ))
    .unwrap();

    SprintBoard {
        epic_auth: epic_auth.id,
        epic_dashboard: epic_dashboard.id,
        task_design_login: task_design_login.id,
        task_oauth_backend: task_oauth_backend.id,
        task_auth_tests: task_auth_tests.id,
        task_deploy_auth: task_deploy_auth.id,
        task_design_dashboard: task_design_dashboard.id,
        task_analytics_api: task_analytics_api.id,
        task_integrate_charts: task_integrate_charts.id,
        dev_alice: dev_alice.id,
        dev_bob: dev_bob.id,
        dev_charlie: dev_charlie.id,
        comp_frontend: comp_frontend.id,
        comp_backend: comp_backend.id,
        comp_testing: comp_testing.id,
        comp_devops: comp_devops.id,
        sprint_1: sprint_1.id,
        sprint_2: sprint_2.id,
    }
}

// =========================================================================
// Tests — run against both backends via macro
// =========================================================================

macro_rules! task_manager_tests {
    ($db_expr:expr, $mod_name:ident) => {
        mod $mod_name {
            use super::*;

            // -----------------------------------------------------------------
            // 1. Basic graph construction — all nodes and edges are created
            // -----------------------------------------------------------------

            #[test]
            fn build_complete_sprint_board() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // All nodes should exist
                assert!(db.get_node(board.epic_auth).unwrap().is_some());
                assert!(db.get_node(board.epic_dashboard).unwrap().is_some());
                assert!(db.get_node(board.task_design_login).unwrap().is_some());
                assert!(db.get_node(board.task_oauth_backend).unwrap().is_some());
                assert!(db.get_node(board.task_auth_tests).unwrap().is_some());
                assert!(db.get_node(board.task_deploy_auth).unwrap().is_some());
                assert!(db.get_node(board.task_design_dashboard).unwrap().is_some());
                assert!(db.get_node(board.task_analytics_api).unwrap().is_some());
                assert!(db.get_node(board.task_integrate_charts).unwrap().is_some());
                assert!(db.get_node(board.dev_alice).unwrap().is_some());
                assert!(db.get_node(board.dev_bob).unwrap().is_some());
                assert!(db.get_node(board.dev_charlie).unwrap().is_some());
                assert!(db.get_node(board.comp_frontend).unwrap().is_some());
                assert!(db.get_node(board.comp_backend).unwrap().is_some());
                assert!(db.get_node(board.sprint_1).unwrap().is_some());
                assert!(db.get_node(board.sprint_2).unwrap().is_some());
            }

            #[test]
            fn node_kinds_are_correct() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                assert_eq!(db.get_node(board.epic_auth).unwrap().unwrap().kind, "epic");
                assert_eq!(
                    db.get_node(board.task_design_login).unwrap().unwrap().kind,
                    "task"
                );
                assert_eq!(
                    db.get_node(board.dev_alice).unwrap().unwrap().kind,
                    "developer"
                );
                assert_eq!(
                    db.get_node(board.comp_frontend).unwrap().unwrap().kind,
                    "component"
                );
                assert_eq!(db.get_node(board.sprint_1).unwrap().unwrap().kind, "sprint");
            }

            #[test]
            fn task_properties_stored_correctly() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let task = db.get_node(board.task_oauth_backend).unwrap().unwrap();
                assert_eq!(
                    task.properties.get("status").unwrap(),
                    &serde_json::json!("in_progress")
                );
                assert_eq!(
                    task.properties.get("priority").unwrap(),
                    &serde_json::json!("critical")
                );
                assert_eq!(
                    task.properties.get("story_points").unwrap(),
                    &serde_json::json!(8)
                );
            }

            // -----------------------------------------------------------------
            // 2. Epic → Task hierarchy
            // -----------------------------------------------------------------

            #[test]
            fn epic_contains_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let auth_tasks = db
                    .neighbors(board.epic_auth, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(auth_tasks.len(), 4);
                let auth_task_ids: Vec<u64> = auth_tasks.iter().map(|n| n.id).collect();
                assert!(auth_task_ids.contains(&board.task_design_login));
                assert!(auth_task_ids.contains(&board.task_oauth_backend));
                assert!(auth_task_ids.contains(&board.task_auth_tests));
                assert!(auth_task_ids.contains(&board.task_deploy_auth));
            }

            #[test]
            fn dashboard_epic_contains_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let dash_tasks = db
                    .neighbors(board.epic_dashboard, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(dash_tasks.len(), 3);
                let dash_task_ids: Vec<u64> = dash_tasks.iter().map(|n| n.id).collect();
                assert!(dash_task_ids.contains(&board.task_design_dashboard));
                assert!(dash_task_ids.contains(&board.task_analytics_api));
                assert!(dash_task_ids.contains(&board.task_integrate_charts));
            }

            #[test]
            fn task_belongs_to_epic_via_incoming() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // From a task, look back to find its epic
                let parent_epics = db
                    .neighbors(
                        board.task_oauth_backend,
                        Direction::Incoming,
                        Some("contains"),
                    )
                    .unwrap();
                assert_eq!(parent_epics.len(), 1);
                assert_eq!(parent_epics[0].id, board.epic_auth);
            }

            // -----------------------------------------------------------------
            // 3. Dependency chains — blocking BFS
            // -----------------------------------------------------------------

            #[test]
            fn direct_blocking_relationship() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // OAuth backend blocks auth tests
                let blocked_by_oauth = db
                    .neighbors(
                        board.task_oauth_backend,
                        Direction::Outgoing,
                        Some("blocks"),
                    )
                    .unwrap();
                assert_eq!(blocked_by_oauth.len(), 1);
                assert_eq!(blocked_by_oauth[0].id, board.task_auth_tests);
            }

            #[test]
            fn blocking_chain_via_bfs() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // BFS from OAuth backend following "blocks" edges should find:
                // auth_tests (depth 1), deploy_auth (depth 2)
                // BFS does NOT include the start node
                let chain = db
                    .bfs(
                        board.task_oauth_backend,
                        10,
                        Direction::Outgoing,
                        Some("blocks"),
                    )
                    .unwrap();

                let chain_ids: Vec<u64> = chain.iter().map(|n| n.id).collect();
                assert!(chain_ids.contains(&board.task_auth_tests));
                assert!(chain_ids.contains(&board.task_deploy_auth));
                assert_eq!(chain.len(), 2);
            }

            #[test]
            fn reverse_blocking_chain() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // From deploy_auth, trace back what blocks it (start node excluded)
                let blockers = db
                    .bfs(
                        board.task_deploy_auth,
                        10,
                        Direction::Incoming,
                        Some("blocks"),
                    )
                    .unwrap();

                let blocker_ids: Vec<u64> = blockers.iter().map(|n| n.id).collect();
                assert!(blocker_ids.contains(&board.task_auth_tests));
                assert!(blocker_ids.contains(&board.task_oauth_backend));
                assert_eq!(blockers.len(), 2);
            }

            #[test]
            fn dependency_relationship() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Login page depends on OAuth backend
                let deps = db
                    .neighbors(
                        board.task_design_login,
                        Direction::Outgoing,
                        Some("depends_on"),
                    )
                    .unwrap();
                assert_eq!(deps.len(), 1);
                assert_eq!(deps[0].id, board.task_oauth_backend);
            }

            #[test]
            fn dfs_blocking_traversal() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // DFS from OAuth backend following "blocks" edges (start excluded)
                let chain = db
                    .dfs(
                        board.task_oauth_backend,
                        10,
                        Direction::Outgoing,
                        Some("blocks"),
                    )
                    .unwrap();

                let chain_ids: Vec<u64> = chain.iter().map(|n| n.id).collect();
                assert!(chain_ids.contains(&board.task_auth_tests));
                assert!(chain_ids.contains(&board.task_deploy_auth));
                assert_eq!(chain.len(), 2);
            }

            // -----------------------------------------------------------------
            // 4. Shortest path — critical dependency path
            // -----------------------------------------------------------------

            #[test]
            fn shortest_path_through_blocking_chain() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Find the shortest path from OAuth backend to deploy
                let path = db
                    .shortest_path(board.task_oauth_backend, board.task_deploy_auth)
                    .unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 3); // oauth → auth_tests → deploy
                assert_eq!(path[0], board.task_oauth_backend);
                assert_eq!(path[1], board.task_auth_tests);
                assert_eq!(path[2], board.task_deploy_auth);
            }

            #[test]
            fn shortest_path_cross_epic() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Analytics API to integrate charts is a direct block
                let path = db
                    .shortest_path(board.task_analytics_api, board.task_integrate_charts)
                    .unwrap();
                assert!(path.is_some());
                let path = path.unwrap();
                assert_eq!(path.len(), 2);
                assert_eq!(path[0], board.task_analytics_api);
                assert_eq!(path[1], board.task_integrate_charts);
            }

            // -----------------------------------------------------------------
            // 5. Sprint board view — kind_index
            // -----------------------------------------------------------------

            #[test]
            fn kind_index_lists_all_tasks() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let tasks = db.list_nodes_by_kind("task", 100, 0).unwrap();
                assert_eq!(tasks.len(), 7); // 4 auth + 3 dashboard
            }

            #[test]
            fn kind_index_lists_all_developers() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let devs = db.list_nodes_by_kind("developer", 100, 0).unwrap();
                assert_eq!(devs.len(), 3);
            }

            #[test]
            fn kind_index_lists_all_epics() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let epics = db.list_nodes_by_kind("epic", 100, 0).unwrap();
                assert_eq!(epics.len(), 2);
            }

            #[test]
            fn kind_index_lists_all_sprints() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let sprints = db.list_nodes_by_kind("sprint", 100, 0).unwrap();
                assert_eq!(sprints.len(), 2);
            }

            #[test]
            fn kind_index_lists_all_components() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let components = db.list_nodes_by_kind("component", 100, 0).unwrap();
                assert_eq!(components.len(), 4);
            }

            #[test]
            fn kind_index_pagination() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let page1 = db.list_nodes_by_kind("task", 3, 0).unwrap();
                assert_eq!(page1.len(), 3);

                let page2 = db.list_nodes_by_kind("task", 3, 3).unwrap();
                assert_eq!(page2.len(), 3);

                let page3 = db.list_nodes_by_kind("task", 3, 6).unwrap();
                assert_eq!(page3.len(), 1);
            }

            // -----------------------------------------------------------------
            // 6. Sprint board — tasks in a sprint
            // -----------------------------------------------------------------

            #[test]
            fn sprint_1_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Sprint 1 has incoming part_of edges from tasks
                let sprint1_tasks = db
                    .neighbors(board.sprint_1, Direction::Incoming, Some("part_of"))
                    .unwrap();
                // Sprint 1: design_login, oauth_backend, auth_tests, design_dashboard
                assert_eq!(sprint1_tasks.len(), 4);
                let ids: Vec<u64> = sprint1_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_design_login));
                assert!(ids.contains(&board.task_oauth_backend));
                assert!(ids.contains(&board.task_auth_tests));
                assert!(ids.contains(&board.task_design_dashboard));
            }

            #[test]
            fn sprint_2_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let sprint2_tasks = db
                    .neighbors(board.sprint_2, Direction::Incoming, Some("part_of"))
                    .unwrap();
                // Sprint 2: deploy_auth, analytics_api, integrate_charts
                assert_eq!(sprint2_tasks.len(), 3);
                let ids: Vec<u64> = sprint2_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_deploy_auth));
                assert!(ids.contains(&board.task_analytics_api));
                assert!(ids.contains(&board.task_integrate_charts));
            }

            // -----------------------------------------------------------------
            // 7. Developer workload — tasks assigned to a developer
            // -----------------------------------------------------------------

            #[test]
            fn alice_workload() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let alice_tasks = db
                    .neighbors(board.dev_alice, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(alice_tasks.len(), 3);
                let ids: Vec<u64> = alice_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_design_login));
                assert!(ids.contains(&board.task_auth_tests));
                assert!(ids.contains(&board.task_design_dashboard));
            }

            #[test]
            fn bob_workload() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let bob_tasks = db
                    .neighbors(board.dev_bob, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(bob_tasks.len(), 2);
                let ids: Vec<u64> = bob_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_oauth_backend));
                assert!(ids.contains(&board.task_analytics_api));
            }

            #[test]
            fn charlie_workload() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let charlie_tasks = db
                    .neighbors(board.dev_charlie, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(charlie_tasks.len(), 2);
                let ids: Vec<u64> = charlie_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_deploy_auth));
                assert!(ids.contains(&board.task_integrate_charts));
            }

            #[test]
            fn task_assignee_from_task_side() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // From task, find who it's assigned to
                let assignee = db
                    .neighbors(
                        board.task_oauth_backend,
                        Direction::Outgoing,
                        Some("assigned_to"),
                    )
                    .unwrap();
                assert_eq!(assignee.len(), 1);
                assert_eq!(assignee[0].id, board.dev_bob);
            }

            // -----------------------------------------------------------------
            // 8. Component ownership
            // -----------------------------------------------------------------

            #[test]
            fn frontend_component_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let frontend_tasks = db
                    .neighbors(board.comp_frontend, Direction::Incoming, Some("part_of"))
                    .unwrap();
                assert_eq!(frontend_tasks.len(), 3);
                let ids: Vec<u64> = frontend_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_design_login));
                assert!(ids.contains(&board.task_design_dashboard));
                assert!(ids.contains(&board.task_integrate_charts));
            }

            #[test]
            fn backend_component_tasks() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let backend_tasks = db
                    .neighbors(board.comp_backend, Direction::Incoming, Some("part_of"))
                    .unwrap();
                assert_eq!(backend_tasks.len(), 2);
                let ids: Vec<u64> = backend_tasks.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_oauth_backend));
                assert!(ids.contains(&board.task_analytics_api));
            }

            // -----------------------------------------------------------------
            // 9. FTS — searching for tasks by description
            // -----------------------------------------------------------------

            #[test]
            fn fts_find_oauth_task() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let results = db.search_fts("OAuth", 10).unwrap();
                assert!(!results.is_empty());
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                assert!(titles.contains(&"Implement OAuth backend"));
            }

            #[test]
            fn fts_find_by_description_keyword() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let results = db.search_fts("Kubernetes", 10).unwrap();
                assert!(!results.is_empty());
                let found_deploy = results
                    .iter()
                    .any(|r| r.node.title == "Deploy auth service");
                assert!(found_deploy);
            }

            #[test]
            fn fts_find_developer_by_specialty() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let results = db.search_fts("authentication", 10).unwrap();
                assert!(!results.is_empty());
                // Should find Bob (specializes in auth) and/or OAuth task
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                let found_relevant = titles.contains(&"Bob")
                    || titles.contains(&"Implement OAuth backend")
                    || titles.contains(&"User Authentication");
                assert!(found_relevant);
            }

            #[test]
            fn fts_find_dashboard_tasks() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let results = db.search_fts("dashboard", 10).unwrap();
                assert!(!results.is_empty());
                let titles: Vec<&str> = results.iter().map(|r| r.node.title.as_str()).collect();
                // Should find dashboard-related nodes
                let found_dashboard =
                    titles.contains(&"Design dashboard layout") || titles.contains(&"Dashboard");
                assert!(found_dashboard);
            }

            // -----------------------------------------------------------------
            // 10. Subgraph — full context of a task
            // -----------------------------------------------------------------

            #[test]
            fn subgraph_of_blocked_task() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Subgraph around auth_tests at depth 1 should include:
                // the task itself, its blocker (oauth_backend), what it blocks (deploy_auth),
                // its assignee (alice), its component (testing), its sprint (sprint-1), its epic (auth)
                let sg = db.subgraph(board.task_auth_tests, 1).unwrap();
                let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                assert!(node_ids.contains(&board.task_auth_tests));
                // incoming blocks from oauth_backend
                assert!(node_ids.contains(&board.task_oauth_backend));
                // outgoing blocks to deploy_auth
                assert!(node_ids.contains(&board.task_deploy_auth));
                // assigned_to alice
                assert!(node_ids.contains(&board.dev_alice));
                // part_of testing component
                assert!(node_ids.contains(&board.comp_testing));
                // part_of sprint 1
                assert!(node_ids.contains(&board.sprint_1));
            }

            #[test]
            fn subgraph_depth_2_from_epic() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Depth 2 from epic_auth: epic → tasks → (assignees, components, sprints, blockers)
                let sg = db.subgraph(board.epic_auth, 2).unwrap();
                let node_ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();

                // Epic itself
                assert!(node_ids.contains(&board.epic_auth));
                // All 4 auth tasks
                assert!(node_ids.contains(&board.task_design_login));
                assert!(node_ids.contains(&board.task_oauth_backend));
                assert!(node_ids.contains(&board.task_auth_tests));
                assert!(node_ids.contains(&board.task_deploy_auth));
                // Depth 2: developers assigned to auth tasks
                assert!(node_ids.contains(&board.dev_alice));
                assert!(node_ids.contains(&board.dev_bob));
                assert!(node_ids.contains(&board.dev_charlie));
            }

            // -----------------------------------------------------------------
            // 11. Edge operations — edge kind index
            // -----------------------------------------------------------------

            #[test]
            fn edge_kind_index_blocks() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let blocks_edges = db.list_edges_by_kind("blocks", 100, 0).unwrap();
                // oauth→auth_tests, auth_tests→deploy, analytics→charts
                assert_eq!(blocks_edges.len(), 3);
            }

            #[test]
            fn edge_kind_index_assigned_to() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let assigned = db.list_edges_by_kind("assigned_to", 100, 0).unwrap();
                assert_eq!(assigned.len(), 7); // 7 tasks, each assigned to one developer
            }

            #[test]
            fn edge_kind_index_contains() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let contains = db.list_edges_by_kind("contains", 100, 0).unwrap();
                assert_eq!(contains.len(), 7); // 4 auth + 3 dashboard
            }

            #[test]
            fn edge_kind_index_depends_on() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let deps = db.list_edges_by_kind("depends_on", 100, 0).unwrap();
                assert_eq!(deps.len(), 2); // login→oauth, dashboard→analytics
            }

            // -----------------------------------------------------------------
            // 12. Task lifecycle — CRUD operations in context
            // -----------------------------------------------------------------

            #[test]
            fn update_task_status() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Move task from in_progress to done
                let mut new_props = HashMap::new();
                new_props.insert("status".to_string(), serde_json::json!("done"));
                new_props.insert("priority".to_string(), serde_json::json!("critical"));
                new_props.insert("story_points".to_string(), serde_json::json!(8));

                let updated = db
                    .update_node(
                        board.task_oauth_backend,
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
                    &serde_json::json!("done")
                );
            }

            #[test]
            fn add_new_task_to_sprint() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Add a new task mid-sprint
                let new_task = db
                    .create_node(make_node_with_props(
                        "task",
                        "Fix OAuth token refresh",
                        "Token refresh fails after 30 minutes, needs fix",
                        task_props("todo", "critical", 3),
                    ))
                    .unwrap();

                // Link to epic, sprint, developer, component
                db.create_edge(make_edge(board.epic_auth, new_task.id, "contains"))
                    .unwrap();
                db.create_edge(make_edge(new_task.id, board.sprint_1, "part_of"))
                    .unwrap();
                db.create_edge(make_edge(new_task.id, board.dev_bob, "assigned_to"))
                    .unwrap();
                db.create_edge(make_edge(new_task.id, board.comp_backend, "part_of"))
                    .unwrap();

                // Make it block deploy too
                db.create_edge(make_edge(new_task.id, board.task_deploy_auth, "blocks"))
                    .unwrap();

                // Verify
                let auth_tasks = db
                    .neighbors(board.epic_auth, Direction::Outgoing, Some("contains"))
                    .unwrap();
                assert_eq!(auth_tasks.len(), 5);

                let bob_tasks = db
                    .neighbors(board.dev_bob, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(bob_tasks.len(), 3);
            }

            #[test]
            fn delete_task_cascades_edges() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Delete a task and verify edges are cleaned up
                db.delete_node(board.task_auth_tests).unwrap();

                // Node gone
                assert!(db.get_node(board.task_auth_tests).unwrap().is_none());

                // OAuth backend no longer blocks auth_tests
                let blocked = db
                    .neighbors(
                        board.task_oauth_backend,
                        Direction::Outgoing,
                        Some("blocks"),
                    )
                    .unwrap();
                assert!(!blocked.iter().any(|n| n.id == board.task_auth_tests));

                // Deploy auth no longer blocked by auth_tests
                let blockers = db
                    .neighbors(board.task_deploy_auth, Direction::Incoming, Some("blocks"))
                    .unwrap();
                assert!(!blockers.iter().any(|n| n.id == board.task_auth_tests));

                // Alice's workload reduced
                let alice_tasks = db
                    .neighbors(board.dev_alice, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(alice_tasks.len(), 2);
            }

            #[test]
            fn reassign_task() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Remove current assignment
                let edges = db
                    .edges_of(board.task_integrate_charts, Direction::Outgoing)
                    .unwrap();
                let assignment_edge = edges
                    .iter()
                    .find(|e| e.kind == "assigned_to" && e.to_id == board.dev_charlie)
                    .unwrap();
                db.delete_edge(assignment_edge.id).unwrap();

                // Reassign to Alice
                db.create_edge(make_edge(
                    board.task_integrate_charts,
                    board.dev_alice,
                    "assigned_to",
                ))
                .unwrap();

                // Verify
                let new_assignee = db
                    .neighbors(
                        board.task_integrate_charts,
                        Direction::Outgoing,
                        Some("assigned_to"),
                    )
                    .unwrap();
                assert_eq!(new_assignee.len(), 1);
                assert_eq!(new_assignee[0].id, board.dev_alice);

                let alice_tasks = db
                    .neighbors(board.dev_alice, Direction::Incoming, Some("assigned_to"))
                    .unwrap();
                assert_eq!(alice_tasks.len(), 4);
            }

            // -----------------------------------------------------------------
            // 13. Advanced queries — cross-cutting concerns
            // -----------------------------------------------------------------

            #[test]
            fn find_all_blocked_tasks() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                // Find tasks that have incoming "blocks" edges
                let blocks_edges = db.list_edges_by_kind("blocks", 100, 0).unwrap();
                let blocked_task_ids: Vec<u64> = blocks_edges.iter().map(|e| e.to_id).collect();

                // auth_tests, deploy_auth, integrate_charts are blocked
                let tasks = db.list_nodes_by_kind("task", 100, 0).unwrap();
                let blocked_tasks: Vec<&Node> = tasks
                    .iter()
                    .filter(|t| blocked_task_ids.contains(&t.id))
                    .collect();
                assert_eq!(blocked_tasks.len(), 3);
            }

            #[test]
            fn find_unblocked_tasks() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                // Tasks with no incoming "blocks" edges
                let blocks_edges = db.list_edges_by_kind("blocks", 100, 0).unwrap();
                let blocked_ids: Vec<u64> = blocks_edges.iter().map(|e| e.to_id).collect();

                let all_tasks = db.list_nodes_by_kind("task", 100, 0).unwrap();
                let unblocked: Vec<&Node> = all_tasks
                    .iter()
                    .filter(|t| !blocked_ids.contains(&t.id))
                    .collect();
                // design_login, oauth_backend, design_dashboard, analytics_api are unblocked
                assert_eq!(unblocked.len(), 4);
            }

            #[test]
            fn developer_properties_stored() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let alice = db.get_node(board.dev_alice).unwrap().unwrap();
                assert_eq!(
                    alice.properties.get("team").unwrap(),
                    &serde_json::json!("frontend")
                );
                assert_eq!(
                    alice.properties.get("email").unwrap(),
                    &serde_json::json!("alice@example.com")
                );
            }

            #[test]
            fn list_recent_shows_latest_nodes() {
                let db = $db_expr;
                let _board = build_sprint_board(&db);

                let recent = db.list_recent(5).unwrap();
                assert_eq!(recent.len(), 5);
                // Most recent should be later-created nodes
            }

            // -----------------------------------------------------------------
            // 14. Multi-hop traversals
            // -----------------------------------------------------------------

            #[test]
            fn bfs_all_edges_from_epic() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // BFS from auth epic with no edge filter, depth 1 (start excluded)
                let neighbors = db
                    .bfs(board.epic_auth, 1, Direction::Outgoing, None)
                    .unwrap();

                // Should include 4 tasks (contains edges), NOT the epic itself
                assert!(neighbors.len() >= 4);
                let ids: Vec<u64> = neighbors.iter().map(|n| n.id).collect();
                assert!(ids.contains(&board.task_design_login));
            }

            #[test]
            fn bfs_depth_limited() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // BFS depth 1 from OAuth backend following blocks (start excluded)
                let depth1 = db
                    .bfs(
                        board.task_oauth_backend,
                        1,
                        Direction::Outgoing,
                        Some("blocks"),
                    )
                    .unwrap();
                let ids1: Vec<u64> = depth1.iter().map(|n| n.id).collect();
                // Should find auth_tests (depth 1) but NOT deploy_auth (depth 2)
                assert_eq!(depth1.len(), 1);
                assert!(ids1.contains(&board.task_auth_tests));
                assert!(!ids1.contains(&board.task_deploy_auth));
            }

            // -----------------------------------------------------------------
            // 15. Edge updates — changing priority weights
            // -----------------------------------------------------------------

            #[test]
            fn update_edge_weight_priority() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Find the blocks edge from oauth → auth_tests
                let edges = db
                    .edges_of(board.task_oauth_backend, Direction::Outgoing)
                    .unwrap();
                let blocks_edge = edges.iter().find(|e| e.kind == "blocks").unwrap();

                // Increase priority weight
                let updated = db
                    .update_edge(
                        blocks_edge.id,
                        EdgePatch {
                            kind: None,
                            weight: Some(10.0),
                            properties: None,
                        },
                    )
                    .unwrap();
                assert_eq!(updated.weight, 10.0);
            }

            #[test]
            fn add_edge_properties() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // Create an edge with properties
                let mut props = HashMap::new();
                props.insert(
                    "reason".to_string(),
                    serde_json::json!("needs API ready first"),
                );
                props.insert("severity".to_string(), serde_json::json!("hard"));

                db.create_edge(make_edge_with_props(
                    board.task_analytics_api,
                    board.task_design_dashboard,
                    "blocks",
                    props,
                ))
                .unwrap();

                let edges = db
                    .edges_of(board.task_analytics_api, Direction::Outgoing)
                    .unwrap();
                let new_block = edges
                    .iter()
                    .find(|e| e.kind == "blocks" && e.to_id == board.task_design_dashboard)
                    .unwrap();
                assert_eq!(
                    new_block.properties.get("reason").unwrap(),
                    &serde_json::json!("needs API ready first")
                );
            }

            // -----------------------------------------------------------------
            // 16. Node lookup by title and UUID
            // -----------------------------------------------------------------

            #[test]
            fn find_task_by_title() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let node = db.get_node_by_title("Implement OAuth backend").unwrap();
                assert!(node.is_some());
                assert_eq!(node.unwrap().id, board.task_oauth_backend);
            }

            #[test]
            fn find_task_by_uuid() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let original = db.get_node(board.task_deploy_auth).unwrap().unwrap();
                let by_uuid = db.get_node_by_uuid(&original.uuid).unwrap();
                assert!(by_uuid.is_some());
                assert_eq!(by_uuid.unwrap().id, board.task_deploy_auth);
            }

            // -----------------------------------------------------------------
            // 17. Edges_of with direction filtering
            // -----------------------------------------------------------------

            #[test]
            fn edges_of_both_directions() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                // auth_tests has edges in both directions
                let all_edges = db.edges_of(board.task_auth_tests, Direction::Both).unwrap();
                // incoming: blocks from oauth, contains from epic
                // outgoing: blocks to deploy, assigned_to alice, part_of testing, part_of sprint1
                assert!(all_edges.len() >= 6);
            }

            #[test]
            fn edges_of_outgoing_only() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let outgoing = db
                    .edges_of(board.task_auth_tests, Direction::Outgoing)
                    .unwrap();
                // blocks deploy, assigned_to alice, part_of testing, part_of sprint1
                assert_eq!(outgoing.len(), 4);
                assert!(outgoing.iter().all(|e| e.from_id == board.task_auth_tests));
            }

            #[test]
            fn edges_of_incoming_only() {
                let db = $db_expr;
                let board = build_sprint_board(&db);

                let incoming = db
                    .edges_of(board.task_auth_tests, Direction::Incoming)
                    .unwrap();
                // blocks from oauth, contains from epic
                assert_eq!(incoming.len(), 2);
                assert!(incoming.iter().all(|e| e.to_id == board.task_auth_tests));
            }
        }
    };
}

// Run all tests against MemoryBackend
task_manager_tests!(memory_db(), memory);

// Run all tests against RedbBackend
#[cfg(feature = "redb-backend")]
mod redb {
    use super::*;

    macro_rules! task_manager_redb_tests {
        ($mod_name:ident) => {
            mod $mod_name {
                use super::*;
                use tempfile::TempDir;

                fn setup() -> (TempDir, Drevo) {
                    let dir = TempDir::new().expect("create temp dir");
                    let db = redb_db(&dir);
                    (dir, db)
                }

                #[test]
                fn build_complete_sprint_board() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    assert!(db.get_node(board.epic_auth).unwrap().is_some());
                    assert!(db.get_node(board.task_integrate_charts).unwrap().is_some());
                }

                #[test]
                fn blocking_chain_via_bfs() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let chain = db
                        .bfs(
                            board.task_oauth_backend,
                            10,
                            Direction::Outgoing,
                            Some("blocks"),
                        )
                        .unwrap();
                    assert_eq!(chain.len(), 2); // auth_tests + deploy_auth (start excluded)
                }

                #[test]
                fn sprint_board_view() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let sprint1_tasks = db
                        .neighbors(board.sprint_1, Direction::Incoming, Some("part_of"))
                        .unwrap();
                    assert_eq!(sprint1_tasks.len(), 4);
                }

                #[test]
                fn developer_workload() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let alice_tasks = db
                        .neighbors(board.dev_alice, Direction::Incoming, Some("assigned_to"))
                        .unwrap();
                    assert_eq!(alice_tasks.len(), 3);
                }

                #[test]
                fn kind_index_works() {
                    let (_dir, db) = setup();
                    let _board = build_sprint_board(&db);
                    let tasks = db.list_nodes_by_kind("task", 100, 0).unwrap();
                    assert_eq!(tasks.len(), 7);
                }

                #[test]
                fn fts_search() {
                    let (_dir, db) = setup();
                    let _board = build_sprint_board(&db);
                    let results = db.search_fts("OAuth", 10).unwrap();
                    assert!(!results.is_empty());
                }

                #[test]
                fn shortest_path_blocking_chain() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let path = db
                        .shortest_path(board.task_oauth_backend, board.task_deploy_auth)
                        .unwrap();
                    assert!(path.is_some());
                    assert_eq!(path.unwrap().len(), 3);
                }

                #[test]
                fn subgraph_extraction() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let sg = db.subgraph(board.task_auth_tests, 1).unwrap();
                    let ids: Vec<u64> = sg.nodes.iter().map(|n| n.id).collect();
                    assert!(ids.contains(&board.task_auth_tests));
                    assert!(ids.contains(&board.task_oauth_backend));
                    assert!(ids.contains(&board.dev_alice));
                }

                #[test]
                fn properties_persist() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    let task = db.get_node(board.task_oauth_backend).unwrap().unwrap();
                    assert_eq!(
                        task.properties.get("status").unwrap(),
                        &serde_json::json!("in_progress")
                    );
                }

                #[test]
                fn delete_cascades_edges() {
                    let (_dir, db) = setup();
                    let board = build_sprint_board(&db);
                    db.delete_node(board.task_auth_tests).unwrap();
                    assert!(db.get_node(board.task_auth_tests).unwrap().is_none());
                    let blocked = db
                        .neighbors(
                            board.task_oauth_backend,
                            Direction::Outgoing,
                            Some("blocks"),
                        )
                        .unwrap();
                    assert!(!blocked.iter().any(|n| n.id == board.task_auth_tests));
                }
            }
        };
    }

    task_manager_redb_tests!(redb_backend);
}
