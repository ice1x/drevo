// drevo browser — pure helpers for the live overview: the GET /events URL
// for the database being viewed, what a change means for what is on screen,
// a coalescer that turns a burst of changes into one refresh, a panel that
// follows the stream while it is open, and the remembered on/off switch. DOM-free so they can be unit-tested under
// `node --test` (see live_refresh.test.js).
//
// UMD: attaches to the global `DrevoLiveRefresh` in the browser (loaded
// before app.js) and to `module.exports` under node's test runner.
(function (root, factory) {
  "use strict";
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.DrevoLiveRefresh = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // The /events event types that mean the graph changed. `lagged` means
  // changes were missed, so it reloads too.
  const CHANGE_EVENTS = Object.freeze(["node", "relationship", "lagged"]);

  // Where the switch is remembered, per browser.
  const STORAGE_KEY = "drevo.liveRefresh";

  // The change stream for database `db` (the default one when empty).
  //
  //   eventsUrl("erp") → "/events?db=erp"
  function eventsUrl(db) {
    return db ? `/events?db=${encodeURIComponent(db)}` : "/events";
  }

  // What a change does: reload the overview when it is on screen, or mark
  // it stale while a query result is shown, so the result stays put.
  function changeAction(overviewShown) {
    return overviewShown ? "refresh" : "mark-stale";
  }

  // Run `run` (which returns a promise) `delayMs` after the first `poke()`,
  // once for any number of pokes in between. A poke while it runs schedules
  // one more run after it ends; runs never overlap.
  //
  // `setTimer` / `clearTimer` default to setTimeout / clearTimeout.
  function createCoalescer({ delayMs, run, setTimer, clearTimer }) {
    const set = setTimer || ((fn, ms) => setTimeout(fn, ms));
    const clear = clearTimer || ((id) => clearTimeout(id));
    let timer = null;
    let running = false;
    let again = false;

    function start() {
      timer = null;
      running = true;
      let result;
      try {
        result = Promise.resolve(run());
      } catch (e) {
        result = Promise.reject(e);
      }
      result
        .catch(() => {})
        .then(() => {
          running = false;
          if (again) {
            again = false;
            poke();
          }
        });
    }

    function poke() {
      if (running) {
        again = true;
        return;
      }
      if (timer === null) timer = set(start, delayMs);
    }

    function cancel() {
      if (timer !== null) clear(timer);
      timer = null;
      again = false;
    }

    function pending() {
      return timer !== null || again;
    }

    return { poke, cancel, pending };
  }

  // A panel that follows the change stream while it is open: `open(db)`
  // starts the stream for `db` (closing an earlier one) and `close()` ends
  // it. A change pokes a coalescer whose `run` reloads the panel, so a
  // burst — an import, a long transaction — reloads once, `delayMs` after
  // the first change. `openStream(url)` defaults to `new EventSource(url)`.
  //
  // The Storage panel uses it (#777): its file size, bloat share and
  // keyspace counts follow the graph instead of needing a Refresh button.
  function createLivePanel({ delayMs, run, openStream, setTimer, clearTimer }) {
    const start = openStream || ((url) => new EventSource(url));
    const refresh = createCoalescer({ delayMs, run, setTimer, clearTimer });
    let stream = null;

    function open(db) {
      close();
      stream = start(eventsUrl(db));
      for (const type of CHANGE_EVENTS) {
        stream.addEventListener(type, () => refresh.poke());
      }
    }

    function close() {
      if (stream) stream.close();
      stream = null;
      refresh.cancel();
    }

    function following() {
      return stream !== null;
    }

    return { open, close, following };
  }

  // Whether the switch is on in `storage` (localStorage); off when nothing
  // is stored or storage is unavailable.
  function readLivePref(storage) {
    try {
      return !!storage && storage.getItem(STORAGE_KEY) === "on";
    } catch (e) {
      return false;
    }
  }

  // Remember the switch in `storage`; a refusing storage is ignored.
  function writeLivePref(storage, on) {
    try {
      if (storage) storage.setItem(STORAGE_KEY, on ? "on" : "off");
    } catch (e) {
      // Private mode or blocked storage: the switch just isn't remembered.
    }
  }

  return {
    CHANGE_EVENTS,
    STORAGE_KEY,
    eventsUrl,
    changeAction,
    createCoalescer,
    createLivePanel,
    readLivePref,
    writeLivePref,
  };
});
