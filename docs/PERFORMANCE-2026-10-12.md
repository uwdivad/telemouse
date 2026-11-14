# Profiling pass — 2026-10-12

The first run of the opt-in hotpath instrumentation (`docs/PROFILING.md`)
over the whole pipeline: a 20-minute capture session under the profiler with
viz receiving the UDP feed, then the analyzer in timing and allocation mode
over four recordings, plus `list` and a cold `trend` over all 40 recordings
(5.0 GB). Nothing was changed; this is the numbers and a ranked list of what
they say to do. Uncommitted tree on top of `6b0944e`, same box as the
September survey (Ryzen 9 5950X, Windows 10 19045, Rust 1.98, `release`
profile, `--features hotpath` / `hotpath,hotpath-alloc`).

**Volume:** one 1200 s capture run (158,543 events, 21,354 batches, viz on
loopback, Kafka configured but no broker reachable), `report` on the 1,155 MB,
137 MB, 2.6 MB and the fresh 14 MB recording, the same 1,155 MB report again
from the cache, `list`, `trend --json-dir` over the folder, and alloc-mode
`report` on the 137 MB and 1,155 MB recordings.

## Headlines

1. **The capture agent's code is 0.3% of its latency budget.** The shipping
   thread spends ~67 µs per batch, 57 µs of it in the loopback UDP send. The
   rest of the 26 ms a batch waits is the 25 ms window plus ~1 ms of park
   overshoot, by design. Lower latency on the live path is a cadence decision,
   not an optimisation (C1 below).
2. **The analyzer's wall time is `load → prepare → kinematics`.** Every other
   metric group runs beside kinematics and finishes first. On the 1,155 MB
   recording that chain is 0.42 + 1.24 + 1.84 s of a 3.97 s run.
3. **Kinematics is memory traffic, not maths.** It builds six 18.6 M-sample
   vectors (859 MB) and summarises each with seven passes; the smoothing
   kernels are a quarter of its time.
4. **Prepare is single-threaded** through the per-run smoothing pass and
   allocates a fresh scratch pair plus a copy per run per lane (984 MB of
   churn on the big recording).
5. **The loader allocates one `Vec<RawEvent>` per batch line** — 1.6 GB of
   churn, 27% of everything the big report allocates — for a 2.7 GB/s read
   that is otherwise fine.
6. **`trend` over the folder ran at 1.3× concurrency**: 21.9 s of per-recording
   work in 16.8 s wall, because the 512 MB in-flight byte budget admits each
   of the five largest recordings alone.
7. Side findings: the pointer-lock detector flapped 70 times in Chrome during
   the capture run; the hotpath sampler itself used 19% of a core under
   capture and ~35% under the analyzer, so absolute times here are slightly
   inflated.

## 1. Capture agent (20 minutes, 25 ms window, 8 ms coalesce)

A 111 Hz mouse on the desk, so the load is light: 146 events/s average, 537
peak, 28 batches/s. No ring drops, ring high-water 31 of 65,536, RSS 14 MB.
Kafka dropped 21,315 envelopes with no broker reachable (expected). Sidecar:
`exit = duration`, `clean = true`.

| Function (T2 unless noted) | calls | avg | p95 | total |
|---|---|---|---|---|
| `Shipper::flush` | 25,161 | 63.2 µs | 100.6 µs | 1.59 s |
| `Shipper::deliver_encoded` | 21,355 | 66.8 µs | 93.8 µs | 1.43 s |
| `sinks::fan_out` | 21,355 | 66.6 µs | 93.6 µs | 1.42 s |
| `UdpSink::send` | 21,806 | **57.4 µs** | 82.4 µs | 1.25 s |
| `JsonlSink::send` | 21,355 | 7.3 µs | 9.8 µs | 156 ms |
| `Shipper::encode` / `EnvelopeEncoder::encode` | 21,806 | 5.2 / 4.6 µs | 7.7 / 7.0 µs | 112 / 101 ms |
| `KafkaSink::send` (queue closed) | 21,355 | 246 ns | 400 ns | 5 ms |
| `record_latency` | 21,354 | 416 ns | 700 ns | 9 ms |
| `raw_input::decode_mouse` (T1) | 158,543 | 149 ns | 500 ns | 24 ms |
| `context_thread::sample` (T3) | 4,790 | 67 µs | 130 µs | 322 ms |
| `Reporter::maybe_report` (T3) | 4,790 | 74 µs¹ | 6.2 µs | 354 ms |
| `Sidecar::refresh` / `meta::write` (T3) | 242 | 1.08 / 1.07 ms | 1.40 / 1.39 ms | 261 / 259 ms |

¹ The average is 239 five-second reports at ~1.5 ms each (stats snapshot,
the log line, the sidecar rewrite) spread over 4,790 ticks; the p95 is a
no-op tick.

**Queues.** `jsonl_queue`: 21,355 sent and received, max depth 1, send→recv
delay avg 11.4 µs, p95 15.2 µs — the writer thread is never behind.
`kafka_queue`: 40 sent, 0 received, then closed by the failed connect.

**Latency.** From the agent's own 5-second stats lines over the 217 windows
that had input:

| | min | avg | max |
|---|---|---|---|
| capture→ship p50 | 23.25 ms | 26.0 ms | 29.0 ms |
| capture→ship p99 | 26.75 ms | 33.4 ms | 35.25 ms |
| ship tail p99 (newest event) | 8.25 ms | 28.9 ms | 35.25 ms |

The viz bridge agreed: `/api/stats` latency p50 26.3 ms, p99 31.4 ms, 20,876
datagrams forwarded, 0 parse errors, 0 seq gaps, queue depth never above 1.

Reading: the first event of a batch waits exactly one window (the window
opens on that event and the park overshoots by ~1 ms), and the p99 adds the
coalesce back-dating (up to `coalesce_ms + 1` = 9 ms). With a slow,
intermittent stream the newest event waits almost as long as the oldest,
because a batch is usually one drain followed by an empty rest of the window.
Everything the code does per batch fits in 70 µs.

## 2. Analyzer on real recordings

`report --quiet --timing`, cold cache, default parameters. Per-phase numbers
are the analyzer's own `--timing` table; the hotpath columns agree with them
to within the profiler's overhead.

| Recording | events | load | prepare | kinematics | micro | quality | wall | RSS max |
|---|---|---|---|---|---|---|---|---|
| 1,155 MB (`s-20260909-015040-c330`) | 15.25 M² | 0.42 s | 1.24 s | 1.84 s | 0.62 s | 0.36 s | 3.97 s | 3.1 GB |
| 137 MB (`s-20260923-065348-7702`) | 1.93 M | 0.07 s | 0.19 s | 0.33 s | 0.10 s | 0.05 s | 0.66 s | 515 MB |
| 14 MB (today's capture) | 0.16 M | 0.03 s | 0.04 s | 0.04 s | 0.02 s | 0.005 s | 0.15 s | 78 MB |
| 2.6 MB (`s-20260922-225752-d8c6`) | 0.04 M | 8.6 ms | 4.3 ms | 5.8 ms | 1.8 ms | 0.9 ms | 25 ms | — |
| 1,155 MB, cache hit | — | — | — | — | — | — | 0.13 s | 86 MB |

² A 30-hour recording: 83.6 M grid cells, hits the 32 M stored-cell cap, so
10.7 M of its events are analysed. 18.6 M of the stored cells are "moving".

**Where the time goes (1,155 MB).** `compute_metrics` is 1.84 s and
`kinematics::compute` is 1.84 s: quality, micro, flicks, clicks, lifts,
markers, per-second and per-minute all hide under it. `sg_run_into` was
called 133,578 times (six lanes × 22,263 runs) for 730 ms in all, so roughly
0.49 s of kinematics is the two differentiating kernels and ~1.35 s is the
per-cell loop and six `Summary::of_vec` calls over 18.6 M samples each.
Prepare's 1.24 s is single-threaded: µs conversion and binning over 15 M
events, then the smoothing pass over the runs. Writing the 59 MB report cache
costs 119 ms; reading it back 127 ms.

**Allocation (alloc mode, exclusive bytes).**

| Function | 137 MB | 1,155 MB | share |
|---|---|---|---|
| `series::prepare` | 287 MB | 1.8 GB | 30–32% |
| `load::parallel::parse_range` (16 workers) | 207 MB | 1.6 GB | 23–27% |
| `series::sg_run_into` | 159 MB | 984 MB | 16–18% |
| `kinematics::compute` | 153 MB | 859 MB | 14–17% |
| `load::load_session` (assemble) | 56 MB | 453 MB | 6–7% |
| `quality::compute_with_warnings` | 19 MB | 151 MB | 2% |

Prepare's share is the grid itself (seven lanes over 32 M stored cells plus
`event_us`). The `sg_run_into` bytes are not the kernel: `prepare` calls the
allocating `sg_run` wrapper, which makes a new `SgScratch` and a `to_vec`
copy per run per lane; kinematics reuses one scratch and shows up only in its
own six vectors. The loader's 1.6 GB is one `Vec<RawEvent>` per batch line,
appended into the body and dropped.

**`list`.** 423 ms: two uncached recordings scanned in 198 ms, and 225 ms
over the 38 cache hits (~6 ms each for the three-sample signature and the
sidecar read).

**`trend` over the folder (cold cache, 40 recordings, 5.0 GB).** 16.8 s wall;
`report_for` totals 21.9 s across the four workers. The 512 MB
`TREND_BYTES_IN_FLIGHT` budget runs the 1,155, 510, 508, 462 and 410 MB
recordings essentially alone, and each is internally parallel only during
load and the kinematics fork; the four scheduler threads averaged 47%, 15%,
14% and 6% of a core.

## What to do, in order

Analyzer first, because that is where the wall time is and the fixes are
local.

- **A1 — Fuse the kinematics summaries.** Mean, standard deviation and max can
  be accumulated in the push loop; the `retain` pass is dead because every
  sample is finite by construction; and the degree-space vectors are the
  count-space ones scaled whenever `pitch_coeff == yaw_coeff` and
  `locked_only` is off (the defaults), so only the three quantile selections
  need a materialised vector. Expect kinematics to roughly halve and its
  859 MB to drop by two thirds.
- **A2 — Parallelise prepare's smoothing pass and reuse the scratch.** Runs
  are independent; a scoped split over runs covers ~60% of prepare, and
  calling `sg_run_into` with one shared `SgScratch` per thread removes the
  984 MB of `sg_run` churn.
- **A3 — Compute the derivative lanes in prepare.** Kinematics re-reads every
  run to apply the two differentiating kernels; doing it in the same pass
  while the run is hot removes four of the six kernel sweeps.
- **A4 — Deserialize batch events straight into the body.** A `SeqAccess`
  visitor that appends to `Body::events` (the shape `EventTally` already
  has) removes the per-line `Vec<RawEvent>`: 1.6 GB less churn and lower
  peak RSS on big sessions, which also helps A6.
- **A5 — Keep the per-second table out of the report cache**, or in a sidecar
  file, if the panel ever reads full reports often: the 59 MB JSON is 83,592
  per-second rows and costs 127 ms to read back.
- **A6 — Size the `trend` byte budget from available memory** (or raise it
  once A1/A4 cut the ~2.7× file-size peak) so large recordings overlap. The
  recorder-side constant was right for the memory it had to protect.
- **C1 — Decide whether the live path should have its own cadence.** The
  UDP sink could receive a datagram per drain (~9 ms) while JSONL and Kafka
  keep 25 ms batches: live latency ~26 → ~10 ms for roughly 110 sends/s at
  57 µs, ~0.6% of a core, and no change to recordings. Today the only lever
  is `batch.window_ms`, which changes every sink and every hop's batch rate.
- **C2 — Look at the pointer-lock detector's single-tick positives.** One
  250 ms tick of `pointer_locked = true` in a browser is a false positive;
  it costs two log lines and a batch with a locked flag and no cursor, 70
  times in 20 minutes here. A one-tick hold (require two consecutive samples)
  would remove them.

Not worth touching: the UDP send (kernel floor, see `docs/BENCHMARKS.md`),
the JSONL enqueue (7 µs, writer never behind), the T3 sidecar rewrite (1 ms
every 5 s off the hot threads), `decode_mouse`.

## Caveats

- hotpath's sampling thread (`hp-threads`) ran at 15–27% of a core during the
  capture run and 35–40% during the analyzer runs; every absolute number
  above includes some of that pressure.
- viz and ctl are not instrumented; the live path past the bridge was read
  only from `/api/stats`.
- The mouse polled at ~111 Hz and the desk was busy with ordinary work, not a
  game: the capture numbers are an idle-desk baseline, not a 1 kHz load test
  (that is what `tools/cpubench` is for).

## Reproducing

```powershell
# capture, 20 minutes, writing to a scratch recordings dir via a copied config
$env:HOTPATH_METRICS_SERVER_OFF = "1"; $env:HOTPATH_OUTPUT_FORMAT = "json-pretty"
$env:HOTPATH_OUTPUT_PATH = "capture.hotpath.json"
cargo run --release -p telemouse-capture --features hotpath -- run --config <scratch>\telemouse.toml --duration-secs 1200
# analyzer, timing then allocation
cargo run --release -p telemouse-analyze --features hotpath -- report s-20260909-015040-c330 --quiet --timing --json-dir <scratch>\reports
cargo build --release -p telemouse-analyze --features hotpath,hotpath-alloc --target-dir target\hp-alloc
.\target\hp-alloc\release\telemouse-analyze.exe report s-20260923-065348-7702 --quiet
```

`HOTPATH_FUNCTIONS_LIMIT=0` prints every instrumented function. The viz bridge
numbers are `GET http://127.0.0.1:7879/api/stats` while capture runs.
