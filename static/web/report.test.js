// Unit tests for the problem-report helpers in report.js (#552).
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  toastFromProblem,
  reproTestCase,
  buildIssue,
  issueUrl,
  rustRawString,
  errorMessage,
  MAX_ISSUE_URL,
} = require("./report.js");

const timeout = {
  seq: 7,
  at: "2026-10-01T10:00:00.000Z",
  level: "ERROR",
  target: "drevo::query",
  message: "statement exceeded the statement timeout",
  fields: { limit_ms: "30000", database: "drevo", protocol: "http", query: "MATCH (n) RETURN n" },
};

test("a statement timeout becomes an error toast naming the limit and query", () => {
  const t = toastFromProblem(timeout);
  assert.equal(t.kind, "error");
  assert.match(t.title, /timed out/i);
  assert.match(t.title, /30000 ms/);
  assert.equal(t.detail, "MATCH (n) RETURN n");
});

test("a generic warning becomes a warn toast with its target", () => {
  const t = toastFromProblem({
    seq: 1, at: "x", level: "WARN", target: "drevo::server", message: "bolt accept failed", fields: {},
  });
  assert.equal(t.kind, "warn");
  assert.equal(t.title, "bolt accept failed");
  assert.equal(t.detail, "drevo::server");
});

test("rustRawString picks enough hashes for any query", () => {
  assert.equal(rustRawString("RETURN 1"), 'r#"RETURN 1"#');
  assert.equal(rustRawString('RETURN "a"#'), 'r##"RETURN "a"#"##');
});

test("the repro test case runs the failing query against a NativeService", () => {
  const text = reproTestCase({
    query: "MATCH (n:Entity) RETURN n",
    params: { k: 1 },
    error: "statement exceeded the 30000 ms statement timeout",
    stats: { nodes: 10, edges: 20 },
  });
  assert.match(text, /```cypher\nMATCH \(n:Entity\) RETURN n\n```/);
  assert.match(text, /NativeService::in_memory\(\)/);
  assert.match(text, /parse\(r#"MATCH \(n:Entity\) RETURN n"#\)/);
  assert.match(text, /"k"/, "params are carried into the test");
  assert.match(text, /10 nodes \/ 20 edges/);
  assert.match(text, /statement exceeded the 30000 ms statement timeout/);
});

test("without a failing query the test case says so instead of inventing one", () => {
  assert.match(reproTestCase({}), /no failing query was captured/i);
});

test("buildIssue assembles the sections and a short title", () => {
  const { title, body } = buildIssue({
    note: "The graph froze after I searched.",
    report: {
      server: { version: "0.0.34", git_sha: "abc123", build_date: "2026-10-01", engine: "native-durable", uptime_seconds: 5 },
      config: { statement_timeout_ms: 30000 },
      graph: { database: "drevo", nodes: 10, edges: 20 },
      problems: [timeout],
    },
    lastFailure: { query: "MATCH (n) RETURN n", params: {}, error: "boom" },
    uiErrors: [{ at: "2026-10-01T10:00:01.000Z", text: "GET /x → 500" }],
    userAgent: "TestAgent/1.0",
  });
  assert.ok(title.length <= 80, title);
  assert.match(title, /timed out|froze/i);
  for (const heading of [
    "## What happened",
    "## Environment",
    "## Failing query",
    "## Recent server problems",
    "## Recent UI errors",
    "## Repro test case",
    "## Screenshot",
  ]) {
    assert.ok(body.includes(heading), `missing ${heading}`);
  }
  assert.match(body, /0\.0\.34/);
  assert.match(body, /abc123/);
  assert.match(body, /TestAgent\/1\.0/);
  assert.match(body, /The graph froze after I searched\./);
  assert.match(body, /GET \/x → 500/);
});

test("issueUrl prefills title and body and stays under the URL budget", () => {
  const url = issueUrl("ice1x/drevo", "T", "B b");
  assert.equal(url, "https://github.com/ice1x/drevo/issues/new?title=T&body=B%20b");

  const huge = "x".repeat(50000);
  const long = issueUrl("ice1x/drevo", "T", huge);
  assert.ok(long.length <= MAX_ISSUE_URL, String(long.length));
  assert.match(decodeURIComponent(long.split("body=")[1]), /truncated/i);
});

test("issueUrl rejects a malformed repo instead of opening an arbitrary URL", () => {
  assert.throws(() => issueUrl("https://evil.example/x", "T", "B"));
  assert.throws(() => issueUrl("a/b/c", "T", "B"));
});

test("errorMessage unwraps the server's JSON error body", () => {
  assert.equal(
    errorMessage('{"error":"Cypher execution error: boom","status":400}'),
    "Cypher execution error: boom",
  );
  assert.equal(errorMessage("plain text"), "plain text");
  assert.equal(errorMessage('{"other":1}'), '{"other":1}');
  assert.equal(errorMessage(""), "");
});

test("a timed-out query gets a test that bounds its running time", () => {
  const text = reproTestCase({
    query: "MATCH (n) RETURN n",
    params: {},
    error: "Cypher execution error: statement exceeded the 200 ms statement timeout",
  });
  assert.match(text, /Instant::now\(\)/);
  assert.match(text, /Duration::from_millis\(200\)/);
  assert.match(text, /must finish within the reported 200 ms limit/);
  assert.doesNotMatch(text, /\n\n\n/, "no stray blank lines");
});
