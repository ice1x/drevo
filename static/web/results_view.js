// drevo browser — pure helpers for showing values: Cypher result cells in the
// Table and JSON views, the view a result opens in, and property values in
// the inspector. DOM-free so they can be unit-tested under `node --test` (see
// results_view.test.js).
//
// UMD: attaches to the global `DrevoResultsView` in the browser (loaded after
// graph_math.js and before app.js) and to `module.exports` under node's test
// runner. No DOM references.
(function (root, factory) {
  "use strict";
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory(require("./graph_math.js"));
  } else {
    root.DrevoResultsView = factory(root.DrevoGraphMath);
  }
})(typeof self !== "undefined" ? self : this, function (GraphMath) {
  "use strict";

  // A graph value on the wire: `{id, kind, ...}`; a relationship also has
  // `from_id` / `to_id`. A plain map with an `id` key has no `kind`.
  function isGraphValue(v) {
    return v !== null && typeof v === "object" && typeof v.id === "number" && "kind" in v;
  }

  // How one result cell reads in the table: `{kind, text}`, plus `id` for a
  // node or relationship so the table can make it clickable.
  //
  //   cellValue("open")      → {kind: "string", text: "open"}
  //   cellValue(null)        → {kind: "null", text: "null"}
  //   cellValue(task)        → {kind: "node", text: "Ship it", id: 7}
  //   cellValue(dependsOn)   → {kind: "relationship", text: "(7)-[:DEPENDS_ON]->(8)", id: 3}
  //   cellValue([1, "a"])    → {kind: "list", text: '[1,"a"]'}
  function cellValue(v) {
    if (v === null || v === undefined) return { kind: "null", text: "null" };
    if (typeof v === "string") return { kind: "string", text: v };
    if (typeof v === "number") return { kind: "number", text: String(v) };
    if (typeof v === "boolean") return { kind: "boolean", text: String(v) };
    if (isGraphValue(v)) {
      if ("from_id" in v) {
        return {
          kind: "relationship",
          text: `(${v.from_id})-[:${v.kind || "RELATED"}]->(${v.to_id})`,
          id: v.id,
        };
      }
      return { kind: "node", text: GraphMath.nodeLabel(v), id: v.id };
    }
    return { kind: Array.isArray(v) ? "list" : "map", text: JSON.stringify(v) };
  }

  // The view a Cypher response opens in: the graph when it holds nodes,
  // otherwise the table.
  function defaultView(resp) {
    const nodes = resp && resp.graph && resp.graph.nodes;
    return Array.isArray(nodes) && nodes.length > 0 ? "graph" : "table";
  }

  // One CSV field (RFC 4180): quoted when it holds a comma, a quote or a line
  // break, with inner quotes doubled.
  function csvField(text) {
    return /[",\r\n]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
  }

  // The result as CSV: a header row, then one line per row, CRLF-separated.
  // Null cells are empty; nodes and relationships use their table label.
  function toCsv(columns, rows) {
    const line = (cells) => cells.map(csvField).join(",");
    const out = [line(columns.map(String))];
    for (const row of rows) {
      out.push(line(row.map((v) => (v === null || v === undefined ? "" : cellValue(v).text))));
    }
    return out.join("\r\n");
  }

  // Dates in the inspector read one way: "YYYY-MM-DD HH:MM:SS UTC".
  function utcText(ms) {
    const d = new Date(ms);
    if (Number.isNaN(d.getTime())) return null;
    return d.toISOString().slice(0, 19).replace("T", " ") + " UTC";
  }

  // Keys whose numbers are epoch timestamps: `created_at`, `seen_ts`, `time`…
  const TIME_KEY_RE = /(^|_)(at|ts|time|timestamp)$/i;
  // An ISO 8601 date-time with an explicit offset; the fraction may run past
  // milliseconds (`.642000000`).
  const ISO_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})$/;

  // How one property reads in the inspector: like a table cell, except that
  // dates read as UTC text with the raw value in `title`. A date is an epoch
  // number (seconds below 1e12, else milliseconds) under a time-like key, or
  // an ISO 8601 string under any key.
  //
  //   propertyValue("created_at", 1782410800000)
  //     → {kind: "date", text: "2026-06-25 18:06:40 UTC", title: "1782410800000"}
  function propertyValue(key, value) {
    let ms = null;
    if (typeof value === "number" && Number.isFinite(value) && TIME_KEY_RE.test(key)) {
      ms = value < 1e12 ? value * 1000 : value;
    } else if (typeof value === "string" && ISO_RE.test(value)) {
      ms = Date.parse(value.replace(/(\.\d{3})\d+/, "$1"));
    }
    const text = ms === null ? null : utcText(ms);
    if (text) return { kind: "date", text, title: String(value) };
    const cell = cellValue(value);
    return { kind: cell.kind, text: cell.text };
  }

  // `text` cut to `max` characters with an ellipsis when longer.
  function clipText(text, max) {
    return text.length > max
      ? { text: `${text.slice(0, max)}…`, clipped: true }
      : { text, clipped: false };
  }

  return { cellValue, defaultView, toCsv, propertyValue, clipText };
});
