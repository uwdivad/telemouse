// The page's replay plumbing and the small UI contracts around it
// (crates/viz/src/app.js), run with
//
//   node --test crates/viz/js-tests/replay.test.mjs
//
// The recording list and a streamed recording come from the harness's fake
// `fetch`; toasts run on its fake timer queue. Also here: the `?session=` id
// rule (core's `is_safe_id`, case for case), the sliders' limits coming from
// the script, and the toggle buttons' `aria-pressed`.

import test from "node:test";
import assert from "node:assert/strict";

import {
  batchEnvelope, ev, jsonResponse, loadApp, markupValues, sessionEnvelope, settle, textResponse,
} from "./harness.mjs";

const LISTING = [
  { id: "s-1", path: "recordings/s-1.jsonl", bytes: 2048, modified_epoch_ms: 1_756_000_100_000,
    started_utc_us: 1_756_000_000_000_000, ended_utc_us: 1_756_000_060_000_000, sidecar: null },
  { id: "s-0", path: "recordings/s-0.jsonl", bytes: 1024, modified_epoch_ms: 1_755_000_000_000,
    started_utc_us: null, ended_utc_us: null, sidecar: null },
];

/** A three-line recording: the session, one batch of two events, a marker. */
function recording(extraLines = []) {
  return [
    JSON.stringify(sessionEnvelope({ session_id: "s-1" })),
    JSON.stringify(batchEnvelope(0, [ev(0.1, 3, 0), ev(0.2, 4, 0)], { session_id: "s-1" })),
    JSON.stringify({ type: "marker", session_id: "s-1", seq_no: 1, ts_qpc: ev(0.15, 0, 0).ts_qpc, ts_utc_us: 0, label: "go" }),
    ...extraLines,
  ].join("\n") + "\n";
}

/** A page whose server lists LISTING and serves `body` for every recording. */
function replayPage(body, opts = {}) {
  return loadApp({
    fetch: (url) => (url === "/api/sessions" ? jsonResponse(LISTING) : textResponse(body, { chunkSize: 37 })),
    ...opts,
  });
}

test("replay lists the recordings and streams the newest one in", async () => {
  const app = replayPage(recording());
  const { engine, ui } = app.telemouse;
  ui.setMode("replay");
  await settle();

  assert.deepEqual(app.fetched, ["/api/sessions", "/api/session/s-1"]);
  const sel = app.elements.get("sessionSel");
  assert.equal(sel.children.length, 2, "one option per recording");
  assert.equal(sel.children[0].value, "s-1");
  assert.match(sel.children[0].textContent, /^s-1 {2}\(.*, 1:00, 2 KB\)$/, "start, span and size");
  assert.match(sel.children[1].textContent, /^s-0 {2}\(.*1 KB\)$/, "no span without timestamps");

  assert.equal(ui.loadedSession, "s-1");
  assert.equal(engine.loading, false);
  assert.equal(engine.sessionId, "s-1");
  assert.equal(engine.ev.n, 2, "a line split across reads still parses");
  assert.equal(engine.markers.length, 1);
  assert.equal(engine.markers[0].label, "go");
  assert.equal(app.elements.get("connText").textContent, "s-1 · 2 events");
  assert.equal(ui.replayPlaying, true, "a loaded recording starts playing");
});

test("a recording list the server will not give is said out loud, not shown as empty", async () => {
  const app = loadApp({ fetch: () => jsonResponse({ error: "boom" }, { status: 500 }) });
  app.telemouse.ui.setMode("replay");
  await settle();
  assert.deepEqual(app.fetched, ["/api/sessions"], "nothing is loaded after a failed list");
  assert.match(app.elements.get("sessionSel").innerHTML, /could not list recordings/);
  assert.equal(app.elements.get("connText").textContent, "recording list failed");
  assert.match(app.elements.get("connPill").className, /\berr\b/);

  // An empty list is a different, quieter answer.
  const empty = loadApp();
  empty.telemouse.ui.setMode("replay");
  await settle();
  assert.match(empty.elements.get("sessionSel").innerHTML, /no recordings found/);
  assert.equal(empty.elements.get("connText").textContent, "no recordings");
});

test("a bad replay line is counted as a bad frame and skipped", async () => {
  const app = replayPage(recording(["{\"type\":\"batch\",", "not json at all"]));
  const { engine, ui } = app.telemouse;
  ui.setMode("replay");
  await settle();
  assert.equal(ui.badFrames, 2, "the stats bar's bad-frames tile sees replay lines too");
  assert.equal(engine.ev.n, 2, "the good lines are all there");
  assert.equal(app.elements.get("connText").textContent, "s-1 · 2 events (2 bad lines)");
});

test("an engine error during a load surfaces instead of passing as a bad line", async () => {
  const app = replayPage(recording());
  const { engine, ui } = app.telemouse;
  const real = engine.ingest;
  engine.ingest = function (env, ...rest) {
    if (env.type === "marker") throw new Error("engine bug");
    return real.call(this, env, ...rest);
  };
  await assert.rejects(ui.loadSession("s-1"), /engine bug/);
  assert.equal(ui.badFrames, 0, "valid JSON is never a bad frame");
  assert.equal(engine.loading, false, "the render loop gets the engine back");
  assert.equal(app.elements.get("connText").textContent, "load interrupted");
});

test("a toast appears, fades, and is removed by its own timers", () => {
  const app = loadApp();
  const toasts = app.sandbox.document.getElementById("toasts");
  app.telemouse.ui.toast("marker", "flick <b>", false);
  assert.equal(toasts.children.length, 1);
  assert.match(toasts.children[0].innerHTML, /flick &lt;b&gt;/, "escaped");
  assert.ok(app.pendingTimers() > 0);
  app.flushTimers();
  assert.equal(toasts.children.length, 0, "gone after the fade");

  // The overlay never shows one.
  const obs = loadApp({ search: "?obs=1" });
  obs.telemouse.ui.toast("data loss", "x", true);
  assert.equal(obs.sandbox.document.getElementById("toasts").children.length, 0);
});

test("?session= accepts exactly what telemouse-core's is_safe_id accepts", () => {
  const { isSafeId } = loadApp().telemouse._test;
  // The cases of core's `safe_ids_are_a_single_path_component_everywhere`.
  for (const ok of ["s-20260823-153000-0a1f", "demo-session", "abc_123", "x", "-lead", "_lead"]) {
    assert.equal(isSafeId(ok), true, ok);
  }
  for (const bad of ["", "a b", "a/b", "a\\b", "a.b", "..", ".hidden", "C:x", "x:stream",
                     "\\\\server\\share", "s-1\n", "ünïcode", null, undefined]) {
    assert.equal(isSafeId(bad), false, String(bad));
  }
  assert.equal(isSafeId("x".repeat(128)), true);
  assert.equal(isSafeId("x".repeat(129)), false);

  // And the page acts on it: a good id opens replay, a bad one stays live.
  assert.equal(loadApp({ search: "?session=demo-session" }).telemouse.ui.mode, "replay");
  assert.equal(loadApp({ search: "?session=a.b" }).telemouse.ui.mode, "live");
});

test("the sliders take their limits from the script, and the OBS trail from the overlay", () => {
  const { telemouse: { _test: { limits }, engine }, elements } = loadApp();
  const buf = elements.get("buf"), decay = elements.get("decay");
  assert.deepEqual([buf.min, buf.max, buf.step, buf.value],
    [String(limits.BUFFER_MS.min), String(limits.BUFFER_MS.max), String(limits.BUFFER_MS.step), "35"]);
  assert.deepEqual([decay.min, decay.max, decay.value], ["0.3", "12", "3"]);
  assert.equal(engine.decay, 3);
  // The markup's own copy agrees, for the moment before the script runs.
  const markup = markupValues();
  assert.equal(markup.get("buf"), String(limits.BUFFER_MS.dflt));
  assert.equal(markup.get("decay"), String(limits.TRAIL_SECS.dflt));
  assert.equal(markup.get("speedSel"), "1", "the selected option");

  const obs = loadApp({ search: "?obs=1&trail=5.5" });
  assert.equal(obs.elements.get("decay").value, "5.5");
  assert.equal(obs.telemouse.engine.decay, 5.5);
});

test("the mode and view switches keep aria-pressed in step with what is shown", () => {
  const app = loadApp();
  const { ui, _test } = app.telemouse;
  const button = (attr, value) => {
    const b = app.sandbox.document.createElement("button");
    b.dataset[attr] = value;
    return b;
  };
  const modes = app.elements.get("modeSwitch");
  modes.appendChild(button("mode", "live"));
  modes.appendChild(button("mode", "replay"));
  const views = app.elements.get("viewSwitch");
  for (const v of ["both", "desk", "pointer"]) views.appendChild(button("view", v));
  const pressed = (box) => box.children.map((b) => b.getAttribute("aria-pressed"));

  ui.setMode("replay");
  assert.deepEqual(pressed(modes), ["false", "true"]);
  assert.ok(modes.children[1].classList.contains("on"));
  _test.setView("pointer");
  assert.deepEqual(pressed(views), ["false", "false", "true"]);
  _test.setView("both");
  assert.deepEqual(pressed(views), ["true", "false", "false"]);
});
