"use strict";
/* =====================================================================
   telemouse viz — one engine, two sources.

   Live mode feeds WebSocket envelopes into the same engine that replay
   mode feeds from a streamed .jsonl file. The engine exposes:

       engine.ingest(envelope)   // session | batch | marker, wire JSON
       engine.tick(nowSeconds)   // advance the play head, consume events
       engine.draw()             // render both panels from engine state

   The wire carries raw HID counts only; every physical unit below is
   derived here from the session envelope (mouse_cpi, games table, the
   QPC anchor).

   Wire tolerance is a hard rule here: the capture agent omits zero-valued
   rare fields (buttons, wheel, wheel_h, device_ix) and absent optionals
   (game, cursor_x/y) from its JSON entirely, and older recordings predate
   fields that exist today. Every read below defaults, never assumes.
   ===================================================================== */

const CM_PER_INCH = 2.54;

/* RI_MOUSE_* transition bits, matching telemouse-core::event::buttons. */
const BTN = {
  LEFT_DOWN: 0x0001, LEFT_UP: 0x0002,
  RIGHT_DOWN: 0x0004, RIGHT_UP: 0x0008,
  MIDDLE_DOWN: 0x0010, MIDDLE_UP: 0x0020,
  X1_DOWN: 0x0040, X1_UP: 0x0080,
  X2_DOWN: 0x0100, X2_UP: 0x0200,
};
BTN.ANY_DOWN = BTN.LEFT_DOWN | BTN.RIGHT_DOWN | BTN.MIDDLE_DOWN | BTN.X1_DOWN | BTN.X2_DOWN;
BTN.ANY_UP = BTN.ANY_DOWN << 1;   // every *_UP bit is its *_DOWN bit shifted left
/* Ring colour per button: left, right, everything else. */
const BTN_KIND = [[BTN.LEFT_DOWN, 0], [BTN.RIGHT_DOWN, 1], [BTN.MIDDLE_DOWN, 2], [BTN.X1_DOWN, 2], [BTN.X2_DOWN, 2]];
const RING_COLORS = ["70,214,138", "124,140,255", "255,176,46"];

/* Used when the session has no profile for the foreground process. */
const FALLBACK_SENS = { sens: 1.0, yaw_coeff: 0.022, pitch_coeff: 0.022, fallback: true };

const RING_LIFE = 0.55;      // seconds a click ring stays visible
const WHEEL_LIFE = 1.2;      // seconds a wheel tick stays visible
const MAX_LIVE_EVENTS = 20000;
/* Keep a low/high-water gap between compactions. Without it, once cursor
   crossed MAX_LIVE_EVENTS every render frame shifted all retained typed-array
   columns just to discard the handful of events consumed in that frame. */
const LIVE_TRIM_CHUNK = 4096;
const LIVE_TRIM_TRIGGER = MAX_LIVE_EVENTS + LIVE_TRIM_CHUNK;
/* Absolute ceiling on the live timeline. ingest() enforces this even when
   nothing is consuming (a backgrounded tab suspends rAF but not the socket),
   which is the difference between a bounded buffer and a leak. */
const HARD_EVENT_CAP = MAX_LIVE_EVENTS * 2;
/* Events one tick may integrate. Bounds the frame that runs when a hidden tab
   comes back and the play head snaps across a backlog: the arithmetic is
   cheap, but doing 40k of it inside one rAF is a visible hitch. */
const MAX_CONSUME_PER_TICK = 12000;
/* Trail rings: 12s (the longest decay) of 1kHz input, with headroom. */
const TRAIL_CAP = 1 << 15;
const EFFECT_CAP = 512;      // click rings / wheel ticks in flight
const CHECKPOINT_SEC = 10;   // integrator snapshot spacing, in timeline seconds
const MAX_DROP_MARKS = 4000;
const PITCH_LIMIT = 89;      // FPS engines clamp pitch; so do we
const PANEL_BG = "#0e131c";  // must match --panel: the canvases are opaque now

const QUERY = new URLSearchParams(location.search);
const PROFILE = QUERY.get("profile") === "1";
/* Draw-rate cap (`?fps=`). A 240Hz monitor means 240 full two-canvas
   repaints a second, and the GPU doing that is the same GPU running the
   game; 120 is visually identical for a trail and halves the cost. An
   unfocused dashboard drops to FPS_BACKGROUND — a window you are not
   looking at should cost nothing you notice. OBS composites at its own
   rate (60 by default), so anything above that is wasted there. */
const FPS_DEFAULT = 120, FPS_OBS_DEFAULT = 60, FPS_BACKGROUND = 30;
const fpsQ = parseFloat(QUERY.get("fps"));
const VIEWS = ["both", "desk", "aim"];
const VIEW = VIEWS.indexOf(QUERY.get("view")) >= 0 ? QUERY.get("view") : "both";

/* ---------- small helpers ---------- */
const clamp = (v, lo, hi) => v < lo ? lo : v > hi ? hi : v;

/* =====================================================================
   OBS browser-source mode

   Reached via the /obs route (server sets `obs_route`) or `?obs=1` on the
   dashboard. Three layers, each overriding the last: built-in defaults,
   the server's [viz.obs] table, then URL query parameters — so one toml
   sets the house style and each OBS source can still tweak its own URL:

       /obs?layout=aim&hud=speed,aim&scale=1.5&bg=0e131c80&grid=0

   What it changes: the chrome (top bar, transport, stats bar, toasts) is
   hidden; the page and panels are transparent unless `bg` says otherwise;
   a HUD replaces the stats bar; nothing touches localStorage; and the
   per-frame DOM work drops to one small HUD repaint at 10Hz.
   ===================================================================== */

const SERVER_CFG = (typeof window.TELEMOUSE_CONFIG === "object" && window.TELEMOUSE_CONFIG) || {};
const OBS_MODE = QUERY.get("obs") === "1" || SERVER_CFG.obs_route === true;

const HUD_ITEMS = {
  speed:   { k: "speed",     unit: "cm/s" },
  aim:     { k: "aim speed", unit: "°/s" },
  cpm:     { k: "clicks/min" },
  eps:     { k: "events/s" },
  dist:    { k: "hand dist", unit: "m" },
  aimdist: { k: "aim dist",  unit: "°" },
  clicks:  { k: "clicks" },
  game:    { k: "game", text: true },
  latency: { k: "latency",   unit: "ms" },
};

/** "transparent" | "rrggbb" | "#rrggbbaa" | "rgb" → { css, alpha }. */
function parseBg(s) {
  s = String(s || "").trim().toLowerCase();
  if (!s || s === "transparent" || s === "none") return { css: "transparent", alpha: 0 };
  let hex = s[0] === "#" ? s.slice(1) : s;
  if (hex.length === 3) hex = hex.replace(/./g, (c) => c + c);
  if (!/^[0-9a-f]{6}([0-9a-f]{2})?$/.test(hex)) return { css: "transparent", alpha: 0 };
  const r = parseInt(hex.slice(0, 2), 16), g = parseInt(hex.slice(2, 4), 16), b = parseInt(hex.slice(4, 6), 16);
  const a = hex.length === 8 ? parseInt(hex.slice(6, 8), 16) / 255 : 1;
  return { css: "rgba(" + r + "," + g + "," + b + "," + a.toFixed(3) + ")", alpha: a };
}

const OBS = OBS_MODE ? (() => {
  const d = Object.assign({
    layout: "split", background: "transparent", hud: ["speed", "aim", "cpm"],
    hud_position: "bottom-left", scale: 1, trail_secs: 3, buffer_ms: 35,
    grid: true, legend: false, labels: false,
  }, SERVER_CFG.obs || {});
  const str = (name, dflt, allowed) => {
    const v = QUERY.get(name);
    const pick = v === null ? dflt : v;
    return allowed.indexOf(pick) >= 0 ? pick : allowed[0];
  };
  const bool = (name, dflt) => {
    const v = QUERY.get(name);
    return v === null ? !!dflt : !(v === "0" || v === "false" || v === "no" || v === "off");
  };
  const num = (name, dflt, lo, hi) => {
    const v = QUERY.get(name);
    const n = v === null ? +dflt : parseFloat(v);
    return isFinite(n) ? clamp(n, lo, hi) : clamp(+dflt, lo, hi);
  };
  const hudRaw = QUERY.get("hud");
  const hud = (hudRaw === null ? (Array.isArray(d.hud) ? d.hud : []) : hudRaw.split(","))
    .map((s) => String(s).trim().toLowerCase())
    .filter((s) => Object.prototype.hasOwnProperty.call(HUD_ITEMS, s));
  return {
    /* `view=desk|aim|both` (the dashboard's spelling) is accepted here too,
       so the same URL suffix works on both routes. */
    layout: str("layout", { desk: "desk", aim: "aim", both: "split" }[QUERY.get("view")] || d.layout,
                ["split", "stack", "desk", "aim"]),
    bg: parseBg(QUERY.get("bg") === null ? d.background : QUERY.get("bg")),
    hud: hud,
    hudPos: str("hudpos", d.hud_position, ["bottom-left", "top-left", "top-right", "bottom-right"]),
    scale: num("scale", d.scale, 0.5, 4),
    trail: num("trail", d.trail_secs, 0.3, 12),
    buffer: num("buffer", d.buffer_ms, 10, 200) / 1000,
    grid: bool("grid", d.grid),
    legend: bool("legend", d.legend),
    labels: bool("labels", d.labels),
  };
})() : null;

const MAX_FPS = isFinite(fpsQ) && fpsQ > 0 ? clamp(fpsQ, 5, 400) : (OBS ? FPS_OBS_DEFAULT : FPS_DEFAULT);

/* Stroke / marker size multiplier: 1 on the dashboard, `scale` in OBS mode. */
const S = OBS ? OBS.scale : 1;
/* Grid lines are near-invisible on a transparent page composited over dark
   game footage, so the overlay gets a lighter, alpha-based palette. */
const GRID = OBS && OBS.bg.alpha < 1
  ? { line: "rgba(255,255,255,0.07)", axis: "rgba(255,255,255,0.16)", wrap: "rgba(200,140,255,0.35)" }
  : { line: "#151c28", axis: "#25303f", wrap: "#3a2b46" };


function niceStep(raw) {
  if (!(raw > 0) || !isFinite(raw)) return 1;
  const exp = Math.floor(Math.log10(raw));
  const base = Math.pow(10, exp);
  const m = raw / base;
  const mult = m <= 1.5 ? 1 : m <= 3.5 ? 2 : m <= 7.5 ? 5 : 10;
  return mult * base;
}

function fmtTime(s) {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  const rest = s - m * 60;
  return m + ":" + (rest < 10 ? "0" : "") + rest.toFixed(2);
}

const pad2 = (n) => (n < 10 ? "0" : "") + n;

/** Local "YYYY-MM-DD HH:MM:SS" for an epoch ms — the wall-clock readout. */
function fmtWallClock(ms) {
  const d = new Date(ms);
  return d.getFullYear() + "-" + pad2(d.getMonth() + 1) + "-" + pad2(d.getDate()) + " " +
         pad2(d.getHours()) + ":" + pad2(d.getMinutes()) + ":" + pad2(d.getSeconds());
}

/** The same, shaped for an <input type=datetime-local> value. */
function toDatetimeLocal(ms) {
  return fmtWallClock(ms).replace(" ", "T");
}

/* Rolling event counter over a fixed window, bucketed so 1kHz input costs
   O(1) per event instead of a 60k-entry array. */
class Rolling {
  constructor(windowSec, buckets) {
    this.win = windowSec;
    this.n = buckets;
    this.bw = windowSec / buckets;
    this.count = new Float64Array(buckets);
    this.stamp = new Float64Array(buckets).fill(-1e12);
  }
  reset() { this.count.fill(0); this.stamp.fill(-1e12); }
  add(t, n) {
    const b = Math.floor(t / this.bw);
    const i = ((b % this.n) + this.n) % this.n;
    if (this.stamp[i] !== b) { this.stamp[i] = b; this.count[i] = 0; }
    this.count[i] += (n === undefined ? 1 : n);
  }
  total(t) {
    const cur = Math.floor(t / this.bw);
    let s = 0;
    for (let i = 0; i < this.n; i++) {
      const age = cur - this.stamp[i];
      if (age >= 0 && age < this.n) s += this.count[i];
    }
    return s;
  }
  perSecond(t) { return this.total(t) / this.win; }
  /* Saved into integrator checkpoints so a backward seek restores the rate
     readouts exactly, not just the positions. */
  save() { return { count: this.count.slice(), stamp: this.stamp.slice() }; }
  load(s) { this.count.set(s.count); this.stamp.set(s.stamp); }
}

/* =====================================================================
   Storage: struct-of-arrays timeline, ring-buffer trails

   At 1kHz a per-event object and a per-trail-point object are ~2000
   allocations a second, all of them garbage within seconds — exactly the
   shape that turns into visible GC sawtooth. Columns and rings replace
   both with fixed typed arrays that are never reallocated in steady state
   and never spliced.
   ===================================================================== */

/** Event timeline as parallel typed arrays, growable by doubling. */
class EventColumns {
  constructor(cap) {
    this.cap = cap || 1 << 16;
    this.n = 0;
    this._alloc(this.cap);
  }
  _alloc(cap) {
    this.t = new Float64Array(cap);
    this.dx = new Int32Array(cap);
    this.dy = new Int32Array(cap);
    this.b = new Int32Array(cap);
    this.w = new Int32Array(cap);
    this.wh = new Int32Array(cap);
    this.mi = new Int32Array(cap);
    this.cap = cap;
  }
  _grow(need) {
    let cap = this.cap;
    while (cap < need) cap *= 2;
    const old = { t: this.t, dx: this.dx, dy: this.dy, b: this.b, w: this.w, wh: this.wh, mi: this.mi };
    const n = this.n;
    this._alloc(cap);
    this.t.set(old.t.subarray(0, n));
    this.dx.set(old.dx.subarray(0, n));
    this.dy.set(old.dy.subarray(0, n));
    this.b.set(old.b.subarray(0, n));
    this.w.set(old.w.subarray(0, n));
    this.wh.set(old.wh.subarray(0, n));
    this.mi.set(old.mi.subarray(0, n));
  }
  push(t, dx, dy, b, w, wh, mi) {
    if (this.n === this.cap) this._grow(this.cap + 1);
    const i = this.n++;
    this.t[i] = t; this.dx[i] = dx; this.dy[i] = dy;
    this.b[i] = b; this.w[i] = w; this.wh[i] = wh; this.mi[i] = mi;
  }
  /** Discard the oldest `k` events. One memmove per column, no splice. */
  dropFront(k) {
    if (k <= 0) return;
    if (k >= this.n) { this.n = 0; return; }
    const n = this.n;
    this.t.copyWithin(0, k, n);
    this.dx.copyWithin(0, k, n);
    this.dy.copyWithin(0, k, n);
    this.b.copyWithin(0, k, n);
    this.w.copyWithin(0, k, n);
    this.wh.copyWithin(0, k, n);
    this.mi.copyWithin(0, k, n);
    this.n = n - k;
  }
  clear() { this.n = 0; }
}

/** Fixed-capacity ring of trail points. Oldest is evicted, never spliced. */
class Trail {
  constructor(cap) {
    this.cap = cap;
    this.head = 0;
    this.n = 0;
    this.x = new Float64Array(cap);
    this.y = new Float64Array(cap);
    this.t = new Float64Array(cap);
    this.v = new Float32Array(cap);
    this.brk = new Uint8Array(cap);
  }
  clear() { this.head = 0; this.n = 0; }
  push(x, y, t, v, brk) {
    const cap = this.cap;
    let i;
    if (this.n === cap) { i = this.head; this.head = this.head + 1 === cap ? 0 : this.head + 1; }
    else { i = (this.head + this.n) % cap; this.n++; }
    this.x[i] = x; this.y[i] = y; this.t[i] = t; this.v[i] = v; this.brk[i] = brk ? 1 : 0;
  }
  trim(cutoff) {
    const cap = this.cap;
    while (this.n > 0 && this.t[this.head] < cutoff) {
      this.head = this.head + 1 === cap ? 0 : this.head + 1;
      this.n--;
    }
  }
}

/** Small ring of short-lived effect objects (click rings, wheel ticks). */
class EffectRing {
  constructor(cap) { this.cap = cap; this.buf = new Array(cap); this.head = 0; this.n = 0; }
  clear() { this.head = 0; this.n = 0; this.buf.fill(undefined); }
  push(o) {
    const cap = this.cap;
    let i;
    if (this.n === cap) { i = this.head; this.head = this.head + 1 === cap ? 0 : this.head + 1; }
    else { i = (this.head + this.n) % cap; this.n++; }
    this.buf[i] = o;
  }
  trim(cutoff) {
    const cap = this.cap;
    while (this.n > 0 && this.buf[this.head].t < cutoff) {
      this.buf[this.head] = undefined;
      this.head = this.head + 1 === cap ? 0 : this.head + 1;
      this.n--;
    }
  }
  at(k) { return this.buf[(this.head + k) % this.cap]; }
}

/* Velocity → colour: cool blue (slow) through cyan and amber to hot white. */
const RAMP = [
  [0.00, 42, 86, 200],
  [0.28, 42, 200, 235],
  [0.52, 120, 235, 190],
  [0.72, 255, 200, 60],
  [0.88, 255, 122, 32],
  [1.00, 255, 250, 245],
];
function rampColor(x) {
  x = clamp(x, 0, 1);
  for (let i = 1; i < RAMP.length; i++) {
    if (x <= RAMP[i][0]) {
      const a = RAMP[i - 1], b = RAMP[i];
      const f = (x - a[0]) / (b[0] - a[0] || 1);
      return [
        Math.round(a[1] + (b[1] - a[1]) * f),
        Math.round(a[2] + (b[2] - a[2]) * f),
        Math.round(a[3] + (b[3] - a[3]) * f),
      ];
    }
  }
  const l = RAMP[RAMP.length - 1];
  return [l[1], l[2], l[3]];
}
/* Pre-quantised ramp so the trail renderer can batch segments into a small
   number of stroke() calls instead of one per sample. */
const VLEVELS = 14;
const RAMP_CSS = [];
for (let i = 0; i < VLEVELS; i++) {
  const c = rampColor(i / (VLEVELS - 1));
  RAMP_CSS.push("rgb(" + c[0] + "," + c[1] + "," + c[2] + ")");
}
const ALEVELS = 7;

/* =====================================================================
   Engine
   ===================================================================== */

const engine = {
  /* session context */
  session: null,
  sessionId: null,
  cpi: 800,
  qpcFreq: 1e7,
  anchorQpc: null,
  anchorUtcUs: null,
  anchorUncertaintyUs: null,
  devices: [],
  sensCache: new Map(),
  game: null,
  sens: FALLBACK_SENS,
  pointerLocked: false,
  cursorX: null, cursorY: null, screenW: 0, screenH: 0,

  /* timeline (shared by live and replay) */
  ev: new EventColumns(),
  metas: [],
  metaBase: 0,
  cursor: 0,
  markers: [],
  markerCursor: 0,
  tStart: 0,
  tEnd: 0,
  /* Seconds of events one live batch covers; floors the live buffer. */
  batchSpan: 0,
  haveTimeline: false,

  /* integrator checkpoints, built at load; empty in live mode */
  checkpoints: [],
  cpBuilt: 0,
  _cpNextT: 0,

  /* play head */
  playT: 0,
  clockStarted: false,
  lastWall: 0,
  /* True while a replay load is streaming in. The loader owns the
     integrator state during that window (it is building checkpoints with
     it), so the render loop must not also be consuming events. */
  loading: false,

  /* integrators */
  deskX: 0, deskY: 0,
  yawRaw: 0, pitch: 0,
  lastEventT: null,

  /* visuals */
  deskTrail: new Trail(TRAIL_CAP),
  aimTrail: new Trail(TRAIL_CAP),
  rings: new EffectRing(EFFECT_CAP),
  wheelTicks: new EffectRing(EFFECT_CAP),
  dirty: true,

  /* stats */
  totalCm: 0,
  totalDeg: 0,
  totalClicks: 0,
  held: 0,            // *_DOWN bits of buttons currently pressed (steady ring at the head)
  drops: 0,
  lostBatches: 0,
  absFrames: 0,
  lastSeq: null,
  dropMarks: [],
  clickRate: new Rolling(60, 60),
  eventRate: new Rolling(2, 20),
  speedCm: 0,
  speedDeg: 0,
  vrefDesk: 30,
  vrefAim: 300,
  _tickCm: 0,
  _tickDeg: 0,
  _quiet: false,

  /* options */
  decay: 3,

  /* ------------------------------------------------------------------ */

  /** Full teardown: nothing from a previous session survives. */
  reset() {
    this.session = null;
    this.sessionId = null;
    this.anchorQpc = null;
    this.anchorUtcUs = null;
    this.anchorUncertaintyUs = null;
    this.devices = [];
    this.sensCache.clear();
    this.game = null;
    this.sens = FALLBACK_SENS;
    this.pointerLocked = false;
    this.cursorX = null; this.cursorY = null;
    this.screenW = 0; this.screenH = 0;
    this.ev.clear();
    this.metas.length = 0;
    this.metaBase = 0;
    this.cursor = 0;
    this.held = 0;
    this.markers.length = 0;
    this.markerCursor = 0;
    this.tStart = 0; this.tEnd = 0;
    this.batchSpan = 0;
    this.haveTimeline = false;
    this.checkpoints.length = 0;
    this.cpBuilt = 0;
    this._cpNextT = 0;
    this.playT = 0;
    this.clockStarted = false;
    this.drops = 0;
    this.lostBatches = 0;
    this.absFrames = 0;
    this.lastSeq = null;
    this.dropMarks.length = 0;
    this.resetTotals();
    this.resetIntegrators();
    this.dirty = true;
  },

  /** Session totals derived by integration. Cleared before any full
      re-integration, since that re-derives them from the first event; NOT
      cleared by "recenter", which only moves the view's origin.
      `drops` / `lostBatches` / `absFrames` are deliberately excluded: they
      are accumulated at ingest time from batch envelopes, not re-derivable
      by replaying events. */
  resetTotals() {
    this.totalCm = 0;
    this.totalDeg = 0;
    this.totalClicks = 0;
    this.clickRate.reset();
    this.eventRate.reset();
  },

  /** Recenter: zero the integrated positions and drop the visible history.
      Session totals survive — they describe the session, not the view. */
  resetIntegrators() {
    this.deskX = 0; this.deskY = 0;
    this.yawRaw = 0; this.pitch = 0;
    this.lastEventT = null;
    this.deskTrail.clear();
    this.aimTrail.clear();
    this.rings.clear();
    this.wheelTicks.clear();
    this.speedCm = 0; this.speedDeg = 0;
    this.dirty = true;
  },

  /** Back to "nothing integrated yet", keeping the timeline itself. */
  rewind() {
    this.resetTotals();
    this.resetIntegrators();
    this.cursor = 0;
    this.held = 0;
    this.markerCursor = 0;
  },

  /** Aim conversion for a process name, with the documented fallback. */
  sensFor(game) {
    const key = (game || "").toLowerCase();
    if (this.sensCache.has(key)) return this.sensCache.get(key);
    let s = FALLBACK_SENS;
    const table = this.session && this.session.games;
    if (table && key && Object.prototype.hasOwnProperty.call(table, key)) {
      const g = table[key];
      s = {
        sens: +g.sens,
        yaw_coeff: g.yaw_coeff === undefined ? 0.022 : +g.yaw_coeff,
        pitch_coeff: g.pitch_coeff === undefined ? 0.022 : +g.pitch_coeff,
        fallback: false,
      };
    }
    this.sensCache.set(key, s);
    return s;
  },

  metaAt(i) { return this.metas[this.ev.mi[i] - this.metaBase]; },

  /** Feed one wire envelope. Identical path for WebSocket and JSONL. */
  ingest(env) {
    if (!env || typeof env !== "object") return;
    const type = env.type;

    if (type === "viz_stats") { ui.onVizStats(env); return; }

    if (type === "session") {
      const id = env.session_id === undefined ? null : env.session_id;
      /* A different session id on the same feed means the capture agent
         restarted: new QPC anchor, new device table, new seq numbering.
         Keeping any of the old state would silently mix two timelines —
         the old events would sit at nonsense offsets against the new
         anchor and the play head would chase a tEnd that no longer exists.
         Tear everything down first, then adopt. */
      if (this.sessionId !== null && id !== null && id !== this.sessionId) {
        const from = this.sessionId;
        this.reset();
        ui.onSessionRestart(from, id);
      }
      this.session = env;
      this.sessionId = id;
      this.cpi = +env.mouse_cpi > 0 ? +env.mouse_cpi : 800;
      const anchor = env.anchor || {};
      this.qpcFreq = +(anchor.qpc_freq || env.qpc_freq) || 1e7;
      this.anchorQpc = anchor.qpc !== undefined ? +anchor.qpc : null;
      /* utc_us is what makes the latency tile possible: it maps event
         offsets back onto this browser's wall clock. */
      this.anchorUtcUs = anchor.utc_us !== undefined ? +anchor.utc_us
        : (env.started_utc_us !== undefined ? +env.started_utc_us : null);
      this.anchorUncertaintyUs = env.anchor_uncertainty_us === undefined
        ? null : +env.anchor_uncertainty_us;
      this.devices = Array.isArray(env.devices) ? env.devices : [];
      this.sensCache.clear();
      this.sens = this.sensFor(this.game);
      this.dirty = true;
      ui.onSessionChanged();
      return;
    }

    if (type === "marker") {
      const t = this.qpcToT(env.ts_qpc);
      this.markers.push({ t: t, label: env.label || "marker" });
      this.noteTime(t);
      return;
    }

    if (type !== "batch") return;

    /* Data-quality accounting happens even for an empty batch. */
    const drops = env.drops_since_last | 0;
    this.drops += drops;
    this.absFrames += (env.abs_frames_since_last | 0);

    let lost = 0;
    const seq = typeof env.seq_no === "number" ? env.seq_no : null;
    if (seq !== null) {
      if (this.lastSeq !== null && seq > this.lastSeq + 1) lost = seq - this.lastSeq - 1;
      if (this.lastSeq === null || seq > this.lastSeq) this.lastSeq = seq;
    }
    this.lostBatches += lost;

    const evs = env.events;
    const have = Array.isArray(evs) && evs.length > 0;

    if (have) {
      /* One shared meta object per batch keeps per-event memory to seven
         typed-array slots while still letting a seek replay the batch
         context exactly. */
      const meta = {
        game: env.game === undefined || env.game === null ? null : env.game,
        locked: !!env.pointer_locked,
        sens: null,
        cx: env.cursor_x === undefined || env.cursor_x === null ? null : +env.cursor_x,
        cy: env.cursor_y === undefined || env.cursor_y === null ? null : +env.cursor_y,
        sw: env.screen_w | 0,
        sh: env.screen_h | 0,
      };
      const mi = this.metaBase + this.metas.length;
      this.metas.push(meta);

      const cols = this.ev;
      let tFirst = 0, tLast = 0;
      for (let i = 0; i < evs.length; i++) {
        const e = evs[i];
        const t = this.qpcToT(e.ts_qpc);
        if (i === 0) tFirst = t;
        tLast = t;
        /* `| 0` is doing real work here: buttons/wheel/wheel_h are omitted
           from the JSON when zero, so these are routinely undefined. */
        cols.push(t, e.dx | 0, e.dy | 0, e.buttons | 0, e.wheel | 0, e.wheel_h | 0, mi);
        this.noteTime(t);
      }
      /* How much time one batch covers (≈ the agent's batch window while the
         mouse is moving). The live buffer is floored at this so a longer
         window on the agent can never turn into a stutter here; it decays
         so a quiet feed does not pin the floor at a stale value. */
      const span = Math.max(0, tLast - tFirst);
      this.batchSpan = Math.max(span, this.batchSpan * 0.9);
    }

    if (drops > 0 || lost > 0) {
      const t = have ? this.qpcToT(evs[0].ts_qpc) : this.tEnd;
      this.dropMarks.push({ t: t, drops: drops, lost: lost });
      if (this.dropMarks.length > MAX_DROP_MARKS) this.dropMarks.shift();
      ui.onDataLoss(drops, lost);
    }

    /* Hard cap, applied at ingest rather than at consume: a backgrounded tab
       stops running rAF but keeps receiving ~1000 events/s, and a trim that
       only the render loop can reach is a trim that never runs. */
    if (ui.mode === "live" && this.ev.n > HARD_EVENT_CAP) {
      this.dropFront(this.ev.n - MAX_LIVE_EVENTS);
    }
  },

  /** Discard the oldest `k` events and everything indexed by them. */
  dropFront(k) {
    if (k <= 0) return;
    k = Math.min(k, this.ev.n);
    this.ev.dropFront(k);
    this.cursor = Math.max(0, this.cursor - k);
    if (this.ev.n > 0) {
      const firstMi = this.ev.mi[0];
      const gone = firstMi - this.metaBase;
      if (gone > 0) { this.metas.splice(0, gone); this.metaBase = firstMi; }
    } else {
      this.metaBase += this.metas.length;
      this.metas.length = 0;
    }
    /* Checkpoints address events by absolute index; shifting the columns
       invalidates every one of them. (Live mode never builds them.) */
    if (this.checkpoints.length) this.checkpoints.length = 0;
    this.cpBuilt = 0;
  },

  qpcToT(qpc) {
    if (this.anchorQpc === null) {
      /* No session envelope yet (a browser that joined a stream with no
         cached session): anchor on the first thing we see so the timeline
         is still monotonic and starts near zero. */
      this.anchorQpc = +qpc;
    }
    return (+qpc - this.anchorQpc) / this.qpcFreq;
  },

  noteTime(t) {
    if (!this.haveTimeline) { this.tStart = t; this.tEnd = t; this.haveTimeline = true; }
    else { if (t < this.tStart) this.tStart = t; if (t > this.tEnd) this.tEnd = t; }
  },

  get duration() { return Math.max(0, this.tEnd - this.tStart); },

  /** The live buffer actually applied: the user's/OBS setting, but never
      less than one batch span plus a frame of slack — the play head must
      not reach the end of a batch before the next one can have arrived. */
  effectiveLiveBuffer() {
    return Math.max(ui.liveBuffer, Math.min(0.2, this.batchSpan + 0.01));
  },

  /** UTC µs the newest received event was captured at, or null if the
      session anchor is unknown. */
  newestUtcUs() {
    if (this.anchorUtcUs === null || !this.haveTimeline) return null;
    return this.anchorUtcUs + this.tEnd * 1e6;
  },

  /* ---------------- checkpoints ---------------- */

  /** Continue building integrator checkpoints up to the end of the loaded
      timeline. Called in slices during a streaming load, so a 100MB session
      never blocks the event loop for more than one slice. */
  extendCheckpoints() {
    const ev = this.ev;
    if (this.cpBuilt === 0) {
      this.rewind();
      this.checkpoints.length = 0;
      this._cpNextT = this.haveTimeline ? this.tStart : 0;
    }
    const savedPlayT = this.playT;
    /* A play head "infinitely far in the future" makes every event
       invisible, so this pass integrates without touching the trails. */
    this.playT = 1e18;
    while (this.cpBuilt < ev.n) {
      const i = this.cpBuilt;
      if (ev.t[i] >= this._cpNextT) {
        this.checkpoints.push(this.snapshotAt(i));
        this._cpNextT = ev.t[i] + CHECKPOINT_SEC;
      }
      this.applyIdx(i);
      this.cpBuilt = i + 1;
    }
    this.playT = savedPlayT;
  },

  snapshotAt(i) {
    return {
      i: i, t: this.ev.t[i],
      deskX: this.deskX, deskY: this.deskY,
      yawRaw: this.yawRaw, pitch: this.pitch,
      lastEventT: this.lastEventT,
      totalCm: this.totalCm, totalDeg: this.totalDeg, totalClicks: this.totalClicks,
      held: this.held,
      vrefDesk: this.vrefDesk, vrefAim: this.vrefAim,
      clickRate: this.clickRate.save(), eventRate: this.eventRate.save(),
    };
  },

  restore(cp) {
    this.cursor = cp.i;
    this.deskX = cp.deskX; this.deskY = cp.deskY;
    this.yawRaw = cp.yawRaw; this.pitch = cp.pitch;
    this.lastEventT = cp.lastEventT;
    this.totalCm = cp.totalCm; this.totalDeg = cp.totalDeg;
    this.totalClicks = cp.totalClicks;
    this.held = cp.held | 0;
    this.vrefDesk = cp.vrefDesk; this.vrefAim = cp.vrefAim;
    this.clickRate.load(cp.clickRate);
    this.eventRate.load(cp.eventRate);
    this.deskTrail.clear(); this.aimTrail.clear();
    this.rings.clear(); this.wheelTicks.clear();
    this.speedCm = 0; this.speedDeg = 0;
    this.markerCursor = 0;
    while (this.markerCursor < this.markers.length &&
           this.markers[this.markerCursor].t <= cp.t) this.markerCursor++;
    this.dirty = true;
  },

  /** Latest checkpoint far enough before `target` that replaying from it
      still rebuilds the whole visible trail. Binary search: a scrub drag
      hits this once per pointer event. */
  pickCheckpoint(target) {
    const want = target - this.decay - 0.1;
    const cps = this.checkpoints;
    let lo = 0, hi = cps.length - 1, best = null;
    while (lo <= hi) {
      const m = (lo + hi) >> 1;
      if (cps[m].t <= want) { best = cps[m]; lo = m + 1; }
      else hi = m - 1;
    }
    return best;
  },

  /* ---------------- play head ---------------- */

  /** Live: track the newest event with a small buffer so batches (25–50ms
      of events each) are spread smoothly across frames instead of landing
      as one jump. */
  advanceLive(wallSec, dt) {
    if (!this.haveTimeline) return;
    if (!this.clockStarted) {
      this.playT = this.tStart;
      this.clockStarted = true;
      return;
    }
    const target = this.tEnd - this.effectiveLiveBuffer();
    const err = target - this.playT;
    if (err > 0.75) {
      /* Way behind — a stall just ended, the tab was hidden, or the stream
         jumped forward. Snapping forward is safe; snapping *backwards* never
         is (it would put trail points in the future and blank the panels), so
         the play head is strictly monotonic and only ever trimmed by rate.
         The backlog this exposes is drained over several frames by
         consume()'s per-tick budget rather than in one long frame. */
      this.playT = target;
      return;
    }
    const rate = err >= 0
      ? clamp(1 + err * 0.8, 1, 2.2)   // behind: catch up, at most 2.2×
      : clamp(1 + err * 0.8, 0.9, 1);  // ahead (idle feed): coast back gently
    /* While the feed is idle the head keeps moving so the trail fades out,
       but it may not run away: cap it just past the point where the whole
       trail has decayed, so a resumed feed is one small snap away. */
    this.playT = Math.min(this.playT + dt * rate, this.tEnd + this.decay + 0.5);
  },

  /** Move the play head to `target`, integrating exactly the events between.
      Forward moves continue from the current state; backward moves restore
      the nearest checkpoint first. Neither one re-integrates from event
      zero, which is what made a scrub drag O(session) per pointer event. */
  seek(target) {
    const forward = target >= this.playT;
    const cp = this.pickCheckpoint(target);
    if (!forward) {
      if (cp) this.restore(cp);
      else this.rewind();
    } else if (cp && cp.i > this.cursor) {
      /* Forward, but far enough that the checkpoint skips work. */
      this.restore(cp);
    }
    this.playT = target;
    this._quiet = true;
    this.consume(Infinity);
    this._quiet = false;
    /* Markers before the seek point are considered already shown. */
    this.markerCursor = 0;
    while (this.markerCursor < this.markers.length &&
           this.markers[this.markerCursor].t <= target) this.markerCursor++;
    this.dirty = true;
  },

  /** Advance the play head to `nowSeconds` and apply every event it passed. */
  tick(nowSec) {
    const dt = this.lastWall ? clamp(nowSec - this.lastWall, 0, 0.25) : 0;
    this.lastWall = nowSec;
    if (this.loading) return this;

    if (ui.mode === "live") {
      this.advanceLive(nowSec, dt);
    } else if (ui.replayPlaying) {
      this.playT = Math.min(this.tEnd, this.playT + dt * ui.replaySpeed);
      if (this.playT >= this.tEnd) ui.setReplayPlaying(false);
    }

    this._tickCm = 0;
    this._tickDeg = 0;
    this.consume(MAX_CONSUME_PER_TICK);

    /* ~100ms smoothing on the readouts. */
    if (dt > 0) {
      const a = 1 - Math.exp(-dt / 0.1);
      this.speedCm += (this._tickCm / dt - this.speedCm) * a;
      this.speedDeg += (this._tickDeg / dt - this.speedDeg) * a;
      /* Colour reference decays slowly so a single flick does not wash the
         palette out for the rest of the session. */
      const k = Math.pow(0.5, dt / 8);
      this.vrefDesk = Math.max(25, this.vrefDesk * k);
      this.vrefAim = Math.max(250, this.vrefAim * k);
    }

    this.trim();
    return this;
  },

  consume(budget) {
    const ev = this.ev;
    const t = ev.t;
    let left = budget === undefined ? MAX_CONSUME_PER_TICK : budget;
    let applied = 0;
    while (this.cursor < ev.n && t[this.cursor] <= this.playT && left > 0) {
      this.applyIdx(this.cursor++);
      applied++;
      left--;
    }
    if (applied > 0) this.dirty = true;
    while (this.markerCursor < this.markers.length &&
           this.markers[this.markerCursor].t <= this.playT) {
      const m = this.markers[this.markerCursor++];
      if (!this._quiet && m.t >= this.playT - 1.0) ui.onMarker(m);
    }
    return applied;
  },

  applyIdx(i) {
    const ev = this.ev;
    const meta = this.metaAt(i);
    if (meta) {
      if (meta.sens === null) meta.sens = this.sensFor(meta.game);
      this.sens = meta.sens;
      this.game = meta.game;
      this.pointerLocked = meta.locked;
      this.cursorX = meta.cx;
      this.cursorY = meta.cy;
      this.screenW = meta.sw;
      this.screenH = meta.sh;
    }
    const et = ev.t[i];
    const dt = this.lastEventT === null ? 0 : et - this.lastEventT;
    this.lastEventT = et;

    /* Trail/effect gating: during a seek fast-forward we still integrate
       every event (positions must be exact) but only the recent tail is
       worth drawing. */
    const visible = et >= this.playT - this.decay - 0.05;
    const fresh = et >= this.playT - RING_LIFE;

    this.eventRate.add(et, 1);

    const edx = ev.dx[i], edy = ev.dy[i];
    if (edx !== 0 || edy !== 0) {
      const s = this.sens;
      const dxcm = edx / this.cpi * CM_PER_INCH;
      const dycm = edy / this.cpi * CM_PER_INCH;
      const dcm = Math.hypot(dxcm, dycm);
      this.deskX += dxcm;
      this.deskY += dycm;
      this.totalCm += dcm;
      this._tickCm += dcm;

      const dyaw = edx * s.sens * s.yaw_coeff;
      const dpitch = edy * s.sens * s.pitch_coeff;
      const ddeg = Math.hypot(dyaw, dpitch);
      this.yawRaw += dyaw;
      this.pitch = clamp(this.pitch + dpitch, -PITCH_LIMIT, PITCH_LIMIT);
      this.totalDeg += ddeg;
      this._tickDeg += ddeg;

      const vCm = dt > 0 ? dcm / dt : 0;
      const vDeg = dt > 0 ? ddeg / dt : 0;
      if (vCm > this.vrefDesk) this.vrefDesk = vCm;
      if (vDeg > this.vrefAim) this.vrefAim = vDeg;

      /* Yaw is kept unwrapped in panel space: the head glides across the
         ±180° seam (drawn as a dashed line) instead of teleporting to the
         far edge, and the camera follows it continuously. */
      if (visible) {
        this.deskTrail.push(this.deskX, this.deskY, et, vCm, false);
        this.aimTrail.push(this.yawRaw, this.pitch, et, vDeg, false);
      }
    }

    const b = ev.b[i];
    if (b & (BTN.ANY_DOWN | BTN.ANY_UP)) {
      /* Press: one expanding ring (the click flash) plus the button joins
         `held`, which draws a steady ring at the head until its release. */
      this.held = (this.held | (b & BTN.ANY_DOWN)) & ~((b & BTN.ANY_UP) >> 1);
      this.dirty = true;
      if (b & BTN.ANY_DOWN) {
        this.totalClicks++;
        this.clickRate.add(et, 1);
        if (fresh) {
          const kind = (b & BTN.LEFT_DOWN) ? 0 : (b & BTN.RIGHT_DOWN) ? 1 : 2;
          this.rings.push({
            t: et, kind: kind,
            dx: this.deskX, dy: this.deskY,
            ax: this.yawRaw, ay: this.pitch,
          });
        }
      }
    }

    if (fresh) {
      const w = ev.w[i], wh = ev.wh[i];
      if (w !== 0) this.wheelTicks.push({ t: et, dir: w > 0 ? 1 : -1, axis: 0 });
      if (wh !== 0) this.wheelTicks.push({ t: et, dir: wh > 0 ? 1 : -1, axis: 1 });
    }
  },

  /** Bound memory: drop consumed events in live mode and expire visuals.
      Reachable from the render loop *and* from a timer, so a hidden tab
      still reclaims. */
  trim() {
    if (ui.mode === "live") {
      if (this.cursor >= LIVE_TRIM_TRIGGER) {
        this.dropFront(this.cursor - MAX_LIVE_EVENTS);
      }
      if (this.ev.n > HARD_EVENT_CAP) this.dropFront(this.ev.n - MAX_LIVE_EVENTS);
      if (this.markerCursor > 256) {
        this.markers.splice(0, this.markerCursor);
        this.markerCursor = 0;
      }
      if (this.dropMarks.length > MAX_DROP_MARKS) {
        this.dropMarks.splice(0, this.dropMarks.length - MAX_DROP_MARKS);
      }
    }
    const cutoff = this.playT - this.decay;
    this.deskTrail.trim(cutoff);
    this.aimTrail.trim(cutoff);
    this.rings.trim(this.playT - RING_LIFE);
    this.wheelTicks.trim(this.playT - WHEEL_LIFE);
  },

  /** True while anything on screen is still animating. */
  visualsAlive() {
    return this.deskTrail.n > 0 || this.aimTrail.n > 0 ||
           this.rings.n > 0 || this.wheelTicks.n > 0;
  },

  draw() {
    deskPanel.render(this);
    aimPanel.render(this);
  },
};

/* =====================================================================
   Panel rendering
   ===================================================================== */

class Panel {
  constructor(canvasId, opts) {
    this.canvas = document.getElementById(canvasId);
    /* alpha:false lets the compositor skip a blend of the whole canvas every
       frame; desynchronized lets it skip a frame of latency getting there.
       Both mean the canvas is opaque, so the background is painted, not
       cleared. The one exception is an OBS overlay with a see-through
       background, which needs the alpha channel and pays for it. */
    this.opaque = !(OBS && OBS.bg.alpha < 1);
    this.bg = OBS ? OBS.bg : { css: PANEL_BG, alpha: 1 };
    this.hidden = false;
    this.ctx = this.canvas.getContext("2d", { alpha: !this.opaque, desynchronized: true });
    this.unit = opts.unit;             // "cm" | "deg"
    this.minHalf = opts.minHalf;       // smallest half-extent, in data units
    this.subEl = document.getElementById(opts.subId);
    this.hintEl = document.getElementById(opts.hintId);
    this.flashEl = document.getElementById(opts.flashId);
    this.cx = 0; this.cy = 0; this.scale = 20;
    this.w = 0; this.h = 0; this.dpr = 1;
    this.buckets = [];
    for (let i = 0; i < VLEVELS * ALEVELS; i++) this.buckets.push([]);
    this.flashUntil = -1;
    this.resize();
    new ResizeObserver(() => this.resize()).observe(this.canvas.parentElement);
  }

  resize() {
    const r = this.canvas.getBoundingClientRect();
    /* A panel the layout hid (OBS `layout=desk` / `aim`) measures 0×0;
       render() skips it entirely rather than drawing into a 1px canvas. */
    this.hidden = r.width < 1 || r.height < 1;
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.w = Math.max(1, Math.round(r.width));
    this.h = Math.max(1, Math.round(r.height));
    this.canvas.width = Math.round(this.w * this.dpr);
    this.canvas.height = Math.round(this.h * this.dpr);
    /* A resized canvas is a blank canvas: force a repaint even if the
       engine is idle. */
    engine.dirty = true;
  }

  toX(x) { return this.w / 2 + (x - this.cx) * this.scale; }
  toY(y) { return this.h / 2 + (y - this.cy) * this.scale; }

  /** Follow the head and keep the visible trail framed, both damped so the
      view never jitters at 1kHz. */
  autoScale(head, trail) {
    const tcx = head.x, tcy = head.y;
    this.cx += (tcx - this.cx) * 0.12;
    this.cy += (tcy - this.cy) * 0.12;

    let hx = this.minHalf, hy = this.minHalf;
    const cap = trail.cap;
    const step = Math.max(1, Math.floor(trail.n / 900));
    for (let k = 0; k < trail.n; k += step) {
      const i = (trail.head + k) % cap;
      const ax = Math.abs(trail.x[i] - this.cx), ay = Math.abs(trail.y[i] - this.cy);
      if (ax > hx) hx = ax;
      if (ay > hy) hy = ay;
    }
    hx = Math.max(hx, this.minHalf);
    hy = Math.max(hy, this.minHalf);
    const target = Math.min(
      (this.w * 0.44) / hx,
      (this.h * 0.44) / hy
    );
    if (!isFinite(target) || target <= 0) return;
    const ratio = target / this.scale;
    if (ratio > 4 || ratio < 0.25) this.scale = target;
    else this.scale += (target - this.scale) * 0.07;
  }

  render(eng) {
    const ctx = this.ctx;
    const isDesk = this.unit === "cm";
    const trail = isDesk ? eng.deskTrail : eng.aimTrail;
    const head = isDesk
      ? { x: eng.deskX, y: eng.deskY }
      : { x: eng.yawRaw, y: eng.pitch };
    const vref = isDesk ? eng.vrefDesk : eng.vrefAim;

    if (this.hidden) return;
    this.autoScale(head, trail);

    ctx.save();
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    if (this.opaque) {
      ctx.fillStyle = this.bg.css;
      ctx.fillRect(0, 0, this.w, this.h);
    } else {
      ctx.clearRect(0, 0, this.w, this.h);
      if (this.bg.alpha > 0) {
        ctx.fillStyle = this.bg.css;
        ctx.fillRect(0, 0, this.w, this.h);
      }
    }

    if (!OBS || OBS.grid) this.drawGrid(ctx, isDesk);
    this.drawTrail(ctx, trail, eng, vref);
    this.drawRings(ctx, eng, isDesk);
    if (eng.held) this.drawHeld(ctx, eng, head);
    this.drawHead(ctx, head);
    if (isDesk) {
      this.drawWheel(ctx, eng);
      /* The desktop-cursor miniature is a debugging aid, not stream content. */
      if (!OBS) this.drawCursorGhost(ctx, eng);
    }
    if (!OBS || OBS.legend) this.drawLegend(ctx, vref, isDesk);

    ctx.restore();
  }

  drawGrid(ctx, isDesk) {
    const halfX = this.w / 2 / this.scale;
    const halfY = this.h / 2 / this.scale;
    const step = niceStep(halfX * 2 / 7);
    const stepText = step >= 1 ? step.toFixed(0) : step.toFixed(2);
    const label = "grid " + stepText + (isDesk ? " cm" : "°");
    if (this.subEl && this.subEl.textContent !== label) this.subEl.textContent = label;

    ctx.lineWidth = 1;
    const x0 = Math.floor((this.cx - halfX) / step) * step;
    const x1 = this.cx + halfX;
    const y0 = Math.floor((this.cy - halfY) / step) * step;
    const y1 = this.cy + halfY;

    ctx.beginPath();
    ctx.strokeStyle = GRID.line;
    for (let x = x0; x <= x1; x += step) {
      if (Math.abs(x) < step * 0.001) continue;
      const px = Math.round(this.toX(x)) + 0.5;
      ctx.moveTo(px, 0); ctx.lineTo(px, this.h);
    }
    for (let y = y0; y <= y1; y += step) {
      if (Math.abs(y) < step * 0.001) continue;
      const py = Math.round(this.toY(y)) + 0.5;
      ctx.moveTo(0, py); ctx.lineTo(this.w, py);
    }
    ctx.stroke();

    /* Origin axes: where "recenter" put you. On the aim panel yaw is
       unwrapped, so every full turn (k·360°) is the same facing and gets
       the axis too. */
    ctx.beginPath();
    ctx.strokeStyle = GRID.axis;
    const oy = Math.round(this.toY(0)) + 0.5;
    ctx.moveTo(0, oy); ctx.lineTo(this.w, oy);
    if (isDesk) {
      const ox = Math.round(this.toX(0)) + 0.5;
      ctx.moveTo(ox, 0); ctx.lineTo(ox, this.h);
    } else {
      for (let x = Math.ceil((this.cx - halfX) / 360) * 360; x <= x1; x += 360) {
        const px = Math.round(this.toX(x)) + 0.5;
        ctx.moveTo(px, 0); ctx.lineTo(px, this.h);
      }
    }
    ctx.stroke();

    if (!isDesk) {
      /* Yaw seams: the ±180° boundary of each turn, i.e. every odd multiple
         of 180°. Crossing one means you are now facing directly behind
         where "recenter" left you. */
      ctx.beginPath();
      ctx.strokeStyle = GRID.wrap;
      ctx.setLineDash([4, 5]);
      for (let b = (Math.floor((this.cx - halfX - 180) / 360) * 360) + 180; b <= x1; b += 360) {
        const px = Math.round(this.toX(b)) + 0.5;
        if (px > -10 && px < this.w + 10) { ctx.moveTo(px, 0); ctx.lineTo(px, this.h); }
      }
      ctx.stroke();
      ctx.setLineDash([]);
    }
  }

  /** Batched polyline rendering: segments are quantised into velocity ×
      age buckets so a 3-second 1kHz trail costs ~100 stroke calls, not
      3000. Reads straight out of the ring's typed arrays. */
  drawTrail(ctx, trail, eng, vref) {
    if (trail.n < 2) return;
    const buckets = this.buckets;
    for (let i = 0; i < buckets.length; i++) buckets[i].length = 0;

    const now = eng.playT;
    const decay = eng.decay;
    const inv = 1 / (vref || 1);
    const cap = trail.cap;
    const TX = trail.x, TY = trail.y, TT = trail.t, TV = trail.v, TB = trail.brk;

    let idx = trail.head;
    let px = this.toX(TX[idx]), py = this.toY(TY[idx]);
    for (let k = 1; k < trail.n; k++) {
      idx = idx + 1 === cap ? 0 : idx + 1;
      const x = this.toX(TX[idx]), y = this.toY(TY[idx]);
      if (!TB[idx]) {
        const age = now - TT[idx];
        if (age <= decay && age >= 0) {
          const vi = clamp(Math.round(Math.sqrt(clamp(TV[idx] * inv, 0, 1)) * (VLEVELS - 1)), 0, VLEVELS - 1);
          const ai = clamp(Math.round((1 - age / decay) * (ALEVELS - 1)), 0, ALEVELS - 1);
          const b = buckets[vi * ALEVELS + ai];
          b.push(px, py, x, y);
        }
      }
      px = x; py = y;
    }

    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    for (let vi = 0; vi < VLEVELS; vi++) {
      for (let ai = 0; ai < ALEVELS; ai++) {
        const b = buckets[vi * ALEVELS + ai];
        if (!b.length) continue;
        const fade = (ai + 1) / ALEVELS;
        ctx.globalAlpha = 0.10 + 0.85 * fade * fade;
        ctx.strokeStyle = RAMP_CSS[vi];
        ctx.lineWidth = (1.1 + 1.5 * (vi / (VLEVELS - 1)) * fade) * S;
        ctx.beginPath();
        for (let k = 0; k < b.length; k += 4) {
          ctx.moveTo(b[k], b[k + 1]);
          ctx.lineTo(b[k + 2], b[k + 3]);
        }
        ctx.stroke();
      }
    }
    ctx.globalAlpha = 1;
  }

  /** Steady ring per held button, riding on the head: the same look as a
      click ring frozen early in its life, one size step per extra button. */
  drawHeld(ctx, eng, head) {
    const x = this.toX(head.x), y = this.toY(head.y);
    let n = 0;
    for (let i = 0; i < BTN_KIND.length; i++) {
      if (!(eng.held & BTN_KIND[i][0])) continue;
      const col = RING_COLORS[BTN_KIND[i][1]];
      const rad = (13 + 6 * n) * S;
      n++;
      ctx.fillStyle = "rgba(" + col + ",0.22)";
      ctx.beginPath(); ctx.arc(x, y, rad * 0.6, 0, Math.PI * 2); ctx.fill();
      ctx.strokeStyle = "rgba(" + col + ",0.9)";
      ctx.lineWidth = 2.2 * S;
      ctx.beginPath(); ctx.arc(x, y, rad, 0, Math.PI * 2); ctx.stroke();
    }
  }

  drawRings(ctx, eng, isDesk) {
    const colors = RING_COLORS;
    const now = eng.playT;
    for (let k = 0; k < eng.rings.n; k++) {
      const r = eng.rings.at(k);
      const age = now - r.t;
      if (age < 0 || age > RING_LIFE) continue;
      const f = age / RING_LIFE;
      const x = this.toX(isDesk ? r.dx : r.ax);
      const y = this.toY(isDesk ? r.dy : r.ay);
      const rad = (4 + 34 * (1 - Math.pow(1 - f, 2.2))) * S;
      const col = colors[r.kind];
      ctx.strokeStyle = "rgba(" + col + "," + (0.9 * (1 - f)).toFixed(3) + ")";
      ctx.lineWidth = (2.2 * (1 - f) + 0.5) * S;
      ctx.beginPath();
      ctx.arc(x, y, rad, 0, Math.PI * 2);
      ctx.stroke();
      if (f < 0.35) {
        ctx.fillStyle = "rgba(" + col + "," + (0.28 * (1 - f / 0.35)).toFixed(3) + ")";
        ctx.beginPath();
        ctx.arc(x, y, rad * 0.6, 0, Math.PI * 2);
        ctx.fill();
      }
    }
  }

  drawHead(ctx, head) {
    const x = this.toX(head.x), y = this.toY(head.y);
    ctx.fillStyle = "rgba(255,255,255,0.10)";
    ctx.beginPath(); ctx.arc(x, y, 9 * S, 0, Math.PI * 2); ctx.fill();
    ctx.fillStyle = "#ffffff";
    ctx.beginPath(); ctx.arc(x, y, 2.6 * S, 0, Math.PI * 2); ctx.fill();
    ctx.strokeStyle = "rgba(53,208,224,0.55)";
    ctx.lineWidth = S;
    ctx.beginPath();
    ctx.moveTo(x - 12 * S, y); ctx.lineTo(x - 5 * S, y);
    ctx.moveTo(x + 5 * S, y); ctx.lineTo(x + 12 * S, y);
    ctx.moveTo(x, y - 12 * S); ctx.lineTo(x, y - 5 * S);
    ctx.moveTo(x, y + 5 * S); ctx.lineTo(x, y + 12 * S);
    ctx.stroke();
  }

  /** Vertical wheel ticks, plus horizontal (tilt) ticks beneath them when
      the mouse reports `wheel_h`. */
  drawWheel(ctx, eng) {
    const now = eng.playT;
    const bx = this.w - 26;
    const by = this.h / 2;
    const hy = Math.min(this.h - 12, by + 70);   // tilt bar baseline
    const hx = this.w - 46;

    ctx.strokeStyle = "#1d2634";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(bx, by - 46); ctx.lineTo(bx, by + 46);
    ctx.stroke();

    let net = 0, netH = 0, sawH = false;
    for (let k = 0; k < eng.wheelTicks.n; k++) {
      const t = eng.wheelTicks.at(k);
      const age = now - t.t;
      if (age < 0 || age > WHEEL_LIFE) continue;
      const f = age / WHEEL_LIFE;
      const alpha = (0.85 * (1 - f)).toFixed(3);
      if (t.axis === 1) {
        sawH = true;
        netH += t.dir;
        const x = hx + t.dir * (8 + 26 * f);
        ctx.strokeStyle = "rgba(124,140,255," + alpha + ")";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.moveTo(x, hy - 6); ctx.lineTo(x, hy + 6);
        ctx.stroke();
      } else {
        net += t.dir;
        const y = by - t.dir * (8 + 38 * f);
        ctx.strokeStyle = "rgba(53,208,224," + alpha + ")";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.moveTo(bx - 7, y); ctx.lineTo(bx + 7, y);
        ctx.stroke();
      }
    }

    ctx.font = "10px ui-monospace, monospace";
    ctx.textAlign = "center";
    if (net !== 0) {
      ctx.fillStyle = "#7c8ba1";
      ctx.fillText((net > 0 ? "+" : "") + net, bx, by + 62);
    }
    if (sawH) {
      /* The tilt axis only draws its rail once the mouse has actually used
         it — most do not have one. */
      ctx.strokeStyle = "#1d2634";
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(hx - 34, hy); ctx.lineTo(hx + 34, hy);
      ctx.stroke();
      if (netH !== 0) {
        ctx.fillStyle = "#7c8cff";
        ctx.fillText((netH > 0 ? "+" : "") + netH + " ⇄", hx, hy + 18);
      }
    }
    ctx.textAlign = "left";
  }

  /** Desktop mode only: a miniature of the screen with the sampled cursor
      position on it. Costs two rects and a dot, and immediately shows when
      the desk-space integration has drifted away from where the pointer
      actually is. */
  drawCursorGhost(ctx, eng) {
    if (eng.pointerLocked) return;
    if (eng.cursorX === null || eng.cursorY === null) return;
    if (!(eng.screenW > 0) || !(eng.screenH > 0)) return;

    const gw = 100;
    const gh = Math.max(24, Math.round(gw * eng.screenH / eng.screenW));
    const x = 12;
    const y = this.h - 32 - gh;
    if (y < 30) return;   // panel too short to be worth the clutter

    ctx.save();
    ctx.globalAlpha = 0.5;
    ctx.fillStyle = "#0b1017";
    ctx.fillRect(x, y, gw, gh);
    ctx.strokeStyle = "#25303f";
    ctx.lineWidth = 1;
    ctx.strokeRect(x + 0.5, y + 0.5, gw - 1, gh - 1);

    const px = x + clamp(eng.cursorX / eng.screenW, 0, 1) * gw;
    const py = y + clamp(eng.cursorY / eng.screenH, 0, 1) * gh;
    ctx.globalAlpha = 0.85;
    ctx.fillStyle = "#35d0e0";
    ctx.beginPath();
    ctx.arc(px, py, 2.4, 0, Math.PI * 2);
    ctx.fill();

    ctx.globalAlpha = 0.6;
    ctx.fillStyle = "#4d5a6d";
    ctx.font = "9px ui-monospace, monospace";
    ctx.fillText("desktop cursor " + eng.screenW + "×" + eng.screenH, x, y - 4);
    ctx.restore();
  }

  drawLegend(ctx, vref, isDesk) {
    const x = 12, y = this.h - 22 * S, w = 108 * S, h = 6 * S;
    for (let i = 0; i < VLEVELS; i++) {
      ctx.fillStyle = RAMP_CSS[i];
      ctx.fillRect(x + (w / VLEVELS) * i, y, w / VLEVELS + 0.6, h);
    }
    ctx.fillStyle = OBS ? "#c8d2df" : "#4d5a6d";
    ctx.font = Math.round(10 * S) + "px ui-monospace, monospace";
    ctx.fillText("0", x, y + 16 * S);
    const top = isDesk
      ? Math.round(vref) + " cm/s"
      : Math.round(vref) + "°/s";
    ctx.fillText(top, x + w - ctx.measureText(top).width, y + 16 * S);
  }

  flash() {
    if (!this.flashEl) return;
    const el = this.flashEl;
    el.style.transition = "none";
    el.style.opacity = "0.85";
    requestAnimationFrame(() => {
      el.style.transition = "opacity .7s ease-out";
      el.style.opacity = "0";
    });
  }
}

const deskPanel = new Panel("deskCanvas", {
  unit: "cm", minHalf: 1.2, subId: "deskSub", hintId: "deskHint", flashId: "deskFlash",
});
const aimPanel = new Panel("aimCanvas", {
  unit: "deg", minHalf: 6, subId: "aimSub", hintId: "aimHint", flashId: "aimFlash",
});

/* =====================================================================
   UI: mode switching, live socket, replay transport, stats
   ===================================================================== */

const $ = (id) => document.getElementById(id);

const LIVE_BUFFER_DEFAULT = 0.035;
const LIVE_BUFFER_KEY = "telemouse.liveBuffer";

const ui = {
  mode: "live",
  paused: false,
  replayPlaying: false,
  replaySpeed: 1,
  scrubbing: false,
  ws: null,
  wsRetry: 0,
  wsTimer: null,
  loadedSession: null,
  loadToken: 0,
  sessions: [],
  /* "go to time" targets: one waiting for the session list, one waiting
     for a recording to finish streaming in. Wall-clock UTC µs or null. */
  pendingUtcUs: null,
  seekAfterLoadUtcUs: null,
  _wallSec: null,
  liveBuffer: LIVE_BUFFER_DEFAULT,
  bridge: null,
  bridgeAt: 0,
  lossToastShown: false,

  setConn(cls, text) {
    const p = $("connPill");
    p.className = "pill" + (cls ? " " + cls : "");
    $("connText").textContent = text;
  },

  setMode(mode) {
    if (mode === this.mode) return;
    this.mode = mode;
    for (const b of $("modeSwitch").children) b.classList.toggle("on", b.dataset.mode === mode);
    $("transport").classList.toggle("hidden", mode !== "replay");
    $("bufCtl").style.display = mode === "live" ? "" : "none";
    this.paused = false;
    this.lossToastShown = false;
    $("btnPause").classList.remove("on");
    /* Cancel any replay load still streaming, and hand the integrator back
       to the render loop. */
    this.loadToken++;
    engine.reset();
    engine.loading = false;
    this.renderNotches();
    if (mode === "live") {
      this.setReplayPlaying(false);
      this.connect();
      $("btnPause").firstChild.nodeValue = "Pause";
    } else {
      this.disconnect();
      this.setConn("", "replay");
      $("btnPause").firstChild.nodeValue = "Pause";
      this.loadSessionList();
    }
    this.renderTransport();
    engine.dirty = true;
  },

  setLiveBuffer(seconds) {
    this.liveBuffer = clamp(seconds, 0.01, 0.2);
    $("buf").value = String(Math.round(this.liveBuffer * 1000));
    $("bufVal").textContent = Math.round(this.liveBuffer * 1000) + "ms";
    /* An OBS source's buffer comes from its URL/config; it must not
       overwrite the dashboard's remembered choice (same origin). */
    if (OBS) return;
    try { localStorage.setItem(LIVE_BUFFER_KEY, String(this.liveBuffer)); } catch (e) { /* private mode */ }
  },

  /* ---------------- live ---------------- */

  connect() {
    this.disconnect();
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = proto + "//" + location.host + "/ws";
    this.setConn("warn", "connecting");
    let ws;
    try { ws = new WebSocket(url); } catch (e) { this.scheduleReconnect(); return; }
    this.ws = ws;
    ws.onopen = () => {
      this.wsRetry = 0;
      this.setConn("ok", "live");
    };
    ws.onmessage = (m) => {
      let env;
      try { env = JSON.parse(m.data); } catch (e) { return; }
      ingest(env);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      if (this.mode === "live") { this.setConn("err", "disconnected"); this.scheduleReconnect(); }
    };
    ws.onerror = () => { /* onclose follows */ };
  },

  disconnect() {
    clearTimeout(this.wsTimer);
    if (this.ws) { const w = this.ws; this.ws = null; try { w.close(); } catch (e) {} }
  },

  scheduleReconnect() {
    if (this.mode !== "live") return;
    const delay = Math.min(8000, 400 * Math.pow(1.7, this.wsRetry++));
    this.setConn("err", "retry in " + Math.round(delay / 100) / 10 + "s");
    clearTimeout(this.wsTimer);
    this.wsTimer = setTimeout(() => this.connect(), delay);
  },

  /* ---------------- replay ---------------- */

  async loadSessionList() {
    const sel = $("sessionSel");
    sel.innerHTML = "<option value=''>— loading —</option>";
    let list = [];
    try {
      const r = await fetch("/api/sessions");
      list = await r.json();
    } catch (e) { list = []; }
    if (!Array.isArray(list) || !list.length) {
      sel.innerHTML = "<option value=''>no recordings found</option>";
      this.setConn("warn", "no recordings");
      return;
    }
    sel.innerHTML = "";
    this.sessions = list;
    for (const s of list) {
      const o = document.createElement("option");
      o.value = s.id;
      const kb = (s.bytes / 1024).toFixed(0);
      /* Prefer the recording's own clock over the file's mtime: the id and
         the anchor are UTC, the user thinks in local time. */
      const startMs = s.started_utc_us != null ? s.started_utc_us / 1000 : s.modified_epoch_ms;
      const when = startMs ? new Date(startMs).toLocaleString() : "";
      const span = (s.started_utc_us != null && s.ended_utc_us != null)
        ? ", " + fmtTime((s.ended_utc_us - s.started_utc_us) / 1e6).replace(/\.\d+$/, "") : "";
      o.textContent = s.id + "  (" + (when ? when + span + ", " : "") + kb + " KB)";
      sel.appendChild(o);
    }
    if (this.pendingUtcUs !== null) {
      const target = this.pendingUtcUs;
      this.pendingUtcUs = null;
      if (this.gotoUtcUs(target)) return;
    }
    await this.loadSession(list[0].id);
  },

  /** When a listed recording ends. A recording still being written has a
      real end (the server reads it off the file's tail), so a missing one
      means an envelope with no batches yet: it ends where it starts. */
  sessionEnd(s) {
    return s.ended_utc_us == null ? s.started_utc_us : s.ended_utc_us;
  },

  /** Which recording was running at `utcUs`: one whose span contains it
      (1 s of slack past the last batch), else the one that ended most
      recently before it. */
  sessionAt(utcUs) {
    let best = null;
    for (const s of this.sessions || []) {
      if (s.started_utc_us == null || s.started_utc_us > utcUs) continue;
      if (utcUs <= this.sessionEnd(s) + 1e6) return s;
      if (!best || this.sessionEnd(s) > this.sessionEnd(best)) best = s;
    }
    return best;
  },

  /** Replay the moment `utcUs`: load the recording that covers it if it is
      not the loaded one, then seek there. Returns false (with a toast) when
      no recording could plausibly hold it. */
  gotoUtcUs(utcUs) {
    if (!isFinite(utcUs)) return false;
    const s = this.sessionAt(utcUs);
    if (!s) {
      this.toast("go to time", "no recording from before " + new Date(utcUs / 1000).toLocaleString(), true);
      return false;
    }
    const drift = utcUs - this.sessionEnd(s);
    if (drift > 1e6) {
      /* Nearest we can do: the end of the last recording before that time. */
      this.toast("go to time", "nothing recorded then — jumping to the end of " + s.id +
                 " (" + fmtTime(drift / 1e6).replace(/\.\d+$/, "") + " earlier)");
    }
    $("sessionSel").value = s.id;
    if (this.loadedSession === s.id && !engine.loading) {
      this.seekUtcUs(utcUs);
      return true;
    }
    this.seekAfterLoadUtcUs = utcUs;
    this.loadSession(s.id);
    return true;
  },

  /** Seek the loaded recording to wall-clock `utcUs`, clamped to its span. */
  seekUtcUs(utcUs) {
    if (engine.anchorUtcUs === null || !engine.haveTimeline) return;
    const t = clamp((utcUs - engine.anchorUtcUs) / 1e6, engine.tStart, engine.tEnd);
    engine.seek(t);
    this.renderTransport();
  },

  /** Wall-clock UTC µs of the play head, or null before a session anchor. */
  playheadUtcUs() {
    if (engine.anchorUtcUs === null || !engine.haveTimeline) return null;
    return engine.anchorUtcUs + engine.playT * 1e6;
  },

  /** Stream a recording in and parse it as it arrives.
      Sessions are hundreds of megabytes; `await response.text()` would hold
      the whole file as one string *and* a second copy as an array of lines
      before a single event existed. Reading the body incrementally keeps
      peak memory at one chunk plus the typed-array columns, and yields to
      the event loop often enough that the page stays interactive (and can
      show progress) while it loads. */
  async loadSession(id) {
    if (!id) return;
    const token = ++this.loadToken;
    this.setReplayPlaying(false);
    this.setConn("warn", "loading " + id);
    engine.reset();
    engine.loading = true;
    this.lossToastShown = false;
    this.renderNotches();

    let res;
    try {
      res = await fetch("/api/session/" + encodeURIComponent(id));
      if (!res.ok) throw new Error("http " + res.status);
    } catch (e) {
      if (token === this.loadToken) { engine.loading = false; this.setConn("err", "load failed"); }
      return;
    }

    const total = +res.headers.get("content-length") || 0;
    let bytes = 0, lines = 0, bad = 0;

    const consumeLine = (line) => {
      if (!line) return;
      try { ingest(JSON.parse(line)); } catch (e) { bad++; }
    };

    try {
      if (!res.body || !res.body.getReader) {
        /* No streams (very old browser): fall back to one big string. */
        const text = await res.text();
        for (const line of text.split("\n")) consumeLine(line.trim());
        lines = 1;
      } else {
        const reader = res.body.getReader();
        const dec = new TextDecoder();
        let carry = "";
        for (;;) {
          const chunk = await reader.read();
          if (token !== this.loadToken) { try { await reader.cancel(); } catch (e) {} return; }
          if (chunk.done) break;
          bytes += chunk.value.length;
          carry += dec.decode(chunk.value, { stream: true });
          let start = 0, nl;
          while ((nl = carry.indexOf("\n", start)) >= 0) {
            consumeLine(carry.slice(start, nl).trim());
            start = nl + 1;
            if (++lines % 20000 === 0) {
              carry = carry.slice(start);
              start = 0;
              this.setConn("warn", "loading " + id + " · " +
                lines.toLocaleString() + " lines" +
                (total ? " · " + Math.round(100 * bytes / total) + "%" : ""));
              await nextTask();
              if (token !== this.loadToken) { try { await reader.cancel(); } catch (e) {} return; }
              engine.extendCheckpoints();
            }
          }
          carry = carry.slice(start);
        }
        carry += dec.decode();
        consumeLine(carry.trim());
      }
    } catch (e) {
      if (token === this.loadToken) { engine.loading = false; this.setConn("err", "load interrupted"); }
      return;
    }

    if (token !== this.loadToken) return;

    engine.extendCheckpoints();
    engine.rewind();
    engine.loading = false;
    this.loadedSession = id;
    engine.seek(engine.tStart);
    this.setConn("ok", id + " · " + engine.ev.n.toLocaleString() + " events" +
                       (bad ? " (" + bad + " bad lines)" : ""));
    this.renderNotches();
    if (this.seekAfterLoadUtcUs !== null) {
      const target = this.seekAfterLoadUtcUs;
      this.seekAfterLoadUtcUs = null;
      this.seekUtcUs(target);
    }
    this.renderTransport();
    this.setReplayPlaying(true);
  },

  setReplayPlaying(on) {
    this.replayPlaying = !!on && this.mode === "replay";
    $("btnPlay").textContent = this.replayPlaying ? "Pause" : "Play";
    $("btnPlay").classList.toggle("on", this.replayPlaying);
  },

  /** Marker notches (amber) and data-loss notches (red) on the scrub track.
      Rebuilt only when the timeline changes, never per frame. */
  renderNotches() {
    const box = $("notches");
    box.innerHTML = "";
    const dur = engine.duration;
    if (dur <= 0) return;
    const frag = document.createDocumentFragment();
    for (const m of engine.markers) {
      const d = document.createElement("div");
      d.className = "notch";
      d.style.left = (100 * (m.t - engine.tStart) / dur) + "%";
      d.title = m.label;
      frag.appendChild(d);
    }
    for (const d0 of engine.dropMarks) {
      const d = document.createElement("div");
      d.className = "notch drop";
      d.style.left = (100 * clamp((d0.t - engine.tStart) / dur, 0, 1)) + "%";
      d.title = (d0.drops ? d0.drops + " ring drops" : "") +
                (d0.drops && d0.lost ? ", " : "") +
                (d0.lost ? d0.lost + " lost batches" : "");
      frag.appendChild(d);
    }
    box.appendChild(frag);
  },

  renderTransport() {
    if (this.mode !== "replay") return;
    const dur = engine.duration;
    const pos = clamp(engine.playT - engine.tStart, 0, dur);
    if (!this.scrubbing) {
      $("scrub").value = String(dur > 0 ? Math.round(1000 * pos / dur) : 0);
    }
    $("scrubFill").style.width = (dur > 0 ? 100 * pos / dur : 0) + "%";
    $("timeText").textContent = fmtTime(pos) + " / " + fmtTime(dur);
    /* Wall clock of the play head, refreshed once per second of timeline
       (not per frame — it is text layout, and it only changes that often). */
    const utc = this.playheadUtcUs();
    const sec = utc === null ? null : Math.floor(utc / 1e6);
    if (sec !== this._wallSec) {
      this._wallSec = sec;
      $("wallText").textContent = sec === null ? "" : fmtWallClock(sec * 1000);
    }
  },

  jumpMarker(dir) {
    if (!engine.markers.length) return;
    const cur = engine.playT;
    let best = null;
    for (const m of engine.markers) {
      if (dir > 0 && m.t > cur + 0.05 && (best === null || m.t < best)) best = m.t;
      if (dir < 0 && m.t < cur - 0.05 && (best === null || m.t > best)) best = m.t;
    }
    if (best === null) best = dir > 0 ? engine.tEnd : engine.tStart;
    engine.seek(best);
    this.renderTransport();
  },

  /* ---------------- notifications ---------------- */

  toast(label, text, bad) {
    /* Stream overlays get the panel flash for markers and nothing for the
       rest: a data-loss banner belongs on the dashboard, not on air. */
    if (OBS) return;
    const el = document.createElement("div");
    el.className = "toast" + (bad ? " bad" : "");
    el.innerHTML = "<span class='lbl'>" + escapeHtml(label) + "</span> &nbsp;" + escapeHtml(text);
    $("toasts").appendChild(el);
    setTimeout(() => { el.classList.add("out"); setTimeout(() => el.remove(), 500); }, 2600);
  },

  onMarker(m) {
    deskPanel.flash();
    aimPanel.flash();
    this.toast("marker", m.label);
  },

  /** First data loss of a live session gets a toast; after that the stat
      tiles carry it (a 1kHz feed that is dropping would otherwise produce a
      wall of toasts). */
  onDataLoss(drops, lost) {
    if (this.mode !== "live" || this.lossToastShown) return;
    this.lossToastShown = true;
    const parts = [];
    if (drops) parts.push(drops + " ring drops");
    if (lost) parts.push(lost + " lost batches");
    this.toast("data loss", parts.join(", ") + " — see the stat bar", true);
  },

  onSessionRestart(from, to) {
    this.lossToastShown = false;
    this.renderNotches();
    this.toast("session", "restarted: " + (from || "?") + " → " + (to || "?"));
  },

  onSessionChanged() {
    const n = engine.devices.length;
    const pill = $("devPill");
    pill.classList.toggle("hidden", n <= 1);
    if (n > 1) {
      $("devText").textContent = n + " devices";
      pill.title = engine.devices.join("\n");
    }
    this.refreshSensHint();
  },

  onVizStats(env) {
    this.bridge = env;
    this.bridgeAt = performance.now() / 1000;
  },

  refreshSensHint() {
    const s = engine.sens;
    const hint = s && s.fallback
      ? "no sens profile" + (engine.game ? " for " + engine.game : "") +
        " — assuming " + FALLBACK_SENS.sens.toFixed(2) + " × " + FALLBACK_SENS.yaw_coeff
      : "";
    if (aimPanel.hintEl.textContent !== hint) aimPanel.hintEl.textContent = hint;
    const dh = engine.session ? "" : "no session envelope yet — CPI assumed " + engine.cpi;
    if (deskPanel.hintEl.textContent !== dh) deskPanel.hintEl.textContent = dh;
  },
};

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** Yield to the event loop between load slices. `setTimeout(0)` rather than a
    microtask: a microtask would not let the renderer or the socket run. */
function nextTask() {
  return new Promise((r) => setTimeout(r, 0));
}

/* =====================================================================
   Profiling (?profile=1)
   ===================================================================== */

const prof = {
  tick: 0, draw: 0, stats: 0, ingest: 0,
  frames: 0, since: 0,
  text: "—",
};

function measure(name, a, b, field) {
  try {
    const m = performance.measure("tm." + name, a, b);
    prof[field] += (m && m.duration) || 0;
  } catch (e) { /* marks were cleared mid-frame; skip this sample */ }
}

function profileRoll(now) {
  if (prof.since === 0) { prof.since = now; return; }
  if (now - prof.since < 1) return;
  const f = Math.max(1, prof.frames);
  prof.text =
    "t " + (prof.tick / f).toFixed(2) +
    " d " + (prof.draw / f).toFixed(2) +
    " s " + (prof.stats / f).toFixed(2) +
    " i " + prof.ingest.toFixed(1) + "/s";
  prof.tick = prof.draw = prof.stats = prof.ingest = 0;
  prof.frames = 0;
  prof.since = now;
  performance.clearMarks();
  performance.clearMeasures();
}

/** The single ingest entry point, so profiling wraps both sources. */
function ingest(env) {
  if (!PROFILE) { engine.ingest(env); return; }
  performance.mark("ig0");
  engine.ingest(env);
  performance.mark("ig1");
  measure("ingest", "ig0", "ig1", "ingest");
}

/* =====================================================================
   Stats bar
   ===================================================================== */

let lastStatsPaint = 0;
let fpsEma = 60;
let refreshEst = 60;
let latEma = null;

function paintStats(now) {
  const e = engine;
  $("sSpeedCm").textContent = e.speedCm.toFixed(e.speedCm < 100 ? 1 : 0);
  $("sSpeedDeg").textContent = Math.round(e.speedDeg).toLocaleString();
  $("sDistM").textContent = (e.totalCm / 100).toFixed(2);
  $("sDistDeg").textContent = Math.round(e.totalDeg).toLocaleString();
  $("sCpm").textContent = Math.round(e.clickRate.total(e.playT));
  $("sClicks").textContent = e.totalClicks + " total";
  $("sEps").textContent = Math.round(e.eventRate.perSecond(e.playT)).toLocaleString();

  /* Latency and lag are both about keeping up with a live feed. In replay
     the "newest event" is hours old and the play head is wherever you put
     it, so both would read as a permanent alarm; show them as N/A instead. */
  const live = ui.mode === "live";
  const capturedUs = live ? e.newestUtcUs() : null;
  if (capturedUs === null) {
    latEma = null;
    $("sLat").textContent = "—";
    $("statLat").classList.remove("alert", "warn");
    $("statLat").classList.toggle("dim", !live);
  } else {
    $("statLat").classList.remove("dim");
    const ms = (Date.now() * 1000 - capturedUs) / 1000;
    latEma = latEma === null ? ms : latEma + (ms - latEma) * 0.25;
    $("sLat").textContent = latEma.toFixed(1);
    $("statLat").classList.toggle("alert", latEma > 25);
    $("statLat").classList.toggle("warn", latEma > 10 && latEma <= 25);
  }

  if (!live) {
    $("sLag").textContent = "—";
    $("statLag").classList.remove("alert");
    $("statLag").classList.add("dim");
  } else {
    const lagMs = e.haveTimeline ? Math.max(0, (e.tEnd - e.playT) * 1000) : 0;
    $("sLag").textContent = Math.round(lagMs).toLocaleString();
    $("statLag").classList.remove("dim");
    $("statLag").classList.toggle("alert", lagMs > 250);
  }

  $("sFps").textContent = Math.round(fpsEma);
  $("sFpsRef").textContent = "/ ~" + Math.round(refreshEst) + "Hz";
  $("statFps").classList.toggle("alert", fpsEma < 0.8 * refreshEst);

  $("sDrops").textContent = e.drops.toLocaleString();
  $("statDrops").classList.toggle("alert", e.drops > 0);
  $("sLost").textContent = e.lostBatches.toLocaleString();
  $("statLost").classList.toggle("alert", e.lostBatches > 0);
  $("sAbs").textContent = e.absFrames.toLocaleString();
  $("statAbs").classList.toggle("dim", e.absFrames === 0);

  paintBridge(now);

  $("sLocked").textContent = e.pointerLocked ? "locked" : "desktop";
  const s = e.sens;
  $("sCpi").textContent = Math.round(e.cpi) + " / " +
    (s ? s.sens.toFixed(2) : "—") + (s && s.fallback ? "*" : "");
  $("sCpi").title = s
    ? "cpi " + e.cpi + ", sens " + s.sens + ", yaw " + s.yaw_coeff + "°/count, pitch " + s.pitch_coeff +
      (e.anchorUncertaintyUs === null ? "" : ", anchor ±" + e.anchorUncertaintyUs + "µs")
    : "";
  $("gameText").textContent = e.game || "no game";

  if (PROFILE) $("sProf").textContent = prof.text;
  ui.refreshSensHint();
}

/** Bridge-side health, pushed by the server once a second — no polling. */
function paintBridge(now) {
  const b = ui.bridge;
  const tile = $("statBridge");
  if (!b) {
    $("sBridge").textContent = "—";
    $("sBridgeSub").textContent = "";
    tile.classList.add("dim");
    tile.classList.remove("alert");
    return;
  }
  const stale = now - ui.bridgeAt > 5;
  const p50 = (b.latency && b.latency.p50_us) || 0;
  const p99 = (b.latency && b.latency.p99_us) || 0;
  $("sBridge").textContent = Math.round(b.datagrams_per_s || 0).toLocaleString();
  $("sBridgeSub").textContent = "/s · p50 " + (p50 / 1000).toFixed(1) + "ms";
  tile.classList.toggle("dim", stale);
  tile.classList.toggle("alert", !stale && (b.parse_errors > 0 || b.lag_drops > 0));
  tile.title =
    "bridge: " + Math.round(b.datagrams_per_s || 0) + " datagrams/s, " +
    (b.forwarded || 0) + " forwarded, " +
    (b.parse_errors || 0) + " parse errors, " +
    (b.lag_drops || 0) + " lag drops (" + (b.lag_disconnects || 0) + " disconnects), " +
    (b.clients || 0) + " ws clients\n" +
    "bridge latency p50 " + (p50 / 1000).toFixed(1) + "ms, p99 " + (p99 / 1000).toFixed(1) +
    "ms over " + ((b.latency && b.latency.samples) || 0) + " batches" +
    (b.latency && b.latency.negative ? " (" + b.latency.negative + " negative — clock skew)" : "") +
    (stale ? "\n(stale: no update in the last 5s)" : "");
}

/* =====================================================================
   OBS HUD — the stats bar's stream-facing replacement. Built once from the
   configured item list; repainted at 10Hz with one textContent write per
   item, and only when the text actually changed.
   ===================================================================== */

const hud = { items: [] };

function buildHud() {
  const box = $("hud");
  box.innerHTML = "";
  hud.items.length = 0;
  if (!OBS || !OBS.hud.length) { box.classList.add("hidden"); return; }
  box.classList.remove("hidden");
  box.classList.add(OBS.hudPos);
  for (const key of OBS.hud) {
    const spec = HUD_ITEMS[key];
    const item = document.createElement("div");
    item.className = "item" + (spec.text ? " text" : "");
    const k = document.createElement("div");
    k.className = "k";
    k.textContent = spec.k;
    const v = document.createElement("div");
    v.className = "v";
    const val = document.createElement("span");
    val.textContent = "—";
    v.appendChild(val);
    if (spec.unit) {
      const u = document.createElement("small");
      u.textContent = spec.unit;
      v.appendChild(u);
    }
    item.appendChild(k);
    item.appendChild(v);
    box.appendChild(item);
    hud.items.push({ key: key, el: val, last: "—" });
  }
}

/* One cached formatter for the HUD's grouped integers: bare toLocaleString()
   constructs a fresh Intl formatter per call, and hudValue runs at 10Hz.
   Same locale, same default options — output is identical. */
const hudIntFmt = new Intl.NumberFormat();

function hudValue(key, e) {
  switch (key) {
    case "speed": return e.speedCm.toFixed(e.speedCm < 100 ? 1 : 0);
    case "aim": return hudIntFmt.format(Math.round(e.speedDeg));
    case "cpm": return String(Math.round(e.clickRate.total(e.playT)));
    case "eps": return hudIntFmt.format(Math.round(e.eventRate.perSecond(e.playT)));
    case "dist": return (e.totalCm / 100).toFixed(2);
    case "aimdist": return hudIntFmt.format(Math.round(e.totalDeg));
    case "clicks": return hudIntFmt.format(e.totalClicks);
    case "game": return e.game ? e.game.replace(/\.exe$/i, "") : "—";
    case "latency": {
      const us = ui.mode === "live" ? e.newestUtcUs() : null;
      if (us === null) { latEma = null; return "—"; }
      const ms = (Date.now() * 1000 - us) / 1000;
      latEma = latEma === null ? ms : latEma + (ms - latEma) * 0.25;
      return latEma.toFixed(1);
    }
  }
  return "—";
}

function paintHud() {
  const e = engine;
  for (const it of hud.items) {
    const v = hudValue(it.key, e);
    if (v !== it.last) { it.last = v; it.el.textContent = v; }
  }
  /* The panel hints are only visible with labels on; paintStats normally
     keeps them current, and this is its stand-in. */
  if (OBS.labels) ui.refreshSensHint();
}

/* =====================================================================
   Main loop — one rAF drives ingest-consumption, drawing, and readouts.

   Redrawing is conditional: a still picture is still a still picture, and
   at 240Hz a full two-canvas repaint of nothing is the single largest
   thing this page does to a laptop battery.
   ===================================================================== */

let lastFrameT = 0;
let wasAlive = false;
let lastDrawMs = 0;

function frame(msNow) {
  requestAnimationFrame(frame);
  const now = msNow / 1000;

  /* Rate cap: the play head still advances every frame (cheap, and keeps
     the data exact); only the expensive part — repainting — is skipped
     until the cap's interval has elapsed. The half-frame slack keeps a
     cap equal to the refresh rate from aliasing to half of it. */
  const cap = (!OBS && !document.hasFocus()) ? Math.min(MAX_FPS, FPS_BACKGROUND) : MAX_FPS;
  const due = msNow - lastDrawMs >= 1000 / cap - 2;

  const dt = lastFrameT ? now - lastFrameT : 0;
  lastFrameT = now;
  if (dt > 0.0025 && dt < 1) {
    const inst = 1 / dt;
    fpsEma += (inst - fpsEma) * 0.1;
    /* Refresh estimate: the fastest frame we have seen lately. Decays so a
       move to a slower monitor is picked up within a minute. */
    refreshEst = Math.max(refreshEst * 0.999, Math.min(inst, 400));
  }

  /* Pausing freezes the picture, never the data: tick() still runs so
     nothing is dropped while the display is held. */
  if (PROFILE) performance.mark("tk0");
  engine.tick(now);
  if (PROFILE) { performance.mark("tk1"); measure("tick", "tk0", "tk1", "tick"); }

  /* Keep drawing while anything is still decaying, and draw one final frame
     after the last trail point expires so the panels actually end up
     empty instead of holding a ghost. */
  const alive = engine.visualsAlive();
  if (due && !ui.paused && (engine.dirty || alive || wasAlive)) {
    if (PROFILE) performance.mark("dr0");
    engine.draw();
    if (PROFILE) { performance.mark("dr1"); measure("draw", "dr0", "dr1", "draw"); }
    engine.dirty = false;
    lastDrawMs = msNow;
    wasAlive = alive;
  }

  if (now - lastStatsPaint >= 0.1) {
    lastStatsPaint = now;
    if (PROFILE) performance.mark("st0");
    /* OBS mode: the stats bar is display:none, so writing ~20 tiles into
       it would be pure layout work; the HUD is the only readout. */
    if (OBS) paintHud();
    else paintStats(now);
    /* The transport writes layout-affecting styles; at 240Hz that was a
       style recalc per frame for a bar that changes by a pixel. Same 100ms
       cadence as the stats. */
    if (ui.mode === "replay") ui.renderTransport();
    if (PROFILE) { performance.mark("st1"); measure("stats", "st0", "st1", "stats"); }
  }

  if (PROFILE) { prof.frames++; profileRoll(now); }
}

/* =====================================================================
   Wiring
   ===================================================================== */

$("modeSwitch").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) ui.setMode(b.dataset.mode);
});

/** Show one panel or both. Mirrors the choice into ?view= so the URL is
    shareable / bookmarkable; the ResizeObservers re-measure the panels. */
let view = "both";
function setView(v) {
  if (VIEWS.indexOf(v) < 0) v = "both";
  view = v;
  document.body.classList.remove("view-desk", "view-aim");
  if (v !== "both") document.body.classList.add("view-" + v);
  for (const b of $("viewSwitch").children) b.classList.toggle("on", b.dataset.view === v);
  const q = new URLSearchParams(location.search);
  if (v === "both") q.delete("view"); else q.set("view", v);
  const qs = q.toString();
  history.replaceState(null, "", location.pathname + (qs ? "?" + qs : "") + location.hash);
  deskPanel.resize();
  aimPanel.resize();
  engine.dirty = true;
}
$("viewSwitch").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) setView(b.dataset.view);
});

$("decay").addEventListener("input", (e) => {
  engine.decay = +e.target.value;
  $("decayVal").textContent = engine.decay.toFixed(1) + "s";
  engine.dirty = true;
});

$("buf").addEventListener("input", (e) => ui.setLiveBuffer(+e.target.value / 1000));

$("btnReset").addEventListener("click", () => engine.resetIntegrators());

$("btnPause").addEventListener("click", () => togglePause());

function togglePause() {
  if (ui.mode === "replay") {
    ui.setReplayPlaying(!ui.replayPlaying);
    return;
  }
  ui.paused = !ui.paused;
  $("btnPause").classList.toggle("on", ui.paused);
  $("btnPause").firstChild.nodeValue = ui.paused ? "Resume" : "Pause";
  engine.dirty = true;
}

$("btnPlay").addEventListener("click", () => ui.setReplayPlaying(!ui.replayPlaying));
$("btnPrevMarker").addEventListener("click", () => ui.jumpMarker(-1));
$("btnNextMarker").addEventListener("click", () => ui.jumpMarker(1));
$("sessionSel").addEventListener("change", (e) => ui.loadSession(e.target.value));

/* "Go to time": the picker is local time (what the user remembers); the
   recordings are UTC µs. `new Date("YYYY-MM-DDTHH:MM:SS")` parses a bare
   datetime-local value as local time, which is exactly the conversion needed. */
function gotoPickedTime() {
  const v = $("gotoTime").value;
  if (!v) return;
  const ms = new Date(v).getTime();
  if (!isFinite(ms)) { ui.toast("go to time", "unreadable date/time", true); return; }
  ui.gotoUtcUs(ms * 1000);
}
$("btnGoto").addEventListener("click", gotoPickedTime);
$("gotoTime").addEventListener("keydown", (e) => { if (e.key === "Enter") { gotoPickedTime(); e.preventDefault(); } });
$("speedSel").addEventListener("change", (e) => { ui.replaySpeed = +e.target.value; });

const scrub = $("scrub");
scrub.addEventListener("pointerdown", () => { ui.scrubbing = true; });
window.addEventListener("pointerup", () => { ui.scrubbing = false; });
scrub.addEventListener("input", () => {
  const dur = engine.duration;
  if (dur <= 0) return;
  ui.scrubbing = true;
  engine.seek(engine.tStart + dur * (+scrub.value / 1000));
  ui.renderTransport();
  engine.draw();
  engine.dirty = false;
});

window.addEventListener("keydown", (e) => {
  if (e.target && /^(INPUT|SELECT|TEXTAREA)$/.test(e.target.tagName)) return;
  if (e.code === "KeyR") { engine.resetIntegrators(); e.preventDefault(); }
  else if (e.code === "Space") { togglePause(); e.preventDefault(); }
  else if (e.code === "KeyV" && !OBS) { setView(VIEWS[(VIEWS.indexOf(view) + 1) % VIEWS.length]); e.preventDefault(); }
  else if (ui.mode === "replay" && e.code === "ArrowRight") {
    engine.seek(Math.min(engine.tEnd, engine.playT + (e.shiftKey ? 10 : 1)));
    ui.renderTransport(); e.preventDefault();
  } else if (ui.mode === "replay" && e.code === "ArrowLeft") {
    engine.seek(Math.max(engine.tStart, engine.playT - (e.shiftKey ? 10 : 1)));
    ui.renderTransport(); e.preventDefault();
  }
});

/* Dragging the window to a monitor with a different DPI fires no resize
   event, so the canvas keeps its old backing-store scale and goes soft.
   A resolution media query does fire — and has to be re-armed each time,
   because it is pinned to the ratio that was current when it was made. */
let dprQuery = null;
function onDprChange() {
  deskPanel.resize();
  aimPanel.resize();
  engine.dirty = true;
  armDprWatch();
}
function armDprWatch() {
  if (dprQuery) {
    if (dprQuery.removeEventListener) dprQuery.removeEventListener("change", onDprChange);
    else if (dprQuery.removeListener) dprQuery.removeListener(onDprChange);
  }
  const dpr = window.devicePixelRatio || 1;
  try {
    dprQuery = matchMedia("(resolution: " + dpr + "dppx)");
    if (dprQuery.addEventListener) dprQuery.addEventListener("change", onDprChange);
    else if (dprQuery.addListener) dprQuery.addListener(onDprChange);
  } catch (e) { dprQuery = null; }
}
armDprWatch();

/* A trim the render loop cannot starve. rAF is suspended in a background
   tab but the socket is not, so this is the timer that keeps a hidden tab
   from growing without bound. */
setInterval(() => engine.trim(), 250);

if (OBS) {
  /* Body classes drive the CSS (chrome hidden, layout, labels); the panels
     were constructed before this ran, so re-measure them once the hidden
     one has collapsed to 0×0. */
  document.body.classList.add("obs", "layout-" + OBS.layout);
  if (OBS.labels) document.body.classList.add("labels");
  document.documentElement.style.setProperty("--s", String(OBS.scale));
  $("decay").value = String(OBS.trail);
  buildHud();
  deskPanel.resize();
  aimPanel.resize();
}
if (!OBS && VIEW !== "both") setView(VIEW);
engine.decay = +$("decay").value;
$("decayVal").textContent = engine.decay.toFixed(1) + "s";
let savedBuffer = LIVE_BUFFER_DEFAULT;
if (OBS) {
  savedBuffer = OBS.buffer;
} else {
  try {
    const v = +localStorage.getItem(LIVE_BUFFER_KEY);
    if (isFinite(v) && v >= 0.01 && v <= 0.2) savedBuffer = v;
  } catch (e) { /* storage blocked */ }
}
ui.setLiveBuffer(savedBuffer);
if (PROFILE) $("statProf").classList.remove("hidden");
/* ?at=<local ISO datetime or epoch ms> opens straight into replay at that
   moment — a bookmarkable "show me 21:14:03 last night" — without first
   opening a live socket it would close a millisecond later. Not for OBS
   sources, which are live by definition. */
const AT = QUERY.get("at");
let atMs = NaN;
if (AT && !OBS) {
  atMs = /^\d{11,}$/.test(AT) ? +AT : new Date(AT).getTime();
  if (!isFinite(atMs)) ui.toast("?at=", "unreadable date/time: " + AT, true);
}
if (isFinite(atMs)) {
  ui.pendingUtcUs = atMs * 1000;
  $("gotoTime").value = toDatetimeLocal(atMs);
  ui.setMode("replay");
} else {
  ui.setConn("warn", "connecting");
  ui.connect();
}
requestAnimationFrame(frame);

/* Handy from the devtools console when tuning: telemouse.engine.playT, etc. */
window.telemouse = {
  engine: engine, ui: ui, deskPanel: deskPanel, aimPanel: aimPanel,
  prof: prof, profiling: PROFILE, obs: OBS,
};
