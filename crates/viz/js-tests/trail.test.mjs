// The trail renderer's decimation (crates/viz/src/app.js, `decimateTrail`)
// and the path `Panel.drawTrail` strokes from it.
//
//   node --test crates/viz/js-tests/trail.test.mjs
//
// The rule being pinned down: dropping the points no pixel would show must
// not change the picture. So with decimation off the output is the old
// renderer's, segment for segment; with it on, the line still ends where the
// trail ends, never strays from the path by more than the step, never
// crosses a break, and costs what the path is long, not what the mouse polls.

import test from "node:test";
import assert from "node:assert/strict";

import { loadApp } from "./harness.mjs";

const app = loadApp();
const { Trail, trailScratch, decimateTrail, trailLevels, trailMinStep } = app.telemouse._test;
const VL = trailLevels.v, AL = trailLevels.a;
const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);

/** A view whose panel coordinates are the trail's own. */
const IDENTITY = { w: 0, h: 0, cx: 0, cy: 0, scale: 1 };

/** A trail from `[x, y, t, v, brk?]` rows. */
function trailOf(points, cap = 4096) {
  const trail = new Trail(cap);
  for (const [x, y, t, v, brk] of points) trail.push(x, y, t, v, !!brk);
  return trail;
}

/** The renderer as it was before decimation: every drawable segment, by
    bucket, as [x0, y0, x1, y1]. */
function reference(points, now, decay, vref) {
  const out = new Map();
  for (let k = 1; k < points.length; k++) {
    const [x, y, t, v, brk] = points[k];
    if (brk) continue;
    const age = now - t;
    if (!(age <= decay && age >= 0)) continue;
    const vi = clamp(Math.round(Math.sqrt(clamp(Math.fround(v) / (vref || 1), 0, 1)) * (VL - 1)), 0, VL - 1);
    const ai = clamp(Math.round((1 - age / decay) * (AL - 1)), 0, AL - 1);
    const b = vi * AL + ai;
    if (!out.has(b)) out.set(b, []);
    out.get(b).push([points[k - 1][0], points[k - 1][1], x, y]);
  }
  return out;
}

/** The segments in a `decimateTrail` result, by bucket, same shape. */
function segmentsOf(path) {
  const out = new Map();
  for (let b = 0; b < path.head.length; b++) {
    for (let i = path.head[b]; i >= 0; i = path.next[i]) {
      assert.equal(path.bucket[i], b, "a chained segment is in its own bucket");
      assert.ok(i >= 1, "a segment has a start point");
      if (!out.has(b)) out.set(b, []);
      out.get(b).push([path.x[i - 1], path.y[i - 1], path.x[i], path.y[i]]);
    }
  }
  return out;
}

const sorted = (m) => [...m.entries()].sort((a, b) => a[0] - b[0]);
const count = (m) => [...m.values()].reduce((n, s) => n + s.length, 0);

/** Distance from point p to segment a–b. */
function distToSegment(px, py, ax, ay, bx, by) {
  const dx = bx - ax, dy = by - ay;
  const len2 = dx * dx + dy * dy;
  const f = len2 ? clamp(((px - ax) * dx + (py - ay) * dy) / len2, 0, 1) : 0;
  return Math.hypot(px - (ax + dx * f), py - (ay + dy * f));
}

/** A deterministic wandering path: `n` points, `hz` apart in time, steps of
    about `step` pixels, speeds all over the colour scale. */
function wander(n, hz, step) {
  let state = 0x1234abcd;
  const rand = () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return (state >>> 8) / 0x1000000;
  };
  const points = [];
  let x = 100, y = 100, heading = 0;
  for (let i = 0; i < n; i++) {
    heading += (rand() - 0.5) * 0.6;
    const d = step * (0.2 + 1.6 * rand());
    x += Math.cos(heading) * d;
    y += Math.sin(heading) * d;
    points.push([x, y, i / hz, d * hz]);
  }
  return points;
}

test("with decimation off the path is the old renderer's, segment for segment", () => {
  const points = wander(3000, 1000, 2.5);
  const now = 3.0, decay = 3, vref = 4000;
  const path = decimateTrail(trailOf(points), IDENTITY, now, decay, vref, 0, trailScratch(4096));
  const want = reference(points, now, decay, vref);
  assert.equal(path.segments, count(want));
  assert.ok(want.size > 20, "the fixture spreads over many buckets (" + want.size + ")");
  assert.deepEqual(sorted(segmentsOf(path)), sorted(want));
});

test("points a pixel or more apart are all kept at the default step", () => {
  // 1 kHz at a brisk pace: nothing here is sub-pixel, so nothing may go.
  const points = wander(2000, 1000, 6).filter((p, i, all) =>
    i === 0 || Math.hypot(p[0] - all[i - 1][0], p[1] - all[i - 1][1]) >= 1);
  const now = points[points.length - 1][2], decay = 3, vref = 9000;
  const path = decimateTrail(trailOf(points), IDENTITY, now, decay, vref, trailMinStep, trailScratch(4096));
  assert.deepEqual(sorted(segmentsOf(path)), sorted(reference(points, now, decay, vref)));
});

test("an 8 kHz crawl costs what the path is long, not what the mouse polls", () => {
  // Three seconds at 8 kHz covering ~300 px: 24,000 reports of 1/80 px each.
  const n = 24000, hz = 8000;
  const points = [];
  for (let i = 0; i < n; i++) {
    const s = (i / (n - 1)) * 300;
    points.push([50 + s, 200 + 20 * Math.sin(s / 25), i / hz, 100]);
  }
  const now = (n - 1) / hz, decay = 3;
  const scratch = trailScratch(1 << 15);
  const path = decimateTrail(trailOf(points, 1 << 15), IDENTITY, now, decay, 400, 0.75, scratch);
  assert.equal(path, scratch, "the result is the scratch it was given");

  assert.ok(path.segments < 600, path.segments + " segments for a 300 px path");
  assert.ok(path.segments > 300, "but not fewer than the path needs (" + path.segments + ")");

  // The line still starts and ends exactly where the trail does.
  const last = points[n - 1];
  assert.equal(path.x[0], points[0][0]);
  assert.equal(path.y[0], points[0][1]);
  assert.equal(path.x[path.n - 1], last[0]);
  assert.equal(path.y[path.n - 1], last[1]);

  // It is one unbroken run: every kept point after the first ends a segment.
  for (let i = 1; i < path.n; i++) assert.ok(path.bucket[i] >= 0, "no break at " + i);

  // And no report is further from the drawn line than the step.
  let at = 1, worst = 0;
  for (const [x, y] of points) {
    let best = Infinity;
    // The kept points are in trail order, so the nearest segment only moves forward.
    for (let i = at; i < path.n; i++) {
      const d = distToSegment(x, y, path.x[i - 1], path.y[i - 1], path.x[i], path.y[i]);
      if (d < best) { best = d; at = i; }
      if (d > best + 2) break;
    }
    if (best > worst) worst = best;
  }
  assert.ok(worst < 0.75, "worst deviation " + worst.toFixed(3) + " px");
});

test("buckets change without a gap: each segment starts where the last one ended", () => {
  // Speed ramps up along the path, so consecutive segments keep changing bucket.
  const points = [];
  for (let i = 0; i < 400; i++) points.push([i * 3, 50, i / 1000, i * 10]);
  const path = decimateTrail(trailOf(points), IDENTITY, 0.4, 3, 4000, 0.75, trailScratch(4096));
  assert.equal(path.segments, 399);
  const buckets = new Set();
  for (let i = 1; i < path.n; i++) {
    buckets.add(path.bucket[i]);
    assert.equal(path.x[i], points[i][0]);
  }
  assert.ok(buckets.size > 5, "the ramp crosses buckets (" + buckets.size + ")");
  // By construction segment i starts at point i-1, which segment i-1 ended
  // at, whatever bucket that one was in. The chains must agree with that:
  // every segment is reachable from exactly one bucket's chain.
  assert.equal(count(segmentsOf(path)), 399);
});

test("nothing is drawn to a break, an expired point or a future one, and the line resumes from it", () => {
  const points = [
    [0, 0, 0.0, 10],      // expired (older than the decay window)
    [10, 0, 0.5, 10],     // expired too: no segment into it
    [20, 0, 2.0, 10],     // in the window: drawn from (10, 0), as it always was
    [30, 0, 2.1, 10],
    [500, 500, 2.2, 10, true],   // a break: nothing drawn to it …
    [510, 500, 2.3, 10],         // … and the line resumes from it
    [520, 500, 9.0, 10],         // in the future (a backwards seek): not drawn
    [530, 500, 2.5, 10],
  ];
  const now = 2.6, decay = 1;
  const path = decimateTrail(trailOf(points), IDENTITY, now, decay, 100, 0.75, trailScratch(64));
  const got = [...segmentsOf(path).values()].flat().sort((a, b) => a[0] - b[0]);
  assert.deepEqual(got, [
    [10, 0, 20, 0], [20, 0, 30, 0], [500, 500, 510, 500], [520, 500, 530, 500],
  ]);
  assert.deepEqual(sorted(segmentsOf(path)), sorted(reference(points, now, decay, 100)));
  // The two leading expired points collapse to one start, not two.
  assert.equal(path.bucket[0], -1);
  assert.equal(path.x[0], 10);
});

test("a hand trembling inside one pixel is still a dot, and a run ends on its last point", () => {
  // Every point within 0.3 px of the first: all of them are skipped …
  const tremble = [];
  for (let i = 0; i < 50; i++) tremble.push([100 + 0.3 * Math.cos(i), 100 + 0.3 * Math.sin(i), i / 1000, 5]);
  let path = decimateTrail(trailOf(tremble), IDENTITY, 0.05, 3, 100, 0.75, trailScratch(64));
  // … except that the run has to end somewhere: one segment, to the newest point.
  assert.equal(path.segments, 1);
  assert.equal(path.x[1], tremble[49][0]);
  assert.equal(path.y[1], tremble[49][1]);

  // The same when the run ends at a break instead of at the end of the trail.
  const points = [
    [0, 0, 0.000, 5], [0.2, 0, 0.001, 5], [0.4, 0, 0.002, 5],
    [300, 300, 0.003, 5, true],
    [300.1, 300, 0.004, 5], [300.2, 300, 0.005, 5],
  ];
  path = decimateTrail(trailOf(points), IDENTITY, 0.01, 3, 100, 0.75, trailScratch(64));
  const got = [...segmentsOf(path).values()].flat().sort((a, b) => a[0] - b[0]);
  assert.deepEqual(got, [[0, 0, 0.4, 0], [300, 300, 300.2, 300]]);
});

test("the view transform is the panel's, and the ring may wrap", () => {
  const view = { w: 800, h: 600, cx: 10, cy: -5, scale: 20 };
  const trail = new Trail(8);
  // Twelve pushes into eight slots: the ring has wrapped and holds the last 8.
  for (let i = 0; i < 12; i++) trail.push(i, i * 0.5, i / 100, 50, false);
  assert.equal(trail.n, 8);
  const path = decimateTrail(trail, view, 0.11, 3, 100, 0.75, trailScratch(8));
  assert.equal(path.n, 8);
  assert.equal(path.segments, 7);
  for (let k = 0; k < 8; k++) {
    const i = 4 + k;
    assert.equal(path.x[k], 400 + (i - 10) * 20);
    assert.equal(path.y[k], 300 + (i * 0.5 + 5) * 20);
  }
});

test("an empty or one-point trail draws nothing and leaves the scratch clean", () => {
  const scratch = trailScratch(16);
  decimateTrail(trailOf([[0, 0, 0, 1], [5, 5, 0.001, 1]], 16), IDENTITY, 0.001, 3, 10, 0.75, scratch);
  assert.equal(scratch.segments, 1);
  decimateTrail(trailOf([[0, 0, 0, 1]], 16), IDENTITY, 0, 3, 10, 0.75, scratch);
  assert.equal(scratch.segments, 0);
  assert.equal(scratch.n, 0);
  assert.ok(scratch.head.every((h) => h === -1), "no stale chain from the last frame");
});

/** A 2D context that records what drawTrail strokes. */
function recordingContext() {
  const strokes = [];
  let path = [];
  const ctx = {
    lineCap: "", lineJoin: "", globalAlpha: 1, strokeStyle: "", lineWidth: 0,
    beginPath() { path = []; },
    moveTo(x, y) { path.push([[x, y]]); },
    lineTo(x, y) { path[path.length - 1].push([x, y]); },
    stroke() {
      strokes.push({
        style: ctx.strokeStyle, alpha: ctx.globalAlpha, width: ctx.lineWidth, cap: ctx.lineCap,
        join: ctx.lineJoin, subpaths: path,
      });
    },
  };
  return { ctx, strokes };
}

test("drawTrail strokes one path per bucket, joined into polylines, covering every segment once", () => {
  const { telemouse } = loadApp();
  const { deskPanel, engine } = telemouse;
  // Pin the view so panel coordinates are the trail's own.
  Object.assign(deskPanel, { w: 0, h: 0, cx: 0, cy: 0, scale: 1, dpr: 1 });
  const points = wander(3000, 1000, 2.5);
  const trail = trailOf(points, 1 << 15);
  engine.playT = 3.0;
  engine.decay = 3;
  const { ctx, strokes } = recordingContext();
  deskPanel.drawTrail(ctx, trail, engine, 4000);

  const path = decimateTrail(trail, deskPanel, 3.0, 3, 4000, trailMinStep, trailScratch(1 << 15));
  const buckets = segmentsOf(path);
  assert.equal(strokes.length, buckets.size, "one stroke per occupied bucket");
  assert.ok(strokes.length <= VL * AL);

  // Unroll the polylines back into segments: exactly the decimated set.
  const drawn = [];
  let subpaths = 0;
  for (const s of strokes) {
    assert.equal(s.cap, "round");
    assert.equal(s.join, "round");
    assert.ok(s.alpha > 0.1 && s.alpha <= 0.95);
    for (const sub of s.subpaths) {
      subpaths++;
      assert.ok(sub.length >= 2, "no moveTo without a lineTo");
      for (let i = 1; i < sub.length; i++) drawn.push([...sub[i - 1], ...sub[i]].join(","));
    }
  }
  const want = [...buckets.values()].flat().map((s) => s.join(","));
  assert.deepEqual(drawn.sort(), want.sort());
  assert.ok(subpaths < drawn.length, "consecutive segments share a subpath (" +
    subpaths + " subpaths for " + drawn.length + " segments)");
  assert.equal(ctx.globalAlpha, 1, "alpha is restored for whatever draws next");
});

test("the decimation step is in device pixels: a 2x canvas keeps half-pixel detail", () => {
  const { telemouse } = loadApp();
  const { deskPanel, engine } = telemouse;
  // Points 0.5 CSS px apart: sub-step at 1x, a full device pixel at 2x.
  const points = [];
  for (let i = 0; i < 200; i++) points.push([i * 0.5, 0, i / 1000, 50]);
  const trail = trailOf(points);
  engine.playT = 0.2;
  engine.decay = 3;
  const segments = (dpr) => {
    Object.assign(deskPanel, { w: 0, h: 0, cx: 0, cy: 0, scale: 1, dpr });
    const { ctx, strokes } = recordingContext();
    deskPanel.drawTrail(ctx, trail, engine, 100);
    return strokes.reduce((n, s) => n + s.subpaths.reduce((m, sub) => m + sub.length - 1, 0), 0);
  };
  assert.equal(segments(2), 199, "nothing dropped at 2x");
  const at1 = segments(1);
  assert.ok(at1 >= 99 && at1 <= 101, "every other point at 1x (" + at1 + ")");
});
