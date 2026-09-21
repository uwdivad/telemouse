# telemouse — the machine interfaces

Everything a script, a notebook or an agent can read or drive, in one place.
Four surfaces: the **control panel's HTTP API** (start, stop, mark, watch),
the **viz server's HTTP + WebSocket** (live stream, health, session files),
the **analyzer's CLI** with its JSON output over the **files on disk**, and
the **MCP server**, which is the first three packaged as typed tools for an
agent. Nothing here needs a browser; nothing here has authentication beyond
the loopback bind and the two header rules below, so keep the servers on
loopback unless you mean otherwise.

Defaults: ctl `127.0.0.1:7880`, viz `127.0.0.1:7879`, capture → viz UDP
`127.0.0.1:7878`. All three come from `telemouse.toml` (`[ctl] http_addr`,
`[viz] http_addr`, `[udp] addr`).

## Two rules every HTTP client must follow

1. **`Host` must name this machine**: `127.0.0.1:7880`, `localhost:7880`,
   any IP literal, or `*.localhost`. Anything else is `403` (the
   DNS-rebinding defence in `telemouse_core::localhost`). Real HTTP clients
   send this by default; a proxy that rewrites `Host` does not.
2. **Mutating calls to ctl carry `X-Telemouse-Ctl: 1`.** Every `POST` without
   it is `403`. Browsers cannot add it cross-origin without a preflight the
   server never answers, which is the point.

Viz serves only `/obs`, `/ws` and `/healthz` to non-loopback peers (for an
OBS overlay on another PC); everything else is loopback-only.

## Control panel — `telemouse-ctl` (`crates/ctl/src/server.rs`)

| Route | Body → response |
|---|---|
| `GET /healthz` | `ok` |
| `GET /api/state?log_since=N` | `{ self_pid, now_unix_s, version, config, places, recording: { enabled, dir }, components: [ComponentState], processes: [ProcInfo] }`. `config` is `{ path, found, seeded, status: "seeded"\|"loaded"\|"defaults", mtime_unix_s? }`. `places` is the absolute locations the panel talks about: `panel_url, config, logs, bin_dir, docs, releases, version, webview_data` (`webview_data` is the window's WebView2 cache folder, `%LOCALAPPDATA%\telemouse\WebView2`; empty in a headless or `--no-webview` run). `ComponentState`: `id, label, summary, kind ("service"\|"task"), bin, bin_path, bin_found, flags: [{flag, help}], takes_session, markers, running, pid, since_unix_s, last_exit: { code, ctrl_break, at_unix_s, hint?, last_line? }, exits, unexpected_exits, args, saving, log: [lines], log_seq, stats?, recording?, foreground_seen?`. `log` is the last 120 lines the child printed (stdout + stderr, ANSI stripped); pass the previous `log_seq` as `log_since` to get only newer lines. `stats` (full build) is the child's last `capture stats` line parsed: `session, events_per_s, events, drops, idle_for_s, udp_unreachable, jsonl_dropped, kafka_dropped, game, pointer_locked`. `foreground_seen` (full build, capture only, while running) is `[{ exe, last_unix_s }]`, the last 8 distinct programs the agent saw in the foreground during this run, most recent first, telemouse's own windows excluded: the first-run guide's "is this your game?". `processes` is every telemouse process on the machine. |
| `GET /api/sessions` | `["<id>.jsonl", ...]`, newest first — the report picker's list. |
| `POST /api/components/{id}/start` | `{ flags?: [..], session?: "<id>.jsonl", save?: bool }` → `{ ok, pid }`. Component ids: `capture`, `viz`, `doctor`, `trend`, `report`. `flags` must be from the component's allow-list (`capture`: `--print`, `--no-kafka`, `--no-udp`, `--record`, `--no-record`; `doctor`: `--json`; `report`: `--timing`); anything else is `400`. `save` is the capture card's switch and becomes `--record`/`--no-record`. `report` needs `session`. `404` unknown id, `409` already running, `500` binary missing. |
| `POST /api/components/{id}/stop` | `{ force?: bool }` → `{ ok, outcome: "graceful"\|"terminated" }`. Ctrl-Break, then terminate after `[ctl] stop_grace_secs`; `force` terminates at once. `409` not running. |
| `POST /api/components/{id}/marker` | `{ label }` → `{ ok, label }`. Drops a labelled marker into a running capture, timestamped on arrival (see *Markers*). `400` blank / multi-line / >120 chars / component without a pipe (only `capture` has one), `409` not running, `500` pipe write failed. |
| `POST /api/processes/{pid}/kill` | → `{ ok, killed: ProcInfo }`. `403` for the panel itself or a process the scan would not list, `404` unknown. |
| `GET /api/config` | `{ path, status, exists, token, settings: { mouse_cpi, marker_hotkey, recording: { enabled, dir }, ctl: { hotkey }, games: { "<exe>": { sens, yaw_coeff, pitch_coeff } }, obs: { layout, background, hud, hud_position, scale } } \| null, error, choices: { obs_layouts, obs_hud_items, obs_hud_positions }, not_editable: [..] }`. The editable subset of `telemouse.toml` as it is on disk now. `token` fingerprints the file's bytes (`"none"` when there is no file). `settings` is `null` and `error` says why when the file cannot be used. |
| `POST /api/config` | `{ token, patch }` → `{ ok, token, created, settings, changed: ["mouse_cpi"\|"marker_hotkey"\|"recording"\|"ctl.hotkey"\|"games"\|"viz.obs"], restart_required: ["ctl hotkey"], next_start: ["capture"\|"viz"] }`. `patch` has the shape of `settings` with every field optional; in `games` a value of `null` removes that game, a new game needs `sens` (`yaw_coeff` defaults to 0.022, `pitch_coeff` to the yaw), and the exe name is trimmed, lowercased and given `.exe`. Comments, ordering and other keys in the file are kept; the result is validated exactly as the binaries validate it before anything is written, and the write is atomic. Bind addresses, `bin_dir`, `log_dir`, Kafka and `[batch]` are not in the patch shape and cannot be changed here. `400 { error, field }` invalid value or unknown key (nothing written), `409 { error, token }` the file changed since `token` (re-`GET` and retry), `500` write failed. `recording` takes effect at once; `next_start` names running components that keep their old settings until restarted; `restart_required` needs the panel restarted. |
| `GET /api/reports/{id}` | The stored `ReportSummary` (see *Analyzer*, `report --summary`) for recording `<id>`, if one was made since the recording last changed *and* carries the schema this panel knows (`telemouse-report-summary/2`); `404` otherwise, which is also how a cache written for an older shape is retired — a finished recording never changes again, so mtime alone would serve it forever. `400` when `id` is not a safe recording id. |
| `POST /api/reports/{id}` | Runs the analyzer in `--summary` mode over `<id>.jsonl` and returns the `ReportSummary`; also stored as `<recordings>/.reports/<id>.summary.json` next to the analyzer's cached `<id>.report.json`. `400` bad id, `404` no such recording, `503` no `telemouse-analyze` beside the panel (minimal zip), `500` the run failed. |
| `POST /api/open` | `{ target: "config"\|"recordings"\|"logs"\|"docs" }` → `{ ok, target, path }`. Opens that place with the shell's default handler (the config in an editor, the recordings or logs folder in Explorer, the docs). The target is a name; the path comes from the server, never from the request. `400` unknown target, `404` when this build has no such place (logs in the minimal build), `500` if the shell refused. |

Exit code `-1073741510` (`STATUS_CONTROL_C_EXIT`) in `last_exit` is a clean
Ctrl-Break exit, shown with `ctrl_break: true`; it is not a failure.

```powershell
$h = @{ 'X-Telemouse-Ctl' = '1' }
Invoke-RestMethod -Method Post http://127.0.0.1:7880/api/components/capture/start -Headers $h -ContentType application/json -Body '{"save":true,"flags":["--no-kafka"]}'
Invoke-RestMethod -Method Post http://127.0.0.1:7880/api/components/capture/marker -Headers $h -ContentType application/json -Body '{"label":"trial 1 start"}'
Invoke-RestMethod -Method Post http://127.0.0.1:7880/api/components/capture/stop -Headers $h -ContentType application/json -Body '{"force":false}'
```

## Viz server — `telemouse-viz` (`crates/viz/src/server.rs`)

| Route | Returns |
|---|---|
| `GET /healthz` | `{ ok, udp_bound, uptime_s }` plus, in the full build, `last_datagram_age_s, clients, stalled, feed: "never"\|"live"\|"stalled", udp_addr, http_addr, version`. `503` with `ok: false` while the UDP listener is not bound. |
| `GET /api/stats` (full build) | `{ uptime_s, datagrams, datagrams_per_s, forwarded, parse_errors, lag_drops, lag_disconnects, clients, session_cached, latency: { samples, p50_us, p99_us, max_us, mean_us, negative }, seq_gaps, ... }` — the bridge's own counters. |
| `GET /api/sessions` | `[{ id, path, bytes, modified_epoch_ms, started_utc_us, ended_utc_us, sidecar }]`, newest first. `sidecar` is the parsed `<id>.meta.json` (below) or `null`. Cached for 5 s. |
| `GET /api/session/{id}` | The raw `.jsonl` recording, streamed. `404` for anything that is not a recording directly in the recordings directory. |
| `GET /ws` | WebSocket. Every capture envelope (below) forwarded verbatim as a text frame — including the live-only `heartbeat` — plus a `{"type":"viz_stats", ...}` frame each second. `Origin`, when present, must be local. At most 16 clients (`503` past that). |
| `GET /`, `GET /obs` | The dashboard and the OBS overlay (HTML). `/?session=<id>` opens the dashboard straight into replay of that recording (`<id>` must pass `is_safe_id`); `/?at=<local datetime>` picks the recording running then. `/obs` layers URL parameters over `[viz.obs]`: `layout` (or `view`), `bg`, `hud`, `hudpos`, `scale`, `trail`, `buffer`, `grid`, `legend`, `labels`, and `stale` (0–60 s without a batch before the overlay stops saying nothing; `0` = never; default `stale_secs` = 3. Past it the overlay shows *idle · 12s* while the agent's heartbeats are still arriving — a still hand — and dims to *no feed · 12s* once they stop too). The dashboard remembers its theme in `localStorage.tmTheme` (`light`/`dark`, absent = follow the system); the overlay is never themed and stores nothing. |

### The UDP stream into it (`[udp] addr`, default `127.0.0.1:7878`)

One JSON envelope per datagram, at most 60 000 bytes, forwarded to `/ws`
verbatim. It carries the same `session`, `batch` and `marker` envelopes as a
recording (shapes under *Files on disk*), plus two things a file never holds:

- the `session` envelope is **repeated every 5 s**, so a viz started
  mid-session can convert counts to cm and degrees;
- **`heartbeat`**: `{"type":"heartbeat", session_id, ts_utc_us}`, sent once a
  second and **only while no batch has gone out**. A still mouse produces no
  events and so no batches, which otherwise reads exactly like a capture
  agent that died. It carries no `seq_no` and no `ts_anchor_us` on purpose:
  it is liveness, not telemetry, and must not land in a consumer's sequence
  or latency accounting. It reaches neither the recording nor Kafka.

A consumer that does not know the tag should ignore the datagram rather than
treat it as corruption. A `telemouse-viz` from v0.2.0 or earlier does not know
`heartbeat`: it rejects each one as an unknown envelope type, which is a
`parse_errors` tick and one `warn` a minute, and nothing else — the batches
keep flowing and the overlay behaves exactly as it did before.

## Capture agent — `telemouse` (`crates/capture`)

```
telemouse run    [--config FILE] [--log-dir DIR] [--print] [--no-kafka] [--no-udp]
                 [--record | --no-record] [--duration-secs N]
telemouse doctor [--config FILE] [--json]
```

Exits `0` on Ctrl-C / Ctrl-Break / `--duration-secs`; non-zero with the
reason on stderr when the config does not parse or raw input cannot be
registered. `doctor` checks the environment (build, OS, config, QPC,
screens, cursor, foreground, devices, UDP bind, recording folder, Kafka
reachability) and prints the resolved config.

### `doctor --json`

One `telemouse-doctor/1` document on stdout and nothing else (the log goes
to stderr, as always), rendered from the same checks as the text rows:

```json
{ "schema": "telemouse-doctor/1", "capture_version": "0.2.0",
  "generated_utc_us": 1790000000000000, "verdict": "warn",
  "checks": [ { "id": "udp", "status": "pass", "title": "UDP sink",
                "detail": "ready -> 127.0.0.1:7878" },
              { "id": "kafka_broker_1", "status": "warn",
                "title": "Kafka broker 1",
                "detail": "192.168.137.67:9092 unreachable",
                "hint": "batches queue and are dropped while the broker is away; ..." } ],
  "config": { ...the resolved config, every relative path absolute... } }
```

- `verdict` is the worst `status` in `checks`: `pass` | `warn` | `fail`.
- `status` is `pass` (nothing to do), `warn` (works, something will be
  missing) or `fail` (this part will not work).
- `hint` is present only when there is something to do about the row.
- `id` is stable across versions and independent of the wording — match on
  it, not on `title` or `detail`. Today's rows, in order: `build`, `os`,
  `config`, `config_overrides`, `clock`, `screen`, `cursor`, `foreground`,
  `devices`, `udp`, `recording`, `kafka`, then `kafka_broker_<n>` per
  configured broker.
- **Exit codes are the same as in text mode**, i.e. doctor exits `0`
  whenever it produced a report — `fail` rows included — and non-zero only
  when it could not get that far (a `telemouse.toml` that exists but does
  not parse, with the reason on stderr). Decide on `verdict`, not on the
  exit code.

### Markers

A marker is a labelled timestamp in the recording (`{"type":"marker"}`
below). Sources:

- **The marker hotkey** (`marker_hotkey` in `telemouse.toml`, F9 by
  default, system-wide; `""` for none) — label `hotkey`.
- **A line on stdin, when stdin is a pipe.** `round 3 start\n` or
  `{"label":"round 3 start"}\n`; blank lines and lines without a usable
  label are ignored; labels are cut to 120 characters. The timestamp is
  taken when the line arrives. A console stdin is never read. The control
  panel starts capture with such a pipe and its `/marker` route writes to
  it. From a shell: `"trial 1" | telemouse run --duration-secs 30`.
- **The agent itself** — a config reload writes a `config_changed` marker
  and a clock-drift check that finds drift writes `anchor_drift_us=<n>`.

`telemouse-analyze report --split-by-marker` gives a sub-report per interval
between markers; every report JSON lists them under `markers[]` as
`{ t_s, t_utc_us, label }`.

## Analyzer — `telemouse-analyze` (`crates/analyze/src/main.rs`)

```
telemouse-analyze list   [--dir recordings] [--json]
telemouse-analyze report <path.jsonl | session-id> [--dir recordings] [--summary]
                         [--json FILE] [--json-dir DIR] [--csv-dir DIR] [--timing] [--quiet]
                         [--flick-threshold N] [--still-threshold N] [--locked-only] [--split-by-marker] [...]
telemouse-analyze trend  [--dir recordings] [--json-dir DIR] [--metric a.b.c]... [--csv FILE] [--json]
```

- **`list --json`** → `[{ path, session_id, started_utc_us, duration_s,
  events, drops, games: [..], bad_lines, losses: [[sink, count], ..],
  exit }]`. Header-only scan; never loads events. `drops` are ring drops
  inside capture; `losses` are per-sink undelivered envelopes from the
  sidecar; `exit` is the sidecar's exit reason or `null`.
- **`report <id|path>`**: a bare id is `<dir>/<id>.jsonl`. Pass `--json-dir`
  so the cached `<id>.report.json` is reused (analyzer version, params and
  the recording's signature must match).
- **`report --summary`** → `telemouse-report-summary/2`, ~3 KB (~5 KB for a
  marked session, ~8 KB with both marker tables full):
  `{ schema, analyzer_version, session: { session_id, started_utc,
  duration_s, event_count, marker_count, mouse_cpi, game, sens,
  deg_per_count, cm_per_360, aim_profile_missing, ... }, quality: { events,
  ring_drops, lost_batches, seq_gaps, monotonicity_violations, bad_lines,
  pct_within_1ms, median_interval_ms, p99_interval_ms, gaps_over_10ms,
  poll_hz, locked_fraction, clean, threads_clean, exit, unfinished,
  capture_profile, sink_losses }, flicks: { count, per_minute,
  amplitude_deg_median, peak_velocity_deg_s_median, duration_ms_median,
  overshoot_ratio_median, overshoot_ratio_p90, settle_ms_median,
  settle_ms_p90, time_to_click_ms_median, clicked_fraction }, micro: {
  total_corrections, clean_segment_fraction, tremor_rms_counts_s,
  tremor_rms_cm_s, band_ratio_8_12, dominant_hz }, clicks: { total,
  per_minute, still_click_fraction, click_to_still_ms_median,
  hold_ms_median, double_clicks }, kinematics: { total_distance_m,
  distance_cm_per_min, moving_fraction, path_efficiency_weighted,
  speed_cm_per_s_median, speed_cm_per_s_p99, speed_deg_per_s_p99 }, lifts:
  { count, per_minute, mean_drift_cm }, markers: [{ t_s, label }, ..],
  markers_total, segments: [{ index, label, next_label, t_start_s, t_end_s,
  flicks, flicks_per_min, overshoot_median, settle_median_ms,
  tremor_rms_counts_s, path_efficiency, clicks_per_min }, ..],
  segments_total, warnings: [..] }`. `clean` means no
  data-quality warning fired; `threads_clean` is the sidecar's own flag.
  If `aim_profile_missing` is true every degree-valued number uses a
  fallback sensitivity and is not comparable across sessions.
  `markers`/`segments` carry at most the first 12 of each (`markers_total`
  and `segments_total` are the real counts) and a label longer than 120
  characters is clipped with `…`, so the document stays a few KB. A
  `segments[i]` runs from the marker named by `label` to the one named by
  `next_label` — both empty at the ends of the session — which is what tells
  a `"sens A"` stretch from a `"sens B"` one. An unmarked recording has
  `markers: []`, `segments: []` and both totals `0`: its single interval
  would only repeat the headline numbers. `/2` added these four fields;
  `/1` had none of them.
- **`report --json FILE`** → the full report: `schema, analyzer_version,
  params, session, quality, kinematics, flicks, micro, clicks, lifts,
  markers[], segments[], per_second[], per_minute[], warnings[], timings[]`.
  Distribution fields are `{ n, mean, median, p90, p99, max, stddev }`. Tens
  of MB for a long session because of `per_second`.
- **`trend --json`** → `[{ session_id, path, started_utc_us, started_utc,
  duration_s, events, drops, game, flicks, flicks_per_min, overshoot_median,
  settle_median_ms, tremor_rms_counts_s, path_efficiency, clicks_per_min,
  distance_m, lifts, cm_per_360, from_cache, extra: [[path, value], ..] }]`,
  oldest first. `--metric micro.band_ratio_8_12` adds any dotted report path
  as an `extra` column.

Exit code is non-zero with the reason on stderr when the recording is
missing or its first line is not a session envelope. Warnings go to stderr;
JSON goes to stdout, so `2>$null` leaves clean JSON.

## MCP server — `telemouse-mcp` (`crates/mcp`)

Everything above as Model Context Protocol tools, so an agent does not have
to know the routes, the CLI flags or the id rules. One process, **stdio
transport only**: the client starts it, speaks JSON-RPC over its stdin and
stdout, and stops it. There is no HTTP transport and no token yet
([AGENTIC-2026-09-13.md](AGENTIC-2026-09-13.md), *Remote*, is the design for
adding one).

```
telemouse-mcp [--config FILE] [--read-only] [--recordings DIR]
              [--ctl ADDR] [--viz ADDR] [--log-dir DIR]
```

It finds `telemouse.toml` the way every other binary does (working directory
first, then next to the exe) and takes the recordings directory from
`[recording] dir` and the two addresses from `[ctl] http_addr` and
`[viz] http_addr`; a wildcard bind is dialled on the loopback of the same
family. **stdout carries the protocol and nothing else** — logging goes to
stderr and, in a full build, to `<log_dir>/mcp.log`.

Registered for Claude Code in the repo's `.mcp.json`:

```json
{ "mcpServers": { "telemouse": { "type": "stdio", "command": "target/release/telemouse-mcp.exe" } } }
```

From an unzipped release the command is `telemouse-mcp.exe` beside
`telemouse.toml`; `claude mcp add telemouse -- <that path>` does the same
thing without editing a file.

### Tools

Read-only, answered from the analyzer library and `GET`s:

| Tool | Input | Result |
|---|---|---|
| `sessions_list` | `limit?` (1-500, default 50) | `{ dir, total, returned, sessions: [...] }`. Each entry is a `list --json` row: `path, session_id, started_utc_us, duration_s, events, drops, games, bad_lines, losses, exit`. Newest first; `total` is how many exist, so a truncated answer says so. |
| `session_summary` | `id` (a recording id, no `.jsonl`) | The `telemouse-report-summary/2` object (*Analyzer*, `report --summary`), ~3 KB. Uses and fills the same `recordings/.reports/<id>.report.json` cache the panel does. |
| `trend` | `metric?: [dotted paths]`, `last?` (1-500) | `{ rows, sessions: [TrendRow] }`, oldest first — the `trend --json` rows, `last` keeping the newest. |
| `health` | — | `{ ctl: {...}, viz: {...} }`: a projection of ctl `GET /api/state` (per component: running, pid, since, exit counts, `last_exit`, the capture agent's `stats`; plus the process table as `pid`/`name`) and of viz `/healthz` + `/api/stats`. The 120-line log rings and the bulk fields are left out. |
| `logs_tail` | `component` (`ctl`, `capture`, `viz`, `doctor`, `trend`, `report`), `lines?` (1-500, default 50) | `{ component, source, lines }`. The panel's live ring for a component it launched, otherwise `<log_dir>/<component>.log`; ANSI stripped. |
| `live_stats` | `seconds?` (0-60, default 5) | Two `/api/stats` readings that far apart, as rates: `{ sampled_s, datagrams_per_s, forwarded_per_s, parse_errors_per_s, lag_drops_per_s, *_delta, latest }`. `seconds: 0` is a single reading. |

Control, every one of them a `POST` to ctl with `X-Telemouse-Ctl: 1` — this
server never spawns, signals or terminates anything itself, so ctl's flag
allow-list and its pid rule are still what decide:

| Tool | Input | Result |
|---|---|---|
| `capture_start` | `save?`, `flags?`, `session?` | ctl's `{ ok, pid }`. `flags` is checked against `--print`, `--no-kafka`, `--no-udp`, `--record`, `--no-record` here as well as there. `session` is carried through for symmetry with the route; the capture agent names its own session, so it changes nothing. |
| `capture_stop` | `force?` | `{ ok, outcome: "graceful" \| "terminated" }`. |
| `marker` | `label` | `{ ok, label }`. One line, trimmed, at most 120 characters. |
| `doctor` | — | Starts the doctor component and polls until it exits, up to 30 s: `{ finished, last_exit?, note?, lines }`. |
| `kill` | `pid` | `{ ok, killed: ProcInfo }`. ctl refuses itself and any pid its own scan would not list. |

`--read-only` removes those five from the router, so `tools/list` does not
mention them and calling one is an unknown tool.

### Errors

A tool that cannot do its job answers with an **error result** (`isError`
with a sentence), never a protocol error and never by exiting: a panel that
is not running, a viz built without `observability`, an id that names no
recording and a flag ctl would reject are all ordinary results, and the
session stays up. Argument problems (an unsafe id, a blank label, a flag
outside the allow-list, a pid that is not one) are refused before any request
is made, and the message names the allowed values. A refusal from ctl is
relayed with its status and its own `error` text, so a `409` still reads
*capture is already running*.

## Files on disk

All relative to the directory of `telemouse.toml` (`[recording] dir`,
default `recordings`).

- **`<id>.jsonl`** — one envelope per line, first line always
  `{"type":"session", ...}` (device list, monitors, `mouse_cpi`, `games`
  sens table, QPC anchor), then `batch` and `marker` lines in order. Grows
  ~150 MB per hour of play at 1 kHz; read it with the analyzer or a
  streaming parser, never whole.
  - `batch`: `{ session_id, seq_no, ts_anchor_us, game?, pointer_locked,
    screen_w, screen_h, cursor_x?, cursor_y?, drops_since_last,
    abs_frames_since_last, events: [{ ts_qpc, dx, dy, buttons?, wheel?,
    wheel_h?, device? }] }` — raw counts, zero fields omitted; `ts_qpc` is
    in `qpc_freq` ticks, mapped to UTC through the session anchor.
  - `marker`: `{ session_id, seq_no, ts_qpc, ts_utc_us, label }`.
  - There is no `heartbeat` line: that envelope exists on the UDP path only
    (above). A reader that meets one anyway skips it rather than counting a
    bad line.
- **`<id>.meta.json`** (full build) — rewritten every 5 s while running with
  `"exit":"running"`, final on a clean stop: `{ session_id,
  capture_version, capture_profile: "release"|"debug", started_utc_us,
  ended_utc_us, exit, clean, events, batches, markers, ring_drops,
  ring_high_water, abs_frames, qpc_freq, anchor_uncertainty_us,
  max_anchor_drift_us, window_ms, coalesce_ms, poll_hz, sinks: { udp|jsonl|
  kafka: { errors, dropped, abandoned } } }`. `exit` vocabulary: `running`,
  `interrupt`, `duration`, `capture-thread-exited`, `shipping-thread-exited`,
  `context-thread-exited`, `capture-thread-stalled`,
  `shipping-thread-stalled`, `context-thread-stalled`, `error`. A sidecar
  still saying `running` after the process is gone means it died without
  a clean stop; its counters are from the last 5-second rewrite.
- **`.telemouse-analyze-index-v1.json`** — the analyzer's `list` cache; safe
  to delete.
- **`<id>.report.json`** wherever `--json-dir` points — the full report,
  reused when current.
- **`recordings/.reports/`** — the control panel's `--json-dir`: the cached
  `<id>.report.json` of every report or trend run from the panel, plus
  `<id>.summary.json` (the `ReportSummary` behind the report card). Safe to
  delete; it is rebuilt on the next report.
- **`logs/ctl.log`, `logs/capture.log`, `logs/viz.log`** (full build, `[ctl]
  log_dir`) — `tracing` lines, size-rotated; a `capture stats` line every
  5 s and a `warn` once a minute for anything that stays wrong.

## Kafka (full build, `[kafka] enabled`)

Topics `mouse.events` (batches), `mouse.sessions` (session envelopes),
`mouse.markers`; key = session id; value = the same JSON envelope as the
JSONL line, zstd-compressed by the producer. Idle heartbeats are not among
them — `mouse.heartbeats` is a reserved name that nothing publishes to — so
a still hand costs the broker nothing. Created on connect with the
broker's defaults. The sink never blocks capture: a stalled broker fills a
bounded queue and the overflow is counted as `kafka_dropped` (stats line)
and `sinks.kafka.dropped` (sidecar). `tools/kafka2parquet/` archives the
topics to Hive-partitioned Parquet.
