---
name: telemouse
description: Answer questions about recorded telemouse sessions (loss, drops, flicks, overshoot, settle, tremor, clicks, sensitivity) and check or drive the running capture/viz/ctl processes, including labelled markers for experiments. Use whenever the user asks about a session, a recording, aim metrics, the sidecar, Kafka loss, or wants capture started, marked or stopped.
---

# telemouse skill

Everything here is read-only unless the user asks to start, mark or stop
something. Prefer the CLI's JSON output over parsing terminal tables. Quote
numbers from the JSON, and say which session id they came from. The full
interface reference is `docs/API.md`; this file is the working subset.

## Binaries and paths

- Release binaries: `target\release\telemouse-analyze.exe`,
  `telemouse.exe` (capture), `telemouse-viz.exe`, `telemouse-ctl.exe`.
  If missing or older than the source, use `cargo run --release -p
  telemouse-analyze -- <args>` instead (slow first time).
- Recordings: `recordings\<session_id>.jsonl` next to `telemouse.toml`, with a
  `<session_id>.meta.json` sidecar per run. `demo-session.jsonl` is a bundled
  sample; exclude it from "my sessions" unless asked.
- Logs (full build only): `logs\ctl.log`, `logs\capture.log`, `logs\viz.log`.
- Use PowerShell on this machine. `Invoke-RestMethod` for HTTP; `curl.exe`
  also works. Add `2>$null` to analyzer calls to keep warnings out of the
  JSON you parse (but read them once: they explain odd metrics).

## Commands

```powershell
# what exists; header-only scan, fast, never loads events
.\target\release\telemouse-analyze.exe list --dir recordings --json

# the headline numbers of one session, ~3 KB of JSON; a bare id works
.\target\release\telemouse-analyze.exe report <session_id> --summary --json-dir recordings\reports

# the full report on disk (large; read only the parts you need)
.\target\release\telemouse-analyze.exe report <session_id> --json-dir recordings\reports --quiet
#   → recordings\reports\<id>.report.json

# one row per session, for comparisons across days
.\target\release\telemouse-analyze.exe trend --dir recordings --json-dir recordings\reports --json
#   add --metric micro.band_ratio_8_12 (any dotted path into the report) for extra columns

# detector thresholds (all optional): --flick-threshold 800 --still-threshold 50
# --locked-only (aim metrics only while pointer-locked) --split-by-marker
```

`report` on a long session takes seconds; always pass `--json-dir` so the
cache (analyzer version + params + recording signature) is reused. Start
with `--summary`; open the full JSON only for per-flick, per-second or
per-marker-segment questions.

## JSON shapes

**`list --json`** → array of `{path, session_id, started_utc_us, duration_s,
events, drops, games[], bad_lines, losses: [[sink, count]...], exit}`.
`drops` = ring drops in capture (events lost before batching). `losses` =
per-sink undelivered envelopes (`kafka`, `udp`, `jsonl`). `exit` = null when
no sidecar, `"running"` when the run is still going or died without a clean
stop.

**`report --summary`** → `{schema: "telemouse-report-summary/2",
session{session_id, started_utc, duration_s, event_count, marker_count,
mouse_cpi, game, sens, cm_per_360, aim_profile_missing}, quality{events,
ring_drops, lost_batches, seq_gaps, bad_lines, pct_within_1ms,
p99_interval_ms, gaps_over_10ms, poll_hz, locked_fraction, clean,
threads_clean, exit, unfinished, capture_profile, sink_losses},
flicks{count, per_minute, amplitude_deg_median, peak_velocity_deg_s_median,
overshoot_ratio_median, overshoot_ratio_p90, settle_ms_median,
settle_ms_p90, time_to_click_ms_median, clicked_fraction},
micro{total_corrections, clean_segment_fraction, tremor_rms_counts_s,
band_ratio_8_12, dominant_hz}, clicks{total, per_minute,
still_click_fraction, click_to_still_ms_median, hold_ms_median,
double_clicks}, kinematics{total_distance_m, distance_cm_per_min,
moving_fraction, path_efficiency_weighted, speed_cm_per_s_median,
speed_cm_per_s_p99}, lifts{count, per_minute, mean_drift_cm},
markers[] ({t_s, label}), markers_total, segments[] ({index, label,
next_label, t_start_s, t_end_s, flicks, flicks_per_min, overshoot_median,
settle_median_ms, tremor_rms_counts_s, path_efficiency, clicks_per_min}),
segments_total, warnings[]}`.
Read `warnings` first. A `segments[i]` runs from the marker `label` to the
marker `next_label` (both empty at the ends of the session): that is how you
tell an `"A start"` stretch from a `"B start"` one without opening the full
JSON. At most the first 12 markers and 12 segments are listed — the `_total`
fields say how many there really are — and an unmarked recording has both
lists empty. `clean` = no data-quality warning; `threads_clean` =
the capture threads all joined. If `aim_profile_missing` is true, every
degree-valued metric uses a fallback sensitivity; say so. Overshoot median
well above 1 suggests sens too high, well below suggests too low.

**Sidecar `<id>.meta.json`** → `{session_id, capture_version,
capture_profile ("debug"|"release"), started_utc_us, ended_utc_us, exit,
clean, events, batches, markers, ring_drops, ring_high_water, abs_frames,
qpc_freq, anchor_uncertainty_us, max_anchor_drift_us, window_ms, coalesce_ms,
poll_hz, sinks: {<name>: {errors, dropped, abandoned}}}`.
Exit vocabulary: `running, interrupt, duration, capture-thread-exited,
shipping-thread-exited, context-thread-exited, capture-thread-stalled,
shipping-thread-stalled, context-thread-stalled, error`. Only `interrupt`
and `duration` with `clean: true` are healthy. A `capture_profile` of
`debug` explains high drop counts.

**Full `report` JSON** top level: `schema, analyzer_version, params, session,
quality, kinematics, flicks, micro, clicks, lifts, markers[] ({t_s,
t_utc_us, label}), segments[] (one per marker interval), per_second[],
per_minute[], warnings[]`. Distribution fields are `{n, mean, median, p90,
p99, max, stddev}`. `flicks.flicks[]` and `clicks.clicks[]` are per-event
tables; usually skip them.

**`trend --json`** → rows of `{session_id, started_utc, duration_s, events,
drops, game, flicks, flicks_per_min, overshoot_median, settle_median_ms,
tremor_rms_counts_s, path_efficiency, clicks_per_min, distance_m, lifts,
cm_per_360, from_cache, extra[]}`.

## Environment check

```powershell
.\target\release\telemouse.exe doctor --json            # one telemouse-doctor/1 document
.\target\release\telemouse.exe doctor --json --config C:\path\telemouse.toml
```

→ `{schema: "telemouse-doctor/1", capture_version, generated_utc_us,
verdict, checks[], config}`. `verdict` is the worst row: `pass` | `warn` |
`fail`. Each row is `{id, status, title, detail, hint?}` — match on `id`,
never on the wording. Rows: `build`, `os`, `config`, `config_overrides`,
`clock`, `screen`, `cursor`, `foreground`, `devices`, `udp`, `recording`,
`kafka`, `kafka_broker_<n>`. `hint` is only there when something can be
done about the row; quote it when reporting. `config` is the resolved
configuration (relative paths made absolute) — it names the mouse's device
strings and the foreground program, so do not paste it whole into a public
issue.

The exit code is `0` whenever doctor produced a document, `fail` rows
included; a non-zero exit means it never got there (usually a
`telemouse.toml` that does not parse — the reason is on stderr). So read
`verdict`, not `$LASTEXITCODE`. Logs go to stderr, so the stdout of
`doctor --json` is safe to pipe into `ConvertFrom-Json`.

## Live processes

All servers bind loopback. Send a `Host` of `127.0.0.1:<port>` or
`localhost:<port>` (the default) or they answer 403.

```powershell
$h = @{ 'X-Telemouse-Ctl' = '1' }                    # required on every POST to ctl
# control panel (7880)
Invoke-RestMethod http://127.0.0.1:7880/api/state          # components, pids, last 120 log lines each, process table
Invoke-RestMethod "http://127.0.0.1:7880/api/state?log_since=<log_seq>"   # only newer log lines
Invoke-RestMethod http://127.0.0.1:7880/api/sessions
# component ids: capture, viz, doctor, trend, report
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7880/api/components/capture/start `
  -Headers $h -ContentType application/json -Body '{"flags":["--no-kafka"],"save":true}'
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7880/api/components/capture/marker `
  -Headers $h -ContentType application/json -Body '{"label":"trial 1 start"}'
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:7880/api/components/capture/stop `
  -Headers $h -ContentType application/json -Body '{"force":false}'

# viz (7879)
Invoke-RestMethod http://127.0.0.1:7879/healthz    # {ok, udp_bound, uptime_s, feed: live|stalled|never, last_datagram_age_s, clients}
Invoke-RestMethod http://127.0.0.1:7879/api/stats  # datagrams, forwarded, parse_errors, lag_drops, latency{p50_us,p99_us,max_us}, seq gaps
Invoke-RestMethod http://127.0.0.1:7879/api/sessions   # [{id, bytes, started_utc_us, ended_utc_us, sidecar}] — sidecar is the meta.json or null
```

Capture flags ctl accepts: `--print`, `--no-kafka`, `--no-udp`, `--record`,
`--no-record`; `doctor` accepts `--json`; `report` accepts `--timing`;
anything else is rejected with 400. `save: true|false` is the
same as `--record`/`--no-record`. Start returns 409 if already running.
Markers: one line of text, at most 120 characters; 400 if blank, 409 if
capture is not running; the label appears in the recording and in the
component log as `--- marker: <label> ---`. Stop is Ctrl-Break then
terminate after `ctl.stop_grace_secs`; an exit code of `-1073741510` is a
clean Ctrl-Break exit, not a failure.

Only start, mark or stop capture when the user asked for it. Never call
`/api/processes/{pid}/kill` unless the user names the pid.

## Recipes

- **"Which sessions lost data?"** `list --json`, filter `losses` non-empty or
  `drops > 0` or `exit` not in (`interrupt`, `duration`). Report id, date,
  duration, and the sink/count.
- **"Summarize my last session."** Newest by `started_utc_us` from `list`,
  then `report <id> --summary --json-dir recordings\reports`. Ten lines max:
  data quality verdict, flick count and overshoot/settle medians, clicks per
  minute and still-click fraction, tremor band ratio, one suggestion.
- **"Compare this week's sessions" / "is my aim more consistent"**: `trend
  --json`, group by `game`, look at `overshoot_median`, `settle_median_ms`,
  `path_efficiency` across rows. Flag any row with `drops > 0` before
  comparing. Sessions are only comparable at the same `cm_per_360`.
- **"Run an experiment" (A/B, trials)**: start capture with `save: true`,
  send a marker at each trial boundary (`"A start"`, `"A end"`, `"B
  start"`...), stop, then `report <id> --summary` and compare its
  `segments[]`: each one names the markers it runs between (`label` →
  `next_label`), so up to 12 intervals need no other file. Go to `report <id>
  --split-by-marker --json FILE` only for more intervals than that, or for
  the per-flick detail. Ask the human to do the physical part between
  markers; say exactly when to start.
- **"Is my setup right?" / "why is the dashboard empty?" / "why is nothing
  recorded?"** `telemouse.exe doctor --json`. Report every row whose
  `status` is not `pass`, with its `detail` and `hint`; if they all pass,
  say so and move on to the live processes below. Through a running panel:
  `POST /api/components/doctor/start` with `{"flags":["--json"]}`, then read
  the JSON out of that component's `log` in `/api/state`.
- **"Is capture healthy right now?"** `/api/state` component `capture`
  running + its `stats`; `/healthz` on viz for `feed`; the last log lines for
  `warn` entries (dropping sink, ring overflow, no input).
- **"Why did that session end?"** Sidecar `exit` + `clean`; if `running`,
  cross-check `logs\capture.log` around `ended_utc_us` for a panic line.

## Gotchas

- Raw counts on the wire; cm and degrees need `mouse_cpi` and a matching
  `[games."<exe>"]` profile in `telemouse.toml`.
- Sessions recorded by a `debug` build drop events at rates a release build
  does not; do not draw conclusions from their `drops`.
- Recordings grow ~150 MB per hour of play; never read a `.jsonl` into the
  context. Use the analyzer.
- Sidecar `ended_utc_us` and `exit: "running"` together mean the last
  5-second rewrite before the process died.
- A marker's timestamp is when the line reached the agent, so a marker sent
  over HTTP is a few ms late; fine for trials, not for per-event alignment.
