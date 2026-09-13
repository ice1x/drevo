// Unit tests for the pure graph-geometry helpers in graph_math.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { meanEdgeLength } = require("./graph_math.js");

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
