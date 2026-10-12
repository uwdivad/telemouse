# Sensor-skip detection — proposal

Written 2026-09-29. **Status: proposal, nothing built.** Records what
telemouse can and cannot see when the mouse's sensor misses movement, and a
design for detecting it, so the work can be picked up later without redoing
the investigation.

## The question

Does telemouse account for the sensor skipping, and does the viz show
skipped or missed inputs?

No. Telemouse detects loss *inside its own pipeline* and shows it. Nothing
looks at whether the mouse itself failed to report motion.

## What is detected today

| Kind of loss | Where it is counted | Where it shows |
|---|---|---|
| Ring-buffer drops: T1 found the SPSC ring full and discarded an event | `drops_since_last` on each batch (`crates/capture/src/shipping.rs`) | viz: red timeline notch, **Drops** tile, one toast per live session; analyze: `ring_drops` warning |
| Lost batches: a gap in `seq_no` between capture and the consumer | viz engine (`app.js`, `lostBatches`), `quality::compute` | viz: same notch, **Lost** tile; analyze: `lost_batches` warning |
| Sidecar count ≠ file count | `quality.rs`, `SidecarSummary` | analyze warning only |

All of these happen after Windows has delivered the `WM_INPUT` frame. A
report the mouse never sent never reaches any of them.

## What a sensor failure looks like in raw input

Raw input sees exactly the HID reports that cross the USB bus. The sensor
failing shows up as one of:

| Failure | What the stream shows | Detectable? |
|---|---|---|
| Tracking lost for a few polls (surface, lift height, dust) | No reports for a stretch in the middle of fast movement | Yes, from shape: see D1, D2 |
| Tracking recovers with a bad correction | One report with an outsized delta, sometimes in the wrong direction | Yes: D3 |
| Movement faster than the sensor's max IPS | Deltas clipped or under-reported, with no gap | Only weakly (a speed plateau); no ground truth in the stream |
| Firmware drops the polling rate | Longer intervals throughout | Already covered: `polling.hz_min` / `hz_max` per minute in `quality.rs` |
| Intentional lift | No reports, bracketed by a drift and a return sweep | Already inferred by `lifts.rs`; must not be counted as a skip |

The analyzer does count intervals over 10 ms (`gaps_over_10ms`,
`quality::GAP_MS`), but deliberately doesn't warn about them
(`quality.rs`, end of `warnings()`): a still hand also reports nothing, so
most gaps are idle gaps. The skip detectors below exist to tell the two
apart.

## The timestamp problem

With the default `batch.coalesce_ms = 8`, the capture thread reads reports
in drains (`crates/capture/src/raw_input.rs`, module doc). Only the first
report of a burst has an observed arrival time. After that, while the hand
keeps moving, the loop runs on a timer and `DrainStamper::spread` spaces a
drain's `n` reports evenly over the period since the previous drain.

So in steady motion, **a missed poll doesn't produce a longer interval**. It
produces one report fewer, and every spacing in that drain stretches by a
fraction. Any detector that reads the intervals can only see a skip longer
than roughly one drain period, and only when the stream falls back to the
queue wait.

Two ways around it:

- Record with `coalesce_ms = 0`: one read per report and real arrival times,
  at a CPU cost: the `raw_input.rs` module doc puts one read per report at
  ~2.8% of a core at 1 kHz, against ~0.5% batched (numbers in
  `docs/BENCHMARKS.md`). The session envelope records `coalesce_ms`, so the analyzer can
  tell which case it has.
- Count reports instead of reading intervals (D2), which works either way.

## Proposed detectors

All of these go in the analyzer and run on recorded sessions. Every threshold
is a `series::Params` field, like the lift thresholds, so it is an analyzer
flag and not a `telemouse.toml` key (a new toml key forces a rebuild of every
binary; see `CLAUDE.md`).

**D1: gap during motion** (`coalesce_ms = 0` sessions only). An interval of
at least `k ×` the modal polling interval (`k` ≈ 3) where smoothed speed is
above a floor on *both* sides and the heading before and after agrees
(cosine above a threshold). A hand that is stopping slows down first, and a
lift reverses direction, so both are excluded. Report the count and the
missing time in ms.

**D2: count deficit** (works at any `coalesce_ms`). Over each span of
sustained motion (speed above the floor, no idle gap), compare the report
count against `span × polling.hz`. The spread stamping hides *where* a
report went missing, but not *that* it did. Report missing reports per
minute of motion. Needs `polling.hz` to be stable (`polling.stability`
high); otherwise the expected count is a guess and the detector should say
so instead of reporting a number.

**D3: delta spike**. A single report whose `|dx, dy|` is more than `m ×` the
median of its neighbours (window of ~5 each side, `m` ≈ 8), or implies a
speed above a physical ceiling at the session's CPI. Flicks ramp up over
several reports, so a one-report spike is not a flick. Also worth checking a
spike whose direction opposes both neighbours.

**D4 (optional): zero reports during motion**. Frames with `dx = dy = 0` and
no button or wheel change, in the middle of fast movement. Some mice send
these normally at low speed, so this needs real data before it earns a
place.

**Considered and rejected: a counter in capture.** T1 knows each drain's
report count and could flag short drains at no cost. But `DrainStamper`
estimates the report interval *from* those counts (`gap / n`), so a short
drain partly adjusts the estimate it would be measured against. It would
also change the wire format, which is `core`'s public contract. The analyzer
can compare against the whole session's modal rate instead, which doesn't
have that problem.

## Where it would land

| Piece | Change |
|---|---|
| `crates/analyze/src/sensor.rs` (new) | D1–D3 over `series::Prepared`; excludes spans `lifts.rs` claimed |
| `quality.rs` / `report.rs` | `QualityReport` gets a `sensor` block: counts, missing ms, missing reports per motion-minute, whether D1 ran (`coalesce_ms = 0`), and the timestamps of each event (capped) |
| Warnings | Like gaps: counts are always reported; a warning only above a rate threshold, so one skip in an hour does not mark the session unclean |
| Summary schema | `telemouse-report-summary/3`; bump `SUMMARY_SCHEMA` in `report.rs` and `crates/ctl/src/manager.rs`, update the MCP `session_summary` description and `docs/API.md` |
| viz replay | Timeline notches in their own colour (not the red `.notch.drop`), with a tooltip naming the detector. Either port D1–D3 to the JS engine with Node tests in `js-tests/`, or load the analyzer's event list; porting keeps replay self-contained |
| viz live | Later, if at all. D3 is cheap enough to run on each batch; D1/D2 need a settled polling estimate |
| ctl panel report card | One line: "sensor: N skips, M spikes" |

No feature flag needed: this is pure analysis with no logging, stats, Kafka
or sidecar side effects. Anticheat posture is unaffected: the analysis only
reads data that is already recorded.

## Validating it

Synthetic streams are straightforward: `analyze/src/testutil.rs`'s
`StreamBuilder` can build fast movement with reports removed, a single
spike, a hand slowing to a stop, and a lift. Those four are the unit tests
that matter, since the last two must stay at zero.

Real ground truth is harder. The plan is to record `coalesce_ms = 0`
sessions on a surface known to make the sensor fail (glass, a mirror, a
glossy magazine) and at a deliberately high lift-off distance, next to a
control session on the normal pad, and check that the counts separate. Mark
the moments the sensor lost tracking with markers (F9 or a stdin marker),
so detections can be matched to them.

## Open questions

- Values for `k`, `m` and the speed floor. They depend on CPI and polling
  rate, so they may need to scale with `polling.hz` and the session's CPI
  instead of being fixed.
- Whether D2 is trustworthy at 4 kHz / 8 kHz, where USB jitter is a larger
  share of the interval.
- Whether D4 is a real signal on any common mouse.
