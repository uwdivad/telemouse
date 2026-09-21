# Developing telemouse

Building from source, what each crate does, how capture works, the build
flavours, Kafka, observability, tests, benches and releasing. The user-facing
page is the [README](../README.md); the module-by-module internals are in
[GUIDE.md](GUIDE.md); every machine interface (HTTP routes, JSON shapes,
files on disk) is in [API.md](API.md); the workspace rules are in
[CONVENTIONS.md](CONVENTIONS.md). This file is not in the release zip.

## Building from source

Needs Rust 1.98 or later (the version `rust-toolchain.toml` pins) on
Windows; the C runtime is linked statically (`.cargo/config.toml`), so the
binaries need no Visual C++ Redistributable. WebView2, which the control
panel's window uses to show its page, is linked through the static loader
library, so no DLL ships either.

```powershell
# 0. One-time: check your environment (QPC clock, monitors, UDP, Kafka reachability)
cargo run -p telemouse-capture -- doctor

# 1. Start capturing. Ctrl-C stops; the marker hotkey (F9 by default) drops a marker ("clutch", "round start"...);
#    so does every line written to its stdin when that is a pipe (the panel's marker API)
cargo run --release -p telemouse-capture -- run --print

# 2. In another terminal: live viz — then open http://127.0.0.1:7879
cargo run --release -p telemouse-viz

# 3. After a session: what did I record, and how did I aim?
cargo run -p telemouse-analyze -- list
cargo run -p telemouse-analyze -- report recordings\<session>.jsonl

# Or drive all of the above from one window: build once, run the panel
cargo build --release --workspace
target\release\telemouse-ctl.exe
```

UDP live-viz and JSONL recording are always on and need no external services.
Kafka is on in the repo config and expects the local broker from `compose.yaml`
(`docker compose up -d`); if it is not running, capture logs a warning and
carries on without it. A bundled demo recording
(`recordings/demo-session.jsonl`) lets you try replay and analysis immediately.

Rust users can install straight from the repository (one command per binary;
the workspace has four binary crates, so `cargo install --git` needs a name):

```powershell
cargo install --git https://github.com/uwdivad/telemouse telemouse-ctl
cargo install --git https://github.com/uwdivad/telemouse telemouse-capture
cargo install --git https://github.com/uwdivad/telemouse telemouse-viz
cargo install --git https://github.com/uwdivad/telemouse telemouse-analyze
```

`telemouse.toml`, `logs/` and `recordings/` then live next to
`telemouse-ctl.exe` (see *What it writes* in [HELP.md](HELP.md)).

The dashboard is tested in Chrome and Edge; the OBS overlay in OBS 28 and
later (its embedded Chromium).

## The pieces

| Binary | Crate | Role |
|---|---|---|
| `telemouse` | `crates/capture` | Capture agent: raw input → 25ms batches → UDP + asynchronous JSONL recording + optional Kafka (full build) |
| `telemouse-viz` | `crates/viz` | Live browser visualization + session replay (UDP→WebSocket bridge, single-file page) |
| `telemouse-analyze` | `crates/analyze` | Offline metrics over recorded sessions |
| `telemouse-ctl` | `crates/ctl` | Control panel: a native window (WebView2) and tray icon over an HTTP JSON API; starts/stops the others, runs the tools, sees and kills telemouse processes |
| `telemouse-mcp` | `crates/mcp` | MCP server over stdio (`rmcp`): the analyze library and the ctl/viz HTTP APIs as typed tools for an agent |
| — | `crates/core` | Shared types, wire format, config, QPC↔UTC clock math |

`tools/` holds optional extras that nothing else depends on: `cpubench`
(a CPU harness, not a workspace member) and `kafka2parquet` (Kafka-to-Parquet
notebooks). `mouse-telemetry-plan.md` at the repo root is the original design
document, of which storage landed as JSONL rather than TimescaleDB.

## Configuration

[telemouse.example.toml](../telemouse.example.toml) is the sample that ships
with releases (as `telemouse.toml`) and that `telemouse-ctl` writes as
`telemouse.toml` on first start when none exists: every server on loopback,
Kafka off.
The repository's own [telemouse.toml](../telemouse.toml) is the development
machine's config — LAN viz bind, Kafka on, its broker addresses — and is
what the binaries read when run from this checkout. All fields optional:

```toml
mouse_cpi = 1600.0            # your mouse's real CPI/DPI → physical cm
marker_hotkey = "f9"          # system-wide chord that drops a marker; the game never sees it; "" = none

[batch]
window_ms = 25                # responsive live default; use 50 to halve per-batch CPU
coalesce_ms = 8               # raw-input drain cadence (0–10); 0 = exact per-report stamps, ~13× the capture CPU

[udp]
addr = "127.0.0.1:7878"       # capture → viz live path

[kafka]
enabled = false               # durable log; capture degrades gracefully without it
brokers = ["127.0.0.1:9092"]  # every entry needs a port

[recording]
enabled = true                # save data: one JSONL file per session (the control panel shows and can override this per run)
dir = "recordings"            # per-session JSONL files

[ctl]
http_addr = "127.0.0.1:7880"  # control panel; keep it on loopback — it can kill processes
stop_grace_secs = 8           # Ctrl-Break → wait this long → terminate (covers the agent's sink drains)
log_dir = "logs"              # ctl.log + one <component>.log per launched component
hotkey = "ctrl+alt+r"         # system-wide (tray): (re)start capture as a new saved session; "" for none

# Aim-space conversion, per game: degrees = counts * sens * coeff.
# Key = lowercase process name of the game (matched automatically).
[games."cs2.exe"]
sens = 1.0
yaw_coeff = 0.022             # Source/Quake/Apex: 0.022
[games."cod.exe"]
sens = 1.0                    # set to your in-game sensitivity
yaw_coeff = 0.0066            # modern CoD (and Overwatch): 0.0066; Valorant: 0.07
pitch_coeff = 0.0066
```

Game keys must be the lowercase executable name (`cs2.exe`, not `CS2.exe`);
the config is refused otherwise, because a key that never matches would
silently turn every aim metric into a guess. Relative paths in the file
(`recording.dir`, `ctl.log_dir`, `ctl.bin_dir`) are relative to the file's
own directory. Every binary looks for `telemouse.toml` in the current
directory first and next to its own executable second, and refuses to start
on a file it cannot parse rather than silently running on defaults.

Raw counts stay raw on the wire; cm and degrees are derived in consumers from
this config — so you can fix a wrong CPI or sens *after* the fact and re-analyze.

**What a recording contains.** Besides the mouse deltas, every batch carries
the name of the foreground executable, the pointer-lock state, the screen size
and (outside games) the absolute cursor position, and the session record
carries your monitor layout and device names. A recording is therefore also a
timestamped log of which application had focus. Recordings are never pruned
by telemouse; they live in `recording.dir` until you delete them.

**Adding a config key** means rebuilding every binary: the panel spawns
whatever is in `bin_dir`, and an older child refuses a `telemouse.toml` with
keys it does not know. Update `telemouse.example.toml` too (a test in
`telemouse-core` compares it against the defaults field by field).

## How capture works

```
mouse HID ──WM_INPUT──▶ T1 hot path ──▶ lock-free SPSC ring ──▶ T2 shipper (25ms batches)
            (QPC timestamp, zero alloc,                            ├─▶ UDP → telemouse-viz → browser (<10ms)
             never blocks)                                         ├─▶ bounded queue → JSONL writer
T3 context (250ms): foreground game,                               └─▶ bounded queue → Kafka worker (optional)
pointer-lock heuristic, cursor, screen
```

- **T1** never allocates or blocks after startup; if the ring ever fills, events
  are counted as drops (a visible data-quality metric), never a capture stall.
- **T1 batches its reads and stays off the queue while the mouse moves.**
  Being woken by the raw-input queue costs Windows ~25–30µs of kernel CPU per
  wake, however many reports are waiting. So T1 waits on the queue only while
  the mouse is still; a burst's first report wakes it, and from then on a
  periodic high-resolution timer paces one `GetRawInputBuffer` drain every
  `batch.coalesce_ms + 1` ms (default 8 + 1) until a drain comes back empty.
  Measured at 1kHz on a 5950X: the capture thread went 2.8% → 1.4% (first
  pass, wait-then-drain) → 0.22% of a core (timer-paced, 8ms); the whole
  stack — capture, viz with a browser connected, the control panel being
  polled — 2.57% → 0.76% loaded, 0.29% → 0.09% idle. A burst's first
  report is stamped exactly; the rest are spread over the drain period, so
  a 1kHz mouse still analyzes as 1.00ms intervals (a pause shorter than the
  window inside a burst is smoothed over). The window is recorded in the
  session envelope as `coalesce_ms`; set it to 0 for one read per report and
  exact stamps (~2.8% of a core), or 2 for the first pass's behaviour (0.45%).
- Each session opens with a `session` record: QPC frequency, QPC↔UTC anchor,
  CPI, sens table, monitor setup — everything needed to reconstruct physical
  units later.
- UDP stays on T2 for minimum live latency. JSONL writes/flushes and Kafka
  connection/production run on bounded workers, so slow storage or an
  unreachable broker cannot stall capture or the live stream. Queue drops and
  abandoned shutdown work are visible in the periodic stats line.
- Useful flags: `--print` (per-batch log line), `--no-kafka` / `--no-udp` /
  `--no-record`, `--duration-secs N` (smoke tests).

## Live viz & replay

`telemouse-viz` serves one self-contained page (no CDN, works offline):

- **Desk-space panel** — your hand's path in real cm, velocity-colored trail
  with time decay.
- **Aim-space panel** — crosshair path in degrees (yaw unbounded, with dashed
  seams at every ±180°; pitch clamped), using the sens profile of whatever game is foreground.
- Click rings per button (an expanding flash on press, plus a steady ring on
  the head for as long as the button is held), wheel ticks, marker toasts, live readouts (cm/s,
  °/s, session distance, clicks/min, events/s, ring drops, bad frames).
- **Replay** — pick any recorded session: play/pause, scrub (exact re-integration),
  0.25×–8× speed, jump-to-marker. Live and replay share the same engine;
  live is just replay at 1× of "now". Keys: `R` recenter, `Space` pause,
  `←/→` seek, `V` cycle views. `?session=<id>` on the URL opens straight
  into replay of that recording (what the panel's *Dashboard* does).
- **Go to time** — type a date/time (local) in the transport bar and the
  page picks the recording that was running then and seeks to that moment;
  the play head's wall-clock time is shown beside the elapsed time.
  `?at=2026-08-29T21:14:03` on the URL opens straight into replay there.
- **View toggle** — show both panels or just one: the Both / Desk / Aim
  switch in the top bar, or `?view=desk` / `?view=aim` on the URL (the
  switch keeps the URL in sync, so a bookmark remembers it).
- **Draw-rate cap** — the dashboard repaints at most 120 times a second
  (`?fps=` to change), and 30 while its window is not focused, so a tab
  left open behind a game does not compete with it for the GPU. The data
  is untouched: only repaints are skipped.
- `?profile=1` shows an in-page frame-time breakdown.

### OBS browser source

The same page has a stream-overlay mode at `/obs`: no chrome, transparent
background, a small HUD instead of the stats bar (the README has the setup
and the second-PC case). Defaults live in `telemouse.toml` under
`[viz.obs]`; every one of them can be overridden per source with URL
parameters, so several sources can share one config and still differ:

| Param | Values | Default |
|---|---|---|
| `layout` | `split` (desk ǀ aim), `stack`, `desk`, `aim` — `view=desk\|aim\|both` is accepted as an alias | `split` |
| `bg` | `transparent`, `rrggbb`, `rrggbbaa` (e.g. `0e131c80` = half-opaque panel) | `transparent` |
| `hud` | comma list of `speed, aim, cpm, eps, dist, aimdist, clicks, game, latency, eventage`; empty hides it | `speed,aim,cpm` |
| `hudpos` | `bottom-left`, `top-left`, `top-right`, `bottom-right` | `bottom-left` |
| `scale` | 0.5–4 — stroke, marker and HUD size, for small sources on a 1080p canvas | `1` |
| `trail` | 0.3–12 s of trail decay | `3` |
| `buffer` | 10–200 ms live buffer (lower = less latency, more stutter risk) | `35` |
| `grid`, `legend`, `labels` | `0`/`1` | `1`, `0`, `0` |
| `fps` | 5–400 — draw-rate cap; OBS composites at its own rate, so match it | `60` |
| `stale` | 0–60 s without data before the overlay dims and shows *no feed*; `0` never | `3` |

e.g. an aim-only overlay in a corner: `/obs?layout=aim&hud=aim,cpm&scale=1.6&grid=0`.
`?obs=1` on the dashboard URL does the same thing. In this mode the page
skips the per-frame stats-bar work, only allocates an alpha canvas when the
background is actually see-through, never writes to `localStorage`, and
shows no toasts (markers still flash the panels).

When the viz is bound to a LAN address, a peer that is not this machine is
served `/obs`, its live socket `/ws` and `/healthz` — nothing else: the
dashboard, the recording list and the recordings themselves answer `403 not
served to the network`. The server only trusts `Host`/`Origin` headers that
are `localhost` or an IP address (its DNS-rebinding guard), so a hostname
URL is answered with `403 host not allowed`. The capture → viz UDP hop stays
on loopback either way, and `telemouse-ctl` stays on loopback (it can kill
processes); its *viz* link still opens the local address.

## Control panel

`telemouse-ctl` is one process: an HTTP JSON API on `http://127.0.0.1:7880`
with the panel page, a native window that shows that page, and a tray icon.
The page has a **Session** section (one *Start recording* / *Stop
recording* button with a save-to-disk switch, elapsed time and live numbers,
a marker field, the dashboard controls), **Recordings** (with *Report* and
*Dashboard*), **Tools** (*Check my setup* = `doctor`, *Report*,
*Trend*, output shown in a panel) and a collapsed **Advanced** section: the
capture flags with plain labels (`--print` / `--no-kafka` / `--no-udp`
underneath), every process on the machine whose executable is a telemouse
binary, or a `cargo run` of one, whoever started it (pid, parent, command
line, CPU, memory, start time, and a two-click *Force stop*; the panel lists
itself but will not kill itself), where things are with *Open* buttons, and
the capture and viz logs. The save switch defaults to `[recording] enabled`
in `telemouse.toml`; the panel adds `--record` / `--no-record` only when you
flip it the other way.

*Stop* is two-stage: the child is spawned in its own process group and gets a
`CTRL_BREAK` (the same path as Ctrl-C in a terminal, so the capture agent
flushes its partial batch and closes its sinks in order), and is terminated if
it is still alive after `ctl.stop_grace_secs`. *Force stop* skips straight to
terminating. Closing the panel with Ctrl-C stops what it started.

Binaries are looked up next to `telemouse-ctl` itself (so `cargo build
--workspace` is all the setup there is), then on `PATH`; `[ctl] bin_dir` or
`--bin-dir` points elsewhere. The API is plain JSON (`GET /api/state`,
`POST /api/components/{id}/start|stop|marker`, `POST /api/processes/{pid}/kill`,
`POST /api/open`; every machine interface is written up in [API.md](API.md));
mutating calls must carry an `X-Telemouse-Ctl: 1` header, which keeps a random
web page open in the same browser from reaching the panel through `localhost`,
and every request must carry a `Host` naming this machine (an IP literal or
`localhost`), which defeats DNS rebinding. `telemouse-viz` applies the same
`Host` rule and additionally refuses WebSocket upgrades from a non-local
`Origin`, so a web page cannot read the live stream. Neither server has any
authentication beyond that: keep both on loopback unless you mean otherwise.
The panel refuses to kill any process the scan would not list, and never
accepts arbitrary command-line arguments — only the flags shown on the page.

### The window and the tray

On Windows the panel runs a small **native GUI** in the same process as the
server: one window titled *telemouse* (1120×760 scaled to DPI, minimum
720×520) that hosts the panel page through **WebView2**, the browser engine
that ships with Windows 10 and 11, and a tray icon. No browser opens on a
normal start; links the page opens in a new tab (the dashboard, the docs,
the releases) go to the system browser. Hidden to the tray, the WebView2
stops rendering and the page drops to its slow poll, so the window costs
nothing while a game is in front. The internals (the STA sequence, the
handlers, the watchdog) are in GUIDE.md §19.5.

**The fallback.** If the WebView2 Runtime is missing, fails to start, or
does not answer within 10 s, the window shows the previous read-only
monospace text status view (`gui::model::window_text`: every component,
the related processes, the log tail of whatever is running) with a banner
saying why, and the page is opened in the default browser once; tray →
*Open in browser* reopens it. Installing the "WebView2 Runtime" from
Microsoft fixes it. To simulate a missing runtime on a machine that has
one:

```powershell
$env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = "C:\nope"; target\release\telemouse-ctl.exe
```

WebView2 keeps its cache in `%LOCALAPPDATA%\telemouse\WebView2` (falling
back to `%TEMP%\telemouse\WebView2`), never next to the config: the unzip
folder may be read-only, synced or on a share, and the browser writes tens
of megabytes. It is reported as `places.webview_data` in `GET /api/state`
and shown in the page's *Where things are*. WebView2 is never
feature-gated: a minimal build needs the window too.

**Build and run, step by step**

1. Build every binary once. The panel launches the others from the directory
   it lives in, so one workspace build is the whole setup:

   ```powershell
   cargo build --release --workspace
   ```

2. Start the panel — any of these:

   ```powershell
   target\release\telemouse-ctl.exe                          # from a terminal: logs go to the console and logs\ctl.log
   cargo run -p telemouse-ctl -- serve --bin-dir target\debug   # debug build, pointed at the debug binaries
   ```

   or double-click `target\release\telemouse-ctl.exe` in Explorer. Started
   that way the panel is the only process on its console, so it hides the
   console window and you get just the GUI. (The console itself stays —
   the graceful stop is a `CTRL_BREAK`, which needs one — so don't be
   surprised to see it in Task Manager.) The panel's own log is in
   `logs\ctl.log`, and everything a launched component printed is in
   `logs\<component>.log` — that is where to look when something died
   while you weren't watching.

3. What appears:
   - the window titled **telemouse** with the panel page (dark by default,
     light when Windows is, with a toggle in the header); the text view
     with a banner instead if the page could not be hosted.
   - a **grey disc in the tray**. It turns **green while the capture agent
     runs**; its tooltip shows both services and the uptime.
   - the same page in any browser at `http://127.0.0.1:7880`.

4. Use the tray:
   - **Left click** the icon: show / hide the window.
   - **Right click**: *Show/Hide window* · *Start capture (save data →
     recordings)* / *Start capture (don't save)* — or, while it runs, *Stop
     capture (saving data | not saving)* and *New session* (greyed if the
     binary is not built) · *Start/Stop viz server* · *Open in browser*
     (the same page, in your default browser) · *Open logs folder* · *Open
     recordings folder* · *Edit telemouse.toml* · *Open docs* · *Exit*.
   - **Close** or **minimise** the window: it only hides to the tray.
   - **Quit**: *Exit* from the tray, Shift+close on the window, or Ctrl-C in
     the console. All three stop what the panel started (Ctrl-Break, then
     terminate after `ctl.stop_grace_secs`) and then remove the icon.

**Flags**

```powershell
telemouse-ctl.exe serve --no-gui                     # headless: web page only, no window, no tray (also the behaviour off Windows)
telemouse-ctl.exe serve --no-webview                 # tray + text-view window, no Edge components loaded; the page stays in your browser
telemouse-ctl.exe serve --http 127.0.0.1:7899        # panel on another port
telemouse-ctl.exe serve --bin-dir D:\telemouse\bin   # binaries somewhere else
telemouse-ctl.exe serve --config C:\path\telemouse.toml
telemouse-ctl.exe serve --log-dir D:\telemouse\logs  # ctl.log + <component>.log somewhere else
$env:RUST_LOG = 'info,telemouse_ctl=debug'; telemouse-ctl.exe   # snapshot cadence, tray events, the WebView2 callbacks
```

**If something is off**

- Window is text-only with a banner: the WebView2 Runtime is missing or
  could not start; the banner and `ctl.log` say which. The page is in your
  browser meanwhile.
- No icon, but the window is there: the tray refused the icon (rare; it is
  logged). The window then stays up and closing it exits.
- No window and no icon: the GUI thread could not create its window; the
  panel keeps serving the web page and logs why. Run from a terminal to see
  the log.
- Icon vanished after Explorer restarted: it is re-added automatically on
  the `TaskbarCreated` message; give it a second.
- Console window hidden and you want it: run from a terminal, or `--no-gui`.

**Tests**: `cargo test -p telemouse-ctl` (the child-process tests take about
a minute) or `cargo test -p telemouse-ctl gui::` for just the GUI logic,
which runs anywhere in well under a second. The page is embedded with
`include_str!`: editing `crates/ctl/src/index.html` does nothing until the
crate is rebuilt.

## Playing with telemouse running

The whole stack — capture agent, viz bridge with a browser connected, control
panel being polled — costs ~0.7% of one core at 1kHz after the 2026-08-29 CPU
pass (the capture agent itself ~0.4%, its raw-input thread ~0.2%) and is not
what you feel in a game. What you can feel is the *viewers*: a dashboard tab on a 240Hz
monitor was measured at over a core of Chrome GPU-process time (two full
canvases, 240 repaints a second, on the game's GPU), and the control panel
page at ~5% of a core scanning the process table. Both are now capped
(120fps / 30fps unfocused; a 4s scan cache), but the cheapest option while
playing is still: close the dashboard tab and hide the panel window to the
tray (a hidden WebView2 stops rendering), keep the OBS source (which draws
at OBS's own rate), and open the dashboard afterwards for replay. If you
want it open, `?fps=60` on the URL halves the cost again.

## The analysis report

`telemouse-analyze report <session.jsonl>` prints a structured summary (a bare
session id from `list` works too, looked up in `--dir`); add `--json out.json`
and/or `--csv-dir DIR` (per-second aggregates + flick table) for further
processing, or `--summary` for the headline numbers as a few KB of JSON —
what a script or an agent should read instead of the full report. Metric
groups, per the plan's catalog:

- **Data quality** — inter-event interval histogram (1KHz mice should cluster
  at ≤1ms), gaps, ring drops, timestamp monotonicity.
- **Kinematics** — smoothed (Savitzky–Golay) velocity/accel/jerk in counts,
  cm and degrees; hand travel; path efficiency.
- **Flicks** — detection with amplitude, peak velocity, duration,
  **overshoot ratio** (the sens-too-high/too-low tell), settle time,
  time-to-click. Thresholds tunable via `--flick-threshold` etc.
- **Micro-control** — correction counts, tremor RMS + 8–12Hz band power
  (fatigue indicator), micro-adjustment size distribution.
- **Trigger discipline** — pre-click stability, click-to-still latency,
  hold durations, double-click intervals, clicks/min.
- **Segmentation & habits** — `--split-by-marker` sub-reports between hotkey
  markers (round/clutch analysis), per-minute fatigue table, inferred
  mousepad repositioning lifts, `--locked-only` to keep desktop-mode noise
  out of aim metrics.

Longitudinal tracking: `report --json-dir DIR` caches per-session reports, and

```powershell
cargo run -p telemouse-analyze -- trend --dir recordings
```

prints one row per session (flicks, overshoot, settle, tremor, path
efficiency) — the substrate for warmup curves, day-to-day consistency, and
sensitivity A/B experiments. Every detector threshold is a CLI flag.

## Kafka (durable log)

`mouse.events` (batches, keyed by session id), `mouse.sessions` (compacted
session configs), `mouse.markers` (hotkey/game-state annotations). JSON
envelopes today; the tagged wire format leaves a seam for a binary schema.
Capture creates the three topics on connect.

A single-node KRaft broker for local use ships as `compose.yaml` (Apache Kafka
3.9, data in a named volume, bound to `127.0.0.1:9092`):

```powershell
docker compose up -d                                  # start the broker
cargo run -p telemouse-capture -- doctor              # ...should now say "reachable"
cargo run --release -p telemouse-capture -- run       # ships every batch to Kafka too

# watch batches arrive (Ctrl-C to stop)
docker compose exec kafka /opt/kafka/bin/kafka-console-consumer.sh `
  --bootstrap-server localhost:9092 --topic mouse.events --from-beginning

docker compose down                                   # stop; "down -v" also wipes the data
```

The sink is a bounded queue in front of a forwarder thread (rskafka, zstd,
25ms linger). If the broker stalls the queue fills and envelopes are dropped
and counted (`kafka_dropped` in the stats line) rather than ever blocking
capture; `--no-kafka` turns the sink off for one run. The recording is the
source of truth: when the agent stops it writes
`recordings/<session>.meta.json` with the final counters, and
`telemouse-analyze list` shows a `LOSS` column (`kafka=400`) for any session
where a sink did not deliver everything, so a broker outage is visible
afterwards without re-deriving it from the JSONL.

## Build flavours

Logging, Kafka and observability are Cargo features, on by default. The
release zip is the default build: all five binaries with everything on
(Kafka compiled in, off in the shipped config). A **minimal** flavour with
the three features compiled out still exists as a build-it-yourself
option; it is no longer published, but CI lints and tests it on every push
so the flags keep working.

| Feature | Crates | What it adds |
|---|---|---|
| `logging` | capture, viz, analyze, ctl | the `tracing` subscriber: stderr (colour only on a terminal, `NO_COLOR` honoured) plus `logs/<component>.log`, size-rotated |
| `observability` | capture, viz, ctl | the 5-second stats lines, latency histograms, `<session>.meta.json` sidecars, `/api/stats`, the stall flag in `/healthz`, child health on the panel and tray |
| `kafka` | capture | the Kafka sink (`rskafka`, zstd); without it `[kafka] enabled = true` is ignored with a warning |
| `quiet` | capture, viz, ctl | compiles every `tracing` call out (`release_max_level_off`) |

```powershell
cargo build --release --workspace                      # everything (what you want on your own machine)
cargo build --release -p telemouse-capture -p telemouse-viz -p telemouse-ctl `
  --no-default-features --features telemouse-capture/quiet,telemouse-viz/quiet,telemouse-ctl/quiet
                                                        # minimal: tracking and visuals only, no log files, no stats, no Kafka
```

The panel starts children with whatever features it was built with; mixing
flavours works (the wire format is the same), you just get less telemetry.
The minimal command above leaves `telemouse-analyze` out; without it the
panel's *Report* has nothing to run, so add
`cargo build --release -p telemouse-analyze` if you want reports. The
panel's window (WebView2) is in both flavours: it is not behind a
feature. To lay a minimal build out like a release, copy the three
exes from `target\release` next to `telemouse.example.toml` renamed to
`telemouse.toml`.

## Observability

In the full build everything logs through `tracing` (`RUST_LOG` to adjust,
default `info`, Kafka client chatter at `warn`). Every binary opens with one
line naming its version, build profile, the config file it read and whether
that file existed, and the values that differ from the defaults. Every
long-running loop emits a structured stats line every 5s — events/s, ring
drops *and* ring high-water, capture→ship latency percentiles, per-sink
errors, polling-rate estimate, current game — and anything that stays wrong
is repeated as a warning once a minute: a sink that is dead or dropping, a
ring overflow, no input for a minute. Latency is measured on one shared
250 µs histogram at every hop, so the agent's and the bridge's percentiles
subtract: capture→ship in the agent (recorded after the send), bridge
p50/p99 in `telemouse-viz` (also at `/api/stats` and pushed live into the
page), and an end-to-end latency tile in the browser. `seq_no` gaps
(transport loss) are counted in the bridge as well as the page, separately
from ring drops. Every binary installs a panic hook that routes panics
through the same log, so a thread dying in a tray-launched process is still
written to `logs/`. The viz's `/healthz` answers 503 with
`{"ok":false,"udp_bound":false,...}` while its UDP listener is not bound and
carries `"feed":"live|stalled|never"` once it is. The agent rewrites
`recordings/<session>.meta.json` every 5 seconds with `"exit":"running"`
until it stops cleanly, so a session that ended in a crash or a forced
shutdown is recognisable afterwards; console close, logoff and Windows
shutdown all stop the agent gracefully.

## Tests, benches, CI

```powershell
cargo test --workspace                          # no mouse, admin, Kafka, or browser needed
node --check crates/viz/src/app.js
node --test "crates/viz/js-tests/*.test.mjs"    # the page's engine against a stub DOM, plus a check that every
                                                # element id app.js looks up exists in index.html
                                                # (cargo test runs them too when node is installed)
cargo bench -p telemouse-analyze                # criterion benches over the loader + hot math
cargo bench -p telemouse-core                   # wire encode/decode, batcher
cargo build --profile profiling                 # release speed + debug symbols for flamegraphs
```

What CI (`.github/workflows/ci.yml`) runs on every push, and what to run
before calling a change done — a formatting or clippy miss fails CI:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p telemouse-capture -p telemouse-viz -p telemouse-ctl --all-targets --locked --no-default-features --features telemouse-capture/quiet,telemouse-viz/quiet,telemouse-ctl/quiet -- -D warnings
cargo test --workspace --locked
cargo test -p telemouse-core -p telemouse-capture -p telemouse-viz -p telemouse-ctl --locked --no-default-features
node --check crates/viz/src/app.js
node --test "crates/viz/js-tests/*.test.mjs"
```

Win32 code is isolated behind `#[cfg(windows)]`; all metric math, batching,
clock mapping and wire logic is pure and unit-tested (flick detection is tested
against synthetic streams with known ground truth). The embedded pages
(`crates/viz/src/index.html`, `app.js`, `crates/ctl/src/index.html`) are
compiled in with `include_str!`, so editing them does nothing until the
crate is rebuilt.

Numbers, method and what is left on the table: [BENCHMARKS.md](BENCHMARKS.md).
The August 2026 performance/observability audit and its resolutions are
documented in [AUDIT-2026-08.md](AUDIT-2026-08.md).
The September implementation pass, including reproducible before/after replay,
analyzer, memory, and sink-latency measurements, is documented in
[PERFORMANCE-2026-09.md](PERFORMANCE-2026-09.md). Where telemouse
plugs into agents (an MCP server, post-session triage, experiment runners,
a live feature stream) and what has landed so far is in
[AGENTIC-2026-09-13.md](AGENTIC-2026-09-13.md); the machine
interfaces it builds on are in [API.md](API.md). The anti-cheat exposure
audit behind [FAIR-PLAY.md](FAIR-PLAY.md) is
[ANTICHEAT-2026-09-14.md](ANTICHEAT-2026-09-14.md): the shipped binaries
only read — never open a handle on a process that is not a telemouse
process, never hook, never synthesize input, never draw over the game,
never advise running as administrator. `tmbench inject` is the one
`SendInput` in the tree; it stays out of the release zips and behind
`TMBENCH_ALLOW_INJECT=1`.

## Releasing

A release is a tag: bump `version` in the root `Cargo.toml`, rename the
`## [Unreleased]` section of `CHANGELOG.md` to `## [X.Y.Z] — date`, commit,
then

```powershell
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin master vX.Y.Z
```

`release.yml` refuses a tag that does not match the Cargo version, runs the
CI gates (fmt, clippy and tests for the default and the minimal flavour),
builds the workspace in release mode with default features, signs the four
executables when signing is configured (below), and packs **one zip**,
`telemouse-vX.Y.Z-windows-x86_64.zip`: `telemouse.exe`, `telemouse-viz.exe`,
`telemouse-analyze.exe`, `telemouse-ctl.exe`, the sample config as
`telemouse.toml`, the license, the user docs (`README.md`, `CHANGELOG.md`,
`docs/GUIDE.md`, `docs/FAIR-PLAY.md`, `docs/API.md`) and the demo recording.
It writes a `.sha256` beside the zip, attaches a build-provenance
attestation, and publishes a GitHub Release whose notes are that changelog
section. This file, `CONVENTIONS.md`, the audit notes and the design plan
stay out of the zip. Releases up to v0.1.3 had two zips (a minimal one
without the analyzer and a `-full` one); the minimal flavour is now only a
build-it-yourself option (*Build flavours* above).

### The icon

Every `build.rs` puts `assets/telemouse.ico` on its executable next to the
version resource and the manifest (16, 24, 32, 48, 64 and 256 px; the tray
disc in the panel's accent `#35d0e0` on its dark `#0b0f16`). The file is
generated, not drawn by hand: `pwsh assets/make-icon.ps1` rewrites it, and
the build scripts rerun when it changes. Explorer caches icons per path, so
a rebuilt exe can show the old picture until the folder is refreshed or the
file is copied somewhere else.

### Code signing

`release.yml` signs the four executables with **Azure Artifact Signing**
(the service called *Trusted Signing* until 2026; the action is
`azure/artifact-signing-action@v2`): SHA-256 digest, RFC 3161 timestamp from
`http://timestamp.acs.microsoft.com`, then a step that fails the release
unless `Get-AuthenticodeSignature` says `Valid` and timestamped for each
exe. Signing happens before packaging, so the zip, its hash and the
attestation describe the signed files.

**It is off until the repository is configured.** The job computes
`SIGNING` from three secrets and three variables; while any of them is
missing the signing steps are skipped, a notice says so on the run, and the
release is published unsigned as before. Nothing else needs to change on the
day the account exists.

Authentication is OIDC (workload identity federation): GitHub hands the job a
short-lived token, Entra ID exchanges it, and no client secret is stored
anywhere. The job runs in the GitHub environment `release` so that the
token's subject is the same for every tag.

One-time setup, in order:

1. **Azure subscription.** Create one at portal.azure.com (pay-as-you-go).
   For an individual, the billing account must be of type *Individual* and
   its legal name and sold-to address must match your government ID: the
   identity validation reads them from there and they end up in the
   certificate subject (name, city, state, country; not street or email).
   Public Trust for individuals is currently limited to the US and Canada;
   organizations have a longer country list. Check the quickstart's
   prerequisites before paying for anything.
2. **Resource provider.** Subscription → *Resource providers* → register
   `Microsoft.CodeSigning`.
3. **Artifact Signing account.** Portal → *Artifact Signing Accounts* →
   *Create*: a resource group, an account name (3–24 characters, globally
   unique), a region, pricing tier **Basic**. Note the region's endpoint,
   e.g. East US = `https://eus.codesigning.azure.net/`, West Europe =
   `https://weu.codesigning.azure.net/`.
4. **Identity validation.** Give yourself the role *Artifact Signing
   Identity Verifier* on the account (Access control (IAM) → Add role
   assignment), then account → *Identity validations* → *Individual* →
   *New identity* → *Public*. It is portal-only and ends with a photo-ID
   check on your phone (Microsoft Authenticator + a verification partner).
   Minutes when it goes well, up to 20 business days when documents are
   requested.
5. **Certificate profile.** Account → *Certificate profiles* → *Create* →
   **Public Trust**, pick the validated identity. Note the profile name.
   (*Public Trust Test* profiles are not trusted by Windows; do not use one
   for releases.)
6. **App registration.** Entra ID → *App registrations* → *New
   registration* (name it e.g. `telemouse-release`, single tenant, no
   redirect URI). Note the *Application (client) ID* and *Directory
   (tenant) ID*.
7. **Federated credential.** On that app: *Certificates & secrets* →
   *Federated credentials* → *Add credential* → scenario *GitHub Actions
   deploying Azure resources*: organization `uwdivad`, repository
   `telemouse`, entity type **Environment**, environment name `release`.
   The subject must read `repo:uwdivad/telemouse:environment:release`.
   No client secret is created.
8. **Role.** On the *certificate profile* (or the signing account): Access
   control (IAM) → Add role assignment → **Artifact Signing Certificate
   Profile Signer** (listed as *Trusted Signing Certificate Profile Signer*
   in older tenants) → assign to the `telemouse-release` app. That
   assignment is also what lets `azure/login` see the subscription; if the
   login step says *No subscriptions found*, add *Reader* on the resource
   group.
9. **GitHub.** Repository → *Settings* → *Secrets and variables* →
   *Actions*. Use these exact names:

   | Kind | Name | Value |
   |---|---|---|
   | secret | `AZURE_CLIENT_ID` | the app registration's Application (client) ID |
   | secret | `AZURE_TENANT_ID` | the Directory (tenant) ID |
   | secret | `AZURE_SUBSCRIPTION_ID` | the subscription's ID |
   | variable | `SIGNING_ENDPOINT` | the region endpoint from step 3, e.g. `https://eus.codesigning.azure.net/` |
   | variable | `SIGNING_ACCOUNT_NAME` | the Artifact Signing account name |
   | variable | `SIGNING_CERTIFICATE_PROFILE` | the certificate profile name |

   Repository-level or on the `release` environment both work (GitHub
   creates that environment on the first tagged run; you can add a required
   reviewer to it if you want a manual gate before anything is signed).
10. Tag a release. The run must show *Sign the executables* and *Verify the
    signatures* green; locally, `Get-AuthenticodeSignature .\telemouse-ctl.exe`
    on the unzipped file says `Valid`.

If OIDC is not an option, the action also takes `AZURE_TENANT_ID`,
`AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET` as environment variables (a
client secret on the app registration, which expires and has to be
rotated); the workflow does not use that path.

**Cost.** Basic is about USD 9.99 a month and includes 5,000 signatures (a
release uses four); there is no per-certificate fee. The certificates are
short-lived (days) and rotate on their own, which is why the timestamp
matters: it keeps an old release verifying after its certificate expired.

**What signing does not buy.** A Public Trust signature removes the
"unknown publisher" wording and gives SmartScreen a stable identity to
attach reputation to, but reputation still has to be earned: a new signer's
downloads can show *Windows protected your PC* until enough people have run
them, and because the certificates rotate, reputation follows the validated
identity rather than one certificate. That is why the README keeps its
*More info → Run anyway* note, worded as "if Windows shows…".

### Later: winget

Not before there is a signed public release. The manifest
(`uwdivad.telemouse`, three YAML files submitted to `microsoft/winget-pkgs`,
`wingetcreate new <zip url>` writes them) would need: `InstallerType: zip`
with `NestedInstallerType: portable`; one `NestedInstallerFiles` entry per
exe with its `RelativeFilePath` inside the zip's top folder
(`telemouse-vX.Y.Z-windows-x86_64\telemouse-ctl.exe`, …) and a
`PortableCommandAlias`; the release zip's URL and `InstallerSha256`;
`Architecture: x64`. Open question for then: winget links portable exes
into its own `Links` folder, and telemouse finds `telemouse.toml`,
`recordings\` and its sibling binaries next to the exe, so the panel has to
be checked when started through that link.
