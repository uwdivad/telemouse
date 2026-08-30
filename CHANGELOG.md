# Changelog

Releases are cut by pushing a `vX.Y.Z` tag that matches `[workspace.package]
version` in `Cargo.toml`; the `release` workflow builds, tests, packages and
publishes the section below that names that version.

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
