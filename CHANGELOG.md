# Changelog

Releases are cut by pushing a `vX.Y.Z` tag that matches `[workspace.package]
version` in `Cargo.toml`; the `release` workflow builds, tests, packages and
publishes the section below that names that version.

## [Unreleased]

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
