// drevo browser — pure helpers for database access in the top bar: whether
// the active database is read-only, how each one is labelled in the picker,
// and which write actions stay available. DOM-free so they can be
// unit-tested under `node --test` (see db_access.test.js).
//
// UMD: attaches to the global `DrevoDbAccess` in the browser (loaded before
// app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.DrevoDbAccess = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  const READ_ONLY = "read-only";

  // Whether `name` is read-only in a GET /databases answer
  // `{databases, default, access: {name: "read-only" | "read-write"}, read_only_server}`.
  // A server that does not report access is read-write.
  function isReadOnly(info, name) {
    if (info && info.read_only_server === true) return true;
    const access = info && info.access && typeof info.access === "object" ? info.access : {};
    return access[name] === READ_ONLY;
  }

  // The picker's label for `name`: "erp" or "erp (read-only)".
  function optionLabel(info, name) {
    return isReadOnly(info, name) ? `${name} (${READ_ONLY})` : name;
  }

  // Whether new databases can be created: not on a read-only server.
  function canCreateDatabases(info) {
    return !(info && info.read_only_server === true);
  }

  // Why a write action is unavailable on `name`, or null when it is.
  function writeBlockedReason(info, name) {
    if (info && info.read_only_server === true) return "The server is read-only.";
    if (isReadOnly(info, name)) return `Database “${name}” is read-only.`;
    return null;
  }

  return { READ_ONLY, isReadOnly, optionLabel, canCreateDatabases, writeBlockedReason };
});
