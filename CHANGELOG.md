# Changelog

Releases are cut by pushing a `vX.Y.Z` tag that matches `[workspace.package]
version` in `Cargo.toml`; the `release` workflow builds, tests, packages and
publishes the section below that names that version.

## [Unreleased]

- **No sample games on a fresh install.** The shipped `telemouse.toml` kept
  two example `[games]` entries, so a new panel listed two games nobody had
  added under *Settings → Your games*. They are comments in the sample now;
  an existing `telemouse.toml` is not touched.
- **`telemouse doctor --json`.** The environment check prints one
  `telemouse-doctor/1` document on stdout — a schema tag, an overall
  `verdict`, and a row per check with a stable `id`, a `status` of
  `pass`/`warn`/`fail`, a human `title` and `detail`, the `hint` when there
  is something to do about it, and the resolved config — so a script or an
  assistant can read the same answer a person gets. Logs stay on stderr, so
  the output pipes straight into a JSON parser. Text and JSON are rendered
  from the same checks, which means the text mode now leads each row with
  its status and says which of them are worth acting on; a debug build, a
  missing config, a screenless machine and a broker that is configured but
  unreachable are called out where they used to be facts you had to read.
  Exit codes are unchanged: `0` whenever a report was produced, `fail` rows
  included, non-zero only when doctor could not get that far. The control
  panel allow-lists `--json` for its *Check my setup* component, so it can
  be asked for over `POST /api/components/doctor/start`
  (`docs/API.md`, `docs/HELP.md`).
- **Tick→UTC conversion without 128-bit division.**
  `QpcAnchor::qpc_to_utc_us` and `ticks_to_us` scale QPC ticks to
  microseconds in 64-bit integers: the delta is carried as an unsigned
  magnitude plus a sign, 10 MHz (every real Windows box) becomes a divide by
  ten that compiles to a multiply, and odd frequencies split the ticks into
  whole seconds and a remainder. The i128 expression stays as the fallback
  wherever 64 bits could overflow, so results are bit-identical to before for
  every input — checked against the old implementation over ~1.6 M
  pseudo-random cases and an explicit edge sweep. The analyzer calls this
  once per event: 8.6 ns → 1.9 ns per call, about −100 ms on a 15 M-event
  report (`docs/PERFORMANCE-2026-09-20.md`, item A4; new
  `crates/core/benches/clock.rs`).
- **The viz server actually stops when it is asked to.** Stopping *Viz
  server* from the panel sent a Ctrl-Break that `telemouse-viz` logged and
  then ignored: it had no shutdown path, so the panel sat out its whole
  grace period (`ctl.stop_grace_secs`, 8 s) and terminated the process,
  which the panel then showed as "exited: code 1". It now closes its
  listener on the signal, tells every connected dashboard, `/obs` overlay
  and script with a WebSocket `Close` frame (the page shows "disconnected"
  and reconnects by itself), answers a socket that reconnects mid-stop with
  `503`, and exits 0 — in about 100 ms in practice. A connection that will
  not end on its own (a browser holding a keep-alive socket, a recording
  still streaming out of `/api/session/{id}`) is bounded by a 750 ms
  deadline and dropped, with one log line naming what was still open, so a
  stop can no longer turn into a kill.
- **The report summary names the marked stretches.**
  `telemouse-analyze report <id> --summary` now carries `markers`
  (`{t_s, label}`) and `segments` (per marker interval: the `label` that
  opens it, the `next_label` that closes it, and the flick, overshoot,
  settle, tremor, path-efficiency and click-rate numbers for that stretch),
  with `markers_total` / `segments_total` beside them. An experiment marked
  `"sens A"` / `"sens B"` can be read off the few-KB summary instead of the
  whole report; at most the first 12 of each are listed and a label longer
  than 120 characters is clipped, so the document stays the size it was. An
  unmarked recording lists neither.
- The summary's schema tag is `telemouse-report-summary/2`. The control
  panel serves a cached `recordings/.reports/<id>.summary.json` only when it
  carries the tag this build knows: a finished recording never changes
  again, so without that test a summary cached before the fields existed
  would be served for the rest of the recording's life. Cached summaries
  from an older build are simply recomputed on the next report.
- The panel's report card lists what you marked, with the offset into the
  recording, when the session has markers.
- **Reports come back about twice as fast.** `telemouse-analyze report` on a
  1156 MB recording went from 6.4 s to 3.3 s, and on a 462 MB one from 3.2 s
  to 1.8 s. Reading the file was the bottleneck — a single-threaded JSON loop
  while the rest of the machine idled — so recordings over 8 MB are now split
  at line boundaries and parsed on every core: 3.1 s of loading became 0.5 s.
  The magnitude helper behind speed, distance and overshoot no longer goes
  through the C runtime's overflow-safe `hypot`, which was 8.5% of the
  analyzer's CPU on its own. Peak memory is unchanged and every number in the
  report is bit-identical to the old build's, over 19 M events of comparison.
- **`telemouse-analyze trend` analyzes sessions at the same time.** A cold
  trend over 36 recordings dropped from ~30 s to ~16 s, a warm one from 1.4 s
  to 0.6 s. Up to four sessions run at once, bounded by how many recording
  bytes are in flight, so a gigabyte-sized recording still runs on its own and
  peak memory is where it was. The table is ordered by session start time as
  before, whatever order the sessions happen to finish in.
- **A still hand no longer looks like a dead capture agent.** The agent
  sends a `{"type":"heartbeat", session_id, ts_utc_us}` datagram once a
  second while no batch is going out, so the viz can tell "capture alive,
  hand still" from "capture gone". Past `stale_secs` the OBS overlay now
  shows *idle · 12s* with the panels at full brightness while those
  heartbeats keep arriving, and only dims to *no feed · 12s* once they stop
  too; the dashboard's pill says *idle · 12s* instead of *waiting for
  capture* / *no data for 12s*. The heartbeat is live-only — it is never
  written to a recording and never produced to Kafka — and carries neither
  a `seq_no` nor a `ts_anchor_us`, so it cannot disturb the seq-gap counter,
  the latency estimator or the datagram-gap histogram. It costs no new timer
  either: the shipping loop already wakes once a second when idle for its
  sink tick, and the heartbeat rides that wake.

  A `telemouse-viz` from v0.2.0 or earlier ignores the new datagram: it
  counts one `parse_errors` per heartbeat and logs one warning a minute,
  and is otherwise unaffected.
- **`window_ms` now means what it says.** The batch window was timed from the
  first event's timestamp but tested against the wall clock, and raw-input
  reads are coalesced — they back-date a drain's stamps by up to
  `coalesce_ms + 1` ms. Every window was short by that much, so the shipped
  default of 25 ms produced about **52 batches a second instead of 40**. The
  window is now wall-clock and runs on a fixed grid, so a live stream ships
  exactly `1000 / window_ms` batches a second. At the same `window_ms` you
  will see **fewer, fuller batches** in recordings and on the dashboard —
  ~25 events per batch at 1 kHz where it used to be ~18 — and a batch
  sequence number covers more time. Nothing about per-event data changes:
  timestamps are still the raw-input stamps, and analysis is unaffected. An
  event reaches the wire within `window_ms + coalesce_ms + 1` ms, and the
  first batch after an idle desk still goes out one window after the hand
  moved.
- **The shipping thread wakes once per batch, not twice.** It used to fall
  back to its idle park after every flush, be unparked by the capture thread
  on the next drain (a syscall on the hot thread), and then park again on the
  window timer. The window now stays open across a flush while the stream is
  live. Together with the cadence fix that is about −0.25% of a core on the
  live path; the capture thread makes one wake-up syscall per burst instead
  of one per batch.
- **The status window stops repainting text nobody changed.** The window's
  text view (what `--no-webview`, a missing WebView2 runtime or a page that
  is still loading shows) rewrote and repainted its full contents on every
  refresh, a second apart, whether or not anything had moved — 0.26–0.32%
  of a core while the window was open, a quarter of it GDI glyph drawing.
  It is now written only when the render says something new: an unchanged
  one is skipped, one whose words changed (a component that started or
  stopped, a new log line) is shown at once, and one where only digits
  moved — the clock, an uptime, a CPU column — waits up to five seconds, so
  the view still ticks without paying for it. The tray tooltip, which
  carries the same uptime, follows the same rule instead of calling
  `Shell_NotifyIcon` every second. When the text does change, the first
  visible line *and* any selection are restored, so a reader of the log
  tail keeps their place and a half-finished copy survives the update.

## [0.2.0] — 2026-09-20

The control panel becomes a program with a window instead of a tray icon
and a browser tab, its page is redesigned for someone who has never read
the docs, and the README is split into a user README and a developer guide.
On top of that: settings are edited in the panel instead of in TOML, a
first-run guide sets CPI and game and records a demo, reports come back as
a readable card, the dashboard shares the panel's look, and the release is
one zip with an icon and (once configured) signed executables.

- **Settings in the panel.** Mouse CPI, the marker and new-recording keys,
  the recordings folder, your games and the OBS overlay look are edited in a
  *Settings* card and saved to `telemouse.toml` without touching your
  comments or other keys (`GET`/`POST /api/config`; the patched file is
  parsed and validated before an atomic write, and a save is refused with
  409 if the file changed on disk underneath it). Network addresses, Kafka
  and the program and log folders stay hand-edit only. The panel reloads
  its own copy at once; a running capture or viz picks changes up on its
  next start, and the page says so.
- **Guided first run.** The "edit the TOML" banner is replaced by a
  three-step guide: set your CPI, pick your game from the programs seen in
  front while you alt-tab (or type it) and give its sensitivity, then
  record a 30-second demo that ends in the report card, or open the shipped
  sample recording. Every step can be skipped and the guide can be re-run
  from Settings. `/api/state` gained `foreground_seen` on the capture
  component for this (full build); there is still no list of game
  executables anywhere.
- **Readable report card.** After a report the panel shows a plain-language
  card: a data-loss verdict in words, session tiles and ten aim metrics
  each explained in one line, with the full text under *Details*
  (`GET`/`POST /api/reports/{id}`, built on `telemouse-analyze report
  --summary`). Reports and trends run from the panel cache under
  `recordings/.reports/`, so repeat runs are instant. Doctor output stays
  text for now.
- **Viz: the OBS "no feed" indicator now works.** Nothing ever evaluated
  it, so `/obs` never dimmed whatever `stale_secs`/`?stale=` said, and the
  dashboard pill stayed on *waiting for capture* even with data flowing.
  Both are now driven from the 10 Hz repaint: the overlay dims and shows a
  *no feed · Ns* badge after `stale_secs` without a batch, does not flash
  while the page connects, and recovers on the next batch. A hidden OBS
  source or inactive scene now really stops drawing. Note that the agent
  sends nothing while the mouse is at rest, so a still hand reads as no
  feed after `stale_secs`; raise it or set it to 0 if that bothers you.
- **Viz dashboard restyle.** Same design tokens as the panel: dark by
  default, light via Windows or the new theme button (Shift-click to follow
  the system again); the canvases follow the theme, including a velocity
  ramp that reads on white. The `/obs` overlay is never themed and stays
  transparent. Top bar, transport and stats bar wrap cleanly down to phone
  width. New `js-tests/feed.test.mjs` drives the frame loop on a fake
  clock.
- **Analyzer.** `--help` groups the flags under Input / Output / Filtering /
  Advanced headings with plainer descriptions and copy-pasteable examples;
  no flag or default changed. Double-clicking `telemouse-analyze.exe` in
  Explorer now explains that it is a command-line tool, points at the
  panel's Report button and waits for Enter instead of flashing a console;
  runs with arguments, from a terminal or with piped stdio are unchanged.
- **One zip.** Releases ship `telemouse-vX.Y.Z-windows-x86_64.zip` with all
  four binaries including the analyzer, built with default features (Kafka
  stays off in the shipped config). The separate `-full` zip is gone; the
  minimal flavour is still linted and tested in CI and is a documented
  build-it-yourself option.
- **Icon and signing.** Every executable carries the telemouse icon
  (`assets/telemouse.ico`, generated by `assets/make-icon.ps1`), and the
  panel window uses it. `release.yml` can sign the executables with Azure
  Artifact Signing (formerly Trusted Signing) over OIDC and verifies each
  signature; the step is skipped, leaving releases unsigned, until the
  repository has the secrets and variables listed in `docs/DEVELOPING.md`,
  "Code signing".
- core: `AppConfig::from_toml` parses and validates config text the same
  way `load` does.
- **A native panel window.** `telemouse-ctl.exe` opens its own window
  titled *telemouse* and shows the control panel in it through WebView2, the
  browser engine that ships with Windows 10 and 11; no browser tab opens on
  a normal start. Links the page opens in a new tab (the dashboard, the
  docs, the releases) go to the system browser. If the WebView2 Runtime is
  missing, fails to start or does not answer within 10 s, the window falls
  back to the previous read-only text status view with a banner saying why,
  and the page is opened in the default browser once. New flag
  `serve --no-webview` keeps the tray and the text window and loads no Edge
  components; `--no-gui` is unchanged. The tray item *Open web panel* is now
  *Open in browser*. WebView2 keeps its cache in
  `%LOCALAPPDATA%\telemouse\WebView2` (shown as `places.webview_data` and in
  the page's *Where things are*); it is the one thing telemouse writes
  outside its own folder, and the removal instructions in `docs/HELP.md`
  name it.
- **Panel page redesign.** Dark by default, light when Windows is (with a
  toggle in the header); a first-run guide when the config was just seeded
  (see above); one *Start recording* / *Stop recording* button
  with a save-to-disk switch, elapsed time and live numbers; markers from
  the page (a label field and *Add marker*, over the existing marker route);
  a recordings list with *Report* and *Dashboard*; tool output
  (doctor, report, trend) shown readably in a panel; plain wording
  throughout (no raw flags or pids on the main surface; capture flags, the
  process table with force-stop, where things are, and the logs live under
  a collapsed *Advanced* section). New route `POST /api/open`
  `{ "target": "config"|"recordings"|"logs"|"docs" }` opens that place with
  the shell's default handler (guard header required; the path comes from
  the server, never the request).
- **Viz.** The dashboard's stats bar gained a *bad frames* tile; its
  painter had been throwing on every tick because the `statBad` element did
  not exist, which left the other tiles unpainted. A new Node test
  (`js-tests/dom-ids.test.mjs`) asserts every element id `app.js` looks up
  exists in `index.html`, and the test command is now
  `node --test "crates/viz/js-tests/*.test.mjs"`. `/?session=<id>` on the
  dashboard opens that recording in replay directly.
- **Docs.** `README.md` is now a one-screen introduction: what telemouse
  is, a dashboard screenshot, three steps to a first report, what you get,
  the fair-play statement and links. The look-it-up material (checking the
  download, the window and tray, settings, the dashboard, OBS on a second
  PC, hotkeys, what it writes and how to remove it, troubleshooting,
  reporting problems) is in the new `docs/HELP.md`, which ships in the zip
  with the screenshot; everything about building, internals, flavours,
  Kafka and releasing moved to `docs/DEVELOPING.md`. The zip no longer
  ships `docs/CONVENTIONS.md` or `mouse-telemetry-plan.md` (contributor material; the guide links to the
  plan on GitHub).

## [0.1.3] — 2026-09-15

Anticheat posture, markers from the panel and from a pipe, and the machine
interfaces an agent needs: session-id reports with a JSON summary, sidecars
in the viz session list, and one reference for every route and file.

- **Anticheat audit and hardening.** `docs/ANTICHEAT-2026-09-14.md` audits
  every Win32 call against what Call of Duty's RICOCHET and Activision's
  enforcement policy act on (public record in `-sources.md`; the user-facing
  statement is `docs/FAIR-PLAY.md`). What changed: the capture agent no
  longer opens a handle on the foreground process to learn its name — it
  reads a process-table snapshot, so it holds no handle on any process but
  itself and names an elevated game without being elevated; `tmbench
  inject` (the benchmark harness's `SendInput` load generator, never
  shipped) refuses to run without `TMBENCH_ALLOW_INJECT=1`; every executable
  carries a Windows version resource (product, description, version) and an
  application manifest (`asInvoker`, per-monitor DPI, Windows 10+); the
  panel's access-denied hint names the protected-folder cause instead of
  suggesting "run as administrator"; the README no longer says
  "anticheat-safe" (no publisher endorses third-party tools; "passive" is
  what the code supports).
- **`marker_hotkey`.** The capture agent's marker chord is configurable:
  `marker_hotkey = "f9"` at the top of `telemouse.toml`, same grammar as
  `[ctl] hotkey`, `""` for none; a `[ctl] hotkey` equal to it is rejected at
  load. New key: rebuild every binary (an older agent refuses the file).
- **Markers from a pipe.** When the capture agent's stdin is a pipe, every
  line written to it becomes a labelled marker (`round 3 start`, or
  `{"label":"round 3 start"}`), timestamped on arrival, exactly like F9. A
  console stdin is left alone. The control panel now starts capture with
  such a pipe and exposes it as `POST /api/components/capture/marker`
  `{ "label": "…" }` (guard header required; 400 for a blank, multi-line or
  over-long label, 409 when capture is not running); the marker is also
  echoed into the component's log. `/api/state` components carry a
  `markers` flag.
- **`telemouse-analyze report` takes a session id.** `report s-2026…`
  looks the id up in `--dir` (default `recordings`) so a caller that only
  knows the id from `list` or the panel need not know the directory; a path
  still works. New `--summary` prints the headline numbers as ~3 KB of JSON
  (`telemouse-report-summary/1`: session, data quality incl. sidecar exit
  and per-sink losses, flicks, micro-control, clicks, kinematics, lifts,
  warnings) instead of the terminal rendering — meant for scripts and
  agents that should not swallow the full report.
- **Sidecar in the viz session list.** `GET /api/sessions` entries carry a
  `sidecar` field with the parsed `<id>.meta.json` (exit reason, counters,
  per-sink losses), or `null` when there is none.
- **Agent-facing docs.** `docs/API.md` describes every machine interface
  (ctl and viz HTTP, WebSocket, the analyzer's JSON, the files on disk) in
  one place; `docs/AGENTIC-2026-09-13.md` is the plan for plugging telemouse
  into agents and its status; `CLAUDE.md` and a `/telemouse` Claude Code
  skill live in the repo for working on and with telemouse from an agent.
  Both release zips now carry `docs/FAIR-PLAY.md` and `docs/API.md` next to
  the guide, so the README's links resolve from the download.
- **Viz timing tiles.** Renamed the former latency readout to **event age**;
  it naturally increases while the mouse is idle. A separate **latency**
  tile measures capture-to-browser delivery on batch arrival, averaging
  event delays within each batch and smoothing across batches. It shows
  **idle** after 3 seconds without a sample. Both are available in the OBS
  HUD as `eventage` and `latency`.

## [0.1.2] — 2026-09-12

Field-audit release: what the tools tell you when something goes wrong, a
few numbers that were degenerate or inflated, and the last mile to a
stranger's PC. Logging, Kafka and observability became Cargo features so the
download can be minimal while the developer build keeps everything.

- **Two release flavours.** `telemouse-vX.Y.Z-windows-x86_64.zip` is now the
  minimal build (capture, viz, control panel; no log files, no Kafka, no
  stats reporting compiled in) and `…-full.zip` has the four binaries with
  every feature. Both carry the loopback sample as `telemouse.toml`, the
  license, the user docs, and the demo recording the README points at (it
  was missing from the zip). Internal audit and hand-off notes no longer
  ship. Release assets carry a build-provenance attestation. CI builds and
  tests both flavours and runs a dependency advisory scan.
- **Features.** `logging` (the `tracing` subscriber: stderr, colour only on
  a terminal and `NO_COLOR` honoured, plus size-rotated `logs/<component>.log`),
  `observability` (the 5-second stats lines, latency histograms, session
  sidecars, `/api/stats`, the feed/stall fields of `/healthz`, child health on
  the panel), `kafka` (capture's sink) and `quiet` (compiles `tracing` calls
  out). All on by default. `CONVENTIONS.md` says where new code goes.
- **License and metadata.** MIT `LICENSE`; `license`, `repository`,
  `description` and `rust-version` on every crate.
- **Static C runtime.** `.cargo/config.toml` links the MSVC CRT statically,
  so the executables no longer import `VCRUNTIME140.dll` and run on a
  Windows install without the Visual C++ Redistributable.
- **Loss is loud.** Capture repeats a warning once a minute while a sink is
  dead or dropping, on ring overflow, and when no input arrives for a
  minute (with a note when it resumes). The panel parses the agent's stats
  line into the capture card, the tray tooltip and `/api/state`, turns the
  tray icon amber while something is wrong, and shows the recording's size
  and the disk's free space.
- **The sidecar survives a crash.** `recordings/<session>.meta.json` is
  written on the first stats tick with `"exit":"running"` and rewritten
  atomically every 5 seconds, so a session that ended in a kill, a reboot
  or a shutdown is recognisable afterwards. New fields: `capture_profile`,
  `qpc_freq`, `anchor_uncertainty_us`, `max_anchor_drift_us`, `window_ms`,
  `coalesce_ms`, `poll_hz`. `telemouse-analyze report` reads it and flags an
  unfinished run and an event-count mismatch; `list` gains `EXIT` and `BAD`
  columns.
- **Shutdown, logoff and console close stop everything gracefully.** Capture
  and the panel register a console control handler that holds the terminal
  event until the recording is flushed and the sidecar written; the tray
  window answers `WM_QUERYENDSESSION` with a shutdown-block reason while the
  children are stopped with a 3-second grace. Every exit path logs its
  reason.
- **Latency percentiles mean something.** Capture and viz share one 250 µs
  histogram in `telemouse-core` (the log2 one printed a constant 32767 at
  the default window), capture records after the send, an overflow prints
  as `>=…`. Capture also reports the mouse's observed polling rate
  (`poll_hz`, `drains_per_s`), UDP `WouldBlock` drops, a mouse plugged in
  mid-session (as a marker), and re-sends the session envelope over UDP
  every 5 seconds so a dashboard opened after capture is calibrated. T1's
  timer wait is finite and its read errors are counted; a hung shipping or
  context thread ends the run as `*-thread-stalled` instead of writing
  mislabelled batches.
- **Analyzer correctness.** On a grid-truncated session the rate numerators
  used the whole recording while the denominator was the analyzed span,
  inflating `events_per_s`, `distance_*_per_min` and `clicks_per_min` by up
  to 3.3× on day-long recordings; the grid cap now counts stored cells, not
  span cells. New: polling-rate estimate and stability, cm/360 in the header
  and `trend`, load timing and progress, bad-line locations, and a note
  (not a warning) when there is under a second of data.
- **Viz.** A dashboard pill that says *waiting for capture on udp …* and
  *no data for N s* instead of a green *live* with nothing behind it; the
  OBS overlay dims and shows *no feed* after `stale_secs` (`[viz.obs]`,
  `?stale=` on the URL, default 3 s, 0 = never) and skips drawing while its
  source is hidden; the two 403 pages explain the remedy (an IP literal, or
  `/obs`); the bridge counts `seq_no` gaps, bytes, queue depth and
  inter-arrival jitter, logs WebSocket peers, and `/healthz` reports
  `feed: live|stalled|never`.
- **Config.** `telemouse.toml` is looked for in the working directory, then
  next to the executable, and relative paths in it resolve against the
  file's directory (a shortcut's working directory no longer scatters
  `logs/` and `recordings/`). viz and the panel refuse a file that does not
  parse instead of silently running on defaults. Game keys must be lowercase
  `.exe` names. Validation errors name the file; a UTF-16 file gets a hint
  to save as UTF-8; `window_ms` and `ring_capacity` have ceilings. Every
  binary logs its version, profile, features and config on start.
- **Control panel.** A taken port is reported (and the running panel
  opened) instead of a silent exit; the status window shows the panel URL,
  config path, log folder and version; tray items open the logs and
  recordings folders, the config file and the docs; the page shows the
  version, a releases link and the absolute paths; a hotkey another program
  owns is shown as *not registered*; config edits are picked up; more exit
  hints (port in use, access denied, invalid config); per-monitor DPI
  awareness; process-table walks no longer repeat for elevated processes.
- **Zero-edit first run.** When `telemouse-ctl` finds no `telemouse.toml` it
  writes the shipped sample there (loopback only, Kafka off) before reading
  it, so a bare `telemouse-ctl.exe` — the release zip's exes copied
  anywhere, or a `cargo install` — comes up on the first double-click. An
  existing file is never touched; tests pin the sample to the compiled
  defaults and to loopback.
- **Repository.** Notebook outputs stripped (they published app usage,
  session times and LAN addresses), `tools/README.md`, `.gitattributes`,
  the IDE folder untracked, README rewritten for the two flavours with
  unblock, checksum, install, what-it-writes and reporting sections.

## [0.1.1] — 2026-09-09

Hardening release from the September production-readiness audit: the
shipped config is loopback-only, the toolchain is pinned, the viz serves
only the overlay to other machines, and every run leaves its loss counters
next to the recording. Windows only, as before; recordings and the wire
format are unchanged and every older recording still loads.

- **Release config split.** `telemouse.example.toml` (loopback everywhere,
  Kafka off) is what the release zip now ships as `telemouse.toml`; the
  repository's `telemouse.toml` is the development machine's own config and
  no longer reaches a release with its LAN bind and broker addresses.
- **Network mode is overlay-only.** When the viz is bound off loopback, a peer
  that is not this machine is served `/obs`, `/ws` and `/healthz`; the
  dashboard, the recording list and the recordings answer 403. Live
  WebSocket clients are capped at 16, pinged every 20 s, and the sessions
  listing is reused for 5 s, so a device on the LAN can no longer pin the
  disk or a core by looping a request. Both servers add `X-Frame-Options:
  DENY`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.
- **Session metadata sidecar.** The agent writes
  `recordings/<session>.meta.json` when it stops — why it stopped, whether
  every thread joined cleanly, event/drop totals, and per-sink errors,
  drops and abandoned envelopes. `telemouse-analyze list` shows a `LOSS`
  column and the JSON listing carries `losses` and `exit`, so a Kafka outage
  that dropped batches is visible afterwards without reconciling the JSONL.
- **Config validation.** Every `kafka.brokers` entry must be `host:port`
  (a port-less entry was silently unreachable). `ctl.stop_grace_secs`
  defaults to 8 s, above the agent's two 3 s sink drains, so a slow disk or
  broker at stop time delays the stop instead of truncating the recording.
- **Recording names follow one rule.** `telemouse_core::recordings::is_safe_id`
  (`[A-Za-z0-9_-]`) is now applied by the panel and the analyzer as well as
  the viz; the panel's separator check alone let a drive-relative
  `C:x.jsonl` resolve outside the recordings directory on Windows.
- **Failures are louder.** Every binary installs a `tracing` panic hook; the
  capture context thread has an alive guard (a panic there used to freeze the
  game name for the rest of the session); viz `/healthz` answers 503 with
  `udp_bound: false` while its listener is down and reports the seconds
  since the last datagram; the panel explains an `unknown field` exit as a
  binary older than the config and rotates component logs by size while it
  runs, not only at startup.
- **Toolchain and CI.** `rust-toolchain.toml` pins 1.98.0 for CI and
  developers; clippy and fmt gate the release job too; every cargo step runs
  `--locked`; the viz page's script is now `crates/viz/src/app.js`, inlined
  at startup, syntax-checked with `node --check` in CI and in the test
  suite, and unit-tested with Node against a stub DOM.
- The raw-input buffer walk rounds each block up to the pointer size (the
  `NEXTRAWINPUTBLOCK` rule) and bounds every block by the buffer.
- Control panel: a system-wide new-session hotkey, `[ctl] hotkey` (default
  `ctrl+alt+r`), stops the capture agent if it is running and starts one that
  saves — a fresh recording without leaving the game. The tray menu gains the
  same *New session* item and shows the chord; a balloon confirms each press.
- Performance/latency follow-up: 256 KiB replay streaming and direct recording
  lookup make a 532 MB replay 7.9x faster; the live browser trims typed arrays
  in chunks; capture/browser defaults are now 25/35 ms for a roughly 35 ms
  live-display floor.
- Analyzer: persistent change-sensitive recording metadata index, one timestamp
  vector instead of two, bounded interval searches, and dependency-aware
  scoped parallelism. On the 8-million-event audit recording, warm listing is
  4,972 → 17–23 ms, per-minute aggregation is 83.7 → 42.5 ms, and report build
  is 1,390 → 846 ms median with a 6.9% peak-memory increase.
- Capture isolation: JSONL writes/one-second flushes and Kafka initialization
  run on bounded workers. Slow storage and the five-second broker connection
  timeout no longer stall the shipping loop or capture startup; queue/drop/
  abandoned-work telemetry makes degradation explicit. Flush-confirmed JSONL
  accounting and Kafka terminal-failure handling avoid silent loss counters
  and repeated per-batch error allocation during an outage.
- Viz aim panel: yaw is drawn unwrapped. Crossing ±180° no longer teleports
  the head to the far edge (which made the camera pan and the zoom balloon);
  the seam is a dashed line at every odd multiple of 180° and the origin axis
  repeats every 360°.
- Local Kafka broker: `compose.yaml` runs a single-node KRaft Apache Kafka
  3.9 on `127.0.0.1:9092` with persistent data; `[kafka] enabled` is now
  `true` in the repo `telemouse.toml`. Capture still degrades to UDP +
  JSONL with a warning when the broker is down.
- OBS overlay from another PC: the repo `telemouse.toml` now binds the viz
  to `0.0.0.0:7879` so a streaming PC on the LAN can use
  `http://<gaming PC IP>:7879/obs` as its Browser source; README and GUIDE
  document the firewall rule and the IP-literal requirement. The control
  panel's *viz* link and the viz's own startup line print a browsable
  loopback URL when the bind is unspecified (`0.0.0.0` / `[::]`).

## [0.1.0] — 2026-08-30

First release: the whole pipeline, Windows only.

- **telemouse** — raw-input mouse capture at device rate (cadence-paced
  drains, ~0.4% of a core at 1 kHz), batched to localhost UDP, a JSONL
  recording, and optionally Kafka. Per-device tracking, horizontal wheel,
  pointer-lock and foreground-app context, QPC/UTC anchor with drift checks,
  config hot-reload with stream markers.
- **telemouse-viz** — UDP→WebSocket bridge and a single-file dashboard with
  live view, replay with scrubbing, and an OBS browser-source mode (`/obs`).
- **telemouse-analyze** — offline metrics (kinematics, flicks, clicks,
  micro-corrections, tremor, lifts, per-second/per-minute tables, quality
  checks), `report` / `trend` / `list`, JSON + CSV output, criterion benches.
- **telemouse-ctl** — control panel on `127.0.0.1:7880` with a native tray
  icon and status window: start/stop the other binaries, run the tools,
  see and kill telemouse processes, a per-run save-data switch, logs under
  `logs/`.
- Security model for the local servers: both refuse DNS-rebound `Host`
  names, viz refuses foreign `Origin`s on `/ws`, ctl requires
  `X-Telemouse-Ctl: 1` on every mutating call, the tray ignores posted
  `WM_COMMAND`s. No authentication beyond that — keep both on loopback.
- 407 tests, clippy clean, rustfmt at defaults, CI on every push.

See `docs/AUDIT-2026-08.md` for the performance and audit history behind
this release and `docs/GUIDE.md` for the full reference.
