// Unit tests for the database-access helpers in db_access.js.
// Run with `node --test static/web/*.test.js` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  READ_ONLY,
  isReadOnly,
  optionLabel,
  canCreateDatabases,
  writeBlockedReason,
} = require("./db_access.js");

const info = {
  databases: ["drevo", "erp"],
  default: "drevo",
  access: { drevo: "read-write", erp: "read-only" },
  read_only_server: false,
};
const readOnlyServer = { ...info, access: { drevo: "read-only", erp: "read-only" }, read_only_server: true };

test("read-only is the access SHOW DATABASES reports", () => {
  assert.equal(READ_ONLY, "read-only");
});

test("isReadOnly follows each database's access", () => {
  assert.equal(isReadOnly(info, "erp"), true);
  assert.equal(isReadOnly(info, "drevo"), false);
  assert.equal(isReadOnly(info, "unknown"), false);
});

test("every database of a read-only server is read-only", () => {
  assert.equal(isReadOnly({ ...readOnlyServer, access: {} }, "drevo"), true);
});

test("a server that does not report access is read-write", () => {
  assert.equal(isReadOnly({ databases: ["drevo"], default: "drevo" }, "drevo"), false);
  assert.equal(isReadOnly(null, "drevo"), false);
  assert.equal(isReadOnly({ access: "nope" }, "drevo"), false);
});

test("optionLabel marks read-only databases", () => {
  assert.equal(optionLabel(info, "erp"), "erp (read-only)");
  assert.equal(optionLabel(info, "drevo"), "drevo");
});

test("databases cannot be created on a read-only server", () => {
  assert.equal(canCreateDatabases(info), true);
  assert.equal(canCreateDatabases(readOnlyServer), false);
  assert.equal(canCreateDatabases(undefined), true);
});

test("writeBlockedReason says why writes are unavailable", () => {
  assert.equal(writeBlockedReason(info, "drevo"), null);
  assert.equal(writeBlockedReason(info, "erp"), "Database “erp” is read-only.");
  assert.equal(writeBlockedReason(readOnlyServer, "drevo"), "The server is read-only.");
});
