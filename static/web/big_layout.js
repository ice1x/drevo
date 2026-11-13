// drevo browser — layout of a large graph, split out of app.js so it can be
// unit-tested under `node --test` with no DOM and no Cytoscape (see
// big_layout.test.js).
//
// UMD: attaches to the global `DrevoBigLayout` in the browser (loaded before
// app.js) and to `module.exports` under node's test runner. No DOM references.
(function (root, factory) {
  "use strict";
  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.DrevoBigLayout = api;
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  const LARGE_GRAPH_NODES = 250;

  function isLargeGraph() {
    return false;
  }

  function nodeBox() {
    return { width: 1, height: 1, offsetY: 0 };
  }

  function overlappingPairs() {
    return 0;
  }

  function createLayout(nodes) {
    return {
      step: () => true,
      progress: () => 1,
      positions: () => nodes.map((n) => ({ id: n.id, x: 0, y: 0 })),
    };
  }

  return { LARGE_GRAPH_NODES, isLargeGraph, nodeBox, overlappingPairs, createLayout };
});
