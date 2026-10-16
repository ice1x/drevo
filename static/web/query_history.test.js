// Unit tests for the query-history helpers in query_history.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  pushQuery,
  stepHistory,
  parseHistory,
  isRunKey,
  fromTextRequest,
  generatedCypher,
  elapsedText,
  MAX_HISTORY,
} = require("./query_history.js");

// ── pushQuery ────────────────────────────────────────────────────────

test("pushQuery puts the newest query first", () => {
  assert.deepEqual(pushQuery(["a"], "b"), ["b", "a"]);
});

test("pushQuery moves a repeated query to the front instead of duplicating it", () => {
  assert.deepEqual(pushQuery(["b", "a", "c"], "a"), ["a", "b", "c"]);
});

test("pushQuery trims the query and ignores a blank one", () => {
  assert.deepEqual(pushQuery(["a"], "  b  "), ["b", "a"]);
  assert.deepEqual(pushQuery(["a"], "   "), ["a"]);
});

test("pushQuery keeps at most MAX_HISTORY entries", () => {
  let h = [];
  for (let i = 0; i < MAX_HISTORY + 10; i++) h = pushQuery(h, `q${i}`);
  assert.equal(h.length, MAX_HISTORY);
  assert.equal(h[0], `q${MAX_HISTORY + 9}`);
});

test("pushQuery does not modify the list it is given", () => {
  const h = ["a"];
  pushQuery(h, "b");
  assert.deepEqual(h, ["a"]);
});

// ── stepHistory ──────────────────────────────────────────────────────
// The cursor is -1 for "the text being typed", 0 for the newest entry.

test("stepping back from the draft shows the newest query", () => {
  assert.deepEqual(stepHistory(["new", "old"], -1, "back"), { index: 0, text: "new" });
});

test("stepping back again shows an older query and stops at the oldest", () => {
  assert.deepEqual(stepHistory(["new", "old"], 0, "back"), { index: 1, text: "old" });
  assert.deepEqual(stepHistory(["new", "old"], 1, "back"), { index: 1, text: "old" });
});

test("stepping forward past the newest returns to the draft", () => {
  assert.deepEqual(stepHistory(["new", "old"], 1, "forward"), { index: 0, text: "new" });
  assert.deepEqual(stepHistory(["new", "old"], 0, "forward"), { index: -1, text: null });
  assert.deepEqual(stepHistory(["new", "old"], -1, "forward"), { index: -1, text: null });
});

test("stepping in an empty history stays on the draft", () => {
  assert.deepEqual(stepHistory([], -1, "back"), { index: -1, text: null });
});

// ── parseHistory ─────────────────────────────────────────────────────

test("parseHistory reads a stored JSON list of strings", () => {
  assert.deepEqual(parseHistory('["a","b"]'), ["a", "b"]);
});

test("parseHistory treats missing or corrupt storage as empty", () => {
  assert.deepEqual(parseHistory(null), []);
  assert.deepEqual(parseHistory("not json"), []);
  assert.deepEqual(parseHistory('{"a":1}'), []);
});

test("parseHistory drops non-string entries and caps the length", () => {
  assert.deepEqual(parseHistory('["a",1,null,"b"]'), ["a", "b"]);
  const many = JSON.stringify(Array.from({ length: MAX_HISTORY + 5 }, (_, i) => `q${i}`));
  assert.equal(parseHistory(many).length, MAX_HISTORY);
});

// ── isRunKey ─────────────────────────────────────────────────────────

test("Ctrl+Enter and Cmd+Enter run the query", () => {
  assert.equal(isRunKey({ key: "Enter", ctrlKey: true }, "MATCH (n)\nRETURN n"), true);
  assert.equal(isRunKey({ key: "Enter", metaKey: true }, "MATCH (n)\nRETURN n"), true);
});

test("plain Enter runs a one-line query", () => {
  assert.equal(isRunKey({ key: "Enter" }, "MATCH (n) RETURN n"), true);
  assert.equal(isRunKey({ key: "Enter" }, "graph database"), true);
});

test("plain Enter in a multi-line query adds a line instead", () => {
  assert.equal(isRunKey({ key: "Enter" }, "MATCH (n)\nRETURN n"), false);
});

test("Shift+Enter always adds a line", () => {
  assert.equal(isRunKey({ key: "Enter", shiftKey: true }, "MATCH (n) RETURN n"), false);
});

test("other keys never run", () => {
  assert.equal(isRunKey({ key: "a", ctrlKey: true }, "x"), false);
});

test("Enter while an IME is composing does not run", () => {
  assert.equal(isRunKey({ key: "Enter", isComposing: true }, "x"), false);
});

// ── Ask: text-to-Cypher ──────────────────────────────────────────────

test("fromTextRequest passes the question as a parameter, not inside the query", () => {
  const q = "which tasks are 'done'?";
  assert.deepEqual(fromTextRequest(q), {
    query: "CALL drevo.cypher.fromText($question) YIELD cypher RETURN cypher",
    params: { question: q },
  });
});

test("fromTextRequest trims the question", () => {
  assert.equal(fromTextRequest("  list projects \n").params.question, "list projects");
});

test("generatedCypher returns the query from the single cypher row", () => {
  const resp = { columns: ["cypher"], rows: [["  MATCH (p:Project) RETURN p  "]] };
  assert.deepEqual(generatedCypher(resp), { cypher: "MATCH (p:Project) RETURN p", answered: true });
});

test("a reply that is only a // comment means the model could not answer", () => {
  const resp = { columns: ["cypher"], rows: [["// no such data in this schema"]] };
  assert.deepEqual(generatedCypher(resp), {
    cypher: "// no such data in this schema",
    answered: false,
  });
});

test("a reply with a leading comment and a query still counts as answered", () => {
  const resp = { columns: ["cypher"], rows: [["// tasks\nMATCH (t:Task) RETURN t"]] };
  assert.equal(generatedCypher(resp).answered, true);
});

test("an empty or malformed reply yields no query", () => {
  assert.deepEqual(generatedCypher({ rows: [] }), { cypher: "", answered: false });
  assert.deepEqual(generatedCypher({ rows: [[null]] }), { cypher: "", answered: false });
  assert.deepEqual(generatedCypher(null), { cypher: "", answered: false });
});

// ── elapsedText ──────────────────────────────────────────────────────

test("elapsedText shows tenths of a second under a minute", () => {
  assert.equal(elapsedText(0), "0.0s");
  assert.equal(elapsedText(420), "0.4s");
  assert.equal(elapsedText(12345), "12.3s");
  assert.equal(elapsedText(59949), "59.9s");
});

test("elapsedText shows minutes and two-digit seconds from a minute on", () => {
  assert.equal(elapsedText(60000), "1m 00s");
  assert.equal(elapsedText(65400), "1m 05s");
  assert.equal(elapsedText(3725000), "62m 05s");
});

test("elapsedText treats a negative or missing duration as zero", () => {
  assert.equal(elapsedText(-5), "0.0s");
  assert.equal(elapsedText(undefined), "0.0s");
});
