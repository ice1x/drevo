// drevo browser — pure helpers for the query editor: the per-browser query
// history, the key that runs a query, the Ask (text-to-Cypher) request and
// reply, how long a request has run, and the node menu's queries. DOM-free so
// they can be unit-tested under `node --test` (see query_history.test.js).
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

  // The POST /cypher body that asks `drevo.cypher.fromText` to turn a question
  // into Cypher. The question travels as a parameter, so quotes in it need no
  // escaping.
  function fromTextRequest(question) {
    return {
      query: "CALL drevo.cypher.fromText($question) YIELD cypher RETURN cypher",
      params: { question: String(question).trim() },
    };
  }

  // The generated query in a `fromText` reply. `answered` is false when the
  // model returned only `//` comment lines, which is how it says the schema
  // cannot answer the question, or when the reply holds no query at all.
  //
  //   generatedCypher({rows: [["MATCH (p:Project) RETURN p"]]})
  //     → {cypher: "MATCH (p:Project) RETURN p", answered: true}
  function generatedCypher(resp) {
    const cell = resp && Array.isArray(resp.rows) && resp.rows[0] ? resp.rows[0][0] : null;
    const cypher = typeof cell === "string" ? cell.trim() : "";
    const answered = cypher
      .split("\n")
      .some((line) => line.trim() !== "" && !line.trim().startsWith("//"));
    return { cypher, answered };
  }

  // How long a request has run, for the status bar: tenths of a second under
  // a minute ("12.3s"), then minutes and seconds ("1m 05s").
  function elapsedText(ms) {
    const t = Math.max(0, Number(ms) || 0);
    if (t < 60000) return `${(Math.floor(t / 100) / 10).toFixed(1)}s`;
    const seconds = Math.floor((t % 60000) / 1000);
    return `${Math.floor(t / 60000)}m ${String(seconds).padStart(2, "0")}s`;
  }

  // A node id as Cypher text: a whole, non-negative number only, so the
  // queries below can never carry anything else.
  function idText(id) {
    const n = typeof id === "string" && /^\d+$/.test(id) ? Number(id) : id;
    if (!Number.isSafeInteger(n) || n < 0) throw new Error(`not a node id: ${id}`);
    return String(n);
  }

  // The Cypher for one node, by id: `MATCH (n) WHERE id(n) = 42 RETURN n`.
  function nodeQuery(id) {
    return `MATCH (n) WHERE id(n) = ${idText(id)} RETURN n`;
  }

  // The Cypher for a node with its relationships and neighbours.
  function neighboursQuery(id) {
    return `MATCH (n)-[r]-(m) WHERE id(n) = ${idText(id)} RETURN n, r, m`;
  }

  return {
    MAX_HISTORY,
    elapsedText,
    nodeQuery,
    neighboursQuery,
    pushQuery,
    stepHistory,
    parseHistory,
    isRunKey,
    fromTextRequest,
    generatedCypher,
  };
});
