// drevo browser — problem notifications and "Report a problem" (#552).
//
// Pure helpers, split out of app.js so they can be unit-tested under
// `node --test` (report.test.js) with no DOM: turning a server problem from
// GET /problems into a toast, and turning a problem report into a prefilled
// GitHub issue (title, Markdown body, repro test case, issue URL).
//
// UMD: attaches to the global `DrevoReport` in the browser (loaded before
// app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.DrevoReport = api;
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // Browsers and GitHub start rejecting URLs somewhere past 8 KB; stay below.
  const MAX_ISSUE_URL = 7800;
  const TRUNCATED_NOTE =
    "\n\n…(truncated to fit the issue URL — the full report is in the downloaded JSON file; please attach it)";

  // A server problem (GET /problems entry) → {kind, title, detail}.
  function toastFromProblem(p) {
    const fields = p.fields || {};
    const kind = p.level === "ERROR" ? "error" : "warn";
    if (fields.limit_ms !== undefined && /timeout/i.test(p.message || "")) {
      return {
        kind,
        title: `Query timed out after ${fields.limit_ms} ms`,
        detail: fields.query || "",
      };
    }
    return { kind, title: p.message || p.level, detail: fields.query || p.target || "" };
  }

  // The human message of a server error response: the `error` field of a
  // `{"error": …, "status": …}` JSON body, else the text as-is.
  function errorMessage(text) {
    try {
      const parsed = JSON.parse(text);
      if (parsed && typeof parsed.error === "string") return parsed.error;
    } catch (_) {
      // not JSON
    }
    return text;
  }

  // `s` as a Rust raw string literal with enough `#`s to contain it.
  function rustRawString(s) {
    let hashes = "#";
    while (s.includes(`"${hashes}`)) hashes += "#";
    return `r${hashes}"${s}"${hashes}`;
  }

  // A Rust literal for a JSON-ish parameter value, as a drevo executor Value.
  function rustValue(v) {
    if (v === null || v === undefined) return "Value::Null";
    if (typeof v === "boolean") return `Value::Bool(${v})`;
    if (typeof v === "number") {
      return Number.isInteger(v) ? `Value::Integer(${v})` : `Value::Float(${v})`;
    }
    if (typeof v === "string") return `Value::String(${rustRawString(v)}.to_string())`;
    if (Array.isArray(v)) return `Value::List(vec![${v.map(rustValue).join(", ")}])`;
    return `/* map parameter: ${JSON.stringify(v)} */ Value::Null`;
  }

  // Markdown: the failing query as Cypher steps plus a Rust integration-test
  // skeleton that runs it against an in-memory NativeService.
  function reproTestCase({ query, params, error, stats } = {}) {
    if (!query) {
      return "_No failing query was captured in this browser session — describe the steps above._";
    }
    const p = params || {};
    const hasParams = Object.keys(p).length > 0;
    const paramLines = Object.keys(p).map(
      (k) => `    params.insert(${JSON.stringify(k)}.to_string(), ${rustValue(p[k])});`,
    );
    const size = stats && stats.nodes !== undefined
      ? `Reported on a graph with ${stats.nodes} nodes / ${stats.edges} edges.`
      : "Graph size unknown.";
    // A statement timeout is a performance bug: the test bounds the running
    // time instead of only checking for success (an in-memory database has
    // no timeout configured, so the call would otherwise just run long).
    const timeout = /statement exceeded the (\d+) ms statement timeout/.exec(error || "");
    const check = timeout
      ? [
          "    let started = Instant::now();",
          "    let result = db.execute(&query, params);",
          `    assert!(result.is_ok(), "{result:?}");`,
          "    assert!(",
          `        started.elapsed() < Duration::from_millis(${timeout[1]}),`,
          `        "must finish within the reported ${timeout[1]} ms limit, took {:?}",`,
          "        started.elapsed()",
          "    );",
        ]
      : [
          "    let result = db.execute(&query, params);",
          `    assert!(result.is_ok(), "{result:?}");`,
        ];
    const lines = [
      "Run against the reporter's graph:",
      "",
      "```cypher",
      query,
      "```",
      "",
    ];
    if (hasParams) {
      lines.push("Parameters:", "", "```json", JSON.stringify(p, null, 2), "```", "");
    }
    lines.push(
      `Observed: \`${error || "(no error text)"}\``,
      "",
      `${size} Load a matching fixture (e.g. the reporter's GraphML export) before the query:`,
      "",
      "```rust",
      "use std::collections::HashMap;",
      timeout ? "use std::time::{Duration, Instant};" : null,
      "",
      "use drevo::cypher::executor::Value;",
      "use drevo::cypher::parser::parse;",
      "use drevo::native_service::NativeService;",
      "",
      "#[test]",
      "fn reported_problem() {",
      "    let db = NativeService::in_memory();",
      "    // TODO: load the fixture graph, e.g. db.import_graphml(&std::fs::read_to_string(\"fixture.graphml\").unwrap()).unwrap();",
      hasParams ? "    let mut params: HashMap<String, Value> = HashMap::new();" : "    let params: HashMap<String, Value> = HashMap::new();",
      ...paramLines,
      `    let query = parse(${rustRawString(query)}).expect("parse");`,
      `    // Reported: ${(error || "").replace(/\n/g, " ")}`,
      ...check,
      "}",
      "```",
    );
    return lines.filter((line) => line !== null).join("\n");
  }

  function codeBlock(text, lang) {
    const fence = text.includes("```") ? "~~~~" : "```";
    return `${fence}${lang || ""}\n${text}\n${fence}`;
  }

  // The issue title: the user's note if any, else the most recent server
  // error, else a generic one. At most 80 characters.
  function issueTitle(note, report) {
    const firstLine = (note || "").trim().split("\n")[0];
    let title = firstLine;
    if (!title) {
      const problems = (report && report.problems) || [];
      const lastError = [...problems].reverse().find((p) => p.level === "ERROR");
      title = lastError ? toastFromProblem(lastError).title : "Problem report from the Web UI";
    }
    title = `[report] ${title}`;
    return title.length > 80 ? title.slice(0, 79) + "…" : title;
  }

  // {title, body} of the GitHub issue for a problem report.
  function buildIssue({ note, report, lastFailure, uiErrors, userAgent } = {}) {
    const r = report || {};
    const server = r.server || {};
    const graph = r.graph || {};
    const config = r.config || {};
    const env = [
      `- drevo ${server.version || "?"} (git ${server.git_sha || "?"}, built ${server.build_date || "?"})`,
      `- engine: ${server.engine || "?"}, uptime ${server.uptime_seconds ?? "?"} s`,
      `- statement timeout: ${config.statement_timeout_ms ? config.statement_timeout_ms + " ms" : "off"}`,
      `- database: ${graph.database || "?"} — ${graph.nodes ?? "?"} nodes / ${graph.edges ?? "?"} edges`,
      `- browser: ${userAgent || "?"}`,
    ].join("\n");

    const failure = lastFailure && lastFailure.query
      ? codeBlock(lastFailure.query, "cypher") + `\n\nError: \`${lastFailure.error || ""}\``
      : "_None captured in this session._";

    const problems = (r.problems || []).slice(-15);
    const problemText = problems.length
      ? codeBlock(
          problems
            .map((p) => {
              const f = Object.entries(p.fields || {})
                .map(([k, v]) => `${k}=${v}`)
                .join(" ");
              return `${p.at} ${p.level} ${p.target}: ${p.message}${f ? "  " + f : ""}`;
            })
            .join("\n"),
          "text",
        )
      : "_None._";

    const ui = (uiErrors || []).slice(-15);
    const uiText = ui.length ? codeBlock(ui.map((e) => `${e.at} ${e.text}`).join("\n"), "text") : "_None._";

    const body = [
      "## What happened",
      (note || "").trim() || "_(no description given)_",
      "",
      "## Environment",
      env,
      "",
      "## Failing query",
      failure,
      "",
      "## Recent server problems",
      problemText,
      "",
      "## Recent UI errors",
      uiText,
      "",
      "## Repro test case",
      reproTestCase({
        query: lastFailure && lastFailure.query,
        params: lastFailure && lastFailure.params,
        error: lastFailure && lastFailure.error,
        stats: graph,
      }),
      "",
      "## Screenshot",
      "<!-- The screenshot was copied to your clipboard: paste it here (Cmd/Ctrl+V). -->",
      "",
      "_Generated by the drevo Web UI \"Report a problem\" button (#552)._",
    ].join("\n");

    return { title: issueTitle(note, r), body };
  }

  // https://github.com/<owner>/<repo>/issues/new with title and body
  // prefilled, the body cut (with a note) so the URL stays under
  // MAX_ISSUE_URL.
  function issueUrl(repo, title, body) {
    if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo || "")) {
      throw new Error(`not a GitHub owner/repo: ${repo}`);
    }
    const base = `https://github.com/${repo}/issues/new?title=${encodeURIComponent(title)}&body=`;
    let text = body;
    let url = base + encodeURIComponent(text);
    while (url.length > MAX_ISSUE_URL && text.length > 0) {
      const over = url.length - MAX_ISSUE_URL;
      // Encoded characters take up to 9 bytes; cut generously and retry.
      const keep = Math.max(0, text.length - Math.ceil(over / 3) - TRUNCATED_NOTE.length - 16);
      text = text.slice(0, keep);
      url = base + encodeURIComponent(text + TRUNCATED_NOTE);
    }
    return url;
  }

  return {
    MAX_ISSUE_URL,
    toastFromProblem,
    errorMessage,
    rustRawString,
    reproTestCase,
    buildIssue,
    issueUrl,
  };
});
