"use strict";
const CFG = window.TELEMOUSE_CTL || {};
const PLACES = CFG.places || {};
const $ = (id) => document.getElementById(id);
const HEADERS = { "X-Telemouse-Ctl": "1", "Content-Type": "application/json" };
/* Inside the native window the page is opened with ?hosted=1 and offers a
   way out to a real browser; in a browser that link would be pointless. */
const HOSTED = new URLSearchParams(location.search).has("hosted");
const VIZ_URL = "http://" + (CFG.viz_http || "127.0.0.1:7879") + "/";
/* Values the panel owns, sent in the page config (server.rs PageLimits)
   rather than repeated here; the fallbacks only matter for a page opened
   without a server. */
const SAVE_FLAGS = new Set(CFG.save_flags || ["--record", "--no-record"]);
const FLAG_LABELS = CFG.flag_labels || {};
const LOW_DISK_BYTES = CFG.low_disk_bytes || 2 * 1073741824;
const DEMO_SESSION = CFG.demo_session || "demo-session";
const LOG_LINES = CFG.log_lines || 120;
const TOOL_TITLES = { doctor: "Check my setup", report: "Report", trend: "Trend" };
const WORDS = { jsonl: "not written to the recording file", kafka: "not delivered to Kafka", udp: "not delivered to the live view" };
const WIZ_DEMO_SECS = 30;
const POLL_MS = 2000;
/* A server error naming a setting → the input to mark. One entry per line:
   a test in settings.rs reads this table against the Patch allow-list. */
const FIELD_INPUTS = {
  "mouse_cpi": "sCpi",
  "marker_hotkey": "sMarkerHotkey",
  "recording.dir": "sRecDir",
  "ctl.hotkey": "sHotkey",
  "viz.obs.layout": "sObsLayout",
  "viz.obs.background": "sObsBg",
  "viz.obs.hud_position": "sObsPos",
  "viz.obs.scale": "sObsScale",
};

/* ---------- state: everything the page remembers, declared before any
   function can run, so nothing reads a binding before it exists ---------- */
let state = null;                // last GET /api/state, with each log merged to a full tail
let sessions = [];
let pollTimer = null;            // the next scheduled poll
let pollRun = null;              // the poll in flight (a promise), so there is only ever one
let pollAgain = false;           // something asked for a fresh poll while one was in flight
const logs = new Map();          // component id → { seq, lines }: what ?log_since= appends to
let elapsedTimer = null;
let awake = true;                // false while nobody is looking: no timers at all
const busy = new Set();          // component ids (or "p<pid>") with a request in flight
const armed = new Map();         // force-stop buttons awaiting a second click, key → timeout
const chosen = { flags: new Set(), save: null, session: null }; // capture flags / save switch / report session
let procsKey = "", recKey = null, flagsKey = "";
let sinceStartMs = null;         // local clock base for the elapsed readout
let activeTool = null;           // task whose output the Tools panel shows
let cfgView = null;              // last GET /api/config
let cfgMtime = null;             // state.config.mtime_unix_s it was read at
let formDirty = false;
const removedGames = new Set();
/* The first-run guide. `demo.timer` is the countdown, cleared on close. */
const wiz = { open: false, auto: true, step: 0, shown: -1, watching: false, seenKey: "", demo: null, busy: false };
/* The report card. */
const report = { id: null, summary: null, error: "", loading: false, watch: null, key: "" };

/* ---------- helpers ---------- */
function esc(s) { return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])); }
/* Write only when the text changed: an unchanged write still dirties layout. */
function setText(el, v) { if (el && el.textContent !== v) el.textContent = v; }
function setHidden(el, hidden) { if (el && el.hidden !== !!hidden) el.hidden = !!hidden; }
function clock(unixS) { return unixS ? new Date(unixS * 1000).toLocaleTimeString() : ""; }
function hms(secs) {
  const s = Math.max(0, Math.floor(secs));
  return Math.floor(s / 3600) + ":" + String(Math.floor((s % 3600) / 60)).padStart(2, "0") + ":" + String(s % 60).padStart(2, "0");
}
function elapsedOf(sinceUnixS) { return sinceUnixS && state ? hms(state.now_unix_s - sinceUnixS) : ""; }
function toast(msg, cls) {
  const el = document.createElement("div");
  el.className = "toast " + (cls || "");
  el.textContent = msg;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), 5000);
}
function setConn(ok, text) {
  $("connPill").className = "pill " + (ok ? "on" : "err");
  setText($("connText"), text);
}
function comp(id) { return state && state.components.find((c) => c.id === id); }
function storage(get, key, value) {
  try {
    if (get) return localStorage.getItem(key);
    if (value === null) localStorage.removeItem(key); else localStorage.setItem(key, value);
  } catch (_) { /* private mode, blocked storage: the page still works */ }
  return null;
}

async function api(method, path, body) {
  const res = await fetch(path, { method, headers: HEADERS, body: body === undefined ? undefined : JSON.stringify(body) });
  let json = null;
  try { json = await res.json(); } catch (_) { /* not JSON */ }
  if (!res.ok) {
    const err = new Error((json && json.error) || (res.status + " " + res.statusText));
    err.status = res.status; err.body = json || {};
    throw err;
  }
  return json;
}

/* ---------- theme ---------- */
function currentTheme() {
  const t = document.documentElement.dataset.theme;
  if (t) return t;
  return matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}
function applyTheme() {
  const t = storage(true, "tmTheme");
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
  setText($("themeBtn"), currentTheme() === "dark" ? "Light theme" : "Dark theme");
  setHidden($("themeSystem"), !t);
}
$("themeBtn").addEventListener("click", () => { storage(false, "tmTheme", currentTheme() === "dark" ? "light" : "dark"); applyTheme(); });
$("themeSystem").addEventListener("click", () => { storage(false, "tmTheme", null); applyTheme(); });
matchMedia("(prefers-color-scheme: light)").addEventListener("change", applyTheme);
applyTheme();

/* ---------- static bits of the page ---------- */
setText($("verText"), "v" + (PLACES.version || "?"));
setText($("buildText"), "telemouse-ctl " + (PLACES.version || "?") + " · built with " + (CFG.features || "?"));
$("relLink").href = PLACES.releases || "https://github.com/uwdivad/telemouse/releases";
$("vizLink").href = VIZ_URL;
if (HOSTED && PLACES.panel_url) { $("browserLink").href = PLACES.panel_url; setHidden($("browserLink"), false); }
function paintMarkerHint() {
  const key = cfgView && cfgView.settings ? cfgView.settings.marker_hotkey : CFG.marker_hotkey;
  setText($("markerHint"), key ? "or press " + key.toUpperCase() + " in the game" : "");
}

/* ---------- polling ---------- */
/* `?log_since=` with each component's cursor: the server then sends only
   the lines that arrived since, and mergeLogs appends them. */
function logCursor() {
  return [...logs].map(([id, l]) => id + ":" + l.seq).join(",");
}
/* Turn each component's `log` (new lines only, once a cursor was sent) back
   into the whole tail the rest of the page reads, at most LOG_LINES long. */
function mergeLogs(components) {
  for (const c of components) {
    const prev = logs.get(c.id);
    const fresh = c.log || [];
    if (prev && c.log_seq < prev.seq) {
      // A cursor from before a panel restart matched nothing: forget it,
      // and the next poll fetches the tail.
      logs.delete(c.id);
      c.log = fresh.slice();
      continue;
    }
    // First sight (the server sent the tail), or a gap (the client fell
    // further behind than the server keeps): start over from what came.
    // Otherwise what came follows on from what is held.
    const restart = !prev || c.log_seq - fresh.length > prev.seq;
    let lines = restart ? fresh.slice() : prev.lines.concat(fresh);
    if (lines.length > LOG_LINES) lines = lines.slice(lines.length - LOG_LINES);
    logs.set(c.id, { seq: c.log_seq, lines });
    c.log = lines;
  }
}
async function pollOnce() {
  try {
    const cursor = logCursor();
    const [s, sess] = await Promise.all([
      api("GET", "/api/state" + (cursor ? "?log_since=" + encodeURIComponent(cursor) : "")),
      api("GET", "/api/sessions"),
    ]);
    mergeLogs(s.components || []);
    state = s; sessions = sess;
    setConn(true, "Connected");
    const mt = s.config ? (s.config.mtime_unix_s || 0) : 0;
    if (mt !== cfgMtime) { cfgMtime = mt; loadConfig(false); }
    $("connPill").title = "telemouse-ctl pid " + s.self_pid;
    paint();
  } catch (e) {
    setConn(false, "Panel not responding");
  }
}
/* Single-flight: one poll at a time, however many callers ask. A call while
   one is in flight does not start a second (that is how the 2 s chain used
   to double); it asks the running one to go round once more, so whoever
   awaits it still sees state from after their own action. The chain is
   rescheduled in exactly one place, when the last round ends. */
function poll() {
  if (pollRun) { pollAgain = true; return pollRun; }
  clearTimeout(pollTimer); pollTimer = null;
  pollRun = (async () => {
    do { pollAgain = false; await pollOnce(); } while (pollAgain && awake);
    pollRun = null;
    pollTimer = awake ? setTimeout(poll, POLL_MS) : null;
  })();
  return pollRun;
}
/* A page nobody is looking at does nothing at all — no fetches, no clock —
   and catches up the moment it is back. That is a hidden browser tab, a
   minimised window, and the native window hidden to the tray: there
   `SetIsVisible(false)` does not make the document `hidden`, so the window
   calls these two hooks through ExecuteScript instead (gui/webview.rs), and
   suspends the page on top of it. */
function sleepPage() {
  awake = false;
  clearTimeout(pollTimer); pollTimer = null;
  clearInterval(elapsedTimer); elapsedTimer = null;
}
function wakePage() {
  awake = true;
  clearTimeout(pollTimer); pollTimer = null;
  if (elapsedTimer === null) elapsedTimer = setInterval(tickElapsed, 1000);
  poll();
}
function tickElapsed() { if (sinceStartMs !== null) setText($("liveElapsed"), hms((Date.now() - sinceStartMs) / 1000)); }
document.addEventListener("visibilitychange", () => { if (document.hidden) sleepPage(); else wakePage(); });
window.telemouseSleep = sleepPage;
window.telemouseWake = wakePage;

/* ---------- actions ---------- */
async function act(id, fn) {
  if (busy.has(id)) return;
  busy.add(id); paint();
  try { await fn(); } catch (e) { toast(e.message, "bad"); }
  busy.delete(id);
  await poll();
}
function startCapture() {
  const body = { flags: [...chosen.flags] };
  // The save switch is sent as a boolean and turned into --record /
  // --no-record by the server against the config it holds now. Untouched
  // = null = the config default.
  if (chosen.save === true || chosen.save === false) body.save = chosen.save;
  return act("capture", async () => {
    await api("POST", "/api/components/capture/start", body);
    toast("Recording started", "ok");
  });
}
function stopComponent(id, force, label) {
  return act(id, async () => {
    const r = await api("POST", "/api/components/" + id + "/stop", { force: !!force });
    toast(label + (r.outcome === "graceful" ? " stopped" : " stopped (forced)"), r.outcome === "graceful" ? "ok" : "");
  });
}
function runTool(id) {
  const body = { flags: [] };
  if (id === "report") {
    body.session = chosen.session || sessions[0] || null;
    if (chosen.flags.has("--timing")) body.flags.push("--timing");
    // The card is drawn from the summary once this run has finished.
    if (body.session) watchReport(body.session.replace(/\.jsonl$/, ""));
  }
  activeTool = id;
  return act(id, async () => {
    try { await api("POST", "/api/components/" + id + "/start", body); }
    catch (e) { if (id === "report") { report.id = null; report.loading = false; } throw e; }
    setHidden($("toolOut"), false);
  });
}
function openPlace(target) {
  return act("open-" + target, async () => { await api("POST", "/api/open", { target }); });
}
function addMarker(label) {
  return act("marker", async () => {
    const r = await api("POST", "/api/components/capture/marker", { label });
    toast("Marker added: " + r.label, "ok");
    $("markerInput").value = "";
  });
}
function killProcess(p) {
  const key = "p" + p.pid;
  if (!armed.has(key)) {
    // Two clicks, no modal: the first arms the button for 4 seconds.
    armed.set(key, setTimeout(() => { armed.delete(key); paint(); }, 4000));
    paint();
    return;
  }
  clearTimeout(armed.get(key)); armed.delete(key);
  return act(key, async () => {
    await api("POST", "/api/processes/" + p.pid + "/kill");
    toast("Stopped " + p.name + " (pid " + p.pid + ")", "ok");
  });
}

/* ---------- painting: targeted writes, no re-rendering on the poll ---------- */
function paint() {
  if (!state) return;
  paintWizard();
  paintSession();
  paintReport();
  paintViz();
  paintRecordings();
  paintTools();
  paintAdvanced();
  renderProcs();
}

/* ---------- first-run guide ----------
   Shown by itself when this start created telemouse.toml (and it was not
   dismissed), and from Settings → "Run setup again". Three skippable steps;
   everything it saves goes through POST /api/config like the Settings form. */
function wizOpen(step) {
  wiz.open = true; wiz.step = step || 0; wiz.shown = -1;
  paint();
  $("firstRun").scrollIntoView({ behavior: "smooth", block: "start" });
}
async function wizClose() {
  wiz.open = false; wiz.auto = false;
  storage(false, "tmFirstRunDismissed", "1");
  // A demo still counting down ends now: its recording is stopped and kept,
  // and no report is opened behind a guide that was just dismissed.
  if (wiz.demo && wiz.demo.cancel) wiz.demo.cancel();
  if (wiz.watching) await wizStopWatching();
  paint();
}
function wizGo(step) { wiz.step = step; paint(); }

function paintWizard() {
  const seeded = !!(state.config && state.config.seeded);
  if (wiz.auto && !wiz.open && seeded && storage(true, "tmFirstRunDismissed") !== "1") wiz.open = true;
  setHidden($("firstRun"), !wiz.open);
  if (!wiz.open) return;
  setText($("firstRunPath"), (state.config && state.config.path) || PLACES.config || "telemouse.toml");
  setHidden($("wizIntro"), !seeded);
  if (wiz.shown !== wiz.step) {
    wiz.shown = wiz.step;
    for (let i = 0; i < 3; i++) setHidden($("wizStep" + i), i !== wiz.step);
    for (const el of $("wizSteps").children) {
      const n = Number(el.dataset.s);
      el.className = n === wiz.step ? "now" : (n < wiz.step ? "done" : "");
    }
  }
  const known = cfgView && cfgView.settings;
  if (wiz.step === 0 && known && !$("wizCpi").value && document.activeElement !== $("wizCpi")) $("wizCpi").value = known.mouse_cpi;
  const c = comp("capture");
  if (wiz.step === 1) {
    // Detection needs the agent's stats line, which the minimal build lacks.
    const can = (CFG.features || "").includes("observability") && c && c.bin_found;
    setHidden($("wizDetect"), !can);
    const w = $("wizWatch");
    setText(w, wiz.watching ? "Stop watching" : "Watch for my game");
    w.disabled = wiz.busy || (!wiz.watching && c && c.running);
    setText($("wizWatchNote"), wiz.watching ? "Watching. Switch to your game now, then come back."
      : (c && c.running ? "A recording is running; the programs it has seen are listed below." : ""));
    const seen = (c && c.foreground_seen) || [];
    const key = seen.map((p) => p.exe).join("|");
    if (key !== wiz.seenKey) {
      wiz.seenKey = key;
      $("wizSeen").innerHTML = seen.length
        ? '<span class="small">Seen in front:</span>' + seen.map((p) => '<button class="btn small" type="button" data-wiz-exe="' + esc(p.exe) + '">Use ' + esc(p.exe) + "</button>").join("")
        : "";
    }
  }
  if (wiz.step === 2) {
    const d = wiz.demo;
    if (d && d.phase === "analysing" && !report.loading) d.phase = "done";
    const btn = $("wizDemo");
    const demoUnderway = !!d && d.phase !== "done" && d.phase !== "failed";
    const noAgent = !c || !c.bin_found;
    const someoneElseRecording = !d && !noAgent && c.running;
    btn.disabled = demoUnderway || noAgent || someoneElseRecording;
    setHidden($("wizCount"), !(d && d.phase === "recording"));
    let note = "";
    if (!c || !c.bin_found) note = "telemouse.exe was not found next to telemouse-ctl.exe, so nothing can be recorded yet.";
    else if (!d && c.running) note = "A recording is already running. Stop it first, or just press Finish.";
    else if (d) note = { starting: "Starting…", recording: "Recording. Move and click!", stopping: "Saving the recording…",
      analysing: "Working out the report…", done: "Done. Your report is below.", failed: d.error || "That did not work." }[d.phase] || "";
    setText($("wizDemoNote"), note);
    setHidden($("wizSample"), !sessions.includes(DEMO_SESSION + ".jsonl"));
  }
}
$("firstRunLater").addEventListener("click", wizClose);
$("wizFinish").addEventListener("click", wizClose);
for (const v of [400, 800, 1600, 3200]) {
  const b = document.createElement("button");
  b.type = "button"; b.className = "btn small"; b.textContent = String(v);
  b.addEventListener("click", () => { $("wizCpi").value = v; });
  $("wizCpiPicks").appendChild(b);
}
for (const v of [0.022, 0.0066, 0.07]) {
  const b = document.createElement("button");
  b.type = "button"; b.className = "btn small"; b.textContent = String(v);
  b.addEventListener("click", () => { $("wizYaw").value = v; });
  $("wizYawPicks").appendChild(b);
}
$("wizCpiNext").addEventListener("click", async () => {
  const v = Number($("wizCpi").value);
  if (!(v > 0)) { setText($("wizCpiErr"), "Type your mouse's CPI as a number, for example 800."); return; }
  setText($("wizCpiErr"), "");
  try {
    if (!cfgView || !cfgView.settings || cfgView.settings.mouse_cpi !== v) await postConfig({ mouse_cpi: v });
    wizGo(1);
  } catch (e) { setText($("wizCpiErr"), e.message); }
});
async function wizStopWatching() {
  wiz.watching = false;
  const c = comp("capture");
  if (c && c.running && !c.saving) { try { await api("POST", "/api/components/capture/stop", { force: false }); } catch (_) { /* already gone */ } }
}
$("wizWatch").addEventListener("click", async () => {
  if (wiz.busy) return;
  wiz.busy = true; paint();
  try {
    if (wiz.watching) await wizStopWatching();
    else { await api("POST", "/api/components/capture/start", { flags: [], save: false }); wiz.watching = true; }
  } catch (e) { toast(e.message, "bad"); }
  wiz.busy = false;
  await poll();
});
$("wizGameNext").addEventListener("click", async () => {
  const exe = $("wizExe").value.trim();
  const sens = Number($("wizSens").value), yaw = Number($("wizYaw").value);
  let why = "";
  if (!exe) why = "Type the game's program name, or press Skip.";
  else if (!(sens > 0)) why = "Type your in-game sensitivity as a number above 0.";
  else if (!(yaw > 0)) why = "Degrees per step must be a number above 0 (0.022 if unsure).";
  setText($("wizGameErr"), why);
  if (why) return;
  try {
    await postConfig({ games: { [exe]: { sens, yaw_coeff: yaw, pitch_coeff: yaw } } });
    if (wiz.watching) await wizStopWatching();
    toast("Game saved", "ok");
    wizGo(2);
    poll();
  } catch (e) { setText($("wizGameErr"), e.message); }
});
async function wizButton(b) {
  if (b.dataset.wizExe) { $("wizExe").value = b.dataset.wizExe.toLowerCase(); $("wizSens").focus(); }
  else if ("wizSkip" in b.dataset) {
    if (wiz.watching) await wizStopWatching();
    if (wiz.step >= 2) wizClose(); else wizGo(wiz.step + 1);
  }
}

/* The demo: start a saving capture, count down, stop it, report on the
   recording that appeared. Each await may fail; the note says what did.
   The countdown lives on `wiz.demo` so closing the guide can end it. */
async function wizDemo() {
  const before = new Set(sessions);
  const d = wiz.demo = { phase: "starting", error: "", timer: null, cancel: null, cancelled: false };
  const fail = (e) => { d.phase = "failed"; d.error = e.message || String(e); paint(); };
  paint();
  try { await api("POST", "/api/components/capture/start", { flags: [], save: true }); } catch (e) { return fail(e); }
  d.phase = "recording";
  await poll();
  const ends = Date.now() + WIZ_DEMO_SECS * 1000;
  await new Promise((done) => {
    const finish = () => { clearInterval(d.timer); d.timer = null; d.cancel = null; done(); };
    d.cancel = () => { d.cancelled = true; finish(); };
    d.timer = setInterval(() => {
      const left = Math.max(0, Math.ceil((ends - Date.now()) / 1000));
      setText($("wizCount"), String(left));
      const c = comp("capture");
      if (left <= 0 || !c || !c.running) finish();
    }, 250);
  });
  d.phase = "stopping"; paint();
  try { await api("POST", "/api/components/capture/stop", { force: false }); } catch (_) { /* it may have ended by itself */ }
  await poll();
  if (d.cancelled) { d.phase = "done"; paint(); return; }
  const made = sessions.find((s) => !before.has(s));
  if (!made) return fail(new Error("The recording did not appear in the recordings folder. Check the capture agent log under Advanced."));
  const rep = comp("report");
  if (!rep || !rep.bin_found) { d.phase = "done"; d.error = ""; toast("Recorded " + made, "ok"); paint(); return; }
  d.phase = "analysing"; paint();
  chosen.session = made;
  await runTool("report");
}
$("wizDemo").addEventListener("click", () => { wizDemo(); });
$("wizSampleReport").addEventListener("click", () => { chosen.session = DEMO_SESSION + ".jsonl"; runTool("report"); });
$("wizSampleDash").addEventListener("click", async () => {
  // A window must be opened from the click itself or it is blocked; the
  // live view is started alongside and the dashboard retries until it is up.
  const v = comp("viz");
  const url = VIZ_URL + "?session=" + encodeURIComponent(DEMO_SESSION);
  if (v && !v.running) {
    try { await api("POST", "/api/components/viz/start", { flags: [] }); await new Promise((r) => setTimeout(r, 1200)); }
    catch (e) { toast(e.message, "bad"); return; }
  }
  window.open(url, "_blank", "noopener");
  poll();
});

/* ---------- settings ---------- */
paintMarkerHint();

async function loadConfig(force) {
  try {
    cfgView = await api("GET", "/api/config");
  } catch (e) {
    // Say so where the form would be, rather than leaving whatever was
    // there (or nothing) as if it were current. A form that loaded before
    // stays, with the failure noted under it. The next poll tries again.
    cfgMtime = null;
    if (!cfgView || !cfgView.settings) {
      setHidden($("settingsForm"), true);
      setHidden($("sUnusable"), false);
      $("sUnusable").innerHTML = "<b>The settings could not be read from the panel</b>: " + esc(e.message) + ". Trying again.";
    } else {
      setText($("sNotice"), "Could not re-read telemouse.toml (" + e.message + "); the form shows it as it was.");
    }
    return;
  }
  if (force || !formDirty) fillSettings();
  paintMarkerHint();
  if (state) paint();
}
/* One save path for the form and the guide. A stale token means the file
   changed under us: re-read it and say so, never write over it. */
async function postConfig(patch, fromForm) {
  if (!cfgView) await loadConfig(true);
  if (!cfgView) throw new Error("The settings could not be read from the panel.");
  try {
    const r = await api("POST", "/api/config", { token: cfgView.token, patch });
    cfgView.token = r.token; cfgView.settings = r.settings; cfgView.exists = true; cfgView.error = null;
    if (fromForm || !formDirty) fillSettings();
    paintMarkerHint();
    return r;
  } catch (e) {
    if (e.status === 409) await loadConfig(true);
    throw e;
  }
}
function numOf(el) { const v = el.value.trim(); return v === "" ? NaN : Number(v); }
function gameRow(exe, g) {
  const row = document.createElement("div");
  row.className = "game";
  row.dataset.exe = exe || "";
  row.innerHTML =
    '<input type="text" data-g="exe" maxlength="64" spellcheck="false" placeholder="mygame.exe" aria-label="Program name"' + (exe ? " readonly" : "") + ">" +
    '<input type="number" data-g="sens" min="0" step="any" aria-label="Sensitivity" placeholder="sensitivity">' +
    '<input type="number" data-g="yaw" min="0" step="any" aria-label="Degrees per step" placeholder="0.022">' +
    '<input type="number" data-g="pitch" min="0" step="any" aria-label="Vertical degrees per step" placeholder="same">' +
    '<button class="btn small" type="button" data-g-remove>Remove</button>';
  const q = (k) => row.querySelector('[data-g="' + k + '"]');
  q("exe").value = exe || "";
  if (g) { q("sens").value = g.sens; q("yaw").value = g.yaw_coeff; q("pitch").value = g.pitch_coeff; }
  else q("yaw").value = 0.022;
  // Vertical follows horizontal until someone sets it apart.
  let lastYaw = q("yaw").value;
  q("yaw").addEventListener("input", () => {
    if (q("pitch").value === "" || q("pitch").value === lastYaw) q("pitch").value = g ? q("yaw").value : "";
    lastYaw = q("yaw").value;
  });
  return row;
}
function fillSettings() {
  const v = cfgView;
  if (!v) return;
  const s = v.settings;
  setHidden($("sUnusable"), !!s);
  setHidden($("settingsForm"), !s);
  if (!s) {
    $("sUnusable").innerHTML = "<b>telemouse.toml cannot be used as it is</b>, so it cannot be edited here: " + esc(v.error || "unknown problem") +
      '<div class="foot"><button class="btn small" data-open="config">Edit telemouse.toml</button></div>';
    return;
  }
  $("sCpi").value = s.mouse_cpi;
  $("sRecDir").value = s.recording.dir;
  $("sRecEnabled").checked = s.recording.enabled;
  $("sMarkerHotkey").value = s.marker_hotkey;
  $("sHotkey").value = s.ctl.hotkey;
  const ch = v.choices || {};
  const opts = (list, cur) => (list || []).map((o) => '<option value="' + esc(o) + '"' + (o === cur ? " selected" : "") + ">" + esc(o) + "</option>").join("");
  $("sObsLayout").innerHTML = opts(ch.obs_layouts, s.obs.layout);
  $("sObsPos").innerHTML = opts(ch.obs_hud_positions, s.obs.hud_position);
  $("sObsScale").value = s.obs.scale;
  $("sObsBg").value = s.obs.background;
  $("sObsHud").innerHTML = (ch.obs_hud_items || []).map((o) =>
    '<label><input type="checkbox" data-hud="' + esc(o) + '"' + (s.obs.hud.includes(o) ? " checked" : "") + ">" + esc(o) + "</label>").join("");
  const host = $("sGames");
  host.textContent = "";
  removedGames.clear();
  const names = Object.keys(s.games);
  for (const exe of names) host.appendChild(gameRow(exe, s.games[exe]));
  if (!names.length) host.innerHTML = '<div class="empty" id="sNoGames">No games yet. Without one, movement is still recorded in full; only the degree numbers use a generic value.</div>';
  setText($("sFile"), v.exists ? v.path : "No settings file yet; saving creates " + v.path);
  showSettingsErrors(null);
  setDirty(false);
}
function setDirty(d) {
  formDirty = d;
  $("sSave").disabled = !d;
  $("sRevert").disabled = !d;
}
function showSettingsErrors(err) {
  for (const el of document.querySelectorAll("#settingsForm [data-err]")) el.textContent = "";
  for (const el of document.querySelectorAll("#settingsForm .invalid")) el.classList.remove("invalid");
  if (!err) return;
  const field = err.field || "";
  const slots = [...document.querySelectorAll("#settingsForm [data-err]")];
  // The most specific slot whose name starts the field: games.x.exe.sens → games.
  const slot = slots.filter((el) => el.dataset.err && field.startsWith(el.dataset.err)).sort((a, b) => b.dataset.err.length - a.dataset.err.length)[0]
    || slots.find((el) => el.dataset.err === "");
  slot.textContent = err.message;
  const input = FIELD_INPUTS[field];
  if (input) $(input).classList.add("invalid");
  const d = slot.closest("details");
  if (d) d.open = true;
  slot.scrollIntoView({ behavior: "smooth", block: "center" });
}
/* The difference between the form and the file, as a patch; throws
   { field, message } for what can be caught before asking the server. */
function collectPatch() {
  const s = cfgView.settings, p = {};
  const bad = (field, message) => { const e = new Error(message); e.field = field; return e; };
  const cpi = numOf($("sCpi"));
  if (!(cpi > 0)) throw bad("mouse_cpi", "Type the CPI as a number above 0, for example 800.");
  if (cpi !== s.mouse_cpi) p.mouse_cpi = cpi;
  const mh = $("sMarkerHotkey").value.trim();
  if (mh !== s.marker_hotkey) p.marker_hotkey = mh;
  const hk = $("sHotkey").value.trim();
  if (hk !== s.ctl.hotkey) p.ctl = { hotkey: hk };
  const rec = {};
  const dir = $("sRecDir").value.trim();
  if (!dir) throw bad("recording.dir", "The recordings folder cannot be empty.");
  if (dir !== s.recording.dir) rec.dir = dir;
  if ($("sRecEnabled").checked !== s.recording.enabled) rec.enabled = $("sRecEnabled").checked;
  if (Object.keys(rec).length) p.recording = rec;

  const games = {};
  for (const exe of removedGames) games[exe] = null;
  for (const row of $("sGames").querySelectorAll(".game")) {
    const q = (k) => row.querySelector('[data-g="' + k + '"]');
    const key = row.dataset.exe;
    const exe = key || q("exe").value.trim();
    const sens = numOf(q("sens")), yaw = numOf(q("yaw")), pitchText = q("pitch").value.trim(), pitch = numOf(q("pitch"));
    if (!key && !exe && isNaN(sens)) continue; // an untouched new row
    if (!exe) throw bad("games", "A game needs its program name, for example mygame.exe.");
    if (!(sens > 0) || !(yaw > 0) || (pitchText && !(pitch > 0))) throw bad("games", exe + ": sensitivity and degrees per step must be numbers above 0.");
    if (!key) { games[exe] = { sens, yaw_coeff: yaw }; if (pitchText) games[exe].pitch_coeff = pitch; continue; }
    const old = s.games[key], g = {};
    if (sens !== old.sens) g.sens = sens;
    if (yaw !== old.yaw_coeff) g.yaw_coeff = yaw;
    if (pitchText && pitch !== old.pitch_coeff) g.pitch_coeff = pitch;
    if (Object.keys(g).length) games[key] = g;
  }
  if (Object.keys(games).length) p.games = games;

  const obs = {};
  if ($("sObsLayout").value !== s.obs.layout) obs.layout = $("sObsLayout").value;
  if ($("sObsPos").value !== s.obs.hud_position) obs.hud_position = $("sObsPos").value;
  const scale = numOf($("sObsScale"));
  if (isNaN(scale)) throw bad("viz.obs.scale", "Size must be a number between 0.5 and 4.");
  if (scale !== s.obs.scale) obs.scale = scale;
  const bg = $("sObsBg").value.trim() || "transparent";
  if (bg !== s.obs.background) obs.background = bg;
  const hud = [...$("sObsHud").querySelectorAll("input:checked")].map((i) => i.dataset.hud);
  if (hud.length !== s.obs.hud.length || !hud.every((o) => s.obs.hud.includes(o))) obs.hud = hud;
  if (Object.keys(obs).length) p.obs = obs;
  return p;
}
$("settingsForm").addEventListener("input", () => setDirty(true));
$("settingsForm").addEventListener("change", () => setDirty(true));
$("sRevert").addEventListener("click", () => { $("sNotice").textContent = ""; fillSettings(); });
$("sWizard").addEventListener("click", () => wizOpen(0));
$("sAddGame").addEventListener("click", () => {
  const none = $("sNoGames");
  if (none) none.remove();
  const row = gameRow("", null);
  $("sGames").appendChild(row);
  row.querySelector("input").focus();
});
$("sGames").addEventListener("click", (e) => {
  const b = e.target.closest("[data-g-remove]");
  if (!b) return;
  const row = b.closest(".game");
  if (row.dataset.exe) removedGames.add(row.dataset.exe);
  row.remove();
  setDirty(true);
});
$("settingsForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  if (busy.has("settings") || !cfgView || !cfgView.settings) return;
  let patch;
  try { patch = collectPatch(); } catch (err) { showSettingsErrors(err); return; }
  showSettingsErrors(null);
  if (!Object.keys(patch).length) { setDirty(false); toast("Nothing to save", ""); return; }
  busy.add("settings"); $("sSave").disabled = true;
  try {
    const r = await postConfig(patch, true);
    const notes = [];
    if ((r.next_start || []).includes("capture")) notes.push("The recording that is running keeps the old settings; the new ones apply from the next recording.");
    if ((r.next_start || []).includes("viz")) notes.push("The live view picks this up the next time it is started.");
    if ((r.restart_required || []).includes("ctl.hotkey")) notes.push("The new-recording key changes after telemouse is restarted (tray icon → Exit, then start it again).");
    setText($("sNotice"), notes.join(" "));
    toast(r.created ? "Settings saved (telemouse.toml created)" : "Settings saved", "ok");
    cfgMtime = null; // the next poll re-reads the file's state
  } catch (err) {
    if (err.status === 400) showSettingsErrors({ field: (err.body && err.body.field) || "", message: err.message });
    else { toast(err.message, "bad"); if (err.status !== 409) showSettingsErrors({ field: "", message: err.message }); }
    setDirty(true);
  }
  busy.delete("settings");
});

/* ---------- report card ---------- */
function fnum(v, d) {
  return (v === null || v === undefined || !isFinite(v)) ? "—" : Number(v).toLocaleString(undefined, { minimumFractionDigits: d || 0, maximumFractionDigits: d || 0 });
}
function durWords(secs) {
  if (!isFinite(secs)) return "—";
  const s = Math.round(secs);
  if (s < 60) return s + " s";
  if (s < 3600) return Math.floor(s / 60) + " min " + (s % 60) + " s";
  return Math.floor(s / 3600) + " h " + Math.floor((s % 3600) / 60) + " min";
}
function watchReport(id) {
  const c = comp("report");
  Object.assign(report, { id, summary: null, error: "", loading: true, watch: c ? c.exits : 0, key: "" });
}
async function fetchSummary() {
  const id = report.id;
  try {
    const s = await api("POST", "/api/reports/" + encodeURIComponent(id));
    if (report.id !== id) return;
    report.summary = s;
  } catch (e) {
    if (report.id !== id) return;
    report.error = e.message;
  }
  report.loading = false;
  paint();
  $("reportCard").scrollIntoView({ behavior: "smooth", block: "start" });
}
function tile(k, v) { return '<div class="stat"><div class="k">' + esc(k) + '</div><div class="v">' + v + "</div></div>"; }
function metric(k, v, unit, help) {
  return '<div class="metric"><div class="k">' + esc(k) + '</div><div class="v">' + v + (unit ? " <small>" + esc(unit) + "</small>" : "") +
    '</div><div class="help">' + esc(help) + "</div></div>";
}
function renderReport(s) {
  const se = s.session || {}, q = s.quality || {}, f = s.flicks || {}, m = s.micro || {}, cl = s.clicks || {}, k = s.kinematics || {}, l = s.lifts || {};
  if (!se.event_count) {
    return '<div class="verdict warn"><div><b>No mouse movement was recorded.</b> The recording is ' + esc(durWords(se.duration_s)) +
      ' long but holds no mouse events. Move the mouse while recording; if you did, run <b>Check my setup</b> under Tools to see whether your mouse is listed.</div></div>';
  }
  const problems = [];
  if (q.ring_drops) problems.push(fnum(q.ring_drops) + " mouse events were lost inside the capture agent (the PC was too busy to keep up).");
  if (q.lost_batches) problems.push(fnum(q.lost_batches) + (q.lost_batches === 1 ? " batch of events is" : " batches of events are") + " missing from the recording.");
  for (const [sink, n] of q.sink_losses || []) if (n) problems.push(fnum(n) + (n === 1 ? " batch was " : " batches were ") + (WORDS[sink] || "lost by " + sink) + ".");
  if (q.bad_lines) problems.push(fnum(q.bad_lines) + " lines of the file could not be read.");
  if (q.monotonicity_violations) problems.push(fnum(q.monotonicity_violations) + " events arrived out of time order.");
  if (q.unfinished) problems.push("The recording was not closed cleanly (the agent was still running or was killed), so the end may be missing.");
  const lossy = !!(q.ring_drops || q.lost_batches || q.bad_lines || (q.sink_losses || []).some(([sink, n]) => sink === "jsonl" && n));
  let cls = "", head = "Nothing was lost. Every mouse event made it into this recording.";
  if (lossy) { cls = "bad"; head = "Some data is missing from this recording. The numbers below are still computed, but treat them with care."; }
  else if (problems.length || !q.clean) { cls = "warn"; head = "The recording is complete, with a few things worth a look."; }
  const notes = s.warnings || [];
  let html = '<div class="verdict ' + cls + '"><div><b>' + esc(head) + "</b>" +
    (problems.length ? "<ul>" + problems.map((p) => "<li>" + esc(p) + "</li>").join("") + "</ul>" : "") +
    (notes.length ? '<details class="sub" style="margin-top:6px"><summary>The analyzer' + "'" + 's own notes (' + notes.length + ")</summary><ul>" + notes.map((p) => "<li>" + esc(p) + "</li>").join("") + "</ul></details>" : "") +
    "</div></div>";

  html += '<div class="tiles">' +
    tile("Length", esc(durWords(se.duration_s))) +
    tile("Mouse events", fnum(se.event_count)) +
    tile("Mouse report rate", q.poll_hz ? fnum(q.poll_hz) + " Hz" : "—") +
    tile("Markers", fnum(se.marker_count)) +
    tile("Hand travel", fnum(k.total_distance_m, 1) + " m") +
    tile("Clicks", fnum(cl.total)) + "</div>";

  // The marker labels, so a session marked "sens A" / "sens B" says which
  // stretch was which without opening the full report.
  const mk = s.markers || [];
  if (mk.length) {
    const total = s.markers_total || mk.length;
    html += '<details class="sub" style="margin:-4px 0 12px"><summary>What you marked (' + fnum(total) + ")</summary><ul>" +
      mk.map((m) => "<li>" + esc(durWords(m.t_s)) + " in — " + esc(m.label) + "</li>").join("") +
      (total > mk.length ? "<li>… and " + fnum(total - mk.length) + " more</li>" : "") + "</ul></details>";
  }

  if (se.aim_profile_missing) {
    html += '<div class="verdict warn"><div><b>' + (se.game ? "No sensitivity is set for " + esc(se.game) + "." : "No game was in front during this recording.") +
      "</b> Degree values below use a generic setting, so compare them only with each other. " +
      (se.game ? '<button class="btn small" type="button" data-add-game="' + esc(se.game) + '">Add it in Settings</button>' : "") + "</div></div>";
  }
  html += '<h2 class="sec">Your aim, in plain numbers</h2><div class="metrics">' +
    metric("Flicks", fnum(f.count), f.count ? fnum(f.per_minute, 1) + " per minute" : "", "Fast, deliberate snaps of the crosshair toward a target.") +
    metric("Typical flick size", f.count ? fnum(f.amplitude_deg_median, 1) : "—", "degrees", "How far a typical flick turns the view. Half of your flicks are smaller, half larger.") +
    metric("Overshoot", f.count && f.overshoot_ratio_median !== null ? fnum(f.overshoot_ratio_median * 100, 1) : "—", "% of the flick", "How far past the target a typical flick goes before coming back. Lower is cleaner.") +
    metric("Settle time", f.count ? fnum(f.settle_ms_median) : "—", "ms", "Time from the end of a flick until the crosshair is steady. Lower is faster.") +
    metric("Flick to click", f.clicked_fraction ? fnum(f.time_to_click_ms_median) : "—", "ms", "Time from starting a flick to pressing the button (" + fnum((f.clicked_fraction || 0) * 100) + "% of flicks ended in a click).") +
    metric("Straightness", k.path_efficiency_weighted !== null && k.path_efficiency_weighted !== undefined ? fnum(k.path_efficiency_weighted * 100) : "—", "%", "How direct your movements are: 100% is a perfectly straight line to where you stopped.") +
    metric("Still when clicking", cl.total ? fnum(cl.still_click_fraction * 100) : "—", "% of clicks", "Share of clicks made with the mouse at rest, rather than while still moving.") +
    metric("Hand shake", fnum(m.tremor_rms_cm_s, 2), "cm/s", "Tiny movement while you try to hold still. Lower is steadier; it rises with tension and tiredness.") +
    metric("Sensitivity", se.cm_per_360 ? fnum(se.cm_per_360, 1) : "—", "cm per full turn", "Mousepad distance for a 360° turn in " + (se.game || "the game") + " at " + fnum(se.mouse_cpi) + " CPI.") +
    metric("Mouse lifts", fnum(l.count), l.count ? fnum(l.per_minute, 1) + " per minute" : "", "Times you picked the mouse up to re-centre it.") +
    "</div>";
  return html;
}
function paintReport() {
  const c = comp("report");
  if (report.loading && report.watch !== null && c && !c.running && c.exits > report.watch) {
    report.watch = null;
    if (c.last_exit && c.last_exit.code === 0) fetchSummary();
    else { report.loading = false; report.error = "The report ended with an error" + (c.last_exit && c.last_exit.hint ? ": " + c.last_exit.hint : "") + ". The text below says more."; }
  }
  setHidden($("reportCard"), !report.id);
  if (!report.id) return;
  const pill = $("rcState");
  pill.className = "pill " + (report.loading ? "warn" : report.error ? "err" : "on");
  setText($("rcStateText"), report.loading ? "working…" : report.error ? "no card" : "ready");
  const se = report.summary && report.summary.session;
  let sub = report.id;
  if (se) sub += " · " + new Date(se.started_utc_us / 1000).toLocaleString() + (se.game ? " · " + se.game : "");
  setText($("rcSub"), sub);
  $("rcDash").href = VIZ_URL + "?session=" + encodeURIComponent(report.id);
  const key = report.loading ? "loading" : report.error ? "e:" + report.error : "s:" + report.id + (se ? se.event_count : "");
  if (key !== report.key) {
    report.key = key;
    $("rcBody").innerHTML = report.loading ? '<div class="empty">Reading the recording and working out the numbers…</div>'
      : report.error ? '<div class="empty">' + esc(report.error) + "</div>"
      : renderReport(report.summary);
    $("rcDetails").open = !!report.error;
  }
  const raw = c ? c.log.join("\n") : "";
  const pre = $("rcRaw");
  if (pre.textContent !== raw) pre.textContent = raw;
}
$("rcClose").addEventListener("click", () => { report.id = null; paint(); });
$("rcBody").addEventListener("click", (e) => {
  const b = e.target.closest("[data-add-game]");
  if (!b) return;
  $("settingsCard").open = true;
  $("sAddGame").click();
  const rows = $("sGames").querySelectorAll(".game");
  const row = rows[rows.length - 1];
  if (row && b.dataset.addGame) { row.querySelector('[data-g="exe"]').value = b.dataset.addGame.toLowerCase(); row.querySelector('[data-g="sens"]').focus(); setDirty(true); }
  $("settingsCard").scrollIntoView({ behavior: "smooth", block: "start" });
});

function paintSession() {
  const c = comp("capture");
  if (!c) return;
  const run = c.running;
  const missing = !c.bin_found;

  let label = "Not recording", cls = "";
  if (missing) { label = "Capture agent not found"; cls = "err"; }
  else if (run) { label = c.saving ? "Recording" : "Running, not saving"; cls = c.saving ? "on" : "warn"; }
  $("sessState").className = "pill " + cls;
  setText($("sessStateText"), label);

  const btn = $("sessBtn");
  setText(btn, run ? "Stop recording" : "Start recording");
  btn.className = "btn big " + (run ? "danger" : "primary");
  btn.disabled = busy.has("capture") || missing;
  btn.title = missing
    ? "telemouse.exe was not found next to telemouse-ctl.exe. Re-extract the zip, or set [ctl] bin_dir in telemouse.toml."
    : (run ? "Stops the capture agent; the recording is closed cleanly." : "Starts the capture agent.");

  const wantSave = chosen.save === null ? state.recording.enabled : chosen.save;
  const sw = $("saveSwitch");
  if (sw.checked !== wantSave) sw.checked = wantSave;
  sw.disabled = run;
  setText($("saveNote"), wantSave ? "→ " + state.recording.dir : "nothing is written to disk");

  setHidden($("live"), !run);
  setHidden($("markerForm"), !run || !c.markers);
  if (run) {
    sinceStartMs = c.since_unix_s ? Date.now() - (state.now_unix_s - c.since_unix_s) * 1000 : null;
    setText($("liveElapsed"), elapsedOf(c.since_unix_s) || "—");
    const st = c.stats || {};
    setText($("liveEps"), st.events_per_s !== undefined ? Math.round(st.events_per_s).toLocaleString() : "—");
    setText($("liveGame"), st.game || "no game in front");
    const r = c.recording;
    let file = "not saving";
    if (c.saving) file = r ? r.session + ".jsonl" + (r.size_bytes !== undefined ? " · " + (r.size_bytes / 1048576).toFixed(1) + " MB" : "") : "saving";
    setText($("liveFile"), file);
    const bad = [];
    if (st.drops) bad.push(st.drops.toLocaleString() + " events lost inside the agent");
    if (st.jsonl_dropped) bad.push(st.jsonl_dropped.toLocaleString() + " events not written to the recording");
    if (st.kafka_dropped) bad.push(st.kafka_dropped.toLocaleString() + " events not sent to Kafka");
    if (st.udp_unreachable) bad.push("no live viewer is listening");
    if (r && r.free_bytes !== undefined && r.free_bytes < LOW_DISK_BYTES) bad.push("only " + (r.free_bytes / 1073741824).toFixed(1) + " GB free on the recordings disk");
    setText($("liveWarn"), bad.join(" · "));
    setHidden($("liveWarn"), !bad.length);
  } else {
    sinceStartMs = null;
  }

  const ex = c.last_exit;
  let note = "";
  if (!run && ex && ex.code !== 0 && !ex.ctrl_break) note = "The capture agent ended with an error at " + clock(ex.at_unix_s) + (ex.hint ? ": " + ex.hint : "") + ".";
  else if (!run && c.unexpected_exits) note = "The capture agent stopped on its own " + c.unexpected_exits + (c.unexpected_exits === 1 ? " time" : " times") + " since the panel started.";
  setText($("sessNote"), note);
}
$("sessBtn").addEventListener("click", () => {
  const c = comp("capture");
  if (!c) return;
  if (c.running) stopComponent("capture", false, "Recording"); else startCapture();
});
$("saveSwitch").addEventListener("change", (e) => { chosen.save = e.target.checked; paint(); });
$("markerForm").addEventListener("submit", (e) => {
  e.preventDefault();
  const label = $("markerInput").value.trim();
  if (label) addMarker(label);
});

function paintViz() {
  const c = comp("viz");
  if (!c) return;
  const run = c.running;
  const btn = $("vizBtn");
  setText(btn, run ? "Stop live view" : "Start live view");
  btn.className = "btn small" + (run ? " danger" : "");
  btn.disabled = busy.has("viz") || !c.bin_found;
  btn.title = c.bin_found ? "" : "telemouse-viz.exe was not found next to telemouse-ctl.exe.";
  setHidden($("vizLink"), !run);
  let note = "";
  if (!c.bin_found) note = "telemouse-viz.exe not found";
  else if (run) note = "serving on " + VIZ_URL + " · OBS overlay: " + VIZ_URL + "obs";
  else if (c.last_exit && c.last_exit.code !== 0 && !c.last_exit.ctrl_break) note = "ended with an error" + (c.last_exit.hint ? ": " + c.last_exit.hint : "");
  else note = "off — start it to watch the mouse live in a browser or OBS";
  setText($("vizNote"), note);
}
$("vizBtn").addEventListener("click", () => {
  const c = comp("viz");
  if (!c) return;
  if (c.running) stopComponent("viz", false, "Live view");
  else act("viz", async () => { await api("POST", "/api/components/viz/start", { flags: [] }); toast("Live view started", "ok"); });
});

function paintRecordings() {
  const key = sessions.join("|");
  if (key !== recKey) {
    recKey = key;
    const host = $("recList");
    if (!sessions.length) {
      host.innerHTML = '<li><div class="empty">No recordings yet. Start a recording above; it lands here when you stop it.</div></li>';
    } else {
      const shown = sessions.slice(0, 40);
      host.innerHTML = shown.map((s) => {
        const id = s.replace(/\.jsonl$/, "");
        return '<li><span class="name">' + esc(id) + '</span><span class="acts">' +
          '<button class="btn small" data-report="' + esc(s) + '">Report</button>' +
          '<a class="btn small" data-dash="' + esc(id) + '" target="_blank" rel="noopener">Dashboard</a></span></li>';
      }).join("");
      for (const a of host.querySelectorAll("a[data-dash]")) a.href = VIZ_URL + "?session=" + encodeURIComponent(a.dataset.dash);
    }
    setText($("recNote"), sessions.length > 40 ? sessions.length + " recordings, newest 40 shown" : (sessions.length ? sessions.length + (sessions.length === 1 ? " recording" : " recordings") : ""));
    // The report picker follows the list.
    const sel = $("reportSel");
    sel.innerHTML = sessions.length
      ? sessions.map((s) => '<option value="' + esc(s) + '">' + esc(s.replace(/\.jsonl$/, "")) + "</option>").join("")
      : '<option value="">no recordings</option>';
  }
  const sel = $("reportSel");
  const want = chosen.session && sessions.includes(chosen.session) ? chosen.session : (sessions[0] || "");
  if (sel.value !== want) sel.value = want;
}
$("reportSel").addEventListener("change", (e) => { chosen.session = e.target.value; });

function paintTools() {
  const tasks = state.components.filter((c) => c.kind === "task");
  const running = tasks.find((c) => c.running);
  for (const btn of document.querySelectorAll("button[data-run]")) {
    const c = comp(btn.dataset.run);
    if (!c) continue;
    const isRun = c.running;
    setText(btn, isRun ? "Running…" : "Run");
    btn.disabled = isRun || busy.has(c.id) || !c.bin_found || (running && !isRun) || (c.takes_session && !sessions.length);
    btn.title = !c.bin_found ? c.bin + " was not found next to telemouse-ctl.exe" : (c.takes_session && !sessions.length ? "Record something first" : "");
  }
  // Which output to show: the running task, else the one last started here,
  // else the most recent one that ever ran.
  let show = running || (activeTool && comp(activeTool)) || null;
  if (!show) {
    let best = null;
    for (const c of tasks) if (c.last_exit && (!best || c.last_exit.at_unix_s > best.last_exit.at_unix_s)) best = c;
    show = best;
  }
  setHidden($("toolOut"), !show);
  if (!show) return;
  setText($("toolTitle"), TOOL_TITLES[show.id] || show.label);
  let status = "";
  if (show.running) status = "running · " + elapsedOf(show.since_unix_s);
  else if (show.last_exit) {
    const ex = show.last_exit;
    status = ex.code === 0 ? "finished at " + clock(ex.at_unix_s)
      : ex.ctrl_break ? "stopped at " + clock(ex.at_unix_s)
      : "ended with an error at " + clock(ex.at_unix_s) + (ex.hint ? ": " + ex.hint : ex.code === null ? "" : " (code " + ex.code + ")");
  }
  // A report that made it into the card keeps its text there, under Details.
  const inCard = show.id === "report" && !!report.id && !report.error && !show.running;
  setText($("toolStatus"), inCard ? status + " · shown in the report card above" : status);
  const stop = $("toolStop");
  setHidden(stop, !show.running);
  stop.disabled = busy.has(show.id);
  stop.dataset.stop = show.id;
  const pre = $("toolLog");
  setHidden(pre, inCard);
  const text = show.log.join("\n");
  if (pre.textContent !== text) { pre.textContent = text; pre.scrollTop = pre.scrollHeight; }
}

function paintAdvanced() {
  const c = comp("capture");
  if (c) {
    const flags = c.flags.filter((f) => !SAVE_FLAGS.has(f.flag));
    const key = JSON.stringify([flags.map((f) => f.flag), c.running]);
    if (key !== flagsKey) {
      flagsKey = key;
      $("advFlags").innerHTML = flags.map((f) =>
        '<label title="' + esc(f.flag + " — " + f.help) + '"><input type="checkbox" data-flag="' + esc(f.flag) + '" ' +
        (chosen.flags.has(f.flag) ? "checked " : "") + (c.running ? "disabled " : "") + ">" + esc(FLAG_LABELS[f.flag] || f.flag) + "</label>"
      ).join("") || '<span class="small">none in this build</span>';
    }
  }
  const cfgNote = state.config ? ({ seeded: "written on first start", loaded: "", defaults: "no file; built-in defaults" }[state.config.status] || "") : "";
  const rows = [
    ["config", (state.config && state.config.path) || PLACES.config, cfgNote, "config"],
    ["recordings", state.recording && state.recording.dir, "", "recordings"],
    ["logs", PLACES.logs, PLACES.logs ? "" : "this build writes no log files", PLACES.logs ? "logs" : ""],
    ["binaries", PLACES.bin_dir, "", ""],
    ["browser cache", PLACES.webview_data, PLACES.webview_data ? "the window's page engine; safe to delete when the panel is closed" : "not used", ""],
    ["docs", PLACES.docs, "", "docs"],
  ];
  const html = rows.map(([k, v, note, open]) =>
    "<div><b>" + esc(k) + "</b><code>" + esc(v || "—") + "</code>" +
    (open && v ? '<button class="btn small" data-open="' + open + '">Open</button>' : "<span></span>") +
    (note ? '<span class="small">' + esc(note) + "</span>" : "") + "</div>"
  ).join("");
  const host = $("places");
  if (host.dataset.key !== html) { host.dataset.key = html; host.innerHTML = html; }
  for (const [id, pre] of [["capture", "capLog"], ["viz", "vizLog"]]) {
    const cc = comp(id);
    const el = $(pre);
    const text = cc ? cc.log.join("\n") : "";
    if (el.textContent !== text) { el.textContent = text; if (el.closest("details").open) el.scrollTop = el.scrollHeight; }
  }
}

function renderProcs() {
  const host = $("procs");
  const ps = state.processes;
  const managed = new Set(state.components.filter((c) => c.running).map((c) => c.pid));
  const key = JSON.stringify([ps.map((p) => [p.pid, p.kind, p.name, managed.has(p.pid)]), [...armed.keys()], [...busy]]);
  if (key === procsKey) {
    // Same rows: patch the live numbers in place.
    for (const p of ps) {
      const row = host.querySelector('tr[data-pid="' + p.pid + '"]');
      if (!row) continue;
      setText(row.querySelector("[data-cpu]"), p.cpu_pct.toFixed(1));
      setText(row.querySelector("[data-mem]"), p.mem_mb.toFixed(1));
      setText(row.querySelector("[data-up]"), elapsedOf(p.started_unix_s));
    }
    return;
  }
  procsKey = key;
  if (!ps.length) { procsKey = ""; host.innerHTML = '<div class="empty">The process list came back empty, which should not happen (this panel is a telemouse process). Check the panel log.</div>'; return; }
  host.innerHTML = "<table><thead><tr><th>pid</th><th>kind</th><th>name</th><th>command</th><th class=\"right\">cpu %</th><th class=\"right\">mem MB</th><th>up</th><th></th></tr></thead><tbody>" +
    ps.map((p) => {
      const key = "p" + p.pid;
      const isArmed = armed.has(key);
      const btn = p.is_self
        ? '<span class="small">this panel</span>'
        : '<button class="btn small danger ' + (isArmed ? "arm" : "") + '" data-kill="' + p.pid + '" ' + (busy.has(key) ? "disabled" : "") + ">" + (isArmed ? "Confirm" : "Force stop") + "</button>";
      return '<tr class="' + (p.is_self ? "self" : "") + '" data-pid="' + p.pid + '">' +
        '<td class="mono">' + p.pid + (p.parent ? '<span class="small"> ← ' + p.parent + "</span>" : "") + "</td>" +
        '<td><span class="kind">' + esc(p.kind) + "</span>" + (managed.has(p.pid) ? ' <span class="small">started here</span>' : "") + "</td>" +
        '<td class="mono">' + esc(p.name) + "</td>" +
        '<td class="cmd" title="' + esc(p.cmd) + '">' + esc(p.cmd) + "</td>" +
        '<td class="right" data-cpu>' + p.cpu_pct.toFixed(1) + "</td>" +
        '<td class="right" data-mem>' + p.mem_mb.toFixed(1) + "</td>" +
        '<td class="mono" data-up title="' + esc(clock(p.started_unix_s)) + '">' + elapsedOf(p.started_unix_s) + "</td>" +
        "<td>" + btn + "</td></tr>";
    }).join("") + "</tbody></table>";
}

/* ---------- wiring (delegated: the lists re-render) ---------- */
/* The one document-level click handler: buttons that are re-rendered, or
   repeated across the page, are handled here by their data attribute. */
document.addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  if (b.dataset.wizExe || "wizSkip" in b.dataset) { wizButton(b); return; }
  if (!state) return;
  if (b.dataset.open) openPlace(b.dataset.open);
  else if (b.dataset.run) runTool(b.dataset.run);
  else if (b.dataset.stop) stopComponent(b.dataset.stop, true, TOOL_TITLES[b.dataset.stop] || b.dataset.stop);
  else if (b.dataset.report) { chosen.session = b.dataset.report; $("reportSel").value = b.dataset.report; runTool("report"); }
  else if (b.dataset.kill) {
    const p = state.processes.find((x) => String(x.pid) === b.dataset.kill);
    if (p) killProcess(p);
  }
});
document.addEventListener("change", (e) => {
  const t = e.target;
  if (t.dataset && t.dataset.flag) { if (t.checked) chosen.flags.add(t.dataset.flag); else chosen.flags.delete(t.dataset.flag); }
});
document.addEventListener("toggle", (e) => {
  const d = e.target;
  if (d.tagName === "DETAILS" && d.open) for (const pre of d.querySelectorAll("pre.log")) pre.scrollTop = pre.scrollHeight;
}, true);

/* For js-tests (and a curious devtools console): the pure pieces and the
   poll loop, without reaching into the page's scope. */
window.telemouseCtl = { esc, renderReport, poll, mergeLogs, logCursor, durWords, fnum, FIELD_INPUTS };

/* Opened in a background tab: nothing runs until it is looked at. */
if (document.hidden) sleepPage(); else wakePage();
