// drevo browser — pure helpers for the running-transactions list in the
// Storage panel: the SHOW TRANSACTIONS query it runs, its rows as objects,
// elapsed times and the TERMINATE request. DOM-free so they can be
// unit-tested under `node --test` (see transactions_view.test.js).
//
// UMD: attaches to the global `DrevoTransactionsView` in the browser (loaded
// before app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.DrevoTransactionsView = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  const COLUMNS = [
    "transactionId",
    "username",
    "protocol",
    "clientAddress",
    "currentQuery",
    "status",
    "isExplicit",
    "elapsedTimeMillis",
  ];

  // What the panel runs to list them.
  const SHOW_QUERY =
    "SHOW TRANSACTIONS YIELD " + COLUMNS.join(", ") + " RETURN " + COLUMNS.join(", ");

  // A `/cypher` answer `{columns, rows}` as one object per transaction,
  // longest running first, without the panel's own SHOW.
  //
  //   rowsFrom({columns: ["transactionId", …], rows: [["drevo-transaction-7", …]]})
  //     → [{id: "drevo-transaction-7", user: "ann", protocol: "bolt", …}]
  function rowsFrom(answer) {
    if (!answer || !Array.isArray(answer.columns) || !Array.isArray(answer.rows)) {
      return [];
    }
    const at = (name) => answer.columns.indexOf(name);
    const pick = (row, name) => {
      const i = at(name);
      return i >= 0 && i < row.length && row[i] !== undefined ? row[i] : null;
    };
    return answer.rows
      .filter(Array.isArray)
      .map((row) => ({
        id: pick(row, "transactionId"),
        user: pick(row, "username"),
        protocol: pick(row, "protocol"),
        client: pick(row, "clientAddress"),
        query: pick(row, "currentQuery"),
        status: pick(row, "status"),
        explicit: pick(row, "isExplicit") === true,
        elapsedMs: pick(row, "elapsedTimeMillis"),
      }))
      .filter((t) => t.query !== SHOW_QUERY)
      .sort((a, b) => (b.elapsedMs || 0) - (a.elapsedMs || 0));
  }

  // Milliseconds in the largest sensible unit: "850 ms", "12.3 s",
  // "4 min 05 s", "2 h 03 min"; a dash when unknown.
  function formatElapsed(ms) {
    if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0) return "—";
    if (ms < 1000) return `${Math.round(ms)} ms`;
    const s = ms / 1000;
    if (s < 60) return `${s.toFixed(1)} s`;
    const pad = (n) => String(n).padStart(2, "0");
    const whole = Math.floor(s);
    if (whole < 3600) return `${Math.floor(whole / 60)} min ${pad(whole % 60)} s`;
    return `${Math.floor(whole / 3600)} h ${pad(Math.floor((whole % 3600) / 60))} min`;
  }

  // The `/cypher` body that terminates `id`; the id travels as a parameter.
  function terminateRequest(id) {
    return { query: "TERMINATE TRANSACTION $id", params: { id } };
  }

  // What a row shows for its statement: the query on one line, clipped to
  // `max` characters, or what an empty one means.
  function queryLabel(t, max) {
    const limit = typeof max === "number" && max > 1 ? max : 160;
    if (typeof t.query !== "string" || t.query === "") {
      return t.explicit ? "(open transaction, between statements)" : "(no statement)";
    }
    const text = t.query.replace(/\s+/g, " ").trim();
    return text.length > limit ? text.slice(0, limit - 1) + "…" : text;
  }

  return { SHOW_QUERY, rowsFrom, formatElapsed, terminateRequest, queryLabel };
});
