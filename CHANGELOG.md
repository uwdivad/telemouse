# Changelog

Releases are cut by pushing a `vX.Y.Z` tag that matches `[workspace.package]
version` in `Cargo.toml`; the `release` workflow builds, tests, packages and
publishes the section below that names that version.

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
