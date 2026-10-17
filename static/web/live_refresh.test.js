// Unit tests for the live-refresh helpers in live_refresh.js.
// Run with `node --test static/web/` (the `web-assets` CI job); no DOM, no deps.
"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  CHANGE_EVENTS,
  STORAGE_KEY,
  eventsUrl,
  changeAction,
  createCoalescer,
  readLivePref,
  writeLivePref,
} = require("./live_refresh.js");

// A hand-driven clock: timers fire only when the test says so.
function fakeTimers() {
  let next = 1;
  const pending = new Map();
  return {
    setTimer(fn, ms) {
      const id = next++;
      pending.set(id, { fn, ms });
      return id;
    },
    clearTimer(id) {
      pending.delete(id);
    },
    count() {
      return pending.size;
    },
    delays() {
      return [...pending.values()].map((t) => t.ms);
    },
    fire() {
      const due = [...pending.entries()];
      pending.clear();
      for (const [, t] of due) t.fn();
    },
  };
}

// Settle queued promise callbacks.
const flush = () => new Promise((resolve) => setImmediate(resolve));

// ── eventsUrl ────────────────────────────────────────────────────────

test("the stream URL names the database being viewed", () => {
  assert.equal(eventsUrl(null), "/events");
  assert.equal(eventsUrl(""), "/events");
  assert.equal(eventsUrl("erp"), "/events?db=erp");
  assert.equal(eventsUrl("bug tracker"), "/events?db=bug%20tracker");
});

// ── CHANGE_EVENTS / changeAction ─────────────────────────────────────

test("changes and lagged both mean the graph changed", () => {
  assert.deepEqual([...CHANGE_EVENTS].sort(), ["lagged", "node", "relationship"]);
});

test("a change reloads the overview, or marks a query result as stale", () => {
  assert.equal(changeAction(true), "refresh");
  assert.equal(changeAction(false), "mark-stale");
});

// ── createCoalescer ──────────────────────────────────────────────────

test("a burst of changes runs one refresh after the delay", async () => {
  const timers = fakeTimers();
  let runs = 0;
  const c = createCoalescer({ delayMs: 800, run: async () => runs++, ...timers });
  c.poke();
  c.poke();
  c.poke();
  assert.equal(timers.count(), 1);
  assert.deepEqual(timers.delays(), [800]);
  assert.equal(runs, 0);
  timers.fire();
  await flush();
  assert.equal(runs, 1);
  assert.equal(c.pending(), false);
});

test("a change during a refresh runs one more once it ends", async () => {
  const timers = fakeTimers();
  let release;
  let runs = 0;
  const c = createCoalescer({
    delayMs: 100,
    run: () => {
      runs++;
      return new Promise((resolve) => (release = resolve));
    },
    ...timers,
  });
  c.poke();
  timers.fire();
  assert.equal(runs, 1);
  c.poke();
  c.poke();
  // Nothing new starts while the first refresh is running.
  assert.equal(timers.count(), 0);
  release();
  await flush();
  assert.equal(timers.count(), 1);
  timers.fire();
  assert.equal(runs, 2);
  release();
  await flush();
  assert.equal(timers.count(), 0);
});

test("a failed refresh does not stop later ones", async () => {
  const timers = fakeTimers();
  let runs = 0;
  const c = createCoalescer({
    delayMs: 10,
    run: async () => {
      runs++;
      throw new Error("network down");
    },
    ...timers,
  });
  c.poke();
  timers.fire();
  await flush();
  c.poke();
  timers.fire();
  await flush();
  assert.equal(runs, 2);
});

test("cancel drops a scheduled refresh", () => {
  const timers = fakeTimers();
  let runs = 0;
  const c = createCoalescer({ delayMs: 10, run: async () => runs++, ...timers });
  c.poke();
  assert.equal(c.pending(), true);
  c.cancel();
  assert.equal(timers.count(), 0);
  assert.equal(c.pending(), false);
  assert.equal(runs, 0);
});

// ── readLivePref / writeLivePref ─────────────────────────────────────

function memoryStorage() {
  const data = new Map();
  return {
    getItem: (k) => (data.has(k) ? data.get(k) : null),
    setItem: (k, v) => data.set(k, String(v)),
    data,
  };
}

test("the live switch is remembered, off by default", () => {
  const storage = memoryStorage();
  assert.equal(readLivePref(storage), false);
  writeLivePref(storage, true);
  assert.equal(storage.data.get(STORAGE_KEY), "on");
  assert.equal(readLivePref(storage), true);
  writeLivePref(storage, false);
  assert.equal(readLivePref(storage), false);
});

test("storage that is missing or refuses access reads as off", () => {
  const refusing = {
    getItem() {
      throw new Error("SecurityError");
    },
    setItem() {
      throw new Error("QuotaExceededError");
    },
  };
  assert.equal(readLivePref(null), false);
  assert.equal(readLivePref(undefined), false);
  assert.equal(readLivePref(refusing), false);
  assert.doesNotThrow(() => writeLivePref(refusing, true));
  assert.doesNotThrow(() => writeLivePref(null, true));
});
