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
| `telemouse` | `crates/capture` | Capture agent: raw input → 25ms batches → UDP + JSONL recording + optional Kafka |
| `telemouse-viz` | `crates/viz` | Live browser visualization + session replay (UDP→WebSocket bridge, single-file page) |
| `telemouse-analyze` | `crates/analyze` | Offline metrics over recorded sessions |
| — | `crates/core` | Shared types, wire format, config, QPC↔UTC clock math |

## Quick start

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
```

Everything works with zero external services: UDP live-viz and JSONL recording
are always on; Kafka is optional and off by default. A bundled demo recording
(`recordings/demo-session.jsonl`) lets you try replay and analysis immediately.

## Configuration

[telemouse.toml](telemouse.toml) — all fields optional:

```toml
mouse_cpi = 1600.0            # your mouse's real CPI/DPI → physical cm

[udp]
addr = "127.0.0.1:7878"       # capture → viz live path

[kafka]
enabled = false               # durable log; capture degrades gracefully without it
brokers = ["127.0.0.1:9092"]

[recording]
dir = "recordings"            # per-session JSONL files

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
             never blocks)                                         ├─▶ recordings/<session>.jsonl
T3 context (250ms): foreground game,                               └─▶ Kafka mouse.events / mouse.sessions /
pointer-lock heuristic, cursor, screen                                       mouse.markers   (optional)
```

- **T1** never allocates or blocks after startup; if the ring ever fills, events
  are counted as drops (a visible data-quality metric), never a capture stall.
- Each session opens with a `session` record: QPC frequency, QPC↔UTC anchor,
  CPI, sens table, monitor setup — everything needed to reconstruct physical
  units later.
- Useful flags: `--print` (per-batch log line), `--no-kafka` / `--no-udp` /
  `--no-record`, `--duration-secs N` (smoke tests).

## Live viz & replay

`telemouse-viz` serves one self-contained page (no CDN, works offline):

- **Desk-space panel** — your hand's path in real cm, velocity-colored trail
  with time decay.
- **Aim-space panel** — crosshair path in degrees (yaw wrapped at ±180°,
  pitch clamped), using the sens profile of whatever game is foreground.
- Click rings per button, wheel ticks, marker toasts, live readouts (cm/s,
  °/s, session distance, clicks/min, events/s, ring drops).
- **Replay** — pick any recorded session: play/pause, scrub (exact re-integration),
  0.25×–8× speed, jump-to-marker. Live and replay share the same engine;
  live is just replay at 1× of "now". Keys: `R` recenter, `Space` pause,
  `←/→` seek.

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

## Kafka topics (optional durable log)

`mouse.events` (batches, keyed by session id), `mouse.sessions` (compacted
session configs), `mouse.markers` (hotkey/game-state annotations). JSON
envelopes today; the tagged wire format leaves a seam for a binary schema.

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
cargo test --workspace              # 318 tests; no mouse, admin, Kafka, or browser needed
cargo bench -p telemouse-analyze    # criterion benches over the hot math
cargo build --profile profiling     # release speed + debug symbols for flamegraphs
```

Append `?profile=1` to the viz URL for an in-page frame-time breakdown.
The August 2026 performance/observability audit and its resolutions are
documented in [docs/AUDIT-2026-08.md](docs/AUDIT-2026-08.md).

Win32 code is isolated behind `#[cfg(windows)]`; all metric math, batching,
clock mapping and wire logic is pure and unit-tested (flick detection is tested
against synthetic streams with known ground truth). Workspace conventions:
[docs/CONVENTIONS.md](docs/CONVENTIONS.md).
