// Unit tests for the large-graph layout in big_layout.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  LARGE_GRAPH_NODES,
  isLargeGraph,
  nodeBox,
  overlappingPairs,
  createLayout,
} = require("./big_layout.js");

// A small deterministic generator, so every run lays out the same graph.
function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

// A graph shaped like a real knowledge graph: a few hubs with many leaves,
// a web of ordinary nodes, and nodes with no relationship at all.
function knowledgeGraph(n, seed) {
  const rand = rng(seed);
  const hubs = Math.max(3, Math.round(n / 200));
  const nodes = [];
  const edges = [];
  for (let i = 0; i < n; i++) {
    const diameter = 22 + Math.floor(rand() * 33);
    const title = 20 + Math.floor(rand() * 101);
    nodes.push({ id: i, ...nodeBox(diameter, title) });
    if (i < hubs) continue;
    const roll = rand();
    if (roll < 0.4) {
      edges.push({ source: Math.floor(rand() * hubs), target: i });
    } else if (roll < 0.85) {
      const links = 1 + Math.floor(rand() * 3);
      for (let k = 0; k < links; k++) {
        edges.push({ source: i, target: Math.floor(rand() * i) });
      }
    }
  }
  return { nodes, edges };
}

function run(layout) {
  let units = 0;
  while (!layout.step(1)) {
    units += 1;
    assert.ok(units < 100000, "the layout must finish");
  }
  return units + 1;
}

// The boxes of a finished layout, checked pair by pair with no help from
// the module under test.
function placedBoxes(nodes, positions) {
  const at = new Map(positions.map((p) => [p.id, p]));
  return nodes.map((n) => {
    const p = at.get(n.id);
    const cy = p.y + (n.offsetY || 0);
    return {
      id: n.id,
      left: p.x - n.width / 2,
      right: p.x + n.width / 2,
      top: cy - n.height / 2,
      bottom: cy + n.height / 2,
    };
  });
}
function bruteOverlaps(boxes) {
  let pairs = 0;
  for (let i = 0; i < boxes.length; i++) {
    const a = boxes[i];
    for (let j = i + 1; j < boxes.length; j++) {
      const b = boxes[j];
      if (a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom) {
        pairs += 1;
      }
    }
  }
  return pairs;
}
function distance(a, b) {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

test("a graph is large from LARGE_GRAPH_NODES nodes on", () => {
  assert.equal(LARGE_GRAPH_NODES, 250);
  assert.equal(isLargeGraph(249), false);
  assert.equal(isLargeGraph(250), true);
  assert.equal(isLargeGraph(5000), true);
  assert.equal(isLargeGraph(0), false);
});

test("nodeBox covers the circle and the title drawn below it", () => {
  // A 30px circle with a 100px title: as wide as the title, as tall as the
  // circle plus the title line, centred half a title line below the node.
  const box = nodeBox(30, 100);
  assert.equal(box.width, 100);
  assert.equal(box.height, 30 + 22);
  assert.equal(box.offsetY, 11);
});

test("nodeBox is never narrower than the circle and caps the title at 120px", () => {
  assert.equal(nodeBox(54, 10).width, 54);
  assert.equal(nodeBox(22, 400).width, 120);
  assert.equal(nodeBox(22, 0).width, 22);
});

test("overlappingPairs counts the boxes that share area", () => {
  const box = { width: 100, height: 50, offsetY: 0 };
  const boxes = [
    { ...box, x: 0, y: 0 },
    { ...box, x: 60, y: 10 }, // overlaps the first
    { ...box, x: 300, y: 0 }, // far from both
  ];
  assert.equal(overlappingPairs(boxes), 1);
});

test("overlappingPairs treats touching boxes as apart and honours the offset", () => {
  const box = { width: 100, height: 50, offsetY: 0 };
  assert.equal(
    overlappingPairs([
      { ...box, x: 0, y: 0 },
      { ...box, x: 100, y: 0 },
    ]),
    0
  );
  // Same centres 40px apart vertically: they overlap, until one box hangs
  // 20px lower than its node.
  assert.equal(
    overlappingPairs([
      { ...box, x: 0, y: 0 },
      { ...box, x: 0, y: 40 },
    ]),
    1
  );
  assert.equal(
    overlappingPairs([
      { ...box, x: 0, y: 0 },
      { ...box, x: 0, y: 40, offsetY: 20 },
    ]),
    0
  );
});

test("overlappingPairs with a padding asks for that much room between boxes", () => {
  const box = { width: 100, height: 50, offsetY: 0 };
  const boxes = [
    { ...box, x: 0, y: 0 },
    { ...box, x: 104, y: 0 },
  ];
  assert.equal(overlappingPairs(boxes), 0);
  assert.equal(overlappingPairs(boxes, 8), 1);
});

test("an empty graph is laid out at once", () => {
  const layout = createLayout([], []);
  assert.equal(layout.step(1), true);
  assert.equal(layout.progress(), 1);
  assert.deepEqual(layout.positions(), []);
});

test("a single node is placed at a finite position", () => {
  const layout = createLayout([{ id: "a", ...nodeBox(30, 60) }], []);
  run(layout);
  const [p] = layout.positions();
  assert.equal(p.id, "a");
  assert.ok(Number.isFinite(p.x) && Number.isFinite(p.y));
});

test("every node gets a finite position under its own id", () => {
  const { nodes, edges } = knowledgeGraph(400, 1);
  const layout = createLayout(nodes, edges);
  run(layout);
  const positions = layout.positions();
  assert.equal(positions.length, nodes.length);
  assert.deepEqual(
    positions.map((p) => p.id),
    nodes.map((n) => n.id)
  );
  for (const p of positions) {
    assert.ok(Number.isFinite(p.x) && Number.isFinite(p.y), `node ${p.id} has a position`);
  }
});

test("step does at most the units asked for and reports progress", () => {
  const { nodes, edges } = knowledgeGraph(300, 2);
  const layout = createLayout(nodes, edges);
  assert.equal(layout.progress(), 0);
  let last = 0;
  let steps = 0;
  let done = false;
  while (!done) {
    done = layout.step(5);
    steps += 1;
    const now = layout.progress();
    assert.ok(now >= last, "progress never goes back");
    assert.ok(now <= 1);
    last = now;
    assert.ok(steps < 20000, "the layout must finish");
  }
  assert.ok(steps > 10, "300 nodes take more than a handful of steps");
  assert.equal(layout.progress(), 1);
  // A finished layout stays finished and keeps its positions.
  const before = JSON.stringify(layout.positions());
  assert.equal(layout.step(100), true);
  assert.equal(JSON.stringify(layout.positions()), before);
});

test("the number of units is bounded whatever the graph size", () => {
  const small = knowledgeGraph(300, 3);
  const big = knowledgeGraph(3000, 3);
  const unitsSmall = run(createLayout(small.nodes, small.edges));
  const unitsBig = run(createLayout(big.nodes, big.edges));
  assert.ok(unitsSmall <= 700, `300 nodes took ${unitsSmall} units`);
  assert.ok(unitsBig <= 700, `3000 nodes took ${unitsBig} units`);
});

test("no two boxes overlap after the layout of a dense 2000-node graph", () => {
  const { nodes, edges } = knowledgeGraph(2000, 4);
  const layout = createLayout(nodes, edges);
  run(layout);
  const boxes = placedBoxes(nodes, layout.positions());
  assert.equal(bruteOverlaps(boxes), 0);
});

test("a hub's two hundred leaves are packed around it without overlapping", () => {
  const nodes = [{ id: 0, ...nodeBox(54, 80) }];
  const edges = [];
  for (let i = 1; i <= 200; i++) {
    nodes.push({ id: i, ...nodeBox(24, 110) });
    edges.push({ source: 0, target: i });
  }
  const layout = createLayout(nodes, edges);
  run(layout);
  const positions = layout.positions();
  assert.equal(bruteOverlaps(placedBoxes(nodes, positions)), 0);
  // 200 boxes of 110×46 cover about 1M px²: a disc of radius ~570px. The
  // leaves stay within twice that of their hub.
  const hub = positions[0];
  for (const p of positions.slice(1)) {
    assert.ok(distance(hub, p) < 1200, `leaf ${p.id} is ${distance(hub, p)}px from the hub`);
  }
});

test("related nodes end up closer than unrelated ones", () => {
  const { nodes, edges } = knowledgeGraph(1000, 5);
  const layout = createLayout(nodes, edges);
  run(layout);
  const at = new Map(layout.positions().map((p) => [p.id, p]));
  let linked = 0;
  for (const e of edges) linked += distance(at.get(e.source), at.get(e.target));
  linked /= edges.length;
  const rand = rng(99);
  let unrelated = 0;
  const samples = 4000;
  for (let i = 0; i < samples; i++) {
    const a = Math.floor(rand() * nodes.length);
    const b = Math.floor(rand() * nodes.length);
    unrelated += distance(at.get(a), at.get(b));
  }
  unrelated /= samples;
  assert.ok(
    linked < unrelated * 0.5,
    `mean edge ${Math.round(linked)}px vs mean pair ${Math.round(unrelated)}px`
  );
});

test("the layout stays compact: its area is a small multiple of the boxes", () => {
  const { nodes, edges } = knowledgeGraph(2000, 6);
  const layout = createLayout(nodes, edges);
  run(layout);
  const boxes = placedBoxes(nodes, layout.positions());
  let left = Infinity;
  let right = -Infinity;
  let top = Infinity;
  let bottom = -Infinity;
  let covered = 0;
  for (const b of boxes) {
    left = Math.min(left, b.left);
    right = Math.max(right, b.right);
    top = Math.min(top, b.top);
    bottom = Math.max(bottom, b.bottom);
    covered += (b.right - b.left) * (b.bottom - b.top);
  }
  const area = (right - left) * (bottom - top);
  assert.ok(area <= covered * 8, `area is ${(area / covered).toFixed(1)}× the boxes`);
});

test("the same graph is laid out the same way every time", () => {
  const a = knowledgeGraph(500, 7);
  const b = knowledgeGraph(500, 7);
  const first = createLayout(a.nodes, a.edges);
  const second = createLayout(b.nodes, b.edges);
  run(first);
  run(second);
  assert.deepEqual(first.positions(), second.positions());
});

test("edges to unknown nodes and self-loops are ignored", () => {
  const nodes = [
    { id: "a", ...nodeBox(30, 60) },
    { id: "b", ...nodeBox(30, 60) },
  ];
  const edges = [
    { source: "a", target: "b" },
    { source: "a", target: "a" },
    { source: "a", target: "ghost" },
    { source: "ghost", target: "b" },
  ];
  const layout = createLayout(nodes, edges);
  run(layout);
  const [a, b] = layout.positions();
  assert.ok(Number.isFinite(a.x) && Number.isFinite(b.x));
  assert.equal(bruteOverlaps(placedBoxes(nodes, [a, b])), 0);
});

test("nodes with no relationship stay near the graph", () => {
  const nodes = [];
  for (let i = 0; i < 600; i++) nodes.push({ id: i, ...nodeBox(22, 90) });
  const layout = createLayout(nodes, []);
  run(layout);
  const positions = layout.positions();
  assert.equal(bruteOverlaps(placedBoxes(nodes, positions)), 0);
  // 600 boxes of 90×44 cover 2.4M px²; a square holding them at a quarter
  // density is ~3100px wide.
  const xs = positions.map((p) => p.x);
  const ys = positions.map((p) => p.y);
  assert.ok(Math.max(...xs) - Math.min(...xs) < 4000);
  assert.ok(Math.max(...ys) - Math.min(...ys) < 4000);
});

test("nodes that start at the same point are pulled apart", () => {
  const nodes = [];
  for (let i = 0; i < 50; i++) nodes.push({ id: i, x: 10, y: 10, ...nodeBox(30, 80) });
  const layout = createLayout(nodes, []);
  run(layout);
  assert.equal(bruteOverlaps(placedBoxes(nodes, layout.positions())), 0);
});

test("adding a node to a laid-out graph keeps the others where they are", () => {
  const { nodes, edges } = knowledgeGraph(600, 8);
  const first = createLayout(nodes, edges);
  run(first);
  const placed = first.positions();
  const at = new Map(placed.map((p) => [p.id, p]));
  // Expand node 300: three new neighbours, the rest start where they were.
  const again = nodes.map((n) => ({ ...n, x: at.get(n.id).x, y: at.get(n.id).y }));
  const moreEdges = edges.slice();
  for (const id of ["new1", "new2", "new3"]) {
    again.push({ id, ...nodeBox(24, 90) });
    moreEdges.push({ source: 300, target: id });
  }
  const second = createLayout(again, moreEdges);
  const units = run(second);
  const after = new Map(second.positions().map((p) => [p.id, p]));
  let moved = 0;
  for (const p of placed) moved += distance(p, after.get(p.id));
  moved /= placed.length;
  assert.ok(moved < 40, `the old nodes moved ${moved.toFixed(1)}px on average`);
  for (const id of ["new1", "new2", "new3"]) {
    const d = distance(after.get(300), after.get(id));
    assert.ok(d < 400, `${id} is ${Math.round(d)}px from the node it was expanded from`);
  }
  assert.equal(bruteOverlaps(placedBoxes(again, second.positions())), 0);
  assert.ok(units < run(createLayout(nodes, edges)), "an update takes fewer units than a fresh layout");
});

test("5000 nodes are laid out in seconds, in short units", () => {
  const { nodes, edges } = knowledgeGraph(5000, 9);
  const layout = createLayout(nodes, edges);
  const started = Date.now();
  let longest = 0;
  let done = false;
  while (!done) {
    const t0 = Date.now();
    done = layout.step(1);
    longest = Math.max(longest, Date.now() - t0);
  }
  const total = Date.now() - started;
  // fcose needs 13 s for 2000 nodes in one uninterruptible call. The bounds
  // are loose enough for a shared CI runner.
  assert.ok(total < 20000, `5000 nodes took ${total}ms`);
  assert.ok(longest < 1000, `the longest unit took ${longest}ms`);
  assert.equal(bruteOverlaps(placedBoxes(nodes, layout.positions())), 0);
});
