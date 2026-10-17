// drevo browser — pure helpers for the physical backups in the Storage
// panel: the request that takes a full or an incremental backup, and each
// backup listed by GET /admin/backups as one line. DOM-free so they can be
// unit-tested under `node --test` (see backups_view.test.js).
//
// UMD: attaches to the global `DrevoBackupsView` in the browser (loaded
// before app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.DrevoBackupsView = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  const LIST_PATH = "/admin/backups";
  const BACKUP_PATH = "/admin/backup";

  // The backups of a GET /admin/backups answer, newest first.
  //
  //   listFrom({directory: "/data/backups", backups: [{name: "a", created_at_ms: 1}, …]})
  //     → [{name: "a", created_at_ms: 1, …}]
  function listFrom(answer) {
    const backups = answer && Array.isArray(answer.backups) ? answer.backups : [];
    return backups
      .filter((b) => b && typeof b.name === "string")
      .slice()
      .sort((a, b) => (b.created_at_ms || 0) - (a.created_at_ms || 0));
  }

  // The POST /admin/backup body: `{}` for a full backup, `{since: <newest>}`
  // for the log written after the newest backup. `null` when an incremental
  // backup has nothing to continue.
  function backupRequest(kind, backups) {
    if (kind !== "incremental") return {};
    const newest = listFrom({ backups })[0];
    return newest ? { since: newest.name } : null;
  }

  // "full" when every database was backed up whole, "incremental" when every
  // one continues the previous backup, "mixed" otherwise.
  function kindOf(backup) {
    const kinds = new Set(
      (backup.databases || []).map((d) => (d.kind === "incremental" ? "incremental" : "full"))
    );
    if (kinds.size === 1) return kinds.has("incremental") ? "incremental" : "full";
    return kinds.size === 0 ? "full" : "mixed";
  }

  // The bytes a backup holds: every database's log plus the server's files.
  function bytesOf(backup) {
    const sum = (items, field) =>
      (items || []).reduce((n, x) => n + (typeof x[field] === "number" ? x[field] : 0), 0);
    return sum(backup.databases, "wal_bytes") + sum(backup.files, "bytes");
  }

  // One backup as the panel shows it.
  //
  //   summary({name: "monday", created_at_ms: 0, databases: [{name: "drevo", kind: "full", wal_bytes: 10}]})
  //     → {name: "monday", when: <Date>, kind: "full", databases: 1, bytes: 10, notes: []}
  function summary(backup) {
    const databases = backup.databases || [];
    return {
      name: backup.name,
      when: typeof backup.created_at_ms === "number" ? new Date(backup.created_at_ms) : null,
      kind: kindOf(backup),
      databases: databases.length,
      bytes: bytesOf(backup),
      // Why a database was backed up whole when an incremental was asked for.
      notes: databases
        .filter((d) => typeof d.full_because === "string" && d.full_because !== "")
        .map((d) => `${d.name}: ${d.full_because}`),
    };
  }

  return { LIST_PATH, BACKUP_PATH, listFrom, backupRequest, kindOf, bytesOf, summary };
});
