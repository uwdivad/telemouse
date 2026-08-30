# Benchmarks

What is measured, how to run it, and the numbers as of 2026-08-28 on the
development box (Ryzen 9 5950X, Windows 10, Rust 1.98, `release` profile:
thin LTO, `codegen-units = 1`). Re-run after touching anything on these paths
and update the tables; criterion keeps its own baseline under
`target/criterion` and prints the delta.

Two kinds of measurement live here. The criterion benches cover the pure,
in-process hot paths (wire encode/decode, batcher, analyzer kernels). The
live pipeline — capture + viz + ctl CPU while a synthetic 1 kHz mouse runs —
is syscall-bound and is measured with the cycle-exact SendInput harness in
[`tools/cpubench`](../tools/cpubench/README.md); its numbers are in the last
section.

## Running

```powershell
cargo bench -p telemouse-core                # wire encode/decode, batcher
cargo bench -p telemouse-analyze             # loader + analysis kernels
cargo bench -p telemouse-analyze -- savgol   # one group

# End to end on a real recording, with the per-phase table:
cargo run --release -p telemouse-analyze -- report recordings/<id>.jsonl --timing --quiet
```

Use a separate `CARGO_TARGET_DIR` if a debug build is open in an IDE, so the
bench build does not evict it (and vice versa). Do not run two cargo jobs at
once while benching: the 10-sample groups are sensitive to contention.

## `telemouse-core` — `benches/wire.rs`

The per-batch work the shipping thread (T2) does 40×/s, and what a consumer
pays to read it back. 25 events = one 25 ms window at 1 kHz (steady state);
448 = `MAX_EVENTS_PER_BATCH`.

| Bench | 25 events | 448 events | Notes |
|---|---|---|---|
| `encode_view` (T2's path: `EnvelopeView` → reused buffer) | 1.38 µs | 21.2 µs | ~21 M events/s; ~55 ns per event, itoa-bound |
| `encode_owned` (`Envelope::to_json`) | 1.53 µs | 20.7 µs | the view saves only the String alloc — the JSON work is identical |
| `decode_envelope` (`Envelope::from_json`), **before** | 6.79 µs | 116 µs | serde's internally-tagged enum buffers the document as `Content` first |
| `decode_envelope`, **after** (tag-prefix fast path) | 2.60 µs | 36.2 µs | −62% / −68%; equal to the raw struct parse |
| `decode_batch_untagged` (`Batch` direct) | 2.53 µs | 36.6 µs | the floor for any JSON decode of this shape |
| `batcher_cycle` (push × n, `should_flush`, `reset`) | 35 ns | 512 ns | ~0.8 ns/event; not worth another look |

Reading: at 1 kHz the whole encode side is ~55 µs/s of CPU (0.0055% of a
core). Nothing in core is on the critical path any more; the raw-input
syscall (~28 µs kernel CPU per read) dwarfs all of it.

## `telemouse-analyze` — `benches/hot_math.rs`

Synthetic sessions with the flick–correct–click–rest shape (`testutil::
bench_events`), at 1 M and 10 M grid cells ("a long warmup", "a full
evening"). Throughput is per stored grid cell except where noted.

| Group | 1 M cells | 10 M cells | Change this pass |
|---|---|---|---|
| `load_session` (JSONL parse + flatten + intern, page-cached, ~3.4 / 34 MB) | 10.8 ms (315 MiB/s) | 106 ms (326 MiB/s) | new bench |
| `prepare` (events → sparse grid, smoothing) | 16.2 ms (61.6 Melem/s) | 215 ms (46.4 Melem/s) | −29% / −14% (was 20.6 / 251 ms; `sg_run` scratch reuse) |
| `savgol` (7-point quadratic smoother, one lane) | 4.43 ms (225 Melem/s) | 45.0 ms (222 Melem/s) | −13% (was 5.15 / 51.5 ms; interior loop) |
| `welch` (`micro::compute`, 20:1 decimated) | 6.45 ms (155 Melem/s) | 67.7 ms (148 Melem/s) | unchanged |
| `flick_detect` | 2.57 ms (389 Melem/s) | 32.9 ms (304 Melem/s) | unchanged (±5% is this group's noise) |

The synthetic loader number (~320 MiB/s) matches the ~350 MB/s seen on the
real recordings below, so the bench is a fair proxy for loader work.

## End to end — real recordings

`telemouse-analyze report --timing --quiet` on the three largest sessions in
`recordings/`. "analysis" is the `--timing` total (everything after load);
"end to end" is the `analysis complete total_ms=` log line (load + analysis +
render). Both are single runs, warm page cache; run-to-run noise is ~3%.

| Recording | Size | Events | Grid cells | analysis before | analysis after | end to end before → after |
|---|---|---|---|---|---|---|
| `s-20260825-030517-46a5` | 26 MB | 349 k | 1.9 M | 278 ms | 201 ms | 370 → 290 ms |
| `s-20260825-215121-4c6b` | 272 MB | 3.9 M | 15.3 M | 2300 ms | 1688 ms | 3077 → 2451 ms |
| `s-20260824-141132-df44` | 462 MB | 6.9 M | 19.7 M | 4216 ms | 2931 ms | 5531 → 4306 ms |

Per phase on the 462 MB session, before → after:

| Phase | before | after | what changed |
|---|---|---|---|
| prepare | 730 ms | 701 ms | `sg_run` now reuses scratch buffers |
| quality | 183 ms | 207 ms | noise (selection-based `Summary` is a wash here — one 8 M sample) |
| **kinematics** | **2306 ms** | **1047 ms** | see below |
| flicks | 95 ms | 124 ms | noise |
| micro | 346 ms | 322 ms | |
| clicks / lifts / markers / per_second / per_minute | 556 ms | 530 ms | |
| load (end-to-end minus analysis) | ~1.3 s | ~1.3 s | ~350 MB/s; untouched |

### What kinematics was doing

Instrumented split before the changes (462 MB session, 8 725 runs, 11.07 M
moving cells): derivatives + moving-cell filter 748 ms, nine `Summary::of`
calls 1.2 s (of which six were sorts of scaled copies of the other three),
event distance loop 67 ms, segment loop 52 ms.

1. **Six redundant sorts.** `speed_cm`, `accel_cm`, `accel_deg`, `jerk_cm`,
   `jerk_deg` are the counts-space samples multiplied by a positive constant,
   and every field of `Summary` is order-preserving-linear, so
   `Summary::scaled(k)` derives them from the one summary that was already
   computed. −770 ms.
2. **Sorting for three quantiles.** `Summary::of` now uses
   `select_nth_unstable` for the median/p90/p99 (each later one partitioning
   only the tail the previous one left) instead of a full `sort_unstable`;
   order statistics are bit-identical (tested), mean/stddev differ only by
   summation order. −170 ms across the pipeline.
3. **Allocation churn in the derivative sweep.** `sg_run` allocated a framed
   input, an output, and a trimmed copy per run per lane (3 × 4 × 8 725
   allocations, ~1 GB of memory traffic); `sg_run_into` + `SavGol::apply_into`
   reuse four scratch pairs for the whole sweep and the caller reads a
   sub-slice. The sample vectors are also sized from an exact moving-cell
   count (one cheap pass) instead of doubling up from a 1 M guess.
4. **`SavGol::apply_with` interior loop.** The interior outputs all use the
   centered kernel; a `windows(width)`/`zip` loop over that stretch drops the
   per-output table lookup and the bounds checks. Same tap order, so the
   result is bit-identical to the general formulation (tested against a
   reference implementation at every length around the window size).

Together: kinematics 2306 → 1047 ms (−55%) with no numeric change beyond
floating-point summation order in `mean`/`stddev`.

## What is left on the table

Ordered by expected payoff on the 462 MB session; none is implemented.

| Candidate | Est. gain | Why not yet |
|---|---|---|
| Fuse the four derivative lanes into one pass (`d1`/`d2` on `vx`/`vy` read the same input twice; the reduction only needs one run at a time) and reduce `ax/ay/jx/jy` straight into `accel`/`jerk` without materializing the lanes | ~200–300 ms | Touches the `sg_run` parity contract that the sparse-grid tests lean on; do it with a dense-vs-sparse parity test in hand. |
| Load: `serde_json` parses every event's `ts_qpc` as a full `u64` through the generic visitor; a hand-rolled line parser for the fixed `{"ts_qpc":..,"dx":..,"dy":..}` shape would roughly halve the ~1.3 s load | ~600 ms | Only worth it if a binary wire format (deferred in the audit) is *not* going to happen; a `RecordingReader` abstraction should land first either way. |
| `prepare` builds `event_us`/`event_t` (two session-length `Vec`s) that only a few phases read | ~100 ms + 110 MB | Needs a survey of which phases actually index by event. |
| `Summary::of` filters into a fresh `Vec` even when the sample has no non-finite values; take `&mut Vec<f64>` from callers that own the sample and partition in place | ~50 ms | Small; API churn across every metric module. |
| `hypot` → `sqrt(x*x + y*y)` in the moving-cell loop (22 M calls) | ~50 ms | Changes results in the last ulp; the accuracy is not needed but the parity tests would need loosening. |
| Parallelism: the phases after `prepare` are independent reads of `Prepared`; `kinematics`, `micro` and `per_second` could run on three threads | ~1 s wall | A 16-core box makes this the biggest wall-clock win, but the crate is deliberately dependency-light; `std::thread::scope` would do without rayon. Do it after the single-thread items so it does not hide them. |

Verified not worth it: `lto = "fat"` / `panic = "abort"` (audit, rejected);
`-C target-cpu=znver3` for the analyzer is a documented opt-in in the root
`Cargo.toml` and helps only the SIMD-able smoother (~10% on `savgol`, nothing
end to end since the smoother is <5% of the pipeline).

## Live pipeline — `tools/cpubench` (2026-08-29)

Method: `bench.ps1` starts `telemouse`, `telemouse-viz` and `telemouse-ctl`
from a separate `target-bench` build on private ports, connects one
WebSocket drain (the dashboard) and polls `/api/state` every 2 s (the
control-panel page), measures 10 s idle, then 30 s while `tmbench inject`
feeds 1000 relative reports/s through `SendInput`. Percentages are of one
core from `QueryThreadCycleTime` deltas (see the harness README for why not
`GetThreadTimes`). Every run's JSON line is in
`tools/cpubench/results-2026-08-29.jsonl`; run-to-run noise on the totals is
about ±0.03.

### Baseline → final

| Run | Config | Load: total | capture (T1 / T2 / T3) | viz | ctl | Idle: total |
|---|---|---|---|---|---|---|
| base-1 | window 25, coalesce 2, queue-wake T1, sysinfo scan | **2.60** | 1.71 (1.41 / 0.28 / 0.03) | 0.42 | 0.47 | 0.32 |
| base-2 | same | **2.54** | 1.68 (1.38 / 0.28 / 0.03) | 0.41 | 0.45 | 0.26 |
| base-2clients | same, 2 WS clients | 2.87 | 1.74 | 0.69 | 0.45 | 0.28 |
| final-1 | window 50, coalesce 8, cadence T1, two-tier scan | **0.76** | 0.43 (0.23 / 0.18 / 0.03) | 0.24 | 0.09 | 0.09 |
| final-2 | same | **0.76** | 0.43 (0.23 / 0.18 / 0.03) | 0.23 | 0.10 | 0.09 |
| final-3 | same, plus T3 screen-metrics change | **0.77** | 0.44 (0.23 / 0.18 / 0.02) | 0.24 | 0.10 | 0.085 |
| final-2clients | same, 2 WS clients | 0.92 | 0.44 | 0.39 | 0.09 | 0.10 |
| final-w25c2 | new code, **old** config (25 / 2) | 1.27 | 0.76 (0.46 / 0.28 / 0.03) | 0.42 | 0.09 | 0.09 |

Loaded: mean 2.57 → mean 0.763, **−70.3%**. Idle: 0.29 → 0.088, −69.7%
(the idle figure is dominated by the harness's 2 s control-panel poll over a
fresh TCP connection each time; a browser tab keeps its connection, and a
hidden tab polls every 10 s). With the old config the code changes alone
are −51%; the rest is the two defaults.

### What each experiment said

T1 variants, all at window 25 / coalesce 2, old viz and ctl (so compare the
T1 column):

| Experiment | T1 | Reading |
|---|---|---|
| as shipped (queue wake, then `MsgWaitForMultipleObjectsEx` on the timer) | 1.41 | — |
| plain `WaitForSingleObject` on the timer inside the window | 1.11 | the win32k wait itself is ~9 µs dearer per call |
| cadence: after a non-empty drain wait the timer only, never the queue | 0.50 | **the queue wake is the cost (~27 µs)**, not the read |
| cadence with a periodic timer (no `SetWaitableTimer` per drain) | 0.46 | adopted |
| 16-report buffer instead of 256 | 1.43 | the read's copy size is irrelevant |

viz variants (T1 as shipped): `current_thread` runtime 0.42 (no change);
blocking UDP thread outside tokio 0.46 (worse: 0.13 on the thread + 0.34 on
the workers); both 0.42. The bridge's cost is the per-client WebSocket send.

Loopback send micro-benchmark (`tmbench tcp` / `udp`, 40 sends/s, 20 s,
sender's own thread cycles minus the sleep-wake floor): **~47 µs per send**
for TCP and UDP alike, 1200 B or 200 B. This is the kernel floor every hop
pays; per batch the pipeline pays it three times (agent send, bridge
receive, one WS send per browser).

Config matrix on the new code (ctl before its second fix, so add ~−0.17 to
the totals):

| window / coalesce | total | T1 | T2 | viz |
|---|---|---|---|---|
| 25 / 2 | 1.45 | 0.45 | 0.28 | 0.42 |
| 50 / 2 | 1.14 | 0.45 | 0.17 | 0.23 |
| 40 / 4 | 1.10 | 0.33 | 0.21 | 0.28 |
| 50 / 4 | 1.04 | 0.33 | 0.18 | 0.24 |
| 50 / 8 | 0.88 | 0.22 | 0.17 | 0.23 |

T1 ≈ 14 µs × 1000/(coalesce+1) drains/s; T2 and viz ∝ batches/s.

### Timestamp quality at each setting

`telemouse-analyze report` on the harness recordings (synthetic 1 kHz input,
so the true interval is 1.00 ms):

| Recording | intervals ≤ 1 ms | median | p99 | max gap |
|---|---|---|---|---|
| baseline (queue wake, coalesce 2) | 71.5% | 1.00 ms | 1.11 ms | 5.6 ms |
| cadence, anchored at prev+step (rejected) | 46.5% | 1.00 ms | 1.59 ms | 5.5 ms |
| cadence, spread, coalesce 8 (**default**) | 57.3% | 1.00 ms | 1.08 ms | 9.0 ms |
| cadence, spread, coalesce 2 | 51.2% | 1.00 ms | 1.27 ms | 5.3 ms |

### What is left on the live path

| Candidate | Est. gain | Why not yet |
|---|---|---|
| Sync WebSocket writer per client outside tokio (tungstenite on the `into_std()` stream) | ≤ 0.1 / client | ~25 µs of the 72 µs per frame is library; the rest is loopback TCP. Not worth the surface. |
| `coalesce_ms` 10 (the cap) | −0.04 | 1 in 10 drains fewer; the quality trade grows for little. |
| Skip the UDP send when the last N were `WSAECONNRESET` (no viz running) | −0.19 while no viz is up | Only helps the no-viz case; needs a re-probe cadence. |
| Fewer ctl polls while the tab is visible (2 s → 5 s) | −0.03 | The page's component log would lag; the harness also overstates this (new TCP connection per poll). |
