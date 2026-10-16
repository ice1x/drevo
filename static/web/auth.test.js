// Unit tests for the sign-in helpers in auth.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { authView, signInError, changePasswordRequest } = require("./auth.js");

// ── authView: what the UI shows after GET /auth/me ───────────────────

test("a 401 means sign in", () => {
  assert.deepEqual(authView(401, null), { view: "sign-in", user: null });
});

test("a signed-in user goes straight to the graph", () => {
  assert.deepEqual(authView(200, { user: "ann", passwordChangeRequired: false }), {
    view: "app",
    user: "ann",
  });
});

test("a user who must change the password sees the change form", () => {
  assert.deepEqual(authView(200, { user: "bob", passwordChangeRequired: true }), {
    view: "change-password",
    user: "bob",
  });
});

test("a server that needs no login shows the graph with no user", () => {
  assert.deepEqual(authView(200, { user: null, passwordChangeRequired: false }), {
    view: "app",
    user: null,
  });
});

test("a server without /auth/me (older build) shows the graph", () => {
  assert.deepEqual(authView(404, null), { view: "app", user: null });
});

// ── signInError: the message under the sign-in form ──────────────────

test("a refused login says so plainly", () => {
  assert.equal(
    signInError(401, { error: "The client is unauthorized due to authentication failure." }),
    "Wrong user name or password."
  );
});

test("a rate limit passes the server's wait time on", () => {
  assert.equal(
    signInError(429, { error: "too many failed logins; try again in 5 s" }),
    "Too many failed attempts: too many failed logins; try again in 5 s"
  );
});

test("a missing field asks for both", () => {
  assert.equal(signInError(400, null), "Enter a user name and a password.");
});

test("anything else shows the status and the server's message", () => {
  assert.equal(signInError(503, { error: "user management is not enabled" }), "Sign-in failed (503): user management is not enabled");
  assert.equal(signInError(500, null), "Sign-in failed (500).");
});

// ── changePasswordRequest: the Cypher for a password change ──────────

test("the password change travels as parameters, never inside the query", () => {
  const req = changePasswordRequest("old'pw", 'new"pw');
  assert.deepEqual(req, {
    query: "ALTER CURRENT USER SET PASSWORD FROM $current TO $new",
    params: { current: "old'pw", new: 'new"pw' },
  });
});
