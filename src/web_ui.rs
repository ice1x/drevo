//! Phase 15 task `00092` — Web UI handlers.
//!
//! Embedded HTML / JS / CSS for the `drevo browser` — a minimal
//! Cytoscape.js graph explorer served by the same `axum` router
//! that backs the HTTP API. Assets live at `static/web/` and are
//! baked into the binary via [`include_str!`] so the deployed
//! `drevo-server` is self-contained — no separate `static/`
//! directory to ship alongside.
//!
//! ## Routes wired by [`api::build_router`]
//!
//! - `GET /ui` → `index.html` (`text/html; charset=utf-8`)
//! - `GET /ui/app.js` → `app.js` (`text/javascript; charset=utf-8`)
//! - `GET /ui/styles.css` → `styles.css` (`text/css; charset=utf-8`)
//!
//! Cytoscape.js itself is loaded from a CDN inside `index.html`
//! (the same pinned-CDN trust model the Neo4j Browser uses). If
//! offline use becomes a requirement, drop a `cytoscape.min.js`
//! into `static/web/` and add a fourth route here.
//!
//! ## Why embed via `include_str!` instead of `ServeDir`?
//!
//! `tower-http::ServeDir` requires the binary to know where its
//! `static/` directory lives at runtime — fine in a container, awkward
//! in the `drevo-server` distribution model that already ships as a
//! single statically-linked binary. Embedding keeps that model intact.
//! The assets are ~10 KB total — negligible binary-size cost.
//!
//! ## Note on port 7474
//!
//! README task `00092` mentions "port 7474" — that's the canonical
//! Neo4j Browser port and the documented suggestion for a stand-alone
//! UI listener. This implementation deliberately serves `/ui` on the
//! **same** `axum` listener as the HTTP API (default `8080` via
//! `DREVO_PORT`); a separate-listener variant can land later if a
//! user actually has both endpoints in a real Neo4j-shaped
//! deployment. Same-origin keeps `fetch('/search/fts', …)` from
//! tripping CORS, which is the bigger ergonomic win.

use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};

/// `index.html` body — embedded at compile time.
const INDEX_HTML: &str = include_str!("../static/web/index.html");

/// `app.js` body — embedded at compile time.
const APP_JS: &str = include_str!("../static/web/app.js");

/// `styles.css` body — embedded at compile time.
const STYLES_CSS: &str = include_str!("../static/web/styles.css");

/// `GET /ui` → serve the HTML shell.
pub async fn serve_index() -> Response {
    asset_response(INDEX_HTML, "text/html; charset=utf-8")
}

/// `GET /ui/app.js` → serve the client JS.
pub async fn serve_app_js() -> Response {
    asset_response(APP_JS, "text/javascript; charset=utf-8")
}

/// `GET /ui/styles.css` → serve the stylesheet.
pub async fn serve_styles_css() -> Response {
    asset_response(STYLES_CSS, "text/css; charset=utf-8")
}

/// Redirect `GET /ui/` (trailing slash) to `/ui` so links inside
/// `index.html` resolve against a stable prefix. Without this,
/// `<script src="/ui/app.js">` is correct from `/ui` but loads
/// `/ui/ui/app.js` if the browser arrived at `/ui/`.
pub async fn redirect_ui_slash() -> Redirect {
    Redirect::permanent("/ui")
}

fn asset_response(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // Short cache: the assets are bundled into the binary,
            // but a `cargo build` rotates them — telling the browser
            // to revalidate on every navigation avoids the "I
            // rebuilt the server and the UI didn't change" surprise
            // during local development. Production deployments
            // can layer a CDN with a longer TTL in front.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_index_references_required_dom_elements() {
        // Every id the front-end script reaches for via
        // `getElementById` must be present in the HTML — otherwise
        // the UI silently breaks at runtime.
        for needle in [
            "id=\"search-form\"",
            "id=\"search-input\"",
            "id=\"results-list\"",
            "id=\"server-info\"",
            "id=\"cy\"",
            "id=\"inspector-body\"",
            "id=\"status-text\"",
        ] {
            assert!(
                INDEX_HTML.contains(needle),
                "index.html must contain `{needle}` — app.js depends on it"
            );
        }
    }

    #[test]
    fn embedded_index_references_local_app_js_and_styles_css() {
        assert!(INDEX_HTML.contains("/ui/app.js"));
        assert!(INDEX_HTML.contains("/ui/styles.css"));
    }

    #[test]
    fn embedded_index_loads_cytoscape_from_cdn() {
        // The Cytoscape.js library is loaded from a CDN. Keep the
        // version-pinned reference under test so a future copy-paste
        // doesn't unpin it.
        assert!(
            INDEX_HTML.contains("cytoscape.min.js"),
            "index.html must reference cytoscape.min.js"
        );
        assert!(
            INDEX_HTML.contains("cytoscape@3."),
            "Cytoscape.js must be version-pinned (e.g. cytoscape@3.X.Y) in the CDN URL"
        );
    }

    #[test]
    fn embedded_app_js_calls_existing_http_endpoints() {
        // Lock the contract between the UI and the HTTP API: the JS
        // must speak the same paths as `src/api.rs`.
        assert!(APP_JS.contains("/search/fts"));
        assert!(APP_JS.contains("/nodes/"));
        assert!(APP_JS.contains("/subgraph"));
    }

    #[test]
    fn embedded_app_js_initialises_cytoscape() {
        assert!(APP_JS.contains("cytoscape("));
        assert!(APP_JS.contains("container: document.getElementById(\"cy\")"));
    }

    #[test]
    fn embedded_styles_css_defines_three_column_layout() {
        // The layout class is what makes the results-pane / canvas /
        // inspector triptych work. Lock it from drifting.
        assert!(STYLES_CSS.contains(".layout"));
        assert!(STYLES_CSS.contains("grid-template-columns"));
    }

    // ── Phase 15 task 00093 — Web UI kinetics ────────────────────────────

    #[test]
    fn embedded_index_loads_fcose_layout_extension() {
        // fcose physics layout is a Cytoscape extension; it (and its
        // `cose-base` / `layout-base` deps) must be loaded from a pinned
        // CDN after the core library, otherwise `layout: { name: "fcose" }`
        // throws "No such layout `fcose` found".
        assert!(
            INDEX_HTML.contains("cytoscape-fcose"),
            "index.html must load the cytoscape-fcose extension"
        );
        assert!(
            INDEX_HTML.contains("fcose@2."),
            "cytoscape-fcose must be version-pinned (e.g. fcose@2.X.Y) in the CDN URL"
        );
        // fcose depends on cose-base which depends on layout-base — both
        // must be present and ordered before the extension.
        assert!(
            INDEX_HTML.contains("cose-base") && INDEX_HTML.contains("layout-base"),
            "cytoscape-fcose needs cose-base + layout-base loaded first"
        );
    }

    #[test]
    fn embedded_index_has_tooltip_container() {
        // The hover tooltip is a single absolutely-positioned element
        // that app.js moves + fills on `mouseover`.
        assert!(
            INDEX_HTML.contains("id=\"cy-tooltip\""),
            "index.html must contain the #cy-tooltip element app.js drives"
        );
    }

    #[test]
    fn embedded_app_js_uses_fcose_layout() {
        // The graph must lay out with fcose physics, not the old
        // static `concentric` arrangement.
        assert!(
            APP_JS.contains("name: \"fcose\""),
            "app.js must run the fcose layout"
        );
        // The legacy concentric layout must be gone so we don't ship two
        // competing layout calls.
        assert!(
            !APP_JS.contains("concentric"),
            "the placeholder concentric layout must be replaced by fcose"
        );
    }

    #[test]
    fn embedded_app_js_double_click_expands_node() {
        // Double-clicking a node fetches its 1-hop neighbourhood and
        // merges it into the existing graph (incremental expansion),
        // rather than replacing the whole canvas like a result click.
        assert!(
            APP_JS.contains("expandNode"),
            "app.js must define an expandNode handler"
        );
        assert!(
            APP_JS.contains("subgraph?depth=1"),
            "expandNode must request the 1-hop subgraph for incremental growth"
        );
        // Cytoscape core has no native double-tap; a manual detector
        // keyed off a time threshold is the documented pattern.
        assert!(
            APP_JS.contains("DOUBLE_TAP_MS"),
            "app.js must implement double-tap detection via a time threshold"
        );
    }

    #[test]
    fn embedded_app_js_colors_nodes_dynamically() {
        // Node colour is derived from `kind` for *any* kind, not just
        // the three hard-coded selectors — a hash-to-hue function gives
        // every distinct kind a stable, distinguishable colour.
        assert!(
            APP_JS.contains("colorForKind"),
            "app.js must derive node colour dynamically from kind"
        );
    }

    #[test]
    fn embedded_app_js_shows_tooltips() {
        // Hovering a node reveals a tooltip; leaving hides it.
        assert!(
            APP_JS.contains("mouseover") && APP_JS.contains("mouseout"),
            "app.js must wire mouseover/mouseout tooltip handlers"
        );
        assert!(
            APP_JS.contains("cy-tooltip"),
            "app.js must drive the #cy-tooltip element"
        );
    }

    #[test]
    fn embedded_styles_css_styles_tooltip() {
        assert!(
            STYLES_CSS.contains("#cy-tooltip"),
            "styles.css must style the #cy-tooltip element"
        );
    }
}
