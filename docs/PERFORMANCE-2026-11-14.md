# Performance audit — 2026-11-14

A whole-workspace pass: the analyzer and the live stack re-measured without
a profiler, plus a code review of the parts the October hotpath pass
(`docs/PERFORMANCE-2026-10-12.md`) never looked at: viz and its page, ctl
and its page, mcp, and the uninstrumented capture threads. Nothing was
changed. None of A1–A6/C1–C2 from October has landed, so that list is
re-validated here rather than repeated.

Same box as before (Ryzen 9 5950X, Windows 10 19045, Rust 1.98), `release`
profile, default features as of the morning's tree (capture still had
`kafka` compiled in), no hotpath.

**How to read the findings.** "Measured" means a number from today's runs.
"From code" means the mechanism was read in the source and its size is an
estimate; those need a measurement before and after any fix.

## Headlines

1. **Nothing regressed, and the live stack is cheaper than the last
   cycle-count baseline.** 1.28% of one core at 1 kHz with one WebSocket
   client and ctl polled (1.62% on 2026-09-20), 0.17% idle (unchanged).
2. **The capture agent has no mechanism that can hitch a game.** No
   `timeBeginPeriod`, no fsync, no lock shared with T1, wake counts
   independent of polling rate. The open questions there are unmeasured
   states (Kafka with a live broker, a true 8 kHz mouse), not defects.
3. **The analyzer's wall time is still `prepare → kinematics`**, and the
   cheapest large win is one October missed: the six kinematics summaries
   run one after another on the critical thread.
4. **Every cache consumer parses a full report to produce a few KB.** A warm
   `trend` reads 347 MB of JSON for 41 rows (0.5 s); `report --summary` on a
   cache hit takes 115 ms. The same path serves the MCP tools and the
   panel's report card.
5. **The browser is the least-measured part and the one with history.** The
   dashboard's trail draw scales linearly with polling rate with no
   sub-pixel decimation, and a hidden dashboard tab keeps its socket.
6. **ctl is near-free when hidden to the tray, but a window left open
   behind the game counts as visible**, and a refused WebView2 suspend is
   never retried or logged.

## 1. Measured: analyzer

Three runs each, wall / total CPU / peak working set, no report cache unless
stated.

| Run | wall | CPU | peak RSS |
|---|---|---|---|
| `report` 1,155 MB (`s-20260909-015040-c330`) | 3.26–3.38 s | 8.6–9.7 s | 3.2 GB |
| `report` 137 MB (`s-20260923-065348-7702`) | 0.51–0.60 s | 1.2–1.5 s | 512 MB |
| `report` 2.6 MB (`s-20260922-225752-d8c6`) | 30–35 ms | — | — |
| `report` 1,155 MB, cache hit | 97–102 ms | 94 ms | 77–86 MB |
| `report --summary` 1,155 MB, cache hit | 112–128 ms | 109–125 ms | 77–86 MB |
| `trend`, cold, 41 recordings / 5.2 GB | 17.3 s | 50.7 s | 3.2 GB |
| `trend`, warm (347 MB of cached reports) | 0.47–0.61 s | 0.6–0.8 s | 87 MB |
| `list`, warm index | 11 ms | — | — |

Phases on the 1,155 MB report: load 0.42 s, prepare 1.11 s, kinematics
1.58 s, micro 0.55 s, quality 0.34 s; build wall 2.69 s, load + build
3.12 s, process total 3.37 s. October's profiled figures (3.97 s wall,
kinematics 1.84 s) were inflated by the sampler as that doc suspected; the
shape is the same.

Two October readings are corrected by these runs:

- **`list` is not 6 ms per cache hit.** 41 hits take 11 ms in total. The
  225 ms attributed to hits in October was the index rewrite and the extra
  signatures forced by its two misses.
- **Cold `trend` uses 2.9 cores on a 16-core box** (50.7 s CPU in 17.3 s).
  The byte budget is the cause, as October said, with one addition below
  (A6).

## 2. Measured: live stack

`tools/cpubench/bench.ps1`, cycle-exact, window 25 ms / coalesce 8 ms, one
WebSocket drain, ctl with its WebView2 window polled every 2 s, recording
on, Kafka off. Percent of one core.

| | capture | viz | ctl | total | 2026-09-20 total |
|---|---|---|---|---|---|
| idle (10 s) | 0.049 | 0.024 | 0.097 | 0.170 | 0.169 |
| 1 kHz inject (30 s) | 0.708 | 0.446 | 0.128 | 1.282 | 1.624 |
| 8 kHz requested, 3.3 kHz delivered | 0.950 | 0.473 | 0.130 | 1.553 | 1.869 |

Capture threads at 1 kHz: T1 0.315, shipping 0.277, context 0.072, JSONL
writer 0.044. At 3.3 kHz T1 rises to 0.522 and shipping to 0.302; only T1
scales with report rate.

Both runs: 0 ring drops, ring high-water 31 and 126 of 65,536, 0 JSONL
drops, 0 seq gaps at viz, flush max 105 µs. Capture→ship p50 29.5 ms, p99
34.5 ms; viz agreed (30.0 / 35.0). That is the 25 ms window plus the
coalesce back-dating, by design (C1 in October).

**C2 is confirmed and worse than October saw it.** With synthetic motion
over an IDE window the pointer-lock detector flipped to `true` and back
about every two seconds: 14 false locks (28 transitions) in 30 s at 1 kHz,
each transition a log line and a batch with the locked flag.

## 3. Analyzer findings

October's list against the code, then what it missed. Constraint on every
parallel change: `parallel_and_sequential_builds_have_identical_results`
compares the two builds' JSON exactly, so per-chunk results must be
concatenated in order.

| | Verdict | Note |
|---|---|---|
| A1 fuse kinematics summaries | Confirmed, smaller | Memory halves (859 → ~430 MB), not two thirds: three count-space vectors must stay for the quantiles. The degree shortcut must stay conditional on `!locked_only && |kx| == |ky|`; an unconditional one was removed once as a bug (`kinematics.rs:498`). `Summary::of_vec` is shared, so it needs a "known finite" constructor, not a deleted `retain`. |
| A2 parallel smoothing + scratch reuse | Confirmed | `prev_last_move` (`series.rs:1234-1239`) chains across runs and needs a serial post-pass. Split by cumulative cells, not run count. Expect prepare 1.1 → ~0.6 s. |
| A3 derivatives in prepare | **Do not do as written** | It relocates the sweeps, and storing four derivative lanes adds ~1 GB. The local equivalent is one fused sweep for d1 and d2 over the same 7-tap window. |
| A4 deserialize into the body | Confirmed for churn only | Peak RSS is not in the loader: the 3.2 GB is the grid (1.79 GB) plus kinematics vectors (0.89 GB) plus events during `compute_metrics`. A4 saves allocator time, not peak, and does not help A6. Needs rollback of `Body::events` on a bad line. |
| A5 per-second table out of the cache | Confirmed | Removing it outright breaks `--csv-dir`, `--json` and `--metric per_second.*` on a cache hit. Do N5 first. |
| A6 trend byte budget | Confirmed, plus a scheduling flaw | Workers take strictly the next file; when it does not fit, every idle worker waits even though later small files would fit. Skip-ahead to the first file that fits is ~15 lines and safe (results land in indexed slots). |

New, in order of expected gain:

- **N1 — Run the six kinematics summaries in parallel** (`kinematics.rs:135-137,
  169-175`). Each `Summary::of_vec` owns an independent 18.6 M-sample
  vector and they run back to back on the wall-critical thread. Scoped
  threads behind the existing `parallel` flag, bit-identical, ~20 lines.
  Estimated −0.7 s of the 3.3 s report. From code.
- **N5 — Stop parsing whole reports for headlines** (`trend.rs:99-110`,
  `report/mod.rs:502`, `atomic.rs:47`). Measured cost: 0.5 s per warm
  `trend`, 115 ms per `--summary`. Deserialize a slim mirror struct that
  skips the row tables on the trend-row and summary paths, write compact
  rather than pretty JSON, and drop the `sync_all` on a disposable cache.
  No schema change. Version and params are also checked only after the full
  parse, so a version bump parses every stale file just to reject it.
- **N3 — Quality sorts 15 M `f64` intervals for three order statistics**
  (`quality.rs:482-505`), after two filter passes and a histogram pass over
  the same vector. The values are integer µs: one fused pass plus
  `select_nth` or a counting histogram gives the same numbers. 0.34 s →
  under 0.1 s and −122 MB. Hidden under kinematics today; it is CPU in
  `trend` and becomes co-critical once N1/A1 land.
- **N4 — Metric groups run serially below 250,000 events**
  (`report/mod.rs:161`). The typical 10–20 minute session is under that.
  Lowering the threshold to ~25 k should take 15–25% off small reports.
  From code; measure.
- **N2 — 24 bytes per grid cell are avoidable** (`series.rs:297,301,306`):
  `clicks` and `last_move` are near-empty `u32` lanes (256 MB at the cap)
  and `speed_raw` is `mag(vx, vy)`. −256 to −512 MB of peak. This, not A4,
  is the lever on the 3.2 GB.
- **N6 — `--metric` converts the whole report to a `serde_json::Value` per
  session** (`trend.rs:262`). Only on that flag and the MCP trend's
  metrics.
- **N7 — A cold `list` scans one file at a time on one thread**
  (`load/mod.rs:487-549`). A first run or an index-version bump pays 5.2 GB
  at single-thread speed.

Checked and fine: the 64-bit `qpc_to_utc_us` fast path exists and is what
prepare calls; no thread spawn on small files; no clones of events or grid
lanes; flicks, lifts, clicks, markers and per-minute are linear.

## 4. Capture and core findings

In good shape. Per-batch allocations are three (the JSONL line, the Kafka
key and payload); no `tracing` call sits on a per-report or per-batch path;
only T1 is raised, to above-normal.

- **K1 — Kafka linger equals the batch window** (`main.rs:410-414`,
  `sinks/kafka.rs:347-350`). Batches arrive every 25 ms and the linger is
  25 ms, so a healthy broker gets roughly one produce RPC per batch, not
  the "couple of RPCs rather than forty" the module doc promises. Each is a
  zstd pass, a LAN send and two wakes. Estimated 0.3–0.5% of a core, which
  would make it the most expensive thread in the process. **Never
  measured**: September ran with Kafka off, October with no broker. Fix: a
  250–500 ms linger (Kafka is not the live path). Measure the
  `telemouse-kafka` thread with cpubench before and after.
- **K2 — T2's window sleep is `park_timeout`** (`shipping.rs:750`), which
  follows the system timer tick. Every measurement so far was taken with a
  browser holding a 1 ms tick. At the default 15.6 ms tick the 25 ms grid
  would wake up to 15 ms late: irregular batches and oldest-event latency
  up to ~50 ms. Cadence, not CPU. Suspicion; one run on a quiet desktop
  settles it. Fix if real: a T2-owned high-resolution waitable timer, as T1
  already has.
- **K3 — Raw Input listener cost at a true 8 kHz is unmeasured.**
  `SendInput` tops out at 3.3 kHz, so the "8 kHz" row above is not one.
  `RIDEV_INPUTSINK` makes win32k queue every report to T1 as well as the
  game; that work is outside the process and no code change reduces it. One
  pass with a real 8 kHz mouse, with and without telemouse.
- **K4 — Log writes are synchronous on T2 and T3**
  (`core/src/logging.rs:170-187`), and the queue-full warning is logged
  while holding the queue mutex (`sinks/queue.rs:124`). If stderr stops
  draining (QuickEdit selection on a hand-run agent, a hung ctl), T3 blocks
  and T2 blocks at its next warning. Low probability under ctl.
- **K5 — Degraded-mode bug** (`raw_input.rs:1063-1078`): after a failed
  `SetWaitableTimer` the live path waits on the unarmed timer, so drains
  come every ~68 ms instead of 9 and overflow the 256-report buffer at
  8 kHz. Treat the timer as absent after an arm failure.
- Small: the JSONL writer is woken once per line (40/s; a 100–250 ms
  drain loop saves ~0.04%); T1 does a 128-bit divide, a counter store and a
  wake per report that could be per drain (~0.03% at 8 kHz); the stats
  histograms are snapshotted twice per 5 s report.

Still open from September and unchanged: skipping the UDP send when no viz
is listening (L7).

## 5. viz findings

The Rust bridge has nothing worth changing: no re-serialisation, refcount
fan-out, lagging clients disconnected, replay streamed in 256 KiB chunks.
Everything is in `app.js`, which no pass has measured in a browser.

- **V1 — Trail draw scales with polling rate** (`app.js:1754-1801`). Every
  event inside the decay window becomes its own `moveTo`/`lineTo` subpath
  with round caps, per panel, per frame. 6 k segments per frame at 1 kHz;
  ~48 k at 8 kHz, almost all sub-pixel. Fix: skip a point under ~0.75 px
  from the last emitted one and join consecutive segments in a bucket into
  polylines. Mechanism certain; the per-frame cost needs a DevTools trace.
- **V2 — The 98 bucket arrays are cleared with `length = 0` and refilled
  every frame** (`app.js:1757,1776`): an estimated 0.5 MB of garbage per
  frame at 1 kHz. A preallocated `Float32Array` per panel removes it. From
  code; confirm with an allocation timeline.
- **V3 — A hidden dashboard tab keeps its WebSocket** and keeps parsing
  every batch it will later discard. ~0.3% of a core in viz per client plus
  40 renderer wakes/s, in exactly the tab-left-open-while-gaming state.
  Close the socket a few seconds after `visibilitychange` to hidden
  (dashboard only, not `/obs`).
- **V4 — The "event age" tile repaints at 10 Hz forever once the feed is
  idle** (`app.js:3009-3012`); `latencyValue` already switches to "idle".
- **V5 — The rAF loop never parks** on a visible idle page (240 callbacks/s
  on this display).
- **V6 — `desynchronized: true` on every canvas** (`app.js:1574`) can put
  the canvas on a low-latency presentation path. Unproven, but given the
  hitching history it deserves a `?desync=0` switch and one A/B session
  with a frame-time overlay.

## 6. ctl and mcp findings

The three earlier fixes are present and effective: one Toolhelp snapshot at
most every 30 s and none while hidden, WebView2 sleep → hide → `TrySuspend`
in the right order, EDIT repaint gated by `worth_painting`.

- **T1 — A window left open behind the game is treated as visible**
  (`gui/win.rs:948-950,1093-1096`). Only close and minimise-to-tray reach
  the cheap state; there is no deactivation handling and `SIZE_MINIMIZED`
  does not touch the webview. While "visible" the page polls two routes
  every 2 s and forces a composited frame per second. Fix: treat minimised
  as hidden, and on deactivation consult `SHQueryUserNotificationState`
  (a shell query, no handle on the game) to enter the hidden power path.
- **T2 — A refused `TrySuspend` is never retried and never logged at
  `info`** (`gui/model.rs:387`, `gui/webview.rs:502-562`). The page stops
  polling, so CPU is fine, but ~300 MiB of WebView2 can stay resident all
  session and `ctl.log` cannot say whether it did. A one-shot retry timer
  and one `info!` line.
- **T3 — MCP `session_summary` and `trend` take the N5 path** on every call
  (`mcp/src/analysis.rs:140-151`), and ctl's
  `recordings/.reports/<id>.summary.json` is never read by mcp.
- **T4 — MCP analysis has no concurrency cap** (`mcp/src/server.rs:453-463`).
  Models issue tool calls in parallel; N cold summaries is N × 16 load
  threads and N × hundreds of MB at normal priority. One semaphore permit,
  as ctl's `summary_lock` already does.
- **T5 — The panel's Report button runs the analyzer twice**
  (`panel.js:241-251,711,789-791`): the text report, then always a POST
  that spawns `report --summary`. `GET /api/reports/{id}` is never called.
  Analyzer children also start at normal priority; `BELOW_NORMAL` for
  analyzer tasks only would keep a mid-game report off the game's cores.
- Small: `/api/sessions` is refetched on every 2 s poll; the publisher
  builds a 40-line snapshot each second that nothing reads while the page
  is hosted; the hidden tray tooltip carries uptime, so Explorer gets a
  `NIM_MODIFY` every 5 s.

## What to do, in order

1. **N1**, then **A1** and **A2**: the 1,155 MB report from ~3.3 s toward
   ~1.7 s, with kinematics memory halved.
2. **N5**, with **T3** and **T5** riding on it: warm `trend`, `--summary`,
   the MCP tools and the report card all get the same fix.
3. **K1**: measure the Kafka thread against the live broker, then raise the
   linger. This machine's config has Kafka on, so it pays this today.
4. **V1–V3**: the dashboard at high polling rates and when hidden.
5. **T1** and **T2**: ctl's cost when the window is merely behind the game.
6. **A6** with skip-ahead, **N3**, **N4**, **N2**.
7. **C2** (pointer-lock one-tick hold), **K5**, **T4**.

Measure before deciding: K2 (quiet desktop), K3 (real 8 kHz mouse), V6.
Drop: A3 as written. Defer: A4 and A5 until N2 and N5 are in.

## Caveats

- The static findings were produced by four parallel code reviews; the
  top items in each area were re-read against the source, the rest were
  not individually re-verified. Every "from code" magnitude is an estimate.
- Another session was editing and testing the tree during this pass (the
  `kafka` default changed part-way). The binaries measured were built
  before that, and a concurrent `cargo test` may have overlapped the bench
  runs; cycle counts are per process, so the effect should be small.
- The 8 kHz row is 3.3 kHz of `SendInput`. No game was running.
- No browser was measured: the bench's WebSocket client is a drain, not a
  renderer.

## Reproducing

```powershell
# analyzer (no cache, then cold and warm trend into a scratch dir)
.\target\release\telemouse-analyze.exe report s-20260909-015040-c330 --quiet --timing
.\target\release\telemouse-analyze.exe trend --json-dir <scratch>\reports
# live stack; never with a game open (the load phase uses SendInput)
cd tools\cpubench; cargo build --release
.\bench.ps1 -Label hz1000 -BinDir ..\..\target\release
.\bench.ps1 -Label hz8000 -BinDir ..\..\target\release -Hz 8000
```

Today's rows are `audit1114-hz1000` and `audit1114-hz8000` in
`tools/cpubench/results.jsonl`.
