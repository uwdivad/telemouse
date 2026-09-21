// The time-driven half of the page (crates/viz/src/app.js): the OBS overlay's
// "no feed" indicator, the dashboard's connection pill, drawing while an OBS
// source is hidden, and the canvas palette read from the design tokens.
//
//   node --test crates/viz/js-tests/feed.test.mjs
//
// The engine tests call functions; these drive `frame()`, because the bug they
// pin down was exactly that: `feedState`, `updateStale` and `ui.refreshConn`
// were all correct and nothing in the frame loop ever called them, so the
// overlay never said "no feed" and the pill never left "waiting for capture".

import test from "node:test";
import assert from "node:assert/strict";

import {
  ANCHOR_UTC_US, batchEnvelope, ev, heartbeatEnvelope, loadApp, sessionEnvelope,
} from "./harness.mjs";

/** Load the page on a fake clock and return a way to let time pass: `run(s)`
    advances both clocks and runs the frame loop at 50 fps the whole way. */
function boot(opts) {
  const clock = { utcMs: ANCHOR_UTC_US / 1000, monotonicMs: 50 };
  const app = loadApp({ clock, ...opts });
  const run = (seconds) => {
    const until = clock.monotonicMs + seconds * 1000;
    while (clock.monotonicMs < until) {
      clock.monotonicMs += 20;
      clock.utcMs += 20;
      app.sandbox.frame(clock.monotonicMs);
    }
  };
  let seq = 0;
  const batch = () => app.telemouse.ui.ws.onmessage({
    data: JSON.stringify(batchEnvelope(seq++, [ev(clock.monotonicMs / 1000, 1, 0)])),
  });
  const beat = () => app.telemouse.ui.ws.onmessage({ data: JSON.stringify(heartbeatEnvelope()) });
  const body = app.sandbox.document.body;
  return {
    ...app, clock, run, batch, beat,
    stale: () => body.classList.contains("stale"),
    idle: () => body.classList.contains("idle"),
  };
}

test("the overlay says no feed after stale_secs of silence and recovers on the next batch", () => {
  const t = boot({ search: "?hud=status,speed", config: { obs_route: true, obs: { stale_secs: 2 } } });
  const { engine, ui } = t.telemouse;
  assert.equal(t.telemouse.obs.stale, 2);

  // First load: the socket is not even open yet. That is not "no feed".
  t.run(0.5);
  assert.equal(t.stale(), false, "no flash of 'no feed' while the page connects");
  ui.ws.onopen();
  ui.ws.onmessage({ data: JSON.stringify(sessionEnvelope()) });
  t.run(1);
  assert.equal(t.stale(), false, "still inside the grace period");

  // Nothing ever arrives: the capture agent is not running.
  t.run(1);
  assert.equal(t.stale(), true, "2 s without a batch");
  assert.match(t.elements.get("feedBadge").textContent, /^no feed · 2s$/);
  assert.equal(t.sandbox.hudValue("status", engine), "no feed (2s)");
  t.run(3);
  assert.match(t.elements.get("feedBadge").textContent, /^no feed · 5s$/, "the badge counts");

  // Data returns: cleared by the batch itself, not by the next repaint.
  t.batch();
  assert.equal(t.stale(), false, "instant recovery");
  assert.equal(t.sandbox.hudValue("status", engine), "live");

  // A feed that was live and stops goes stale again, measured from its last batch.
  for (let i = 0; i < 10; i++) { t.run(0.5); t.batch(); }
  assert.equal(t.stale(), false, "a steady feed is never stale");
  t.run(1.9);
  assert.equal(t.stale(), false);
  t.run(0.3);
  assert.equal(t.stale(), true);

  // A reconnect is not data: a bridge that comes back with no agent behind it
  // must not clear the indicator for another grace period.
  ui.ws.onclose();
  ui.connect();
  ui.ws.onopen();
  t.run(0.5);
  assert.equal(t.stale(), true, "a fresh socket with nothing on it is still no feed");
  t.batch();
  assert.equal(t.stale(), false);
});

test("a still hand under a live agent reads idle, not no feed", () => {
  const t = boot({ search: "?hud=status,speed", config: { obs_route: true, obs: { stale_secs: 2 } } });
  const { engine, ui } = t.telemouse;
  ui.ws.onopen();
  ui.ws.onmessage({ data: JSON.stringify(sessionEnvelope()) });

  // A second of movement, then the hand stops but the agent keeps saying hello.
  for (let i = 0; i < 10; i++) { t.batch(); t.run(0.1); }
  assert.equal(t.stale(), false);
  assert.equal(t.idle(), false);
  for (let i = 0; i < 3; i++) { t.beat(); t.run(1); }

  assert.equal(t.idle(), true, "the agent is there; the hand is not");
  assert.equal(t.stale(), false, "idle never dims the panels");
  assert.match(t.elements.get("feedBadge").textContent, /^idle · \ds$/);
  assert.match(t.sandbox.hudValue("status", engine), /^idle \(\ds\)$/);

  // Now the agent dies. Three missed heartbeats later it is no feed after all.
  t.run(3.1);
  assert.equal(t.idle(), false);
  assert.equal(t.stale(), true, "a dead agent still dims");
  assert.match(t.elements.get("feedBadge").textContent, /^no feed · \d+s$/);

  // It comes back with the hand still: idle again, instantly, not live.
  t.beat();
  assert.equal(t.stale(), false, "instant recovery, exactly like a batch");
  assert.equal(t.idle(), true);
  assert.match(t.sandbox.hudValue("status", engine), /^idle \(\d+s\)$/);

  // Only data is live.
  t.batch();
  assert.equal(t.idle(), false);
  assert.equal(t.sandbox.hudValue("status", engine), "live");
});

test("a heartbeat is liveness only: it never touches the timeline or the numbers", () => {
  const t = boot();
  const { engine, ui } = t.telemouse;
  ui.ws.onopen();
  ui.ws.onmessage({ data: JSON.stringify(sessionEnvelope()) });
  for (let i = 0; i < 4; i++) { t.batch(); t.run(0.05); }

  const snap = () => ({
    events: engine.ev.n, cursor: engine.cursor, tEnd: engine.tEnd,
    newestEventT: engine.newestEventT, batchSpan: engine.batchSpan,
    lastSeq: engine.lastSeq, lostBatches: engine.lostBatches,
    drops: engine.drops, absFrames: engine.absFrames,
    dropMarks: engine.dropMarks.length, metas: engine.metas.length,
    latency: engine.arrivalLatencyMs, latencyAt: engine.latencyReceivedAt,
    bad: ui.badFrames, lastBatchAt: ui.lastBatchAt, lastDataAt: ui.lastDataAt,
  });
  const before = snap();
  assert.ok(before.events > 0 && before.lastSeq !== null, "there is something to disturb");

  for (let i = 0; i < 20; i++) t.beat();
  assert.deepEqual(snap(), before, "a heartbeat is not a batch of zero events");
  assert.ok(ui.lastHeartbeatAt > 0, "but it did register as a sign of life");
  assert.equal(ui.heartbeatAge(), 0);

  // A malformed one is a bad frame like any other, not a crash.
  ui.ws.onmessage({ data: '{"type":"heartbeat"' });
  assert.equal(ui.badFrames, before.bad + 1);
});

test("the dashboard pill says idle when the agent is up and the hand is still", () => {
  const t = boot({ config: { udp_addr: "127.0.0.1:7878" } });
  const pill = () => t.elements.get("connPill").className + " | " + t.elements.get("connText").textContent;
  t.telemouse.ui.ws.onopen();
  assert.equal(pill(), "pill warn | waiting for capture on udp 127.0.0.1:7878");

  // The agent is running; it has simply never seen the hand move. That is not
  // "waiting for capture" any more.
  for (let i = 0; i < 3; i++) { t.beat(); t.run(1); }
  assert.match(pill(), /^pill ok \| idle · \ds$/);

  t.batch();
  t.run(0.2);
  assert.equal(pill(), "pill ok | live");

  // Movement stops, heartbeats continue: idle, counting from the last batch.
  for (let i = 0; i < 5; i++) { t.beat(); t.run(1); }
  assert.equal(pill(), "pill ok | idle · 5s");

  // Heartbeats stop too: back to the old verdict.
  t.run(3.5);
  assert.equal(pill(), "pill warn | no data for 9s");
});

test("overlayFeed is those two clocks and nothing else", () => {
  const { overlayFeed } = loadApp().telemouse;
  assert.equal(overlayFeed(0, Infinity, 3), "live");
  assert.equal(overlayFeed(3, Infinity, 3), "live", "the grace period is inclusive");
  assert.equal(overlayFeed(3.1, Infinity, 3), "down", "no heartbeat ever: the old verdict");
  assert.equal(overlayFeed(3.1, 0.5, 3), "idle");
  assert.equal(overlayFeed(3.1, 3, 3), "idle", "two missed heartbeats is still alive");
  assert.equal(overlayFeed(3.1, 3.01, 3), "down", "three, and the agent is gone");
  assert.equal(overlayFeed(600, 0.2, 0), "live", "stale=0 turns the indicator off entirely");
  assert.equal(overlayFeed(600, Infinity, 0), "live");
});

test("?stale= overrides [viz.obs] stale_secs, and 0 turns the indicator off", () => {
  const url = boot({ search: "?stale=5", config: { obs_route: true, obs: { stale_secs: 2 } } });
  assert.equal(url.telemouse.obs.stale, 5);
  url.run(4);
  assert.equal(url.stale(), false, "the config's 2 s does not apply");
  url.run(1.5);
  assert.equal(url.stale(), true);

  const off = boot({ search: "?obs=1&stale=0", config: { obs: { stale_secs: 2 } } });
  off.run(90);
  assert.equal(off.stale(), false, "0 = never");

  const dflt = boot({ search: "?obs=1" });
  assert.equal(dflt.telemouse.obs.stale, 3, "built-in default");
  dflt.run(3.2);
  assert.equal(dflt.stale(), true);
});

test("the dashboard never gets the overlay's stale class", () => {
  const t = boot();
  t.telemouse.ui.ws.onopen();
  t.run(10);
  assert.equal(t.stale(), false);
});

test("the dashboard pill follows the feed: waiting, live, no data, live again", () => {
  const t = boot({ config: { udp_addr: "127.0.0.1:7878" } });
  const pill = () => t.elements.get("connPill").className + " | " + t.elements.get("connText").textContent;
  assert.equal(pill(), "pill warn | connecting");
  t.telemouse.ui.ws.onopen();
  assert.equal(pill(), "pill warn | waiting for capture on udp 127.0.0.1:7878");
  t.run(5);
  assert.equal(pill(), "pill warn | waiting for capture on udp 127.0.0.1:7878");

  t.batch();
  t.run(0.2);
  assert.equal(pill(), "pill ok | live");
  t.run(4);
  assert.equal(pill(), "pill warn | no data for 4s");
  t.batch();
  t.run(0.2);
  assert.equal(pill(), "pill ok | live");

  // The socket's own states win while it is down.
  t.telemouse.ui.ws.onclose();
  const down = pill();
  t.run(1);
  assert.equal(pill(), down, "the frame loop does not overwrite 'retry in …'");
  assert.match(down, /^pill err \| retry in /);
});

test("a hidden OBS source keeps ticking and stops drawing", () => {
  const t = boot({ search: "?obs=1", globals: { obsstudio: {} } });
  const { engine, ui } = t.telemouse;
  let draws = 0;
  const draw = engine.draw;
  engine.draw = function () { draws++; return draw.call(this); };
  ui.ws.onopen();
  ui.ws.onmessage({ data: JSON.stringify(sessionEnvelope()) });

  const feed = (seconds) => { for (let i = 0; i < seconds * 10; i++) { t.batch(); t.run(0.1); } };
  feed(1);
  assert.ok(draws > 0, "visible: draws");

  t.fire("obsSourceVisibleChanged", { visible: false });
  draws = 0;
  const before = engine.cursor;
  feed(1);
  assert.equal(draws, 0, "hidden: no canvas work");
  assert.ok(engine.cursor > before, "hidden: the timeline still advances");

  t.fire("obsSourceVisibleChanged", { visible: true });
  t.run(0.1);
  assert.ok(draws > 0, "back on screen: repaints");

  t.fire("obsSourceActiveChanged", { active: false });
  draws = 0;
  feed(0.5);
  assert.equal(draws, 0, "inactive scene: no canvas work either");
});

test("the canvas palette comes from the design tokens, and a light canvas swaps the ramp", () => {
  const { telemouse } = loadApp();
  const { palette, paletteFromTokens, buildRamp, rampCss } = telemouse;
  const dark = { ...palette, ring: [...palette.ring] };
  const darkRamp = [...rampCss];
  assert.equal(palette.light, false);
  assert.equal(darkRamp.length, 14);
  assert.equal(darkRamp[13], "rgb(255,250,245)", "the dark ramp ends in hot white");

  // No stylesheet (or tokens that are not plain hex): nothing changes.
  paletteFromTokens(() => "");
  paletteFromTokens((name) => (name === "--canvas" ? "color-mix(in srgb, red, blue)" : "var(--nope)"));
  assert.deepEqual({ ...palette, ring: [...palette.ring] }, dark);

  const light = {
    "--canvas": " #ffffff", "--grid": "#edf1f6", "--grid-axis": "#c9d2df", "--grid-wrap": "#d6c3ea",
    "--head": "#16202c", "--accent": "#0b8f9c", "--line": "#dde3ec", "--surface": "#fff",
    "--line-strong": "#c9d2df", "--fg-2": "#4e5d70", "--fg-3": "#7b8899",
    "--lmb": "#178f4f", "--rmb": "#4a5ad6", "--aux": "#b26a00",
  };
  paletteFromTokens((name) => light[name] || "");
  buildRamp();
  assert.equal(palette.canvas, "#ffffff", "values are trimmed");
  assert.equal(palette.head, "#16202c");
  assert.equal(palette.headHalo, "rgba(22,32,44,0.10)");
  assert.equal(palette.cross, "rgba(11,143,156,0.55)");
  assert.deepEqual([...palette.ring], ["23,143,79", "74,90,214", "178,106,0"]);
  assert.equal(palette.ghostFill, "#fff");
  assert.equal(palette.light, true);
  assert.equal(rampCss.length, 14, "rebuilt in place");
  assert.notDeepEqual([...rampCss], darkRamp);
  // Every stop of the light ramp has to read on white: none may be near it.
  for (const css of rampCss) {
    const [r, g, b] = css.match(/\d+/g).map(Number);
    assert.ok(0.2126 * r + 0.7152 * g + 0.0722 * b < 170, css + " is too light for a white canvas");
  }
});

test("the overlay keeps its built-in palette whatever the page's theme", () => {
  const seeThrough = loadApp({ search: "?obs=1" }).telemouse.palette;
  assert.equal(seeThrough.grid, "rgba(255,255,255,0.07)", "alpha grid over footage");
  assert.equal(seeThrough.head, "#ffffff");
  const solid = loadApp({ search: "?obs=1&bg=0e131c" }).telemouse.palette;
  assert.equal(solid.grid, "#161e2b");
  assert.equal(solid.light, false);
});
