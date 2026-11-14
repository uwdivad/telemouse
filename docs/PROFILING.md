# Profiling telemouse with hotpath

*2026-10-12.* What the opt-in [hotpath](https://hotpath.rs) instrumentation
measures in the capture agent and the analyzer, how to run it, how to read a
report, and the rules for adding more. `docs/PERFORMANCE-2026-09.md` and
`docs/PERFORMANCE-2026-09-20.md` hold the measured history this tooling now
makes repeatable; `docs/BENCHMARKS.md` covers the criterion and cpubench
numbers.

## What is profiled, and what it costs

hotpath 0.28 is wired into two of the five telemouse binaries: the capture
agent `telemouse` and the analyzer `telemouse-analyze`, including the analyzer
library that the MCP server links. For every instrumented function it records
call count, average, P95 and total wall time, and with the `hotpath-alloc`
feature the bytes allocated. The two sink queues (JSONL recording, Kafka)
report messages per second, send-to-receive latency and queue depth.

It is off by default and free when off. Each crate has a `hotpath` Cargo
feature; without it every `#[hotpath::...]` attribute and `hotpath::channel!`
call expands to the original code and the `hotpath` crate pulls in no
dependencies. The release zip, the `--no-default-features` build, CI and the
shipped binaries are therefore unchanged. The profiler only runs in a build you
make yourself with `--features hotpath`.

| Crate | Binary | Where the guard sits | What is instrumented |
| --- | --- | --- | --- |
| `crates/capture` | `telemouse` | `#[hotpath::main]` on `main` | the `Shipper` impl block, 13 functions, 2 queues |
| `crates/analyze` | `telemouse-analyze` | `#[hotpath::main]` on `run` | 27 functions across load, series, metrics, report and trend |

viz, ctl and mcp carry no instrumentation. Their work is UI, process
supervision and proxying, not the hot path.

## Quick start

Build with the feature, run as usual, and the report prints to stdout when the
process exits.

```powershell
# Analyzer over one recording; --quiet drops the text report so only the profile prints
cargo run --release -p telemouse-analyze --features hotpath -- report <session-id> --quiet

# Capture for 30 s with every sink on (reads telemouse.toml from the current directory)
cargo run --release -p telemouse-capture --features hotpath -- run --duration-secs 30

# Capture without writing a recording or talking to Kafka
cargo run --release -p telemouse-capture --features hotpath -- run --no-record --no-kafka --no-udp --duration-secs 10

# Add allocation counts
cargo run --release -p telemouse-analyze --features 'hotpath,hotpath-alloc' -- report <session-id> --quiet
```

- Use `--release` (or `--profile profiling`, which adds debug symbols). A dev
  build measures unoptimized code and its ratios do not carry over.
- Set `$env:HOTPATH_METRICS_SERVER_OFF='1'` for plain report runs. A profiled
  build otherwise starts a small HTTP metrics server for the live TUI, and
  Windows may show a firewall prompt for it.
- Capture needs a mouse to produce events. A session with no sinks still
  exercises the decoder, the shipping thread and the context thread, but the
  queue rows only appear when the JSONL or Kafka sink exists.
- The feature is per crate: `-p telemouse-capture --features hotpath` enables
  `telemouse-capture/hotpath`. Enabling it for the whole workspace at once is
  not useful, because the other binaries have no guard.

## What the capture agent profiles

The agent is three threads: T1 reads raw input, T2 batches and ships, T3
samples the desktop context. Almost every measurement sits on T2, because that
is where per-batch work happens; T1 carries exactly one so the latency-critical
thread is not slowed by the profiler. Names below are how the report prints
them.

| Function | Thread | Called | What the time is |
| --- | --- | --- | --- |
| `telemouse::main` | main | once | the whole run; the 100 % row every other row is a share of |
| `raw_input::decode_mouse` | T1 capture | per raw input report (1 kHz to 8 kHz) | turning one `RAWMOUSE` frame into a `RawEvent`; the only T1 cost measured |
| `Shipper::flush` | T2 shipping | per batch window, about 40/s at 25 ms while events flow | drain the pending events into a batch envelope and deliver it to every sink; the per-batch cost of T2 |
| `Shipper::encode` | T2 | per envelope | serialize one envelope, including the warn-and-count path on failure |
| `EnvelopeEncoder::encode` | T2 | per envelope | the serde_json part alone; the gap to `Shipper::encode` is bookkeeping |
| `Shipper::deliver` | T2 | per session or marker envelope | encode plus fan-out for one non-batch envelope |
| `Shipper::deliver_encoded` | T2 | per envelope | hand the encoded bytes to each sink; the fan-out step alone |
| `Shipper::send_live` | T2 | heartbeats, session re-sends | delivery to the UDP sink only |
| `Shipper::tick` | T2 | once a second | sink housekeeping |
| `Shipper::close` | T2 | once at shutdown | final flush, waiting for the JSONL writer, the bounded Kafka drain |
| `sinks::fan_out` | T2 | per envelope | the loop over sinks; what one slow `send` costs the batch |
| `sinks::send_to` | T2 | per targeted envelope | single-sink delivery |
| `sinks::tick_all` | T2 | once a second | the loop over every sink's `tick` |
| `UdpSink::send` | T2 | per envelope | the `sendto` on the loopback or LAN socket; would-block and oversize drops are counted here |
| `JsonlSink::send` | T2 | per envelope | copying the line into the recording queue; a rising P95 means the writer thread is behind |
| `KafkaSink::send` | T2 | per envelope | `try_send` into the Kafka queue; only in a `kafka` build |
| `shipping::record_latency` | T2 | per batch | the capture-to-ship latency histogram update; only in an `observability` build |
| `meta::write` | T3 and main | every 5 s and at exit | serialize the sidecar, write the temp file, rename it into place |
| `Sidecar::refresh` | T3 context | every 5 s | the whole sidecar refresh including the stats snapshot |
| `context_thread::sample` | T3 | 4 per second | foreground window, Toolhelp process snapshot, pointer-lock check; the 7 ms kernel floor from the CPU bench lives here |
| `Reporter::maybe_report` | T3 | 4 per second, reports every 5 s | the stats report line and its escalation logic |

Left out on purpose: `Shipper::new` and `Shipper::report_failure` (noise),
`ShipperCore::push` (per event, trivial), the rtrb ring itself, and the JSONL
writer loop and Kafka forwarder, which are single long-lived calls whose cost
shows up as queue depth instead.

## What the analyzer profiles

One measurement per analysis phase, so the report lines up with the analyzer's
own `--timing` table, plus the inner numeric loop and the parallel loader's
per-chunk work.

| Phase | Function | Called | What the time is |
| --- | --- | --- | --- |
| run | `telemouse_analyze::run` | once | everything after argument parsing, including render and file output |
| load | `load::load_session` | once per report | read and parse the JSONL plus the sidecar; sequential below the size threshold, one chunk per core above it |
| load | `load::parallel::parse_range` | once per worker chunk, 0 for small files | parsing one byte range; a P95 far above the average means chunks are uneven |
| load | `load::scan_session` | once per recording in `list` | header and line classification without keeping events |
| load | `load::scan_dir` | once in `list` and `trend` | the whole listing including the index cache |
| prepare | `series::prepare` | once | resample events onto the grid, smooth, find segments |
| prepare | `series::sg_run_into` | once per grid run per lane, hundreds to thousands | the Savitzky-Golay kernel over one run; the inner numeric hot loop |
| prepare | `series::movement_segments` | once | the moving-versus-still split |
| metrics | `report::build_with_parallelism` | once | prepare, all metric groups and the summary |
| metrics | `report::compute_metrics` | once | the metric groups; two of them run on a second thread for large sessions, so this is wall time and the children overlap |
| metrics | `quality::compute_with_warnings` | once | polling histogram, gaps, drops, batch loss |
| metrics | `kinematics::compute` | once | velocity, acceleration, jerk, distance, path efficiency |
| metrics | `flicks::compute`, `flicks::detect_with_cells` | once each | flick detection and its aggregation |
| metrics | `micro::compute_full`, `micro::segment_pass`, `micro::tremor_pass` | once each | sub-movements, then the tremor residual and Welch band power |
| metrics | `clicks::compute` | once | trigger discipline |
| metrics | `lifts::compute` | once | repositioning lifts |
| metrics | `markers::intervals`, `markers::segment_reports` | once each | marker segmentation and the per-interval headline metrics |
| metrics | `per_second::compute`, `per_minute::compute` | once each | the longitudinal tables |
| output | `Report::render` | once unless `--quiet` | the text report |
| output | `Report::write_json`, `Report::write_csvs` | when `--json` or `--csv-dir` is given | the file writes |
| trend | `trend::report_for` | once per report | cache check, then build or read back |
| trend | `trend::read_cache`, `trend::write_cache` | per cached session | the `<id>.report.json` round trip |
| trend | `trend::compute` | once per `trend` | every session in the folder |

The guard sits on `run`, not on `main`. The analyzer's `main` leaves through
`std::process::exit` to skip freeing a multi-gigabyte heap, and that exit would
also skip the report, which prints when the guarded function returns.

## The two queues

Both sink queues are created through `hotpath::channel!`, so a run with the
matching sink enabled adds a channels section to the report. The queues are
where the field audit's silent loss lived, and the point of wrapping them is to
see backpressure while it happens rather than in the sidecar afterwards.

| Label | Channel | Bound | Producer to consumer | What a bad reading looks like |
| --- | --- | --- | --- | --- |
| `jsonl_queue` | `std::sync::mpsc::sync_channel<Vec<u8>>` | 256 lines | T2 shipping to the `telemouse-jsonl` writer thread | Max queue near 256, or Recv/s below Sent/s for long: storage cannot keep up, and `jsonl_dropped` in the stats line and the sidecar's loss column will confirm it |
| `kafka_queue` | `tokio::sync::mpsc::channel<KafkaJob>` | 128 jobs | T2 shipping to the `telemouse-kafka` forwarder (its own current-thread tokio runtime) | Max queue pinned at 128 with Sent/s above Recv/s: the broker or the network is behind; this is the 508 k dropped batches of the 2026-09-12 field audit, now visible live |

How to read the channel columns:

- **Sent/s, Recv/s** are lifetime averages since the queue's first message, so
  a bursty session reads lower than its in-burst rate.
- **Queue / Max queue** are the in-flight depth now and its peak. The peak
  against the bound is the number that matters.
- **Latency** is send-to-receive time per message. For `jsonl_queue` that is
  how long a line waits before the writer picks it up; a P95 climbing into the
  hundreds of milliseconds means the writer is blocked on the disk.
- **Inst** is how many channel instances the row aggregates. Both queues are
  created once per run, so it is 1.

In struct fields and function signatures the endpoints are typed
`hotpath::wrap::std::sync::mpsc::SyncSender` and
`hotpath::wrap::tokio::sync::mpsc::Receiver`. Without the feature those paths
are plain re-exports of the std and tokio types, which is why the same code
compiles in both flavours. The tests build their channels through the same
macro so the parameter types line up.

Not wrapped: the marker channel from T1 and T3 into T2, because markers are
rare and its sender is threaded through five signatures, and the rtrb ring
between T1 and T2, which is not a library hotpath supports. The ring's
high-water mark is in the agent's own stats line (`ring_high_water`).

## Reading a report: the 2026-10-12 runs

(The full-length pass taken the same day, with a 20-minute capture run, the
analyzer over every recording and the ranked list of fixes, is
`docs/PERFORMANCE-2026-10-12.md`.)

Two rules make the timing table readable. The `% Total` column nests:
`report_for` contains `build_with_parallelism`, which contains
`compute_metrics`, so child rows are never added to their parents. And `Avg`
against `P95` separates steady cost from spikes: an average above the P95 means
a few outliers carry the total.

**Analyzer.** `report s-20260922-225752-d8c6 --quiet`, a 2.6 MB recording of
37,409 events over 89 s, finished in 24.9 ms. Pulling the non-overlapping
phases out of the nested table:

| Phase | Time (ms) | Share of run |
| --- | --- | --- |
| `load::load_session` | 8.70 | 35 % |
| `kinematics::compute` | 6.10 | 25 % |
| `series::prepare` | 3.25 | 13 % |
| `micro::compute_full` | 1.74 | 7 % |
| `quality::compute_with_warnings` | 1.00 | 4 % |
| other metric groups together | 1.88 | 8 % |
| startup, summary, cache check | 2.2 | 9 % |

Kinematics is the largest single metric group, more than half of
`compute_metrics`. `series::sg_run_into` ran 228 times at 10.95 µs average but
42 µs P95, which is the spread of grid-run lengths, not a problem. The
analyzer's own log lines (`elapsed_ms=8.0` for the load, 3.3 for prepare, 6.1
for kinematics) agree with the hotpath rows to within a tenth of a millisecond,
which is the cross-check that the instrumentation is measuring what it claims.
On the 1.2 GB recording from the 2026-09-20 performance survey the load share
is far larger; this file is too small to show the load-bound shape.

**Capture.** `run --no-record --no-kafka --no-udp --duration-secs 5` saw 1,233
events in 89 batches. The whole agent is below one percent of its 5.04 s wall
time:

- `context_thread::sample`, 20 calls, 12.1 ms total, is the single largest
  cost at 0.24 %. Its average (605 µs) is well above its P95 (128 µs): the
  first Toolhelp snapshot is slow and the rest are not.
- `Shipper::flush` cost 6.65 µs per batch, of which `EnvelopeEncoder::encode`
  is 4.64 µs. Serialization is most of what T2 does per batch.
- `raw_input::decode_mouse` cost 80 ns per report, 99 µs over the run. T1's
  measured work is negligible; its real cost is the kernel wake, which no
  function timer sees.
- The call counts check the model: 89 batches gave 89 `record_latency` calls,
  93 encodes (89 batches plus the session envelope and three heartbeats), one
  `deliver` for the session envelope.

In the threads section `hp-threads` at 20 % CPU is the profiler's own sampling
thread, not telemouse. The three worker threads read 0.0 % at this load, and
peak RSS was 17 MB.

## Modes and options

Everything below is set in the environment of the profiled process; no code
changes. Both binaries use hotpath's defaults (P95 only, 15 function rows,
table output).

**Allocation profiling.** `--features 'hotpath,hotpath-alloc'` adds an
`alloc-bytes` section with bytes allocated per function. It is exclusive by
default: a function's own allocations, with nested instrumented calls taken
out. `HOTPATH_ALLOC_CUMULATIVE=1` includes them, and
`HOTPATH_ALLOC_METRIC=count` sorts by allocation count instead of bytes. The
macro installs the counting global allocator itself; neither crate declares its
own allocator, so there is nothing to reconcile. For capture, steady-state
allocations per batch should be near zero because the envelope encoder reuses
its buffer; bytes on `Shipper::flush` in an alloc report are a finding.

**Environment variables that matter here.** The full list is at
[hotpath.rs/configuration](https://hotpath.rs/configuration).

| Variable | Use |
| --- | --- |
| `HOTPATH_OUTPUT_FORMAT` | `table` (default), `json`, `json-pretty`, `none` |
| `HOTPATH_OUTPUT_PATH` | write the report to a file instead of stdout; needed when the agent is started by ctl, whose log captures stdout, and for the MCP server |
| `HOTPATH_REPORT` | which sections: `functions-timing,channels`, or `auto,-threads` |
| `HOTPATH_FUNCTIONS_LIMIT` | rows in the functions table, default 15 (the header prints `15/21` when rows are cut); `0` shows all |
| `HOTPATH_FUNCTIONS_NAME_DEPTH` | module segments kept in names, default 2 (`series::prepare`); `0` prints the full path |
| `HOTPATH_FOCUS` | substring or `/regex/` filter on function names, e.g. `Shipper` or `/^(load\|series)/` |
| `HOTPATH_SHUTDOWN_MS` | force exit and report after a fixed time; for capture prefer `--duration-secs`, see the caveats |
| `HOTPATH_METRICS_SERVER_OFF` | `1` disables the metrics server on `localhost:6770` (`HOTPATH_METRICS_PORT` moves it) |
| `HOTPATH_FUNCTIONS_TIME_SAMPLING_RATE` | measure a fraction of calls when a measurement sits on a very hot path |
| `HOTPATH_USER_METADATA` | `commit=abc123,window=25`: tags printed in the report header and stored in the JSON |

**JSON for comparisons.** `HOTPATH_OUTPUT_FORMAT=json
HOTPATH_OUTPUT_PATH=tmp/report.json` writes a machine-readable report; the same
recording through two builds gives two files to diff. The JSON carries the git
commit when the binary was built inside the checkout.

**Live TUI.** `cargo install hotpath --features tui --version '^0.28'`, then
`hotpath console` in one terminal and the profiled agent in another. The TUI
reads the metrics server, so leave it on for this. It is the right mode for a
long capture session: watch `jsonl_queue` depth while moving the mouse hard, or
`context_thread::sample` while alt-tabbing between a game and the desktop. See
[hotpath.rs/profiling_modes](https://hotpath.rs/profiling_modes).

## Caveats

- **The profiled build opens a port.** With the feature on, the process
  listens on `localhost:6770` for the TUI. It is loopback only, but Windows
  may show a firewall prompt the first time. This never applies to the shipped
  binaries, which do not contain the profiler. Set
  `HOTPATH_METRICS_SERVER_OFF=1` for report-only runs.
- **Wall time, not CPU time.** A row that waits counts the wait:
  `load_session` on the disk, `Shipper::close` waiting for the writer thread,
  a blocking `UdpSink::send`. hotpath has a `hotpath-cpu` sampling mode, but
  it is not wired here and would need the `samply` profiler installed.
- **Profiler overhead is in the numbers.** Each measurement costs on the order
  of 100 ns plus a queue push, and the `hp-threads` row at about 20 % of one
  core is hotpath's own sampler. Sub-percent figures for the agent include that
  noise. For absolute CPU cost of the agent the references remain its own
  stats line and the `tools/cpubench` harness.
- **T1 is mostly invisible.** `decode_mouse` is its only measurement. The
  kernel wake per report and `GetRawInputData` sit outside any function timer;
  the CPU bench measures those.
- **Children can exceed the parent.** `compute_metrics` forks two metric
  groups onto a second thread for large sessions, so its children's totals can
  sum to more than its own wall time.
- **`HOTPATH_SHUTDOWN_MS` skips the ordered teardown.** It exits the process
  when the timer fires, so the final partial batch, the bounded Kafka drain and
  the sidecar's `exit` field never happen and the sidecar stays at `running`.
  Use `--duration-secs` or Ctrl-C for capture.
- **Runs read the live `telemouse.toml`.** On this machine that means Kafka on
  and the viz bind on the LAN. Pass `--no-kafka`, `--no-udp` or `--no-record`
  when only local numbers are wanted; a profiled run with recording on writes a
  real session into `recordings/`.
- **Build one binary at a time.** Cargo unifies features per build, so
  `--workspace --features telemouse-capture/hotpath` would also activate the
  analyzer library's macros inside `telemouse-mcp`, which has no guard. Always
  `-p <crate> --features hotpath`.
- **Early exits print no report.** The analyzer's `exit(2)` for an Explorer
  double-click and `cargo run` without a subcommand return before the guard;
  the capture agent's `explain_and_wait` path returns normally and prints an
  empty one.
- **Duplicate labels fail late.** `cargo check` accepts them; `cargo build` or
  `cargo test` reports `symbol ... is already defined`.
- **Not instrumented.** viz, ctl and mcp; the JSONL writer loop and the Kafka
  forwarder (long-lived calls, visible through their queues instead); the rtrb
  ring; the stats snapshot.

## Extending the instrumentation

Add measurements where a question needs them and nowhere else; the initial set
is a starting point, not a coverage goal. The rules that keep both build
flavours compiling:

1. Never gate a hotpath macro behind `cfg`. Every macro is a no-op without the
   feature, so `#[cfg_attr(feature = "hotpath", ...)]` buys nothing and
   clutters the code. Other `cfg` attributes may sit above it, as
   `shipping::record_latency` shows with its `observability` gate.
2. Free functions take `#[hotpath::measure]`. A method inside an `impl` block
   that is not under `measure_all` takes
   `#[hotpath::measure(impl_type = "TypeName")]`, so the report prints
   `Type::method` and CPU attribution matches the symbol.
3. `#[hotpath::measure_all]` on an `impl` block or an inline `mod` measures
   everything in it; exclude constructors and rate-limited warning paths with
   `#[hotpath::skip]`. Trait impls are measured for timing but not for CPU
   sampling.
4. `hotpath::measure_block!("label", { ... })` times an ad-hoc block. Literal
   labels are unique per crate across `measure_block!`, `measure(label = ...)`
   and `channel!(label = ...)`. A duplicate fails `cargo build` and
   `cargo test` but not `cargo check`, so verify with a build.
5. Channels: `hotpath::channel!(constructor, label = "name")` at the creation
   site. A bounded std channel also needs `capacity = n`, equal to the
   constructor argument, because the wrapper rebuilds the channel from it. Any
   struct field or signature that names the endpoint type switches to the
   `hotpath::wrap::` path, and tests that construct the channel by hand go
   through the macro too. Locks have `hotpath::mutex!` and
   `hotpath::rw_lock!`; none are wrapped today because the agent's only
   `Mutex` is a warning throttle.
6. Keep T1 light. Each measurement costs on the order of 100 ns per call, and
   T1 runs up to 8,000 times a second; `decode_mouse` is the one measurement
   it carries on purpose.
7. Another binary follows the same recipe: `hotpath = { workspace = true }`,
   the four passthrough features, and `#[hotpath::main]` on its entry
   function, placed under `#[tokio::main]` when there is one. For the MCP
   server, stdout is the protocol channel, so a profiled run there must set
   `HOTPATH_OUTPUT_PATH`.

Before calling a change done, build and test all three flavours and the CI
list:

```powershell
cargo check -p <crate> --all-targets                        # feature off
cargo check -p <crate> --all-targets --no-default-features  # minimal
cargo build -p <crate> --all-targets --features hotpath     # feature on; catches duplicate labels
cargo test  -p <crate> --features hotpath                   # the wrappers under test
# then the usual: cargo fmt, both clippy runs, cargo test --workspace --locked, the Node tests
```

## Regression policy and CI

`hotpath/policy.toml` is the default policy file from hotpath, unchanged.
Nothing in telemouse's own CI reads it today; it is the input for
[hotpath Cloud](https://hotpath.rs/cloud), the pull-request regression check,
which is not set up for this repository (no token, no workflow step). It is in
place so that turning the check on later is a workflow change only.

| Key | Value | Meaning |
| --- | --- | --- |
| `functions.min_percent_total` | 2 | only functions that take at least 2 % of the run are judged; the rest is noise |
| `functions.alloc.judged` | true | allocation changes fail a pull request |
| `functions.alloc.min_percent_change` | 20 | a 20 % move in average or total bytes counts as a regression |
| `functions.timing.judged` | false | timing is reported on the pull request but never fails it, since CI runners are too noisy for wall time |
| `functions.timing.min_percent_change` | 50 | the threshold at which timing would be flagged if judging were turned on |
| `min_calls` | 1 | no minimum call count before a function is compared |

Every key is documented at
[hotpath.rs/regression_policy](https://hotpath.rs/regression_policy). Leave
the defaults until there is a baseline to judge against. For a deterministic
comparison between two commits without the cloud, run the same recording
through the analyzer with `HOTPATH_OUTPUT_FORMAT=json` and
`HOTPATH_OUTPUT_PATH` on each commit and diff the two files;
`cargo bench -p telemouse-analyze` remains the criterion-based micro-benchmark
for the math kernels.
