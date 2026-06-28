// drevo browser — minimal Cytoscape.js front-end driving the existing
// Phase 8 HTTP API. Phase 15 task `00092`.
//
// Wire shape (defined in src/api.rs):
//   GET  /                       → ServerInfo { name, version, ... }
//   POST /search/fts             → { query, limit } → [{ node, score }]
//   GET  /nodes/{id}             → Node
//   GET  /nodes/{id}/subgraph    → { nodes: [Node], edges: [Edge] }
//
// Everything is plain ES2017 — no module bundler, no transpile step.

(function () {
  "use strict";

  // ── State ───────────────────────────────────────────────────────────
  /** @type {cytoscape.Core | null} */
  let cy = null;
  let currentRootId = null;
  let lastResults = []; // Array of { node, score }

  // Double-tap detection (task 00093): Cytoscape core emits no native
  // double-tap, so we fold two taps on the same node within this many
  // milliseconds into an "expand" gesture.
  const DOUBLE_TAP_MS = 300;
  let lastTapId = null;
  let lastTapAt = 0;

  // ── DOM refs ────────────────────────────────────────────────────────
  const $form = document.getElementById("search-form");
  const $input = document.getElementById("search-input");
  const $results = document.getElementById("results-list");
  const $serverInfo = document.getElementById("server-info");
  const $inspectorBody = document.getElementById("inspector-body");
  const $statusText = document.getElementById("status-text");
  const $tooltip = document.getElementById("cy-tooltip");
  const $nodeLimit = document.getElementById("node-limit");
  const $kindChips = document.getElementById("kind-chips");

  // ── Graph overview state ─────────────────────────────────────────────
  // The whole graph dump is fetched once from /export/json and cached;
  // the on-load overview and the kind-chip filters all render bounded
  // samples from this cache, so changing the node limit or switching
  // kinds never re-hits the network.
  /** @type {{nodes: any[], edges: any[]} | null} */
  let graphCache = null;
  const degreeById = new Map(); // node id → degree (for top-N sampling)
  let activeKind = null; // null = all kinds

  // ── Dynamic node colour (task 00093) ────────────────────────────────
  // A handful of common kinds get curated, semantically-suggestive
  // colours; every other kind is hashed to a stable hue so any schema
  // — CBT journal, story, tasks, ERP, bug tracker — gets distinct,
  // repeatable node colours without a hard-coded palette per project.
  const CURATED_KIND_COLORS = {
    person: "#76e3a4",
    project: "#f5a623",
    task: "#bf6cf2",
    bug: "#f56565",
    entry: "#5bd1f9",
    character: "#f9c95b",
  };
  /** Map an arbitrary `kind` string to a stable hex colour. */
  function colorForKind(kind) {
    const key = (kind || "").toLowerCase();
    if (!key) return "#5b8df9"; // node-default (no kind)
    if (CURATED_KIND_COLORS[key]) return CURATED_KIND_COLORS[key];
    // FNV-1a-ish string hash → hue on the colour wheel. Fixed
    // saturation/lightness keeps every colour legible on the dark canvas.
    let h = 2166136261;
    for (let i = 0; i < key.length; i++) {
      h ^= key.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    const hue = Math.abs(h) % 360;
    return `hsl(${hue}, 62%, 62%)`;
  }

  // ── HTTP helpers ────────────────────────────────────────────────────
  async function apiGet(path) {
    const r = await fetch(path, { headers: { Accept: "application/json" } });
    if (!r.ok) {
      throw new Error(`GET ${path} → ${r.status} ${r.statusText}`);
    }
    return r.json();
  }
  async function apiPost(path, body) {
    const r = await fetch(path, {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
    if (!r.ok) {
      throw new Error(`POST ${path} → ${r.status} ${r.statusText}`);
    }
    return r.json();
  }

  // ── Status line ─────────────────────────────────────────────────────
  function status(text, kind) {
    $statusText.textContent = text;
    $statusText.className = "";
    if (kind === "error") $statusText.classList.add("status-error");
    if (kind === "ok") $statusText.classList.add("status-ok");
  }

  // ── Server info — top-right ────────────────────────────────────────
  async function loadServerInfo() {
    try {
      const info = await apiGet("/");
      const name = info.name || "drevo";
      const ver = info.version || "?";
      $serverInfo.textContent = `${name} v${ver}`;
    } catch (e) {
      $serverInfo.textContent = "(disconnected)";
      status("Cannot reach drevo HTTP API at /. Is the server running?", "error");
    }
  }

  // ── Layout (task 00093) ────────────────────────────────────────────
  // fcose = "fast Compound Spring Embedder": a force-directed physics
  // layout that spreads nodes organically and animates into place.
  // `randomize: false` reuses current positions on re-runs so an
  // incremental expand nudges the graph rather than reshuffling it.
  function fcoseOptions(animate) {
    return {
      name: "fcose",
      animate: animate !== false,
      animationDuration: 500,
      randomize: false,
      fit: true,
      padding: 28,
      nodeRepulsion: 6500,
      idealEdgeLength: 90,
      nestingFactor: 0.1,
    };
  }
  function runLayout(animate) {
    if (!cy) return;
    cy.layout(fcoseOptions(animate)).run();
  }

  // ── Cytoscape init ─────────────────────────────────────────────────
  function initCytoscape() {
    cy = cytoscape({
      container: document.getElementById("cy"),
      layout: fcoseOptions(false),
      style: [
        {
          selector: "node",
          style: {
            // Dynamic colour: every distinct `kind` gets a stable hue
            // via colorForKind, so the palette is no longer capped at a
            // few hard-coded selectors.
            "background-color": (ele) => colorForKind(ele.data("kind")),
            label: "data(title)",
            color: "#d9dde7",
            "text-valign": "bottom",
            "text-margin-y": 6,
            "font-size": 11,
            width: 28,
            height: 28,
            "border-width": 1,
            "border-color": "#0f1115",
            "transition-property": "background-color, border-color, width, height",
            "transition-duration": "160ms",
          },
        },
        {
          selector: "node.root",
          style: { "border-width": 3, "border-color": "#76e3a4" },
        },
        {
          selector: "node.expanded",
          style: { "border-color": "#5b8df9" },
        },
        {
          selector: "node:selected",
          style: { "border-width": 3, "border-color": "#f5f7ff" },
        },
        {
          selector: "edge",
          style: {
            width: 1.5,
            "line-color": "#4a4f5e",
            "target-arrow-color": "#4a4f5e",
            "target-arrow-shape": "triangle",
            "curve-style": "bezier",
            label: "data(kind)",
            "font-size": 9,
            color: "#8a91a3",
            "text-rotation": "autorotate",
          },
        },
      ],
      wheelSensitivity: 0.2,
    });

    // Single tap → inspect; double tap (same node, within the threshold)
    // → expand its 1-hop neighbourhood.
    cy.on("tap", "node", (evt) => {
      const node = evt.target;
      const id = node.id();
      const now = evt.timeStamp || Date.now();
      if (lastTapId === id && now - lastTapAt < DOUBLE_TAP_MS) {
        lastTapId = null;
        lastTapAt = 0;
        expandNode(node);
        return;
      }
      lastTapId = id;
      lastTapAt = now;
      renderInspector(node.data("raw"));
    });
    cy.on("tap", (evt) => {
      if (evt.target === cy) {
        clearInspector();
      }
    });

    // Hover tooltips (task 00093): show title + kind + id near the node.
    cy.on("mouseover", "node", (evt) => showTooltip(evt.target));
    cy.on("mousemove", "node", (evt) => positionTooltip(evt.target));
    cy.on("mouseout", "node", () => hideTooltip());
    cy.on("pan zoom drag", () => hideTooltip());
  }

  // ── Tooltips (task 00093) ──────────────────────────────────────────
  function showTooltip(node) {
    if (!$tooltip) return;
    const raw = node.data("raw") || {};
    const title = raw.title || node.data("title") || `#${node.id()}`;
    const kind = raw.kind || node.data("kind") || "(no kind)";
    $tooltip.innerHTML = "";
    const t = document.createElement("div");
    t.className = "cy-tooltip-title";
    t.textContent = title;
    const k = document.createElement("div");
    k.className = "cy-tooltip-kind";
    k.textContent = `${kind} · id ${node.id()}`;
    $tooltip.appendChild(t);
    $tooltip.appendChild(k);
    $tooltip.setAttribute("aria-hidden", "false");
    $tooltip.classList.add("visible");
    positionTooltip(node);
  }
  function positionTooltip(node) {
    if (!$tooltip || !$tooltip.classList.contains("visible")) return;
    const p = node.renderedPosition();
    // Offset above-right of the node; the canvas is the offset parent.
    $tooltip.style.left = `${Math.round(p.x + 14)}px`;
    $tooltip.style.top = `${Math.round(p.y - 10)}px`;
  }
  function hideTooltip() {
    if (!$tooltip) return;
    $tooltip.classList.remove("visible");
    $tooltip.setAttribute("aria-hidden", "true");
  }

  // ── Double-click expansion (task 00093) ────────────────────────────
  // Fetch the clicked node's 1-hop neighbourhood and merge it into the
  // current canvas, growing the graph incrementally instead of
  // replacing it the way a results-list click does.
  async function expandNode(node) {
    const nodeId = node.data("raw") ? node.data("raw").id : Number(node.id());
    hideTooltip();
    status(`Expanding neighbours of node ${nodeId}…`);
    try {
      const sub = await apiGet(`/nodes/${nodeId}/subgraph?depth=1`);
      const added = mergeSubgraph(sub);
      node.addClass("expanded");
      runLayout(true);
      cy.fit(undefined, 28);
      status(
        added === 0
          ? `Node ${nodeId} has no new neighbours.`
          : `Expanded node ${nodeId}: +${added} new element${added === 1 ? "" : "s"}.`,
        "ok"
      );
    } catch (e) {
      status(`Expand failed: ${e.message}`, "error");
    }
  }

  // ── FTS search ──────────────────────────────────────────────────────
  async function runSearch(query) {
    status(`Searching for ${JSON.stringify(query)}…`);
    try {
      const resp = await apiPost("/search/fts", { query, limit: 25 });
      // The HTTP layer wraps the result; accept either `[ScoredNode]`
      // or `{ results: [...] }` to stay robust against minor shape
      // changes.
      const results = Array.isArray(resp) ? resp : resp.results || [];
      lastResults = results;
      renderResults(results);
      status(
        results.length === 0
          ? `No results for ${JSON.stringify(query)}.`
          : `${results.length} result${results.length === 1 ? "" : "s"}.`,
        results.length === 0 ? null : "ok"
      );
    } catch (e) {
      status(`Search failed: ${e.message}`, "error");
    }
  }

  // ── Cypher query mode (Neo4j-Browser-style) ────────────────────────
  // The top bar is dual-mode: an input that begins with a Cypher clause
  // keyword runs against POST /cypher (the same executor Bolt uses);
  // anything else is a full-text search. This lets `MATCH (n) RETURN n
  // LIMIT 10` Just Work the way it does in the Neo4j Browser.
  const CYPHER_RE =
    /^\s*(MATCH|OPTIONAL\s+MATCH|CREATE|MERGE|RETURN|WITH|UNWIND|CALL|DETACH\s+DELETE|DELETE|SET|REMOVE|FOREACH)\b/i;
  function looksLikeCypher(text) {
    return CYPHER_RE.test(text);
  }

  async function runCypher(query) {
    status("Running Cypher…");
    try {
      // Own fetch (not apiPost) so we can surface the executor's error
      // text — /cypher returns the parse/exec message as the body on 400.
      const r = await fetch("/cypher", {
        method: "POST",
        headers: { Accept: "application/json", "Content-Type": "application/json" },
        body: JSON.stringify({ query }),
      });
      const text = await r.text();
      if (!r.ok) {
        status(`Cypher error: ${text}`, "error");
        return;
      }
      renderCypherResult(JSON.parse(text));
    } catch (e) {
      status(`Cypher failed: ${e.message}`, "error");
    }
  }

  function renderCypherResult(resp) {
    const graph = resp.graph || { nodes: [], edges: [] };
    // Draw whatever nodes/relationships the query touched.
    if (graph.nodes.length > 0) {
      renderSubgraph({ nodes: graph.nodes, edges: graph.edges }, null);
    } else if (cy) {
      cy.elements().remove();
    }
    renderCypherRows(resp.columns || [], resp.rows || []);
    const s = resp.stats || {};
    const writes =
      (s.nodes_created || 0) +
      (s.relationships_created || 0) +
      (s.properties_set || 0) +
      (s.nodes_deleted || 0) +
      (s.relationships_deleted || 0);
    const writeNote = writes > 0 ? ` · ${writes} write${writes === 1 ? "" : "s"}` : "";
    status(
      `${resp.rows.length} row${resp.rows.length === 1 ? "" : "s"} · graph: ${
        graph.nodes.length
      } node${graph.nodes.length === 1 ? "" : "s"}, ${graph.edges.length} edge${
        graph.edges.length === 1 ? "" : "s"
      }${writeNote}.`,
      "ok"
    );
  }

  // Render the tabular result in the left rail. Node/edge-valued cells show
  // a readable label; scalars show their JSON. A node cell stays clickable
  // to load its 2-hop neighbourhood, like a search result.
  function renderCypherRows(columns, rows) {
    $results.innerHTML = "";
    if (rows.length === 0) {
      const li = document.createElement("li");
      li.className = "results-empty";
      li.textContent = "Query returned no rows.";
      $results.appendChild(li);
      return;
    }
    const table = document.createElement("table");
    table.className = "cypher-table";
    const thead = document.createElement("thead");
    const htr = document.createElement("tr");
    for (const c of columns) {
      const th = document.createElement("th");
      th.textContent = c;
      htr.appendChild(th);
    }
    thead.appendChild(htr);
    table.appendChild(thead);
    const tbody = document.createElement("tbody");
    for (const row of rows) {
      const tr = document.createElement("tr");
      for (const cell of row) {
        const td = document.createElement("td");
        if (cell && typeof cell === "object" && typeof cell.id === "number" && "kind" in cell) {
          // A node (or edge) value — show a clickable label.
          td.className = "cell-node";
          td.textContent = cell.title || `${cell.kind || "node"} #${cell.id}`;
          if (!("from_id" in cell)) {
            td.addEventListener("click", () => selectResult(cell.id, null));
          }
        } else {
          td.textContent = JSON.stringify(cell);
        }
        tr.appendChild(td);
      }
      tbody.appendChild(tr);
    }
    table.appendChild(tbody);
    $results.appendChild(table);
  }

  function renderResults(results) {
    $results.innerHTML = "";
    if (results.length === 0) {
      const li = document.createElement("li");
      li.className = "results-empty";
      li.textContent = "No matching nodes.";
      $results.appendChild(li);
      return;
    }
    for (const r of results) {
      const node = r.node || r;
      const li = document.createElement("li");
      li.dataset.nodeId = String(node.id);
      const title = document.createElement("span");
      title.className = "result-title";
      title.textContent = node.title || `#${node.id}`;
      li.appendChild(title);
      const meta = document.createElement("span");
      meta.className = "result-meta";
      meta.textContent = `${node.kind || "(no kind)"} · id ${node.id}`;
      li.appendChild(meta);
      li.addEventListener("click", () => selectResult(node.id, li));
      $results.appendChild(li);
    }
  }

  async function selectResult(nodeId, liEl) {
    // Mark selection in the results list.
    Array.from($results.querySelectorAll("li.selected")).forEach((el) =>
      el.classList.remove("selected")
    );
    if (liEl) liEl.classList.add("selected");
    currentRootId = nodeId;
    status(`Loading 2-hop subgraph around node ${nodeId}…`);
    try {
      // /subgraph endpoint already returns { nodes, edges }.
      const sub = await apiGet(`/nodes/${nodeId}/subgraph?depth=2`);
      renderSubgraph(sub, nodeId);
      status(
        `Loaded ${sub.nodes.length} node${
          sub.nodes.length === 1 ? "" : "s"
        }, ${sub.edges.length} edge${sub.edges.length === 1 ? "" : "s"}.`,
        "ok"
      );
      // Also pre-fill the inspector with the root node.
      const root = sub.nodes.find((n) => n.id === nodeId);
      if (root) renderInspector(root);
    } catch (e) {
      status(`Subgraph load failed: ${e.message}`, "error");
    }
  }

  // ── Cytoscape rendering ────────────────────────────────────────────
  // Map a wire subgraph to Cytoscape element descriptors.
  function toElements(subgraph, rootId) {
    const nodes = (subgraph.nodes || []).map((n) => ({
      group: "nodes",
      data: {
        id: String(n.id),
        title: n.title || `#${n.id}`,
        kind: n.kind || "",
        raw: n,
      },
      classes: n.id === rootId ? "root" : "",
    }));
    const edges = (subgraph.edges || []).map((e) => ({
      group: "edges",
      data: {
        id: `e${e.id}`,
        source: String(e.from_id),
        target: String(e.to_id),
        kind: e.kind || "",
        raw: e,
      },
    }));
    return { nodes, edges };
  }

  // Replace the canvas with a fresh subgraph (results-list click).
  function renderSubgraph(subgraph, rootId) {
    if (!cy) return;
    cy.elements().remove();
    const { nodes, edges } = toElements(subgraph, rootId);
    cy.add(nodes);
    cy.add(edges);
    runLayout(true);
    cy.fit(undefined, 28);
  }

  // Merge a subgraph into the existing canvas (double-click expand),
  // skipping elements already present. Returns the count added.
  function mergeSubgraph(subgraph) {
    if (!cy) return 0;
    const { nodes, edges } = toElements(subgraph, currentRootId);
    let added = 0;
    for (const el of [...nodes, ...edges]) {
      if (cy.getElementById(el.data.id).empty()) {
        cy.add(el);
        added++;
      }
    }
    return added;
  }

  // ── Inspector ──────────────────────────────────────────────────────
  function renderInspector(node) {
    if (!node) return clearInspector();
    $inspectorBody.innerHTML = "";
    const kindBadge = document.createElement("div");
    kindBadge.className = "inspector-kind";
    kindBadge.textContent = node.kind || "(no kind)";
    $inspectorBody.appendChild(kindBadge);
    const heading = document.createElement("h3");
    heading.textContent = node.title || `#${node.id}`;
    heading.style.margin = "0.25rem 0 0.6rem";
    $inspectorBody.appendChild(heading);
    addRow("id", String(node.id));
    if (node.uuid) addRow("uuid", uuidToHyphenated(node.uuid));
    if (node.created_at !== undefined) addRow("created_at", String(node.created_at));
    if (node.updated_at !== undefined) addRow("updated_at", String(node.updated_at));
    const props = node.properties || {};
    const propKeys = Object.keys(props).sort();
    for (const k of propKeys) {
      addRow(k, JSON.stringify(props[k]));
    }
  }
  function addRow(k, v) {
    const row = document.createElement("div");
    row.className = "inspector-row";
    const kEl = document.createElement("span");
    kEl.className = "k";
    kEl.textContent = k;
    const vEl = document.createElement("span");
    vEl.className = "v";
    vEl.textContent = v;
    row.appendChild(kEl);
    row.appendChild(vEl);
    $inspectorBody.appendChild(row);
  }
  function clearInspector() {
    $inspectorBody.innerHTML =
      '<p class="inspector-empty">Click a node in the canvas to view its properties.</p>';
  }

  // ── UUID byte-array → hyphenated string ────────────────────────────
  function uuidToHyphenated(bytes) {
    if (typeof bytes === "string") return bytes;
    if (!Array.isArray(bytes) || bytes.length !== 16) return JSON.stringify(bytes);
    const hex = bytes.map((b) => b.toString(16).padStart(2, "0")).join("");
    return (
      hex.slice(0, 8) +
      "-" +
      hex.slice(8, 12) +
      "-" +
      hex.slice(12, 16) +
      "-" +
      hex.slice(16, 20) +
      "-" +
      hex.slice(20)
    );
  }

  // ── Graph overview (Neo4j-Browser-style initial view) ──────────────
  // On load drevo shows a bounded sample of the graph instead of a blank
  // canvas. The sample size is user-configurable (#node-limit, mirroring
  // Neo4j's "Initial Node Display Limit"), and per-kind chips let you
  // focus one label at a time. Dumping all ~thousands of nodes at once
  // would be an unreadable hairball, so we cap the display like Neo4j.

  /** Read the configurable sample size, clamped to a sane range. */
  function currentLimit() {
    const n = parseInt($nodeLimit && $nodeLimit.value, 10);
    if (!Number.isFinite(n) || n < 1) return 50;
    return Math.min(n, 2000);
  }

  /** Fetch the graph once, build the degree map, draw the first sample. */
  async function loadOverview() {
    status("Loading graph overview…");
    try {
      const dump = await apiGet("/export/json");
      graphCache = { nodes: dump.nodes || [], edges: dump.edges || [] };
      degreeById.clear();
      for (const e of graphCache.edges) {
        degreeById.set(e.from_id, (degreeById.get(e.from_id) || 0) + 1);
        degreeById.set(e.to_id, (degreeById.get(e.to_id) || 0) + 1);
      }
      renderKindChips();
      renderSample();
    } catch (e) {
      status(`Overview load failed: ${e.message}`, "error");
    }
  }

  /** Distinct kinds with counts, most-common first. */
  function kindCounts() {
    const counts = new Map();
    for (const n of (graphCache && graphCache.nodes) || []) {
      const k = n.kind || "(no kind)";
      counts.set(k, (counts.get(k) || 0) + 1);
    }
    return [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  }

  /** Build the clickable label-chip rail (an "All" chip + one per kind). */
  function renderKindChips() {
    if (!$kindChips) return;
    $kindChips.innerHTML = "";
    const total = (graphCache && graphCache.nodes.length) || 0;
    const makeChip = (label, count, kindOrNull) => {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = "kind-chip";
      if (activeKind === kindOrNull) chip.classList.add("active");
      if (kindOrNull) chip.style.setProperty("--chip-color", colorForKind(kindOrNull));
      const name = document.createElement("span");
      name.className = "kind-chip-name";
      name.textContent = label;
      const c = document.createElement("span");
      c.className = "kind-chip-count";
      c.textContent = String(count);
      chip.appendChild(name);
      chip.appendChild(c);
      chip.addEventListener("click", () => {
        activeKind = kindOrNull;
        renderKindChips();
        renderSample();
      });
      return chip;
    };
    $kindChips.appendChild(makeChip("All", total, null));
    for (const [kind, count] of kindCounts()) {
      $kindChips.appendChild(makeChip(kind, count, kind));
    }
  }

  /** Pick the top-`limit` highest-degree nodes from a pool (id asc ties). */
  function pickSampleNodes(pool, limit) {
    return [...pool]
      .sort(
        (a, b) =>
          (degreeById.get(b.id) || 0) - (degreeById.get(a.id) || 0) || a.id - b.id
      )
      .slice(0, limit);
  }

  /** Edges whose BOTH endpoints are in the sampled set (no dangling ends). */
  function inducedEdges(idSet) {
    return ((graphCache && graphCache.edges) || []).filter(
      (e) => idSet.has(e.from_id) && idSet.has(e.to_id)
    );
  }

  /** Render the current view: a bounded, induced sample of the cache. */
  function renderSample() {
    if (!graphCache) return;
    const limit = currentLimit();
    const pool = activeKind
      ? graphCache.nodes.filter((n) => (n.kind || "(no kind)") === activeKind)
      : graphCache.nodes;
    const sample = pickSampleNodes(pool, limit);
    const idSet = new Set(sample.map((n) => n.id));
    const edges = inducedEdges(idSet);
    renderSubgraph({ nodes: sample, edges }, null);
    const scope = activeKind ? `kind “${activeKind}”` : "all kinds";
    status(
      `Overview: ${sample.length} of ${pool.length} node${
        pool.length === 1 ? "" : "s"
      } (${scope}), ${edges.length} edge${edges.length === 1 ? "" : "s"}.`,
      "ok"
    );
  }

  // ── Bootstrap ──────────────────────────────────────────────────────
  $form.addEventListener("submit", (e) => {
    e.preventDefault();
    const q = $input.value.trim();
    if (!q) return;
    // Dual-mode: a Cypher clause keyword routes to the executor; plain
    // text is a full-text search.
    if (looksLikeCypher(q)) runCypher(q);
    else runSearch(q);
  });

  // Re-render (from cache, no re-fetch) when the node limit changes.
  if ($nodeLimit) {
    $nodeLimit.addEventListener("change", () => renderSample());
  }

  document.addEventListener("DOMContentLoaded", () => {
    initCytoscape();
    loadServerInfo();
    loadOverview();
  });
  // Some bundlers / browsers race: if DOMContentLoaded already fired,
  // initialise immediately.
  if (document.readyState !== "loading") {
    initCytoscape();
    loadServerInfo();
    loadOverview();
  }
})();
