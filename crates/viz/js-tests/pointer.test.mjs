// The Windows pointer model behind the "Pointer" panel (crates/viz/src/app.js),
// run with
//
//   node --test crates/viz/js-tests/pointer.test.mjs
//
// The numbers here are the ones in the model's header comment, worked from
// the default SmoothMouse curve by hand: what one count is worth at a crawl,
// at the curve's points, past its last point, and with precision off. Then
// the engine side: settings taken from the session envelope, the defaults
// (and the hint) when it has none, the pixel integration, and the views and
// OBS layouts that show the panel.

import test from "node:test";
import assert from "node:assert/strict";

import { batchEnvelope, ev, loadApp, sessionEnvelope } from "./harness.mjs";

const near = (got, want, tol, what) =>
  assert.ok(Math.abs(got - want) <= tol, `${what || ""} ${got} vs ${want}`);

/** The model and its helpers, off the page's devtools handle. */
function model(settings) {
  const { telemouse } = loadApp();
  return {
    m: new telemouse.PointerModel(telemouse.pointerSettingsFrom(settings)),
    telemouse,
  };
}

test("the slider tables match the control panel's eleven notches, interpolated between", () => {
  const { speedMultiplier } = loadApp().telemouse;
  assert.equal(speedMultiplier(10, false), 1);
  assert.equal(speedMultiplier(10, true), 1);
  assert.equal(speedMultiplier(6, false), 0.5);
  assert.equal(speedMultiplier(1, false), 1 / 32);
  assert.equal(speedMultiplier(20, false), 3.5);
  assert.equal(speedMultiplier(20, true), 2);
  assert.equal(speedMultiplier(2, true), 0.2);
  // Registry-set values between notches: halfway between the neighbours.
  near(speedMultiplier(11, true), 1.1, 1e-12);
  near(speedMultiplier(3, false), (1 / 16 + 1 / 4) / 2, 1e-12);
  // Out of range clamps rather than falling off the table.
  assert.equal(speedMultiplier(0, false), 1, "0 is not a speed; the default");
  assert.equal(speedMultiplier(99, true), 2);
  assert.equal(speedMultiplier(-5, true), 0.1);
});

test("with precision on, one count is worth 0.58 px at a crawl and 2.2 px at 12 in/s", () => {
  const { m } = model(null);
  const px = 96 / 150;
  // One count per report: 1/3.5 in/s, on the first segment.
  const slope0 = m.s.curve_y[1] / m.s.curve_x[1];
  near(m.gain(1), slope0 / 3.5 * px, 1e-12, "crawl");
  near(m.gain(1), 0.5826, 1e-3, "crawl ≈ 0.58 px/count");
  // The third inflection point, 1.25 in/s: 4.375 counts per report.
  near(m.gain(1.25 * 3.5), m.s.curve_y[2] * px / (1.25 * 3.5), 1e-12);
  near(m.gain(1.25 * 3.5), 0.7753, 1e-3, "third point ≈ 0.78 px/count");
  // 12 in/s sits on the last segment.
  near(m.gain(42), 2.236, 2e-3, "12 in/s ≈ 2.2 px/count");
  // Past the last point the last segment continues: gain keeps rising.
  const slopeLast = (568 - m.s.curve_y[3]) / (40 - m.s.curve_x[3]);
  near(m.curve(80), 568 + 40 * slopeLast, 1e-9, "linear beyond 40 in/s");
  assert.ok(m.gain(280) > m.gain(140) && m.gain(140) > m.gain(42));
  assert.equal(m.gain(0), 0);
  assert.equal(m.curve(0), 0);
});

test("the cursor moves whole pixels and the remainder carries, so a crawl steps", () => {
  const { m } = model(null);
  // 0.58 px per count: nothing on the first count, one pixel on the second.
  m.apply(1, 0);
  assert.deepEqual([m.outX, m.outY], [0, 0]);
  near(m.moveX, 0.5826, 1e-3, "the movement is still reported");
  m.apply(1, 0);
  assert.deepEqual([m.outX, m.outY], [1, 0]);
  near(m.rx, 0.1652, 1e-3, "remainder");
  m.apply(1, 0);
  assert.deepEqual([m.outX, m.outY], [0, 0]);
  m.apply(1, 0);
  assert.deepEqual([m.outX, m.outY], [1, 0]);
  // Sign is kept in the remainder: the truncation is towards zero.
  m.reset();
  m.apply(-1, 0);
  assert.deepEqual([m.outX, m.outY], [0, 0]);
  assert.ok(m.rx < 0);
  m.apply(-1, 0);
  assert.equal(m.outX, -1);
  // Direction is preserved: a diagonal report scales both axes alike.
  m.reset();
  m.apply(30, -40);
  near(m.moveX / m.moveY, 30 / -40, 1e-12, "direction");
  assert.ok(m.outX > 30 && m.outY < -40, "fast: past 1:1");
});

test("with precision off the pointer is the counts times the slider, remainder and all", () => {
  const { m } = model({ enhance_pointer_precision: false, speed: 6 });
  assert.equal(m.epp, false);
  assert.equal(m.mult, 0.5);
  assert.equal(m.gain(1), 0.5);
  assert.equal(m.gain(1000), 0.5, "no speed dependence");
  m.apply(3, -2);
  assert.deepEqual([m.outX, m.outY], [1, -1]);
  assert.deepEqual([m.rx, m.ry], [0.5, 0]);
  m.apply(1, 0);
  assert.deepEqual([m.outX, m.outY], [1, 0]);
  assert.deepEqual([m.rx, m.ry], [0, 0]);
  assert.equal(m.describe(), "precision off · 6/20 · 96 dpi");

  const one = model({ enhance_pointer_precision: false, speed: 10 }).m;
  one.apply(7, -9);
  assert.deepEqual([one.outX, one.outY], [7, -9], "the middle notch is 1:1");
});

test("the screen DPI and the slider scale the curve's output", () => {
  const base = model(null).m;
  const hidpi = model({ screen_dpi: 144 }).m;
  near(hidpi.gain(10), base.gain(10) * 1.5, 1e-12, "150% scaling: 1.5× the pixels");
  const fast = model({ speed: 20 }).m;
  near(fast.gain(10), base.gain(10) * 2, 1e-12, "slider at the top: 2× with precision on");
  assert.equal(fast.describe(), "precision on · 20/20 · 96 dpi");
});

test("settings default field by field, and say when they were assumed", () => {
  const { pointerSettingsFrom, WIN_POINTER_DEFAULTS } = loadApp().telemouse;
  const d = pointerSettingsFrom(null);
  assert.equal(d.assumed, true);
  assert.equal(d.enhance_pointer_precision, true);
  assert.equal(d.speed, 10);
  assert.equal(d.screen_dpi, 96);
  assert.deepEqual([...d.curve_x], [...WIN_POINTER_DEFAULTS.curve_x]);

  const p = pointerSettingsFrom({ speed: 14, enhance_pointer_precision: false });
  assert.equal(p.assumed, false);
  assert.equal(p.speed, 14);
  assert.equal(p.enhance_pointer_precision, false);
  assert.equal(p.screen_dpi, 96, "unset fields default");

  // Garbage in individual fields falls back on its own, not all at once.
  const g = pointerSettingsFrom({
    speed: 99, screen_dpi: "abc", curve_x: [1, 2, 3], curve_y: [0, 5, 4, 6, 7],
    enhance_pointer_precision: 0,
  });
  assert.equal(g.speed, 10);
  assert.equal(g.screen_dpi, 96);
  assert.deepEqual([...g.curve_x], [...WIN_POINTER_DEFAULTS.curve_x], "wrong length");
  assert.deepEqual([...g.curve_y], [...WIN_POINTER_DEFAULTS.curve_y], "not monotonic");
  assert.equal(g.enhance_pointer_precision, false, "0 is a falsy switch, not garbage");

  // A flat curve (the "no acceleration" registry fix) is a valid curve.
  const flat = pointerSettingsFrom({ curve_y: [0, 0, 0, 0, 0] });
  assert.deepEqual([...flat.curve_y], [0, 0, 0, 0, 0]);
});

test("the engine takes the settings from the session envelope, or assumes and says so", () => {
  const { telemouse: { engine, ui }, elements } = loadApp();
  ui.refreshSensHint();
  assert.match(elements.get("ptrHint").textContent, /no session envelope yet — assuming/);

  engine.ingest(sessionEnvelope());
  ui.refreshSensHint();
  assert.equal(engine.pointerSettings, null);
  assert.match(elements.get("ptrHint").textContent, /no pointer settings in this session — assuming a fresh Windows install/);
  assert.equal(engine.pointer.describe(), "precision on · 10/20 · 96 dpi");

  engine.ingest(sessionEnvelope({
    session_id: "s-2",
    pointer: { enhance_pointer_precision: false, speed: 8, screen_dpi: 120,
               curve_x: [0, 0.43, 1.25, 3.86, 40], curve_y: [0, 1.37, 5.3, 24.3, 568] },
  }));
  ui.refreshSensHint();
  assert.equal(elements.get("ptrHint").textContent, "");
  assert.equal(engine.pointer.describe(), "precision off · 8/20 · 120 dpi");
  assert.equal(engine.pointer.mult, 0.75);

  // A restart drops the settings with the rest of the session.
  engine.reset();
  assert.equal(engine.pointerSettings, null);
  assert.equal(engine.pointer.describe(), "precision on · 10/20 · 96 dpi");
});

test("the pointer integrates in whole pixels next to the desk and aim integrations", () => {
  const { engine, ui } = loadApp().telemouse;
  ui.setMode("replay");
  engine.ingest(sessionEnvelope({ pointer: { enhance_pointer_precision: false, speed: 10 } }));
  engine.ingest(batchEnvelope(0, [ev(0.1, 3, -2), ev(0.2, 1, 0, { buttons: 0x0001 })]));
  engine.seek(0.5);   // within a click ring's life of the click, so the ring exists
  assert.deepEqual([engine.ptrX, engine.ptrY], [4, -2]);
  near(engine.totalPx, Math.hypot(3, 2) + 1, 1e-12);
  assert.equal(engine.ptrTrail.n, 2);
  // The click ring knows where the cursor was.
  assert.equal(engine.rings.n, 1);
  assert.deepEqual([engine.rings.at(0).px, engine.rings.at(0).py], [4, -2]);
  // The desk integration is untouched by the pointer settings.
  near(engine.deskX, 4 / 1600 * 2.54, 1e-12);

  // Recenter zeroes the cursor and its remainder like the other two.
  engine.pointer.rx = 0.4;
  engine.resetIntegrators();
  assert.deepEqual([engine.ptrX, engine.ptrY, engine.pointer.rx, engine.ptrTrail.n], [0, 0, 0, 0]);
});

test("a crawl through the curve reaches the cursor as steps, the speed readout as motion", () => {
  const { engine, ui } = loadApp().telemouse;
  ui.setMode("replay");
  engine.ingest(sessionEnvelope());   // defaults: precision on
  const events = [];
  for (let i = 0; i < 10; i++) events.push(ev(0.1 + i * 0.01, 1, 0));
  engine.ingest(batchEnvelope(0, events));
  engine.seek(1.0);
  // Ten single counts at 0.58 px each: five pixels of cursor, 5.8 of motion.
  assert.equal(engine.ptrX, 5);
  near(engine.totalPx, 5.826, 1e-2);
  assert.equal(engine.ptrTrail.n, 5, "one trail point per pixel the cursor moved");
});

test("the dashboard's pointer and compare views are body classes the stylesheet keys on", () => {
  const { telemouse: { ptrPanel, _test: { setView } }, sandbox } = loadApp();
  const body = sandbox.document.body.classList;
  const views = () => body.list.filter((c) => c.startsWith("view-"));
  setView("pointer");
  assert.deepEqual(views(), ["view-pointer"]);
  setView("compare");
  assert.deepEqual(views(), ["view-compare"]);
  setView("desk");
  assert.deepEqual(views(), ["view-desk"]);
  setView("both");
  assert.deepEqual(views(), []);
  setView("cursor");
  assert.deepEqual(views(), [], "unknown → both");
  assert.equal(ptrPanel.space, "ptr");
  assert.equal(ptrPanel.unit, "px");
});

test("?view=pointer opens the dashboard on the pointer panel", () => {
  const { sandbox } = loadApp({ search: "?view=pointer" });
  assert.ok(sandbox.document.body.classList.contains("view-pointer"));
  const compare = loadApp({ search: "?view=compare" }).sandbox;
  assert.ok(compare.document.body.classList.contains("view-compare"));
});

test("the OBS overlay takes the pointer panel as its whole layout or beside the raw path", () => {
  const solo = loadApp({ search: "?obs=1&layout=pointer" });
  assert.equal(solo.telemouse.obs.layout, "pointer");
  assert.ok(solo.sandbox.document.body.classList.contains("layout-pointer"));
  const pair = loadApp({ search: "?obs=1&layout=compare&hud=speed,pointer" });
  assert.equal(pair.telemouse.obs.layout, "compare");
  assert.deepEqual([...pair.telemouse.obs.hud], ["speed", "pointer"]);
  // The dashboard's spelling works on the overlay too.
  assert.equal(loadApp({ search: "?obs=1&view=pointer" }).telemouse.obs.layout, "pointer");
  assert.equal(loadApp({ search: "?obs=1&view=compare" }).telemouse.obs.layout, "compare");
  // And the server's [viz.obs] default can name it.
  assert.equal(loadApp({ search: "?obs=1", config: { obs_route: true, obs: { layout: "compare" } } }).telemouse.obs.layout, "compare");
  assert.equal(loadApp({ search: "?obs=1&layout=cursor" }).telemouse.obs.layout, "split", "unknown → first");
});

test("the pointer speed reaches the stats bar and the HUD", () => {
  const { telemouse: { engine, ui, _test }, elements } = loadApp({ search: "?obs=1&hud=pointer" });
  ui.setMode("replay");
  engine.ingest(sessionEnvelope({ pointer: { enhance_pointer_precision: false, speed: 10 } }));
  engine.speedPx = 1234.6;
  _test.paintStats(1);
  assert.equal(elements.get("sSpeedPx").textContent, (1235).toLocaleString());
  assert.equal(_test.hudValue("pointer", engine), new Intl.NumberFormat().format(1235));
});
