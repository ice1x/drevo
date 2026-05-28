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
}
