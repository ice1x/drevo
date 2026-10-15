// Unit tests for the query-result helpers in results_view.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { cellValue, defaultView, toCsv } = require("./results_view.js");

const node = { id: 7, kind: "Task", title: "Ship it", properties: {} };
const rel = { id: 3, kind: "DEPENDS_ON", from_id: 7, to_id: 8, properties: {} };

// ── cellValue ────────────────────────────────────────────────────────

test("a string cell shows its text without JSON quotes", () => {
  assert.deepEqual(cellValue("open"), { kind: "string", text: "open" });
});

test("an empty string stays an empty string, not null", () => {
  assert.deepEqual(cellValue(""), { kind: "string", text: "" });
});

test("null and a missing value are both null", () => {
  assert.deepEqual(cellValue(null), { kind: "null", text: "null" });
  assert.deepEqual(cellValue(undefined), { kind: "null", text: "null" });
});

test("the string 'null' is a string, distinguishable from null", () => {
  assert.equal(cellValue("null").kind, "string");
});

test("numbers and booleans show as plain text", () => {
  assert.deepEqual(cellValue(42), { kind: "number", text: "42" });
  assert.deepEqual(cellValue(-0.5), { kind: "number", text: "-0.5" });
  assert.deepEqual(cellValue(true), { kind: "boolean", text: "true" });
  assert.deepEqual(cellValue(false), { kind: "boolean", text: "false" });
});

test("a node cell is labelled by its title and carries its id", () => {
  assert.deepEqual(cellValue(node), { kind: "node", text: "Ship it", id: 7 });
});

test("an untitled node falls back to its name property", () => {
  const n = { id: 9, kind: "Person", title: "", properties: { name: "Ada" } };
  assert.equal(cellValue(n).text, "Ada");
});

test("a relationship cell shows its type and both endpoint ids", () => {
  assert.deepEqual(cellValue(rel), {
    kind: "relationship",
    text: "(7)-[:DEPENDS_ON]->(8)",
    id: 3,
  });
});

test("lists and maps show as compact JSON", () => {
  assert.deepEqual(cellValue([1, "a", null]), { kind: "list", text: '[1,"a",null]' });
  assert.deepEqual(cellValue({ a: 1, b: [2] }), { kind: "map", text: '{"a":1,"b":[2]}' });
});

test("a map that merely has an id is a map, not a node", () => {
  assert.equal(cellValue({ id: 1, name: "x" }).kind, "map");
});

// ── defaultView ──────────────────────────────────────────────────────

test("a result with nodes opens the graph view", () => {
  const resp = { columns: ["n"], rows: [[node]], graph: { nodes: [node], edges: [] } };
  assert.equal(defaultView(resp), "graph");
});

test("a result with only scalars opens the table view", () => {
  const resp = { columns: ["t"], rows: [["a"]], graph: { nodes: [], edges: [] } };
  assert.equal(defaultView(resp), "table");
});

test("a result without a graph section opens the table view", () => {
  assert.equal(defaultView({ columns: [], rows: [] }), "table");
  assert.equal(defaultView(null), "table");
});

// ── toCsv ────────────────────────────────────────────────────────────

test("toCsv writes a header row and one line per row, CRLF-separated", () => {
  const csv = toCsv(["title", "n"], [["a", 1], ["b", 2]]);
  assert.equal(csv, "title,n\r\na,1\r\nb,2");
});

test("toCsv quotes fields with commas, quotes or line breaks", () => {
  const csv = toCsv(["v"], [["a,b"], ['say "hi"'], ["two\nlines"]]);
  assert.equal(csv, 'v\r\n"a,b"\r\n"say ""hi"""\r\n"two\nlines"');
});

test("toCsv leaves null cells empty and labels nodes", () => {
  const csv = toCsv(["n", "x"], [[node, null]]);
  assert.equal(csv, "n,x\r\nShip it,");
});

test("toCsv quotes a header that needs it", () => {
  assert.equal(toCsv(["a,b"], []), '"a,b"');
});
