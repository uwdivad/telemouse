// Unit tests for the page's engine (crates/viz/src/app.js), run with
//
//   node --test crates/viz/js-tests/engine.test.mjs
//
// and by `cargo test -p telemouse-viz` when Node is installed. They cover
// the arithmetic a browser session only checks by eye: unit conversion,
// unwrapped yaw, loss accounting, the live-buffer floor, the live-mode
// memory cap, checkpointed seeking, session restarts and OBS parameter
// clamping.

import test from "node:test";
import assert from "node:assert/strict";

import {
  ANCHOR_QPC,
  batchEnvelope,
  ev,
  loadApp,
  sessionEnvelope,
  syntheticStream,
} from "./harness.mjs";

const CM_PER_INCH = 2.54;

function fresh(opts) {
  const { telemouse } = loadApp(opts);
  return telemouse;
}

/** Integrate everything up to `t` from scratch, bypassing checkpoints. */
function integrateFromZero(engine, t) {
  engine.rewind();
  engine.playT = t;
  engine.consume(Infinity);
  return snapshot(engine);
}

function snapshot(engine) {
  return {
    cursor: engine.cursor,
    deskX: engine.deskX,
    deskY: engine.deskY,
    yawRaw: engine.yawRaw,
    pitch: engine.pitch,
    totalCm: engine.totalCm,
    totalDeg: engine.totalDeg,
    totalClicks: engine.totalClicks,
    held: engine.held,
  };
}

test("a session envelope sets the calibration and anchors the timeline", () => {
  const { engine } = fresh();
  assert.equal(engine.sessionId, null);
  engine.ingest(sessionEnvelope());
  assert.equal(engine.sessionId, "s-test");
  assert.equal(engine.cpi, 1600);
  assert.equal(engine.qpcFreq, 1e7);
  assert.equal(engine.anchorQpc, ANCHOR_QPC);
  assert.equal(engine.devices.length, 2);
  // An event half a second after the anchor lands at t = 0.5 s.
  engine.ingest(batchEnvelope(0, [ev(0.5, 1, 0)]));
  assert.equal(engine.ev.n, 1);
  assert.ok(Math.abs(engine.ev.t[0] - 0.5) < 1e-9);
  assert.ok(Math.abs(engine.tEnd - 0.5) < 1e-9);
});

test("motion integrates to centimetres and to unwrapped aim degrees", () => {
  const { engine, ui } = fresh();
  ui.mode = "replay";
  engine.ingest(sessionEnvelope());
  // Four inches to the right, one inch per batch, at sens 2 × 0.022 °/count.
  for (let i = 0; i < 4; i++) {
    engine.ingest(batchEnvelope(i, [ev(0.1 * (i + 1), 1600, 0)]));
  }
  engine.seek(1.0);
  assert.ok(Math.abs(engine.deskX - 4 * CM_PER_INCH) < 1e-9);
  assert.ok(Math.abs(engine.totalCm - 4 * CM_PER_INCH) < 1e-9);
  // 4 × 1600 × 2 × 0.022 = 281.6°: past ±180 and not wrapped.
  assert.ok(Math.abs(engine.yawRaw - 281.6) < 1e-9, `yaw ${engine.yawRaw}`);
  assert.ok(Math.abs(engine.totalDeg - 281.6) < 1e-9);
  assert.equal(engine.pitch, 0);
  assert.equal(engine.game, "cs2.exe");
  assert.equal(engine.sens.fallback, false);
});

test("pitch is clamped and a game without a profile falls back", () => {
  const { engine, ui } = fresh();
  ui.mode = "replay";
  engine.ingest(sessionEnvelope());
  engine.ingest(batchEnvelope(0, [ev(0.1, 0, 100_000)], { game: "unknown.exe" }));
  engine.seek(1.0);
  assert.equal(engine.pitch, 89, "pitch is clamped like an FPS engine");
  assert.equal(engine.sens.fallback, true);
  assert.equal(engine.sens.sens, 1.0);
});

test("seq gaps and ring drops are counted separately", () => {
  const { engine } = fresh();
  engine.ingest(sessionEnvelope());
  engine.ingest(batchEnvelope(0, [ev(0.1, 1, 0)]));
  engine.ingest(batchEnvelope(1, [ev(0.2, 1, 0)], { drops_since_last: 2 }));
  engine.ingest(batchEnvelope(5, [ev(0.3, 1, 0)])); // 2, 3, 4 never arrived
  engine.ingest(batchEnvelope(4, [ev(0.35, 1, 0)])); // late, not a new gap
  assert.equal(engine.lostBatches, 3);
  assert.equal(engine.drops, 2);
  assert.equal(engine.lastSeq, 5);
  assert.equal(engine.dropMarks.length, 2);
});

test("the live buffer never drops below one batch span plus 10 ms", () => {
  const { engine, ui } = fresh();
  engine.ingest(sessionEnvelope());
  ui.liveBuffer = 0.01;
  // A batch covering 25 ms of events.
  const events = [];
  for (let i = 0; i < 25; i++) events.push(ev(1 + i / 1000, 1, 0));
  engine.ingest(batchEnvelope(0, events));
  assert.ok(Math.abs(engine.batchSpan - 0.024) < 1e-9);
  assert.ok(Math.abs(engine.effectiveLiveBuffer() - 0.034) < 1e-9);
  ui.liveBuffer = 0.1;
  assert.equal(engine.effectiveLiveBuffer(), 0.1, "a larger setting wins");
  // The floor decays while the feed is quiet rather than pinning forever.
  engine.ingest(batchEnvelope(1, [ev(2, 1, 0)]));
  assert.ok(engine.batchSpan < 0.024);
});

test("live mode caps the timeline even when nothing consumes it", () => {
  const { engine, ui } = fresh();
  ui.mode = "live";
  engine.ingest(sessionEnvelope());
  // ~45 s of 1 kHz input into a hidden tab: nothing ticks, so only ingest
  // can keep memory bounded.
  for (const b of syntheticStream(45)) engine.ingest(b);
  assert.ok(engine.ev.n <= 40_000, `timeline held ${engine.ev.n} events`);
  assert.ok(engine.ev.n >= 20_000);
  // The metas kept are exactly those the remaining events point at.
  assert.equal(engine.metaAt(0) !== undefined, true);
  assert.equal(engine.metaAt(engine.ev.n - 1) !== undefined, true);
});

test("a backward seek restores a checkpoint and matches integrating from zero", () => {
  const { engine, ui } = fresh();
  ui.mode = "replay";
  engine.ingest(sessionEnvelope());
  for (const b of syntheticStream(35)) engine.ingest(b);
  engine.extendCheckpoints();
  assert.ok(engine.checkpoints.length >= 3, `checkpoints ${engine.checkpoints.length}`);
  engine.rewind();

  const target = 27.31;
  engine.seek(target);
  const viaSeek = snapshot(engine);
  assert.ok(viaSeek.totalClicks > 0, "the stream has clicks");
  assert.ok(viaSeek.cursor > 20_000);

  // Backward, then forward again: restore + replay, not a full pass.
  engine.seek(4.2);
  assert.ok(engine.cursor < viaSeek.cursor);
  engine.seek(target);
  assert.deepEqual(snapshot(engine), viaSeek);

  // The reference: every event from the first, no checkpoint involved.
  const reference = integrateFromZero(engine, target);
  assert.deepEqual(viaSeek, reference);
});

test("a marker is placed on the timeline and reported once when passed", () => {
  const { engine, ui } = fresh();
  ui.mode = "replay";
  const seen = [];
  ui.onMarker = (m) => seen.push(m.label);
  engine.ingest(sessionEnvelope());
  engine.ingest(batchEnvelope(0, [ev(0.1, 1, 0), ev(2.0, 1, 0)]));
  engine.ingest({ type: "marker", session_id: "s-test", seq_no: 0, ts_qpc: ev(1.0, 0, 0).ts_qpc, ts_utc_us: 0, label: "clutch" });
  assert.equal(engine.markers.length, 1);
  assert.ok(Math.abs(engine.markers[0].t - 1.0) < 1e-9);
  engine.seek(0.5);
  assert.deepEqual(seen, []);
  engine.playT = 1.5;
  engine.consume(Infinity);
  assert.deepEqual(seen, ["clutch"]);
  engine.playT = 1.9;
  engine.consume(Infinity);
  assert.deepEqual(seen, ["clutch"], "not reported twice");
});

test("a session with a new id tears the old timeline down first", () => {
  const { engine, ui } = fresh();
  const restarts = [];
  ui.onSessionRestart = (from, to) => restarts.push([from, to]);
  engine.ingest(sessionEnvelope({ session_id: "s-a" }));
  engine.ingest(batchEnvelope(0, [ev(0.1, 1, 0)], { session_id: "s-a" }));
  assert.equal(engine.ev.n, 1);
  engine.ingest(sessionEnvelope({ session_id: "s-b", mouse_cpi: 800 }));
  assert.equal(engine.sessionId, "s-b");
  assert.equal(engine.cpi, 800);
  assert.equal(engine.ev.n, 0, "old events are gone");
  assert.equal(engine.lastSeq, null);
  assert.deepEqual(restarts, [["s-a", "s-b"]]);
  // The same id again is a refresh, not a restart.
  engine.ingest(batchEnvelope(0, [ev(0.1, 1, 0)], { session_id: "s-b" }));
  engine.ingest(sessionEnvelope({ session_id: "s-b" }));
  assert.equal(engine.ev.n, 1);
  assert.equal(restarts.length, 1);
});

test("a batch that arrives before any session anchors itself", () => {
  const { engine } = fresh();
  engine.ingest(batchEnvelope(0, [ev(3.0, 1, 0), ev(3.001, 1, 0)]));
  assert.equal(engine.ev.n, 2);
  assert.equal(engine.ev.t[0], 0, "the first event becomes t = 0");
  assert.ok(Math.abs(engine.ev.t[1] - 0.001) < 1e-9);
});

test("OBS parameters are validated and clamped, with the server's defaults underneath", () => {
  const config = {
    obs_route: true,
    obs: {
      layout: "stack", background: "#0e131c80", hud: ["speed", "aim", "cpm"],
      hud_position: "top-right", scale: 1.5, trail_secs: 2.0, buffer_ms: 40,
      grid: true, legend: false, labels: false,
    },
  };
  // No query: the server's table applies.
  let { obs } = fresh({ config });
  assert.equal(obs.layout, "stack");
  assert.equal(obs.hudPos, "top-right");
  assert.equal(obs.scale, 1.5);
  assert.equal(obs.trail, 2.0);
  assert.equal(obs.buffer, 0.04);
  assert.ok(Math.abs(obs.bg.alpha - 0x80 / 255) < 1e-9);

  // Out-of-range and unknown values clamp or fall back; unknown HUD items drop.
  ({ obs } = fresh({
    config,
    search: "?layout=sideways&scale=9&trail=0.01&buffer=9999&hud=speed,wpm,cpm&bg=notacolour&hudpos=middle&grid=0",
  }));
  assert.equal(obs.layout, "split", "unknown layout falls back to the first");
  assert.equal(obs.scale, 4);
  assert.equal(obs.trail, 0.3);
  assert.equal(obs.buffer, 0.2);
  // Spread: the app's arrays come from the vm realm, whose Array prototype
  // is not this one's, and deepEqual compares prototypes.
  assert.deepEqual([...obs.hud], ["speed", "cpm"]);
  assert.equal(obs.bg.css, "transparent");
  assert.equal(obs.hudPos, "bottom-left");
  assert.equal(obs.grid, false);

  // The dashboard's `view=` spelling is accepted as the layout.
  ({ obs } = fresh({ config, search: "?view=aim" }));
  assert.equal(obs.layout, "aim");

  // Not the OBS route and no ?obs=1: no OBS object at all.
  ({ obs } = fresh({ config: { obs_route: false, obs: config.obs } }));
  assert.equal(obs, null);
});
