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
//! - `GET /ui/vendor/cytoscape.min.js` → Cytoscape.js core
//! - `GET /ui/vendor/layout-base.js` → fcose dep
//! - `GET /ui/vendor/cose-base.js` → fcose dep
//! - `GET /ui/vendor/cytoscape-fcose.js` → fcose physics layout extension
//!
//! Cytoscape.js and its `fcose` layout extension are **vendored** under
//! `static/web/vendor/` and baked into the binary alongside the rest of
//! the UI — they are NOT loaded from a public CDN. Earlier revisions
//! pulled them from `unpkg.com`, but that breaks the WebUI in any
//! environment where the CDN is unreachable: offline boxes, locked-down
//! networks, and privacy browsers (Brave Shields blocks `unpkg.com` by
//! default), leaving the graph canvas stuck on "connecting…". Serving the
//! libraries same-origin makes the container fully self-contained — the
//! WebUI works the instant `/ui` loads, no third-party request required.
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

/// Cytoscape.js core (`cytoscape@3.30.2`) — vendored, embedded at compile time.
const VENDOR_CYTOSCAPE: &str = include_str!("../static/web/vendor/cytoscape.min.js");

/// `layout-base@2.0.1` — fcose transitive dep, embedded at compile time.
const VENDOR_LAYOUT_BASE: &str = include_str!("../static/web/vendor/layout-base.js");

/// `cose-base@2.2.0` — fcose dep (needs `layout-base`), embedded at compile time.
const VENDOR_COSE_BASE: &str = include_str!("../static/web/vendor/cose-base.js");

/// `cytoscape-fcose@2.2.0` — fcose physics layout, embedded at compile time.
const VENDOR_FCOSE: &str = include_str!("../static/web/vendor/cytoscape-fcose.js");

/// `webcola@3.4.0` — the WebCola constraint-solver, embedded at compile time.
/// Backs the live force layout (`cytoscape-cola`) used for Neo4j-Browser-style
/// drag interaction: dragging a node tugs its neighbours via the running sim.
const VENDOR_COLA: &str = include_str!("../static/web/vendor/cola.min.js");

/// `cytoscape-cola@2.5.1` — the Cytoscape adapter for WebCola, embedded at
/// compile time. Registers the `cola` layout; needs the `cola` global first.
const VENDOR_CYTOSCAPE_COLA: &str = include_str!("../static/web/vendor/cytoscape-cola.js");

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

/// `GET /ui/vendor/cytoscape.min.js` → serve the vendored Cytoscape core.
pub async fn serve_vendor_cytoscape() -> Response {
    vendor_response(VENDOR_CYTOSCAPE)
}

/// `GET /ui/vendor/layout-base.js` → serve the vendored fcose dep.
pub async fn serve_vendor_layout_base() -> Response {
    vendor_response(VENDOR_LAYOUT_BASE)
}

/// `GET /ui/vendor/cose-base.js` → serve the vendored fcose dep.
pub async fn serve_vendor_cose_base() -> Response {
    vendor_response(VENDOR_COSE_BASE)
}

/// `GET /ui/vendor/cytoscape-fcose.js` → serve the vendored fcose layout.
pub async fn serve_vendor_fcose() -> Response {
    vendor_response(VENDOR_FCOSE)
}

/// `GET /ui/vendor/cola.min.js` → serve the vendored WebCola solver.
pub async fn serve_vendor_cola() -> Response {
    vendor_response(VENDOR_COLA)
}

/// `GET /ui/vendor/cytoscape-cola.js` → serve the vendored cola adapter.
pub async fn serve_vendor_cytoscape_cola() -> Response {
    vendor_response(VENDOR_CYTOSCAPE_COLA)
}

/// Redirect `GET /ui/` (trailing slash) to `/ui` so links inside
/// `index.html` resolve against a stable prefix. Without this,
/// `<script src="/ui/app.js">` is correct from `/ui` but loads
/// `/ui/ui/app.js` if the browser arrived at `/ui/`.
pub async fn redirect_ui_slash() -> Redirect {
    Redirect::permanent("/ui")
}

/// Serve a vendored JavaScript library. Unlike [`asset_response`]'s
/// `no-cache`, the third-party bundles are large (~700 KB combined) and
/// version-pinned, so a short positive TTL avoids re-downloading them on
/// every navigation while still letting a version bump propagate within
/// the hour after a rebuild.
fn vendor_response(body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
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
    fn embedded_index_loads_cytoscape_from_local_vendor() {
        // Cytoscape.js is vendored same-origin under /ui/vendor/, NOT
        // pulled from a public CDN — see the module docs for why (Brave
        // Shields / offline / locked-down networks broke the CDN load).
        assert!(
            INDEX_HTML.contains("/ui/vendor/cytoscape.min.js"),
            "index.html must load the vendored Cytoscape core from /ui/vendor/"
        );
    }

    #[test]
    fn embedded_index_has_no_external_asset_sources() {
        // Guard against a future copy-paste re-introducing a CDN-loaded
        // <script>/<link>. Any third-party (or protocol-relative) origin
        // re-creates the "connecting…" failure on networks that block it.
        // We check actual `src=`/`href=` values, not prose comments (which
        // legitimately mention unpkg.com to explain *why* it was dropped).
        for ext in [
            "src=\"http",
            "src='http",
            "src=\"//",
            "href=\"http",
            "href='http",
            "href=\"//",
        ] {
            assert!(
                !INDEX_HTML.contains(ext),
                "index.html must not load assets from an external origin (`{ext}`) — vendor them locally"
            );
        }
    }

    #[test]
    fn vendored_libraries_are_embedded_and_nonempty() {
        // The four bundles must actually be baked into the binary, in the
        // dependency order app.js relies on (core, then layout-base →
        // cose-base → fcose).
        assert!(
            VENDOR_CYTOSCAPE.len() > 100_000 && VENDOR_CYTOSCAPE.contains("cytoscape"),
            "Cytoscape core bundle missing or truncated"
        );
        assert!(
            VENDOR_LAYOUT_BASE.len() > 10_000,
            "layout-base bundle missing"
        );
        assert!(VENDOR_COSE_BASE.len() > 10_000, "cose-base bundle missing");
        assert!(
            VENDOR_FCOSE.len() > 10_000 && VENDOR_FCOSE.contains("fcose"),
            "cytoscape-fcose bundle missing or truncated"
        );
        // cola (WebCola + adapter) backs the live drag simulation.
        assert!(VENDOR_COLA.len() > 10_000, "cola (webcola) bundle missing");
        assert!(
            VENDOR_CYTOSCAPE_COLA.len() > 5_000 && VENDOR_CYTOSCAPE_COLA.contains("cola"),
            "cytoscape-cola bundle missing or truncated"
        );
    }

    #[test]
    fn embedded_index_loads_cola_for_live_drag() {
        // The live-drag force layout needs WebCola + the cytoscape-cola
        // adapter, vendored same-origin and loaded after the Cytoscape core.
        assert!(
            INDEX_HTML.contains("/ui/vendor/cola.min.js")
                && INDEX_HTML.contains("/ui/vendor/cytoscape-cola.js"),
            "index.html must load the vendored cola libraries for live drag"
        );
        let core = INDEX_HTML.find("/ui/vendor/cytoscape.min.js").unwrap();
        let cola = INDEX_HTML.find("/ui/vendor/cola.min.js").unwrap();
        let adapter = INDEX_HTML.find("/ui/vendor/cytoscape-cola.js").unwrap();
        assert!(
            core < cola && cola < adapter,
            "cola must load after the core, and the adapter after cola"
        );
    }

    #[test]
    fn embedded_app_js_live_drag_runs_cola_simulation() {
        // Dragging a node must run a live cola force simulation so connected
        // neighbours tug along (Neo4j-Browser parity), keyed off `drag`/`free`.
        assert!(
            APP_JS.contains("name: \"cola\""),
            "app.js must run the cola layout for live drag"
        );
        assert!(
            APP_JS.contains("startLiveLayout") && APP_JS.contains("stopLiveLayout"),
            "app.js must start/stop the live simulation"
        );
        assert!(
            APP_JS.contains("\"drag\"") && APP_JS.contains("\"free\""),
            "app.js must drive the live simulation off node drag/free events"
        );
        assert!(
            APP_JS.contains("infinite: true"),
            "the live cola layout must run continuously (infinite) during drag"
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
        // `cose-base` / `layout-base` deps) must be loaded same-origin
        // after the core library, otherwise `layout: { name: "fcose" }`
        // throws "No such layout `fcose` found".
        assert!(
            INDEX_HTML.contains("/ui/vendor/cytoscape-fcose.js"),
            "index.html must load the vendored cytoscape-fcose extension"
        );
        // fcose depends on cose-base which depends on layout-base — both
        // must be present and ordered before the extension.
        assert!(
            INDEX_HTML.contains("/ui/vendor/cose-base.js")
                && INDEX_HTML.contains("/ui/vendor/layout-base.js"),
            "cytoscape-fcose needs cose-base + layout-base loaded first"
        );
        // Load order matters: core, then layout-base, cose-base, fcose.
        let core = INDEX_HTML.find("/ui/vendor/cytoscape.min.js").unwrap();
        let lb = INDEX_HTML.find("/ui/vendor/layout-base.js").unwrap();
        let cb = INDEX_HTML.find("/ui/vendor/cose-base.js").unwrap();
        let fc = INDEX_HTML.find("/ui/vendor/cytoscape-fcose.js").unwrap();
        assert!(
            core < lb && lb < cb && cb < fc,
            "vendored scripts must load in order: core, layout-base, cose-base, fcose"
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

    // ── Graph overview on load (Neo4j-Browser-style initial view) ────────

    #[test]
    fn embedded_index_has_overview_controls() {
        // A configurable node-count input (Neo4j's "Initial Node Display
        // Limit") and a container for the per-kind chips must exist in the
        // HTML for app.js to drive.
        assert!(
            INDEX_HTML.contains("id=\"node-limit\""),
            "index.html must contain the #node-limit control (configurable sample size)"
        );
        assert!(
            INDEX_HTML.contains("id=\"kind-chips\""),
            "index.html must contain the #kind-chips container for label chips"
        );
    }

    #[test]
    fn embedded_app_js_renders_overview_on_load() {
        // On load the canvas must auto-render a bounded sample of the graph
        // (not sit empty), fetched once from /export/json and cached.
        assert!(
            APP_JS.contains("/export/json"),
            "app.js must fetch the graph dump from /export/json for the overview"
        );
        assert!(
            APP_JS.contains("loadOverview"),
            "app.js must define a loadOverview() entry point"
        );
        // The overview must actually run at bootstrap, alongside the
        // existing cytoscape init / server-info calls.
        assert!(
            APP_JS.matches("loadOverview(").count() >= 2,
            "app.js must call loadOverview() at startup (definition + invocation)"
        );
    }

    #[test]
    fn embedded_app_js_sample_is_configurable_and_kind_filtered() {
        // The sample size is read from the #node-limit control (configurable
        // like Neo4j), and clicking a kind chip re-renders that kind.
        assert!(
            APP_JS.contains("node-limit"),
            "app.js must read the configurable sample size from #node-limit"
        );
        assert!(
            APP_JS.contains("renderSample"),
            "app.js must define renderSample() to draw a bounded node set"
        );
        assert!(
            APP_JS.contains("kind-chips") || APP_JS.contains("renderKindChips"),
            "app.js must build the per-kind chips"
        );
    }

    #[test]
    fn embedded_app_js_induced_edges_only() {
        // A sample of N nodes must draw only edges whose BOTH endpoints are
        // in the sampled set — otherwise cytoscape throws on a dangling
        // edge endpoint. Lock the membership-filter helper.
        assert!(
            APP_JS.contains("inducedEdges") || APP_JS.contains("has(e.from_id)"),
            "app.js must keep only induced edges (both endpoints in the sample)"
        );
    }

    #[test]
    fn embedded_styles_css_styles_kind_chips() {
        assert!(
            STYLES_CSS.contains(".kind-chip"),
            "styles.css must style the .kind-chip elements"
        );
    }

    // ── Light / dark theme ───────────────────────────────────────────────

    #[test]
    fn embedded_index_defaults_to_dark_theme_with_toggle() {
        assert!(
            INDEX_HTML.contains("data-theme=\"dark\""),
            "default theme must be dark"
        );
        assert!(
            INDEX_HTML.contains("id=\"theme-toggle\""),
            "the topbar must carry a #theme-toggle control"
        );
    }

    #[test]
    fn embedded_styles_css_defines_both_themes() {
        // Dark is the default `:root` palette; light overrides via the
        // `[data-theme="light"]` selector. Both must be present.
        assert!(
            STYLES_CSS.contains("html[data-theme=\"light\"]")
                || STYLES_CSS.contains("[data-theme=\"light\"]"),
            "styles.css must define a light-theme override block"
        );
        assert!(
            STYLES_CSS.contains("--radius"),
            "styles.css must use the modern radius variable (rounded UI)"
        );
    }

    #[test]
    fn embedded_app_js_toggles_theme_and_reskins_canvas() {
        assert!(
            APP_JS.contains("applyTheme") && APP_JS.contains("data-theme"),
            "app.js must toggle the document data-theme"
        );
        assert!(
            APP_JS.contains("cyStyle"),
            "app.js must re-skin the Cytoscape canvas per theme"
        );
        assert!(
            APP_JS.contains("drevo-theme"),
            "app.js must persist the chosen theme"
        );
    }

    #[test]
    fn embedded_app_js_declutters_the_graph() {
        // The cramped look came from relationship captions everywhere + node
        // labels piling over edges. Captions are hidden by default and shown
        // on hover; node labels carry a halo so they stay legible.
        assert!(
            APP_JS.contains("\"text-opacity\": 0"),
            "edge captions must be hidden by default"
        );
        assert!(
            APP_JS.contains("edge.hl") || APP_JS.contains("addClass(\"hl\")"),
            "edge captions must be revealed on hover/selection"
        );
        assert!(
            APP_JS.contains("text-outline-width"),
            "node labels must have a halo so they read over edges/other nodes"
        );
    }

    // ── Cypher query bar (Neo4j-Browser-style) ───────────────────────────

    #[test]
    fn embedded_app_js_runs_cypher_queries() {
        // The top bar must accept Cypher (not only FTS) and POST it to the
        // /cypher endpoint, auto-detecting which mode the input is.
        assert!(
            APP_JS.contains("/cypher"),
            "app.js must POST to the /cypher endpoint"
        );
        assert!(
            APP_JS.contains("runCypher"),
            "app.js must define runCypher()"
        );
        assert!(
            APP_JS.contains("looksLikeCypher"),
            "app.js must auto-detect Cypher vs FTS input"
        );
        // The graph projection returned by /cypher must reach the canvas.
        assert!(
            APP_JS.contains(".graph"),
            "app.js must render the /cypher graph projection"
        );
        // "Connect result nodes" (Neo4j Browser parity): a node-only result
        // must still be wired with the edges that exist between the returned
        // nodes, so `MATCH (n) RETURN n` is not a disconnected grid.
        assert!(
            APP_JS.contains("connectResultNodes"),
            "app.js must connect result nodes with their inter-node edges"
        );
    }
}
