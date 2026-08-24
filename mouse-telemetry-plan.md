# Mouse Telemetry — Implementation Plan

Goal: capture raw mouse deltas during gaming with microsecond-quality timestamps, stream to Kafka, drive (a) a low-latency live visualization, (b) full movement **replay**, and (c) a durable dataset for performance analysis.

---

## Phase 0 — Decisions locked in

| Concern | Choice | Why |
|---|---|---|
| Capture language | Rust (`windows`, `rtrb`, `rdkafka` crates) | No GC pauses, typed Win32, single binary |
| Capture mechanism | Raw Input API (`RIDEV_INPUTSINK`, `WM_INPUT`) | True HID deltas, pre-acceleration, passive/anticheat-safe |
| Timestamps | `QueryPerformanceCounter` at receipt + one QPC↔UTC anchor per session | Monotonic, µs precision, replayable |
| Transport | Kafka (durable log) + localhost UDP/WebSocket dual-write (live path) | Viz latency never gated on Kafka |
| Storage | TimescaleDB (or Parquet + DuckDB to start) | Right-sized for single user |
| Viz | Browser, Canvas/WebGL, second monitor | No in-game overlay → zero anticheat surface |

---

## Phase 1 — Capture agent (Rust)

**Threads:**
- **T1 capture (hot path):** message-only window (`HWND_MESSAGE`) → register raw input for mouse usage page with `RIDEV_INPUTSINK` → on `WM_INPUT`: read `dx`, `dy`, button flags, wheel; QPC timestamp; push fixed-size struct to lock-free SPSC ring buffer (`rtrb`); return. Zero heap allocation after startup. Fallback to `GetRawInputBuffer` batch-drain if running an 8KHz mouse.
- **T2 shipping:** drain ring → assemble 25ms batches → serialize → async Kafka produce + local UDP fan-out. Counts and reports ring-buffer drops (should be zero; if not, it's a visible data-quality metric, never a capture stall).
- **T3 context (slow, 250ms tick):** poll `GetForegroundWindow` → process name (which game is active), screen resolution, and a pointer-lock heuristic (cursor position frozen while deltas flow ⇒ in-game raw input mode). Read-only, no handles into the game process.

**Event record (per input event):**
```
ts_qpc      u64   // performance counter at receipt
dx, dy      i32   // raw HID counts
buttons     u16   // bitfield: L/R/M down/up, X buttons
wheel       i16   // wheel delta if any
```

**Batch envelope (what actually goes to Kafka):**
```
session_id, seq_no, ts_anchor (first event, UTC-mapped),
game (process name), pointer_locked (bool),
screen_w, screen_h, cursor_x, cursor_y (sampled, desktop mode only),
events: [event record...]
drops_since_last (u32)
```

**Session config (produced once per session to a compacted `sessions` topic):**
mouse CPI, per-game sensitivity + yaw/pitch conversion factors (e.g., CS2: `deg = counts × sens × 0.022`), monitor setup, QPC frequency, QPC↔UTC anchor. Everything needed to reconstruct physical cm and aim-space degrees **later** — keep raw counts on the wire, derive in consumers.

**Extras:** global hotkey (e.g., F9) producing a `marker` event — lets you tag "round start / clutch / tilt" moments manually before any game-API integration exists.

Milestone: binary prints live batches; deltas visibly flow while a game with raw input is running; drop counter stays at 0 at 1KHz polling.

## Phase 2 — Kafka topics

- `mouse.events` — the batch stream. Key = `session_id` (per-session ordering in one partition). `linger.ms=25`, zstd, retention 30d+.
- `mouse.sessions` — compacted; session config records.
- `mouse.markers` — hotkey markers + (later) game-state events, so game context evolves without touching the hot schema.

Start with JSON for debuggability; move `mouse.events` to a binary schema (protobuf/flatbuffers) once stable — at 1KHz the payload savings are real.

## Phase 3 — Live visualization

Small consumer (any language) bridging local UDP → WebSocket → browser page:

- **Desk-space panel:** integrate `dx/dy` → hand path, scaled to real cm via CPI. Trail with time-decay, color by velocity. This is the "movement of the physical mouse" view.
- **Aim-space panel:** integrate yaw/pitch via per-game conversion → crosshair path in degrees. This is the "movement of the center of screen" view. Wrap yaw at ±180°.
- Click events as bursts/rings; wheel as ticks; live stat readouts (current velocity, cm this session, APM-ish click rate).
- Decay/reset controls; a "recenter integrator" key since integrated positions drift unbounded across long sessions.

Latency target: event → pixels in <10ms. Achievable because this path never touches Kafka.

## Phase 4 — Replay engine

Deltas + QPC timestamps are a *complete* recording — replay is just re-integration at original timing:

- Reader pulls a session from Kafka (or storage), reconstructs the event timeline via QPC→UTC anchor.
- Re-render both panels with a transport bar: play/pause, scrub, 0.25×–8× speed, jump-to-marker.
- Same rendering code as live viz (live = replay at 1× of "now") — build once, feed from two sources.
- Export: render a session to video/GIF for sharing clips of your aim.

## Phase 5 — Storage + analysis

- Consumer sinks `mouse.events` into TimescaleDB hypertable (or hourly Parquet files) keyed by `(session_id, ts)`.
- Derived tables built in batch: per-second aggregates, detected flicks, detected sub-movements (schemas below fall out of the metrics list).
- Notebooks (DuckDB/pandas) over the derived tables; later a small dashboard (Grafana over Timescale is nearly free).

---

## Metrics catalog for performance analysis

Raw counts + timestamps + clicks are enough to derive all of these. Compute in consumers, never in the capture agent.

### Kinematics (per sample / per second)
- **Velocity, acceleration, jerk** in counts/s and cm/s (smoothed; e.g., Savitzky–Golay to avoid amplifying sensor noise).
- **Distance traveled** per session/minute — total hand travel in meters; also in aim-space degrees.
- **Path efficiency**: net displacement ÷ total path length over a movement. 1.0 = perfectly straight pull; chronically low = wobble or over-correction.

### Flicks (the headline aim metric)
Detect: velocity crosses a high threshold → movement segment until velocity returns near zero.
- **Amplitude** (degrees in aim-space), **peak velocity**, **duration**.
- **Overshoot ratio**: distance of direction-reversal correction after the flick ÷ flick amplitude. The single most diagnostic aim metric — trends here tell you if your sensitivity is too high/low.
- **Settle time**: end of ballistic phase → velocity stabilized near zero.
- **Time-to-click**: flick start → button-down. Paired with overshoot ≈ a Fitts'-law-style speed/accuracy profile of your aim, no game API needed.

### Sub-movements & micro-control
- **Correction count** per aiming sequence (velocity zero-crossings / direction reversals): one big pull + one micro-correct = clean; 4–5 stutters = noisy tracking.
- **Tremor/jitter**: high-pass the velocity signal, measure RMS power (and FFT band ~8–12Hz). Sensitive fatigue/caffeine indicator.
- **Micro-adjustment size distribution**: histogram of small movement amplitudes — your "fine aim" fingerprint.

### Trigger discipline (clicks)
- **Pre-click stability**: mouse velocity in the 50–100ms window before button-down. Shooting while still vs. spraying while dragging.
- **Click-to-still latency**: last significant movement → click.
- **Button hold durations**, double-click intervals, clicks/min.

### Positioning & habits
- **Repositioning lifts** (inferred): true lifts are invisible to HID, but slow sustained one-direction drift followed by fast opposite movement ≈ running out of pad. Count per session → mousepad usage profile.
- **Aim-space heatmap**: dwell time by yaw/pitch — do you check the same angles, over-favor one side?
- **Desk-space heatmap**: where on the pad your hand lives.

### Session-level / longitudinal
- **Fatigue curves**: overshoot ratio, tremor RMS, path efficiency vs. minutes-into-session. Answers "when do I degrade?"
- **Warmup curve**: same metrics over the first N minutes across sessions — how long until you hit baseline.
- **Consistency**: day-to-day variance of your flick profile; time-of-day effects.
- **Sensitivity experiments**: config changes are recorded in `mouse.sessions`, so A/B-ing sensitivities against overshoot/settle metrics becomes a real experiment with data.

### Data quality (trust the numbers)
- Inter-event interval distribution (should cluster at 1ms for 1KHz — gaps = stalls), ring-buffer drop counts, batch latency, timestamp monotonicity violations.

### Later, with game context (markers → CS2 GSI / demo parsing)
Per-round segmentation; movement signatures in clutch vs. eco rounds; pre-death vs. pre-kill aim comparison; correlating tremor/fatigue metrics with actual K/D over time.

---

## Build order

1. **Phase 1** capture agent → deltas printing, drops = 0. *(The only hard part; everything after is plumbing and fun.)*
2. **Phase 2** Kafka wiring + session config topic.
3. **Phase 3** live viz, desk-space panel first, then aim-space.
4. **Phase 5-lite** sink to Parquet immediately (cheap) so data accumulates from day one — even before analysis code exists.
5. **Phase 4** replay (reuses viz).
6. Metrics notebooks, then automate the good ones into derived tables + dashboard.

Step 4's ordering is deliberate: start hoarding raw data early. Every metric above is computable retroactively from raw counts — but only if you kept them.
