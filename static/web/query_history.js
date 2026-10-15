// drevo browser — pure helpers for the query editor: the per-browser query
// history and the key that runs a query. DOM-free so they can be unit-tested
// under `node --test` (see query_history.test.js).
//
// UMD: attaches to the global `DrevoQueryHistory` in the browser (loaded
// before app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.DrevoQueryHistory = api;
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // How many past queries are kept.
  const MAX_HISTORY = 50;

  // The history with `query` added as the newest entry: trimmed, a repeat
  // moved to the front rather than duplicated, capped at MAX_HISTORY. A blank
  // query leaves the history as is. Returns a new list.
  //
  //   pushQuery(["b", "a"], "a") → ["a", "b"]
  function pushQuery(history, query) {
    const q = String(query).trim();
    if (q === "") return history.slice();
    return [q, ...history.filter((h) => h !== q)].slice(0, MAX_HISTORY);
  }

  // Move through the history (newest first). `index` is -1 for the text being
  // typed, 0 for the newest entry. Returns the new index and the text to show,
  // or `text: null` when back on the typed text.
  //
  //   stepHistory(["new", "old"], -1, "back")   → {index: 0, text: "new"}
  //   stepHistory(["new", "old"], 0, "forward") → {index: -1, text: null}
  function stepHistory(history, index, direction) {
    let next = direction === "back" ? index + 1 : index - 1;
    next = Math.max(-1, Math.min(next, history.length - 1));
    return { index: next, text: next === -1 ? null : history[next] };
  }

  // The history stored as JSON (e.g. in localStorage). Anything that is not a
  // list of strings reads as an empty history.
  function parseHistory(stored) {
    let value;
    try {
      value = JSON.parse(stored);
    } catch (_) {
      return [];
    }
    if (!Array.isArray(value)) return [];
    return value.filter((q) => typeof q === "string").slice(0, MAX_HISTORY);
  }

  // Whether a keydown in the editor runs the query. Ctrl/Cmd+Enter always
  // does; plain Enter does while the query is one line, and adds a line once
  // it has several. Shift+Enter always adds a line.
  function isRunKey(event, text) {
    if (event.key !== "Enter" || event.isComposing || event.shiftKey) return false;
    if (event.ctrlKey || event.metaKey) return true;
    return !String(text).includes("\n");
  }

  return { MAX_HISTORY, pushQuery, stepHistory, parseHistory, isRunKey };
});
