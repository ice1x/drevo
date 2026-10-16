// drevo browser — pure helpers for signing in: which screen to show after
// GET /auth/me, the message for a refused sign-in, and the request for a
// password change. DOM-free so they can be unit-tested under `node --test`
// (see auth.test.js).
//
// UMD: attaches to the global `DrevoAuth` in the browser (loaded before
// app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    root.DrevoAuth = api;
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // The screen for a GET /auth/me reply: "sign-in" (401), "change-password"
  // (signed in, but the password must be changed first) or "app". A server
  // that needs no login — or predates /auth/me — goes straight to the app.
  //
  //   authView(200, {user: "ann", passwordChangeRequired: false})
  //     → {view: "app", user: "ann"}
  function authView(status, body) {
    if (status === 401) return { view: "sign-in", user: null };
    const user = (status === 200 && body && body.user) || null;
    if (user && body.passwordChangeRequired) return { view: "change-password", user };
    return { view: "app", user };
  }

  // The message under the sign-in form for a refused POST /auth/login.
  function signInError(status, body) {
    const said = body && typeof body.error === "string" ? body.error : "";
    if (status === 401) return "Wrong user name or password.";
    if (status === 429) return `Too many failed attempts: ${said}`;
    if (status === 400) return "Enter a user name and a password.";
    return said ? `Sign-in failed (${status}): ${said}` : `Sign-in failed (${status}).`;
  }

  // The POST /cypher body that changes the signed-in user's password. Both
  // passwords travel as parameters, so quotes in them need no escaping and
  // they never appear in the query text.
  function changePasswordRequest(current, next) {
    return {
      query: "ALTER CURRENT USER SET PASSWORD FROM $current TO $new",
      params: { current, new: next },
    };
  }

  return { authView, signInError, changePasswordRequest };
});
