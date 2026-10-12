"use strict";
/* =====================================================================
   telemouse viz — one engine, two sources.

   Live mode feeds WebSocket envelopes into the same engine that replay
   mode feeds from a streamed .jsonl file. The engine exposes:

       engine.ingest(envelope)   // session | batch | marker, wire JSON
       engine.tick(nowSeconds)   // advance the play head, consume events
       engine.draw()             // render every panel from engine state

   The engine never reaches into the UI: what it needs from it (mode, live
   buffer, replay transport) arrives as a small `ctx` argument, and what it
   has to say (a session restarted, data was lost, a marker passed) goes out
   through `engine.hooks`, which the UI fills in once at start-up.

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
/* Ring colour per button: left, right, everything else. The second number
   is an index into PAL.ring (see "Palette" below). */
const BTN_KIND = [[BTN.LEFT_DOWN, 0], [BTN.RIGHT_DOWN, 1], [BTN.MIDDLE_DOWN, 2], [BTN.X1_DOWN, 2], [BTN.X2_DOWN, 2]];

/* Used when the session has no profile for the foreground process. */
const FALLBACK_SENS = { sens: 1.0, yaw_coeff: 0.022, pitch_coeff: 0.022, fallback: true };
/* Used until a session envelope says otherwise. */
const DEFAULT_CPI = 800;
const DEFAULT_QPC_FREQ = 1e7;

/* =====================================================================
   Limits and defaults of the user-facing knobs, defined once. The sliders
   in index.html get their min/max/step from here at start-up, and the OBS
   parameters are clamped to the same ranges.

   They mirror telemouse-core's config validator (crates/core/src/config.rs):
   BUFFER_MS ↔ OBS_BUFFER_MS_RANGE, TRAIL_SECS ↔ OBS_TRAIL_SECS_RANGE,
   OBS_SCALE ↔ OBS_SCALE_RANGE, and the defaults ↔ ObsConfig::default().
   A Rust test in crates/viz/src/server.rs checks that they still agree.
   ===================================================================== */
const BUFFER_MS = Object.freeze({ min: 10, max: 200, step: 5, dflt: 35 });
const TRAIL_SECS = Object.freeze({ min: 0.3, max: 12, step: 0.1, dflt: 3 });
const OBS_SCALE = Object.freeze({ min: 0.5, max: 4, dflt: 1 });
/* The overlay's built-in defaults: what an `/obs` page uses for anything
   the server's [viz.obs] table and the URL leave unsaid. */
const OBS_DEFAULTS = Object.freeze({
  layout: "split", background: "transparent", hud: Object.freeze(["speed", "aim", "cpm"]),
  hud_position: "bottom-left", scale: OBS_SCALE.dflt, trail_secs: TRAIL_SECS.dflt,
  buffer_ms: BUFFER_MS.dflt, grid: true, legend: false, labels: false, stale_secs: 3,
});

/** The same rule as telemouse-core's `recordings::is_safe_id`: 1–128
    characters of `[A-Za-z0-9_-]`. A `?session=` that fails it is ignored. */
function isSafeId(id) {
  return typeof id === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(id);
}

/** True for `null` and `undefined`: the two ways the wire says "absent". */
const isNil = (v) => v === null || v === undefined;

/* =====================================================================
   Windows pointer ballistics — the "pointer" panel

   The wire is raw input: HID counts before Windows touches them, which is
   what a game reading raw input sees. The desktop cursor sees something
   else — the counts after "Enhance pointer precision" and the pointer speed
   slider — and the pointer panel shows *that*, by re-running Windows'
   transfer function over every event. The inputs Windows had (the
   precision switch, the slider, the SmoothMouse{X,Y}Curve registry curve,
   the system DPI) ride in the session envelope's `pointer` field; a
   recording that predates it gets a fresh install's defaults and a hint.

   The algorithm, per mouse report (Microsoft's "Pointer Ballistics for
   Windows XP" paper, with the Windows 7 correction that replaced the
   refresh-rate term by a fixed 150):

     mouse speed    v  = |(dx, dy)| / 3.5                 "inches/s"
     pointer speed  y  = curve(v)     piecewise-linear through the five
                                      registry points, extrapolated past
                                      the last one
     pixels         p  = y · dpi / 150 · slider,  in the direction of (dx, dy)

   so with the default curve, 96 dpi and the slider in the middle, one count
   is 0.58 px at a crawl, 0.78 px at 1.25 in/s (the third point), 1:1 at
   about 2.1 in/s and 2.2 px at 12 in/s. Without
   precision the pixels are counts × the slider's linear multiplier. Either
   way the cursor moves whole pixels and Windows keeps the remainder, which
   is why a slow hand moves it in steps: the model keeps it too.

   What is *not* modelled, and why the panel is a close reconstruction and
   not a recording of the cursor: the report-rate dependence (3.5 assumes
   Windows' 125 Hz era; a 1000 Hz mouse sends the same distance as eight
   smaller reports, each of which lands lower on the curve — exactly as the
   real cursor does, since the counts are the real reports), and any
   smoothing across reports the paper hints at, which is not documented
   well enough to copy.
   ===================================================================== */

/** A fresh Windows install; the curves are the registry's exact 16.16 values. */
const WIN_POINTER_DEFAULTS = Object.freeze({
  enhance_pointer_precision: true,
  speed: 10,
  curve_x: Object.freeze([0, 0x6e15 / 65536, 1.25, 0x3dc29 / 65536, 40]),
  curve_y: Object.freeze([0, 0x15eb8 / 65536, 0x54ccd / 65536, 0x184ccd / 65536, 568]),
  screen_dpi: 96,
});
/* Windows' two hard-coded scale factors: counts per (inch/second) on the way
   into the curve, and the "mouse update rate" on the way out of it. */
const EPP_MOUSE_SCALE = 3.5;
const EPP_POINTER_RATE = 150;
/* The pointer-speed slider's eleven notches as SPI_GETMOUSESPEED values, and
   the multiplier each applies: one table with precision off (the classic
   1/32 … 3.5) and another with it on. A registry-set value between two
   notches is interpolated. */
const SPEED_NOTCHES = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20];
const SPEED_MULT_LINEAR = [1 / 32, 1 / 16, 1 / 4, 1 / 2, 3 / 4, 1, 1.5, 2, 2.5, 3, 3.5];
const SPEED_MULT_EPP = [0.1, 0.2, 0.4, 0.6, 0.8, 1.0, 1.2, 1.4, 1.6, 1.8, 2.0];

/** The slider's multiplier for a 1..20 speed value. */
function speedMultiplier(speed, epp) {
  const table = epp ? SPEED_MULT_EPP : SPEED_MULT_LINEAR;
  const s = clamp(+speed || 10, 1, 20);
  let i = 0;
  while (i < SPEED_NOTCHES.length - 1 && SPEED_NOTCHES[i + 1] <= s) i++;
  if (i === SPEED_NOTCHES.length - 1 || SPEED_NOTCHES[i] === s) return table[i];
  const f = (s - SPEED_NOTCHES[i]) / (SPEED_NOTCHES[i + 1] - SPEED_NOTCHES[i]);
  return table[i] + (table[i + 1] - table[i]) * f;
}

/** A five-point curve from the wire, or null when it is not one the model
    can run: five finite non-decreasing numbers starting at 0. */
function pointerCurve(raw) {
  if (!Array.isArray(raw) || raw.length !== 5) return null;
  const out = new Array(5);
  for (let i = 0; i < 5; i++) {
    const v = +raw[i];
    if (!Number.isFinite(v) || v < 0 || (i > 0 && v < out[i - 1])) return null;
    out[i] = v;
  }
  return out[0] === 0 ? out : null;
}

/** Settings for the model from a session envelope's `pointer` (or anything
    else): every field defaults on its own, so a partial or garbled object
    still yields a runnable set. `assumed` says whether *anything* was taken
    from the input. */
function pointerSettingsFrom(raw) {
  const d = WIN_POINTER_DEFAULTS;
  const r = raw && typeof raw === "object" ? raw : {};
  const speed = Math.round(+r.speed);
  const dpi = Math.round(+r.screen_dpi);
  return {
    enhance_pointer_precision: r.enhance_pointer_precision === undefined
      ? d.enhance_pointer_precision : !!r.enhance_pointer_precision,
    speed: speed >= 1 && speed <= 20 ? speed : d.speed,
    curve_x: pointerCurve(r.curve_x) || d.curve_x,
    curve_y: pointerCurve(r.curve_y) || d.curve_y,
    screen_dpi: dpi >= 24 && dpi <= 1000 ? dpi : d.screen_dpi,
    assumed: !(raw && typeof raw === "object"),
  };
}

/** Windows' pointer transfer function, one instance per session. The
    per-event path allocates nothing: results land in `outX`/`outY` (whole
    pixels, what the cursor did) and `moveX`/`moveY` (the movement before
    truncation, for speed readouts that should not see the pixel steps). */
class PointerModel {
  constructor(settings) {
    this.s = settings;
    this.epp = !!settings.enhance_pointer_precision;
    this.mult = speedMultiplier(settings.speed, this.epp);
    /* pixels per curve-unit of pointer speed */
    this.pxScale = settings.screen_dpi / EPP_POINTER_RATE;
    this.rx = 0; this.ry = 0;        // sub-pixel remainders
    this.outX = 0; this.outY = 0;
    this.moveX = 0; this.moveY = 0;
  }

  reset() { this.rx = 0; this.ry = 0; this.outX = this.outY = this.moveX = this.moveY = 0; }

  /** Pointer speed for a mouse speed, off the curve (both in Windows'
      "inches/s"). Linear between points; the last segment continues. */
  curve(v) {
    const X = this.s.curve_x, Y = this.s.curve_y;
    if (v <= 0) return 0;
    let i = 1;
    while (i < 4 && X[i] < v) i++;
    const x0 = X[i - 1], x1 = X[i];
    if (x1 <= x0) return Y[i];
    return Y[i - 1] + (Y[i] - Y[i - 1]) * (v - x0) / (x1 - x0);
  }

  /** Pixels per count for a report of `mag` counts. */
  gain(mag) {
    if (!(mag > 0)) return 0;
    if (!this.epp) return this.mult;
    const v = mag / EPP_MOUSE_SCALE;
    return this.curve(v) * this.pxScale / mag * this.mult;
  }

  /** One report. Whole pixels in `outX`/`outY`, the remainder carried. */
  apply(dx, dy) {
    const g = this.gain(Math.sqrt(dx * dx + dy * dy));
    const mx = dx * g, my = dy * g;
    this.moveX = mx; this.moveY = my;
    const ax = this.rx + mx, ay = this.ry + my;
    /* `|| 0` turns Math.trunc's -0 into 0. */
    const ix = Math.trunc(ax) || 0, iy = Math.trunc(ay) || 0;
    this.rx = ax - ix; this.ry = ay - iy;
    this.outX = ix; this.outY = iy;
  }

  /** "precision on · 10/20 · 96 dpi", for the panel subtitle. */
  describe() {
    const s = this.s;
    return "precision " + (this.epp ? "on" : "off") + " · " + s.speed + "/20 · " + s.screen_dpi + " dpi";
  }
}

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
/* both is desk + aim (the original pair); pointer is Windows' cursor on
   its own; compare puts the raw hand path and the cursor side by side. */
const VIEWS = ["both", "desk", "aim", "pointer", "compare"];
const VIEW = VIEWS.indexOf(QUERY.get("view")) >= 0 ? QUERY.get("view") : "both";

/* ---------- small helpers ---------- */
const clamp = (v, lo, hi) => v < lo ? lo : v > hi ? hi : v;

/* One cached formatter for grouped integers: bare toLocaleString()
   constructs a fresh Intl formatter per call, and the readouts run at 10Hz.
   Same locale, same default options — output is identical. */
const INT_FMT = new Intl.NumberFormat();
const fmtInt = (n) => INT_FMT.format(n);

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
/* Where the bridge is listening for the capture agent. Injected by the
   server; empty when the page is opened straight off disk. The page says it
   out loud while it waits, because "nothing is moving" and "the agent is
   shipping to a different port" look identical otherwise. */
const UDP_ADDR = typeof SERVER_CFG.udp_addr === "string" ? SERVER_CFG.udp_addr : "";

const HUD_ITEMS = {
  speed:   { k: "speed",     unit: "cm/s" },
  aim:     { k: "aim speed", unit: "°/s" },
  pointer: { k: "pointer",   unit: "px/s" },
  cpm:     { k: "clicks/min" },
  eps:     { k: "events/s" },
  dist:    { k: "hand dist", unit: "m" },
  aimdist: { k: "aim dist",  unit: "°" },
  clicks:  { k: "clicks" },
  game:    { k: "game", text: true },
  latency: { k: "latency",   unit: "ms" },
  eventage: { k: "event age", unit: "ms" },
  status:  { k: "feed", text: true },
};

/* Upper bound on the OBS stale timer, matching `MAX_OBS_STALE_SECS` in
   telemouse-core's config validator: past a minute the indicator would
   outlive the stream it is meant to warn about. */
const OBS_STALE_MAX = 60;
/* How long the dashboard's connection pill waits before it stops saying
   "live". Not configurable: the pill is a diagnostic, not stream content. */
const NO_DATA_SECS = 3;
/* An idle capture agent sends `{"type":"heartbeat"}` once a second
   (HEARTBEAT_INTERVAL in crates/capture/src/shipping.rs) so a still hand can
   be told from a dead agent. Three missed in a row and we stop calling it
   alive: enough slack for a dropped datagram or a scheduling hiccup, short
   enough that a killed agent reads as gone rather than idle. A capture agent
   older than this page sends none, which lands on exactly the old
   behaviour. */
const AGENT_ALIVE_SECS = 3;

/** Seconds on the page's own monotonic clock. */
function nowSec() {
  return performance.now() / 1000;
}

/**
 * One rule for "is anything actually arriving", shared by the dashboard's
 * connection pill and the OBS stale indicator.
 *
 *   down     — the socket is not open
 *   waiting  — open, but nothing has ever arrived (capture agent not running)
 *   idle     — no batch, but the agent's heartbeat says it is running and the
 *              hand is simply still
 *   stale    — batches were arriving and stopped, and nothing said hello
 *   live     — a batch arrived within `staleAfter`
 *
 * `age` is measured from the last batch, or from when the socket opened when
 * there has never been one. `heartbeatAge` is seconds since the capture
 * agent's last idle heartbeat, `Infinity` when there has never been one.
 * `staleAfter <= 0` disables the stale verdict.
 */
function feedState(o) {
  const since = o.lastBatchAt || o.since || 0;
  const age = since ? Math.max(0, o.now - since) : 0;
  const alive = o.heartbeatAge <= AGENT_ALIVE_SECS;
  if (!o.connected) return { kind: "down", age: age };
  if (!o.lastBatchAt) return { kind: alive ? "idle" : "waiting", age: age };
  if (o.staleAfter > 0 && age > o.staleAfter) return { kind: alive ? "idle" : "stale", age: age };
  return { kind: "live", age: age };
}

/**
 * Whether the overlay should show its "no feed" state: `quietFor` seconds
 * have passed without a batch, and that is more than `staleAfter`
 * (`[viz.obs] stale_secs`, `?stale=`; 0 or less = never).
 *
 * Deliberately blind to the socket. "Seconds without data" is the one thing
 * the viewer can see, and it reads the same whether the capture agent
 * stopped, the bridge went away or the socket is between retries — so a page
 * that has just loaded, or a socket that has just dropped, gets the same
 * grace period as a feed that went quiet, instead of flashing "no feed"
 * for the moment it takes to connect.
 */
function feedIsStale(quietFor, staleAfter) {
  return staleAfter > 0 && quietFor > staleAfter;
}

/**
 * The overlay's three-state verdict, from the two clocks the page keeps:
 * seconds since the last batch (`quietFor`) and seconds since the capture
 * agent's last idle heartbeat (`heartbeatAge`, `Infinity` when there has
 * never been one).
 *
 *   live — inside the grace period; nothing to say
 *   idle — quiet, but the agent is still there. A still hand is not a dead
 *          capture, so the panels keep their brightness and only the badge
 *          speaks.
 *   down — quiet, and nothing has said hello: no feed.
 */
function overlayFeed(quietFor, heartbeatAge, staleAfter) {
  if (!feedIsStale(quietFor, staleAfter)) return "live";
  return heartbeatAge <= AGENT_ALIVE_SECS ? "idle" : "down";
}

/** Connection-pill text for a feed state: class, and what it says. */
function connPillState(st, udpAddr) {
  switch (st.kind) {
    case "live":    return { cls: "ok", text: "live" };
    case "idle":    return { cls: "ok", text: "idle · " + Math.round(st.age) + "s" };
    case "waiting": return { cls: "warn", text: "waiting for capture on udp " + (udpAddr || "?") };
    case "stale":   return { cls: "warn", text: "no data for " + Math.round(st.age) + "s" };
    default:        return { cls: "err", text: "disconnected" };
  }
}

/** "#rgb" | "#rrggbb" (and, with `alpha`, "#rrggbbaa"), leading `#`
    optional → [r, g, b, a] with a in 0..1, or null when it is not hex. */
function hexChannels(s, alpha) {
  let h = String(s || "").trim().toLowerCase().replace(/^#/, "");
  if (h.length === 3) h = h.replace(/./g, (c) => c + c);
  if (!(alpha ? /^[0-9a-f]{6}([0-9a-f]{2})?$/ : /^[0-9a-f]{6}$/).test(h)) return null;
  const ch = (k) => parseInt(h.slice(k, k + 2), 16);
  return [ch(0), ch(2), ch(4), h.length === 8 ? ch(6) / 255 : 1];
}

/** "transparent" | "rrggbb" | "#rrggbbaa" | "rgb" → { css, alpha }. */
function parseBg(s) {
  s = String(s || "").trim().toLowerCase();
  const c = s && s !== "transparent" && s !== "none" ? hexChannels(s, true) : null;
  if (!c) return { css: "transparent", alpha: 0 };
  return { css: "rgba(" + c[0] + "," + c[1] + "," + c[2] + "," + c[3].toFixed(3) + ")", alpha: c[3] };
}

const OBS = OBS_MODE ? (() => {
  const d = Object.assign({}, OBS_DEFAULTS, SERVER_CFG.obs || {});
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
    return Number.isFinite(n) ? clamp(n, lo, hi) : clamp(+dflt, lo, hi);
  };
  const hudRaw = QUERY.get("hud");
  const hud = (hudRaw === null ? (Array.isArray(d.hud) ? d.hud : []) : hudRaw.split(","))
    .map((s) => String(s).trim().toLowerCase())
    .filter((s) => Object.prototype.hasOwnProperty.call(HUD_ITEMS, s));
  return {
    /* `view=` (the dashboard's spelling) is accepted here too, so the same
       URL suffix works on both routes. `pointer` is Windows' cursor as the
       whole overlay; `compare` is the raw hand path beside it. */
    layout: str("layout", { desk: "desk", aim: "aim", both: "split", pointer: "pointer", compare: "compare" }[QUERY.get("view")] || d.layout,
                ["split", "stack", "desk", "aim", "pointer", "compare"]),
    bg: parseBg(QUERY.get("bg") === null ? d.background : QUERY.get("bg")),
    hud: hud,
    hudPos: str("hudpos", d.hud_position, ["bottom-left", "top-left", "top-right", "bottom-right"]),
    scale: num("scale", d.scale, OBS_SCALE.min, OBS_SCALE.max),
    trail: num("trail", d.trail_secs, TRAIL_SECS.min, TRAIL_SECS.max),
    buffer: num("buffer", d.buffer_ms, BUFFER_MS.min, BUFFER_MS.max) / 1000,
    grid: bool("grid", d.grid),
    legend: bool("legend", d.legend),
    labels: bool("labels", d.labels),
    /* Seconds of silence before the overlay says there is no feed; `0` turns
       the indicator off for a source that must never draw anything but the
       trails. Clamped to the same 0..60 the config validator enforces. */
    stale: num("stale", d.stale_secs, 0, OBS_STALE_MAX),
  };
})() : null;

const MAX_FPS = Number.isFinite(fpsQ) && fpsQ > 0 ? clamp(fpsQ, 5, 400) : (OBS ? FPS_OBS_DEFAULT : FPS_DEFAULT);

/* Stroke / marker size multiplier: 1 on the dashboard, `scale` in OBS mode. */
const S = OBS ? OBS.scale : 1;
/* =====================================================================
   Palette

   Everything the canvases draw with, in one object. The dashboard fills it
   from the page's design tokens (the CSS custom properties on :root) once
   at start-up and again when the theme changes — never per frame: a
   getComputedStyle in the draw path is a forced style recalc at 240Hz.
   The draw code only ever reads ready-made strings from PAL.

   The OBS overlay does not read the tokens at all. It is composited over
   game footage, not over the page, so it keeps these built-in values
   whatever the theme; grid lines, near-invisible on a transparent page
   over dark footage, get a lighter, alpha-based set.
   ===================================================================== */

const PAL = {
  canvas: "#0b0f16",
  grid: "#161e2b", gridAxis: "#2a3547", gridWrap: "#3a2b46",
  head: "#ffffff", headHalo: "rgba(255,255,255,0.10)",
  cross: "rgba(53,208,224,0.55)",
  accent: "#35d0e0", accentRgb: "53,208,224",
  rail: "#222c3d", ghostFill: "#121826", ghostLine: "#2e3a50",
  text: "#9aa8bb", textDim: "#66748a",
  /* "r,g,b" per button kind: left, right, everything else. */
  ring: ["70,214,138", "124,140,255", "255,176,46"],
  light: false,
};
if (OBS) {
  PAL.textDim = "#c8d2df";
  if (OBS.bg.alpha < 1) {
    PAL.grid = "rgba(255,255,255,0.07)";
    PAL.gridAxis = "rgba(255,255,255,0.16)";
    PAL.gridWrap = "rgba(200,140,255,0.35)";
  }
}

/** "#rgb" | "#rrggbb" → "r,g,b", or null when it is not a hex colour. */
function hexToRgb(hex) {
  const c = hexChannels(hex, false);
  return c === null ? null : c[0] + "," + c[1] + "," + c[2];
}

/** Overwrite PAL from a token lookup (`name` → value, "" when unset). A token
    that is missing or not plain hex leaves the built-in value alone, so a page
    opened without its stylesheet still draws. Pure: the tests drive it. */
function paletteFromTokens(get) {
  const hex = (name) => (hexToRgb(get(name)) === null ? null : String(get(name)).trim());
  const set = (key, name) => { const v = hex(name); if (v !== null) PAL[key] = v; };
  set("canvas", "--canvas");
  set("grid", "--grid"); set("gridAxis", "--grid-axis"); set("gridWrap", "--grid-wrap");
  set("head", "--head");
  set("accent", "--accent");
  set("rail", "--line"); set("ghostFill", "--surface"); set("ghostLine", "--line-strong");
  set("text", "--fg-2"); set("textDim", "--fg-3");
  PAL.headHalo = "rgba(" + hexToRgb(PAL.head) + ",0.10)";
  PAL.accentRgb = hexToRgb(PAL.accent);
  PAL.cross = "rgba(" + PAL.accentRgb + ",0.55)";
  ["--lmb", "--rmb", "--aux"].forEach((name, i) => {
    const rgb = hexToRgb(get(name));
    if (rgb !== null) PAL.ring[i] = rgb;
  });
  /* A light canvas needs the other velocity ramp: the dark one ends in white. */
  const c = hexToRgb(PAL.canvas).split(",");
  PAL.light = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2] > 140;
}


function niceStep(raw) {
  if (!(raw > 0) || !Number.isFinite(raw)) return 1;
  const exp = Math.floor(Math.log10(raw));
  const base = Math.pow(10, exp);
  const m = raw / base;
  const mult = m <= 1.5 ? 1 : m <= 3.5 ? 2 : m <= 7.5 ? 5 : 10;
  return mult * base;
}

function fmtTime(s) {
  if (!Number.isFinite(s) || s < 0) s = 0;
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

/* The timeline's columns, by name and element type: time, the deltas, the
   button/wheel words, and the index of the batch meta the event belongs to. */
const EVENT_COLUMNS = [
  ["t", Float64Array], ["dx", Int32Array], ["dy", Int32Array],
  ["b", Int32Array], ["w", Int32Array], ["wh", Int32Array], ["mi", Int32Array],
];

/** Event timeline as parallel typed arrays, growable by doubling. */
class EventColumns {
  constructor(cap) {
    this.cap = cap || 1 << 16;
    this.n = 0;
    this._alloc(this.cap);
  }
  _alloc(cap) {
    for (const [name, Type] of EVENT_COLUMNS) this[name] = new Type(cap);
    this.cap = cap;
  }
  _grow(need) {
    let cap = this.cap;
    while (cap < need) cap *= 2;
    const old = EVENT_COLUMNS.map(([name]) => this[name]);
    const n = this.n;
    this._alloc(cap);
    EVENT_COLUMNS.forEach(([name], k) => this[name].set(old[k].subarray(0, n)));
  }
  /* The hot path stays spelled out: one store per column, no loop. */
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
    for (const [name] of EVENT_COLUMNS) this[name].copyWithin(0, k, n);
    this.n = n - k;
  }
  clear() { this.n = 0; }
}

/* Ring index arithmetic shared by Trail and EffectRing: both are
   `{ cap, head, n }` with the oldest entry at `head`. */

/** The slot the next push writes, evicting the oldest entry when full. */
function ringClaim(r) {
  if (r.n === r.cap) {
    const i = r.head;
    r.head = r.head + 1 === r.cap ? 0 : r.head + 1;
    return i;
  }
  return (r.head + r.n++) % r.cap;
}

/** Forget the oldest entry. */
function ringShift(r) {
  r.head = r.head + 1 === r.cap ? 0 : r.head + 1;
  r.n--;
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
    const i = ringClaim(this);
    this.x[i] = x; this.y[i] = y; this.t[i] = t; this.v[i] = v; this.brk[i] = brk ? 1 : 0;
  }
  trim(cutoff) {
    while (this.n > 0 && this.t[this.head] < cutoff) ringShift(this);
  }
}

/** Small ring of short-lived effect objects (click rings, wheel ticks). */
class EffectRing {
  constructor(cap) { this.cap = cap; this.buf = new Array(cap); this.head = 0; this.n = 0; }
  clear() { this.head = 0; this.n = 0; this.buf.fill(undefined); }
  push(o) {
    this.buf[ringClaim(this)] = o;
  }
  trim(cutoff) {
    while (this.n > 0 && this.buf[this.head].t < cutoff) {
      this.buf[this.head] = undefined;
      ringShift(this);
    }
  }
  at(k) { return this.buf[(this.head + k) % this.cap]; }
}

/* Velocity → colour: cool blue (slow) through cyan and amber to hot white. */
const RAMP_DARK = [
  [0.00, 42, 86, 200],
  [0.28, 42, 200, 235],
  [0.52, 120, 235, 190],
  [0.72, 255, 200, 60],
  [0.88, 255, 122, 32],
  [1.00, 255, 250, 245],
];
/* The same walk on a light canvas, where white would vanish: it ends hot and
   dark instead, and every stop is deep enough to read on white. */
const RAMP_LIGHT = [
  [0.00, 52, 84, 200],
  [0.28, 0, 140, 186],
  [0.52, 16, 150, 104],
  [0.72, 204, 138, 0],
  [0.88, 226, 88, 12],
  [1.00, 150, 16, 24],
];
function rampColor(x, RAMP) {
  RAMP = RAMP || RAMP_DARK;
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
/** (Re)build the quantised ramp for the current palette, in place. */
function buildRamp() {
  const ramp = PAL.light ? RAMP_LIGHT : RAMP_DARK;
  for (let i = 0; i < VLEVELS; i++) {
    const c = rampColor(i / (VLEVELS - 1), ramp);
    RAMP_CSS[i] = "rgb(" + c[0] + "," + c[1] + "," + c[2] + ")";
  }
}
buildRamp();
const ALEVELS = 7;

/* =====================================================================
   Engine
   ===================================================================== */

/** Everything a session accumulates, as it is before the first envelope.
    Used once to build the engine and again by `reset()`, so a field added
    here is reset by construction — the hand-kept list it replaces had
    drifted (the CPI and the colour scale of the previous session survived a
    restart). Options the user set (`decay`) and the UI wiring (`hooks`,
    `panels`) are not session state and live on the engine itself. */
function initialEngineState() {
  return {
    /* session context */
    session: null,
    sessionId: null,
    cpi: DEFAULT_CPI,
    qpcFreq: DEFAULT_QPC_FREQ,
    anchorQpc: null,
    anchorUtcUs: null,
    anchorUncertaintyUs: null,
    devices: [],
    sensCache: new Map(),
    game: null,
    sens: FALLBACK_SENS,
    pointerLocked: false,
    cursorX: null, cursorY: null, screenW: 0, screenH: 0,
    /* The session's `pointer` field as received (null when it had none), and
       the Windows transfer function built from it — or from the defaults. */
    pointerSettings: null,
    pointer: new PointerModel(pointerSettingsFrom(null)),

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
    newestEventT: null,
    arrivalLatencyMs: null,
    latencyReceivedAt: null,

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
    /* Where Windows' cursor would be, in whole screen pixels from "recenter". */
    ptrX: 0, ptrY: 0,
    lastEventT: null,

    /* visuals */
    deskTrail: new Trail(TRAIL_CAP),
    aimTrail: new Trail(TRAIL_CAP),
    ptrTrail: new Trail(TRAIL_CAP),
    rings: new EffectRing(EFFECT_CAP),
    wheelTicks: new EffectRing(EFFECT_CAP),
    dirty: true,

    /* stats */
    totalCm: 0,
    totalDeg: 0,
    totalPx: 0,
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
    speedPx: 0,
    vrefDesk: 30,
    vrefAim: 300,
    /* px/s: a 25 cm/s hand at 800 cpi is ~8000 counts/s, more once the curve
       bites, so the pointer ramp starts hotter than the desk one. */
    vrefPtr: 6000,
    _tickCm: 0,
    _tickDeg: 0,
    _tickPx: 0,
    _quiet: false,
  };
}

/** What the engine needs to know about the UI, as the UI last said it. The
    frame loop passes one in; a direct call (a test, the devtools console)
    gets `engine.hooks.ctx()`. */
function defaultEngineCtx() {
  return { mode: "live", liveBuffer: BUFFER_MS.dflt / 1000, replayPlaying: false, replaySpeed: 1 };
}

/** The engine's outbound events. No-ops until the UI installs its own. */
function defaultEngineHooks() {
  const none = () => {};
  return {
    ctx: defaultEngineCtx,
    onVizStats: none, onHeartbeat: none, onBatch: none, onDataLoss: none,
    onSessionRestart: none, onSessionChanged: none, onMarker: none, onReplayEnd: none,
  };
}

const engine = {
  /* options */
  decay: TRAIL_SECS.dflt,
  /* wiring, set once at start-up */
  hooks: defaultEngineHooks(),
  /* what draw() renders into; Panel instances, set once at start-up */
  panels: [],

  /* ------------------------------------------------------------------ */

  /** Full teardown: nothing from a previous session survives. */
  reset() {
    Object.assign(this, initialEngineState());
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
    this.totalPx = 0;
    this.totalClicks = 0;
    this.clickRate.reset();
    this.eventRate.reset();
  },

  /** Recenter: zero the integrated positions and drop the visible history.
      Session totals survive — they describe the session, not the view. */
  resetIntegrators() {
    this.deskX = 0; this.deskY = 0;
    this.yawRaw = 0; this.pitch = 0;
    this.ptrX = 0; this.ptrY = 0;
    /* The sub-pixel remainder is position too: a recentred cursor starts
       on a pixel. */
    this.pointer.reset();
    this.lastEventT = null;
    this.deskTrail.clear();
    this.aimTrail.clear();
    this.ptrTrail.clear();
    this.rings.clear();
    this.wheelTicks.clear();
    this.speedCm = 0; this.speedDeg = 0; this.speedPx = 0;
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

  /** Feed one wire envelope. Identical path for WebSocket and JSONL.
      `receivedUtcMs` / `receivedAt` are the live arrival stamps (wall clock
      and page clock); `ctx` is the UI state the batch path consults. */
  ingest(env, receivedUtcMs = null, receivedAt = nowSec(), ctx = this.hooks.ctx()) {
    if (!env || typeof env !== "object") return;
    switch (env.type) {
      case "viz_stats": this.hooks.onVizStats(env); return;
      /* Liveness, not telemetry. The capture agent sends one a second while
         nothing is moving, so "hand still" stops looking like "agent dead".
         It must stay out of everything the batch path does: no timeline
         entry, no seq-gap accounting, no latency sample, no drop marks — the
         page's numbers have to describe the same stream whether or not the
         agent is new enough to send these. */
      case "heartbeat": this.hooks.onHeartbeat(); return;
      case "session": this.ingestSession(env); return;
      case "marker": this.ingestMarker(env); return;
      case "batch": this.ingestBatch(env, receivedUtcMs, receivedAt, ctx); return;
      default: return;
    }
  },

  ingestSession(env) {
    const id = env.session_id === undefined ? null : env.session_id;
    /* A different session id on the same feed means the capture agent
       restarted: new QPC anchor, new device table, new seq numbering.
       Keeping any of the old state would silently mix two timelines — the
       old events would sit at nonsense offsets against the new anchor and the
       play head would chase a tEnd that no longer exists. Tear everything
       down first, then adopt. */
    if (this.sessionId !== null && id !== null && id !== this.sessionId) {
      const from = this.sessionId;
      this.reset();
      this.hooks.onSessionRestart(from, id);
    }
    this.session = env;
    this.sessionId = id;
    this.cpi = +env.mouse_cpi > 0 ? +env.mouse_cpi : DEFAULT_CPI;
    const anchor = env.anchor || {};
    this.qpcFreq = +(anchor.qpc_freq || env.qpc_freq) || DEFAULT_QPC_FREQ;
    this.anchorQpc = anchor.qpc !== undefined ? +anchor.qpc : null;
    /* utc_us is what makes the timing tiles possible: it maps event offsets
       back onto this browser's wall clock. */
    this.anchorUtcUs = anchor.utc_us !== undefined ? +anchor.utc_us
      : (env.started_utc_us !== undefined ? +env.started_utc_us : null);
    this.anchorUncertaintyUs = env.anchor_uncertainty_us === undefined
      ? null : +env.anchor_uncertainty_us;
    this.devices = Array.isArray(env.devices) ? env.devices : [];
    this.sensCache.clear();
    this.sens = this.sensFor(this.game);
    /* What the desktop did with the counts. A session from a capture agent
       that did not record it (or not on Windows) gets a fresh install's
       settings, and the pointer panel says so. */
    this.pointerSettings = env.pointer && typeof env.pointer === "object" ? env.pointer : null;
    this.pointer = new PointerModel(pointerSettingsFrom(this.pointerSettings));
    this.dirty = true;
    this.hooks.onSessionChanged();
  },

  ingestMarker(env) {
    const t = this.qpcToT(env.ts_qpc);
    this.markers.push({ t: t, label: env.label || "marker" });
    this.noteTime(t);
  },

  ingestBatch(env, receivedUtcMs, receivedAt, ctx) {
    /* A batch — of any size, including empty — is the feed being alive.
       Everything that says "waiting for capture" or "no feed" hangs off this
       one timestamp. */
    this.hooks.onBatch();

    /* Data-quality accounting happens even for an empty batch. */
    const loss = this.accountLoss(env);
    const evs = env.events;
    const have = Array.isArray(evs) && evs.length > 0;
    if (have) {
      const tSum = this.appendEvents(env, evs);
      /* Replay has no arrival timestamp and must never produce a sample. */
      if (ctx.mode === "live" && receivedUtcMs !== null) {
        this.sampleLatency(receivedUtcMs, tSum / evs.length * 1000, receivedAt);
      }
    }

    if (loss.drops > 0 || loss.lost > 0) {
      const t = have ? this.qpcToT(evs[0].ts_qpc) : this.tEnd;
      this.dropMarks.push({ t: t, drops: loss.drops, lost: loss.lost });
      if (this.dropMarks.length > MAX_DROP_MARKS) this.dropMarks.shift();
      this.hooks.onDataLoss(loss.drops, loss.lost);
    }

    /* Hard cap, applied at ingest rather than at consume: a backgrounded tab
       stops running rAF but keeps receiving ~1000 events/s, and a trim that
       only the render loop can reach is a trim that never runs. */
    if (ctx.mode === "live" && this.ev.n > HARD_EVENT_CAP) {
      this.dropFront(this.ev.n - MAX_LIVE_EVENTS);
    }
  },

  /** Ring drops the agent reported, batches missing from the `seq_no`
      sequence (a number that goes backwards is a late arrival, not a gap),
      and discarded absolute frames: counted, and returned for the marks. */
  accountLoss(env) {
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
    return { drops: drops, lost: lost };
  },

  /** Put a batch's events on the timeline. Returns the sum of their times
      (seconds), which the latency sample averages. */
  appendEvents(env, evs) {
    /* One shared meta object per batch keeps per-event memory to seven
       typed-array slots while still letting a seek replay the batch context
       exactly. */
    const meta = {
      game: isNil(env.game) ? null : env.game,
      locked: !!env.pointer_locked,
      sens: null,
      cx: isNil(env.cursor_x) ? null : +env.cursor_x,
      cy: isNil(env.cursor_y) ? null : +env.cursor_y,
      sw: env.screen_w | 0,
      sh: env.screen_h | 0,
    };
    const mi = this.metaBase + this.metas.length;
    this.metas.push(meta);

    const cols = this.ev;
    let tFirst = 0, tLast = 0, tSum = 0;
    for (let i = 0; i < evs.length; i++) {
      const e = evs[i];
      const t = this.qpcToT(e.ts_qpc);
      if (i === 0) tFirst = t;
      tLast = t;
      tSum += t;
      this.newestEventT = this.newestEventT === null ? t : Math.max(this.newestEventT, t);
      /* `| 0` is doing real work here: buttons/wheel/wheel_h are omitted
         from the JSON when zero, so these are routinely undefined. */
      cols.push(t, e.dx | 0, e.dy | 0, e.buttons | 0, e.wheel | 0, e.wheel_h | 0, mi);
      this.noteTime(t);
    }
    /* How much time one batch covers (≈ the agent's batch window while the
       mouse is moving). The live buffer is floored at this so a longer window
       on the agent can never turn into a stutter here; it decays so a quiet
       feed does not pin the floor at a stale value. */
    this.batchSpan = Math.max(Math.max(0, tLast - tFirst), this.batchSpan * 0.9);
    return tSum;
  },

  /** One delivery-latency sample: arrival wall clock minus the batch's mean
      capture time (`meanOffsetMs` after the session anchor). Sampled once at
      delivery, before playback or paint; averaging the capture times
      includes the wait inside the capture batch. Smoothed, except after an
      idle gap, where the first sample starts fresh. */
  sampleLatency(receivedUtcMs, meanOffsetMs, receivedAt) {
    if (this.anchorUtcUs === null) return;
    const ms = receivedUtcMs - (this.anchorUtcUs / 1000 + meanOffsetMs);
    if (!Number.isFinite(ms)) return;
    const idle = this.latencyReceivedAt === null || receivedAt - this.latencyReceivedAt >= NO_DATA_SECS;
    this.arrivalLatencyMs = idle ? ms : this.arrivalLatencyMs + (ms - this.arrivalLatencyMs) * 0.25;
    this.latencyReceivedAt = receivedAt;
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
  effectiveLiveBuffer(ctx = this.hooks.ctx()) {
    return Math.max(ctx.liveBuffer, Math.min(BUFFER_MS.max / 1000, this.batchSpan + 0.01));
  },

  /** UTC µs the newest received event was captured at, or null if the
      session anchor is unknown. */
  newestUtcUs() {
    if (this.anchorUtcUs === null || this.newestEventT === null) return null;
    return this.anchorUtcUs + this.newestEventT * 1e6;
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
      /* The pixel position and the model's carried remainder: without the
         remainder a seek would land the cursor a pixel off from a
         straight play-through. */
      ptrX: this.ptrX, ptrY: this.ptrY, ptrRx: this.pointer.rx, ptrRy: this.pointer.ry,
      lastEventT: this.lastEventT,
      totalCm: this.totalCm, totalDeg: this.totalDeg, totalPx: this.totalPx,
      totalClicks: this.totalClicks,
      held: this.held,
      vrefDesk: this.vrefDesk, vrefAim: this.vrefAim, vrefPtr: this.vrefPtr,
      clickRate: this.clickRate.save(), eventRate: this.eventRate.save(),
    };
  },

  restore(cp) {
    this.cursor = cp.i;
    this.deskX = cp.deskX; this.deskY = cp.deskY;
    this.yawRaw = cp.yawRaw; this.pitch = cp.pitch;
    this.ptrX = cp.ptrX; this.ptrY = cp.ptrY;
    this.pointer.rx = cp.ptrRx; this.pointer.ry = cp.ptrRy;
    this.lastEventT = cp.lastEventT;
    this.totalCm = cp.totalCm; this.totalDeg = cp.totalDeg; this.totalPx = cp.totalPx;
    this.totalClicks = cp.totalClicks;
    this.held = cp.held | 0;
    this.vrefDesk = cp.vrefDesk; this.vrefAim = cp.vrefAim; this.vrefPtr = cp.vrefPtr;
    this.clickRate.load(cp.clickRate);
    this.eventRate.load(cp.eventRate);
    this.deskTrail.clear(); this.aimTrail.clear(); this.ptrTrail.clear();
    this.rings.clear(); this.wheelTicks.clear();
    this.speedCm = 0; this.speedDeg = 0; this.speedPx = 0;
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
  advanceLive(dt, ctx) {
    if (!this.haveTimeline) return;
    if (!this.clockStarted) {
      this.playT = this.tStart;
      this.clockStarted = true;
      return;
    }
    const target = this.tEnd - this.effectiveLiveBuffer(ctx);
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
  tick(nowSec, ctx = this.hooks.ctx()) {
    const dt = this.lastWall ? clamp(nowSec - this.lastWall, 0, 0.25) : 0;
    this.lastWall = nowSec;
    if (this.loading) return this;

    if (ctx.mode === "live") {
      this.advanceLive(dt, ctx);
    } else if (ctx.replayPlaying) {
      this.playT = Math.min(this.tEnd, this.playT + dt * ctx.replaySpeed);
      if (this.playT >= this.tEnd) this.hooks.onReplayEnd();
    }

    this._tickCm = 0;
    this._tickDeg = 0;
    this._tickPx = 0;
    this.consume(MAX_CONSUME_PER_TICK);

    /* ~100ms smoothing on the readouts. */
    if (dt > 0) {
      const a = 1 - Math.exp(-dt / 0.1);
      this.speedCm += (this._tickCm / dt - this.speedCm) * a;
      this.speedDeg += (this._tickDeg / dt - this.speedDeg) * a;
      this.speedPx += (this._tickPx / dt - this.speedPx) * a;
      /* Colour reference decays slowly so a single flick does not wash the
         palette out for the rest of the session. */
      const k = Math.pow(0.5, dt / 8);
      this.vrefDesk = Math.max(25, this.vrefDesk * k);
      this.vrefAim = Math.max(250, this.vrefAim * k);
      this.vrefPtr = Math.max(5000, this.vrefPtr * k);
    }

    this.trim(ctx);
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
      if (!this._quiet && m.t >= this.playT - 1.0) this.hooks.onMarker(m);
    }
    return applied;
  },

  /** Integrate event `i`: its batch context, then motion, then buttons and
      wheel. */
  applyIdx(i) {
    const ev = this.ev;
    this.applyMeta(this.metaAt(i));
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
      this.applyMotion(edx, edy, et, dt, visible);
      this.applyPointer(edx, edy, et, dt, visible);
    }
    this.applyButtons(ev.b[i], et, fresh);
    if (fresh) this.applyWheel(ev.w[i], ev.wh[i], et);
  },

  /** The batch context an event was captured in: game, sens, cursor. */
  applyMeta(meta) {
    if (!meta) return;
    if (meta.sens === null) meta.sens = this.sensFor(meta.game);
    this.sens = meta.sens;
    this.game = meta.game;
    this.pointerLocked = meta.locked;
    this.cursorX = meta.cx;
    this.cursorY = meta.cy;
    this.screenW = meta.sw;
    this.screenH = meta.sh;
  },

  /** One report into the hand (cm) and the crosshair (degrees). */
  applyMotion(edx, edy, et, dt, visible) {
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

    /* Yaw is kept unwrapped in panel space: the head glides across the ±180°
       seam (drawn as a dashed line) instead of teleporting to the far edge,
       and the camera follows it continuously. */
    if (visible) {
      this.deskTrail.push(this.deskX, this.deskY, et, vCm, false);
      this.aimTrail.push(this.yawRaw, this.pitch, et, vDeg, false);
    }
  },

  /** The same report through Windows' pointer ballistics. The position moves
      by whole pixels, as the cursor does; the speed readout and the trail
      colour use the movement before truncation, or a crawl would read as a
      burst of 1000 px/s steps. */
  applyPointer(edx, edy, et, dt, visible) {
    const pm = this.pointer;
    pm.apply(edx, edy);
    const dpx = Math.hypot(pm.moveX, pm.moveY);
    this.totalPx += dpx;
    this._tickPx += dpx;
    const vPx = dt > 0 ? dpx / dt : 0;
    if (vPx > this.vrefPtr) this.vrefPtr = vPx;
    if (pm.outX !== 0 || pm.outY !== 0) {
      this.ptrX += pm.outX;
      this.ptrY += pm.outY;
      if (visible) this.ptrTrail.push(this.ptrX, this.ptrY, et, vPx, false);
    }
  },

  /** Press: one expanding ring (the click flash) plus the button joins
      `held`, which draws a steady ring at the head until its release. */
  applyButtons(b, et, fresh) {
    if (!(b & (BTN.ANY_DOWN | BTN.ANY_UP))) return;
    this.held = (this.held | (b & BTN.ANY_DOWN)) & ~((b & BTN.ANY_UP) >> 1);
    this.dirty = true;
    if (!(b & BTN.ANY_DOWN)) return;
    this.totalClicks++;
    this.clickRate.add(et, 1);
    if (fresh) {
      const kind = (b & BTN.LEFT_DOWN) ? 0 : (b & BTN.RIGHT_DOWN) ? 1 : 2;
      this.rings.push({
        t: et, kind: kind,
        dx: this.deskX, dy: this.deskY,
        ax: this.yawRaw, ay: this.pitch,
        px: this.ptrX, py: this.ptrY,
      });
    }
  },

  applyWheel(w, wh, et) {
    if (w !== 0) this.wheelTicks.push({ t: et, dir: w > 0 ? 1 : -1, axis: 0 });
    if (wh !== 0) this.wheelTicks.push({ t: et, dir: wh > 0 ? 1 : -1, axis: 1 });
  },

  /** Bound memory: drop consumed events in live mode and expire visuals.
      Reachable from the render loop *and* from a timer, so a hidden tab
      still reclaims. */
  trim(ctx = this.hooks.ctx()) {
    if (ctx.mode === "live") {
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
    this.ptrTrail.trim(cutoff);
    this.rings.trim(this.playT - RING_LIFE);
    this.wheelTicks.trim(this.playT - WHEEL_LIFE);
  },

  /** True while anything on screen is still animating. */
  visualsAlive() {
    return this.deskTrail.n > 0 || this.aimTrail.n > 0 || this.ptrTrail.n > 0 ||
           this.rings.n > 0 || this.wheelTicks.n > 0;
  },

  draw() {
    for (const p of this.panels) p.render(this);
  },
};
Object.assign(engine, initialEngineState());

/* =====================================================================
   Panel rendering
   ===================================================================== */

/* What each panel reads off the engine, chosen once per panel instead of a
   string compare per frame: the trail, the head position, the colour scale,
   and where a click ring sits in this space. */
const PANEL_SPACES = {
  desk: {
    trail: (e) => e.deskTrail, x: (e) => e.deskX, y: (e) => e.deskY, vref: (e) => e.vrefDesk,
    ringX: (r) => r.dx, ringY: (r) => r.dy,
  },
  aim: {
    trail: (e) => e.aimTrail, x: (e) => e.yawRaw, y: (e) => e.pitch, vref: (e) => e.vrefAim,
    ringX: (r) => r.ax, ringY: (r) => r.ay,
  },
  ptr: {
    trail: (e) => e.ptrTrail, x: (e) => e.ptrX, y: (e) => e.ptrY, vref: (e) => e.vrefPtr,
    ringX: (r) => r.px, ringY: (r) => r.py,
  },
};

class Panel {
  constructor(canvasId, opts) {
    this.canvas = document.getElementById(canvasId);
    /* alpha:false lets the compositor skip a blend of the whole canvas every
       frame; desynchronized lets it skip a frame of latency getting there.
       Both mean the canvas is opaque, so the background is painted, not
       cleared. The one exception is an OBS overlay with a see-through
       background, which needs the alpha channel and pays for it. */
    this.opaque = !(OBS && OBS.bg.alpha < 1);
    /* The overlay's configured background; the dashboard paints PAL.canvas,
       which follows the theme. */
    this.bg = OBS ? OBS.bg : null;
    this.hidden = false;
    this.ctx = this.canvas.getContext("2d", { alpha: !this.opaque, desynchronized: true });
    /* Which of the engine's three integrations this panel draws: the hand
       in cm, the crosshair in degrees, or Windows' cursor in pixels. */
    this.space = opts.space;           // "desk" | "aim" | "ptr"
    this.of = PANEL_SPACES[opts.space];
    this.isDesk = opts.space === "desk";
    this.isAim = opts.space === "aim";
    this._head = { x: 0, y: 0 };
    /* A resized canvas is a blank canvas: whoever owns the frame loop is told
       so it repaints even when nothing else changed. */
    this.onResize = opts.onResize || (() => {});
    this.unit = opts.unit;             // "cm" | "deg" | "px"
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
    this.onResize();
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
    if (!Number.isFinite(target) || target <= 0) return;
    const ratio = target / this.scale;
    if (ratio > 4 || ratio < 0.25) this.scale = target;
    else this.scale += (target - this.scale) * 0.07;
  }

  render(eng) {
    if (this.hidden) return;
    const ctx = this.ctx;
    const of = this.of;
    const trail = of.trail(eng);
    const head = this._head;
    head.x = of.x(eng); head.y = of.y(eng);
    const vref = of.vref(eng);
    this.autoScale(head, trail);

    ctx.save();
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    if (this.opaque) {
      ctx.fillStyle = this.bg ? this.bg.css : PAL.canvas;
      ctx.fillRect(0, 0, this.w, this.h);
    } else {
      ctx.clearRect(0, 0, this.w, this.h);
      if (this.bg.alpha > 0) {
        ctx.fillStyle = this.bg.css;
        ctx.fillRect(0, 0, this.w, this.h);
      }
    }

    if (!OBS || OBS.grid) this.drawGrid(ctx, eng, this.isAim);
    this.drawTrail(ctx, trail, eng, vref);
    this.drawRings(ctx, eng);
    if (eng.held) this.drawHeld(ctx, eng, head);
    this.drawHead(ctx, head);
    if (this.isDesk) {
      this.drawWheel(ctx, eng);
      /* The desktop-cursor miniature is a debugging aid, not stream content. */
      if (!OBS) this.drawCursorGhost(ctx, eng);
    }
    if (!OBS || OBS.legend) this.drawLegend(ctx, vref);

    ctx.restore();
  }

  /** The unit as the labels print it, after a number. */
  unitLabel() {
    return this.unit === "cm" ? " cm" : this.unit === "deg" ? "°" : " px";
  }

  drawGrid(ctx, eng, isAim) {
    const halfX = this.w / 2 / this.scale;
    const halfY = this.h / 2 / this.scale;
    const step = niceStep(halfX * 2 / 7);
    const stepText = step >= 1 ? step.toFixed(0) : step.toFixed(2);
    /* The pointer panel's subtitle also says which Windows it is showing:
       the same recording reads differently with precision off. */
    const label = "grid " + stepText + this.unitLabel() +
      (this.space === "ptr" ? " · " + eng.pointer.describe() : "");
    if (this.subEl && this.subEl.textContent !== label) this.subEl.textContent = label;

    ctx.lineWidth = 1;
    const x0 = Math.floor((this.cx - halfX) / step) * step;
    const x1 = this.cx + halfX;
    const y0 = Math.floor((this.cy - halfY) / step) * step;
    const y1 = this.cy + halfY;

    ctx.beginPath();
    ctx.strokeStyle = PAL.grid;
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
    ctx.strokeStyle = PAL.gridAxis;
    const oy = Math.round(this.toY(0)) + 0.5;
    ctx.moveTo(0, oy); ctx.lineTo(this.w, oy);
    if (!isAim) {
      const ox = Math.round(this.toX(0)) + 0.5;
      ctx.moveTo(ox, 0); ctx.lineTo(ox, this.h);
    } else {
      for (let x = Math.ceil((this.cx - halfX) / 360) * 360; x <= x1; x += 360) {
        const px = Math.round(this.toX(x)) + 0.5;
        ctx.moveTo(px, 0); ctx.lineTo(px, this.h);
      }
    }
    ctx.stroke();

    if (isAim) {
      /* Yaw seams: the ±180° boundary of each turn, i.e. every odd multiple
         of 180°. Crossing one means you are now facing directly behind
         where "recenter" left you. */
      ctx.beginPath();
      ctx.strokeStyle = PAL.gridWrap;
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
      const col = PAL.ring[BTN_KIND[i][1]];
      const rad = (13 + 6 * n) * S;
      n++;
      ctx.fillStyle = "rgba(" + col + ",0.22)";
      ctx.beginPath(); ctx.arc(x, y, rad * 0.6, 0, Math.PI * 2); ctx.fill();
      ctx.strokeStyle = "rgba(" + col + ",0.9)";
      ctx.lineWidth = 2.2 * S;
      ctx.beginPath(); ctx.arc(x, y, rad, 0, Math.PI * 2); ctx.stroke();
    }
  }

  drawRings(ctx, eng) {
    const colors = PAL.ring;
    const now = eng.playT;
    const of = this.of;
    for (let k = 0; k < eng.rings.n; k++) {
      const r = eng.rings.at(k);
      const age = now - r.t;
      if (age < 0 || age > RING_LIFE) continue;
      const f = age / RING_LIFE;
      const x = this.toX(of.ringX(r));
      const y = this.toY(of.ringY(r));
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
    ctx.fillStyle = PAL.headHalo;
    ctx.beginPath(); ctx.arc(x, y, 9 * S, 0, Math.PI * 2); ctx.fill();
    ctx.fillStyle = PAL.head;
    ctx.beginPath(); ctx.arc(x, y, 2.6 * S, 0, Math.PI * 2); ctx.fill();
    ctx.strokeStyle = PAL.cross;
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

    ctx.strokeStyle = PAL.rail;
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
        ctx.strokeStyle = "rgba(" + PAL.ring[1] + "," + alpha + ")";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.moveTo(x, hy - 6); ctx.lineTo(x, hy + 6);
        ctx.stroke();
      } else {
        net += t.dir;
        const y = by - t.dir * (8 + 38 * f);
        ctx.strokeStyle = "rgba(" + PAL.accentRgb + "," + alpha + ")";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.moveTo(bx - 7, y); ctx.lineTo(bx + 7, y);
        ctx.stroke();
      }
    }

    ctx.font = "10px ui-monospace, monospace";
    ctx.textAlign = "center";
    if (net !== 0) {
      ctx.fillStyle = PAL.text;
      ctx.fillText((net > 0 ? "+" : "") + net, bx, by + 62);
    }
    if (sawH) {
      /* The tilt axis only draws its rail once the mouse has actually used
         it — most do not have one. */
      ctx.strokeStyle = PAL.rail;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(hx - 34, hy); ctx.lineTo(hx + 34, hy);
      ctx.stroke();
      if (netH !== 0) {
        ctx.fillStyle = "rgb(" + PAL.ring[1] + ")";
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
    ctx.fillStyle = PAL.ghostFill;
    ctx.fillRect(x, y, gw, gh);
    ctx.strokeStyle = PAL.ghostLine;
    ctx.lineWidth = 1;
    ctx.strokeRect(x + 0.5, y + 0.5, gw - 1, gh - 1);

    const px = x + clamp(eng.cursorX / eng.screenW, 0, 1) * gw;
    const py = y + clamp(eng.cursorY / eng.screenH, 0, 1) * gh;
    ctx.globalAlpha = 0.85;
    ctx.fillStyle = PAL.accent;
    ctx.beginPath();
    ctx.arc(px, py, 2.4, 0, Math.PI * 2);
    ctx.fill();

    ctx.globalAlpha = 0.8;
    ctx.fillStyle = PAL.textDim;
    ctx.font = "9px ui-monospace, monospace";
    ctx.fillText("desktop cursor " + eng.screenW + "×" + eng.screenH, x, y - 4);
    ctx.restore();
  }

  drawLegend(ctx, vref) {
    const x = 12, y = this.h - 22 * S, w = 108 * S, h = 6 * S;
    for (let i = 0; i < VLEVELS; i++) {
      ctx.fillStyle = RAMP_CSS[i];
      ctx.fillRect(x + (w / VLEVELS) * i, y, w / VLEVELS + 0.6, h);
    }
    ctx.fillStyle = PAL.textDim;
    ctx.font = Math.round(10 * S) + "px ui-monospace, monospace";
    ctx.fillText("0", x, y + 16 * S);
    const top = Math.round(vref) + this.unitLabel() + "/s";
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

const repaint = () => { engine.dirty = true; };
const deskPanel = new Panel("deskCanvas", {
  space: "desk", unit: "cm", minHalf: 1.2, subId: "deskSub", hintId: "deskHint", flashId: "deskFlash",
  onResize: repaint,
});
const aimPanel = new Panel("aimCanvas", {
  space: "aim", unit: "deg", minHalf: 6, subId: "aimSub", hintId: "aimHint", flashId: "aimFlash",
  onResize: repaint,
});
/* 1.2 cm of hand at 800 cpi is ~380 counts, about that many pixels 1:1. */
const ptrPanel = new Panel("ptrCanvas", {
  space: "ptr", unit: "px", minHalf: 200, subId: "ptrSub", hintId: "ptrHint", flashId: "ptrFlash",
  onResize: repaint,
});
const PANELS = [deskPanel, aimPanel, ptrPanel];
engine.panels = PANELS;

/* =====================================================================
   UI: mode switching, live socket, replay transport, stats
   ===================================================================== */

const $ = (id) => document.getElementById(id);

/* ---------- rate-limited console reporting ----------
   A page fed at 1kHz can produce thousands of identical complaints a second,
   and a console with a thousand identical lines in it is a console nobody
   reads. Each kind is loud the first time and then at most once every
   WARN_EVERY seconds; the counters in the stats bar carry the rest. */
const WARN_EVERY = 30;
const warnedAt = Object.create(null);

/** Report `kind` (with optional detail) at most once per WARN_EVERY.
    Returns whether it was actually logged — which the tests read. */
function warn(kind, detail) {
  const now = nowSec();
  const last = warnedAt[kind];
  if (last !== undefined && now - last < WARN_EVERY) return false;
  warnedAt[kind] = now;
  try {
    console.warn("[telemouse] " + kind + (detail === undefined || detail === null ? "" : ": " + detail));
  } catch (e) { /* no console (some OBS builds) */ }
  return true;
}

const LIVE_BUFFER_DEFAULT = BUFFER_MS.dflt / 1000;
const LIVE_BUFFER_KEY = "telemouse.liveBuffer";

/** A two-state button: the `on` class the stylesheet keys on, and the
    `aria-pressed` a screen reader announces, always together. */
function setPressed(el, on) {
  el.classList.toggle("on", !!on);
  el.setAttribute("aria-pressed", on ? "true" : "false");
}

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
  /* ?session=<id>: the recording to open once the list is in. */
  pendingSessionId: null,
  _wallSec: null,
  liveBuffer: LIVE_BUFFER_DEFAULT,
  bridge: null,
  bridgeAt: 0,
  lossToastShown: false,
  /* Live-feed bookkeeping (see `feedState`). `wsOpen` is the socket, not the
     data: an open socket with nothing coming down it is exactly the case the
     "waiting for capture" state exists for. */
  wsOpen: false,
  lastBatchAt: 0,
  feedSince: 0,
  /* When the page last had data: the newest batch, or the moment the page
     loaded while there has been none. Unlike `lastBatchAt` no socket event
     resets it; the overlay's "no feed" timer runs off this alone. */
  lastDataAt: nowSec(),
  /* When the capture agent last said hello with nothing to report. 0 means
     never — which is also every capture agent that predates the heartbeat,
     so the page falls back to exactly its old behaviour against one. Not
     reset by a reconnect: the agent is a different process from the socket,
     and a heartbeat from half a second ago is still evidence it is alive. */
  lastHeartbeatAt: 0,
  udpAddr: UDP_ADDR,
  /* Frames the page could not use: unparseable WebSocket messages and bad
     replay lines. Shown in the stats bar next to the bridge's own parse
     errors, which count the datagrams the *bridge* could not use. */
  badFrames: 0,

  setConn(cls, text) {
    const name = "pill" + (cls ? " " + cls : "");
    const p = $("connPill");
    if (p.className !== name) p.className = name;
    const t = $("connText");
    /* The pill ellipsizes in a narrow window; the tooltip keeps the whole text. */
    if (t.textContent !== text) { t.textContent = text; p.title = text; }
  },

  /** A batch arrived. */
  onBatch() {
    this.lastBatchAt = this.lastDataAt = nowSec();
    /* Recovery is instant rather than "within the next 100ms repaint": an
       overlay that stays dimmed for a tenth of a second after the feed comes
       back is a visible flicker on stream. */
    if (OBS && staleView.kind !== "live") updateStale();
  },

  /** The capture agent is up with nothing to report.

      Liveness only: it is deliberately not `lastDataAt`, because the number
      the badge and the pill count is seconds since the hand last moved, and a
      heartbeat is not the hand moving. */
  onHeartbeat() {
    this.lastHeartbeatAt = nowSec();
    /* Same reason as `onBatch`: an overlay that keeps saying "no feed" for
       another tenth of a second after the agent is heard from is a visible
       flicker on stream. */
    if (OBS && staleView.kind === "down") updateStale();
  },

  /** Seconds since the last heartbeat; Infinity when there has never been
      one, which is what makes `AGENT_ALIVE_SECS` a one-sided test. */
  heartbeatAge(now) {
    if (now === undefined) now = nowSec();
    return this.lastHeartbeatAt ? Math.max(0, now - this.lastHeartbeatAt) : Infinity;
  },

  /** The pill's live-mode states. The socket's own states (connecting,
      disconnected, retrying) are written where they happen and win: this
      only speaks while the socket is up. */
  refreshConn() {
    if (this.mode !== "live" || !this.wsOpen) return;
    const now = nowSec();
    const p = connPillState(
      feedState({
        connected: true,
        lastBatchAt: this.lastBatchAt,
        since: this.feedSince,
        now: now,
        heartbeatAge: this.heartbeatAge(now),
        staleAfter: NO_DATA_SECS,
      }),
      this.udpAddr
    );
    this.setConn(p.cls, p.text);
  },

  /** A frame the page could not use. Counted always, logged rarely. */
  noteBad(kind, detail) {
    this.badFrames++;
    warn(kind, detail);
  },

  setMode(mode) {
    if (mode === this.mode) return;
    this.mode = mode;
    for (const b of $("modeSwitch").children) setPressed(b, b.dataset.mode === mode);
    $("transport").classList.toggle("hidden", mode !== "replay");
    $("bufCtl").style.display = mode === "live" ? "" : "none";
    this.paused = false;
    this.lossToastShown = false;
    $("btnPause").classList.remove("on");
    $("btnPause").firstChild.nodeValue = "Pause";
    /* Cancel any replay load still streaming, and hand the integrator back
       to the render loop. */
    this.loadToken++;
    engine.reset();
    engine.loading = false;
    this.renderNotches();
    if (mode === "live") {
      this.setReplayPlaying(false);
      this.connect();
    } else {
      this.disconnect();
      this.setConn("", "replay");
      this.loadSessionList();
    }
    this.renderTransport();
    engine.dirty = true;
  },

  setLiveBuffer(seconds) {
    this.liveBuffer = clamp(seconds, BUFFER_MS.min / 1000, BUFFER_MS.max / 1000);
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
    try { ws = new WebSocket(url); } catch (e) { warn("ws open", e && e.message); this.scheduleReconnect(); return; }
    this.ws = ws;
    ws.onopen = () => {
      this.wsRetry = 0;
      this.wsOpen = true;
      /* A fresh socket has seen nothing yet: the feed age starts here, not
         at whatever the last session left behind. */
      this.lastBatchAt = 0;
      this.feedSince = nowSec();
      this.refreshConn();
    };
    ws.onmessage = (m) => {
      const receivedUtcMs = Date.now(), receivedAt = nowSec();
      let env;
      try { env = JSON.parse(m.data); } catch (e) {
        this.noteBad("ws parse", e && e.message);
        return;
      }
      ingest(env, receivedUtcMs, receivedAt);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.wsOpen = false;
      if (this.mode === "live") { this.setConn("err", "disconnected"); this.scheduleReconnect(); }
    };
    /* `onclose` always follows, and carries the retry; this is only here so
       the reason shows up once in the console instead of never. */
    ws.onerror = () => { warn("ws error", url); };
  },

  disconnect() {
    clearTimeout(this.wsTimer);
    this.wsOpen = false;
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
    let failed = false;
    try {
      const r = await fetch("/api/sessions");
      if (!r.ok) throw new Error("http " + r.status);
      list = await r.json();
    } catch (e) {
      failed = true;
      list = [];
      warn("session list", e && e.message);
    }
    if (!Array.isArray(list) || !list.length) {
      sel.innerHTML = failed
        ? "<option value=''>could not list recordings</option>"
        : "<option value=''>no recordings found</option>";
      this.setConn(failed ? "err" : "warn", failed ? "recording list failed" : "no recordings");
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
      const startMs = !isNil(s.started_utc_us) ? s.started_utc_us / 1000 : s.modified_epoch_ms;
      const when = startMs ? new Date(startMs).toLocaleString() : "";
      const span = (!isNil(s.started_utc_us) && !isNil(s.ended_utc_us))
        ? ", " + fmtTime((s.ended_utc_us - s.started_utc_us) / 1e6).replace(/\.\d+$/, "") : "";
      o.textContent = s.id + "  (" + (when ? when + span + ", " : "") + kb + " KB)";
      sel.appendChild(o);
    }
    if (this.pendingUtcUs !== null) {
      const target = this.pendingUtcUs;
      this.pendingUtcUs = null;
      if (this.gotoUtcUs(target)) return;
    }
    if (this.pendingSessionId !== null) {
      const id = this.pendingSessionId;
      this.pendingSessionId = null;
      if (list.some((s) => s.id === id)) {
        sel.value = id;
        await this.loadSession(id);
        return;
      }
      this.toast("session", "no recording named " + id, true);
    }
    await this.loadSession(list[0].id);
  },

  /** When a listed recording ends. A recording still being written has a
      real end (the server reads it off the file's tail), so a missing one
      means an envelope with no batches yet: it ends where it starts. */
  sessionEnd(s) {
    return isNil(s.ended_utc_us) ? s.started_utc_us : s.ended_utc_us;
  },

  /** Which recording was running at `utcUs`: one whose span contains it
      (1 s of slack past the last batch), else the one that ended most
      recently before it. */
  sessionAt(utcUs) {
    let best = null;
    for (const s of this.sessions || []) {
      if (isNil(s.started_utc_us) || s.started_utc_us > utcUs) continue;
      if (utcUs <= this.sessionEnd(s) + 1e6) return s;
      if (!best || this.sessionEnd(s) > this.sessionEnd(best)) best = s;
    }
    return best;
  },

  /** Replay the moment `utcUs`: load the recording that covers it if it is
      not the loaded one, then seek there. Returns false (with a toast) when
      no recording could plausibly hold it. */
  gotoUtcUs(utcUs) {
    if (!Number.isFinite(utcUs)) return false;
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

  /** Stream a recording in and parse it as it arrives (see `streamLines`),
      building seek checkpoints between slices so the page stays interactive
      and shows progress while a large session loads. */
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
    let bad = 0;

    /* A line that is not JSON is a bad frame: counted, logged rarely, and
       skipped. Anything `ingest` throws is a bug in this page, not in the
       recording, and is left to surface. */
    const consumeLine = (line) => {
      let env;
      try { env = JSON.parse(line); } catch (e) {
        bad++;
        this.noteBad("replay line", e && e.message);
        return;
      }
      ingest(env);
    };
    const onSlice = async (lines, bytes) => {
      this.setConn("warn", "loading " + id + " · " +
        INT_FMT.format(lines) + " lines" +
        (total ? " · " + Math.round(100 * bytes / total) + "%" : ""));
      await nextTask();
      if (token !== this.loadToken) return false;
      engine.extendCheckpoints();
      return true;
    };

    let finished;
    try {
      finished = await streamLines(res, consumeLine, onSlice, () => token === this.loadToken);
    } catch (e) {
      if (token === this.loadToken) { engine.loading = false; this.setConn("err", "load interrupted"); }
      /* A read that failed is the network's doing; anything else came out of
         the engine and must not vanish into "load interrupted". */
      if (!(e instanceof StreamReadError)) throw e;
      warn("replay load", e.message);
      return;
    }

    if (!finished || token !== this.loadToken) return;

    engine.extendCheckpoints();
    engine.rewind();
    engine.loading = false;
    this.loadedSession = id;
    engine.seek(engine.tStart);
    this.setConn("ok", id + " · " + INT_FMT.format(engine.ev.n) + " events" +
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
    /* The label already says what a press does; no aria-pressed on top. */
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
    for (const p of PANELS) p.flash();
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
    this.bridgeAt = nowSec();
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
    /* The pointer panel is only as true as the settings it was given. */
    const ph = engine.pointerSettings ? ""
      : (engine.session ? "no pointer settings in this session" : "no session envelope yet") +
        " — assuming a fresh Windows install (precision on, 10/20, 96 dpi)";
    if (ptrPanel.hintEl.textContent !== ph) ptrPanel.hintEl.textContent = ph;
  },
};

/* The engine's view of the UI: one object, refreshed on each call rather
   than allocated (a replay load ingests millions of lines). */
const uiCtx = defaultEngineCtx();
function uiContext() {
  uiCtx.mode = ui.mode;
  uiCtx.liveBuffer = ui.liveBuffer;
  uiCtx.replayPlaying = ui.replayPlaying;
  uiCtx.replaySpeed = ui.replaySpeed;
  return uiCtx;
}

/* The engine's outbound events land on `ui`. Each hook looks its handler up
   at call time, so a handler replaced later (the tests do) is the one used. */
engine.hooks = {
  ctx: uiContext,
  onVizStats: (env) => ui.onVizStats(env),
  onHeartbeat: () => ui.onHeartbeat(),
  onBatch: () => ui.onBatch(),
  onDataLoss: (drops, lost) => ui.onDataLoss(drops, lost),
  onSessionRestart: (from, to) => ui.onSessionRestart(from, to),
  onSessionChanged: () => ui.onSessionChanged(),
  onMarker: (m) => ui.onMarker(m),
  onReplayEnd: () => ui.setReplayPlaying(false),
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

/* Lines parsed between two yields to the event loop during a replay load. */
const LOAD_SLICE_LINES = 20000;

/** A response body that could not be read: the network's fault, not the
    page's, and the one failure a replay load reports as "interrupted". */
class StreamReadError extends Error {}

/** Feed every non-empty line of a fetch Response to `onLine` (trimmed) as
    the body streams in.

    Sessions are hundreds of megabytes; `await response.text()` would hold
    the whole file as one string *and* a second copy as an array of lines
    before a single event existed. Reading incrementally keeps peak memory at
    one chunk, and every `sliceLines` lines `onSlice(lines, bytes)` is awaited
    so the caller can yield, show progress, or say stop (by returning false).
    `alive()` is asked after every read; false cancels the read too.

    Returns true when the whole body was read, false when it was stopped.
    A failed read throws `StreamReadError`; whatever `onLine` throws
    propagates unchanged. */
async function streamLines(res, onLine, onSlice, alive = () => true, sliceLines = LOAD_SLICE_LINES) {
  const feed = (line) => { if (line) onLine(line); };
  if (!res.body || !res.body.getReader) {
    /* No streams (very old browser): fall back to one big string. */
    let text;
    try { text = await res.text(); } catch (e) { throw new StreamReadError(e && e.message); }
    for (const line of text.split("\n")) feed(line.trim());
    return true;
  }
  const reader = res.body.getReader();
  const stop = async () => { try { await reader.cancel(); } catch (e) { /* already closed */ } return false; };
  const dec = new TextDecoder();
  let carry = "", bytes = 0, lines = 0;
  for (;;) {
    let chunk;
    try { chunk = await reader.read(); } catch (e) { throw new StreamReadError(e && e.message); }
    if (!alive()) return stop();
    if (chunk.done) break;
    bytes += chunk.value.length;
    carry += dec.decode(chunk.value, { stream: true });
    let start = 0, nl;
    while ((nl = carry.indexOf("\n", start)) >= 0) {
      feed(carry.slice(start, nl).trim());
      start = nl + 1;
      if (++lines % sliceLines === 0) {
        carry = carry.slice(start);
        start = 0;
        if (!(await onSlice(lines, bytes)) || !alive()) return stop();
      }
    }
    carry = carry.slice(start);
  }
  feed((carry + dec.decode()).trim());
  return true;
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
function ingest(env, receivedUtcMs = null, receivedAt = nowSec()) {
  const ctx = uiContext();
  if (!PROFILE) { engine.ingest(env, receivedUtcMs, receivedAt, ctx); return; }
  performance.mark("ig0");
  engine.ingest(env, receivedUtcMs, receivedAt, ctx);
  performance.mark("ig1");
  measure("ingest", "ig0", "ig1", "ingest");
}

/* =====================================================================
   Stats bar
   ===================================================================== */

let lastStatsPaint = 0;
let fpsEma = 60;
let refreshEst = 60;

/* Every element the stats bar writes, looked up once. This runs ten times a
   second over ~25 ids; getElementById is cheap but not free, and the lookups
   were most of what the function did. */
const SE = {};
for (const id of [
  "sSpeedCm", "sSpeedDeg", "sSpeedPx", "sDistM", "sDistDeg", "sCpm", "sClicks", "sEps",
  "sLat", "sLatUnit", "statLat", "sAge", "statAge", "sLag", "statLag", "sFps", "sFpsRef", "statFps",
  "sDrops", "statDrops", "sLost", "statLost", "sAbs", "statAbs",
  "sBad", "statBad", "statBridge", "sBridge", "sBridgeSub",
  "sLocked", "sCpi", "gameText", "sProf",
]) SE[id] = $(id);

/** Write only when the text actually changed: an unchanged textContent write
    still dirties layout for the node. */
function setText(el, v) {
  if (el && el.textContent !== v) el.textContent = v;
}

/** Same for a tooltip, which is a much longer string. */
function setTitle(el, v) {
  if (el && el.title !== v) el.title = v;
}

/* Tooltip inputs, so the two long strings below are rebuilt when something
   in them changes rather than ten times a second. */
let cpiTipKey = null;
let bridgeTipKey = null;

/* Stats-bar tiles that show exactly a HUD readout: element id → hudValue key. */
const SHARED_READOUTS = [
  ["sSpeedCm", "speed"], ["sSpeedDeg", "aim"], ["sSpeedPx", "pointer"], ["sDistM", "dist"],
  ["sDistDeg", "aimdist"], ["sCpm", "cpm"], ["sEps", "eps"], ["sAge", "eventage"],
];

function paintStats(now) {
  const e = engine;
  for (const [id, key] of SHARED_READOUTS) setText(SE[id], hudValue(key, e, now));
  setText(SE.sClicks, hudValue("clicks", e, now) + " total");

  /* Event age keeps increasing during normal inactivity. Delivery latency
     only changes on arrival; old samples become idle. Neither is a replay metric. */
  const live = ui.mode === "live";
  SE.statAge.classList.toggle("dim", !live);
  const latency = hudValue("latency", e, now);
  const measured = latency !== "—" && latency !== "idle";
  setText(SE.sLat, latency);
  setText(SE.sLatUnit, measured ? "ms" : "");
  SE.statLat.classList.toggle("dim", !measured);
  SE.statLat.classList.toggle("alert", measured && e.arrivalLatencyMs > 25);
  SE.statLat.classList.toggle("warn", measured && e.arrivalLatencyMs > 10 && e.arrivalLatencyMs <= 25);

  if (!live) {
    setText(SE.sLag, "—");
    SE.statLag.classList.remove("alert");
    SE.statLag.classList.add("dim");
  } else {
    const lagMs = e.haveTimeline ? Math.max(0, (e.tEnd - e.playT) * 1000) : 0;
    setText(SE.sLag, fmtInt(Math.round(lagMs)));
    SE.statLag.classList.remove("dim");
    SE.statLag.classList.toggle("alert", lagMs > 250);
  }

  setText(SE.sFps, String(Math.round(fpsEma)));
  setText(SE.sFpsRef, "/ ~" + Math.round(refreshEst) + "Hz");
  SE.statFps.classList.toggle("alert", fpsEma < 0.8 * refreshEst);

  setText(SE.sDrops, fmtInt(e.drops));
  SE.statDrops.classList.toggle("alert", e.drops > 0);
  setText(SE.sLost, fmtInt(e.lostBatches));
  SE.statLost.classList.toggle("alert", e.lostBatches > 0);
  setText(SE.sAbs, fmtInt(e.absFrames));
  SE.statAbs.classList.toggle("dim", e.absFrames === 0);
  setText(SE.sBad, fmtInt(ui.badFrames));
  SE.statBad.classList.toggle("dim", ui.badFrames === 0);
  SE.statBad.classList.toggle("alert", ui.badFrames > 0);

  paintBridge(now);

  setText(SE.sLocked, e.pointerLocked ? "locked" : "desktop");
  const s = e.sens;
  setText(SE.sCpi, Math.round(e.cpi) + " / " +
    (s ? s.sens.toFixed(2) : "—") + (s && s.fallback ? "*" : ""));
  const cpiKey = s
    ? e.cpi + "|" + s.sens + "|" + s.yaw_coeff + "|" + s.pitch_coeff + "|" + e.anchorUncertaintyUs
    : "";
  if (cpiKey !== cpiTipKey) {
    cpiTipKey = cpiKey;
    setTitle(SE.sCpi, s
      ? "cpi " + e.cpi + ", sens " + s.sens + ", yaw " + s.yaw_coeff + "°/count, pitch " + s.pitch_coeff +
        (e.anchorUncertaintyUs === null ? "" : ", anchor ±" + e.anchorUncertaintyUs + "µs")
      : "");
  }
  setText(SE.gameText, e.game || "no game");

  if (PROFILE) setText(SE.sProf, prof.text);
  ui.refreshSensHint();
}

/** Bridge-side health, pushed by the server once a second — no polling.
    A bridge built without the `observability` feature never pushes, which is
    a tile that reads "n/a", not an error. */
function paintBridge(now) {
  const b = ui.bridge;
  const tile = SE.statBridge;
  if (!b) {
    setText(SE.sBridge, "n/a");
    setText(SE.sBridgeSub, "");
    tile.classList.add("dim");
    tile.classList.remove("alert");
    if (bridgeTipKey !== "none") {
      bridgeTipKey = "none";
      setTitle(tile, "the bridge is not reporting stats (built without the observability feature, or not connected yet)");
    }
    return;
  }
  const stale = now - ui.bridgeAt > 5;
  const p50 = (b.latency && b.latency.p50_us) || 0;
  const p99 = (b.latency && b.latency.p99_us) || 0;
  setText(SE.sBridge, fmtInt(Math.round(b.datagrams_per_s || 0)));
  setText(SE.sBridgeSub, "/s · p50 " + (p50 / 1000).toFixed(1) + "ms");
  tile.classList.toggle("dim", stale);
  tile.classList.toggle("alert", !stale && (b.parse_errors > 0 || b.lag_drops > 0));

  /* The whole tooltip in one key: it is ~15 numbers, and the push that
     changes them arrives once a second, not ten times. */
  const key = (b.uptime_s || 0) + "|" + stale + "|" + (b.seq_gaps || 0) + "|" + (b.queue_depth || 0);
  if (key === bridgeTipKey) return;
  bridgeTipKey = key;
  setTitle(tile,
    "bridge: " + Math.round(b.datagrams_per_s || 0) + " datagrams/s, " +
    (b.kb_per_s || 0).toFixed(1) + " KB/s, " +
    (b.forwarded || 0) + " forwarded, " +
    (b.parse_errors || 0) + " parse errors, " +
    (b.seq_gaps || 0) + " seq gaps, " +
    (b.lag_drops || 0) + " lag drops (" + (b.lag_disconnects || 0) + " disconnects), " +
    (b.clients || 0) + " ws clients, queue " + (b.queue_depth || 0) +
    " (max " + (b.queue_depth_max || 0) + ")\n" +
    "bridge latency p50 " + (p50 / 1000).toFixed(1) + "ms, p99 " + (p99 / 1000).toFixed(1) +
    "ms over " + ((b.latency && b.latency.samples) || 0) + " batches" +
    (b.latency && b.latency.negative ? " (" + b.latency.negative + " negative — clock skew)" : "") +
    "\ndatagram gap p99 " + (b.gap_p99_ms || 0).toFixed(1) + "ms, max " + (b.gap_max_ms || 0).toFixed(1) + "ms" +
    (stale ? "\n(stale: no update in the last 5s)" : ""));
}

/* =====================================================================
   Feed indicator (OBS) and source visibility

   On stream, a dead capture agent used to look exactly like a still hand:
   the trails simply stop moving. The agent now sends a heartbeat once a
   second while nothing else is going out, which splits that into two
   verdicts after `?stale=` seconds of quiet (0 turns the indicator off):
   *idle* — the agent is there, the hand is not moving, so the badge says so
   and the panels are left alone — and *no feed*, where nothing has said
   hello either and the panels dim. Both recover the instant data returns.

   Visibility is the other half: OBS keeps a hidden browser source's page
   running — script, socket and all — so the only way a source that is not on
   screen costs nothing is to keep ticking (the timeline must stay exact) and
   stop drawing. The events are OBS's own; `document.visibilityState` is the
   fallback in a plain browser, and a page with neither is unchanged.
   ===================================================================== */

const staleView = { kind: "live", on: false, age: 0, badge: null };

/** Re-evaluate the overlay's feed verdict. Runs from the 10Hz block of the
    frame loop (the only thing that can turn it on) and from `ui.onBatch` /
    `ui.onHeartbeat` (which relax it the instant something arrives). While it
    is saying anything at all, the badge counts the quiet in whole seconds —
    one text write a second.

    `staleView.on` stays the *dim* flag alone, so it keeps meaning "no feed":
    an idle agent is not a fault, and its panels keep their brightness. */
function updateStale() {
  if (!OBS) return;
  const now = nowSec();
  staleView.age = Math.max(0, now - ui.lastDataAt);
  const kind = overlayFeed(staleView.age, ui.heartbeatAge(now), OBS.stale);
  if (kind !== staleView.kind) {
    staleView.kind = kind;
    staleView.on = kind === "down";
    document.body.classList.toggle("stale", kind === "down");
    document.body.classList.toggle("idle", kind === "idle");
    engine.dirty = true;
  }
  if (kind !== "live") {
    if (!staleView.badge) staleView.badge = $("feedBadge");
    setText(staleView.badge,
      (kind === "idle" ? "idle \u00b7 " : "no feed \u00b7 ") + fmtQuiet(staleView.age));
  }
}

/** "7s", "4m 05s": how long the feed has been quiet. */
function fmtQuiet(sec) {
  sec = Math.floor(sec);
  return sec < 60 ? sec + "s" : Math.floor(sec / 60) + "m " + pad2(sec % 60) + "s";
}

/* Each channel is tracked separately: OBS reports "this source is hidden"
   and "the scene it is in is not active" independently, and either one means
   nobody is looking. */
const visibility = { source: true, active: true, page: true };

function isVisible() {
  return visibility.source && visibility.active && visibility.page;
}

function setVisible(channel, on) {
  on = !!on;
  if (visibility[channel] === on) return;
  const was = isVisible();
  visibility[channel] = on;
  /* Coming back: one repaint, so the panels are not a frozen picture from
     however long ago the source was hidden. */
  if (!was && isVisible()) engine.dirty = true;
}

(function watchVisibility() {
  const detail = (e) => (e && e.detail) || {};
  if (typeof window.obsstudio === "object" && window.obsstudio !== null) {
    window.addEventListener("obsSourceVisibleChanged", (e) => setVisible("source", detail(e).visible));
    window.addEventListener("obsSourceActiveChanged", (e) => setVisible("active", detail(e).active));
    return;
  }
  if (typeof document.visibilityState === "string") {
    const sync = () => setVisible("page", document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", sync);
    sync();
  }
})();

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
    let unit = null;
    if (spec.unit) {
      const u = document.createElement("small");
      u.textContent = spec.unit;
      v.appendChild(u);
      unit = u;
    }
    item.appendChild(k);
    item.appendChild(v);
    box.appendChild(item);
    hud.items.push({ key: key, el: val, unit: unit, last: "—" });
  }
}

function latencyValue(e, now = nowSec()) {
  if (ui.mode !== "live" || e.arrivalLatencyMs === null) return "—";
  if (now - e.latencyReceivedAt >= NO_DATA_SECS) return "idle";
  return e.arrivalLatencyMs.toFixed(1);
}

/** One readout as text. The HUD's whole vocabulary, and the stats bar's
    shared part: both surfaces format a number the same way because they
    format it here. */
function hudValue(key, e, now = nowSec()) {
  switch (key) {
    case "speed": return e.speedCm.toFixed(e.speedCm < 100 ? 1 : 0);
    case "aim": return fmtInt(Math.round(e.speedDeg));
    case "pointer": return fmtInt(Math.round(e.speedPx));
    case "cpm": return String(Math.round(e.clickRate.total(e.playT)));
    case "eps": return fmtInt(Math.round(e.eventRate.perSecond(e.playT)));
    case "dist": return (e.totalCm / 100).toFixed(2);
    case "aimdist": return fmtInt(Math.round(e.totalDeg));
    case "clicks": return fmtInt(e.totalClicks);
    case "game": return e.game ? e.game.replace(/\.exe$/i, "") : "—";
    case "latency": return latencyValue(e, now);
    case "eventage": {
      const us = ui.mode === "live" ? e.newestUtcUs() : null;
      return us === null ? "—" : ((Date.now() * 1000 - us) / 1000).toFixed(1);
    }
    case "status":
      if (staleView.kind === "live") return "live";
      return (staleView.kind === "idle" ? "idle (" : "no feed (") + fmtQuiet(staleView.age) + ")";
  }
  return "—";
}

function paintHud() {
  const e = engine;
  for (const it of hud.items) {
    const v = hudValue(it.key, e);
    if (v !== it.last) { it.last = v; it.el.textContent = v; }
    if (it.key === "latency" && it.unit) setText(it.unit, v === "idle" || v === "—" ? "" : "ms");
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
  engine.tick(now, uiContext());
  if (PROFILE) { performance.mark("tk1"); measure("tick", "tk0", "tk1", "tick"); }

  /* Keep drawing while anything is still decaying, and draw one final frame
     after the last trail point expires so the panels actually end up
     empty instead of holding a ghost. */
  /* A hidden OBS source (or an inactive scene) keeps ticking and stops
     drawing; `setVisible` marks the engine dirty on the way back. */
  const alive = engine.visualsAlive();
  if (due && !ui.paused && isVisible() && (engine.dirty || alive || wasAlive)) {
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
    /* The feed verdicts are time-driven: nothing arrives to announce that
       nothing is arriving, so they are re-evaluated here or never. */
    if (OBS) { updateStale(); paintHud(); }
    else { ui.refreshConn(); paintStats(now); }
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
  document.body.classList.remove("view-desk", "view-aim", "view-pointer", "view-compare");
  if (v !== "both") document.body.classList.add("view-" + v);
  for (const b of $("viewSwitch").children) setPressed(b, b.dataset.view === v);
  const q = new URLSearchParams(location.search);
  if (v === "both") q.delete("view"); else q.set("view", v);
  const qs = q.toString();
  history.replaceState(null, "", location.pathname + (qs ? "?" + qs : "") + location.hash);
  for (const p of PANELS) p.resize();
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
  if (!Number.isFinite(ms)) { ui.toast("go to time", "unreadable date/time", true); return; }
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
  for (const p of PANELS) p.resize();
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

/* =====================================================================
   Theme (dashboard only)

   Same contract as the control panel: `localStorage.tmTheme` is "light" or
   "dark" when the user picked one, absent to follow the system; the choice
   lands on <html data-theme>, which is all the stylesheet looks at. (The
   head of index.html applies it before first paint; this keeps it current.)
   The two pages are different origins, so each remembers its own.

   The canvases cannot follow CSS, so every change re-reads the tokens into
   PAL and repaints once. The overlay is never themed and never gets here.
   ===================================================================== */

/* The storage key, and the OBS test at the top of this file, are repeated in
   index.html's pre-paint script, which has to run before this file exists;
   change them together. */
const THEME_KEY = "tmTheme";
let lightQuery = null;
try { lightQuery = matchMedia("(prefers-color-scheme: light)"); } catch (e) { lightQuery = null; }

function currentTheme() {
  const t = document.documentElement.dataset.theme;
  if (t === "light" || t === "dark") return t;
  return lightQuery && lightQuery.matches ? "light" : "dark";
}

/** Tokens → PAL → ramp. Returns false where there is no stylesheet to read. */
function readTokens() {
  if (OBS || typeof getComputedStyle !== "function") return false;
  let cs;
  try { cs = getComputedStyle(document.documentElement); } catch (e) { return false; }
  paletteFromTokens((name) => cs.getPropertyValue(name));
  buildRamp();
  engine.dirty = true;
  return true;
}

function applyTheme(repaint) {
  if (OBS) return;
  let t = themePicked;
  if (t === undefined) {
    try { t = localStorage.getItem(THEME_KEY); } catch (e) { t = null; /* storage blocked */ }
  }
  const root = document.documentElement;
  if (t === "light" || t === "dark") root.dataset.theme = t;
  else delete root.dataset.theme;
  /* What the system prefers, for the stylesheet's "no pick" case (see the
     token rules in index.html). */
  root.dataset.sys = lightQuery && lightQuery.matches ? "light" : "dark";
  setText($("themeBtn"), currentTheme() === "dark" ? "Light theme" : "Dark theme");
  /* A paused picture is not redrawn by the frame loop; repaint it here so
     the canvases never sit in the old theme under new chrome. */
  if (readTokens() && repaint) { engine.draw(); engine.dirty = false; }
}

/* This page load's pick, so the button still works where storage is blocked. */
let themePicked;

function setTheme(t) {
  themePicked = t;
  try {
    if (t === null) localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, t);
  } catch (e) { /* storage blocked: the choice lasts until reload */ }
  applyTheme(true);
}

if (!OBS) {
  /* Shift-click hands the choice back to the system (the panel has a
     "use system" link for this; the top bar has no room for one). */
  $("themeBtn").addEventListener("click", (e) =>
    setTheme(e.shiftKey ? null : currentTheme() === "dark" ? "light" : "dark"));
  if (lightQuery && lightQuery.addEventListener) lightQuery.addEventListener("change", () => applyTheme(true));
  applyTheme(false);
}

/* A trim the render loop cannot starve. rAF is suspended in a background
   tab but the socket is not, so this is the timer that keeps a hidden tab
   from growing without bound. */
setInterval(() => engine.trim(uiContext()), 250);

/** A range input's limits and starting value, from the one definition at the
    top of this file rather than from the markup's copy of it. */
function configureSlider(el, spec, value) {
  el.min = String(spec.min);
  el.max = String(spec.max);
  el.step = String(spec.step);
  el.value = String(value);
}
configureSlider($("buf"), BUFFER_MS, BUFFER_MS.dflt);
configureSlider($("decay"), TRAIL_SECS, OBS ? OBS.trail : TRAIL_SECS.dflt);

if (OBS) {
  /* Body classes drive the CSS (chrome hidden, layout, labels); the panels
     were constructed before this ran, so re-measure them once the hidden
     one has collapsed to 0×0. */
  document.body.classList.add("obs", "layout-" + OBS.layout);
  if (OBS.labels) document.body.classList.add("labels");
  document.documentElement.style.setProperty("--s", String(OBS.scale));
  buildHud();
  for (const p of PANELS) p.resize();
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
    if (Number.isFinite(v) && v >= BUFFER_MS.min / 1000 && v <= BUFFER_MS.max / 1000) savedBuffer = v;
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
  if (!Number.isFinite(atMs)) ui.toast("?at=", "unreadable date/time: " + AT, true);
}
/* ?session=<id> opens that recording in replay: what the control panel's
   Recordings list links to. The id must pass the server's own rule
   (`isSafeId`); the server 404s anything else anyway. */
const SESSION = QUERY.get("session");
const wantSession = !OBS && isSafeId(SESSION);
if (Number.isFinite(atMs)) {
  ui.pendingUtcUs = atMs * 1000;
  $("gotoTime").value = toDatetimeLocal(atMs);
  ui.setMode("replay");
} else if (wantSession) {
  ui.pendingSessionId = SESSION;
  ui.setMode("replay");
} else {
  ui.setConn("warn", "connecting");
  ui.connect();
}
requestAnimationFrame(frame);

/* Handy from the devtools console when tuning: telemouse.engine.playT, etc. */
window.telemouse = {
  engine: engine, ui: ui, deskPanel: deskPanel, aimPanel: aimPanel, ptrPanel: ptrPanel,
  prof: prof, profiling: PROFILE, obs: OBS,
  /* The Windows pointer model on its own, for the tests and for tuning. */
  PointerModel: PointerModel, pointerSettingsFrom: pointerSettingsFrom, speedMultiplier: speedMultiplier,
  WIN_POINTER_DEFAULTS: WIN_POINTER_DEFAULTS,
  /* For the Node tests, which have no animation frames of their own. */
  frame: frame, staleView: staleView, feedIsStale: feedIsStale, overlayFeed: overlayFeed,
  palette: PAL, paletteFromTokens: paletteFromTokens, buildRamp: buildRamp, rampCss: RAMP_CSS,
};
/* What the Node tests reach for beyond the devtools handle: page-level
   functions that would otherwise only be reachable as leaked VM globals. */
window.telemouse._test = {
  paintStats: paintStats, hudValue: hudValue, setView: setView, isSafeId: isSafeId,
  streamLines: streamLines, limits: { BUFFER_MS: BUFFER_MS, TRAIL_SECS: TRAIL_SECS, OBS_SCALE: OBS_SCALE },
};
