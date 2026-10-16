// Unit tests for the query-result helpers in results_view.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { cellValue, defaultView, toCsv, propertyValue, clipText } = require("./results_view.js");

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

// ── propertyValue (inspector rows) ───────────────────────────────────

test("an epoch-millis value under a time-like key reads as a UTC date", () => {
  assert.deepEqual(propertyValue("created_at", 1782410800000), {
    kind: "date",
    text: "2026-06-25 18:06:40 UTC",
    title: "1782410800000",
  });
});

test("epoch seconds under a time-like key are recognised too", () => {
  assert.equal(propertyValue("updated_ts", 1782410800).text, "2026-06-25 18:06:40 UTC");
});

test("a number under an ordinary key stays a number", () => {
  assert.deepEqual(propertyValue("count", 1782410800000), {
    kind: "number",
    text: "1782410800000",
  });
});

test("an ISO 8601 string reads as the same UTC date format, under any key", () => {
  assert.deepEqual(propertyValue("created", "2026-02-21T15:02:07.642000000+00:00"), {
    kind: "date",
    text: "2026-02-21 15:02:07 UTC",
    title: "2026-02-21T15:02:07.642000000+00:00",
  });
  assert.equal(propertyValue("due", "2026-02-21T18:02:07+03:00").text, "2026-02-21 15:02:07 UTC");
  assert.equal(propertyValue("seen", "2026-02-21T15:02:07Z").text, "2026-02-21 15:02:07 UTC");
});

test("a plain date or a date-like word is left as text", () => {
  assert.deepEqual(propertyValue("day", "2026-02-21"), { kind: "string", text: "2026-02-21" });
  assert.equal(propertyValue("note", "2026-02-21T is not a date").kind, "string");
});

test("strings lose their quotes; null, lists and maps read like table cells", () => {
  assert.deepEqual(propertyValue("name", "Task052"), { kind: "string", text: "Task052" });
  assert.deepEqual(propertyValue("x", null), { kind: "null", text: "null" });
  assert.deepEqual(propertyValue("tags", ["a", "b"]), { kind: "list", text: '["a","b"]' });
});

// ── clipText ─────────────────────────────────────────────────────────

test("clipText leaves short text alone", () => {
  assert.deepEqual(clipText("short", 10), { text: "short", clipped: false });
});

test("clipText cuts long text at the limit and marks it", () => {
  assert.deepEqual(clipText("abcdefghij", 4), { text: "abcd…", clipped: true });
});
