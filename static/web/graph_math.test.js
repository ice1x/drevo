// Unit tests for the pure graph-geometry helpers in graph_math.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  meanEdgeLength,
  segmentLength,
  nodeLabel,
  visibleKinds,
  colorCandidates,
  colorValue,
  legendEntries,
} = require("./graph_math.js");

test("meanEdgeLength averages the segment lengths", () => {
  // A 3-long and a 4-long segment → mean 3.5.
  const segments = [
    { x1: 0, y1: 0, x2: 3, y2: 0 }, // length 3
    { x1: 0, y1: 0, x2: 0, y2: 4 }, // length 4
  ];
  assert.equal(meanEdgeLength(segments), 3.5);
});

test("meanEdgeLength uses the Euclidean (hypotenuse) distance", () => {
  // 3-4-5 triangle: a single (0,0)->(3,4) segment is length 5.
  assert.equal(meanEdgeLength([{ x1: 0, y1: 0, x2: 3, y2: 4 }]), 5);
});

test("meanEdgeLength falls back to 90 on an empty graph", () => {
  assert.equal(meanEdgeLength([]), 90);
});

test("meanEdgeLength honours a finite custom fallback", () => {
  assert.equal(meanEdgeLength([], 42), 42);
});

test("meanEdgeLength ignores a non-finite fallback", () => {
  assert.equal(meanEdgeLength([], NaN), 90);
  assert.equal(meanEdgeLength([], Infinity), 90);
});

test("meanEdgeLength skips zero-length and non-finite segments", () => {
  const segments = [
    { x1: 5, y1: 5, x2: 5, y2: 5 }, // zero-length → skipped
    { x1: 0, y1: 0, x2: NaN, y2: 0 }, // non-finite → skipped
    { x1: 0, y1: 0, x2: 10, y2: 0 }, // length 10 → the only counted one
  ];
  assert.equal(meanEdgeLength(segments), 10);
});

test("meanEdgeLength falls back when every segment is degenerate", () => {
  const segments = [
    { x1: 1, y1: 1, x2: 1, y2: 1 },
    { x1: 2, y1: 2, x2: 2, y2: 2 },
  ];
  assert.equal(meanEdgeLength(segments), 90);
});

// segmentLength — the per-edge rest length backing the jolt-free live drag.

test("segmentLength returns the Euclidean length of one segment", () => {
  // 3-4-5 triangle.
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: 3, y2: 4 }), 5);
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: 10, y2: 0 }), 10);
});

test("segmentLength preserves per-edge variation (no averaging)", () => {
  // The whole point of the fix: a short and a long edge keep their OWN lengths,
  // so cola starts every spring at rest instead of at a shared mean.
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: 20, y2: 0 }), 20);
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: 200, y2: 0 }), 200);
});

test("segmentLength falls back to 90 on a degenerate segment", () => {
  assert.equal(segmentLength({ x1: 5, y1: 5, x2: 5, y2: 5 }), 90); // zero length
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: NaN, y2: 0 }), 90); // non-finite
  assert.equal(segmentLength(null), 90);
  assert.equal(segmentLength(undefined), 90);
});

test("segmentLength honours a finite custom fallback, ignores a bad one", () => {
  assert.equal(segmentLength({ x1: 0, y1: 0, x2: 0, y2: 0 }, 42), 42);
  assert.equal(segmentLength(null, Infinity), 90);
});

test("nodeLabel shows a real title as-is", () => {
  assert.equal(nodeLabel({ id: 1, kind: "Note", title: "Groceries", properties: { name: "x" } }), "Groceries");
});

test("nodeLabel hides the synthesised placeholder title (#545)", () => {
  const synth = "__cypher__:Person:01a0f3aa968d7892884ea14067ed2018";
  assert.equal(nodeLabel({ id: 7, kind: "Person", title: synth, properties: { name: "Ada" } }), "Ada");
  assert.equal(nodeLabel({ id: 7, kind: "Person", title: synth, properties: {} }), "Person #7");
  assert.equal(nodeLabel({ id: 7, kind: "Person", title: synth }), "Person #7");
});

test("nodeLabel falls back through common naming properties", () => {
  assert.equal(nodeLabel({ id: 2, kind: "Msg", properties: { text: "hello" } }), "hello");
  assert.equal(nodeLabel({ id: 3, kind: "Row", properties: { label: "L" } }), "L");
  assert.equal(nodeLabel({ id: 4, kind: "N", properties: { name: 42 } }), "42");
  assert.equal(nodeLabel({ id: 5, properties: { title: "__cypher__:N:abc" } }), "node #5");
  assert.equal(nodeLabel(null), "");
});

// ── visibleKinds ─────────────────────────────────────────────────────

const COUNTS = [
  ["Entity", 3307],
  ["Migration", 213],
  ["Task", 71],
  ["Entity:Task", 23],
  ["task", 1],
];

test("visibleKinds shows the top labels and counts the rest as hidden", () => {
  const v = visibleKinds(COUNTS, { limit: 2 });
  assert.deepEqual(v.shown.map(([k]) => k), ["Entity", "Migration"]);
  assert.equal(v.hidden, 3);
});

test("visibleKinds shows everything when expanded", () => {
  const v = visibleKinds(COUNTS, { limit: 2, expanded: true });
  assert.equal(v.shown.length, 5);
  assert.equal(v.hidden, 0);
});

test("visibleKinds keeps the active label visible even outside the top", () => {
  const v = visibleKinds(COUNTS, { limit: 2, active: "task" });
  assert.deepEqual(v.shown.map(([k]) => k), ["Entity", "Migration", "task"]);
  assert.equal(v.hidden, 2);
});

test("a filter matches case-insensitively anywhere and shows every match", () => {
  const v = visibleKinds(COUNTS, { limit: 1, filter: "TASK" });
  assert.deepEqual(v.shown.map(([k]) => k), ["Task", "Entity:Task", "task"]);
  assert.equal(v.hidden, 0);
});

test("a blank filter is no filter", () => {
  assert.deepEqual(visibleKinds(COUNTS, { limit: 2, filter: "  " }), visibleKinds(COUNTS, { limit: 2 }));
});

test("a filter that matches nothing shows nothing", () => {
  assert.deepEqual(visibleKinds(COUNTS, { filter: "zzz" }), { shown: [], hidden: 0 });
});

// ── Colour by ────────────────────────────────────────────────────────

const mk = (id, kind, properties) => ({ id, kind, title: `n${id}`, properties });
const NODES = [
  mk(1, "Entity", { type: "Module", project: "a", name: "x1", status: "open" }),
  mk(2, "Entity", { type: "Module", project: "a", name: "x2" }),
  mk(3, "Entity", { type: "Task", project: "b", name: "x3" }),
  mk(4, "Entity", { type: "Task", project: "b", name: "x4" }),
  mk(5, "Task", { name: "x5", count: 3 }),
];

test("colorCandidates offers string properties whose top values cover most nodes", () => {
  // type and project: their top 2 values cover all 4 nodes that have them.
  // name: every value differs, so the top 2 cover only 2 of 5; status sits on
  // 1 of 5 nodes (< 30%); count is not a string.
  assert.deepEqual(colorCandidates(NODES, { topValues: 2 }), ["project", "type"]);
});

test("a long tail of rare values is fine while the top values dominate", () => {
  const nodes = [];
  for (let i = 0; i < 80; i++) nodes.push(mk(i, "E", { type: i % 2 ? "Task" : "File" }));
  for (let i = 80; i < 100; i++) nodes.push(mk(i, "E", { type: `rare${i}` }));
  assert.deepEqual(colorCandidates(nodes, { topValues: 2 }), ["type"]);
});

test("colorCandidates orders by coverage, then by key", () => {
  const nodes = [...NODES, mk(6, "X", { project: "a" })];
  assert.deepEqual(colorCandidates(nodes, { topValues: 2 }), ["project", "type"]);
});

test("a property with a single value is not worth colouring by", () => {
  const nodes = [mk(1, "A", { env: "prod" }), mk(2, "A", { env: "prod" })];
  assert.deepEqual(colorCandidates(nodes), []);
});

test("colorValue reads the label or the chosen property", () => {
  assert.equal(colorValue(NODES[0], "kind"), "Entity");
  assert.equal(colorValue(NODES[0], "type"), "Module");
  assert.equal(colorValue(NODES[4], "type"), "", "no value reads as empty");
});

test("legendEntries counts values, most common first, and caps the list", () => {
  assert.deepEqual(legendEntries(NODES, "type", 2), {
    entries: [["Module", 2], ["Task", 2]],
    more: 1,
  });
  assert.deepEqual(legendEntries(NODES, "kind", 5), {
    entries: [["Entity", 4], ["Task", 1]],
    more: 0,
  });
});

test("legendEntries lists a missing value last, as an empty key", () => {
  const { entries } = legendEntries(NODES, "type", 5);
  assert.deepEqual(entries[entries.length - 1], ["", 1]);
});
