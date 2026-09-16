// drevo browser — pure graph-geometry helpers, split out of app.js so they can
// be unit-tested under `node --test` with no DOM and no Cytoscape (see
// graph_math.test.js). These are the numbers that drive the interactive layout,
// so a regression here is a visible UI bug — hence a real, executable test.
//
// UMD: attaches to the global `DrevoGraphMath` in the browser (loaded before
// app.js) and to `module.exports` under node's test runner. No DOM references.
(function (root, factory) {
  "use strict";
  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.DrevoGraphMath = api;
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // Mean length of a list of line segments `{x1, y1, x2, y2}`, ignoring
  // degenerate ones (zero-length or non-finite coordinates). Falls back to
  // `fallback` (default 90) when no usable segment remains.
  //
  // This backs the live drag simulation's spring rest-length: feeding it the
  // graph's CURRENT mean edge length makes the cola sim start already at
  // equilibrium, so a drag only nudges the local neighbourhood instead of
  // springing the whole graph to a different edge length (the #440 bug).
  function meanEdgeLength(segments, fallback) {
    const fb = typeof fallback === "number" && Number.isFinite(fallback) ? fallback : 90;
    let sum = 0;
    let n = 0;
    for (const s of segments) {
      const d = Math.hypot(s.x1 - s.x2, s.y1 - s.y2);
      if (Number.isFinite(d) && d > 0) {
        sum += d;
        n += 1;
      }
    }
    return n > 0 ? sum / n : fb;
  }

  // Length of a single line segment `{x1, y1, x2, y2}`, falling back to
  // `fallback` (default 90) for a degenerate (zero-length or non-finite) one.
  //
  // This backs the live drag simulation's PER-EDGE spring rest-length: giving
  // cola each edge's *own* current length as its rest length means every spring
  // starts exactly at rest, so a drag perturbs only the dragged node's
  // neighbourhood and the rest of the graph stays put — instead of a single
  // mean rest-length springing every off-mean edge (the hub's long and short
  // edges) toward one value and jolting the whole graph on the first tick.
  function segmentLength(segment, fallback) {
    const fb = typeof fallback === "number" && Number.isFinite(fallback) ? fallback : 90;
    if (!segment) return fb;
    const d = Math.hypot(segment.x1 - segment.x2, segment.y1 - segment.y2);
    return Number.isFinite(d) && d > 0 ? d : fb;
  }

  return { meanEdgeLength, segmentLength };
});
