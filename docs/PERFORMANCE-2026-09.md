# Performance and latency implementation — 2026-09-05

This pass implements the follow-up recommendations from the repository audit.
Measurements were taken on the development machine (Ryzen 9 5950X, Windows,
Rust 1.98, the workspace release profile with thin LTO) against the current
worktree and the real recordings under `recordings/`.

## Results

| Area | Before | After | Change |
|---|---:|---:|---:|
| 532,343,946-byte replay transfer | 2.929 s / 182 MB/s | 0.369 s median / 1.44 GB/s | 7.9x throughput |
| Replay time to first byte | 8.955 ms | 1.7–2.0 ms | 78–81% lower |
| Live typed-array compaction, synthetic 1,200-frame run | 1,200 copies / 17.447 ms | 2 copies / 0.082 ms | 600x fewer copies; 213x less copy time |
| List 26 recordings / 2.202 GB, warm persistent cache (CLI elapsed) | 4,971.6 ms | 17–23 ms | 216–292x faster |
| List internal scan time, warm cache | 4,948.8 ms | 6–7 ms | about 825x faster |
| Per-minute aggregation, 7,997,571 events | 83.7 ms | 42.5 ms | 49% lower |
| Analyzer build, 7,997,571 events | 1,389.5 ms median sequential | 845.5 ms median parallel | 39.2% lower |
| Analyzer benchmark wall time, same input | 1,458.8 ms median sequential | 905.6 ms median parallel | 37.9% lower |
| Analyzer peak working set | 874.1 MiB median sequential | 934.7 MiB median parallel | +60.6 MiB / +6.9% |
| Redundant timestamp storage, same input | 61.0 MiB | 0 | 61.0 MiB saved |
| Kafka sink construction, unreachable broker | up to the 5,000 ms connection timeout | 254 µs measured | connection removed from capture startup |
| Kafka sink shutdown in the same test | initialization could own the caller timeout | 953 µs measured | worker cancellation and bounded teardown |
| Kafka terminal failure (first async implementation → reviewed version) | ~40 allocated/formatted error paths/s (144,000/hour) | 10,000 quiet counted drops in 473 µs (47 ns/send) | one causal error; no permanent error loop |
| JSONL enqueue while the writer is deliberately blocked | inherited storage latency | 14 µs to count bounded-queue overflow | shipping thread remains responsive |
| Default capture/browser live buffers | 50 ms / 55 ms | 25 ms / 35 ms | roughly 60 ms → 35 ms live floor |

The first metadata-cache population was 7.041 s on the cold/noisy audit run;
the cache is intended to accelerate repeated listings. It is persistent, so
normal control-panel and CLI use takes the 17–23 ms warm path. A changed or
actively growing recording is rescanned, and a corrupt cache is discarded.

The live-buffer change is an explicit latency/CPU tradeoff. At 25 ms the
pipeline handles 40 batches/s instead of 20; each batch costs about 150 µs of
kernel CPU across capture, UDP, the bridge, and each browser. Use
`window_ms = 50` when minimum background CPU matters more than responsiveness.
The raw-input coalescing default remains 8 ms.

## What changed

### Replay and browser

- `/api/session/{id}` uses a 256 KiB `ReaderStream` buffer instead of the
  library's 4 KiB default.
- Resolving one recording now performs a strict, direct regular-file lookup.
  It no longer lists and head/tail-probes all sibling recordings (up to about
  3.25 MiB of reads for the current 26-file directory).
- The live timeline compacts at a 24,096-event high-water mark and returns to
  20,000 events, rather than shifting every typed-array column every frame.
- Dashboard and OBS defaults are consistently 35 ms. Existing browser
  `localStorage` overrides still win.

### Analyzer

- `scan_dir` maintains `.telemouse-analyze-index-v1.json` beside recordings.
  Entries are keyed by filename, size, created/modified timestamps, and FNV-1a
  samples from the front, middle, and tail. Publication is flush/sync plus
  atomic rename; stale, changing, version-mismatched, and corrupt entries are
  never trusted.
- `Prepared` keeps only integer microsecond event timestamps. The sparse click
  path converts its button-event timestamps to seconds on demand.
- Segment and flick/click interval queries use sorted range searches and an
  advancing per-minute cursor instead of rescanning the whole session for
  every interval.
- Reports with at least 250,000 events on a machine with at least four logical
  CPU use scoped threads. Quality and kinematics overlap the event/aggregation
  dependency chain; marker aggregation overlaps per-second and per-minute
  work. Smaller reports remain sequential.
- `--timing` distinguishes the sum of overlapping phase elapsed times from
  actual build wall time.

Parallel and sequential reports are compared after removing timing and
generation fields; the test fixture includes flicks, clicks, a marker, and a
quality warning. The outputs are identical.

A final release verification after the review fixes measured sequential versus
parallel build time at 1,391.5 versus 858.9 ms, report wall time at 1,448.0
versus 915.8 ms, and load-plus-report time at 2,720.6 versus 2,164.8 ms. This
independent sample agrees with the three-run medians above.

### Capture sinks

- Kafka creates its bounded channel immediately. Broker connection, topic
  setup, and producers initialize on the Kafka worker under the existing
  five-second timeout. Initialization is cancellable; shutdown permits a
  three-second drain and caps runtime teardown at 100 ms.
- A terminal Kafka initialization failure is logged and counted once. Later
  envelopes take a quiet counted-drop path; failed accepted deliveries, task
  panics, and bounded-drain abandonment are accounted without counter races.
- JSONL owns its file and `BufWriter` on a dedicated bounded FIFO worker.
  Accepted records remain outstanding until an explicit flush succeeds, then
  are acknowledged together. Periodic one-second flushes cannot starve under
  sustained load.
- A saturated JSONL queue cannot stall UDP or later capture batches. It drops
  the overflowing envelope and exposes `jsonl_queued`, `jsonl_dropped`, and
  `jsonl_abandoned` in periodic and final telemetry. The queue holds 256
  envelopes—about 6.4 seconds at the new 25 ms batch cadence—and shutdown has
  a three-second drain bound. Failure and timeout races claim unresolved work
  exactly once, including buffered and in-flight records.

`BufWriter::flush` moves bytes into the operating-system cache; it is the
process-shutdown guarantee used here, not power-loss persistence. Kafka produce
calls remain concurrent to preserve batching throughput, so consumers should
use `seq_no` rather than assuming completion order across messages or topics.

## Reproducing the measurements

```powershell
# Pure hot paths and loader
cargo bench -p telemouse-core
cargo bench -p telemouse-analyze

# Real analyzer comparison (ignored because it needs a local recording).
# Override the default audit recording with TELEMOUSE_BENCH_SESSION if needed.
cargo test --release -p telemouse-analyze report::tests::real_session_sequential_perf_probe --lib -- --ignored --exact --nocapture
cargo test --release -p telemouse-analyze report::tests::real_session_parallel_perf_probe --lib -- --ignored --exact --nocapture

# Sink latency and failure-accounting probes
cargo test -p telemouse-capture sinks:: -- --nocapture
```

Replay was measured by requesting `/api/session/s-20260904-042856-5006` from
a release `telemouse-viz` process and discarding the response. The live trim
comparison ran the seven typed-array columns through 1,200 frames consuming
eight events per frame in Node/V8. Analyzer medians are three fresh test
processes per mode so peak working set is not contaminated by allocator reuse
from the other mode.

## Validation

- Analyzer: 140 library tests plus 4 integration tests passed; the forced
  parallel/sequential equivalence test passed.
- Capture: 119 tests passed, including blocked/failing writers, exact-once
  timeout accounting, terminal Kafka failure, task panic, and bounded shutdown.
- Visualization: 63 tests passed; embedded JavaScript syntax and focused
  Clippy checks passed.
- Core: 51 tests passed.
- Control panel: 51 tests passed.
- The analyzer doctest passed. The two ignored real-recording probes were run
  separately in release mode and both passed.

The repository-wide result is 429 passing tests, 2 ignored manual probes, and
0 failures. Formatting and diff checks pass. Workspace-wide Clippy remains the
final gate: the sandbox cannot load the existing `serde` artifact, and running
Clippy outside it requires explicit approval.
