# telemouse — the from-zero guide

This document explains the whole repository as if you had never seen it: what
the program is for, how the pieces fit, what every crate does internally,
where the important numbers come from, and how to build, run, test, and
extend it. It is written from a full read of the code as of 2026-09-04
(v0.1.0 plus the unreleased changes listed at the top of `CHANGELOG.md`) and
cites `file:line` so you can jump straight to the source.

If you only read one section, read **§2 (the 10-minute mental model)**.

---

## Contents

1. [What telemouse is](#1-what-telemouse-is)
2. [The 10-minute mental model](#2-the-10-minute-mental-model)
3. [Repository layout](#3-repository-layout)
4. [Building, running, testing](#4-building-running-testing)
5. [`crates/core` — the shared contract](#5-cratescore--the-shared-contract)
6. [`crates/capture` — the capture agent (`telemouse`)](#6-cratescapture--the-capture-agent-telemouse)
7. [`crates/viz` — live viz, replay, OBS overlay (`telemouse-viz`)](#7-cratesviz--live-viz-replay-obs-overlay-telemouse-viz)
8. [`crates/analyze` — offline metrics (`telemouse-analyze`)](#8-cratesanalyze--offline-metrics-telemouse-analyze)
9. [Configuration reference (`telemouse.toml`)](#9-configuration-reference-telemousetoml)
10. [The wire format, line by line](#10-the-wire-format-line-by-line)
11. [Time: QPC, anchors, and why timestamps are trustworthy](#11-time-qpc-anchors-and-why-timestamps-are-trustworthy)
12. [Units: counts → cm → degrees](#12-units-counts--cm--degrees)
13. [Observability: what the logs tell you](#13-observability-what-the-logs-tell-you)
14. [Performance design decisions](#14-performance-design-decisions)
15. [Testing philosophy](#15-testing-philosophy)
16. [How to extend it (recipes)](#16-how-to-extend-it-recipes)
17. [Known rough edges](#17-known-rough-edges)
18. [Glossary](#18-glossary)
19. [`crates/ctl` — the control panel (`telemouse-ctl`)](#19-cratesctl--the-control-panel-telemouse-ctl)

---

## 1. What telemouse is

telemouse records **what your physical mouse actually did** while you play a
game, at the resolution the mouse itself reports (1 report per millisecond for
a 1 kHz mouse), and turns that into:

- a **live visualization** in a browser (your hand's path on the desk in real
  centimetres, and your crosshair's path in degrees),
- a **recording** of every session on disk that can be **replayed** later,
- an **analysis report** of aim metrics (flick overshoot, settle time, tremor,
  trigger discipline, fatigue over the session…),
- optionally a **Kafka** stream for a durable long-term dataset.

Two design commitments shape everything:

**It is passive.** Input comes from the Windows *Raw Input* API. A hidden
window registers with `RIDEV_INPUTSINK`, which asks Windows to deliver a
*copy* of every HID mouse report to it, even while a game is in the
foreground. Nothing is injected into the game and no input is ever
synthesized; the only thing read about the game is the foreground window's
process name, taken from a process-table snapshot so no handle is ever
opened on the game. There is no overlay. That is everything anti-cheat
systems are documented to act on, and telemouse does none of it — but no
publisher endorses third-party tools, so the claim stops there. The exact
Win32 surface is in [FAIR-PLAY.md](FAIR-PLAY.md); the audit behind it in
[ANTICHEAT-2026-09-14.md](ANTICHEAT-2026-09-14.md).

**Raw counts stay raw on the wire.** The mouse reports integer "counts" (one
count = 1/CPI inch). telemouse never converts these before writing them down.
Centimetres and degrees are *derived by consumers* from a per-session config
record (CPI, per-game sensitivity). So if you discover your CPI was configured
wrong, you fix the config and re-analyze; the recording is still correct.

The original design document is [`mouse-telemetry-plan.md`](https://github.com/uwdivad/telemouse/blob/master/mouse-telemetry-plan.md) (in the repository, not in the release zip).
The code implements phases 1–5 of that plan (capture, Kafka topics, live viz,
replay, analysis), with the exception that "storage" is JSONL files instead of
TimescaleDB/Parquet.

---

## 2. The 10-minute mental model

There are five crates in one Cargo workspace — four binaries over one shared
library:

```
                    ┌─────────────────────────────────────────────┐
                    │  crates/core  (library: telemouse-core)     │
                    │  RawEvent, Batch, SessionConfig, Marker,    │
                    │  Envelope (wire), QpcAnchor, units, config, │
                    │  localhost (Host/Origin guard)              │
                    └────────┬──────────────┬───────────────┬─────┘
                             │              │               │
        ┌────────────────────▼───┐   ┌──────▼─────────┐  ┌──▼────────────────────┐
        │ crates/capture         │   │ crates/viz     │  │ crates/analyze        │
        │ bin: telemouse         │   │ bin:           │  │ bin: telemouse-analyze│
        │ Win32 raw input →      │   │ telemouse-viz  │  │ JSONL → 1ms grid →    │
        │ batches → UDP/JSONL/   │   │ UDP→WebSocket  │  │ metrics report        │
        │ Kafka                  │   │ + browser page │  │                       │
        └───────┬────────────────┘   └──────▲─────────┘  └──────────▲────────────┘
                │ UDP 127.0.0.1:7878        │                        │
                └───────────────────────────┘                        │
                │ recordings/<session>.jsonl ─────────────────────────┘
                │                              (also served by viz for replay)
                └─▶ Kafka (optional; compose.yaml runs a local broker)

        ┌────────────────────────────────────────────────────────────────────┐
        │ crates/ctl  bin: telemouse-ctl — control panel on 127.0.0.1:7880   │
        │ starts/stops the three binaries above; native window (WebView2)   │
        │ showing the panel page, tray icon, text status view as fallback   │
        └────────────────────────────────────────────────────────────────────┘
```

The data model has exactly **three record types**, and every transport (UDP
datagram, JSONL line, Kafka message) carries one JSON-encoded record per unit:

| Record | When | Contains |
|---|---|---|
| `session` | once, at start | session id, QPC frequency, QPC↔UTC anchor, mouse CPI, per-game sens table, monitors, device list |
| `batch` | every ~25 ms while the mouse moves (`batch.window_ms`) | up to 448 `RawEvent`s (`ts_qpc, dx, dy, buttons, wheel, wheel_h, device_ix`) plus context (game, pointer-locked, cursor, drop counters, seq_no) |
| `marker` | on the marker hotkey (F9 by default) / a line on a piped stdin / config change / clock drift | a labelled timestamp |

The capture agent runs **three threads**:

- **T1 (hot path)** owns the hidden window. While the mouse is still it
  blocks on the input queue; the first report of a burst wakes it, and from
  then on a high-resolution timer paces one drain every
  `batch.coalesce_ms + 1` ms (9 ms by default) until a drain comes back empty.
  Each drain reads every pending report in one syscall, stamps them with
  `QueryPerformanceCounter`, and pushes fixed-size structs into a lock-free
  single-producer/single-consumer ring buffer. It never allocates, never
  blocks, never touches the network. If the ring is full it drops and counts.
- **T2 (shipper)** drains the ring, groups events into 25 ms batches (`batch.window_ms`), serializes
  each batch to JSON *once*, sends UDP inline, and hands JSONL/Kafka owned jobs
  to bounded workers.
- **T3 (context)** ticks every 250 ms and asks Windows which process is in the
  foreground, where the cursor is, and whether the cursor is "frozen while
  deltas flow" (the pointer-lock heuristic = you are in a game with raw input).
  It also re-checks the clock anchor every 60 s and watches `telemouse.toml` for
  edits.

The viz is a tiny bridge (UDP in → WebSocket out) plus a single self-contained
HTML page. The page has one **engine** that integrates deltas into positions;
"live" is just "replay at 1× of now", so the same code draws both.

The analyzer loads a JSONL file, converts the irregular event stream into a
**sparse 1 ms grid** of velocities, smooths it with Savitzky–Golay, and runs a
set of detectors (flicks, corrections, tremor, clicks, lifts) over the grid.

That is the whole system. Everything below is detail.

---

## 3. Repository layout

```
telemouse/
├── Cargo.toml                 workspace: 5 members, shared deps, release/profiling profiles
├── telemouse.toml             the one config file (every binary reads it)
├── compose.yaml               local single-node Kafka broker (docker compose up -d)
├── CHANGELOG.md               release notes; the release workflow publishes the matching section
├── README.md                  user-facing: download, run, first session, OBS, troubleshooting
├── mouse-telemetry-plan.md    the original design/plan document (repository only)
├── .github/workflows/
│   ├── ci.yml                 fmt --check, clippy -D warnings, tests on windows-latest, every push
│   └── release.yml            on a vX.Y.Z tag: build, test, zip the binaries, GitHub Release
├── docs/
│   ├── DEVELOPING.md          building from source, flavours, Kafka, benches, releasing
│   ├── CONVENTIONS.md         workspace rules (edition 2024, tracing, no panics on degraded env…)
│   ├── AUDIT-2026-08.md       Aug-2026 perf/observability audit and every pass since, with leftovers
│   ├── BENCHMARKS.md          criterion numbers, the live-stack CPU table, what is left on the table
│   ├── PERFORMANCE-2026-09.md implemented follow-up with reproducible before/after measurements
│   ├── HANDOFF-2026-09-03-kafka.md  standing up the Kafka broker on another machine
│   └── GUIDE.md               this file
├── recordings/                per-session JSONL files; demo-session.jsonl is bundled
├── tools/cpubench/            `tmbench` (not a workspace member) + bench.ps1: cycle-exact CPU harness
└── crates/
    ├── core/      src/{lib,event,batch,batcher,wire,clock,units,session,config,localhost}.rs
    │              benches/wire.rs
    ├── capture/   src/{main,raw_input,shipping,context_thread,context,session_setup,
    │                   stats,platform,devices,pointer_lock,shutdown,sinks}.rs
    │              src/sinks/{udp,jsonl,kafka}.rs
    ├── viz/       src/{main,udp,hub,server,recordings,shutdown,stats}.rs
    │              + src/index.html
    ├── ctl/       src/{main,procs,manager,server}.rs + src/gui/{mod,feed,model,win}.rs
    │              + src/index.html
    └── analyze/   src/{lib,main,load,series,savgol,kinematics,flicks,micro,clicks,
                        quality,lifts,markers,per_second,per_minute,stats,timefmt,
                        trend,report,testutil}.rs
                   tests/report_pipeline.rs   benches/hot_math.rs
```

Sizes, roughly: core ~2k lines, capture ~5.6k, viz ~2.3k Rust + ~2.5k HTML/JS,
ctl ~4.4k Rust + ~0.3k HTML, analyze ~8.9k. `.idea/`, `.playwright-mcp/`,
`logs/` and `target-bench*/` are git-ignored IDE, browser-automation, panel-log
and benchmark leftovers, not part of the build.

Workspace-level facts (`Cargo.toml`):

- Edition **2024**, resolver 3.
- `[workspace.package] version` is the release version; `release.yml` refuses
  a `vX.Y.Z` tag that does not match it.
- Shared deps: `serde`, `serde_json`, `thiserror`, `anyhow`, `toml 0.9`,
  `tracing`, `tracing-subscriber` (env-filter), `clap 4` (derive).
- `[profile.release]`: thin LTO, `codegen-units = 1` (fat LTO / `panic=abort`
  were measured net-negative for this syscall-bound workload, and ordered
  teardown relies on unwinding).
- `[profile.profiling]`: release + debug symbols, for flamegraphs
  (`cargo build --profile profiling`).

---

## 4. Building, running, testing

Requirements: recent stable Rust; Windows for actual capture. Everything
*except* `telemouse run` compiles and runs on any OS (the Win32 code is behind
`#[cfg(windows)]`, and `doctor`, the viz, and the analyzer are portable).

```powershell
# optional: the local Kafka broker (compose.yaml). Capture runs fine without it.
docker compose up -d

# sanity-check the machine: QPC, monitors, devices, UDP bind, Kafka reachability
cargo run -p telemouse-capture -- doctor

# capture (Ctrl-C to stop; the marker hotkey, F9 by default, drops a marker)
cargo run --release -p telemouse-capture -- run --print

# live viz: open http://127.0.0.1:7879  (OBS overlay at /obs)
cargo run --release -p telemouse-viz

# analysis
cargo run -p telemouse-analyze -- list
cargo run -p telemouse-analyze -- report recordings\demo-session.jsonl
cargo run -p telemouse-analyze -- trend --dir recordings

# or drive capture / viz / the tools from one page + tray icon (§19)
cargo build --release --workspace
target\release\telemouse-ctl.exe    # http://127.0.0.1:7880

# tests / benches
cargo test --workspace              # no mouse/admin/Kafka/browser needed (ctl's child-process tests take minutes)
cargo bench -p telemouse-analyze    # criterion over the loader + hot math
cargo bench -p telemouse-core       # wire encode/decode, batcher
```

Logging everywhere is `tracing` with `RUST_LOG` (default `info`), e.g.
`$env:RUST_LOG="debug"; cargo run -p telemouse-capture -- run`. Colour is
emitted only when stderr is a terminal (and `NO_COLOR` is unset); the
subscriber is always set up through `telemouse_core::logging::init`, which
can also write a size-rotated `<log_dir>/<component>.log`.

**Build flavours.** Logging, Kafka and observability are Cargo features,
on by default (the release zip has them) and compiled out of a minimal
build you can make yourself (`docs/DEVELOPING.md`):

| Feature | Crates | Adds |
|---|---|---|
| `logging` | capture, viz, analyze, ctl | the `tracing` subscriber (stderr + rotated log files) |
| `observability` | capture, viz, ctl | 5-second stats lines, latency histograms, `<session>.meta.json` sidecars, `/api/stats`, the feed/stall fields of `/healthz`, child health on the panel |
| `kafka` | capture | the Kafka sink; without it `[kafka] enabled = true` is ignored with a warning |
| `quiet` | capture, viz, ctl | `tracing/release_max_level_off`: every `tracing` call compiled out |

```powershell
cargo build --release --workspace                  # everything (the developer build)
cargo build --release -p telemouse-capture -p telemouse-viz -p telemouse-ctl `
  --no-default-features --features telemouse-capture/quiet,telemouse-viz/quiet,telemouse-ctl/quiet
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --all -- --check`, clippy
with `-D warnings` and the tests for both flavours on `windows-latest` for
every push, plus a dependency advisory scan, so the tree must stay
rustfmt-clean at default settings and must build with `--no-default-features`.
A release is a tag: bump `version` in the root `Cargo.toml`, add a
`## [X.Y.Z]` section to `CHANGELOG.md`, commit, tag `vX.Y.Z`, push the tag;
`release.yml` lints and tests both flavours, builds the default one in
release mode, signs the executables when the signing secrets are configured
(`docs/DEVELOPING.md`, "Code signing") and publishes a GitHub Release with
one zip, `telemouse-vX.Y.Z-windows-x86_64.zip`: all four binaries including
`telemouse-analyze`, with a SHA-256 and a build-provenance attestation. The
release notes are that changelog section. The zip carries the loopback
sample as `telemouse.toml`, the license, this guide, and the demo recording.

A good first hands-on exercise: run `telemouse-viz`, open the page, pick
`demo-session` in the replay dropdown, and scrub. Then run
`telemouse-analyze report recordings\demo-session.jsonl` and match the flicks
in the report to what you saw.

---

## 5. `crates/core` — the shared contract

`telemouse-core` is a pure library: no Win32, no I/O beyond reading the config
file, no async. `CONVENTIONS.md` calls it "the contract — do not change its
public API without coordinating", because all three binaries deserialize each
other's output through it.

### 5.1 `event.rs` — `RawEvent`

```rust
pub struct RawEvent {
    pub ts_qpc: u64,      // QueryPerformanceCounter at receipt
    pub dx: i32,          // raw HID counts, pre-acceleration
    pub dy: i32,          // positive = down (HID convention)
    pub buttons: u16,     // transition bitfield (see below); omitted from JSON when 0
    pub wheel: i16,       // vertical wheel delta (±120 per detent typical); omitted when 0
    pub wheel_h: i16,     // horizontal/tilt wheel; omitted when 0
    pub device_ix: u8,    // index into SessionConfig.devices; 0 = unknown; omitted when 0
}
```

It is `Copy` and fixed-size on purpose: T1 pushes it through the ring buffer
without allocating. The `buttons` bits (`event.rs:8-25`) are **numerically
identical to Win32's `RI_MOUSE_*` transition flags** so the hot path can mask
`RAWMOUSE.usButtonFlags` straight through: `LEFT_DOWN=0x0001, LEFT_UP=0x0002,
RIGHT_DOWN=0x0004, … X2_UP=0x0200`, `MASK=0x03FF`, plus `ANY_DOWN` / `ANY_UP`
helpers. Note these are *transitions*, not state: an event says "left went
down", not "left is held".

`skip_serializing_if` on the rare fields is what got a motion-only event from
~71 to ~45 bytes on the wire; consumers must treat a missing field as 0
(`serde(default)` handles that on the Rust side; the JS page uses `| 0`).

### 5.2 `batch.rs` — `Batch`

The envelope T2 assembles every ~25 ms (`batch.window_ms`):

| field | meaning |
|---|---|
| `session_id` | ties the batch to its `session` record |
| `seq_no` | monotonic per session; a **gap at a consumer means transport loss** (distinct from ring drops) |
| `ts_anchor_us` | UTC µs of the first event, via the session anchor |
| `game` | foreground process name (lowercase), omitted if unknown |
| `pointer_locked` | pointer-lock heuristic result at assembly time |
| `screen_w/h` | primary screen size |
| `cursor_x/y` | sampled cursor, **omitted while locked** (meaningless in a game) |
| `drops_since_last` | ring-buffer drops since previous batch (should be 0) |
| `abs_frames_since_last` | absolute-motion `WM_INPUT` frames discarded (RDP, tablets, virtual devices) |
| `events` | `Vec<RawEvent>`, ≤ `MAX_EVENTS_PER_BATCH` |

`BatchView<'a>` is a serialize-only borrowing mirror of `Batch` (same field
order, same `skip_serializing_if`s, byte-identical JSON — a parity test
enforces it) so T2 can encode a batch straight out of the batcher's `Vec` and
the context snapshot without cloning; `Batch::as_view()` builds one. The
deserialize side stays on the owned `Batch`.

### 5.3 `batcher.rs` — `Batcher`

Pure policy: "flush when full (`max_events`) or when `window_ticks` have
elapsed since the batch's first event". Driven entirely by caller-supplied QPC
values, so it is unit-tested without a clock (`flushes_on_window_elapsed`,
`flushes_on_max_events_regardless_of_time`). `Batcher::with_window_ms(max,
window_ms, qpc_freq)` does the ms→ticks conversion. T2 owns one.

### 5.4 `wire.rs` — `Envelope`

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope { Session(SessionConfig), Batch(Batch), Marker(Marker) }
```

One JSON `Envelope` per UDP datagram / JSONL line / Kafka message. `topic()`
maps to `mouse.sessions` / `mouse.events` / `mouse.markers`; `key()` is always
the session id for stable partition routing. Kafka produces are concurrent for
batching throughput and the three record types use separate topics, so consumers
use `seq_no` rather than assuming visibility order across messages or topics.
`EnvelopeView<'a>` is the borrowing twin for the hot path — only a `Batch`
variant exists, wrapping a `BatchView`, because session and marker envelopes
are rare enough to serialize from the owned type.

Two constants matter: `MAX_UDP_PAYLOAD = 60_000` bytes and
`MAX_EVENTS_PER_BATCH = 448`. The test `full_batch_fits_in_udp_datagram`
serializes a worst-case batch (every field at its widest value) and asserts it
fits; this is why the cap is 448 and not a round number. At the default 25 ms
window the cap only binds above ~18 kHz polling (~9 kHz at 50 ms).

### 5.5 `clock.rs` — `QpcAnchor`

```rust
pub struct QpcAnchor { pub qpc: u64, pub utc_us: i64, pub qpc_freq: u64 }
```

`QueryPerformanceCounter` is monotonic and microsecond-quality but has an
arbitrary zero. One anchor per session (a QPC reading paired with a UTC
reading at "the same instant") lets any consumer map QPC → UTC:
`utc_us = anchor.utc_us + (qpc - anchor.qpc) * 1e6 / qpc_freq`, truncating
toward zero. The analyzer runs this once per event, so it is done in 64-bit
integers — the tick delta as an unsigned magnitude plus a sign, and at the
ubiquitous 10 MHz a divide by ten that compiles to a multiply — with the
`i128` form kept as a bit-identical fallback for the frequencies and tick
spans where 64 bits could overflow. `ticks_to_us` and `ms_to_ticks` are the
other two helpers. See §11 for how the anchor is measured.

### 5.6 `units.rs`

```rust
counts_to_cm(counts, cpi)        = counts / cpi * 2.54
counts_to_yaw_deg(counts, g)     = counts * g.sens * g.yaw_coeff
counts_to_pitch_deg(counts, g)   = counts * g.sens * g.pitch_coeff
wrap_yaw_deg(deg)                → (-180, 180]
```

That is the entire physics. See §12.

### 5.7 `session.rs` — `SessionConfig`, `GameSens`, `MonitorInfo`, `Marker`

`SessionConfig` is the first line of every recording and the `mouse.sessions`
record: `session_id, started_utc_us, qpc_freq, anchor, anchor_uncertainty_us,
mouse_cpi, devices: Vec<String>, games: BTreeMap<String, GameSens>, monitors,
capture_version, coalesce_ms`. `sens_for(process)` is a case-insensitive
lookup. `GameSens { sens, yaw_coeff, pitch_coeff }` defaults both coefficients
to 0.022 (Source engine) when absent.

Several fields are `#[serde(default)]` specifically so **old recordings keep
loading** (`pre_coalescing_session_json_still_parses`,
`pre_device_tracking_json_still_parses`). Backward compatibility of the file
format is treated as a hard requirement throughout.

`Marker { session_id, seq_no, ts_qpc, ts_utc_us, label }` has its own
sequence counter, separate from batches.

### 5.8 `config.rs` — `AppConfig`

Mirrors `telemouse.toml` (see §9). Two deliberate strictness choices from the
audit:

- Every struct is `#[serde(default, deny_unknown_fields)]`. A typo like
  `mouse_dpi` is a **parse error**, not a silently-ignored key, because at the
  default CPI every cm metric would be quietly wrong
  (`typoed_key_is_rejected_not_ignored`).
- `AppConfig::validate()` runs on load and rejects values that would corrupt
  metrics or destabilize the pipeline: non-positive CPI/sens/coeffs,
  `window_ms == 0`, `max_events` outside `1..=448`, `ring_capacity <
  max_events`, `coalesce_ms > MAX_COALESCE_MS (10)`, Kafka enabled with no
  brokers, `ctl.stop_grace_secs > MAX_STOP_GRACE_SECS (60)`, and every
  `[viz.obs]` field against its allowed vocabulary
  (`OBS_LAYOUTS`, `OBS_HUD_ITEMS`, `OBS_HUD_POSITIONS`, ranges for scale /
  trail / buffer).

`load_or_default(path)` returns defaults if the file is absent, so every
binary runs with zero configuration.

### 5.9 `localhost.rs` — the local-server trust rule

Shared by viz and ctl so both apply the same rule. `host_is_trusted(host)`
accepts a `Host` header (with or without port) only when it is an IP literal,
`localhost`, or `*.localhost` — a name an attacker's DNS could point at this
machine is refused, which is the DNS-rebinding guard behind every `403 host
not allowed`. `origin_is_trusted(origin)` applies the same rule to an `Origin`
URL (viz uses it on `/ws`). `browse_addr` / `browse_addr_str` turn a wildcard
bind (`0.0.0.0` / `[::]`) into the loopback of the same family for anything
that prints or links a URL, since browsers refuse `http://0.0.0.0/`; every
other address, and anything unparseable, is returned unchanged.
`SECURITY_HEADERS` is the list both servers stamp on every response
(`X-Frame-Options: DENY`, `nosniff`, `no-referrer`).

The viz adds one more rule of its own (`server::LAN_ROUTES`): when it is
bound off loopback, a peer that is not this machine is served only `/obs`,
`/ws` and `/healthz`; the dashboard, `/api/sessions` and `/api/session/{id}`
answer `403 not served to the network`. The peer address comes from
`ConnectInfo<server::Peer>`, attached by the `NoDelayListener`. It also caps
live WebSocket clients at `MAX_WS_CLIENTS` (16, 503 past it), pings each one
every 20 s so a vanished peer is dropped, and reuses a `/api/sessions`
listing for 5 s.

Recording *names* obey one rule everywhere — `telemouse_core::recordings::is_safe_id`
(`[A-Za-z0-9_-]`, ≤ 128 chars) — used by the viz to serve, the panel to
launch the analyzer, and the analyzer to list. That alphabet is what makes
`dir.join("{id}.jsonl")` a direct child of the directory on every platform;
a separator check alone let a drive-relative `C:x.jsonl` through on Windows.

---

## 6. `crates/capture` — the capture agent (`telemouse`)

### 6.1 CLI (`main.rs:33-84`)

- `telemouse run [--config telemouse.toml] [--print] [--no-kafka] [--no-udp]
  [--no-record | --record] [--duration-secs N]`
  `--print` logs one line per batch; the `--no-*` flags force the
  corresponding `enabled` false and `--record` forces `recording.enabled`
  true (the two recording flags conflict; the control panel's save-data
  switch is implemented with them); `--duration-secs` auto-stops (smoke
  tests).
- `telemouse doctor [--config …] [--json]` reports what the agent sees, as
  a row per check: build and features, OS, config loaded?, non-default
  settings, QPC frequency and resolution, screens + refresh, cursor,
  foreground process, enumerated mice, UDP bind test, recording dir
  writable?, a TCP probe of each Kafka broker (500 ms), then the resolved
  config. Works on non-Windows.

  `doctor.rs` is split so that only the *looking* touches the machine:
  `probe()` gathers `Facts` (Win32 through `platform`, one UDP socket,
  one `create_dir_all`, a connect per broker) and everything after it is
  pure. `Facts::into_report()` judges each fact into a `Check`
  (`id`/`status`/`title`/`detail`/`hint`), the verdict is the worst status,
  and `Report::render_text()` and serde draw the same rows — text mode and
  `--json` cannot drift, and the whole judging half is unit-tested on a
  machine with no mouse. `--json` prints one `telemouse-doctor/1` document
  and nothing else on stdout (logs are on stderr); the exit code means the
  same in both modes — `0` once a report exists, `fail` rows included,
  non-zero only when the config could not be read at all. Bump `SCHEMA`
  when a field changes meaning, and keep `docs/API.md`'s id list in step.

### 6.2 Startup, step by step (`cmd_run`, `main.rs:172-452`)

1. Load `file_cfg` from disk and `cfg` = file_cfg + CLI overrides. Both are
   kept: T3 diffs later reloads against the *file* version.
2. `platform::disable_power_throttling()` — opt out of Windows EcoQoS so
   P/E-core parking can't starve capture.
3. Read `qpc_freq`; take a **sandwich anchor** (`measure_anchor`: QPC, UTC,
   QPC; anchor at the midpoint, record the half-width as
   `anchor_uncertainty_us`); derive the session id `s-YYYYMMDD-HHMMSS-xxxx`.
4. `devices::enumerate_mice()` → `DeviceTable` (names go into the session
   record; T1 maps raw-input device handles onto the same indices).
5. Build `SessionConfig`.
6. Create `Arc<Stats>` and the sinks in order **UDP, JSONL, Kafka**. Any
   constructor failure is a `warn!` and that sink is skipped — never a crash
   (`main.rs:240-270`).
7. Create the `rtrb::RingBuffer<RawEvent>` (`ring_capacity` slots, default
   65 536), the `mpsc` marker channel, the `SharedContext`, and the
   coordination objects: `Shutdown` (flag + condvar), `capture_stopped`,
   `capture_alive`, `CaptureHandles` (atomic HWND / thread id), `RingWaker`.
8. Install the Ctrl-C handler → `shutdown.set()`.
9. Spawn **T1** `"telemouse-capture"`. Its wrapper clears `capture_alive` and
   notifies `shutdown` on *any* exit, so a dead capture thread wakes main
   immediately instead of leaving a zombie agent.
10. Spawn **T3** `"telemouse-context"`, then **T2** `"telemouse-shipping"`.
11. Main sleeps on the shutdown condvar (with the `--duration-secs` deadline
    if given), waking on Ctrl-C / Ctrl-Break (the `ctrlc` crate treats both
    the same, which is what the control panel's graceful stop relies on),
    deadline, `capture_alive == false`, or `shipping_alive == false` — a dead
    T1 or T2 is logged as an error and shuts the agent down rather than
    leaving a zombie.

**Teardown** (`main.rs:397-450`) is ordered so nothing hangs: `shutdown.set()`
→ post `WM_TELEMOUSE_QUIT` to T1's window (fallback: `PostThreadMessageW`
`WM_QUIT`; last resort: detach T1 rather than join forever) → join T1 → set
`capture_stopped`, wake T2 → join T2 (which flushes the partial batch) → join
T3 → log the final `session finished` line including latency percentiles.
Joins go through `join_loudly`: a worker that panicked is reported and the
process exits with an error instead of pretending the run was clean.

### 6.3 T1 — `raw_input.rs`

This is the only genuinely hard code in the repo, and the audit spent most of
its effort here.

**Setup** (`win::run`, `raw_input.rs:647-760`): publish the thread id; raise
priority to `ABOVE_NORMAL`; `RegisterClassW` a window class
`"TelemouseRawInputClass"` with `wndproc`; `CreateWindowExW` with parent
`HWND_MESSAGE` (a *message-only* window: no screen presence at all);
`RegisterRawInputDevices` for usage page 1 / usage 2 (generic mouse) with
`RIDEV_INPUTSINK`; `RegisterHotKey` for `marker_hotkey` (F9 by default, with
`MOD_NOREPEAT`; failure only warns, and `""` registers nothing).
All per-thread state (`CaptureState`: the ring producer, stats, marker
sender, waker, context, device table, running totals) lives on the thread's
stack with a pointer in `GWLP_USERDATA`.

**The pump** (`'pump`, `raw_input.rs:759-860`) — the key idea from the two
CPU passes. The cadence timer is a `CreateWaitableTimerExW` with
`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (falling back to a plain waitable
timer, then to `thread::sleep`, which overshoots by ~0.5–0.8 ms); no
`timeBeginPeriod` anywhere:

```
live = false
loop {
    if live {
        WaitForSingleObject(cadence_timer)      // periodic: coalesce + 1 ms (default 9 ms)
        stamping = Spread{prev_now}             // nothing observed an arrival this period
    } else {
        MsgWaitForMultipleObjectsEx(INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)  // idle: block on the queue
        wake = qpc()                            // the burst's first report: exact
        SetWaitableTimer(due = coalesce, period = coalesce + 1 ms); WaitForSingleObject(timer)
        stamping = Anchored{wake}
    }
    now = qpc()
    n = drain_buffer(stamping)                  // GetRawInputBuffer, 256 reports per call
    live = n > 0                                // empty drain ⇒ hand stopped ⇒ cancel timer, back to the queue
    PeekMessage loop: WM_QUIT → break; stray WM_INPUT → single GetRawInputData; else DispatchMessage
}
```

Why: measured cycle-exact (`QueryThreadCycleTime`, 1 kHz synthetic input),
being **woken by the raw-input queue** costs ~25–30 µs of kernel CPU per
wake — the read itself is cheap. One wake per report at 1 kHz was ~2.8% of a
core; the 2026-08-25 pass waited 2 ms after each wake and drained in one
call (T1 1.39%); the 2026-08-29 pass stopped waiting on the queue at all
while reports keep coming — a periodic high-resolution timer paces the
drains — which took T1 to 0.45% at the same 2 ms window and 0.22% at the
8 ms default (~14 µs per drain, so cost ∝ drains/s = 1000 / (coalesce + 1)).
Messages (hotkey, quit, display change) are handled after the next drain, at
most one window late. The trade is timestamp precision inside a drain, which
is what `DrainStamper` handles.

**`DrainStamper`** (`raw_input.rs`, pure, tested) — two stamping modes:

- *Anchored* (the first drain of a burst): report 0 is stamped at `wake`
  (exact — that's when Windows said "input available"); report *i* is
  stamped `wake + step·i`, clamped to never exceed `now` (the read time).
  `step` is the estimated report interval, refined from consecutive anchored
  drains (`gap_since_previous_wake / previous_drain_count`), clamped to
  `[1 tick, coalesce + 1 ms]`, smoothed 3:1, and ignoring pauses over 20 ms.
- *Spread* (every cadence drain after that): nothing observed any arrival,
  so the `n` reports are spread evenly over the period since the previous
  drain, the last one at `now`. Even spacing bounds the *interval* error —
  a period one report short changes every spacing by a fraction instead of
  leaving one double-length gap — which is what velocity math cares about,
  and keeps stamps monotonic across drains for free.

On the 1 kHz harness recording the analyzer sees a median interval of
1.00 ms either way (p99 1.11 ms anchored-only vs ~1.4 ms with cadence
drains; a pause shorter than the window inside a burst is smoothed over). The
session record stores `coalesce_ms` so the analyzer knows the precision. Set
`coalesce_ms = 0` for one read per report and exact stamps.

**Per report** (`process_mouse`, `raw_input.rs:551-600`): resolve `hDevice`
→ `device_ix` (`devices::index_for`, resolves each handle's name exactly
once); `decode_mouse` (rejects `MOUSE_MOVE_ABSOLUTE` frames → counted as
`abs_frames`; reads `usButtonData` as a signed wheel into `wheel` or `wheel_h`
by flag; masks buttons with `buttons::MASK`); push onto the `rtrb` producer;
on a full ring, bump `ring_drops` and drop; bump `events`; store
`last_event_qpc`; call `waker.wake()`. All counters are **relaxed stores of a
thread-local running total**, not atomic RMWs, and `T1Counters` is
`#[repr(align(64))]` so nothing T2/T3 writes shares T1's cache line
(`t1_counters_own_their_cache_line`).

**`RingWaker`** (`raw_input.rs:106-150`): a `parked: AtomicBool` plus the T2
`Thread` handle. `wake()` is one relaxed load and only calls `unpark` if T2
actually armed the flag — so the per-report cost is a load, not a syscall.

**`wndproc`** handles `WM_INPUT` (fallback single read), `WM_HOTKEY` (the marker chord →
`MarkerSignal { ts_qpc, label: "hotkey" }` on the channel + wake T2),
`WM_DISPLAYCHANGE` (flag for T3 to re-read monitors), and
`WM_TELEMOUSE_QUIT`/`WM_DESTROY` → `PostQuitMessage`.

### 6.4 T2 — `shipping.rs`

Constants: `MAX_PARK = 25 ms`, `MIN_PARK = 500 µs`, `DRAIN_BUDGET = 8192`
events per pass, `TICK_INTERVAL = IDLE_PARK = 1 s`, `WARN_INTERVAL = 10 s`.

`ShipperCore` wraps the core `Batcher` with the session id, anchor, two
sequence counters (batches, markers), and two `DropAccountant`s (wrapping
deltas of T1's monotonic `ring_drops` / `abs_frames` totals, so each batch
carries "since last"). `build_batch` maps the first event's QPC to
`ts_anchor_us`, copies the context snapshot (game / locked / screen), and uses
`ctx.batch_cursor()` (cursor omitted while locked).

The loop (`run`, `shipping.rs:343-510`):

1. First thing, deliver `Envelope::Session` so the recording's first line is
   always the session record.
2. Each pass: sample `consumer.slots()` into `ring_high_water`; pop up to
   `DRAIN_BUDGET` events, pushing each into the batcher and flushing whenever
   `should_flush(ev.ts_qpc)` fires (checked per push so a batch never exceeds
   the datagram budget); drain the marker channel; time-based flush; every 1 s
   `tick_all` the sinks for maintenance. JSONL flush timing belongs entirely
   to its writer worker.
3. Park: if nothing is pending, arm the waker (`begin_park`), re-check the
   ring is empty, `park_timeout(1 s)`. If a batch is open, `park_timeout`
   for the remaining window (`park_hint`, 0.5–25 ms) *without* arming the
   waker — events pile up in the ring and are drained in bulk when the window
   expires. This is the second half of the CPU win: T2 wakes once per batch,
   not once per report (ring high-water ~45 instead of 1).

`flush` (`shipping.rs:512`) reads the context `Arc` once per batch (~40×/s at
the default window), records latency
(`capture_to_ship` from the first event, `ship_tail` from the last),
optionally prints, then `encode_and_deliver`: **serialize once** into a reused
64 KiB buffer (`EnvelopeEncoder`) and `fan_out` the same `&str` to every sink.
Sink failures are counted per sink and warned through a `WarnLimiter` (one
warn per sink per 10 s with a suppressed count).

### 6.5 T3 — `context_thread.rs`

Ticks every 250 ms (`TICK`); waits on the shutdown condvar between ticks.
Each tick `sample()` reads the primary screen, cursor, and foreground process
(via `ForegroundCache`, which only re-resolves the executable name when the
PID changes — one process-table snapshot per alt-tab, and no handle is ever
opened on the foreground process; see ANTICHEAT-2026-09-14.md), and feeds the
cursor plus "events since last tick" to `PointerLockDetector`.

**Pointer-lock heuristic** (`pointer_lock.rs`): cursor *frozen* while
*deltas flow* for 2 consecutive samples ⇒ locked (a game is consuming raw
input). Cursor moved ⇒ unlocked. Frozen with no events ⇒ unlocked (an idle
desk is not evidence). This is what `pointer_locked` on every batch means, and
what `--locked-only` in the analyzer filters on.

Also on its schedule: every 60 s re-measure the anchor and log drift in ppm,
emitting an `anchor_drift_us=N` marker past ±200 µs (a second `session` record
is deliberately never emitted); every 2 s stat `telemouse.toml` and on mtime
change reload + validate + log a diff (`describe_config_change`) + emit a
`config_changed` marker (a broken edit keeps the previous config); every 5 s
compute and log the stats line (§13); on `WM_DISPLAYCHANGE` re-read monitors.

`context.rs`: `SharedContext` is an `ArcSwap<ContextSnapshot>` — readers take
one lock-free `Arc` snapshot per flush rather than cloning strings or taking a
mutex.

### 6.6 Sinks (`sinks.rs`, `sinks/*.rs`)

```rust
pub trait Sink: Send {
    fn name(&self) -> &'static str;
    fn send(&mut self, topic: &'static str, key: &str, payload: &str) -> Result<()>;
    fn tick(&mut self) -> Result<()> { Ok(()) }
}
```

- **`UdpSink`** — binds `127.0.0.1:0`, `connect`s to `udp.addr`,
  non-blocking. `ConnectionReset`/`Refused` (viz not running — the normal
  state) are counted as `udp_unreachable` and are *not* errors; `WouldBlock`
  is a silent drop; oversized payloads count `udp_oversized`.
- **`JsonlSink`** — T2 copies the finished line into a bounded 256-envelope
  FIFO; a dedicated worker owns `<dir>/<session_id>.jsonl` and its 64 KiB
  `BufWriter`. It flushes at least once per second even under sustained load,
  records `jsonl_flush_max_us`, and drains/final-flushes on drop for up to 3 s.
  Records remain `jsonl_queued` until an explicit flush succeeds; failure and
  timeout races count unresolved buffered/in-flight work exactly once.
  Saturation drops quietly without blocking T2 and is visible as
  `jsonl_dropped` / `jsonl_abandoned`. This flush reaches the OS cache; it is
  not a power-loss `sync_data` guarantee.
- **`KafkaSink`** — `rskafka` (pure Rust; *not* rdkafka despite the plan).
  `connect` creates a bounded channel and returns immediately; the worker owns
  the 5 s broker/topic/producer initialization timeout, so an unavailable
  broker cannot delay capture startup. One `BatchProducer` per topic uses
  25 ms linger and zstd. `send` is `try_send` on the bounded (128) channel to
  the current-thread tokio worker with a bounded `JoinSet`; a full channel
  drops and warns *once on the edge* (and logs recovery). Drop cancels pending
  initialization or waits up to 3 s to drain, caps runtime teardown at 100 ms,
  and records `kafka_abandoned`. A terminal initialization failure emits one
  causal error; later envelopes are quiet counted drops instead of repeated
  fan-out errors. Accepted delivery failures and worker/task panics are also
  included in abandonment/error accounting.

Everything degrades: no viz, no Kafka, no writable dir → warnings, capture
continues.

### 6.7 Other modules

- `platform.rs` — thin Win32 wrappers (`qpc`, `qpc_freq`, `primary_screen`,
  `cursor_pos`, `foreground_process_name`, `monitors`, priority/throttling)
  with a non-Windows `imp` that uses an `Instant` as a 1 GHz "QPC" and returns
  empty/None for everything else, so logic tests run anywhere.
- `devices.rs` — `DeviceTable` (index 0 = `"unknown"`, max 256 because
  `device_ix: u8`), `enumerate_mice` via `GetRawInputDeviceList`.
- `session_setup.rs` — pure: `sandwich_anchor`, `anchor_drift_us`,
  `drift_ppm`, session id formatting, `build_session_config`.
- `stats.rs` — see §13.
- `shutdown.rs` — `Shutdown { AtomicBool, Mutex, Condvar }` with the
  lock-then-notify dance so a waiter can't miss the flag.

### 6.8 Thread/data-flow diagram

```
 Windows Raw Input (RIDEV_INPUTSINK, HWND_MESSAGE)             F9 / WM_DISPLAYCHANGE
          │                                                            │
 ┌────────▼──────────── T1 "telemouse-capture" ──────────────────────▼─────────┐
 │ idle: MsgWait(queue) → live: periodic timer (coalesce+1 ms) → GetRawInputBuffer │
 │ → DrainStamper → decode_mouse → device_ix → rtrb push (drop+count if full)   │
 │ counters: events, ring_drops, abs_frames, last_event_qpc (relaxed stores)    │
 └──┬──────────────────────┬──────────────────┬──────────────────┬─────────────┘
    │ SPSC ring<RawEvent>  │ RingWaker.wake() │ mpsc<MarkerSignal>│ display_changed flag
 ┌──▼──────────────────────▼─── T2 "telemouse-shipping" ───┐  ┌──▼── T3 "telemouse-context" ──┐
 │ park/unpark → drain → Batcher → flush(): ctx.get()      │◀─│ 250ms sample → ctx.set()       │
 │ EnvelopeEncoder (serialize once) → fan_out              │  │ 5s stats, 60s drift, 2s config │
 │ [UdpSink | JsonlSink | KafkaSink]; tick_all each 1s     │  │ → markers (+wake T2)           │
 └──┬──────────┬──────────────┬────────────────────────────┘  └────────────────────────────────┘
    ▼          ▼              ▼
 UDP :7878  recordings/  bounded chan → "telemouse-kafka" (tokio + rskafka)
```

---

## 7. `crates/viz` — live viz, replay, OBS overlay (`telemouse-viz`)

### 7.1 Rust side

`telemouse-viz [serve] [--config] [--udp ADDR] [--http ADDR] [--recordings DIR]`.
Runtime: tokio multi-thread pinned to **2 workers** (one socket, one listener,
a few WebSockets; a single-thread runtime measured no better). Startup
(`serve`, `main.rs:98-173`): load config (defaults on error), warn once if
the HTTP bind is not loopback (there is no authentication on the page, the
stream or the recordings), build `Arc<Hub>`, spawn `udp::listen` (bind
failure = warn, "live mode stays idle"), spawn `stats_reporter` (1 Hz), build
the axum router, serve. The listener is wrapped in `NoDelayListener`, which
sets `TCP_NODELAY` on every accepted connection — axum 0.8 dropped the option
and one small WebSocket frame every batch window is the worst case for Nagle
plus delayed ACK. The startup line prints the dashboard and `/obs` URLs
through `localhost::browse_addr`, so a `0.0.0.0` bind is shown as
`127.0.0.1` with a hint that other PCs use this machine's IP.

**Stopping** (`shutdown.rs`, `serve_until_stopped`). The console control
handler does one thing — `Shutdown::fire()`, a `watch<bool>` every clone can
wait on. Reporting the event as *handled* is also taking responsibility for
leaving: Windows' default "terminate now" no longer runs, so something in the
process has to act. Three things do. `axum::serve(…).with_graceful_shutdown`
stops accepting and lets in-flight requests finish; each `/ws` task watches
the same signal and answers with a `Close` frame, because an upgraded socket
outlives the HTTP connection it came from and nothing else would ever close
it (the page then shows "disconnected" and reconnects); and a new upgrade
arriving mid-stop gets `503 stopping`. Racing all of that is
`SHUTDOWN_DEADLINE` (750 ms from the signal), so a connection that will not
end on its own — a half-sent request, a held keep-alive socket, a 400 MB
replay still streaming — is dropped and logged rather than waited on. The
process then exits 0, well inside `ctl.stop_grace_secs` (8 s, clamped to 3 s
when Windows is waiting); a terminal console event is held up to
`SHUTDOWN_GRACE` (1.5 s) so teardown finishes before the process is taken.
Until v0.2.0 the handler only logged and `axum::serve` had no shutdown future
at all, so every "graceful" stop of viz was really the panel's grace period
running out followed by `TerminateProcess` (exit code 1).

**`udp.rs`** — `recv_from` into a buffer of `MAX_UDP_PAYLOAD + 8 KiB`, hand
bytes to `hub.publish`. Rejections are logged with a sanitized 96-byte
`payload_prefix`, first-of-each-kind loud then once per 60 s (`RejectLog`).

**`hub.rs`** — the fan-out. `classify_datagram` is a **tag probe**, not a
full parse: it deserializes only `{type, ts_anchor_us?}` (`TagProbe`) and
forwards the original text verbatim as `Arc<str>`. Reason: at 1 kHz the full
per-event serde parse *was* the bridge's whole CPU cost and bought nothing —
the browser parses anyway. `Hub` holds a `broadcast::Sender<Arc<str>>` of
capacity 256 (~6 s), caches the **latest `session` frame** so a late-joining
client gets exactly one copy first (`subscribe` builds the catch-up frame
under the same lock), and records `now_utc − ts_anchor_us` per batch as bridge
latency. Note: the hub does **not** track `seq_no`; gap detection is in the
page.

**`server.rs`** — `index.html` is `include_str!`'d. At startup `Pages::render`
produces two variants by substituting `/*__TELEMOUSE_CONFIG__*/` inside
`window.TELEMOUSE_CONFIG = {…}` with `{obs_route, obs: <ObsConfig>}` (`<`
escaped as `<` so config can't break out of the `<script>`). Routes:

| Path | Returns |
|---|---|
| `GET /`, `/index.html` | dashboard page (`no-cache`) |
| `GET /obs` | same page with `obs_route: true` |
| `GET /healthz` | `ok` |
| `GET /ws` | WebSocket: catch-up session frame, then every published frame; a client that lags the broadcast channel is sent `Close` and dropped (it costs itself, not everyone) |
| `GET /api/stats` | JSON `StatsPayload` (same shape as the pushed `viz_stats` frame) |
| `GET /api/sessions` | JSON list of `{id, path, bytes, modified_epoch_ms, started_utc_us, ended_utc_us}` — the time span is probed from 64 KB at each end of the file (session anchor; last batch/marker), so listing a 500 MB recording costs two small reads |
| `GET /api/session/{id}` | the JSONL file **streamed** with `Content-Length` (8 KB chunks; hour-long sessions never live in memory) |

**`recordings.rs`** — `is_safe_id` (≤128 chars, `[A-Za-z0-9_-]`, no leading
dot) *and* membership in the actual directory listing are both required to
resolve a session id; traversal, absolute paths, and NTFS `:$DATA` tricks all
fail the second check.

**`stats.rs`** — `LatencyHist`: 1024 buckets × 250 µs (0–256 ms), exact max,
negatives (clock skew) counted separately; `roll_latency` publishes the
completed 5 s window. `StatsPayload`: `type:"viz_stats", uptime_s, datagrams,
datagrams_per_s, forwarded, parse_errors, lag_drops, lag_disconnects,
clients, session_cached, latency{samples,p50_us,p99_us,max_us,mean_us,negative}`.

### 7.2 The page (`index.html`)

Single file, no CDN (a test asserts no external references). Roughly:

**One engine, two feeds.** `engine.ingest(envelope)`, `engine.tick(now)`,
`engine.draw()`. Live: `ws.onmessage → JSON.parse → ingest`. Replay:
`/api/session/{id}` streamed, split into lines, parsed → the same `ingest`.

**Ingest** (`engine.ingest`, `index.html:909-1060`) appends into
`EventColumns`, a struct-of-arrays timeline (`t: Float64Array`, `dx/dy/b/w/wh/mi:
Int32Array`, doubling growth, `dropFront` via `copyWithin`). Each batch pushes
one shared `meta` (`game, locked, sens, cx, cy, sw, sh`) and events index it.
Time is `qpcToT(qpc) = (qpc − anchorQpc) / qpcFreq` seconds. This is where
`drops_since_last`, `abs_frames_since_last`, and **seq_no gaps** (`lost = seq −
lastSeq − 1`) are accumulated, with red notches on the scrub track and a
one-time toast. A `session` envelope with a *different* id calls
`engine.reset()` first — a capture-agent restart must not mix timelines.

**Integrator** (`applyIdx`, `:1261-1345`) is the unit conversion: desk
`dx / cpi × 2.54` cm; aim `dx × sens × yaw_coeff`, `dy × sens × pitch_coeff`,
pitch clamped to ±89°, yaw accumulated raw and drawn unwrapped so the head
glides across the ±180° seam (a dashed line at every odd multiple of 180°, with
the origin axis repeated every 360°) instead of teleporting to the far edge. `sensFor(game)` uses the session's `games`
table or a flagged fallback (`sens 1.0, 0.022`; the CPI/sens tile shows `*`).
Velocity colouring uses a decaying peak reference (8 s half-life).

**Scrub without O(n) re-integration.** During a replay load,
`extendCheckpoints` snapshots the full integrator state every 10 timeline
seconds (`CHECKPOINT_SEC`). `seek` restores the nearest checkpoint on backward
moves or long forward jumps and integrates forward from there — verified
bit-identical to full re-integration in the audit.

**Play head.** Live targets `tEnd − liveBuffer` (default 35 ms, slider
10–200 ms, persisted in `localStorage["telemouse.liveBuffer"]` — the only key
used, and never in OBS mode); if >0.75 s behind it snaps forward, otherwise it
rate-adjusts 0.9×–2.2×. `tick` clamps `dt` to 0.25 s, consumes at most 12 000
events per frame, and `trim`s: compact after 24 096 consumed events back to a
20 000-event low-water mark, 40 000 hard cap, trail/ring/wheel lifetimes.

**Drawing.** `Trail` is a fixed ring (`1<<15`) of typed arrays. `drawTrail`
buckets segments into 14 velocity × 7 age classes and issues one `stroke()`
per non-empty bucket (~100 calls instead of thousands). Canvases use `{alpha:
false, desynchronized: true}` unless the OBS background is translucent.
**Dirty-flag rendering**: `frame()` always ticks but only draws when
`engine.dirty || visualsAlive()` — an idle page issues zero draws. Stats/HUD
repaint at 10 Hz. The desk panel also draws a cursor "ghost" minimap in
desktop mode (a live correctness check on integration).

**Stat tiles**: speed cm/s, aim °/s, hand distance m, aim distance °,
clicks/min, events/s, event age (time since the newest received mouse event
was captured; increases normally while idle), latency (browser arrival time
minus mean event capture time, smoothed across batches; warn >10 ms, alert
>25 ms, shows idle after 3 seconds without a sample), lag-behind-live (alert >250 ms),
FPS, ring drops, lost batches, abs frames, bridge (datagrams/s + p50 from
`viz_stats`), pointer locked/desktop, CPI/sens, and profile ms with
`?profile=1` (which wraps tick/draw/stats/ingest in `performance.measure`).

**Controls**: play/pause, prev/next marker, session select, speed 0.25×–8×,
scrub with amber marker notches; keys `R` recenter, `Space` pause,
`←/→` ±1 s (`Shift` ±10 s) in replay, `V` cycles the view. **View switch**:
the Both / Desk / Aim segment in the top bar (`setView`) shows one panel or
both and mirrors the choice into `?view=` so a bookmark keeps it; `/obs`
accepts `view=` as an alias of `layout=`. **Draw-rate cap** (`MAX_FPS`,
`?fps=`, 5–400): the dashboard repaints at most 120×/s, 60 in OBS mode, and
`FPS_BACKGROUND` (30) while the window is not focused; `engine.tick` still
runs every rAF so the data stays exact. **Go to time**: a `datetime-local`
picker (local time) → `ui.gotoUtcUs`, which picks the recording whose
`[started_utc_us, ended_utc_us]` contains the moment (else the newest one
that started before it, with a toast saying how far off it landed), loads it
if needed, and seeks to `(utc − anchor.utc_us) / 1e6` on the timeline. The
transport shows the play head's wall clock next to the elapsed time.
`?at=2026-08-29T21:14:03` (local ISO, or epoch ms) opens straight into replay
at that moment. WebSocket reconnect backs off
`400·1.7ⁿ` ms up to 8 s.

**Look and theme.** The dashboard uses the control panel's design tokens
(`--bg --surface --surface-2 --line --line-strong --fg --fg-2 --fg-3 --accent
--accent-ink --ok --warn --bad --shadow --sans --mono --r --r-sm`, copied from
`crates/ctl/src/index.html`): dark by default, light under
`:root[data-theme="light"]` or `prefers-color-scheme: light` when no theme was
picked. The *Light theme / Dark theme* button stores `localStorage.tmTheme`
(same key and behaviour as the panel, separate origin so separately
remembered; Shift-click hands the choice back to Windows), and a script in
`<head>` applies it before first paint. The canvases cannot follow CSS, so
everything they draw with lives in one `PAL` object: `readTokens()` fills it
from the tokens plus the canvas-only ones (`--canvas --grid --grid-axis
--grid-wrap --head --lmb --rmb --aux`, plain hex on purpose) at start-up and
on a theme change — **never per frame** — and `buildRamp()` swaps the velocity
ramp for one that ends dark instead of white on a light canvas. The top bar
and the transport wrap; the stats bar is a CSS grid (`auto-fill`), so tiles
keep one width on every row; under 900 px the panels stack at ≥240 px each and
the page scrolls instead of squeezing the canvases.

**Feed state** is time-driven, so the 10 Hz block of `frame()` is what
evaluates it (nothing arrives to announce that nothing is arriving):
`ui.refreshConn()` moves the dashboard pill between *waiting for capture on
udp …*, *live* and *no data for N s* (`feedState`, fixed 3 s), and
`updateStale()` drives the overlay's indicator. `ui.onBatch()` clears the
overlay's state the instant a batch arrives. `js-tests/feed.test.mjs` drives
`frame()` on a fake clock for both.

**OBS mode** (`?obs=1` or the `/obs` route): config layering is built-in
defaults → `[viz.obs]` → URL params (`layout, bg, hud, hudpos, scale, trail,
buffer, grid, legend, labels, stale`); chrome hidden, HUD replaces the stats
bar, toasts suppressed, no localStorage, **not themed** (the head script pins
the dark tokens, drops the `color-scheme` hint so the page stays see-through,
and `PAL` keeps its built-in values; HUD and label colours are fixed light
text with a shadow because they sit on game footage, not on the page).
**No feed**: after `stale` seconds without a batch (`[viz.obs] stale_secs`,
`?stale=`, 0–60, 0 = never) — counted from the last batch, or from page load
when there has been none, and deliberately blind to the socket so a page
that is still connecting does not flash it — `body.stale` dims the canvases
to 40 % and shows a *no feed · 12s* badge top centre (`#feedBadge`); the
`status` HUD item says the same. The capture agent sends nothing while the
mouse is still, so a hand at rest for longer than `stale` reads as no feed
too; raise `stale_secs` (or set 0) if that bothers a stream. **Hidden
source**: `obsSourceVisibleChanged` / `obsSourceActiveChanged` (or
`visibilitychange` in a plain browser) stop `frame()` from drawing while it
keeps ticking; coming back repaints once. **Hidden tabs**: rAF stops but the socket
doesn't, so `ingest` enforces the hard cap and a `setInterval(trim, 250)`
keeps memory bounded; on return the play head snaps forward.

**OBS on another PC**: bind the viz to the LAN (`[viz] http_addr =
"0.0.0.0:7879"` or `--http 0.0.0.0:7879`; `serve` warns once that the stream
and the recordings are now reachable without authentication), allow TCP 7879
inbound on the Private firewall profile, and point the streaming PC's Browser
source at `http://<gaming PC's IPv4>:7879/obs`. The page builds its WebSocket
URL from `location.host`, so nothing else changes. The `Host`/`Origin` guards
in `telemouse_core::localhost` accept IP literals, which is why the URL must be
an IP and not a machine name (a name would be refused as a possible DNS
rebind). UDP capture → viz stays on loopback; only the HTTP/WS side opens up.

### 7.3 Datagram → pixel

```
UDP datagram → udp::listen → hub.publish → classify_datagram (tag probe)
  → record_latency, cache session, broadcast Arc<str> (cap 256)
  → client_loop → WebSocket text frame
  → ws.onmessage → JSON.parse → engine.ingest → EventColumns + metas
  → rAF frame(): tick → advance play head → consume → applyIdx (counts→cm/°)
      → Trail/EffectRing → (if dirty) Panel.render → grid, bucketed strokes, rings, head
  → 10 Hz: refreshConn + paintStats (dashboard) / updateStale + paintHud (OBS) / transport
```

---

## 8. `crates/analyze` — offline metrics (`telemouse-analyze`)

### 8.1 CLI

- `report <SESSION.jsonl | session-id> [--dir recordings] [--summary]
  [--json FILE] [--json-dir DIR] [--csv-dir DIR] [--timing] [--quiet]
  [detector flags…]` — a bare id is looked up as `<dir>/<id>.jsonl`;
  `--summary` prints `Report::summary()` (the headline numbers, ~3 KB of
  JSON — ~5 KB for a marked session — tagged `telemouse-report-summary/2`)
  instead of the terminal rendering. It carries the first 12 markers and marker intervals with their
  labels (`markers`, `markers_total`, `segments`, `segments_total`), so an
  experiment marked "sens A" / "sens B" is readable without the full
  document; `MAX_SUMMARY_MARKERS` and `MAX_SUMMARY_LABEL_CHARS` are what keep
  it a few KB.
- `trend [--dir recordings] [--json-dir DIR] [--metric a.b.c]… [--csv FILE]
  [--json] [detector flags…]`
- `list [--dir recordings] [--json]` — header-only scan, never loads events.

`--help` groups the flags under clap `help_heading`s (Input, Output,
Filtering, then three Advanced groups for the detector parameters) and ends
each command with copy-pasteable examples (`after_help`). Headings appear in
the order their first flag is declared, so the field order of `ParamFlags`
is the order of the groups.

**Double-click guard** (`explorer.rs`). Started with no arguments, as the only
process on its console (`GetConsoleProcessList` = 1, the test ctl's
`hide_console_if_owned` uses; declared here as a one-function `extern` block
rather than a `windows` dependency) and with stdin and stdout both that
console, the exe prints what it is, points at the Report button in
telemouse-ctl, and waits for Enter before exiting 2 — instead of a window
that flashes and vanishes. The decision is the pure
`should_hold_window(Launch)`; any argument, a shared console, piped stdio
(ctl, scripts, CI) or a non-Windows build never reaches the blocking read.

Every detector parameter is a flag; unset flags keep `Params::default()`
(`series.rs:106-133`):

| Flag | Default | Meaning |
|---|---|---|
| `--grid-dt` | 0.001 s | grid cell |
| `--sg-half` / `--sg-order` | 3 / 2 | Savitzky–Golay half-window (cells) and polynomial order |
| `--flick-threshold` | 800 counts/s | smoothed speed that starts a flick |
| `--still-threshold` | 50 counts/s | "not moving" |
| `--still-hold-ms` | 20 | quiet cells needed to call a flick settled |
| `--click-window-ms` | 300 | max flick-start → click to pair them |
| `--pre-click-lo-ms` / `--pre-click-hi-ms` | 100 / 50 | pre-click stability window (before the click) |
| `--double-click-max-ms` | 500 | |
| `--min-segment-ms` | 3 | drop movement segments shorter than this |
| `--min-reversal-ms` | 4 | a direction reversal must sustain this long (rejects SG edge lobes) |
| `--tremor-baseline-ms` | 200 | boxcar width for the tremor residual |
| `--max-grid-cells` | 32 000 000 (~8.9 h) | grid truncation cap |
| `--lift-drift-min-ms` / `-max-speed` / `-min-counts` | 120 / 4000 / 400 | lift: the slow drift |
| `--lift-return-min-speed` / `--lift-max-gap-ms` / `--lift-opposite-cos` | 8000 / 400 / −0.6 | lift: the snap-back |
| `--locked-only` | off | degree-valued metrics only over `pointer_locked` spans |
| `--split-by-marker` | off | render per-interval sub-reports (they're always in the JSON) |

`report` goes through `trend::report_for`, so `--json-dir` caching is shared:
a cached `<id>.report.json` is reused only if analyzer version, params (exact
JSON round-trip), and recording mtime all match.

### 8.2 Library API (`lib.rs`)

Three calls: `load::load_session(path)` → `report::build(session,
Params::default())` → `report.render()`. `testutil` is a public module so
integration tests and benches can build synthetic sessions.

### 8.3 Pipeline

**`load.rs`.** One `Envelope` per line, first line must be `session`.
Designed for 500 MB files: `Vec<RawEvent>` capacity from `file_len / 71`, one
reused 64 KiB line buffer, 1 MiB `BufReader`, dispatch by literal prefix
(`{"type":"batch"`) before serde, batches deserialized through a borrowing
shadow struct `BatchRef<'a>` (game as `Cow`, events moved out), game names
interned as `Arc<str>`. Malformed lines count as `bad_lines` rather than
failing. Output `LoadedSession { config, events, markers, batches: Vec<BatchMeta>,
total_drops, total_abs_frames, bad_lines }` with `dominant_game()`,
`device_indices()`, `resolve_aim()` (falls back to 0.022 and flags it).
Measured: 88 ms for a 32 MB file.

**`series.rs` — the sparse 1 ms grid.** `prepare(session, params)` takes the
session *by value* (no 200 MB clone). Event times become integer µs since
`t0` (integer on purpose: `0.003/0.001` floors to 2 in floating point —
`grid_binning_is_exact_at_every_millisecond`). A dense grid would be ~52 B/cell
= 560 MB for three hours, mostly zeros, so the grid is a list of **`Run`s**:
contiguous spans of cells around activity, each holding `vx, vy` (binned
counts ÷ dt), `vxs, vys` (SG-smoothed), `speed_raw`, `speed`, `clicks`, and
reverse-lookup tables. Runs are padded by `pad = max(sg_half,
tremor_baseline_ms/2, 1)` on both sides so every filter window over a
data-bearing cell lies entirely inside a run and every cell outside is exactly
0 — **the sparse grid is bit-identical to the dense one**
(`sparse_grid_matches_a_dense_grid`). `Grid` queries: `run_at`, `runs_in`,
`quiet_start_after` (skips inter-run silence in O(1)), `displacement`,
`path_length`, `peak_speed`, `clicks_in`, `last_moving_cell` (O(log runs) via
`last_move` / `prev_last_move`). `movement_segments` yields maximal spans above
`still_speed` at least `min_segment_ms` long. `Prepared` also carries
`locked_events` / `locked_cells` spans derived from `pointer_locked` batches
and `analysis_duration_s` (the honest denominator when the grid is truncated).

**`savgol.rs`.** `SavGol::new(half, order, deriv)` derives coefficients from
the least-squares normal equations (Gaussian elimination with pivoting) rather
than hardcoding, with one weight vector per output offset so the first/last
`half` cells are evaluated off-centre from the nearest full window (a
polynomial of degree ≤ order passes through unchanged everywhere). `apply(y,
dt)` scales by `dt^-deriv`. Tests confirm the textbook `(−3,12,17,12,−3)/35`
kernel.

**`kinematics.rs`.** Accel and jerk come from SG *differentiating* operators
(deriv 1 and 2) applied per run and fused with the moving-cell filter so
nothing session-length is materialised. Reported in counts, cm, and degrees:
speed/accel/jerk summaries, `total_distance_{counts,cm,m,deg}`,
`net_yaw_deg`/`net_pitch_deg`, `path_efficiency` (per segment: `min(1,
|net| / path_length)`; also length-weighted), `moving_time_s`,
`moving_fraction`, `distance_cm_per_min`.

**`flicks.rs` — the headline detector** (`detect`, `flicks.rs:102`):

1. Scan runs for smoothed `speed ≥ flick_speed` (800).
2. **Start** = rewind while `speed > still_speed`. **Ballistic end** =
   advance while `speed > still_speed`.
3. **Settle** = `quiet_start_after(ballistic_end, hold)` — the first point
   after which `still_hold_ms` (20) consecutive cells are still. Corrective
   sub-movements push this later; a movement more than 20 ms later is a new
   flick.
4. **Amplitude** = |displacement(start → ballistic end)| from raw velocity;
   unit vector `(ux, uy)`.
5. **Overshoot ratio** = correction ÷ amplitude, where correction is the
   displacement(ballistic end → settle) projected onto `(ux, uy)`, counted
   **only if negative** (a reversal against the flick direction). This is the
   metric the plan calls "the sens-too-high/too-low tell".
6. **Time-to-click** = next button-down within `click_window_ms` after the
   flick start, found with a monotonic two-pointer over sorted click times
   (linear in the session; parity-tested against the old scan).
7. Emit `Flick { t_start_s, t_ballistic_end_s, t_end_s, duration_ms,
   amplitude_deg/counts, peak_velocity_deg_s/counts_s, overshoot_ratio,
   correction_deg, settle_ms, time_to_click_ms, direction_deg, corrections }`
   and resume after `settle_at`.

`reversals()` counts sign changes of velocity projected onto the heading with
a `still_speed` deadband and a `min_reversal_ms` sustain requirement, which
rejects the one-cell negative lobe the SG kernel leaves at a movement's
trailing edge.

**`micro.rs`.** *Corrections*: `reversals` per movement segment
(`corrections_per_segment`, `clean_segment_fraction` = ≤1 correction).
*Tremor*: residual = velocity − centred 200 ms boxcar (prefix sums; chosen
because the 7-point SG smoother's passband reaches hundreds of Hz whereas a
200 ms boxcar has nulls at 5/10/15 Hz and leaves 8–12 Hz intact);
`tremor_rms_counts_s` over cells with `speed_raw > 0`; per-second energy kept
in `TremorSeries` for the per-minute table and marker segments. *Band power*:
residual block-averaged 20:1 to 50 Hz (the boxcar doubles as anti-alias),
Hann-windowed 2 s blocks at half-hop, Goertzel at each integer Hz 1–25,
averaged over active blocks → `band_power_8_12`, `band_ratio_8_12`,
`dominant_hz`. *Micro-adjustment histogram*: segment net amplitude bucketed at
`[2,5,10,20,50,100,200,500,1000,2000]` counts.

**`clicks.rs`.** Walks all five buttons per event (one event can carry
several transitions). Per down: `pre_click_speed` = mean speed in the window
100→50 ms before; `click_to_still_ms` = time since `last_moving_cell`;
double-click gap per button. Per up: `hold_ms`. Aggregates:
`clicks_per_min`, `still_click_fraction`, `unmatched_downs`, `per_button`.

**`quality.rs`.** Inter-event interval histogram (edges 0.5/1/2/4/8/10/20/50/
100 ms), median/p99/max interval, `pct_within_1ms`, `gaps_over_10ms` (counted,
never warned — stillness is silent), `monotonicity_violations`, `ring_drops`,
`lost_batches`/`seq_gaps`, `bad_lines`, `abs_frames`, `batch_latency_ms`
(`ts_anchor_us − qpc_to_utc(first_event)`), `dominant_game_share` (warn
<0.8), `locked_fraction` (warn <0.9), `device_indices` (warn if >1),
`anchor_uncertainty_us`, `aim_profile_missing`, `grid_truncated`. `clean =
warnings().is_empty()`.

**`lifts.rs`.** Lifts are invisible to HID, so infer: segment *a* is a slow
sustained drift (≥120 ms, peak ≤4000 counts/s, ≥400 counts), then within
≤400 ms segment *b* is a fast return (peak ≥8000) whose direction cosine
against *a* is ≤ −0.6. Emit `Lift { drift_ms, drift_cm, gap_ms, return_cm,
opposition, direction_deg }`.

**`markers.rs`, `per_second.rs`, `per_minute.rs`.** Intervals between markers
always start with an unlabelled one at 0. Per-second rows (events, distance
in counts/cm/deg, mean/max speed, clicks, flicks, moving_ms, marker) roll up
into per-minute rows with `overshoot_median`, `settle_median_ms`,
`tremor_rms`, `path_efficiency`, `moving_fraction` — the fatigue/warm-up
curve. On a truncated grid the partial last second is dropped so the tail
doesn't spike.

**`report.rs`.** `build()` runs, timed per phase: prepare → quality →
kinematics → flicks → micro → clicks → lifts → markers → per_second →
per_minute. `Report { schema: "telemouse-analyze/2", analyzer_version,
generated_utc_us, compute_ms, grid_cells, grid_stored_cells, grid_dt_us,
params, session, quality, kinematics, flicks, micro, clicks, lifts, markers,
segments, per_second, per_minute, warnings, timings }`. `render()` is the
78-column terminal report; `write_json` streams pretty JSON; `write_csvs`
writes `per_second.csv`, `flicks.csv`, `per_minute.csv`.

**`trend.rs`.** One `TrendRow` per `*.jsonl` (flicks, flicks/min, overshoot
median, settle median, tremor RMS, path efficiency, clicks/min, distance m,
lifts, plus any `--metric` dotted paths resolved through the report JSON).
This is the substrate for warm-up curves and sensitivity A/B.

Measured on a synthetic 3-hour / 814k-event fixture: full analysis 730 ms.

---

## 9. Configuration reference (`telemouse.toml`)

All fields optional; unknown keys are errors. `telemouse.example.toml` is the
loopback sample that ships with releases (copied into the zip as
`telemouse.toml`) and that `telemouse-ctl` writes as `telemouse.toml` on
first start when none exists; the repository's `telemouse.toml` is the
development machine's own config.

Every binary looks for the file with `telemouse_core::paths::locate_config`:
the path given (default `telemouse.toml`) in the working directory first,
then next to the executable. Relative paths inside the file (`recording.dir`,
`ctl.log_dir`, `ctl.bin_dir`) are relative to the file's own directory. A
file that exists but does not parse is refused by every binary; only a
missing file means defaults. Game keys must be the lowercase executable name
and end in `.exe`, or the config is refused: a key that never matched would
silently turn every degree-valued metric into a guess.

```toml
mouse_cpi = 1600.0            # counts per inch → cm. Wrong value = wrong cm, fixable later.
marker_hotkey = "f9"          # system-wide chord that drops a marker (same grammar as [ctl] hotkey, must differ from it); "" = none

[batch]
window_ms = 25                # responsive live default; use 50 to halve per-batch CPU
max_events = 448              # ≤ MAX_EVENTS_PER_BATCH
ring_capacity = 65536         # SPSC ring slots; ≥ max_events
coalesce_ms = 8               # T1 drain cadence − 1 ms (0–10). 8 ≈ 0.22% of a core at 1 kHz, 2 ≈ 0.45%, 0 = exact per-report stamps ≈ 2.8%

[udp]
enabled = true
addr = "127.0.0.1:7878"       # capture → viz

[kafka]
enabled = false               # local broker via compose.yaml; capture never depends on it
brokers = ["127.0.0.1:9092"]  # host:port, always — a port-less entry is a config error

[recording]
enabled = true
dir = "recordings"

[viz]
http_addr = "127.0.0.1:7879"  # 0.0.0.0:7879 to serve the OBS overlay to another PC on the LAN (that PC gets /obs + /ws only; no auth: trusted networks only)

[ctl]
http_addr = "127.0.0.1:7880"  # control panel; loopback — it can kill processes
# bin_dir = "target/release"  # where the binaries are; default: next to telemouse-ctl, then PATH
stop_grace_secs = 8           # Ctrl-Break → wait → terminate (0–60); ≥ the agent's two 3 s sink drains
log_dir = "logs"              # ctl.log + <component>.log per launched component; rotated at 8 MB
hotkey = "ctrl+alt+r"         # system-wide, needs the tray: stop capture if running, start one that saves; "" = none

[viz.obs]                     # defaults for /obs; every one overridable by URL param
layout = "split"              # split | stack | desk | aim
background = "transparent"    # or #rrggbb / #rrggbbaa
hud = ["speed", "aim", "cpm"] # also eps, dist, aimdist, clicks, game, latency, eventage
hud_position = "bottom-left"
scale = 1.0                   # 0.5–4
trail_secs = 3.0              # 0.3–12
buffer_ms = 35                # 10–200
stale_secs = 3.0              # 0–60; seconds without data before the overlay dims and says "no feed" (0 = never)
grid = true
legend = false
labels = false

[games."cs2.exe"]             # key = lowercase process name; degrees = counts × sens × coeff
sens = 1.0
yaw_coeff = 0.022             # Source/Quake/Apex 0.022; modern CoD & Overwatch 0.0066; Valorant 0.07
pitch_coeff = 0.022
```

Who reads what: capture reads everything and snapshots `mouse_cpi` + `games`
into the session record; viz reads `udp.addr`, `viz.*`, `recording.dir`;
ctl reads `ctl.*`, `viz.http_addr` (for the dashboard link) and
`recording.dir` (for the report picker), and hands the same file to every
component it launches via `--config`;
analyze reads nothing from it (it uses the session record inside the file —
that is the point of recording the config per session).

Mid-session edits are detected by T3 (2 s mtime poll), validated, diffed into
the log, and recorded as a `config_changed` marker — but not re-applied to the
running session (the session record is immutable).

---

## 10. The wire format, line by line

From `recordings/demo-session.jsonl` (an older-format file — note the `null`s
and `"buttons":0` that current capture omits; both forms load):

```json
{"type":"session","session_id":"demo-session","started_utc_us":1756000000000000,
 "qpc_freq":10000000,"anchor":{"qpc":5000000000,"utc_us":1756000000000000,"qpc_freq":10000000},
 "anchor_uncertainty_us":9,"mouse_cpi":1600.0,
 "devices":["\\\\?\\HID#VID_1532&PID_0099&MI_00#DEMO","\\\\?\\HID#VID_046D&PID_C548&MI_01#DEMO"],
 "games":{"cs2.exe":{"sens":1.0,"yaw_coeff":0.022,"pitch_coeff":0.022}},
 "monitors":[{"width":2560,"height":1440,"refresh_hz":240,"primary":true}],
 "capture_version":"0.1.0-demo"}

{"type":"batch","session_id":"demo-session","seq_no":0,"ts_anchor_us":1756000000001000,
 "game":"cs2.exe","pointer_locked":true,"screen_w":2560,"screen_h":1440,
 "cursor_x":null,"cursor_y":null,"drops_since_last":0,
 "events":[{"ts_qpc":5000010000,"dx":8,"dy":0,"buttons":0,"wheel":0}, …]}

{"type":"marker","session_id":"demo-session","seq_no":0,"ts_qpc":5028800000,
 "ts_utc_us":1756000002880000,"label":"flick"}
```

Reading the batch: `qpc_freq` is 10 MHz, so `ts_qpc` 5000010000 is
(5000010000 − 5000000000)/10 000 000 = 1.0 ms after the anchor → UTC
1756000000001000 µs, which matches `ts_anchor_us`. Consecutive events are
10 000 ticks = 1 ms apart: a 1 kHz mouse. `dx: 8` at 1600 CPI is 0.0127 cm of
hand travel and, in CS2 at sens 1.0, 0.176° of yaw.

A current-format motion event is just `{"ts_qpc":…,"dx":…,"dy":…}`.

Kafka: same JSON, topics `mouse.events` (batches), `mouse.sessions`
(compacted, session records), `mouse.markers`; key = session id.

---

## 11. Time: QPC, anchors, and why timestamps are trustworthy

- Every event is stamped with `QueryPerformanceCounter` (10 MHz on modern
  Windows) in T1. QPC is monotonic and immune to wall-clock adjustments.
- The **anchor** pairs one QPC reading with UTC. It is measured as a
  *sandwich* — QPC, `SystemTimePreciseAsFileTime`-quality UTC, QPC — anchored
  at the QPC midpoint, with the half-width recorded as
  `anchor_uncertainty_us` (typically single-digit µs). All later mapping is
  integer arithmetic in `QpcAnchor`, so every consumer gets identical results.
- T3 re-measures every 60 s and logs drift in ppm; a `anchor_drift_us=N`
  marker is emitted past ±200 µs so the analyzer can see it. The session
  record is never re-emitted (one anchor per session is the contract).
- With coalescing, only the first report of a *burst* is stamped at its
  observed arrival; the rest of that drain are spaced by the estimated
  interval and clamped to the read time, and every later drain of the burst
  (taken on the cadence timer) is spread evenly over its period, ending at
  the read time. `SessionConfig.coalesce_ms` tells you the precision (8 ms
  default). The audit measured the analyzer's view of a 1 kHz mouse as median
  1.00 ms, p99 1.25 ms with coalescing vs 1.00 / 1.29 ms exact.
- The viz's latency tile samples browser arrival time minus the batch's mean
  event capture time, mapped through the session anchor. It includes capture
  batching and transport, but excludes playback buffering and rendering.
  Event age separately shows how old the newest received mouse event is.
  Both require synchronized capture and browser clocks; negative latency
  indicates clock skew. The bridge also counts negative samples separately.
- The analyzer works in integer µs since `t0` and flags any non-monotonic
  interval as a `monotonicity_violation`.

---

## 12. Units: counts → cm → degrees

A mouse at CPI *c* reports one count per 1/*c* inch of travel, so
**cm = counts / CPI × 2.54**. That is the desk-space panel and every
`*_cm` metric.

Games turn counts into camera rotation linearly: **degrees = counts × sens ×
coeff**, where `coeff` is the engine's degrees-per-count at sensitivity 1
(`m_yaw` in Source = 0.022). The per-game table lives in `[games]` and is
snapshotted into the session record; the game in effect for each batch is the
foreground process name. The viz picks `sensFor(batch.game)` per batch; the
analyzer picks the **dominant game** for the whole session (and warns if it
holds <80% of events). A missing profile falls back to 0.022 and is flagged
(`aim_profile_missing`, `*` in the viz tile, `FALLBACK` in the report).

Yaw accumulates without bound and the viz draws it unwrapped, marking each
±180° seam with a dashed line; `analyze` wraps to (−180°, 180°]. Pitch is
clamped at ±89° in the viz. `--locked-only` restricts degree metrics to
pointer-locked spans so desktop mousing doesn't count as "aim".

---

## 13. Observability: what the logs tell you

"Metrics are logs here, no metrics server" (CONVENTIONS.md). Every
long-running loop emits a structured `tracing` line every 5 s. All of this
chapter is the `logging` and `observability` features (§4): the minimal
release zip has none of it, the developer build has all of it. Anything that
stays wrong is repeated as a `warn` once a minute — a sink that is dead or
dropping, a ring overflow, no input for a minute — so a problem is never only
a number in the stats line. Every binary opens with one line naming its
version, build profile, features, the config file it read and whether the
file existed, and the values that differ from the defaults.

**Capture (`stats.rs`, T3 logs it)** — 28 structured fields: `events_per_s,
events, batches_per_s, batches, drops, drops_delta, abs_frames, markers,
udp_errors, udp_unreachable, udp_oversized, jsonl_errors, kafka_errors,
jsonl_queued, jsonl_dropped, jsonl_abandoned, kafka_queued, kafka_dropped,
kafka_abandoned, ring_high_water,
jsonl_flush_max_us, capture_to_ship_us_p50, capture_to_ship_us_p99,
ship_tail_us_p99, idle_for_s, game, pointer_locked, session_id`.

How to read it: `drops` > 0 means the ring filled (T2 stalled) — should never
happen; `ring_high_water` rising is the early warning. `udp_unreachable` just
means the viz isn't running. `idle_for_s` distinguishes "mouse is still" from
"capture is broken" (the latter also kills the process loudly via
`capture_alive`). `capture_to_ship_us_p50` is typically ≈ half the batch
window. The latency histogram is a zero-allocation 32-bucket log2 array of
atomics; percentiles are the upper bound of the bucket holding the quantile
(conservative). `jsonl_queued` means accepted but not yet flush-confirmed;
`*_abandoned` is unresolved accepted work claimed by a failure or bounded
shutdown, while `*_dropped` was never accepted by that sink.

**Viz** — `viz stats` line every 5 s (silent when idle with no clients) and
the same numbers at `/api/stats` and pushed into the page as `viz_stats`:
datagrams/s, forwarded, parse_errors, clients, lag drops/disconnects, bridge
latency p50/p99/max. Observed loopback bridge latency p50 ≈ 0.2 ms.

**Analyzer** — a `warn` per data-quality problem, per-phase timings
(`--timing`), and a final `analysis complete total_ms=` line.

Two kinds of loss are always kept separate: **ring drops**
(`drops_since_last`, capture-side) and **lost batches** (`seq_no` gaps,
transport-side). Both are visible in the capture log, the viz tiles, and the
analyzer's quality section.

**The session metadata sidecar.** When the agent stops it writes
`recordings/<session_id>.meta.json` (`telemouse_core::recordings::SessionMeta`,
built by `capture::meta`): why the run ended, whether every thread joined
cleanly, the event/batch/marker/ring-drop totals, and per enabled sink its
`errors`, `dropped` (refused: queue full or sink already failed) and
`abandoned` (accepted, never delivered). `telemouse-analyze list` reads it
into a `LOSS` column and the JSON listing (`losses`, `exit`), so a Kafka
outage that dropped batches is visible weeks later without re-deriving it
from the JSONL. The sidecar is re-read on every listing even when the
recording itself is cache-hit, because it lands after the recording's last
flush.

**Panics.** Every binary installs `telemouse_core::panic_hook` at startup: a
panic on any thread is a `tracing::error!` (component, thread, location,
message) before the default hook prints it, so it reaches `ctl.log` /
`<component>.log` for a tray-launched process. In the agent all three worker
threads carry an `AliveGuard`; a context-thread panic stops the run
(`exit = context-thread-exited` in the sidecar) rather than recording a
frozen game name for the rest of the session.

**Health.** viz `/healthz` is `{ok, udp_bound, last_datagram_age_s, clients,
uptime_s}` — 503 while the UDP listener is not bound. The panel's `/healthz`
is still a constant `ok`: answering is its health.

**Unexpected exits with a hint.** The panel reads a child's last lines when it
exits unasked; an `unknown field` config rejection (a binary older than
`telemouse.toml`) is reported as `code 1 (telemouse.toml has a key this
binary does not know: rebuild the workspace)` in the log, the page and the
tray, instead of a bare code.

---

## 14. Performance design decisions

**Where the cost is when a game is running (measured 2026-08-29, CoD at
240Hz):** the capture agent at 1kHz is ~0.5% of one core and the bridge
~1.3% — neither is felt in-game. The dashboard *tab* was: Chrome's GPU
process at 124% of a core and the tab's renderer at ~45%, from two full
canvases repainted at the monitor's 240Hz on the game's GPU; the control
panel page added ~5% (a process-table walk per 1.5 s poll). Hence the draw
cap in `frame()` (`?fps=`, default 120 / OBS 60, 30 while the dashboard is
unfocused — `engine.tick` still runs every rAF so the data stays exact) and
the 4 s scan cache in ctl. The cheapest configuration while playing remains
no dashboard tab at all: OBS composites its browser source at its own rate.

Numbers from `docs/AUDIT-2026-08.md` (5950X, 1 kHz mouse, cycle-exact via
`QueryThreadCycleTime`): the first CPU pass took capture+viz from 4.52% of a
core to 2.18%; the second (2026-08-29) took the whole stack — capture, viz
with one browser connected, ctl being polled — from **2.57% to 0.76%**
loaded and from 0.29% to 0.09% idle. The per-hop costs that remain are
kernel floors (~47 µs per loopback send, ~15 µs per raw-input drain) times
rates. The 2026-09 latency pass changed the live default to `window_ms` 25
while retaining the CPU-saving `coalesce_ms` 8; use 50ms when minimum CPU is
more important than responsiveness. `docs/BENCHMARKS.md` has the measured
tradeoffs.

| Decision | Where | Why |
|---|---|---|
| Live stream paced by a periodic high-res timer; the queue wait only while idle | T1 | a wake *by the raw-input queue* costs ~27 µs of kernel CPU; a timer wake plus the read ~15 µs |
| Coalesced raw-input reads (`GetRawInputBuffer` once per period) | T1 | cost is per drain, not per report: 1000/(coalesce+1) drains/s |
| 25 ms batch window (40 batches/s) | T2/viz | responsive live default; 50ms halves the ~150 µs-per-batch kernel cost |
| Two-tier process scan: Toolhelp enumeration every 30 s, per-PID queries per poll | ctl | the snapshot alone is ~7 ms of kernel time; sysinfo's full walk was ~16 ms |
| Wake T2 only on empty→non-empty, then sleep out the window | T1/T2 | one wake per batch instead of per report |
| 1 s idle park; markers/config/shutdown wake T2 explicitly | T2 | no 25 ms idle wakeups |
| Cache-line-isolated T1 counters, relaxed stores | T1 | no false sharing, no RMWs on the hot path |
| Serialize once, `&str` to all sinks; owned byte buffers in worker queues | T2 | one JSON encode per batch; JSONL/Kafka each copy once to leave T2 immediately |
| `Arc<ContextSnapshot>` read once per flush | T2/T3 | no per-iteration mutex + String clone |
| Foreground name re-resolved only on PID change | T3 | ~14 400 handle opens/hour → ~one per alt-tab |
| ABOVE_NORMAL T1 priority, EcoQoS opt-out | T1 | E-core parking can't starve capture |
| Tag-probe instead of full parse; `Arc<str>` broadcast | viz hub | serde was the whole bridge cost |
| Typed-array columns, ring-buffer trails, bucketed strokes, dirty-flag draw | page | zero draws when idle; no per-event objects |
| Integrator checkpoints every 10 s | page | scrub cost bounded by checkpoint spacing, not session length |
| Streamed `/api/session/{id}` | viz | hour-long files never in memory |
| Sparse run-based grid, by-value `prepare`, fused derivatives | analyze | 562 → 358 MB on the bench fixture; −192 MB clone |
| Two-pointer flick/click matching, reverse tables for click-to-still | analyze | both quadratics gone |
| 20:1 decimated Welch/Goertzel | analyze | ~40× less spectral work, ground truth unchanged |
| High-resolution waitable timer for the cadence (`thread::sleep` only as a fallback), **no** `timeBeginPeriod` | T1 | `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` fires on time; std's sleep overshoots by ~0.5–0.8 ms and a global timer-period change would tax every process |

Things measured and *left alone*: the viz bridge's ~0.45% (it's recv/send
syscalls) and a binary wire schema (the JSON seam is intact and
skip-zero-fields bought 35% for free). The September pass moved JSONL onto a
bounded worker after deliberately blocked-storage testing showed that this
could isolate T2 without making shutdown unbounded.

---

## 15. Testing philosophy

`cargo test --workspace` runs 462 passing tests (core 64, capture 124, viz 73,
ctl 55, analyze 141 + 4 integration, one doctest) with no mouse, admin rights,
Kafka, or browser, plus 11 Node tests over the viz page's engine when Node is
installed; CI runs the same on `windows-latest` with the toolchain pinned in
`rust-toolchain.toml`. The rule (CONVENTIONS.md) is that every
crate's *pure logic* is unit-tested and Win32/network side effects sit behind
thin traits or `#[cfg(windows)]` modules exercised manually.

- **core**: JSON round-trips, backward-compat of old files, the datagram
  budget test, `BatchView`/`EnvelopeView` byte-parity with the owned types,
  config rejection tables (including `[ctl]`), and the `localhost` rule
  (trusted hosts/origins, wildcard binds browse as loopback).
- **capture**: `DrainStamper` against a synthetic 10 MHz clock at 1 and 8 kHz;
  `ShipperCore` end-to-end with `mock::RecordingSink` / `FailingSink` (batch
  boundaries, drop attribution, marker sequencing); sandwich anchor math;
  histogram buckets; `T1Counters` layout assertion; config-reload diffing with
  a temp file; the real T3 thread stopping promptly. The Win32 test is the
  single `windows_reports_a_primary_screen_and_at_least_one_monitor`.
- **viz**: tag-probe acceptance/rejection cases, late-joiner session cache,
  lagging subscribers, config injection can't escape `<script>`, page is
  self-contained (the served page inlines `app.js` and references nothing),
  path-traversal rejection, rebound `Host` names refused on every route and
  foreign `Origin`s on `/ws`, a LAN peer refused everything but the overlay
  routes, the WebSocket client cap, the sessions-listing cache, `/healthz`
  reflecting the UDP bind, security headers on every response,
  `/api/session/{id}` streams with a length, recording time-range probing,
  latency percentile behaviour, the bundled demo recording is a valid
  envelope stream. **JS tests** (`crates/viz/js-tests/`, `node --test`,
  also run from `cargo test` when Node is installed): `harness.mjs` loads
  `app.js` into a V8 context with a stub DOM; `engine.test.mjs` checks unit
  conversion, unwrapped yaw and clamped pitch, seq-gap/ring-drop accounting,
  the live-buffer floor, the live-mode memory cap, backward seek through a
  checkpoint against integration from zero, marker delivery, session
  restarts, anchoring without a session, and OBS parameter clamping.
- **ctl**: see §19.4.
- **analyze**: **ground-truth synthetic streams** via `testutil::StreamBuilder`
  (`move_ms`, `tremor_ms(drift, amp, hz)`, `lift(...)`, `button(bits)`,
  `push_at_us` for out-of-order) — e.g. a 1500-count pull with a 100-count
  reversal must yield overshoot 0.0667; a 10 Hz tremor must dominate the
  8–12 Hz band; sparse grid must equal dense grid bit-for-bit; two-pointer
  matching must equal the old linear scan. `tests/report_pipeline.rs` writes
  a real JSONL fixture and checks the full load → build → render → JSON/CSV
  path plus the cache lifecycle (Missing → Hit → Stale). `benches/hot_math.rs`
  runs `prepare`, `savgol`, `welch`, `flick_detect` at 1M and 10M cells.

---

## 16. How to extend it (recipes)

**Add a game.** Add `[games."yourgame.exe"]` with `sens` and coefficients to
`telemouse.toml`. Key is the lowercase executable name (see `doctor` or the
`game` field in the stats line to learn it). Nothing else changes.

**Add a field to `RawEvent` or `Batch`.** Do it in `core` with
`#[serde(default)]` (and `skip_serializing_if` if it is usually zero) so old
recordings still load; add a `pre_*_still_parses` test; re-run
`full_batch_fits_in_udp_datagram` — if it fails, lower `MAX_EVENTS_PER_BATCH`.
Then populate it in capture (`decode_mouse` or `build_batch`), render it in
`index.html` (default it with `| 0`), and consume it in `load.rs`.

**Add a metric to the analyzer.** Write a module that takes `&Prepared` and
returns a serializable struct; call it from `report::build` inside a
`phases.lap("name")`; add it to `Report`; render it; add a `StreamBuilder`
test with a known answer; if it should show up in `trend`, add a column to
`TrendRow` (or just use `--metric your.path`). Bump `SCHEMA` if the JSON
shape changes incompatibly.

**Add a sink.** Implement `Sink` (`send` gets the already-serialized
`&str`); construct it in `cmd_run` behind a config flag; failures must return
`Err` (counted, rate-limited) rather than panic; add a `count_sink_error`
route in `stats.rs`.

**Add a stat tile to the page.** Add the tile in the stats bar markup, compute
the value in `paintStats` (10 Hz), and if it's a data-quality signal give it
an `alert` class threshold. For OBS, add its key to `OBS_HUD_ITEMS` in core
(validation) and to `paintHud`.

**Debug "no events".** `doctor` → is a mouse enumerated? Run with `--print`:
lines appear on movement? If not, check the stats line's `idle_for_s` and
whether `capture_alive` tripped. If events flow but the viz is blank, check
`udp_unreachable` in capture and `datagrams_per_s` / `parse_errors` in the
viz log or `/api/stats`.

---

## 17. Known rough edges

Observed during this read; none are correctness bugs in normal use.

- `markers::segment_reports` computes a displacement and discards it
  (`markers.rs:153-154`).
- `trend::report_for` assumes the file stem is the session id for the cache;
  a renamed recording is cached under the stem.
- The report cache invalidates on mtime, analyzer version, schema, and
  params — not on content hash.
- The hub caches only the latest `session` frame for late joiners; no batch
  backlog (by design, but worth knowing when a client connects mid-session).
- The plan's TimescaleDB/Parquet storage and aim/desk-space heatmaps are not
  implemented; `trend`/`--json-dir` are the intended substrate.
- The local servers have no authentication: the `Host`/`Origin` rule (§5.9)
  and the loopback default are the whole model. Binding the viz to
  `0.0.0.0` for a LAN OBS source exposes the live overlay (`/obs` + `/ws`)
  to that network; the dashboard and the recordings stay on this machine.
- `mouse.sessions` is created with the broker's defaults, not compacted
  (rskafka's `create_topic` takes no configs); compact it at the broker if
  session records must outlive the events' retention.
- The marker hotkey (`marker_hotkey`, F9 by default) is one chord with one
  fixed label; labelled markers come from a piped stdin — what the panel
  provides — see `capture/src/stdin_markers.rs`. A `[ctl] hotkey` equal to
  it is rejected at config load, since whichever registered second would
  silently lose.
- In-game hitching traced to the dashboard tab's GPU load and the panel's
  process scan, not to capture (§14); an input drop-out while dragging the
  OBS window is still unexplained.
- The performance leftovers ranked by value are in `docs/BENCHMARKS.md`
  ("What is left on the table" / "What is left on the live path") and the
  end of `docs/AUDIT-2026-08.md`. The `## [Unreleased]` section at the top
  of `CHANGELOG.md` lists what is in the tree but not yet in a tagged
  release; cutting a release renames it to the version.

---

## 18. Glossary

- **Count** — one unit of mouse movement, 1/CPI inch.
- **CPI / DPI** — counts per inch; the mouse's resolution.
- **Raw Input / `WM_INPUT`** — Windows API delivering HID reports
  pre-acceleration; `RIDEV_INPUTSINK` = receive even when not foreground.
- **QPC** — `QueryPerformanceCounter`, the monotonic high-resolution clock.
- **Anchor** — one (QPC, UTC) pair per session that maps QPC to wall time.
- **Coalesce window** — `batch.coalesce_ms` (default 8): T1 waits this long
  after a burst's first report, then drains every window + 1 ms on a timer.
- **Ring drops** — events lost because the SPSC ring was full (capture side).
- **Lost batches** — `seq_no` gaps seen by a consumer (transport side).
- **Pointer locked** — heuristic: cursor frozen while deltas flow ⇒ in-game.
- **Abs frames** — absolute-motion input frames (RDP/tablets), discarded.
- **Sens / coeff** — game sensitivity and engine degrees-per-count.
- **Flick** — a fast movement above 800 counts/s until it settles.
- **Overshoot ratio** — reversal distance after a flick ÷ flick amplitude.
- **Settle time** — ballistic end → 20 ms of stillness.
- **Savitzky–Golay** — least-squares polynomial smoothing/differentiation.
- **Run** — a contiguous span of the sparse 1 ms grid around activity.
- **Marker** — a labelled timestamp (the marker hotkey, a line on a piped stdin, config change, clock drift).
- **Envelope** — the tagged JSON record (`session` | `batch` | `marker`).

---

## 19. `crates/ctl` — the control panel (`telemouse-ctl`)

One process, one page (`http://127.0.0.1:7880`), for the person who would
otherwise juggle three terminals: start and stop the capture agent and the viz
server, run `doctor` / `analyze trend` / `analyze report`, read what they
print, and see — and kill — every telemouse process on the machine, whoever
started it. It is a launcher, not a shell: every component is a fixed binary
with fixed base arguments, plus flags from a per-component allow-list.

```powershell
cargo build --release --workspace     # the panel launches the binaries next to itself
target\release\telemouse-ctl.exe      # or: cargo run -p telemouse-ctl -- serve --bin-dir target\release
```

### 19.1 Modules

| File | Owns |
|---|---|
| `main.rs` | clap CLI (`serve --config --http --bin-dir --no-gui --no-webview --log-dir`), config load, tracing to stderr **and** `<log_dir>/ctl.log` (the console is hidden in tray mode, so the file is where the panel's own warnings live), 2-worker tokio runtime, the 500 ms reaper, the WebView2 data folder (`places::webview_data_dir`, created once, `None` → text view), Ctrl-C *or* the tray's Exit → `stop_all` before exit, then the GUI thread is joined. Binds the HTTP listener *before* `gui::spawn`, so the window's first navigation finds a live server. Warns if bound to a non-loopback address. |
| `gui/mod.rs` | `spawn(GuiDeps) -> Option<GuiHandle>`: wires the publisher task and the `ctl-gui` OS thread; `None` off Windows or with `--no-gui`. `GuiDeps` carries the panel URL, the WebView2 data folder and `no_webview`. `GuiHandle::shutdown` posts quit to the window (or the thread) and joins. |
| `gui/feed.rs` | Portable runtime side: `Snapshot`, `GuiLink` (channel, `panel_url`, `webview_data_dir`, `no_webview`), `run_publisher` (1 s while the window is visible, every 5th tick and no process scan while hidden, at once on `poke`), and the spawned `start`/`stop`/`new_session` actions (the last one stop-then-start-saving, guarded against re-entry). |
| `gui/model.rs` | Pure: `render_text` (the text view's body, CRLF, fixed columns), `WebStatus` + `web_banner` (the lines above it: "Loading the panel…", or why the page is not hosted and where it is instead), `window_text` (banner + body), `tooltip` (≤127 chars), `icon_state`, `menu` (start *or* stop per service, greyed when the binary is missing, *New session* while capture runs, the hotkey as accelerator text, *Open in browser*, the *Open …* items), `panel_url`, `icon_bitmap` (the disc, drawn at runtime — no `.ico`, no resource compiler). |
| `gui/win.rs` | `#[cfg(windows)]`: one top-level window (`telemouse`, 1120×760 scaled to DPI, minimum 720×520) that hosts the page through `webview.rs` with one read-only `EDIT` as the text fallback, `Shell_NotifyIcon`, the popup menu, `CreateIconIndirect` icons, `TaskbarCreated` re-add, the `RegisterHotKey` new-session chord with its `WM_HOTKEY` handler and tray balloon, `WM_DPICHANGED`, and the message loop. See §19.5. |
| `gui/webview.rs` | `#[cfg(windows)]`: the WebView2 host. `begin` probes the runtime and asks for an environment; two completion handlers (`on_environment`, `on_controller`) create the controller, tune the settings and navigate; `on_navigated` retries a refused connection; `timer` (watchdog, navigation retry), `resize`, `position_changed`, `set_visible`, `close`, and `fallback` (text view + banner, browser opened once). Handlers capture the window handle, never the state pointer. |
| `procs.rs` | `classify(name, cmd) -> Option<ProcKind>` — the *only* definition of "related" (`telemouse*.exe`, plus `cargo` whose command line names telemouse). `Scanner` takes one Toolhelp snapshot every 30 s and answers in between with per-PID queries (about 20 µs each) for the few matching names; a process whose handle cannot be opened (an elevated one) is judged alive or gone from the `OpenProcess` error, not from a table walk; `scan_cached(ttl)` serves a short cache; `kill` re-runs `classify` on the live process, refuses itself, and drops the cache. |
| `manager.rs` | The component catalogue (`COMPONENTS`), `ManagerConfig`, `StartRequest` validation (`arguments()`), spawning with piped stdout/stderr into a `LogSink` per component (a 400-line ring for the page and tray, plus `<log_dir>/<id>.log` so output survives a panel restart; rotated at 8 MB), `try_wait` reaping with exit accounting (`exits` / `unexpected_exits`; an exit nobody asked for is a `warn!` with component, pid, args, uptime and code), and the two-stage stop. |
| `server.rs` | axum router, the `Host` check on every request, the `X-Telemouse-Ctl` guard on every `POST`, JSON error bodies, the page with its injected config (`PageConfig`: the viz link, passed through `localhost::browse_addr_str` so a `0.0.0.0` viz bind still links to loopback, `stop_grace_secs`, the marker hotkey, the build's features and the absolute `Places`). `/api/state` carries `version`, `config` (path, found, seeded, status, mtime) and `places`, and accepts `?log_since=<n>` to send only new log lines. `POST /api/open` opens one of the places by name (§19.2). |
| `places.rs` | The absolute locations the panel talks about — version, panel URL, config, logs, binaries, docs, releases, and the WebView2 data folder (`webview_data_dir`: `%LOCALAPPDATA%\telemouse\WebView2`, else `%TEMP%\telemouse\WebView2`, never next to the config) — decided once at startup so the text view's header, the tray's *Open …* items, `/api/open` and the page cannot disagree. |
| `settings.rs` | The settings editor's pure half. `Settings` is the editable subset of `AppConfig` (`mouse_cpi`, `marker_hotkey`, `[recording] enabled`/`dir`, `[ctl] hotkey`, `[games]`, the `[viz.obs]` basics); `Patch` is the change request, and its `deny_unknown_fields` shape *is* the allow-list: bind addresses, `bin_dir`, `log_dir`, Kafka and `[batch]` cannot be expressed in it. `apply_patch` edits the file's text with `toml_edit` (values are replaced through the existing item so the comment lines above a key, its trailing comment, the ordering and unknown keys survive; a value that already means the same is left as written), `checked` parses the result back through `AppConfig::from_toml` + `validate` and turns the error into `{ field, reason }` without the file path, `token` is an FNV-1a fingerprint of the bytes, `write_atomic` is temp-file-plus-rename in the same directory, and `save` strings them together (token check → patch → validate → seed from the embedded sample when there is no file → write). `game_key` is a shape check on a user-typed exe name (trim, lowercase, `.exe` appended, no separators / control / reserved characters, ≤ 64 chars) — deliberately not a list of names. `changed` + `effects` say which groups moved and who only sees them later (`restart_required`: the tray's hotkey; `next_start`: a running capture or viz). Also owns `SAMPLE_CONFIG` and `seed_config`, which `main.rs` uses on first start. |
| `stats.rs` | (`observability`) Parses the capture agent's `capture stats` line into `ChildStats` for the card, the tooltip and `/api/state`; `parse_foreground_line` + `note_seen` keep the last 8 distinct programs the agent saw in front during the current run (`foreground_seen` on the capture component; never `-` or a `telemouse*` name), which is what the first-run guide offers as *Use <exe>*; `RecordingLive` is the recording's size and the disk's free space; a sink that drops, a ring overflow, an idle mouse or a viz nobody is listening for turns the tray icon amber. |
| `index.html` | Self-contained page (inline CSS and JS, system fonts, no external URLs — the server test rejects any): dark by default, light via `prefers-color-scheme` with a header toggle; a three-step first-run guide when this start seeded the config (`config.seeded`; CPI → game, picked from `foreground_seen` while a non-saving capture watches, or typed → a 30 s demo recording that ends in the report card; every step skippable, dismissal in `localStorage.tmFirstRunDismissed`, reachable again from *Settings → Run setup again*, plus a sample-recording offer when `demo-session.jsonl` is listed); *Session* (start/stop recording, save switch, elapsed and live numbers, a marker field over the marker route, the dashboard link), *Recordings*, *Tools* with the child's output in a panel, a *Report* card drawn from the analyzer's `ReportSummary` (`POST /api/reports/{id}` after the text report exits 0: a verdict on loss in words, six tiles, the marker labels and their offsets when the session has any, ten aim metrics with one-line explanations, the raw text under *Details*), a collapsed *Settings* form over `/api/config` (inline errors placed by the `field` of a 400, a notice built from `next_start` / `restart_required`), and a collapsed *Advanced* section (capture flags, the process table with force-stop, where things are with *Open* buttons over `/api/open`, the logs). Polls `/api/state?log_since=` + `/api/sessions` every 2 s (10 s while hidden), patches values in place, two-click kill (no modal dialogs). Works in the window and in a normal browser. |

### 19.2 The API

| Route | Effect |
|---|---|
| `GET /api/state` | `{ self_pid, now_unix_s, version, config, places, recording: { enabled, dir }, components: [ComponentState], processes: [ProcInfo] }`. `places` includes `webview_data`, the window's WebView2 cache folder (empty when there is no window or with `--no-webview`). `recording` is the config default; each component carries its run's `args`, `exits` / `unexpected_exits` since the panel started (a service that died without a stop, or a task that finished non-zero), and, for capture, `saving` (`recording_saves(config, args)`: `--no-record` / `--record` beat the config). Reaps exited children as a side effect. |
| `GET /api/sessions` | `*.jsonl` names in `recording.dir`, newest first (the report picker). |
| `POST /api/components/{id}/start` | body `{ flags: [..], session?: "x.jsonl", save?: bool }`. `save` is the capture card's switch: the server turns it into `--record` / `--no-record` against its own `recording.enabled` (`recording_flags`), so the page and the tray never derive the flag themselves; omitted = the config default. 404 unknown, 409 already running, 400 disallowed flag / bad session (including `save` on a component without the switch), 500 spawn failure (binary missing). |
| `POST /api/components/{id}/stop` | body `{ force?: bool }` → `{ outcome: "graceful" \| "terminated" }`. 409 if not running. |
| `POST /api/components/{id}/marker` | body `{ label }` → `{ ok, label }`. Writes `label\n` to the child's stdin; only components with `markers: true` (capture) are started with a pipe. 400 for a blank, multi-line or >120-character label or a component without a pipe, 409 if not running, 500 if the write fails. Echoed as `--- marker: <label> ---` in the component log. |
| `POST /api/processes/{pid}/kill` | 403 for this panel or an unrelated process, 404 unknown, 500 if the OS refuses. |
| `GET /api/config` | `{ path, status, exists, token, settings, error, choices, not_editable }`: the editable settings as the file on disk has them now (paths as written, not resolved), the token a save must echo, and the OBS vocabularies for the form. A file that cannot be used gives `settings: null` and `error`; no file gives the shipped sample's values and `token: "none"`. |
| `POST /api/config` | body `{ token, patch }` → `{ ok, token, created, settings, changed, restart_required, next_start }`. See `settings.rs` in §19.1 for the pipeline. 400 `{ error, field }` when the patched file would not load (nothing is written) or the body is not a `Patch` (which is how a non-editable key is refused); 409 `{ error, token }` when the file is not the one `token` was taken from; 500 when the write fails. Afterwards the manager re-reads the file at once (`reload_config`), so `recording` in `/api/state` and the next start follow; children read the file when they start. |
| `GET /api/reports/{id}` | The stored `ReportSummary` for that recording when one exists, is not older than the recording, and carries `manager::SUMMARY_SCHEMA` (the analyzer's tag, repeated in ctl because the panel only spawns the exe); 404 otherwise. A finished recording never changes again, so the schema test is what retires a summary cached before the shape grew a field. `id` must pass `is_safe_id` (400). Runs nothing. |
| `POST /api/reports/{id}` | Runs `telemouse-analyze report <recordings>/<id>.jsonl --summary --json-dir <recordings>/.reports` (fixed arguments, one at a time, 5 min cap), returns the JSON it prints and keeps it as `.reports/<id>.summary.json`. The `report` and `trend` tasks get the same `--json-dir` (`Component::report_cache`), so after a text report this is a cache hit. 400 bad id, 404 no such recording, 503 when `telemouse-analyze` is not there (the minimal zip), 500 when the run fails. |
| `POST /api/open` | body `{ target: "config" \| "recordings" \| "logs" \| "docs" }` → `{ ok, target, path }`. Opens that place with the shell's default handler (`gui::open_url` in `spawn_blocking`): the config in an editor, a folder in Explorer, the docs. The target is an allow-listed name mapped to a `Places` string on the server, never a client path. 400 unknown target, 404 when this build has no such place (logs without the `logging` feature), 500 if the shell refused. The page's first-run banner and its *Open* buttons use it; before it, the page could only show paths for the user to copy. |

Every `POST` without `X-Telemouse-Ctl: 1` is a 403. A browser only adds a
custom header to a cross-origin request after a CORS preflight, which this
server never answers, so a stray web page cannot reach the panel via
`localhost`. DNS rebinding would make such a page same-origin, so every
request (GET included — `/api/state` returns child command lines and logs) is
also refused with 403 unless its `Host` is an IP literal, `localhost`, or
`*.localhost` (`telemouse_core::localhost`, shared with viz, which applies the
same rule and an `Origin` check on `/ws`). That plus the loopback bind is the
whole security model — the panel is a local tool that can terminate processes,
and is documented as such.

### 19.3 How a stop works

Children are spawned with `CREATE_NEW_PROCESS_GROUP`, which does two things:
the panel's own Ctrl-C no longer reaches them (so `main.rs` stops them itself
on the way out), and `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` reaches
*exactly* that child. The capture agent's `ctrlc` handler treats Ctrl-Break
like Ctrl-C: partial batch flushed, sinks closed in order, Kafka drained with
a bound. The manager polls `try_wait` every 100 ms for `ctl.stop_grace_secs`,
then calls `Child::kill` (TerminateProcess). If the panel has no console
(started detached), the event cannot be delivered and it terminates at once.
A process that reports `STATUS_CONTROL_C_EXIT` (`-1073741510`) is shown as
"exited on Ctrl-Break", not as a failure.

### 19.4 Tests (`cargo test -p telemouse-ctl`, 55 tests)

- `gui::model`: icon follows capture only; uptime formatting; tooltip names
  both services and stays under the `szTip` bound; the log focus prefers a
  running service, then recency; the text is CRLF-only and carries every
  section (including the hotkey line); the menu offers start *or* stop per
  service, greys a missing binary, offers *New session* only while capture
  runs, and puts the chord on the item the hotkey is equivalent to;
  `panel_url` swaps an unspecified bind address for loopback; the icon is an
  opaque disc with transparent, masked corners; `web_banner` is empty while
  hosted, names the reason and the URL in the fallback, and omits the
  "install the runtime" line for `--no-webview`.
- `places`: `webview_data_dir_in` prefers `LOCALAPPDATA`, falls back to
  `TEMP`, and is `None` when both are unset or empty.
- `gui::feed`: a visible publisher delivers components, processes, the
  hotkey label and a wake; a hidden one skips the process scan and answers
  a poke at once; the publisher exits when its receiver is dropped; tray
  start/stop over an empty `bin_dir` are refused cleanly and still poke;
  `new_session` holds its guard while in flight, releases it and pokes when
  done, and drops a press made while one is in flight.

- `procs`: the `classify` vocabulary (case-insensitive names, cargo only
  when it names telemouse, unrelated names invisible); a live scan keeps its
  ordering invariant and marks `is_self`; `kill` refuses self, refuses PID 4
  (Windows System) / PID 1, and reports unknown PIDs.
- `manager`: a real child (`cmd /C echo … && ping`, or `sh -c` elsewhere)
  is started, shows as running with its PID, has its stdout captured, refuses
  a second start, is terminated, and records its exit; graceful stop always
  ends the child within grace+slack; the allow-list and the session rules
  (`..`, separators, non-`.jsonl`, missing file) are enforced by
  `arguments()`; unknown components and missing binaries are errors, not
  panics; session listing is newest-first and `.jsonl`-only; `resolve_bin`
  honours `bin_dir`; the log ring is bounded; the catalogue's binaries are all
  names `classify` recognises.
- `server`: page is self-contained and carries the injected config; the
  config cannot break out of its `<script>`; `/api/state` has the documented
  shape; every mutating route is 403 without the header; start/stop map
  manager errors to statuses (using an empty `bin_dir`, so tests never launch
  a real agent); kill refuses self and unrelated PIDs; `/api/open` is 403
  without the header, 400 for an unknown target and 404 over an empty
  `Places`.
- `settings`: an empty patch (or one that says what the file says) returns
  the text byte for byte; changed values keep every comment, the ordering
  and unknown keys; games are added, updated and removed (inline tables
  too); missing sections are created without empty `[games]` / `[viz]`
  headers; the patch shape refuses every non-editable key; `game_key` is a
  shape rule; errors name their field and drop the file path; a string
  cannot smuggle TOML; `save` checks the token, writes nothing on an invalid
  result, leaves no temp file, and creates a missing file from the sample.
  `server` drives the same through `/api/config` (403 / 400 / 409, comments
  kept, the panel's own `recording` refreshed) and `/api/reports/{id}` (403
  without the guard on `POST`, 400 for unsafe ids, 404, 503 without an
  analyzer, a stored summary served only when not older than the recording).
- `core::config`: the `[ctl]` section parses, defaults, and rejects
  `stop_grace_secs > 60` and an unparsable `hotkey`; `core::hotkey`: chords
  parse case- and space-insensitively, named and function keys map to their
  VK codes, typing keys need a modifier, `""`/`none`/`off` disable, and
  errors name the offending part.

### 19.5 The native window and the tray (Windows)

`telemouse-ctl serve` shows a native window and a tray icon unless
`--no-gui` is given (off Windows there is never one). The window, titled
*telemouse* (1120×760 scaled to the monitor's DPI, minimum 720×520, a plain
`WS_OVERLAPPEDWINDOW`), hosts the control panel page itself through
**WebView2**, the browser engine that ships with Windows 10 and 11, so a
normal start opens no browser tab. Under the page sits one read-only
monospace `EDIT` control, the **text view**, which is what the window shows
while the page is loading and whenever it cannot be hosted. A
`Shell_NotifyIcon` icon and a popup menu complete it.

**The WebView2 sequence** (`gui/webview.rs`). COM is initialised as an STA
on the `ctl-gui` thread, since WebView2 is single-threaded COM whose
callbacks arrive as posted messages on the creating thread — the existing
`GetMessageW` loop is the only loop involved. After the window is on screen
`begin` probes the runtime (`GetAvailableCoreWebView2BrowserVersionString`;
the version is logged) and asks for an environment with the user-data
folder from `GuiLink`; the environment's completion handler asks it for a
controller parented to the window; the controller's handler paints the
page's own background colour (no white flash), sizes it to the client
rectangle, turns off context menus, zoom, the status bar, host objects, web
messages and the built-in error page (dev tools only in debug builds),
routes every `NewWindowRequested` (the dashboard, the docs, the releases —
every `target="_blank"`) to the system browser through `ShellExecuteW`,
navigates to the panel URL, and hides the text view. Each handler captures
only the window handle and reaches the state through `GWLP_USERDATA`, the
same rule as `wndproc`: a callback that fires after the window is gone
finds nothing and returns. `WM_SIZE` calls `SetBounds`, `WM_MOVE` /
`WM_WINDOWPOSCHANGED` call `NotifyParentWindowPositionChanged`, and
`WM_DPICHANGED` applies the suggested rectangle (this also fixes the text
view, which ignored DPI changes before). Hiding the window to the tray
calls `SetIsVisible(false)`: a hidden WebView2 stops rendering and the
page's document reports `hidden`, so it falls to its 10 s poll and the
window costs nothing while a game is in front.

**The fallback.** Every failure ends in `webview::fallback`: a runtime that
is not installed, an environment or controller that fails, a watchdog timer
that fires because neither answered within 10 s, or a navigation that is
still refused after six retries half a second apart. The partial COM
objects are released, the text view is shown with `model::web_banner` above
`render_text` ("The panel page is not shown here: <reason>. It is open in
your browser at <url> — tray → Open in browser reopens it. Install the
WebView2 Runtime from Microsoft to see the panel in this window."), the
page is opened in the default browser once, and the reason is a `warn!`.
`--no-webview` starts in that state on purpose — tray and text view, no
Edge components loaded, no browser opened, an `info!` instead. To simulate
a missing runtime on a machine that has one, set
`WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=C:\nope` before starting. Nothing here
panics and nothing blocks the thread.

**What the text view shows.** `gui::model::window_text` over a `Snapshot`:
the banner, then every component (label, kind, running/stopped/not built,
pid, uptime, how the last run exited, summary), the related-processes table
(pid, kind, name, CPU %, working set, "(this panel)"), and the log tail of
the *focus* component — the running capture agent, else the running viz
server, else whatever started or exited last. It is re-rendered only while
it is showing. The tray icon is a grey disc while idle and a green one
while capture runs (drawn at runtime by `icon_bitmap` and
`CreateIconIndirect`; there is no `.ico` and no resource compiler); its
tooltip names both services with uptime.

**The user-data folder.** WebView2 writes a cache, cookies and lock files,
tens of megabytes, so it never goes next to the config (the unzip folder
may be read-only, synced or on a share): `places::webview_data_dir` picks
`%LOCALAPPDATA%\telemouse\WebView2`, else `%TEMP%\telemouse\WebView2`;
`main.rs` creates it once and hands it to `Places` (as `webview_data`, shown
in the page's *Where things are* and in `/api/state`) and to `GuiDeps`. No
writable folder means the text view. It is the one thing telemouse writes
outside its own folder, and the removal instructions in `docs/HELP.md` name it.

**What the tray does.** Left click toggles the window; right click opens
the menu: *Show/Hide window*, *Start capture (save data → dir)* and *Start
capture (don't save)* **or** *Stop capture (saving data | not saving)*
(starts greyed when the binary is missing; the save choice becomes
`--record` / `--no-record` only when it differs from `[recording] enabled`),
*New session (restart capture, save data → dir)* while capture runs, the
same start/stop for the viz server, *Open in browser* (`ShellExecuteW` on
the panel URL, loopback if the bind address was unspecified — the same page
the window shows), *Open logs folder*, *Open recordings folder*, *Edit
telemouse.toml*, *Open docs*, and *Exit*. Closing or minimising the window
hides it to the tray; Shift+close, or the tray's Exit, quits — through the
same graceful path as Ctrl-C (`stop_all`, then the icon goes away).
Doctor, the analyzers and process kills are on the page, which is now in
the window.

**Anti-cheat posture.** WebView2 runs as Microsoft-signed
`msedgewebview2.exe` child processes that render into this ordinary
window. The panel opens no handle on any other process, hooks nothing and
draws nothing over a game; `procs.rs` lists processes by their `telemouse*`
stem, so the WebView2 children never appear in the process table and cannot
become kill targets.

**The new-session hotkey.** `[ctl] hotkey` (default `ctrl+alt+r`, `""` for
none; grammar in `core::hotkey`, rejected at config load) is registered
system-wide with `RegisterHotKey` on the panel window, so it works from
inside a game without alt-tabbing. Pressing it runs `feed::new_session`:
stop the capture agent if it is running (gracefully, so the file it was
writing is complete), then start one with `save = true` — a fresh
`recordings/<session>.jsonl` whatever the config default says. Presses
during the stop's grace period are dropped (`GuiLink::restarting`), and a
balloon on the tray icon confirms the restart, since the icon is green both
before and after. The same action is the *New session* menu item, and the
menu item the hotkey is equivalent to shows the chord as its accelerator
text. The message is `WM_HOTKEY`, which only the input system generates for
a registered chord — unlike a posted `WM_COMMAND` it cannot be forged by
another process. If another program already owns the chord the panel warns
and runs without it; pick another in `telemouse.toml`.

**How it is wired.** The Win32 message loop must own the window's thread,
so `gui::spawn` starts an OS thread named `ctl-gui` (`gui/win.rs`, the same
`GWLP_USERDATA` + `PostMessageW` idiom as the capture thread) and a
publisher task on the tokio runtime (`gui/feed.rs`). The publisher
snapshots the manager and the process table once a second while the window
is visible (`snapshot(40)` + `scan_cached(2 s)`), only every fifth tick and
without a process scan while it is hidden, and at once when poked (after an
action, on show); it hands each `Snapshot` over a `watch` channel and posts
`WM_APP_REFRESH`. The UI thread never blocks on the runtime: it reads
`rx.borrow()`, and its actions are `Handle::spawn`ed futures whose outcome
shows up in the next snapshot, exactly like the web page's. While the text
view is showing it is re-rendered from the snapshot with the scroll
position preserved (while the page is hosted the snapshot only feeds the
icon); the icon is `NIM_MODIFY`ed only when its state or tooltip changed.
Explorer restarts are handled by re-adding the icon on the `TaskbarCreated`
message. Teardown releases the WebView2 controller before `DestroyWindow`
and calls `CoUninitialize` last. Every failure — no page, no window, no
tray — is a `warn!` and a headless server, never a panic; if the tray
cannot be added the window stays up and close means exit, so nobody is
stranded.

**Why the console stays.** Graceful stop is `GenerateConsoleCtrlEvent`,
which needs the panel and its children to share a console, and a child of a
console-less parent would open its own console window. So the binary keeps
the console subsystem. Started from Explorer or a shortcut the panel is the
only process on its console (`GetConsoleProcessList` says 1) and hides the
console *window* (`ShowWindow(GetConsoleWindow(), SW_HIDE)`); started from
a terminal it leaves it alone, so logs stay in view.

**Manual checks.** `cargo run -p telemouse-ctl -- serve --bin-dir
target\debug`: the window shows the dark panel within about a second
(`ctl.log` has the runtime version line and "panel page hosted in the
window") + grey icon; right-click → *Start capture* → green within a second
and the page agrees; *Stop capture* → grey, "stopped" on the page and
"exited: Ctrl-Break" in the text view's log tail; close → hidden, icon
click → back; *open dashboard* → the system browser, not a second window;
`--no-webview` → text view, no `msedgewebview2.exe` children;
`$env:WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = "C:\nope"` → text view with the
banner and the browser opened once; *Exit* → children stopped, icon gone. Menu choices are the return value of
`TrackPopupMenu` (`TPM_RETURNCMD`), not `WM_COMMAND`s — a posted
`WM_COMMAND` from another process is ignored, since it could otherwise start
a capture or exit the panel. To script the same loop, use the HTTP API the
page uses (`Invoke-RestMethod -Method Post -Headers @{'X-Telemouse-Ctl'='1'}
http://127.0.0.1:7880/api/components/capture/start`, then `/stop`) and watch
the icon follow; `WM_CLOSE` (0x10) posted to the window still hides it.
