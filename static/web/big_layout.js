// drevo browser — layout of a large graph, split out of app.js so it can be
// unit-tested under `node --test` with no DOM and no Cytoscape (see
// big_layout.test.js).
//
// The positions come from two vendored libraries, and this file only
// describes the graph to them:
//   - d3-force (vendor/d3-force.min.js and the three d3 modules it needs)
//     arranges the nodes, one tick at a time, so the caller can paint
//     between ticks;
//   - WebCola's `removeOverlaps` (vendor/cola.min.js) then moves the boxes —
//     a node and the title drawn below it — apart, as little as it takes.
//
// UMD: attaches to the global `DrevoBigLayout` in the browser (loaded after
// the vendor files and before app.js) and to `module.exports` under node's
// test runner. No DOM references.
(function (root, factory) {
  "use strict";
  // The d3 files are browser builds: under node they would look for npm
  // packages, so the tests evaluate them the way a page does, on one shared
  // global of their own.
  function forceLibraryForNode() {
    const fs = require("node:fs");
    const path = require("node:path");
    const vm = require("node:vm");
    const page = vm.createContext({
      setTimeout,
      clearTimeout,
      setInterval,
      clearInterval,
      Date,
      performance,
    });
    for (const file of [
      "d3-dispatch.min.js",
      "d3-quadtree.min.js",
      "d3-timer.min.js",
      "d3-force.min.js",
    ]) {
      const source = fs.readFileSync(path.join(__dirname, "vendor", file), "utf8");
      vm.runInContext(source, page, { filename: file });
    }
    return page.d3;
  }
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory(forceLibraryForNode(), require("./vendor/cola.min.js"));
  } else {
    root.DrevoBigLayout = factory(root.d3, root.cola);
  }
})(typeof self !== "undefined" ? self : this, function (d3, cola) {
  "use strict";

  // From this many nodes on, app.js lays the graph out with this module
  // instead of fcose, which blocks the tab for seconds at this size.
  const LARGE_GRAPH_NODES = 250;

  // The title is drawn below the node: one line of this height, at most
  // TITLE_MAX wide (the `text-max-width` of the node style in app.js).
  const TITLE_LINE = 22;
  const TITLE_MAX = 120;

  // Room left between two boxes.
  const BOX_GAP = 8;

  // A node is simulated as the circle around its box, this much larger: the
  // looser the circles, the less is left for overlap removal to move.
  const CIRCLE_ROOM = 1.05;

  // Ticks of the two stages of the simulation, and the most passes of
  // overlap removal after them (one is enough unless boxes were piled up).
  const ARRANGE_TICKS = 100;
  const SPACE_TICKS = 60;
  const SEPARATE_PASSES = 4;

  function isLargeGraph(nodeCount) {
    return nodeCount >= LARGE_GRAPH_NODES;
  }

  // The box a node takes on the canvas: its circle and the title below it.
  // `offsetY` is how far the centre of the box lies below the node's centre.
  function nodeBox(diameter, titleWidth) {
    const title = Math.min(Math.max(titleWidth || 0, 0), TITLE_MAX);
    return {
      width: Math.max(diameter, title),
      height: diameter + TITLE_LINE,
      offsetY: TITLE_LINE / 2,
    };
  }

  // How many pairs of boxes (`{x, y, width, height, offsetY}`) share area,
  // or come closer than `padding` to each other. Touching boxes are apart.
  function overlappingPairs(boxes, padding, stopAtFirst) {
    const grow = (padding || 0) / 2;
    const rects = boxes.map((b) => {
      const cy = b.y + (b.offsetY || 0);
      return {
        left: b.x - b.width / 2 - grow,
        right: b.x + b.width / 2 + grow,
        top: cy - b.height / 2 - grow,
        bottom: cy + b.height / 2 + grow,
      };
    });
    rects.sort((a, b) => a.left - b.left);
    let pairs = 0;
    for (let i = 0; i < rects.length; i++) {
      const a = rects[i];
      for (let j = i + 1; j < rects.length && rects[j].left < a.right; j++) {
        const b = rects[j];
        if (a.top < b.bottom && b.top < a.bottom) {
          pairs += 1;
          if (stopAtFirst) return pairs;
        }
      }
    }
    return pairs;
  }

  function isPlaced(node) {
    return Number.isFinite(node.x) && Number.isFinite(node.y);
  }

  // Lay `nodes` (`{id, width, height, offsetY[, x, y]}`) and `edges`
  // (`{source, target}` by id) out. Returns `{step, progress, positions}`:
  // `step(units)` does at most that many units of work — a tick of the
  // simulation or a pass of overlap removal — and says whether the layout is
  // finished.
  //
  // Nodes that come with a position keep it as far as the others allow: a
  // graph where some nodes are placed is only completed — the new nodes go
  // next to a neighbour and whatever overlaps is moved apart.
  function createLayout(nodes, edges) {
    if (nodes.length === 0) {
      return { step: () => true, progress: () => 1, positions: () => [] };
    }

    // The simulation works on circles. Titles make the boxes wider than
    // tall, so it runs in a space stretched vertically until the average
    // box is square: a circle then stands for such a box with the least
    // room wasted. A body is the centre of a box in that space.
    let widths = 0;
    let heights = 0;
    for (const n of nodes) {
      widths += n.width;
      heights += n.height;
    }
    const stretch = widths > 0 && heights > 0 ? widths / heights : 1;

    const update = nodes.some(isPlaced);
    const byId = new Map();
    const bodies = nodes.map((n) => {
      const body = {
        id: n.id,
        node: n,
        radius: (Math.hypot(n.width + BOX_GAP, (n.height + BOX_GAP) * stretch) / 2) * CIRCLE_ROOM,
      };
      if (isPlaced(n)) {
        body.x = n.x;
        body.y = (n.y + (n.offsetY || 0)) * stretch;
      }
      byId.set(n.id, body);
      return body;
    });
    const links = [];
    for (const e of edges || []) {
      const source = byId.get(e.source);
      const target = byId.get(e.target);
      if (source && target && source !== target) {
        links.push({ source, target });
        source.degree = (source.degree || 0) + 1;
        target.degree = (target.degree || 0) + 1;
      }
    }
    if (update) {
      placeNewBodies(bodies, links);
      unstack(bodies);
    }

    // A fresh layout runs in two stages. The first arranges the graph as
    // points — relationships pull, everything else pushes — which is cheap
    // and decides who ends up next to whom. The picture is then enlarged
    // until the boxes have room, and the second stage evens the spacing out
    // with one circle per box.
    const simulation = d3.forceSimulation(bodies).stop();
    if (!update) {
      simulation
        .alphaDecay(1 - Math.pow(0.001, 1 / ARRANGE_TICKS))
        .force("link", d3.forceLink(links))
        .force("charge", d3.forceManyBody().theta(1.2))
        .force("x", d3.forceX(0).strength(0.05))
        .force("y", d3.forceY(0).strength(0.05));
    }

    function makeRoom() {
      let left = Infinity;
      let right = -Infinity;
      let top = Infinity;
      let bottom = -Infinity;
      let needed = 0;
      for (const b of bodies) {
        left = Math.min(left, b.x);
        right = Math.max(right, b.x);
        top = Math.min(top, b.y);
        bottom = Math.max(bottom, b.y);
        needed += 4 * b.radius * b.radius;
      }
      const area = Math.max((right - left) * (bottom - top), 1);
      const factor = Math.sqrt((1.3 * needed) / area);
      // The stretched space is `stretch` times taller than the canvas, so
      // the picture grows that much more in height here to keep, on the
      // canvas, the shape the first stage gave it.
      const wide = factor / Math.sqrt(stretch);
      const tall = factor * Math.sqrt(stretch);
      for (const b of bodies) {
        b.x *= wide;
        b.y *= tall;
        b.vx = 0;
        b.vy = 0;
      }
      simulation
        .alpha(0.3)
        .alphaDecay(1 - Math.pow(0.001 / 0.3, 1 / SPACE_TICKS))
        .force(
          "link",
          d3
            .forceLink(links)
            .distance((l) => l.source.radius + l.target.radius)
            .strength((l) => 0.3 / Math.min(l.source.degree, l.target.degree))
        )
        .force("charge", null)
        .force("x", null)
        .force("y", null)
        .force(
          "collide",
          d3
            .forceCollide((b) => b.radius)
            .strength(1)
            .iterations(2)
        );
    }

    const ticksWanted = update ? 0 : ARRANGE_TICKS + SPACE_TICKS;
    let ticks = 0;
    let halves = 0;
    let done = false;

    function positions() {
      return bodies.map((b) => ({
        id: b.id,
        x: b.x,
        y: b.y / stretch - (b.node.offsetY || 0),
      }));
    }

    function boxes() {
      return bodies.map((b) => ({
        x: b.x,
        y: b.y / stretch - (b.node.offsetY || 0),
        width: b.node.width,
        height: b.node.height,
        offsetY: b.node.offsetY,
      }));
    }

    // A pass of overlap removal over the boxes, each with half the gap
    // around it, is WebCola's `removeOverlaps` done in two units of work:
    // first along x, then along y.
    function rectangles() {
      return bodies.map((b) => {
        const halfWidth = (b.node.width + BOX_GAP) / 2;
        const halfHeight = (b.node.height + BOX_GAP) / 2;
        const cy = b.y / stretch;
        return new cola.Rectangle(b.x - halfWidth, b.x + halfWidth, cy - halfHeight, cy + halfHeight);
      });
    }

    function separateAlongX() {
      const rects = rectangles();
      const centres = rects.map((r) => new cola.Variable(r.cx()));
      new cola.Solver(centres, cola.generateXConstraints(rects, centres)).solve();
      bodies.forEach((b, i) => {
        b.x = centres[i].position();
      });
    }

    function separateAlongY() {
      const rects = rectangles();
      const centres = rects.map((r) => new cola.Variable(r.cy()));
      new cola.Solver(centres, cola.generateYConstraints(rects, centres)).solve();
      bodies.forEach((b, i) => {
        b.y = centres[i].position() * stretch;
      });
    }

    function step(units) {
      for (let i = 0; i < units && !done; i++) {
        if (ticks < ticksWanted) {
          if (ticks === ARRANGE_TICKS) makeRoom();
          simulation.tick();
          ticks += 1;
          continue;
        }
        if (halves % 2 === 1) {
          separateAlongY();
          halves += 1;
          continue;
        }
        if (halves >= 2 * SEPARATE_PASSES || overlappingPairs(boxes(), 0, true) === 0) {
          done = true;
          break;
        }
        separateAlongX();
        halves += 1;
      }
      return done;
    }

    function progress() {
      if (done) return 1;
      return Math.min(0.99, (ticks + halves) / (ticksWanted + 2));
    }

    return { step, progress, positions };
  }

  // Bodies that sit on exactly the same point give overlap removal no
  // direction to move them in: fan each such pile out around its point.
  function unstack(bodies) {
    const piles = new Map();
    for (const b of bodies) {
      const key = b.x + "," + b.y;
      if (!piles.has(key)) piles.set(key, []);
      piles.get(key).push(b);
    }
    const turn = Math.PI * (3 - Math.sqrt(5));
    for (const pile of piles.values()) {
      for (let i = 1; i < pile.length; i++) {
        const far = pile[i].radius * Math.sqrt(i);
        pile[i].x += far * Math.cos(i * turn);
        pile[i].y += far * Math.sin(i * turn);
      }
    }
  }

  // Give every body without a position one next to a placed neighbour, or
  // around the placed graph when it has none.
  function placeNewBodies(bodies, links) {
    const neighbours = new Map();
    for (const l of links) {
      for (const [a, b] of [
        [l.source, l.target],
        [l.target, l.source],
      ]) {
        if (!neighbours.has(a)) neighbours.set(a, []);
        neighbours.get(a).push(b);
      }
    }
    let cx = 0;
    let cy = 0;
    let placed = 0;
    for (const b of bodies) {
      if (b.x === undefined) continue;
      cx += b.x;
      cy += b.y;
      placed += 1;
    }
    cx /= placed;
    cy /= placed;
    let reach = 0;
    for (const b of bodies) {
      if (b.x !== undefined) reach = Math.max(reach, Math.hypot(b.x - cx, b.y - cy) + b.radius);
    }
    // The golden angle spreads the new nodes of one neighbour around it.
    const turn = Math.PI * (3 - Math.sqrt(5));
    let waiting = bodies.filter((b) => b.x === undefined);
    let order = 0;
    while (waiting.length > 0) {
      const next = [];
      let progressed = false;
      for (const b of waiting) {
        const anchor = (neighbours.get(b) || []).find((o) => o.x !== undefined);
        if (!anchor) {
          next.push(b);
          continue;
        }
        const angle = order * turn;
        const far = anchor.radius + b.radius;
        b.x = anchor.x + Math.cos(angle) * far;
        b.y = anchor.y + Math.sin(angle) * far;
        order += 1;
        progressed = true;
      }
      if (!progressed) {
        for (const b of next) {
          const angle = order * turn;
          b.x = cx + Math.cos(angle) * (reach + b.radius);
          b.y = cy + Math.sin(angle) * (reach + b.radius);
          order += 1;
        }
        break;
      }
      waiting = next;
    }
  }

  return { LARGE_GRAPH_NODES, isLargeGraph, nodeBox, overlappingPairs, createLayout };
});
