//! Phase 15 task `00092` — integration tests for the embedded
//! Web UI handlers.
//!
//! Sibling to `src/web_ui.rs` (unit tests). This file drives the
//! actual `axum::Router` returned by [`drevo::api::build_router`]
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
use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let db = Arc::new(Drevo::open_in_memory().expect("open in-memory db"));
    let state = ApiState::new(db);
    build_router(state)
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
