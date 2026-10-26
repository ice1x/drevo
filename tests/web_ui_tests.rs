//! Phase 15 task `00092` — integration tests for the embedded
//! Web UI handlers.
//!
//! Sibling to `src/web_ui.rs` (unit tests). This file drives the
//! actual `axum::Router` returned by [`drevo::native_api::build_native_router`]
//! via `tower::ServiceExt::oneshot` — the same pattern used by
//! `tests/http_api_tests.rs`. No real TCP listener; the router is
//! exercised in-process so the tests run on any CI cell that
//! compiles the `http` feature.
//!
//! Scope:
//!   * each new route serves the expected payload with the right
//!     `Content-Type`;
//!   * `GET /ui/` redirects to `/ui` (so root-relative `/ui/app.js`
//!     in `index.html` resolves correctly regardless of trailing
//!     slash);
//!   * unknown paths under `/ui/` fall through to the existing 404
//!     handler (no accidental open static-file directory).

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(NativeService::in_memory());
    let state = NativeApiState::new(db);
    build_native_router(state)
}

/// Send a `GET` and return (status, content-type header, body bytes).
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec();
    (status, content_type, bytes)
}

// ── Routes serve their assets ──────────────────────────────────────────

#[tokio::test]
async fn ui_root_returns_html_200() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("text/html"),
        "GET /ui must return text/html, got {ct:?}"
    );
    let body = String::from_utf8(bytes).expect("utf-8");
    // Smoke check: the embedded HTML must include the DOM ids the
    // front-end script reaches for.
    assert!(body.contains("id=\"cy\""));
    assert!(body.contains("id=\"search-input\""));
}

#[tokio::test]
async fn ui_app_js_returns_javascript_200() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("text/javascript"),
        "GET /ui/app.js must return text/javascript, got {ct:?}"
    );
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(
        body.contains("cytoscape("),
        "app.js must initialise Cytoscape"
    );
    assert!(
        body.contains("/search/fts"),
        "app.js must call the FTS endpoint"
    );
}

// ── Stale-cache invalidation (issue #433) ──────────────────────────────
// The client-side sample cache must be re-fetchable so a graph mutated on
// the server (over Bolt or another client) does not keep showing deleted or
// stale nodes/edges until a hard reload.

#[tokio::test]
async fn ui_offers_a_graph_refresh_control() {
    let app = make_app();
    let (_, _, bytes) = get(&app, "/ui").await;
    let html = String::from_utf8(bytes).expect("utf-8");
    assert!(
        html.contains("id=\"graph-refresh\""),
        "index.html must expose a Refresh control to re-fetch the graph (#433)"
    );
}

#[tokio::test]
async fn ui_app_js_reloads_the_overview_after_writes_and_on_refresh() {
    let app = make_app();
    let (_, _, bytes) = get(&app, "/ui/app.js").await;
    let js = String::from_utf8(bytes).expect("utf-8");
    // The Refresh control re-fetches the pool.
    assert!(
        js.contains("graph-refresh"),
        "app.js must wire the #graph-refresh control (#433)"
    );
    // A successful UI graph write invalidates the cache by re-fetching.
    assert!(
        js.contains("isGraphWrite"),
        "app.js must detect graph writes to invalidate the sample cache (#433)"
    );
    // Both paths re-run the pool fetch.
    assert!(
        js.matches("loadOverview()").count() >= 3,
        "loadOverview() must be reachable from the write path and the Refresh \
         control, not only on first load (#433)"
    );
}

// ── Layout stability: a click must not re-run the physics ──────────────
// The live cola simulation must arm on an actual drag, not on `grab`
// (mouse-down), so that merely clicking/selecting a node does not kick off a
// whole-graph re-simulation that visibly reshuffles the layout.

#[tokio::test]
async fn ui_live_physics_arms_on_drag_not_on_grab() {
    let app = make_app();
    let (_, _, bytes) = get(&app, "/ui/app.js").await;
    let js = String::from_utf8(bytes).expect("utf-8");
    assert!(
        js.contains(r#"cy.on("drag", "node""#),
        "the live layout must start on a real drag"
    );
    assert!(
        !js.contains(r#"cy.on("grab", "node""#),
        "the live layout must NOT arm on grab (a bare click would reshuffle the graph)"
    );
    // Hierarchies lay out crossing-free rather than tangling under a force layout.
    assert!(
        js.contains("breadthfirst"),
        "a forest/tree must use the hierarchical breadthfirst layout"
    );
    // The live cola sim's spring rest-length must be derived from the CURRENT
    // on-screen geometry, not a hardcoded constant, and PER EDGE — a shared mean
    // springs every off-mean edge (a hub's long and short edges) toward one
    // value, jolting the whole graph on the first drag tick. Each edge gets its
    // own current length as its rest length so every spring starts at rest and
    // only the dragged neighbourhood moves.
    assert!(
        js.contains("edgeLength: (edge)"),
        "the live layout must set a PER-EDGE spring rest-length, not a shared constant/mean"
    );
    assert!(
        js.contains("DrevoGraphMath.segmentLength"),
        "the per-edge length math must come from the unit-tested graph_math.js module"
    );
    assert!(
        js.contains("DrevoGraphMath.meanEdgeLength"),
        "the mean edge length (the per-edge fallback) still comes from graph_math.js"
    );
    assert!(
        !js.contains("edgeLength: 150"),
        "the live layout must NOT hardcode a spring length that can desync from the static layout"
    );
    // Repacking disconnected components on sim start hurls peripheral nodes
    // across the canvas — the other half of the drag jolt.
    assert!(
        js.contains("handleDisconnected: false"),
        "the live layout must NOT repack disconnected components (that jolts peripheral nodes)"
    );
    // The live sim must run ONLY on the dragged node's closed neighbourhood, not
    // the whole graph. cola is a stress layout: over the full graph its all-pairs
    // model reflows every node toward cola's own equilibrium (≠ the fcose layout
    // the graph is sitting in), so distant nodes visibly spread apart during a
    // drag even though only smooth-adjacent springs were tuned. Confining the
    // layout's element set to the 1-hop neighbourhood freezes everything else, so
    // only the dragged node's direct neighbours tug along.
    assert!(
        js.contains("closedNeighborhood()"),
        "the live layout must be confined to the dragged node's closed neighbourhood"
    );
    assert!(
        !js.contains("cy.layout(colaLiveOptions())"),
        "the live cola sim must run on the neighbourhood collection, not the whole cy graph \
         (whole-graph cola stress-reflows distant nodes and they drift apart during a drag)"
    );
}

// The pure geometry module app.js depends on must be served same-origin and
// must load before app.js (which reads `window.DrevoGraphMath` at drag time).
#[tokio::test]
async fn ui_serves_graph_math_module_before_app_js() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/graph_math.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("text/javascript"),
        "GET /ui/graph_math.js must return javascript, got {ct:?}"
    );
    let js = String::from_utf8(bytes).expect("utf-8");
    assert!(
        js.contains("meanEdgeLength") && js.contains("DrevoGraphMath"),
        "graph_math.js must export meanEdgeLength on DrevoGraphMath"
    );
    assert!(
        js.contains("nodeLabel") && js.contains("__cypher__:"),
        "graph_math.js must export nodeLabel, which hides placeholder titles (#545)"
    );
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    assert!(
        app_js.contains("DrevoGraphMath.nodeLabel("),
        "app.js must label nodes through nodeLabel, never the raw title"
    );

    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let gm = idx
        .find("/ui/graph_math.js")
        .expect("index.html must load graph_math.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(
        gm < appjs,
        "graph_math.js must be loaded before app.js so DrevoGraphMath exists"
    );
}

// Query results (#613): the pure results-view helpers are served same-origin
// after graph_math.js (they label nodes through it) and before app.js, and the
// page has the Graph / Table / JSON tabs over the main area.
#[tokio::test]
async fn ui_serves_results_view_module_and_result_tabs() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/results_view.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in ["DrevoResultsView", "cellValue", "defaultView", "toCsv"] {
        assert!(js.contains(needle), "results_view.js must define {needle}");
    }

    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let gm = idx
        .find("/ui/graph_math.js")
        .expect("index.html must load graph_math.js");
    let rv = idx
        .find("/ui/results_view.js")
        .expect("index.html must load results_view.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(
        gm < rv && rv < appjs,
        "load order: graph_math.js, results_view.js, app.js"
    );
    for id in [
        "id=\"view-tabs\"",
        "data-view=\"graph\"",
        "data-view=\"table\"",
        "data-view=\"json\"",
        "id=\"table-view\"",
        "id=\"json-view\"",
        "id=\"copy-csv\"",
    ] {
        assert!(idx.contains(id), "index.html must contain `{id}`");
    }

    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoResultsView.cellValue(",
        "DrevoResultsView.defaultView(",
        "DrevoResultsView.toCsv(",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
}

// Query editor (#615): the query bar is a multi-line textarea with a history
// menu, and the pure history / run-key helpers are served before app.js.
#[tokio::test]
async fn ui_serves_query_history_module_and_multiline_editor() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/query_history.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoQueryHistory",
        "pushQuery",
        "stepHistory",
        "parseHistory",
        "isRunKey",
    ] {
        assert!(js.contains(needle), "query_history.js must define {needle}");
    }

    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let qh = idx
        .find("/ui/query_history.js")
        .expect("index.html must load query_history.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(qh < appjs, "query_history.js must load before app.js");
    for needle in [
        "<textarea id=\"search-input\"",
        "id=\"history-toggle\"",
        "id=\"history-menu\"",
    ] {
        assert!(idx.contains(needle), "index.html must contain `{needle}`");
    }

    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoQueryHistory.pushQuery(",
        "DrevoQueryHistory.stepHistory(",
        "DrevoQueryHistory.parseHistory(",
        "DrevoQueryHistory.isRunKey(",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
}

// Ask (#616): the query bar has an Ask button that sends the text to
// `drevo.cypher.fromText` through the tested helpers and reads the server's
// text-to-Cypher status from `GET /`.
#[tokio::test]
async fn ui_has_ask_button_wired_to_text_to_cypher() {
    let app = make_app();
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    assert!(
        idx.contains("id=\"ask-button\""),
        "index.html must have the Ask button"
    );

    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoQueryHistory.fromTextRequest(",
        "DrevoQueryHistory.generatedCypher(",
        "info.text2cypher",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }

    let (_, _, js) = get(&app, "/ui/query_history.js").await;
    let js = String::from_utf8(js).expect("utf-8");
    assert!(js.contains("drevo.cypher.fromText($question)"));
}

// Label chips (#620): a filter box and a show-all toggle over the chips, and
// app.js picks the chips through the tested helper.
#[tokio::test]
async fn ui_label_chips_have_filter_and_show_all_toggle() {
    let app = make_app();
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    for id in ["id=\"kind-filter\"", "id=\"kind-more\""] {
        assert!(idx.contains(id), "index.html must contain `{id}`");
    }
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    assert!(app_js.contains("DrevoGraphMath.visibleKinds("));
    let (_, _, gm) = get(&app, "/ui/graph_math.js").await;
    assert!(String::from_utf8(gm)
        .expect("utf-8")
        .contains("function visibleKinds"));
}

// Inspector (#622): property values go through the tested formatter, long
// text is clipped, and a relationship's ends are links to their nodes.
#[tokio::test]
async fn ui_inspector_formats_values_and_links_relationship_ends() {
    let app = make_app();
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoResultsView.propertyValue(",
        "DrevoResultsView.clipText(",
        "inspector-link",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
    let (_, _, rv) = get(&app, "/ui/results_view.js").await;
    let rv = String::from_utf8(rv).expect("utf-8");
    assert!(rv.contains("function propertyValue") && rv.contains("function clipText"));
}

// Colour by (#624): a select and a legend in the left rail, driven by the
// tested helpers.
#[tokio::test]
async fn ui_colours_nodes_by_a_chosen_property() {
    let app = make_app();
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    for id in ["id=\"color-by\"", "id=\"color-legend\""] {
        assert!(idx.contains(id), "index.html must contain `{id}`");
    }
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoGraphMath.colorCandidates(",
        "DrevoGraphMath.colorValue(",
        "DrevoGraphMath.legendEntries(",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
}

// Hub labels (#626): node size, hubs and label size come from the tested
// helpers, and hubs / the clicked neighbourhood keep their labels at any zoom.
#[tokio::test]
async fn ui_labels_hubs_and_focus_at_any_zoom() {
    let app = make_app();
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoGraphMath.hubIds(",
        "DrevoGraphMath.nodeSize(",
        "DrevoGraphMath.labelFontSize(",
        "node.hub, node.focus",
        "\"min-zoomed-font-size\": 0",
    ] {
        assert!(app_js.contains(needle), "app.js must contain {needle}");
    }
}

// Request progress (#628): the status bar counts up while a request runs and
// a Stop button abandons it in the browser.
#[tokio::test]
async fn ui_shows_request_progress_with_a_stop_button() {
    let app = make_app();
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    assert!(
        idx.contains("id=\"stop-request\""),
        "index.html must have the Stop button"
    );
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoQueryHistory.elapsedText(",
        "new AbortController()",
        "signal: req.controller.signal",
    ] {
        assert!(app_js.contains(needle), "app.js must contain {needle}");
    }
}

// Node menu (#630): a right-click on a node opens a menu whose Cypher comes
// from the tested helpers.
#[tokio::test]
async fn ui_has_a_node_context_menu() {
    let app = make_app();
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    assert!(
        idx.contains("id=\"node-menu\""),
        "index.html must have the node menu"
    );
    for action in ["expand", "focus", "query", "copy-cypher", "copy-id", "hide"] {
        assert!(
            idx.contains(&format!("data-action=\"{action}\"")),
            "the node menu must offer {action}"
        );
    }
    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "\"cxttap\", \"node\"",
        "DrevoQueryHistory.nodeQuery(",
        "DrevoQueryHistory.neighboursQuery(",
    ] {
        assert!(app_js.contains(needle), "app.js must contain {needle}");
    }
}

// Sign-in (#634): the pure sign-in helpers are served before app.js, the page
// has the sign-in and password-change dialogs and the user menu, and app.js
// drives them through /auth/me, /auth/login and /auth/logout.
#[tokio::test]
async fn ui_serves_sign_in_module_and_dialogs() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/auth.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoAuth",
        "authView",
        "signInError",
        "changePasswordRequest",
    ] {
        assert!(js.contains(needle), "auth.js must define {needle}");
    }

    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let auth = idx
        .find("/ui/auth.js")
        .expect("index.html must load auth.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(auth < appjs, "auth.js must load before app.js");
    for id in [
        "id=\"sign-in-modal\"",
        "id=\"sign-in-form\"",
        "id=\"sign-in-user\"",
        "id=\"sign-in-password\"",
        "id=\"sign-in-msg\"",
        "id=\"change-password-form\"",
        "id=\"user-menu\"",
        "id=\"sign-out\"",
    ] {
        assert!(idx.contains(id), "index.html must contain `{id}`");
    }

    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "DrevoAuth.authView(",
        "DrevoAuth.signInError(",
        "DrevoAuth.changePasswordRequest(",
        "\"/auth/me\"",
        "\"/auth/login\"",
        "\"/auth/logout\"",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
}

// The Storage panel lists running transactions with a Terminate button: the
// pure helpers are served same-origin before app.js, and the page has the
// list's container.
#[tokio::test]
async fn ui_serves_transactions_view_module_and_the_storage_list() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/transactions_view.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoTransactionsView",
        "SHOW_QUERY",
        "rowsFrom",
        "formatElapsed",
        "terminateRequest",
    ] {
        assert!(
            js.contains(needle),
            "transactions_view.js must define {needle}"
        );
    }
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let tv = idx
        .find("/ui/transactions_view.js")
        .expect("index.html must load transactions_view.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(tv < appjs, "transactions_view.js must load before app.js");
    assert!(idx.contains("id=\"storage-transactions\""));
}

// The top bar marks read-only databases: the pure helpers are served
// same-origin before app.js, and the picker has the badge.
#[tokio::test]
async fn ui_serves_db_access_module_and_the_read_only_badge() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/db_access.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoDbAccess",
        "isReadOnly",
        "optionLabel",
        "canCreateDatabases",
        "writeBlockedReason",
    ] {
        assert!(js.contains(needle), "db_access.js must define {needle}");
    }
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let da = idx
        .find("/ui/db_access.js")
        .expect("index.html must load db_access.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(da < appjs, "db_access.js must load before app.js");
    assert!(
        idx.contains("id=\"db-access\""),
        "the picker must hold the badge"
    );
}

// The Storage panel takes physical backups: the pure helpers are served
// same-origin before app.js, and the page has the list and both buttons.
#[tokio::test]
async fn ui_serves_backups_view_module_and_the_backup_buttons() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/backups_view.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoBackupsView",
        "/admin/backups",
        "/admin/backup",
        "backupRequest",
        "summary",
    ] {
        assert!(js.contains(needle), "backups_view.js must define {needle}");
    }
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let bv = idx
        .find("/ui/backups_view.js")
        .expect("index.html must load backups_view.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(bv < appjs, "backups_view.js must load before app.js");
    for id in [
        "id=\"storage-backups-list\"",
        "id=\"storage-backup-full\"",
        "id=\"storage-backup-incremental\"",
    ] {
        assert!(idx.contains(id), "the Storage panel must hold {id}");
    }
}

// The live overview: its pure helpers are served same-origin before app.js,
// the overview panel has the switch, and the stream it follows is served.
#[tokio::test]
async fn ui_serves_live_refresh_module_and_the_live_switch() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/live_refresh.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in [
        "DrevoLiveRefresh",
        "CHANGE_EVENTS",
        "eventsUrl",
        "changeAction",
        "createCoalescer",
        "readLivePref",
    ] {
        assert!(js.contains(needle), "live_refresh.js must define {needle}");
    }
    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let live = idx
        .find("/ui/live_refresh.js")
        .expect("index.html must load live_refresh.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(live < appjs, "live_refresh.js must load before app.js");
    assert!(idx.contains("id=\"graph-live\""));
}

// Problem notifications + "Report a problem" (#552): the pure report helpers
// are served same-origin before app.js, the page has the toast stack, the
// topbar button and the report dialog, and app.js wires them to the server.
#[tokio::test]
async fn ui_serves_report_module_and_wires_problem_reporting() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/report.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/javascript"), "got {ct:?}");
    let js = String::from_utf8(bytes).expect("utf-8");
    for needle in ["DrevoReport", "issueUrl", "buildIssue", "toastFromProblem"] {
        assert!(js.contains(needle), "report.js must define {needle}");
    }

    let (_, _, idx) = get(&app, "/ui").await;
    let idx = String::from_utf8(idx).expect("utf-8");
    let report = idx
        .find("/ui/report.js")
        .expect("index.html must load report.js");
    let appjs = idx.find("/ui/app.js").expect("index.html must load app.js");
    assert!(report < appjs, "report.js must load before app.js");
    for id in [
        "id=\"toasts\"",
        "id=\"report-toggle\"",
        "id=\"report-modal\"",
        "id=\"report-note\"",
        "id=\"report-body\"",
        "id=\"report-shot\"",
        "id=\"report-open\"",
    ] {
        assert!(idx.contains(id), "index.html must contain {id}");
    }

    let (_, _, app_js) = get(&app, "/ui/app.js").await;
    let app_js = String::from_utf8(app_js).expect("utf-8");
    for needle in [
        "/problems?since=",
        "/report",
        "DrevoReport",
        "cy.png(",
        "ClipboardItem",
    ] {
        assert!(app_js.contains(needle), "app.js must use {needle}");
    }
}

#[tokio::test]
async fn ui_styles_css_returns_css_200() {
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/ui/styles.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ct.starts_with("text/css"),
        "GET /ui/styles.css must return text/css, got {ct:?}"
    );
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(body.contains(".layout"));
}

#[tokio::test]
async fn ui_root_with_trailing_slash_redirects_to_ui() {
    let app = make_app();
    let req = Request::builder()
        .method("GET")
        .uri("/ui/")
        .body(Body::empty())
        .expect("build request");
    let response = app.clone().oneshot(req).await.expect("router response");
    // `Redirect::permanent` → 308. The browser then re-issues against
    // `/ui` so that `<script src="/ui/app.js">` (a root-relative
    // path) resolves against the stable prefix.
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    let loc = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(loc, "/ui");
}

#[tokio::test]
async fn ui_unknown_subpath_falls_through_to_404() {
    let app = make_app();
    let (status, _ct, _bytes) = get(&app, "/ui/no-such-asset.png").await;
    // Unknown asset under `/ui/` must not silently serve anything —
    // the existing API fallback handler returns 404. Without this
    // assertion, accidentally adding `ServeDir` (which behaves
    // differently on missing files than the explicit-route pattern
    // we use here) would slip through review.
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Cache headers + payload sanity ─────────────────────────────────────

#[tokio::test]
async fn ui_assets_set_no_cache_for_dev() {
    let app = make_app();
    for uri in ["/ui", "/ui/app.js", "/ui/styles.css"] {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("build request");
        let response = app.clone().oneshot(req).await.expect("router response");
        let cache = response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cache.contains("no-cache"),
            "GET {uri} must set Cache-Control: no-cache (got {cache:?})"
        );
    }
}

#[tokio::test]
async fn ui_html_includes_search_form() {
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(body.contains("<form id=\"search-form\""));
    assert!(body.contains("type=\"text\""));
}

#[tokio::test]
async fn ui_html_includes_node_inspector() {
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(body.contains("class=\"inspector\""));
    assert!(body.contains("Node inspector"));
}

#[tokio::test]
async fn ui_html_settings_panel_configures_text_to_cypher() {
    // The Settings panel sets both providers at runtime: embeddings and the
    // text-to-Cypher upstream behind Ask.
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    for id in [
        "settings-text2cypher-status",
        "settings-text2cypher-upstream",
        "settings-text2cypher-key",
        "settings-text2cypher-model",
        "settings-text2cypher-save",
        "settings-text2cypher-msg",
    ] {
        assert!(body.contains(&format!("id=\"{id}\"")), "missing #{id}");
    }
    let (_status, _ct, bytes) = get(&app, "/ui/app.js").await;
    let js = String::from_utf8(bytes).expect("utf-8");
    assert!(
        js.contains("/config/text2cypher"),
        "app.js must call the endpoint"
    );
}

#[tokio::test]
async fn ui_html_settings_panel_sets_the_slow_statement_threshold() {
    // The slow-statement log can be turned on from the Settings panel while
    // an incident is happening (GET/POST /config/slow-query).
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    for id in [
        "settings-slowquery-status",
        "settings-slowquery-ms",
        "settings-slowquery-save",
        "settings-slowquery-msg",
    ] {
        assert!(body.contains(&format!("id=\"{id}\"")), "missing #{id}");
    }
    let (_status, _ct, bytes) = get(&app, "/ui/app.js").await;
    let js = String::from_utf8(bytes).expect("utf-8");
    assert!(
        js.contains("/config/slow-query"),
        "app.js must call the endpoint"
    );
}

#[tokio::test]
async fn ui_html_includes_storage_panel() {
    // The Storage panel (bloat + keyspaces + backup) is opened from the ⛁
    // topbar button and rendered into the #storage-modal dialog.
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(
        body.contains("id=\"storage-toggle\""),
        "topbar must expose the storage-panel trigger button"
    );
    assert!(
        body.contains("id=\"storage-modal\""),
        "the storage modal dialog must be present"
    );
    assert!(
        body.contains("id=\"storage-backup\""),
        "the panel must offer a GraphML backup download"
    );
    assert!(
        body.contains("id=\"storage-shrink\""),
        "the panel must offer the online shrink action"
    );
    assert!(
        body.contains("id=\"storage-bench-run\""),
        "the panel must offer the benchmark action"
    );
}

#[tokio::test]
async fn ui_app_js_calls_storage_endpoints() {
    // app.js must drive the panel off the real storage endpoints; a rename of
    // /storage/keyspaces (server side) without updating the UI would silently
    // break the panel — this pins the contract.
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui/app.js").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(
        body.contains("/storage/bloat"),
        "app.js must fetch bloat stats"
    );
    assert!(
        body.contains("/storage/keyspaces"),
        "app.js must fetch the keyspace breakdown"
    );
    assert!(
        body.contains("/export/graphml"),
        "app.js must wire the GraphML backup link"
    );
    assert!(
        body.contains("/storage/shrink"),
        "app.js must POST the online shrink action"
    );
    assert!(
        body.contains("/storage/benchmark"),
        "app.js must POST the benchmark action"
    );
}

#[tokio::test]
async fn ui_html_references_vendored_cytoscape() {
    // Cytoscape is vendored same-origin (see src/web_ui.rs) — the HTML
    // must point at /ui/vendor/, never a public CDN, so the WebUI works
    // offline / behind a CDN-blocking browser.
    let app = make_app();
    let (_status, _ct, bytes) = get(&app, "/ui").await;
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(body.contains("/ui/vendor/cytoscape.min.js"));
    assert!(!body.contains("src=\"http"), "no external <script src>");
}

#[tokio::test]
async fn ui_serves_vendored_javascript_bundles() {
    // Each vendored library must be reachable over the real router with a
    // JavaScript content-type and a non-trivial body. A 404 here is the
    // exact failure mode (canvas stuck on "connecting…") this route set
    // exists to prevent.
    let app = make_app();
    for path in [
        "/ui/vendor/cytoscape.min.js",
        "/ui/vendor/layout-base.js",
        "/ui/vendor/cose-base.js",
        "/ui/vendor/cytoscape-fcose.js",
        "/ui/vendor/cola.min.js",
        "/ui/vendor/cytoscape-cola.js",
    ] {
        let (status, ct, bytes) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK, "{path} must be served");
        assert!(
            ct.contains("javascript"),
            "{path} must have a JavaScript content-type, got `{ct}`"
        );
        assert!(bytes.len() > 10_000, "{path} body looks truncated");
    }
}

#[tokio::test]
async fn ui_overview_dependency_export_json_is_reachable() {
    // The on-load graph overview fetches /export/json once and renders a
    // bounded sample client-side. Lock that the endpoint the UI depends on
    // is wired and returns the {nodes, edges} dump shape.
    let app = make_app();
    let (status, ct, bytes) = get(&app, "/export/json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.contains("json"), "export must be JSON, got `{ct}`");
    let body = String::from_utf8(bytes).expect("utf-8");
    assert!(
        body.contains("\"nodes\"") && body.contains("\"edges\""),
        "export dump must carry nodes + edges arrays the overview reads"
    );
}

// ── Non-UI routes unchanged ────────────────────────────────────────────

#[tokio::test]
async fn existing_api_routes_still_respond() {
    // Regression guard: the new `/ui*` routes must not shadow the
    // existing API ones. Smoke-check `/health` because it's cheap.
    let app = make_app();
    let (status, _ct, _bytes) = get(&app, "/health").await;
    assert_eq!(status, StatusCode::OK);
}
