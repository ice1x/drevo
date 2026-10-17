// Unit tests for the physical-backup helpers in backups_view.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  LIST_PATH,
  BACKUP_PATH,
  listFrom,
  backupRequest,
  kindOf,
  bytesOf,
  summary,
} = require("./backups_view.js");

function backup(name, createdAt, databases, files) {
  return {
    name,
    format: 1,
    created_at_ms: createdAt,
    version: "0.0.0",
    default_database: "drevo",
    databases,
    files: files || [],
  };
}

const full = (name, wal) => ({ name, kind: "full", wal_bytes: wal, change_head: 1 });
const incr = (name, wal) => ({ name, kind: "incremental", wal_bytes: wal, change_head: 2 });

// ── paths ────────────────────────────────────────────────────────────
test("the panel talks to the admin backup routes", () => {
  assert.equal(LIST_PATH, "/admin/backups");
  assert.equal(BACKUP_PATH, "/admin/backup");
});

// ── listFrom ─────────────────────────────────────────────────────────
test("listFrom sorts backups newest first", () => {
  const answer = {
    directory: "/data/backups",
    backups: [backup("old", 10, []), backup("new", 30, []), backup("mid", 20, [])],
  };
  assert.deepEqual(
    listFrom(answer).map((b) => b.name),
    ["new", "mid", "old"]
  );
});

test("listFrom does not reorder the answer it was given", () => {
  const answer = { backups: [backup("a", 1, []), backup("b", 2, [])] };
  listFrom(answer);
  assert.deepEqual(
    answer.backups.map((b) => b.name),
    ["a", "b"]
  );
});

test("listFrom is empty for a missing or malformed answer", () => {
  assert.deepEqual(listFrom(null), []);
  assert.deepEqual(listFrom({}), []);
  assert.deepEqual(listFrom({ backups: "nope" }), []);
});

test("listFrom drops entries without a name", () => {
  const answer = { backups: [backup("kept", 1, []), { created_at_ms: 2 }, null] };
  assert.deepEqual(
    listFrom(answer).map((b) => b.name),
    ["kept"]
  );
});

// ── backupRequest ────────────────────────────────────────────────────
test("a full backup sends an empty body", () => {
  assert.deepEqual(backupRequest("full", [backup("a", 1, [])]), {});
  assert.deepEqual(backupRequest("full", []), {});
});

test("an incremental backup continues the newest backup", () => {
  const backups = [backup("sunday", 100, []), backup("monday", 200, []), backup("friday", 50, [])];
  assert.deepEqual(backupRequest("incremental", backups), { since: "monday" });
});

test("an incremental backup with nothing to continue is null", () => {
  assert.equal(backupRequest("incremental", []), null);
  assert.equal(backupRequest("incremental", undefined), null);
});

// ── kindOf ───────────────────────────────────────────────────────────
test("kindOf reports full, incremental or mixed", () => {
  assert.equal(kindOf(backup("a", 1, [full("drevo", 1), full("crm", 1)])), "full");
  assert.equal(kindOf(backup("b", 1, [incr("drevo", 1), incr("crm", 1)])), "incremental");
  assert.equal(kindOf(backup("c", 1, [incr("drevo", 1), full("crm", 1)])), "mixed");
  assert.equal(kindOf(backup("d", 1, [])), "full");
});

// ── bytesOf ──────────────────────────────────────────────────────────
test("bytesOf adds every database's log and the server files", () => {
  const b = backup(
    "a",
    1,
    [full("drevo", 1000), incr("crm", 24)],
    [{ name: "security.json", bytes: 300, sha256: "00" }]
  );
  assert.equal(bytesOf(b), 1324);
});

test("bytesOf ignores missing sizes", () => {
  assert.equal(bytesOf({ databases: [{ name: "x" }], files: [{ name: "y" }] }), 0);
  assert.equal(bytesOf({}), 0);
});

// ── summary ──────────────────────────────────────────────────────────
test("summary describes one backup", () => {
  const s = summary(backup("monday", 1_700_000_000_000, [full("drevo", 10), full("crm", 5)]));
  assert.equal(s.name, "monday");
  assert.equal(s.when.getTime(), 1_700_000_000_000);
  assert.equal(s.kind, "full");
  assert.equal(s.databases, 2);
  assert.equal(s.bytes, 15);
  assert.deepEqual(s.notes, []);
});

test("summary says why a database was backed up whole", () => {
  const crm = { ...full("crm", 5), full_because: "the log was compacted since the previous backup" };
  const s = summary(backup("tuesday", 1, [incr("drevo", 3), crm]));
  assert.equal(s.kind, "mixed");
  assert.deepEqual(s.notes, ["crm: the log was compacted since the previous backup"]);
});

test("summary has no date without a timestamp", () => {
  assert.equal(summary({ name: "x" }).when, null);
});
