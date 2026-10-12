// Loads crates/ctl/src/panel.js into a fresh V8 context with just enough of a
// browser stubbed out for the script to run to its last line, and hands back
// what the page exposes on `window.telemouseCtl`.
//
// Modelled on crates/viz/js-tests/harness.mjs. The stubs are deliberately
// dumb: elements remember text, values, classes and data attributes; timers
// are recorded, never fired by themselves; `fetch` is whatever the test
// passes (by default a request that never answers, so nothing paints).

import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const PANEL_JS_PATH = path.join(HERE, "..", "src", "panel.js");
export const INDEX_HTML_PATH = path.join(HERE, "..", "src", "index.html");
export const PANEL_JS = fs.readFileSync(PANEL_JS_PATH, "utf8");
export const INDEX_HTML = fs.readFileSync(INDEX_HTML_PATH, "utf8");

/** A JSON fixture from js-tests/fixtures. */
export function fixture(name) {
  return JSON.parse(fs.readFileSync(path.join(HERE, "fixtures", name), "utf8"));
}

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
  };
}

function element(id) {
  const el = {
    id,
    textContent: "",
    value: "",
    innerHTML: "",
    title: "",
    className: "",
    href: "",
    hidden: false,
    disabled: false,
    checked: false,
    open: false,
    dataset: {},
    children: [],
    style: {},
    classList: classList(),
    listeners: {},
    addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); },
    removeEventListener() {},
    appendChild(c) { this.children.push(c); return c; },
    remove() {},
    focus() {},
    scrollIntoView() {},
    closest() { return null; },
    querySelector() { return null; },
    querySelectorAll() { return []; },
  };
  return el;
}

/** A promise with its resolve/reject handles, for a fetch the test answers. */
export function deferred() {
  let resolve, reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

/** What `fetch` resolves to: enough of a Response for `api()`. */
export function response(json, status = 200) {
  return { ok: status >= 200 && status < 300, status, statusText: "", json: async () => json };
}

/**
 * Load the page script. `config` is what the server injects as
 * window.TELEMOUSE_CTL; `fetch(url, init)` answers the page's requests.
 * Returns { ctl, sandbox, elements, timers, requests }: `ctl` is
 * window.telemouseCtl, `timers` every setTimeout the page scheduled (not
 * fired), `requests` every URL it fetched.
 */
export function loadPanel({ config = {}, fetch = null, hidden = false } = {}) {
  const elements = new Map();
  const timers = [];
  const requests = [];
  const document = {
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, element(id));
      return elements.get(id);
    },
    createElement(tag) { return element("<" + tag + ">"); },
    addEventListener() {},
    querySelectorAll() { return []; },
    hidden,
    activeElement: null,
    documentElement: element("html"),
  };
  const sandbox = {
    console,
    URLSearchParams,
    Promise,
    Error,
    document,
    TELEMOUSE_CTL: config,
    location: { search: "", protocol: "http:", host: "127.0.0.1:7880", pathname: "/" },
    localStorage: { getItem() { return null; }, setItem() {}, removeItem() {} },
    matchMedia() { return { matches: false, addEventListener() {} }; },
    fetch(url, init) {
      requests.push({ url, method: (init && init.method) || "GET" });
      return fetch ? fetch(url, init) : new Promise(() => {});
    },
    setTimeout(fn, ms) { timers.push({ fn, ms }); return timers.length; },
    clearTimeout() {},
    setInterval() { return 0; },
    clearInterval() {},
    open() {},
  };
  sandbox.window = sandbox;
  sandbox.globalThis = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(PANEL_JS, sandbox, { filename: "panel.js" });
  if (!sandbox.telemouseCtl) throw new Error("panel.js did not publish window.telemouseCtl");
  return { ctl: sandbox.telemouseCtl, sandbox, elements, timers, requests };
}

/** Let every queued promise callback run. */
export async function settle(rounds = 20) {
  for (let i = 0; i < rounds; i++) await new Promise((r) => setImmediate(r));
}
