// Loads crates/viz/src/app.js into a fresh V8 context with just enough of a
// browser stubbed out for the script to run to its last line, and hands back
// what the page exposes on `window.telemouse` (engine, ui, panels).
//
// The stubs are deliberately dumb: elements remember classes, text and values,
// canvases hand out a 2D context whose every method is a no-op, timers never
// fire, no animation frame runs and no socket connects. The engine is pure
// arithmetic over typed arrays, which is exactly what the tests exercise; the
// DOM only has to not throw.

import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const APP_JS_PATH = path.join(HERE, "..", "src", "app.js");
const APP_JS = fs.readFileSync(APP_JS_PATH, "utf8");

function classList() {
  const set = new Set();
  return {
    add(...c) { c.forEach((x) => set.add(x)); },
    remove(...c) { c.forEach((x) => set.delete(x)); },
    toggle(c, force) {
      const on = force === undefined ? !set.has(c) : !!force;
      if (on) set.add(c); else set.delete(c);
      return on;
    },
    contains(c) { return set.has(c); },
    get list() { return [...set]; },
  };
}

function context2d() {
  const target = { measureText: () => ({ width: 0 }) };
  return new Proxy(target, {
    get(t, key) { return key in t ? t[key] : () => {}; },
    set() { return true; },
  });
}

function element(id) {
  const el = {
    id,
    textContent: "",
    value: "",
    innerHTML: "",
    title: "",
    className: "",
    hidden: false,
    width: 0,
    height: 0,
    dataset: {},
    children: [],
    firstChild: { nodeValue: "" },
    style: { setProperty() {}, },
    classList: classList(),
    addEventListener() {},
    removeEventListener() {},
    appendChild(c) { this.children.push(c); return c; },
    remove() {},
    focus() {},
    querySelector() { return null; },
    querySelectorAll() { return []; },
    getBoundingClientRect() { return { width: 800, height: 600, left: 0, top: 0 }; },
    getContext() { return context2d(); },
  };
  el.parentElement = { id: id + "-parent", getBoundingClientRect: el.getBoundingClientRect };
  return el;
}

class ResizeObserver {
  constructor() {}
  observe() {}
  unobserve() {}
  disconnect() {}
}

class WebSocket {
  constructor(url) { this.url = url; this.sent = []; this.closed = false; }
  send(m) { this.sent.push(m); }
  close() { this.closed = true; }
}

/**
 * Load the app. `search` is the URL query (e.g. "?obs=1&layout=aim");
 * `config` is what the server injects as window.TELEMOUSE_CONFIG.
 * Returns { telemouse, sandbox } — `telemouse` is the page's devtools handle.
 */
export function loadApp({ search = "", config = {} } = {}) {
  const elements = new Map();
  const document = {
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, element(id));
      return elements.get(id);
    },
    createElement(tag) { return element("<" + tag + ">"); },
    createDocumentFragment() { return element("#fragment"); },
    addEventListener() {},
    hasFocus() { return true; },
    hidden: false,
    body: element("body"),
    documentElement: element("html"),
  };
  const sandbox = {
    console,
    performance,
    URLSearchParams,
    TextDecoder,
    document,
    ResizeObserver,
    WebSocket,
    TELEMOUSE_CONFIG: config,
    devicePixelRatio: 1,
    location: { search, protocol: "http:", host: "127.0.0.1:7879", pathname: "/", hash: "" },
    history: { replaceState() {} },
    localStorage: { getItem() { return null; }, setItem() {}, removeItem() {} },
    matchMedia() { return { addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {} }; },
    addEventListener() {},
    removeEventListener() {},
    requestAnimationFrame() { return 0; },
    cancelAnimationFrame() {},
    setInterval() { return 0; },
    clearInterval() {},
    setTimeout() { return 0; },
    clearTimeout() {},
  };
  sandbox.window = sandbox;
  sandbox.globalThis = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(APP_JS, sandbox, { filename: "app.js" });
  if (!sandbox.telemouse) throw new Error("app.js did not publish window.telemouse");
  return { telemouse: sandbox.telemouse, sandbox, elements };
}

/* ---------- wire-format builders (mirror telemouse-core) ---------- */

export const QPC_FREQ = 10_000_000;
export const ANCHOR_QPC = 5_000_000_000;
export const ANCHOR_UTC_US = 1_756_000_000_000_000;

export function sessionEnvelope(overrides = {}) {
  return {
    type: "session",
    session_id: "s-test",
    started_utc_us: ANCHOR_UTC_US,
    qpc_freq: QPC_FREQ,
    anchor: { qpc: ANCHOR_QPC, utc_us: ANCHOR_UTC_US, qpc_freq: QPC_FREQ },
    anchor_uncertainty_us: 3,
    mouse_cpi: 1600.0,
    devices: ["unknown", "\\\\?\\HID#VID_1532&PID_0099"],
    games: { "cs2.exe": { sens: 2.0, yaw_coeff: 0.022, pitch_coeff: 0.022 } },
    monitors: [{ width: 2560, height: 1440, refresh_hz: 240, primary: true }],
    capture_version: "0.1.0",
    coalesce_ms: 8,
    ...overrides,
  };
}

/** QPC ticks for `seconds` after the anchor. */
export function qpcAt(seconds) {
  return ANCHOR_QPC + Math.round(seconds * QPC_FREQ);
}

export function batchEnvelope(seq, events, overrides = {}) {
  return {
    type: "batch",
    session_id: "s-test",
    seq_no: seq,
    ts_anchor_us: ANCHOR_UTC_US + (events.length ? Math.round(((events[0].ts_qpc - ANCHOR_QPC) * 1e6) / QPC_FREQ) : 0),
    game: "cs2.exe",
    pointer_locked: true,
    screen_w: 2560,
    screen_h: 1440,
    drops_since_last: 0,
    abs_frames_since_last: 0,
    events,
    ...overrides,
  };
}

/** A motion event; zero-valued rare fields are omitted, as the agent does. */
export function ev(seconds, dx, dy, extra = {}) {
  return { ts_qpc: qpcAt(seconds), dx, dy, ...extra };
}

/**
 * A deterministic 1 kHz stream: `seconds` of 25 ms batches whose deltas come
 * from a small LCG, with a click every 700th event. Same input, same
 * numbers, every run.
 */
export function syntheticStream(seconds, { seq0 = 0, start = 0, hz = 1000 } = {}) {
  let state = 0x2545f491;
  const rand = () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return (state >>> 8) / 0x1000000;
  };
  const batches = [];
  const perBatch = Math.round(hz * 0.025);
  const total = Math.round(seconds * hz);
  let seq = seq0;
  for (let i = 0; i < total; i += perBatch) {
    const events = [];
    for (let j = i; j < Math.min(total, i + perBatch); j++) {
      const t = start + j / hz;
      const dx = Math.round((rand() - 0.5) * 40);
      const dy = Math.round((rand() - 0.5) * 12);
      const extra = {};
      if (j % 700 === 350) extra.buttons = 0x0001; // LEFT_DOWN
      if (j % 700 === 380) extra.buttons = 0x0002; // LEFT_UP
      events.push(ev(t, dx, dy, extra));
    }
    batches.push(batchEnvelope(seq++, events));
  }
  return batches;
}
