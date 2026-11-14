// A hidden dashboard tab lets its live socket go and takes it back when it
// is looked at again (crates/viz/src/app.js, `ui.pageVisibility`), and
// `?desync=0`.
//
//   node --test crates/viz/js-tests/hidden.test.mjs
//
// The tab this is for is the one left open behind a game: rAF is suspended
// there, but the socket was not, so every batch was parsed and appended for
// a picture nobody was painting.

import test from "node:test";
import assert from "node:assert/strict";

import {
  ANCHOR_UTC_US, batchEnvelope, ev, loadApp, sessionEnvelope,
} from "./harness.mjs";

function boot(opts) {
  const clock = { utcMs: ANCHOR_UTC_US / 1000, monotonicMs: 50 };
  const app = loadApp({ clock, visibility: "visible", ...opts });
  const el = (id) => app.elements.get(id);
  return {
    ...app, clock,
    ui: app.telemouse.ui,
    engine: app.telemouse.engine,
    pill: () => el("connPill").className + " | " + el("connText").textContent,
    send: (env) => app.telemouse.ui.ws.onmessage({ data: JSON.stringify(env) }),
    /** Let `seconds` pass on the frame loop (which is what repaints the pill). */
    run(seconds) {
      const until = clock.monotonicMs + seconds * 1000;
      while (clock.monotonicMs < until) {
        clock.monotonicMs += 20;
        clock.utcMs += 20;
        app.telemouse.frame(clock.monotonicMs);
      }
    },
    /** Toasts on screen (the box only exists once something has been toasted). */
    toasts: () => (el("toasts") ? el("toasts").children.length : 0),
  };
}

test("a dashboard hidden past the grace period closes its socket and reopens it on return", () => {
  const t = boot({ config: { udp_addr: "127.0.0.1:7878" } });
  const { ui, engine } = t;
  const first = ui.ws;
  first.onopen();
  t.send(sessionEnvelope());
  t.send(batchEnvelope(0, [ev(0.01, 5, 0, { buttons: 0x0001 })]));   // left button goes down
  t.run(0.2);
  assert.equal(t.pill(), "pill ok | live");
  assert.equal(engine.held, 1);

  // Hidden: nothing happens yet, only a timer.
  const pending = t.pendingTimers();
  t.setVisibility("hidden");
  assert.equal(ui.ws, first, "the socket survives the grace period");
  assert.equal(first.closed, false);
  assert.equal(t.pendingTimers(), pending + 1);

  // The grace period runs out.
  t.flushTimers();
  assert.equal(first.closed, true, "socket closed");
  assert.equal(ui.ws, null);
  assert.equal(ui.wsOpen, false);
  assert.equal(ui.parked, true);
  assert.equal(t.pill(), "pill | paused while hidden", "not an error state");
  assert.equal(t.pendingTimers(), 0, "and no reconnect is scheduled into the dark");

  // The browser reports the close afterwards; it is no longer our socket.
  first.onclose();
  assert.equal(t.pill(), "pill | paused while hidden", "a late onclose does not say 'disconnected'");
  assert.equal(t.pendingTimers(), 0, "nor start a retry");

  // Back: a new socket, at once, with no error on the way.
  t.setVisibility("visible");
  assert.notEqual(ui.ws, null);
  assert.notEqual(ui.ws, first);
  assert.equal(ui.parked, false);
  assert.equal(t.pill(), "pill warn | connecting");
  ui.ws.onopen();
  assert.equal(t.pill(), "pill warn | waiting for capture on udp 127.0.0.1:7878");

  // The bridge replays the session, then the feed carries on 400 batches later.
  const lostBefore = engine.lostBatches;
  const toastsBefore = t.toasts();
  t.send(sessionEnvelope());
  t.send(batchEnvelope(400, [ev(10.0, 5, 0)]));
  t.run(0.2);
  assert.equal(t.pill(), "pill ok | live");
  assert.equal(engine.lostBatches, lostBefore, "the batches not listened to are not counted as lost");
  assert.equal(engine.dropMarks.length, 0);
  assert.equal(t.toasts(), toastsBefore, "and nobody is told data was lost");
  assert.equal(engine.held, 0, "a button whose release went unheard is not drawn as held");

  // From here a real gap is loss again.
  t.send(batchEnvelope(403, [ev(10.1, 5, 0)]));
  assert.equal(engine.lostBatches, lostBefore + 2);
});

test("a glance at another tab does not reconnect", () => {
  const t = boot();
  const first = t.ui.ws;
  first.onopen();
  t.setVisibility("hidden");
  t.setVisibility("visible");
  t.flushTimers();
  assert.equal(t.ui.ws, first);
  assert.equal(first.closed, false);
  assert.equal(t.ui.parked, false);
  // Hidden twice in a row arms one timer, not two.
  t.setVisibility("hidden");
  t.setVisibility("hidden");
  assert.equal(t.pendingTimers(), 1);
});

test("the overlay never gives up its socket, in OBS or in a plain browser", () => {
  for (const opts of [
    { search: "?obs=1" },
    { config: { obs_route: true } },
    { search: "?obs=1", visibility: "hidden" },
  ]) {
    const t = boot(opts);
    const first = t.ui.ws;
    first.onopen();
    t.setVisibility("hidden");
    assert.equal(t.pendingTimers(), 0, "no timer is even armed");
    t.flushTimers();
    assert.equal(t.ui.ws, first);
    assert.equal(first.closed, false);
    assert.equal(t.ui.parked, false);
    t.setVisibility("visible");
    assert.equal(t.ui.ws, first, "and coming back changes nothing");
  }
  // A real OBS source reports visibility through obsstudio events instead.
  const t = boot({ search: "?obs=1", globals: { obsstudio: {} } });
  const first = t.ui.ws;
  t.fire("obsSourceVisibleChanged", { visible: false });
  t.fire("obsSourceActiveChanged", { active: false });
  t.flushTimers();
  assert.equal(t.ui.ws, first);
  assert.equal(first.closed, false);
});

test("replay is left alone while hidden, and going live afterwards connects normally", () => {
  const t = boot();
  const { ui } = t;
  ui.setMode("replay");
  assert.equal(ui.ws, null);
  t.setVisibility("hidden");
  t.flushTimers();
  assert.equal(ui.parked, false, "nothing to park");
  assert.match(t.pill(), /no recordings|replay/);
  t.setVisibility("visible");
  assert.equal(ui.ws, null, "coming back does not open a live socket under replay");

  // Parked in live, then switched to replay before coming back: stays replay.
  ui.setMode("live");
  ui.ws.onopen();
  t.setVisibility("hidden");
  t.flushTimers();
  assert.equal(ui.parked, true);
  ui.setMode("replay");
  t.setVisibility("visible");
  assert.equal(ui.ws, null);
  assert.equal(ui.parked, false);
  ui.setMode("live");
  assert.notEqual(ui.ws, null);
});

test("a socket that was already retrying when the tab was hidden stops, and returns without backoff", () => {
  const t = boot();
  const { ui } = t;
  const first = ui.ws;
  first.onopen();
  first.onclose();                       // the bridge went away
  assert.match(t.pill(), /^pill err \| retry in /);
  assert.equal(t.pendingTimers(), 1, "a retry is pending");
  ui.wsRetry = 6;                        // well into its backoff

  t.setVisibility("hidden");
  // Run only the hide timer's consequences: parking cancels the retry.
  ui.parkHidden();
  assert.equal(ui.parked, true);
  assert.equal(t.pill(), "pill | paused while hidden");
  t.flushTimers();
  assert.equal(ui.ws, null, "no retry fired while hidden");

  t.setVisibility("visible");
  assert.notEqual(ui.ws, null, "reconnects at once");
  assert.equal(ui.wsRetry, 0, "with the backoff reset");
  assert.equal(t.pill(), "pill warn | connecting");
  // If the bridge is still gone, the ordinary retry logic takes over again.
  ui.ws.onclose();
  assert.match(t.pill(), /^pill err \| retry in 0\.4s$/);
});

test("a tab that loads hidden parks after the grace period; one that is shown first does not", () => {
  const hidden = boot({ visibility: "hidden" });
  assert.notEqual(hidden.ui.ws, null, "it connects first, as always");
  hidden.flushTimers();
  assert.equal(hidden.ui.parked, true);
  assert.equal(hidden.ui.ws, null);

  // parkHidden re-checks: a timer that fires after the tab is back is a no-op.
  const t = boot();
  const first = t.ui.ws;
  t.ui.parkHidden();
  assert.equal(t.ui.parked, false);
  assert.equal(t.ui.ws, first);
});

test("?desync=0 turns the low-latency canvas hint off; anything else leaves it on", () => {
  /** What each of the three canvases asked `getContext` for. */
  const asked = (search) => {
    const { elements } = loadApp({ search });
    return ["deskCanvas", "aimCanvas", "ptrCanvas"].map((id) => elements.get(id).contextOptions.desynchronized);
  };
  assert.deepEqual(asked(""), [true, true, true], "default unchanged");
  assert.deepEqual(asked("?desync=0"), [false, false, false]);
  assert.deepEqual(asked("?desync=1"), [true, true, true]);
  assert.deepEqual(asked("?desync=off"), [true, true, true], "only 0 is the switch");
  assert.deepEqual(asked("?obs=1&desync=0"), [false, false, false], "the overlay takes it too");
  assert.equal(loadApp({ search: "?desync=0" }).telemouse._test.desync, false);
});
