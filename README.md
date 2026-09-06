# telemouse

Raw mouse telemetry for gaming. Captures true HID deltas — pre-acceleration,
microsecond-quality timestamps — while you play, streams them to a live
visualization, records every session, replays them, and computes an aim-metrics
report over the data. Implements [mouse-telemetry-plan.md](mouse-telemetry-plan.md).

**Passive and anticheat-safe by design:** input comes from the Windows Raw
Input API (`RIDEV_INPUTSINK` on a hidden message-only window — the OS delivers
a copy of every HID report), and game detection only reads the foreground
process's executable name. No injection, no hooks, no reads of game memory,
no in-game overlay.

## The pieces

| Binary | Crate | Role |
|---|---|---|
| `telemouse` | `crates/capture` | Capture agent: raw input → 25ms batches → UDP + asynchronous JSONL recording + optional Kafka |
| `telemouse-viz` | `crates/viz` | Live browser visualization + session replay (UDP→WebSocket bridge, single-file page) |
| `telemouse-analyze` | `crates/analyze` | Offline metrics over recorded sessions |
| `telemouse-ctl` | `crates/ctl` | Control panel: start/stop the others, run the tools, see and kill telemouse processes |
| — | `crates/core` | Shared types, wire format, config, QPC↔UTC clock math |

## Quick start

Prebuilt binaries: each [GitHub release](https://github.com/uwdivad/telemouse/releases)
ships `telemouse-vX.Y.Z-windows-x86_64.zip` (the four executables,
`telemouse.toml`, docs) with a SHA-256 alongside. Unzip, edit
`telemouse.toml`, run `telemouse-ctl.exe`. From source:

```powershell
# 0. One-time: check your environment (QPC clock, monitors, UDP, Kafka reachability)
cargo run -p telemouse-capture -- doctor

# 1. Start capturing. Ctrl-C stops; F9 drops a marker ("clutch", "round start"...)
cargo run --release -p telemouse-capture -- run --print

# 2. In another terminal: live viz — then open http://127.0.0.1:7879
cargo run --release -p telemouse-viz

# 3. After a session: what did I record, and how did I aim?
cargo run -p telemouse-analyze -- list
cargo run -p telemouse-analyze -- report recordings\<session>.jsonl

# Or drive all of the above from one page: build once, open http://127.0.0.1:7880
cargo build --release --workspace
target\release\telemouse-ctl.exe
```

UDP live-viz and JSONL recording are always on and need no external services.
Kafka is on in the repo config and expects the local broker from `compose.yaml`
(`docker compose up -d`); if it is not running, capture logs a warning and
carries on without it. A bundled demo recording
(`recordings/demo-session.jsonl`) lets you try replay and analysis immediately.

## Configuration

[telemouse.toml](telemouse.toml) — all fields optional:

```toml
mouse_cpi = 1600.0            # your mouse's real CPI/DPI → physical cm

[batch]
window_ms = 25                # responsive live default; use 50 to halve per-batch CPU
coalesce_ms = 8               # raw-input drain cadence (0–10); 0 = exact per-report stamps, ~13× the capture CPU

[udp]
addr = "127.0.0.1:7878"       # capture → viz live path

[kafka]
enabled = true                # durable log; capture degrades gracefully without it
brokers = ["127.0.0.1:9092"]

[recording]
enabled = true                # save data: one JSONL file per session (the control panel shows and can override this per run)
dir = "recordings"            # per-session JSONL files

[ctl]
http_addr = "127.0.0.1:7880"  # control panel; keep it on loopback — it can kill processes
stop_grace_secs = 5           # Ctrl-Break → wait this long → terminate
log_dir = "logs"              # ctl.log + one <component>.log per launched component

# Aim-space conversion, per game: degrees = counts * sens * coeff.
# Key = lowercase process name of the game (matched automatically).
[games."cs2.exe"]
sens = 1.0
yaw_coeff = 0.022             # Source/Quake/Apex: 0.022
[games."cod.exe"]
sens = 6.0
yaw_coeff = 0.0066            # modern CoD (and Overwatch): 0.0066; Valorant: 0.07
pitch_coeff = 0.0066
```

Raw counts stay raw on the wire; cm and degrees are derived in consumers from
this config — so you can fix a wrong CPI or sens *after* the fact and re-analyze.

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
  °/s, session distance, clicks/min, events/s, ring drops).
- **Replay** — pick any recorded session: play/pause, scrub (exact re-integration),
  0.25×–8× speed, jump-to-marker. Live and replay share the same engine;
  live is just replay at 1× of "now". Keys: `R` recenter, `Space` pause,
  `←/→` seek, `V` cycle views.
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

## OBS browser source

The same page has a stream-overlay mode: no chrome, transparent background,
a small HUD instead of the stats bar. In OBS add a **Browser** source with

```
http://127.0.0.1:7879/obs
```

(leave OBS's default custom CSS in place — it just makes the body
transparent, which the page does anyway). Size it however you like; the
panels fill the source. Tick *Refresh browser when scene becomes active* if
you toggle the scene a lot — the page reconnects to the bridge on its own
either way.

### OBS on a different computer

The overlay is just a web page, so a streaming PC on the same LAN can load it
from the gaming PC. Three things on the gaming PC (the one running capture and
`telemouse-viz`):

1. Bind the viz to every interface instead of loopback, in `telemouse.toml`:

   ```toml
   [viz]
   http_addr = "0.0.0.0:7879"
   ```

   (or once, without touching the config: `telemouse-viz serve --http 0.0.0.0:7879`).
   Restart `telemouse-viz`; it logs a warning that it is reachable from the
   network, which is the point.

2. Let the port through Windows Firewall, Private profile only, from an
   elevated PowerShell:

   ```powershell
   New-NetFirewallRule -DisplayName "telemouse-viz (LAN)" -Direction Inbound `
     -Protocol TCP -LocalPort 7879 -Action Allow -Profile Private
   ```

   (`Remove-NetFirewallRule -DisplayName "telemouse-viz (LAN)"` undoes it.)
   The network the two PCs share must be marked *Private* in Windows
   (`Get-NetConnectionProfile`), or the rule does not apply.

3. Find the gaming PC's LAN address: `ipconfig` → *IPv4 Address* on the wired
   or Wi-Fi adapter, e.g. `192.168.1.168`. Give it a DHCP reservation on the
   router if you can, so the OBS source does not go stale after a reboot.

Then, in OBS on the streaming PC, the Browser source URL is

```
http://192.168.1.168:7879/obs
```

with the same URL parameters as above. **Use the IP literal, not the PC's
name**: `http://GAMING-PC:7879/obs` is answered with `403 host not allowed`,
because the server only trusts `Host`/`Origin` headers that are `localhost`
or an IP address (its DNS-rebinding guard). Note there is no login: anyone
who can reach the port can watch the live stream and download every
recording, so keep the bind on a network you trust, and set `http_addr` back
to `127.0.0.1:7879` when you do not need it. The capture → viz UDP hop stays
on loopback either way, and `telemouse-ctl` stays on loopback (it can kill
processes); its *viz* link still opens the local address.

Defaults live in `telemouse.toml` under `[viz.obs]`; every one of them can be
overridden per source with URL parameters, so several sources can share one
config and still differ:

| Param | Values | Default |
|---|---|---|
| `layout` | `split` (desk ǀ aim), `stack`, `desk`, `aim` — `view=desk\|aim\|both` is accepted as an alias | `split` |
| `bg` | `transparent`, `rrggbb`, `rrggbbaa` (e.g. `0e131c80` = half-opaque panel) | `transparent` |
| `hud` | comma list of `speed, aim, cpm, eps, dist, aimdist, clicks, game, latency`; empty hides it | `speed,aim,cpm` |
| `hudpos` | `bottom-left`, `top-left`, `top-right`, `bottom-right` | `bottom-left` |
| `scale` | 0.5–4 — stroke, marker and HUD size, for small sources on a 1080p canvas | `1` |
| `trail` | 0.3–12 s of trail decay | `3` |
| `buffer` | 10–200 ms live buffer (lower = less latency, more stutter risk) | `55` |
| `grid`, `legend`, `labels` | `0`/`1` | `1`, `0`, `0` |
| `fps` | 5–400 — draw-rate cap; OBS composites at its own rate, so match it | `60` |

e.g. an aim-only overlay in a corner: `/obs?layout=aim&hud=aim,cpm&scale=1.6&grid=0`.
`?obs=1` on the dashboard URL does the same thing. In this mode the page
skips the per-frame stats-bar work, only allocates an alpha canvas when the
background is actually see-through, never writes to `localStorage`, and
shows no toasts (markers still flash the panels).

## Control panel

`telemouse-ctl` serves one page at `http://127.0.0.1:7880` with two halves:

- **Components** — a card each for the capture agent, the viz server, `doctor`,
  `analyze trend` and `analyze report`. *Start* launches the binary with the
  panel's own `--config`; the capture card has a **save data** switch (it
  defaults to `[recording] enabled` in `telemouse.toml`; the panel adds
  `--record` / `--no-record` only when you flip it the other way), shows
  *saving → recordings/* or *not saving* while the agent runs, and exposes
  `--print` / `--no-kafka` / `--no-udp` as checkboxes; the report card has a
  recording picker. The top bar shows the configured default. Each card
  shows pid, uptime, how the last run exited, and the last lines the child
  printed (`stdout` and `stderr`, kept across restarts).
- **Related processes** — every process on the machine whose executable is a
  telemouse binary, or a `cargo run` of one, whoever started it: pid, parent,
  command line, CPU, memory, start time, and a two-click *kill* button. The
  panel lists itself but will not kill itself.

*Stop* is two-stage: the child is spawned in its own process group and gets a
`CTRL_BREAK` (the same path as Ctrl-C in a terminal, so the capture agent
flushes its partial batch and closes its sinks in order), and is terminated if
it is still alive after `ctl.stop_grace_secs`. *Kill* skips straight to
terminating. Closing the panel with Ctrl-C stops what it started.

Binaries are looked up next to `telemouse-ctl` itself (so `cargo build
--workspace` is all the setup there is), then on `PATH`; `[ctl] bin_dir` or
`--bin-dir` points elsewhere. The API is plain JSON (`GET /api/state`,
`POST /api/components/{id}/start|stop`, `POST /api/processes/{pid}/kill`);
mutating calls must carry an `X-Telemouse-Ctl: 1` header, which keeps a random
web page open in the same browser from reaching the panel through `localhost`,
and every request must carry a `Host` naming this machine (an IP literal or
`localhost`), which defeats DNS rebinding. `telemouse-viz` applies the same
`Host` rule and additionally refuses WebSocket upgrades from a non-local
`Origin`, so a web page cannot read the live stream. Neither server has any
authentication beyond that: keep both on loopback unless you mean otherwise.
The panel refuses to kill any process the scan would not list, and never
accepts arbitrary command-line arguments — only the flags shown on the cards.

### Tray icon and status window

On Windows the panel also runs a small **native GUI**: a tray icon and a
status window, in the same process as the web page. It is deliberately
minimal (one plain window, no GPU-rendered toolkit) so it costs nothing while
a game is running.

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
   - a window titled **telemouse-ctl**: a monospace readout of every
     component (running / stopped / not built, pid, uptime, how the last run
     exited), the related processes with CPU % and memory, and the log tail
     of whatever is running (capture first, then viz, else the last thing
     that ran). It refreshes once a second while it is on screen.
   - a **grey disc in the tray**. It turns **green while the capture agent
     runs**; its tooltip shows both services and the uptime.
   - the web page, unchanged, at `http://127.0.0.1:7880`.

4. Use the tray:
   - **Left click** the icon: show / hide the window.
   - **Right click**: *Show/Hide window* · *Start capture (save data →
     recordings)* / *Start capture (don't save)* — or, while it runs, *Stop
     capture (saving data | not saving)* (greyed if the binary is not
     built) · *Start/Stop viz server* · *Open web panel* (your browser, on
     the panel's address) · *Exit*. The window's `SAVE DATA` line shows the
     configured default and what the running agent is actually doing.
   - **Close** or **minimise** the window: it only hides to the tray.
   - **Quit**: *Exit* from the tray, Shift+close on the window, or Ctrl-C in
     the console. All three stop what the panel started (Ctrl-Break, then
     terminate after `ctl.stop_grace_secs`) and then remove the icon.
   - Doctor, the analyzers and process *kill* stay on the web page; the
     tray covers only the two everyday services.

5. Typical session: right-click → *Start capture* (icon goes green) →
   *Start viz server* → *Open web panel* → play → *Exit* when done.

**Flags**

```powershell
telemouse-ctl.exe serve --no-gui                     # headless: web page only (also the behaviour off Windows)
telemouse-ctl.exe serve --http 127.0.0.1:7899        # panel on another port
telemouse-ctl.exe serve --bin-dir D:\telemouse\bin   # binaries somewhere else
telemouse-ctl.exe serve --config C:\path\telemouse.toml
telemouse-ctl.exe serve --log-dir D:\telemouse\logs  # ctl.log + <component>.log somewhere else
$env:RUST_LOG = 'info,telemouse_ctl=debug'; telemouse-ctl.exe   # snapshot cadence, tray events
```

**If something is off**

- No icon, but the window is there: the tray refused the icon (rare; it is
  logged). The window then stays up and closing it exits.
- No window and no icon: the GUI thread could not create its window; the
  panel keeps serving the web page and logs why. Run from a terminal to see
  the log.
- Icon vanished after Explorer restarted: it is re-added automatically on
  the `TaskbarCreated` message; give it a second.
- Console window hidden and you want it: run from a terminal, or `--no-gui`.

**Tests**: `cargo test -p telemouse-ctl` (51 tests; the child-process tests
take about a minute) or `cargo test -p telemouse-ctl gui::` for just the GUI
logic, which runs anywhere in well under a second.

## Playing with telemouse running

The whole stack — capture agent, viz bridge with a browser connected, control
panel being polled — costs ~0.7% of one core at 1kHz after the 2026-08-29 CPU
pass (the capture agent itself ~0.4%, its raw-input thread ~0.2%) and is not
what you feel in a game. What you can feel is the *viewers*: a dashboard tab on a 240Hz
monitor was measured at over a core of Chrome GPU-process time (two full
canvases, 240 repaints a second, on the game's GPU), and the control panel
page at ~5% of a core scanning the process table. Both are now capped
(120fps / 30fps unfocused; a 4s scan cache), but the cheapest option while
playing is still: close the dashboard and panel tabs, keep the OBS source
(which draws at OBS's own rate), and open the dashboard afterwards for
replay. If you want it open, `?fps=60` on the URL halves the cost again.

## The analysis report

`telemouse-analyze report <session.jsonl>` prints a structured summary; add
`--json out.json` and/or `--csv-dir DIR` (per-second aggregates + flick table)
for further processing. Metric groups, per the plan's catalog:

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
- **Segmentation & habits** — `--split-by-marker` sub-reports between F9
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
capture; `--no-kafka` turns the sink off for one run.

## Observability

Everything logs through `tracing` (`RUST_LOG` to adjust, default `info`).
Every long-running loop emits a structured stats line every 5s — events/s,
ring drops *and* ring high-water, capture→ship latency p50/p99, per-sink
errors, idle-vs-broken heartbeat, current game — so a silent data-quality
problem doesn't exist. Latency is measured at every hop: capture→ship
histograms in the agent, bridge p50/p99 in `telemouse-viz` (also at
`/api/stats` and pushed live into the page), and an end-to-end latency tile
in the browser alongside lag-behind-live and render FPS. `seq_no` gaps
(transport loss) are tracked separately from ring drops everywhere.

## Development

```powershell
cargo test --workspace              # no mouse, admin, Kafka, or browser needed
cargo bench -p telemouse-analyze    # criterion benches over the loader + hot math
cargo bench -p telemouse-core       # wire encode/decode, batcher
# numbers, method and what is left on the table: docs/BENCHMARKS.md
cargo build --profile profiling     # release speed + debug symbols for flamegraphs
```

Append `?profile=1` to the viz URL for an in-page frame-time breakdown.
The August 2026 performance/observability audit and its resolutions are
documented in [docs/AUDIT-2026-08.md](docs/AUDIT-2026-08.md).
The September implementation pass, including reproducible before/after replay,
analyzer, memory, and sink-latency measurements, is documented in
[docs/PERFORMANCE-2026-09.md](docs/PERFORMANCE-2026-09.md).

**Releasing.** CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on
every push. A release is a tag: bump `version` in the root `Cargo.toml`, add
a `## [X.Y.Z]` section to `CHANGELOG.md`, commit, then

```powershell
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin master vX.Y.Z
```

`release.yml` refuses a tag that does not match the Cargo version, builds and
tests in release mode, zips the binaries with the config and docs, and publishes
a GitHub Release whose notes are that changelog section.

Win32 code is isolated behind `#[cfg(windows)]`; all metric math, batching,
clock mapping and wire logic is pure and unit-tested (flick detection is tested
against synthetic streams with known ground truth). Workspace conventions:
[docs/CONVENTIONS.md](docs/CONVENTIONS.md).
