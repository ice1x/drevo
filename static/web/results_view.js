// drevo browser — pure helpers for showing a Cypher result in the Table and
// JSON views and picking the view a result opens in. DOM-free so they can be
// unit-tested under `node --test` (see results_view.test.js).
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

  return { cellValue, defaultView, toCsv };
});
