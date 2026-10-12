// Loads crates/viz/src/app.js into a fresh V8 context with just enough of a
// browser stubbed out for the script to run to its last line, and hands back
// what the page exposes on `window.telemouse` (engine, ui, panels).
//
// The stubs are deliberately dumb: elements remember classes, attributes, text
// and values (form controls start at the `value="…"` index.html gives them),
// canvases hand out a 2D context whose every method is a no-op, no animation
// frame runs and no socket connects. Timeouts queue until a test calls
// `flushTimers()`; intervals never fire. `fetch` answers from a test-supplied
// function (by default: an empty recording list). The engine is pure
// arithmetic over typed arrays, which is most of what the tests exercise; the
// DOM only has to not throw.

import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const APP_JS_PATH = path.join(HERE, "..", "src", "app.js");
export const INDEX_HTML_PATH = path.join(HERE, "..", "src", "index.html");
const APP_JS = fs.readFileSync(APP_JS_PATH, "utf8");
const INDEX_HTML = fs.readFileSync(INDEX_HTML_PATH, "utf8");

/**
 * The starting value of every form control in index.html, by id: an
 * `<input>`'s `value="…"`, a `<select>`'s selected (or first) option. What a
 * browser would show before the script touches anything.
 */
export function markupValues(html = INDEX_HTML) {
  const values = new Map();
  const attr = (tag, name) => {
    const m = tag.match(new RegExp("\\s" + name + "=\"([^\"]*)\""));
    return m ? m[1] : null;
  };
  for (const [tag] of html.matchAll(/<input\b[^>]*>/g)) {
    const id = attr(tag, "id");
    if (id !== null) values.set(id, attr(tag, "value") ?? "");
  }
  for (const m of html.matchAll(/<select\b([^>]*)>([\s\S]*?)<\/select>/g)) {
    const id = attr(m[1], "id");
    if (id === null) continue;
    const options = [...m[2].matchAll(/<option\b([^>]*)>/g)].map((o) => o[1]);
    const chosen = options.find((o) => /\sselected\b/.test(o)) ?? options[0];
    values.set(id, chosen === undefined ? "" : attr(chosen, "value") ?? "");
  }
  return values;
}
const MARKUP_VALUES = markupValues();

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
  const attrs = new Map();
  const el = {
    id,
    textContent: "",
    value: MARKUP_VALUES.get(id) ?? "",
    innerHTML: "",
    title: "",
    className: "",
    hidden: false,
    width: 0,
    height: 0,
    dataset: {},
    children: [],
    parentNode: null,
    firstChild: { nodeValue: "" },
    style: { setProperty() {}, },
    classList: classList(),
    addEventListener() {},
    removeEventListener() {},
    setAttribute(name, value) { attrs.set(name, String(value)); },
    getAttribute(name) { return attrs.has(name) ? attrs.get(name) : null; },
    removeAttribute(name) { attrs.delete(name); },
    appendChild(c) { this.children.push(c); c.parentNode = this; return c; },
    remove() {
      const p = this.parentNode;
      if (!p) return;
      const i = p.children.indexOf(this);
      if (i >= 0) p.children.splice(i, 1);
      this.parentNode = null;
    },
    focus() {},
    querySelector() { return null; },
    querySelectorAll() { return []; },
    getBoundingClientRect() { return { width: 800, height: 600, left: 0, top: 0 }; },
    getContext() { return context2d(); },
  };
  el.parentElement = { id: id + "-parent", getBoundingClientRect: el.getBoundingClientRect };
  return el;
}

/**
 * A queue standing in for `setTimeout`: nothing runs until `flush()`, which
 * runs every queued callback in due order — including ones queued while it
 * runs (a toast's fade schedules its removal) — up to a sanity limit.
 */
function timerQueue() {
  let nextId = 1, now = 0;
  const pending = new Map();
  return {
    setTimeout(fn, delay = 0, ...args) {
      const id = nextId++;
      pending.set(id, { fn, args, at: now + Math.max(0, +delay || 0), id });
      return id;
    },
    clearTimeout(id) { pending.delete(id); },
    get size() { return pending.size; },
    flush(limit = 10_000) {
      let ran = 0;
      while (pending.size) {
        if (ran++ >= limit) throw new Error("flushTimers: timers keep scheduling timers");
        const t = [...pending.values()].sort((a, b) => a.at - b.at || a.id - b.id)[0];
        pending.delete(t.id);
        now = Math.max(now, t.at);
        t.fn(...t.args);
      }
      return ran;
    },
  };
}

/** A `fetch` Response whose body is `obj` as JSON. */
export function jsonResponse(obj, { status = 200 } = {}) {
  return textResponse(JSON.stringify(obj), { status });
}

/**
 * A `fetch` Response carrying `text`, readable both ways the page reads one:
 * `json()` / `text()`, and `body.getReader()` handing out `chunkSize`-byte
 * chunks (so a line can straddle two reads, as it does on a real socket).
 * Content-Length is set, as the server sets it.
 */
export function textResponse(text, { status = 200, chunkSize = 64 } = {}) {
  const bytes = new TextEncoder().encode(text);
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: { get: (name) => (name.toLowerCase() === "content-length" ? String(bytes.length) : null) },
    json: async () => JSON.parse(text),
    text: async () => text,
    body: {
      getReader() {
        let at = 0;
        return {
          read: async () => {
            if (at >= bytes.length) return { done: true, value: undefined };
            const value = bytes.subarray(at, at + chunkSize);
            at += chunkSize;
            return { done: false, value };
          },
          cancel: async () => { at = bytes.length; },
        };
      },
    },
  };
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
 * `globals` is merged into the page's `window` before the script runs (e.g.
 * `{ obsstudio: {} }` to look like an OBS browser source).
 * `fetch(url)` answers the page's requests (return a Response from
 * `jsonResponse` / `textResponse`, or throw); by default every request gets
 * an empty JSON list.
 * Returns { telemouse, sandbox, elements, fire, fetched, flushTimers } —
 * `telemouse` is the page's devtools handle; `fire(type, detail)` dispatches
 * a window event to whatever the page registered for it; `fetched` lists the
 * URLs requested; `flushTimers()` runs every queued timeout. No animation
 * frame ever runs by itself: tests that need the frame loop call
 * `telemouse.frame(ms)`.
 */
export function loadApp({ search = "", config = {}, clock = null, globals = {}, fetch = null } = {}) {
  const timers = timerQueue();
  const fetched = [];
  const answer = fetch || (() => jsonResponse([]));
  const elements = new Map();
  const listeners = new Map();
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
    fetch: async (url) => { fetched.push(String(url)); return answer(String(url)); },
    ResizeObserver,
    WebSocket,
    TELEMOUSE_CONFIG: config,
    devicePixelRatio: 1,
    location: { search, protocol: "http:", host: "127.0.0.1:7879", pathname: "/", hash: "" },
    history: { replaceState() {} },
    localStorage: { getItem() { return null; }, setItem() {}, removeItem() {} },
    matchMedia() { return { addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {} }; },
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(fn);
    },
    removeEventListener() {},
    requestAnimationFrame() { return 0; },
    cancelAnimationFrame() {},
    setInterval() { return 0; },
    clearInterval() {},
    setTimeout: (fn, delay, ...args) => timers.setTimeout(fn, delay, ...args),
    clearTimeout: (id) => timers.clearTimeout(id),
  };
  Object.assign(sandbox, globals);
  sandbox.window = sandbox;
  if (clock) {
    sandbox.Date = class extends Date { static now() { return clock.utcMs; } };
    sandbox.performance = { now: () => clock.monotonicMs };
  }
  sandbox.globalThis = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(APP_JS, sandbox, { filename: "app.js" });
  if (!sandbox.telemouse) throw new Error("app.js did not publish window.telemouse");
  const fire = (type, detail) => (listeners.get(type) || []).forEach((fn) => fn({ type, detail }));
  return {
    telemouse: sandbox.telemouse, sandbox, elements, fire, fetched,
    flushTimers: () => timers.flush(),
    pendingTimers: () => timers.size,
  };
}

/** Let every pending promise continuation run (fetch answers, awaited reads). */
export async function settle(rounds = 20) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
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

/** The capture agent's once-a-second "still here, hand still". */
export function heartbeatEnvelope(overrides = {}) {
  return {
    type: "heartbeat",
    session_id: "s-test",
    ts_utc_us: ANCHOR_UTC_US,
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
