// Unit tests for the running-transactions helpers in transactions_view.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  SHOW_QUERY,
  rowsFrom,
  formatElapsed,
  terminateRequest,
  queryLabel,
} = require("./transactions_view.js");

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

function response(rows) {
  return { columns: COLUMNS, rows };
}

// ── SHOW_QUERY ───────────────────────────────────────────────────────

test("the panel asks for every column it shows", () => {
  assert.match(SHOW_QUERY, /^SHOW TRANSACTIONS YIELD /);
  for (const c of COLUMNS) {
    assert.ok(SHOW_QUERY.includes(c), `${c} missing from ${SHOW_QUERY}`);
  }
});

// ── rowsFrom ─────────────────────────────────────────────────────────

test("rows become objects named by column, longest running first", () => {
  const rows = rowsFrom(
    response([
      ["drevo-transaction-7", "ann", "bolt", "10.0.0.7:5123", "MATCH (n) RETURN n", "Running", false, 1200],
      ["drevo-transaction-3", null, "http", null, null, "Idle", true, 95000],
    ]),
  );
  assert.equal(rows.length, 2);
  assert.deepEqual(rows[0], {
    id: "drevo-transaction-3",
    user: null,
    protocol: "http",
    client: null,
    query: null,
    status: "Idle",
    explicit: true,
    elapsedMs: 95000,
  });
  assert.equal(rows[1].id, "drevo-transaction-7");
  assert.equal(rows[1].user, "ann");
});

test("the panel's own SHOW is left out", () => {
  const rows = rowsFrom(
    response([
      ["drevo-transaction-9", null, "http", null, SHOW_QUERY, "Running", false, 1],
      ["drevo-transaction-8", null, "bolt", null, "RETURN 1", "Running", false, 2],
    ]),
  );
  assert.deepEqual(
    rows.map((r) => r.id),
    ["drevo-transaction-8"],
  );
});

test("columns are found by name, whatever their order", () => {
  const rows = rowsFrom({
    columns: ["elapsedTimeMillis", "transactionId"],
    rows: [[5, "erp-transaction-1"]],
  });
  assert.equal(rows[0].id, "erp-transaction-1");
  assert.equal(rows[0].elapsedMs, 5);
  assert.equal(rows[0].user, null);
});

test("an empty or malformed answer gives no rows", () => {
  assert.deepEqual(rowsFrom(null), []);
  assert.deepEqual(rowsFrom({}), []);
  assert.deepEqual(rowsFrom({ columns: COLUMNS }), []);
});

// ── formatElapsed ────────────────────────────────────────────────────

test("elapsed time reads in the largest sensible unit", () => {
  assert.equal(formatElapsed(0), "0 ms");
  assert.equal(formatElapsed(850), "850 ms");
  assert.equal(formatElapsed(12345), "12.3 s");
  assert.equal(formatElapsed(245000), "4 min 05 s");
  assert.equal(formatElapsed(7380000), "2 h 03 min");
});

test("a missing or bad elapsed time reads as a dash", () => {
  assert.equal(formatElapsed(null), "—");
  assert.equal(formatElapsed(undefined), "—");
  assert.equal(formatElapsed(-5), "—");
  assert.equal(formatElapsed("12"), "—");
});

// ── terminateRequest ─────────────────────────────────────────────────

test("terminating sends the id as a parameter, never spliced into the query", () => {
  const id = "drevo-transaction-1' OR 1=1 //";
  assert.deepEqual(terminateRequest(id), {
    query: "TERMINATE TRANSACTION $id",
    params: { id },
  });
});

// ── queryLabel ───────────────────────────────────────────────────────

test("an idle explicit transaction says so; a long query is clipped", () => {
  assert.equal(
    queryLabel({ query: null, explicit: true }),
    "(open transaction, between statements)",
  );
  assert.equal(queryLabel({ query: null, explicit: false }), "(no statement)");
  assert.equal(queryLabel({ query: "RETURN 1" }), "RETURN 1");
  const long = "MATCH (n) WHERE n.x = 1 ".repeat(20);
  const label = queryLabel({ query: long }, 40);
  assert.equal(label.length, 40);
  assert.ok(label.endsWith("…"));
  assert.equal(queryLabel({ query: "MATCH  (n)\n  RETURN n" }), "MATCH (n) RETURN n");
});
