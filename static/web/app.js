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

  // ── DOM refs ────────────────────────────────────────────────────────
  const $form = document.getElementById("search-form");
  const $input = document.getElementById("search-input");
  const $results = document.getElementById("results-list");
  const $serverInfo = document.getElementById("server-info");
  const $inspectorBody = document.getElementById("inspector-body");
  const $statusText = document.getElementById("status-text");

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

  // ── Cytoscape init ─────────────────────────────────────────────────
  function initCytoscape() {
    cy = cytoscape({
      container: document.getElementById("cy"),
      // Conservative defaults — Phase 15 task 00093 will replace this
      // with fcose physics + dynamic colours + tooltips.
      layout: { name: "concentric", animate: false, padding: 24 },
      style: [
        {
          selector: "node",
          style: {
            "background-color": "#5b8df9",
            label: "data(title)",
            color: "#d9dde7",
            "text-valign": "bottom",
            "text-margin-y": 6,
            "font-size": 11,
            width: 28,
            height: 28,
            "border-width": 1,
            "border-color": "#0f1115",
          },
        },
        {
          selector: 'node[kind="person"]',
          style: { "background-color": "#76e3a4" },
        },
        {
          selector: 'node[kind="project"]',
          style: { "background-color": "#f5a623" },
        },
        {
          selector: 'node[kind="task"]',
          style: { "background-color": "#bf6cf2" },
        },
        {
          selector: "node.root",
          style: { "border-width": 3, "border-color": "#76e3a4" },
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

    cy.on("tap", "node", (evt) => {
      const node = evt.target;
      renderInspector(node.data("raw"));
    });
    cy.on("tap", (evt) => {
      if (evt.target === cy) {
        clearInspector();
      }
    });
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
  function renderSubgraph(subgraph, rootId) {
    if (!cy) return;
    cy.elements().remove();
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
    cy.add(nodes);
    cy.add(edges);
    cy.layout({ name: "concentric", animate: false, padding: 24 }).run();
    cy.fit(undefined, 24);
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

  // ── Bootstrap ──────────────────────────────────────────────────────
  $form.addEventListener("submit", (e) => {
    e.preventDefault();
    const q = $input.value.trim();
    if (q) runSearch(q);
  });

  document.addEventListener("DOMContentLoaded", () => {
    initCytoscape();
    loadServerInfo();
  });
  // Some bundlers / browsers race: if DOMContentLoaded already fired,
  // initialise immediately.
  if (document.readyState !== "loading") {
    initCytoscape();
    loadServerInfo();
  }
})();
