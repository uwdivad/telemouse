# Performance survey — 2026-09-20

A measurement pass over every component, taken right after the WebView2
control-panel window landed (uncommitted tree on top of `a0e4e9b`). Nothing
was changed; this is numbers, profiles and a ranked list of what to do next.
Same box as the earlier passes: Ryzen 9 5950X, Windows 10 19045, Rust 1.98,
`release` profile (thin LTO, `codegen-units = 1`), binaries built into
`target-bench`. Profiles use the `profiling` profile (release + symbols).

**Volume:** 20 criterion benchmarks, 24 timed `report` runs over the 8 largest
recordings, 5 `list`, 10 cache/`trend`/memory runs, 21 viz HTTP timings,
16 live-stack runs (`bench.ps1` method, cycle-exact), 4 Node engine runs, and
9 sampling profiles (analyzer ×1, capture/viz/ctl ×2 each under load, ctl
fallback ×1, plus one symbol smoke test). Raw live results:
`tools/cpubench/results-2026-09-20.jsonl`.

## Headlines

1. **Nothing regressed in the hot paths.** Every criterion group is flat or
   faster than the 2026-08-28 table (prepare −17%, savgol −17%, flick −27%).
2. **The analyzer is now load-bound.** Load is 34–38% of wall time and is a
   single-threaded serde_json line loop while 15 cores idle. A chunk-parallel
   loader is worth about −40% end to end on big sessions.
3. **`hypot` is 8.5% of all analyzer CPU**, not the ~50 ms the August table
   guessed. MSVC's `_hypot` is the slow, overflow-safe one.
4. **The new WebView2 window costs more than the whole telemouse stack at
   idle**: +0.43–0.51% of a core and ~310 MiB across six `msedgewebview2`
   processes while the window is open (its startup state), and still
   **0.32% and 309 MiB after it is hidden to the tray**. The three telemouse
   processes together idle at 0.155% and 34 MiB.
5. **With its window open, `--no-webview` is the most expensive ctl mode**,
   not the cheapest: the `EDIT` fallback rewrites and repaints its full text
   on every refresh whether or not it changed (ctl 0.26–0.32% idle vs 0.10%
   with WebView2, 0.03% with `--no-gui`). Hidden to the tray it drops to
   0.02%, as designed. *(Fixed 2026-09-21 — see "Landed" under §4.)*
6. **`window_ms = 25` actually ships ~52 batches/s, not 40**, because the
   window is measured from the first event's back-dated stamp. T2 and viz
   cost scale with batches/s, so the live path pays ~30% more than the docs'
   model says.
7. The live path itself is where the August pass left it: syscall-bound, user
   code under 3% of samples. Capture-to-ship p50 25.75 ms / p99 27.25 ms,
   viz delivery p50 26.0 / p99 27.1 ms, zero drops in every run.

## 1. Criterion (in-process hot paths)

| Bench | 2026-08-28 | now | Δ |
|---|---|---|---|
| `encode_view/25` · `/448` | 1.38 µs · 21.2 µs | 1.26 µs · 19.1 µs | −9% · −10% |
| `encode_owned/25` · `/448` | 1.53 µs · 20.7 µs | 1.48 µs · 19.2 µs | −3% · −7% |
| `decode_envelope/25` · `/448` | 2.60 µs · 36.2 µs | 2.59 µs · 34.9 µs | flat · −4% |
| `decode_batch_untagged/25` · `/448` | 2.53 µs · 36.6 µs | 2.50 µs · 36.4 µs | flat |
| `batcher_cycle/25` · `/448` | 35 ns · 512 ns | 33.9 ns · 461 ns | −3% · −10% |
| `load_session` 1 M · 10 M cells | 10.8 · 106 ms | 9.93 · 102 ms (342 · 337 MiB/s) | −8% · −4% |
| `prepare` 1 M · 10 M | 16.2 · 215 ms | 13.4 · 174 ms | −17% · −19% |
| `savgol` 1 M · 10 M | 4.43 · 45.0 ms | 3.67 · 39.2 ms | −17% · −13% |
| `welch` 1 M · 10 M | 6.45 · 67.7 ms | 5.85 · 62.9 ms | −9% · −7% |
| `flick_detect` 1 M · 10 M | 2.57 · 32.9 ms | 1.89 · 28.6 ms | −27% · −13% |

## 2. Analyzer on real recordings

`telemouse-analyze report <file> --timing --quiet`, 3 runs each, warm page
cache, process wall time (median). Run-to-run spread ≤ 3%.

| Recording | Size | Events | wall | CPU | load MB/s | Aug end-to-end |
|---|---|---|---|---|---|---|
| `s-20260909-015040-c330` | 1156 MB | 15.25 M | **5648 ms** | 7531 ms | 406 | — |
| `s-20260912-214651-f423` | 510 MB | 7.01 M | 2879 ms | 3938 ms | 400 | — |
| `s-20260904-042856-5006` | 508 MB | 8.00 M | 3511 ms | 5031 ms | 393 | — |
| `s-20260824-141132-df44` | 462 MB | 6.86 M | 2636 ms | 3609 ms | 384 | 4306 ms (−39%) |
| `s-20260910-202017-1d83` | 411 MB | 5.65 M | 2347 ms | 3219 ms | 404 | — |
| `s-20260905-065904-3279` | 294 MB | 3.91 M | 1744 ms | 2422 ms | 404 | — |
| `s-20260825-215121-4c6b` | 272 MB | 3.91 M | 1559 ms | 2094 ms | 395 | 2451 ms (−36%) |
| `s-20260903-010853-90d3` | 264 MB | 4.06 M | 1943 ms | 2750 ms | 395 | — |

CPU/wall is only 1.33–1.43: the September phase parallelism helps, but load
and `prepare` (together ~55% of wall) run alone on one thread.

Phase table, 1156 MB session (wall 5.63 s; phases after `prepare` overlap):

| Phase | ms | | Phase | ms |
|---|---|---|---|---|
| **load** | **2845** | | micro | 564 |
| **prepare** | **1249** | | quality | 334 |
| kinematics | 1341 | | markers | 327 |
| per_second | 275 | | flicks / lifts / per_minute / clicks | 161 / 127 / 133 / 33 |
| build wall (parallel) | 2591 | | phase elapsed sum | 7388 |

Other commands:

| Run | Result |
|---|---|
| `report --summary` (1156 MB) | 5612 ms — same work, smaller print |
| `report --json-dir`, miss → hit | 5705 ms → **97–104 ms** (reads a 59 MB `report.json`) |
| `list`, 36 recordings, first → warm | 167 ms → 12 ms |
| `trend`, 36 recordings, cold report cache | **22.9 s** (sessions analyzed one after another) |
| `trend`, warm | 550 ms (parses 336 MB of cached full reports for a 36-row table) |
| Peak working set, 1156 MB / 462 MB session | **2918 MiB** / 1509 MiB |

### Analyzer profile (1156 MB session, 1 ms sampling, 7270 on-CPU samples ≈ CPU-ms)

By thread: main 73.9%, kinematics worker 16.2%, micro 5.0%, quality 2.6%,
per-second/markers 2.3%.

| Inclusive | Share | Note |
|---|---|---|
| `load::load_session` | **41.3%** | single thread |
| ↳ `serde_json::from_str::<BatchRef>` | 32.9% | `Vec<RawEvent>` visitor 25.2%, `parse_str` 9.6% (key matching), `parse_integer` 3.2% |
| ↳ `load::next_line` | 5.4% | `read_line` + UTF-8 validation of the whole file |
| `series::prepare` | 18.9% | main thread, before any parallel phase can start |
| `kinematics::compute` | 16.2% | `Summary::of_vec` 7.0% (selects 3.0%, a full `sort_unstable` 1.7%) |
| `sg_run_into` / `SavGol::apply_*` | 8.2% / 6.5% | includes 116 samples of `memset` (fresh zeroed buffers) |
| heap: `RtlAllocateHeap` + `RtlFreeHeap` + `memset` + `VirtualFree` | 7.3 + 4.9 + 5.7 + 2.2% | per-line event `Vec`s, lane vectors, teardown |
| `drop_glue<Prepared>` + `drop_glue<Vec<Run>>` | 3.3% + 3.2% | freeing ~2.5 GiB right before `exit` |

| Self | Share | Callers |
|---|---|---|
| `_hypot` (CRT) | **8.5%** | kinematics 190, `prepare` 177, markers 78, `peak_aim_speed` 96, per_second 72 |
| `__divti3` (128-bit divide) | 2.1% | `prepare` 140 — `QpcAnchor::qpc_to_utc_us`, once per event |

## 3. viz HTTP and replay

| Route | Result |
|---|---|
| `/healthz`, `/api/sessions` (warm), `/` (120 KB page) | ~1.0 ms each; first `/api/sessions` 14.6 ms |
| `/api/session/<508 MB>` | TTFB 1.2 ms, 0.23–0.32 s, **1.65–2.33 GB/s** (Sept: 1.44 GB/s) |
| `/api/session/<1156 MB>` | TTFB 1.4 ms, 0.51 s, 2.37 GB/s |
| viz working set after serving 5 GB | 9.0 MiB |

Server-side replay is done. The cost has moved into the browser (section 5).

## 4. Live stack (capture + viz + ctl)

`bench.ps1` method on private ports: 10 s idle, 30 s of `tmbench inject` at
1 kHz, one WebSocket drain, ctl page polled every 2 s, recording on, Kafka
off. Percent of one core from `QueryProcessCycleTime`. "Total" is the three
telemouse processes, as in BENCHMARKS.md; WebView2 children are listed
separately. The driver was a scratch copy of `bench.ps1` that adds ctl flags,
WebView2 child accounting and working sets.

| Run | idle | **load** | capture (T1 / T2 / ctx / jsonl) | viz | ctl | WebView2 idle / load | WV RAM |
|---|---|---|---|---|---|---|---|
| default ×3 (25 / 8) | 0.147 · 0.157 · 0.161 | **1.301 · 1.381 · 1.319** | 0.70 (0.22 / 0.36 / 0.076 / 0.049) | 0.49 | 0.137 | 0.51 / 0.34 | 6 procs, 312 MiB |
| window 25 / coalesce 2 | 0.158 | 1.437 | 0.88 (0.47 / 0.29 / 0.076 / 0.042) | 0.42 | 0.137 | 0.51 / 0.36 | |
| window 25 / coalesce 4 | 0.154 | 1.302 | 0.73 (0.31 / 0.30 / …) | 0.43 | 0.137 | | |
| window 50 / coalesce 8 | 0.157 | **0.862** | 0.49 (0.23 / 0.16 / 0.075 / 0.024) | 0.23 | 0.135 | | |
| 2 WS clients | 0.166 | 1.616 | 0.68 | **0.80** | 0.138 | | |
| no ctl | 0.056 | 1.153 | 0.66 (ctx 0.044) | 0.49 | — | — | — |
| ctl `--no-gui` | 0.094 | 1.238 | 0.67 (ctx 0.041) | 0.50 | **0.067** (idle 0.033) | — | — |
| ctl `--no-webview` | **0.380** | 1.588 | 0.72 | 0.50 | **0.372** (idle 0.323) | — | — |
| ctl poll 500 ms | 0.242 | 1.449 | 0.73 | 0.50 | 0.218 | | |
| 125 Hz mouse | 0.171 | 1.008 | 0.52 (0.16 / 0.25 / …) | 0.35 | 0.139 | | |
| ~3.4 kHz (asked 4 k) | 0.158 | 1.480 | 0.85 (0.35 / 0.37 / …) | 0.49 | 0.138 | | |
| ~3.4 kHz (asked 8 k) | 0.156 | 1.510 | 0.85 (0.36 / 0.36 / …) | 0.50 | 0.161 | | |

Working sets: capture 8.1 MiB, viz 6.8 MiB, ctl 18.8 MiB (8.3 with
`--no-gui`). Noise on the load total is ±0.04 over the three default runs.
`SendInput` tops out near 3.4 k reports/s on this box, so the two high-rate
rows are the same load; true 8 kHz needs a real mouse.

Against the August numbers: window 50 / coalesce 8 was 0.76, now 0.862. T1
and T2 are unchanged (0.23 / 0.16); the difference is two threads that did
not exist then, `telemouse-context` (0.075) and the `telemouse-jsonl` writer
(0.024–0.049). The shipped default moved to window 25 on 09-05, which is why
the everyday figure is 1.33 rather than 0.76.

### ctl window: open vs hidden to the tray

ctl alone, no external poller, 12 s each; the window was hidden by posting
`WM_CLOSE` (which goes to the tray), exactly what the close button does.

| Mode | window | ctl | WebView2 children | WebView2 RAM |
|---|---|---|---|---|
| WebView2 | open | 0.069 | 0.427 | 301 MiB |
| WebView2 | hidden | 0.026 | **0.319** | **309 MiB** |
| `--no-webview` | open | **0.264** | — | — |
| `--no-webview` | hidden | 0.018 | — | — |

`webview::set_visible` already calls `SetIsVisible(false)` on hide, but that
only stops rendering: the page's timers and `/api/state` polling keep
running and all six processes stay resident.

#### Landed 2026-09-21 — L2, the text view's repaint

`gui::model::worth_painting` now sits in front of both `set_text` and
`Shell_NotifyIcon`: identical text is never written again, text whose words
changed is written at once, and text where only digits moved (the
`refreshed …` clock, the uptimes, the CPU column) waits until the control
has been stale for five seconds. `--no-webview`, window open, ctl alone in a
scratch folder, 60 s of `TotalProcessorTime` per run, three runs each,
alternating:

| Pair | before | after | Δ |
|---|---|---|---|
| 1 | 1.198% | 0.469% | −61% |
| 2 | 0.859% | 0.547% | −36% |
| 3 | 0.729% | 0.286% | −61% |
| median | **0.859%** | **0.469%** | **−45%** |

Roughly half the cost of an open text window goes away; what is left is the
once-a-second snapshot and process scan, not GDI. **Indicative only**: the
box was compiling for other agents throughout, which is both why the spread
between runs is wide and why "before" sits above the 0.26–0.32% measured on
09-20 — ctl's per-second scan walks every process on the machine, and there
were a lot of them. Every pair was measured back to back with the same
method, so the direction is solid even where the absolute numbers are not.

### Live profiles (1 ms sampling, 28 s under load)

Threads in these processes run for tens of microseconds and go back to
sleep, so nearly every sample catches a thread re-entering its wait. Read
the wait rows as "number of wake-ups", not time spent waiting.

| Process | Where the samples land |
|---|---|
| capture | T1 re-arming its timer wait 45.1%; T2 re-parking 32.5%; **T2 inside `sinks::udp::send` → `NtDeviceIoControlFile` 12.1% + winsock 1.5%**; jsonl worker 3.0%; context 2.7%; `NtUserGetRawInputBuffer` 0.5%; all serde/encode under 0.5% |
| viz | 98.4% `NtRemoveIoCompletionEx` (IOCP wake per datagram and per WS frame); application code ~1% |
| ctl (WebView2 mode) | tokio parks 71%, GUI message pump 9.8%, **`Manager::refresh_config` → `ZwCreateFile` 5.7%**, `resolve_bin` → `ZwCreateFile` 1.7% |
| ctl `--no-webview`, idle | **`NtGdiExtTextOutW` + `DrawStream` + `BitBlt` + glyph shaping ≈ 25% of samples** on `ctl-gui`: the fallback text view is repainting |

Two things fall out of the capture profile:

- **T2 wakes twice per batch.** After a flush nothing is pending, so it takes
  the "T1 will wake me" park; T1's next drain unparks it
  (`ZwAlertThreadByThreadId` shows up on T1 inside `process_mouse`), T2 opens
  the batch, parks again on the window timer, wakes, flushes. ~104 wakes/s
  for ~52 flushes/s.
- **52 flushes/s at `window_ms = 25`.** Every run logs `batches_per_s ≈ 51–52`
  and `reports_per_drain ≈ 9.6`. The window is timed from the first event's
  stamp, and cadence draining back-dates that stamp by up to one drain
  (~9.6 ms), so the window closes after two drains (~19 ms), not 25 ms.

Side observation, not performance: during injection the log shows
`pointer lock transition` flipping every 1–3 s with `game="explorer.exe"`.
The synthetic cursor is pinned against a screen edge; the lock heuristic
reads that as pointer lock. Real desktop use can hit the same edge case.

## 5. Dashboard engine (Node, `js-tests/harness.mjs`, replay mode)

| Stream | events | JSON.parse | ingest | checkpoints | full consume | seek (avg of 200) | live per batch | heap |
|---|---|---|---|---|---|---|---|---|
| 10 min @ 1 kHz | 0.6 M | 133 ms | 231 ms | 506 ms | 523 ms | 4.9 ms | 32 µs | ~100–200 MB |
| 60 min @ 1 kHz | 3.6 M | 975 ms | 1742 ms | **3122 ms** | 3067 ms | 4.2 ms | 26 µs | 342 MB |
| 10 min @ 8 kHz | 4.8 M | 966 ms | 2246 ms | **3743 ms** | 3921 ms | **33.6 ms** | 205 µs | 442 MB |

Live steady state is cheap (32 µs of engine work per 25 ms batch). Replay is
not: ~0.27 µs/event parse + ~0.47 µs/event ingest + ~0.85 µs/event
checkpointing, all on the main thread. Extrapolated to the 15 M-event
recording that is roughly 4 + 7 + 13 s of blocked tab after a 0.5 s
download. At 8 kHz a scrub seek costs 34 ms, two frames at 60 Hz, because
checkpoints are spaced in time rather than in events.

## What to do, in order

Estimates are for the 1156 MB session (5.65 s) or the default live run
(1.33% load, 0.155% idle) unless stated.

### Analyzer

| # | Change | Est. gain | Notes |
|---|---|---|---|
| A1 | **Chunk-parallel load.** Read the file once, split at newline boundaries into N byte ranges, parse each range on its own thread into its own `Vec<RawEvent>` + `Vec<BatchMeta>`, concatenate in order. Header stays serial. | load 2.85 s → ~0.4 s; **−40% end to end** | Lines are independent. Line numbers for `BadLines` need a per-chunk newline count; `GameInterner` becomes per-chunk + merge. Supersedes the August "hand-rolled line parser" row: parallelism buys more than a faster scalar parser and keeps serde. |
| A2 | Deserialize events straight into the shared vector (`DeserializeSeed` over `&mut Vec<RawEvent>`) instead of a fresh `Vec` per line + `append` | −5–8% of load | 0.6 M alloc/free pairs on the big session; `finish_grow` is 4.5% of samples. Composes with A1 (per-chunk vector). |
| A3 | `hypot` → `(x*x + y*y).sqrt()` at the 7 call sites in `series.rs` and the ones in kinematics/markers/per_second/flicks/lifts | **−8% CPU, ~−250 ms wall** (177 ms of it on the serial `prepare` path) | Counts-per-second magnitudes cannot overflow a double; differs in the last ulp, so the dense/sparse parity tests need an epsilon. The August table priced this at ~50 ms; the profile says 5× that. |
| A4 | `qpc_to_utc_us`: when `dticks` fits `i64` (always, for any real uptime) divide in `i64`; keep the `i128` path as the fallback | ~−140 ms wall in `prepare` | The "fast path" still does `i128 / 10`, which is a `__divti3` call per event. Same for `ticks_to_us`. Parity test already exists. |
| A5 | Skip teardown: `std::process::exit` after the report is flushed (or `mem::forget(prepared)`) | −250–450 ms wall | 6.5% of samples are `drop_glue` + `VirtualFree` unwinding 2.5 GiB the OS reclaims anyway. CLI only; keep drops in the library. |
| A6 | Start `prepare`'s event-time pass while load is still running, or fold it into A1's per-chunk workers | −150 ms | Falls out of A1 almost for free. |
| A7 | `trend`: analyze sessions concurrently (bounded by memory: ~2.5× file size each) | cold 22.9 s → ~7 s | Today 36 sessions run strictly one after another. |
| A8 | `trend` / `list` read a small `<id>.summary.json` beside the full report | warm 550 ms → ~20 ms | Warm `trend` parses 336 MB of per-second tables to print 36 rows. `--summary` already defines the shape. |
| A9 | Peak memory: 2.9 GiB for a 1.16 GB file | — | Mostly the grid lanes (32 M stored cells × several `f64` lanes). `f32` lanes for the smoothed velocities, or freeing `speed_raw` after `micro`, would cut ~0.5–1 GiB. Only matters on 8 GB machines; measure before doing. |

### Live path and control panel

| # | Change | Est. gain | Notes |
|---|---|---|---|
| L1 | **On hide-to-tray, `TrySuspend` the WebView (resume on show), or close the controller after a few minutes hidden and recreate it on show.** Also have the page stop polling on `visibilitychange`. | hidden: −0.32% CPU; closing also frees ~309 MiB | Hidden-to-tray is the state ctl sits in during a game, and today it is 2× the idle cost of everything else combined. `SetIsVisible(false)` alone does not stop timers. `TrySuspend` needs the controller invisible first, which `set_visible` already arranges. The `EDIT` fallback must keep working (CLAUDE.md). Relevant to the in-game hitching history: six resident Chromium processes are the kind of background load that showed up before. |
| L2 | ~~Fallback text view: keep the last rendered string and skip `set_text` when `model::window_text` is unchanged~~ **landed 2026-09-21, about −45% of ctl with the window open (see above)** | ctl 0.26–0.32 → ~0.05% with the window open | `refresh` already skips hidden and hosted states; what is missing is the change check. Profile: a quarter of ctl's samples are GDI text output in `--no-webview` mode. A plain equality check is not enough on its own — the clock and the uptimes move every tick — so digit-only changes go on a five-second lane. |
| L3 | Make `window_ms` mean what it says: time the window from when T2 opened the batch (or align the flush to the drain cadence) | 52 → 40 batches/s: **−0.15–0.19%** (T2 + viz + each WS client scale with batches/s) | Or keep the behaviour and fix the docs' "40 batches/s". Either way the latency/CPU trade in PERFORMANCE-2026-09.md is currently computed from the wrong rate. |
| L4 | T2: while the stream is active (a batch flushed within the last window), park on the window timer only; keep the T1 unpark for the idle→active edge | **−0.10–0.13%**, and removes T1's unpark syscall per batch | Halves T2 wake-ups (104 → 52/s). First-event latency is unchanged because the idle edge still wakes promptly. |
| L5 | `telemouse-context`: 0.075% with ctl running vs 0.044% without | −0.03% | New since August. Worth a look at what it does per tick when the foreground window is ctl's; a snapshot only on foreground *change* should be near zero. |
| L6 | ctl `refresh_config` / `resolve_bin` open files on every `/api/state` poll | −0.01–0.02%; more at 500 ms polling (ctl 0.218%) | Cache on `(mtime, len)` from one `stat`, or re-read only every few seconds. |
| L7 | Skip the UDP send while the last N failed with `WSAECONNRESET` (no viz up) | −0.19% with no viz | Still open from August; UDP send is the single largest real cost on T2 (13.6% of capture samples). |
| L8 | Second WS client costs +0.30% | — | Known kernel floor (~47 µs/send). L3 cuts it by a quarter for free. |

### Dashboard replay

| # | Change | Est. gain | Notes |
|---|---|---|---|
| J1 | Build checkpoints incrementally during ingest, or lazily on first seek into a region, instead of one pass at the end | removes a 3–13 s main-thread stall on long sessions | 0.85 µs/event today; ingest already visits every event. |
| J2 | Parse + ingest replay in a Worker and transfer the typed arrays | tab stays responsive during 5–10 s loads | The columns are already typed arrays, so they transfer without a copy. |
| J3 | Space checkpoints by event count, not time | 8 kHz seek 34 ms → ~5 ms | Keeps scrubbing inside one frame for high-polling mice. |

### Not worth doing (measured)

- Anything in `telemouse-core` encode/decode: 1.3 µs per batch, 52 times a second.
- viz replay serving: 2.3 GB/s, 1.2 ms TTFB.
- viz/ctl user-space code under load: ~1% of their samples.
- `/api/sessions`, `list`: 1 ms / 12 ms warm.
- Mouse rate: 1 kHz → 3.4 kHz adds only 0.13% on T1 and nothing elsewhere.

## Reproducing

```powershell
$env:CARGO_TARGET_DIR = "target-bench"
cargo build --release --workspace --locked
cargo build --profile profiling --workspace --locked     # symbols for profiles
cargo bench -p telemouse-core --locked
cargo bench -p telemouse-analyze --locked
target-bench\release\telemouse-analyze.exe report recordings\<id>.jsonl --timing --quiet
tools\cpubench\bench.ps1 -Label default            # never with a game open
node --test "crates/viz/js-tests/*.test.mjs"       # harness the engine bench reuses
```

The profiles came from a ~300-line unprivileged sampler written for this pass
(`SuspendThread` + `StackWalk64` via dbghelp, gated on `QueryThreadCycleTime`
so only on-CPU threads are sampled, folded-stack output). xperf/ETW needs
elevation, which the project never asks for. It only ever opened telemouse
processes it had started. It is not in the tree; if it is worth keeping it
belongs next to `tmbench` under `tools/`, outside the release zips.
