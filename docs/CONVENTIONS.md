# telemouse — workspace conventions

Read this before touching any crate. The plan is `mouse-telemetry-plan.md` at
the repo root.

## Layout

| Crate | Path | Owns |
|---|---|---|
| `telemouse-core` | `crates/core` | Shared types, wire format, config, pure logic. **The contract — do not change its public API without coordinating.** |
| `telemouse-capture` (bin `telemouse`) | `crates/capture` | Win32 raw-input capture agent, sinks (UDP, Kafka, JSONL recording). |
| `telemouse-viz` | `crates/viz` | UDP→WebSocket bridge, embedded browser viz (live + replay). |
| `telemouse-analyze` | `crates/analyze` | Offline metrics over recorded sessions. |
| `telemouse-ctl` | `crates/ctl` | Control panel: launches/stops the binaries above (fixed argument allow-lists), lists and kills telemouse processes. |
| `telemouse-mcp` | `crates/mcp` | MCP server (stdio): the analyze library and ctl/viz HTTP as typed tools for an agent. Proxies ctl; never launches or kills anything itself. |

Each workstream edits **only its own crate**. Root `Cargo.toml` already lists
all members and shared `[workspace.dependencies]`; add crate-local deps with
`cargo add -p <your-crate> <dep>` (uses the network to pick current versions).

## Wire format (from `telemouse-core::wire`)

- One JSON `Envelope` (`{"type": "session" | "batch" | "marker", ...}`) per
  Kafka message, per UDP datagram, per JSONL line.
- Topics: `mouse.events`, `mouse.sessions`, `mouse.markers`. Kafka key =
  `session_id`. `mouse.sessions` is *meant* to be compacted, but the client
  creates topics with the broker's defaults (rskafka's `create_topic` takes
  no configs), so set `cleanup.policy=compact` on it at the broker if you
  need session records to outlive the events' retention.
- Recording ids (`<session_id>`) obey `telemouse_core::recordings::is_safe_id`
  everywhere a name is listed or accepted; the capture agent also writes
  `<session_id>.meta.json` next to each recording with the run's final
  counters (events, drops, per-sink losses).
- Recording files: `recordings/<session_id>.jsonl`, first line always the
  `session` envelope, then `batch`/`marker` envelopes in order.
- Raw counts on the wire, always. Consumers derive cm/degrees via
  `telemouse-core::units` + the session's `mouse_cpi` / `games` table.

## Conventions

- Edition 2024, latest stable Rust. Target OS: Windows (win32) — but keep
  everything that *can* be platform-neutral platform-neutral, behind
  `#[cfg(windows)]` only where Win32 is genuinely required, so `cargo test`
  logic tests run anywhere.
- Errors: `anyhow` in binaries, `thiserror` for library-ish error types.
- Logging/observability: `tracing` everywhere (`tracing-subscriber` with
  `EnvFilter`, default `info`, env var `RUST_LOG`). Every long-running loop
  emits periodic stats at `info` (rates, drops, queue depths, errors) —
  metrics are logs here, no metrics server. Anything that stays wrong is
  repeated as a `warn` once a minute, never only as a number in the stats
  line.
- Build flavours: `logging`, `observability` and (capture only) `kafka` are
  Cargo features, on by default, off in a minimal build; `quiet`
  compiles `tracing` calls out. New code that logs to a file, reports
  stats, or talks to Kafka goes behind the matching feature; the pipeline
  itself (capture → UDP → viz, JSONL recording, the pages) never does. Every
  crate must build and pass tests with `--no-default-features` as well as
  with defaults; CI runs both, plus the `quiet` variant with clippy. The
  subscriber is only ever set up through `telemouse_core::logging::init`
  (terminal detection, `NO_COLOR`, rotated files), never directly.
- Paths: every binary finds `telemouse.toml` with
  `telemouse_core::paths::locate_config` (CWD first, then next to the exe)
  and resolves relative paths in it against the file's directory with
  `AppConfig::resolve_paths`; a config that exists but does not parse is
  refused, never silently replaced by defaults.
- Config: `telemouse.toml` at repo root via `telemouse_core::config::AppConfig`.
  CLI (clap, derive) may override specific fields; don't invent parallel config.
- Tests: every crate has unit tests for its pure logic (`cargo test -p <crate>`
  must pass on Windows without admin rights, a mouse, Kafka, or a browser).
  Win32/network side effects live behind thin traits or in `#[cfg(windows)]`
  modules exercised manually.
- No panics on degraded environments: missing Kafka broker, unbindable UDP
  port, etc. log a warning and keep the rest of the pipeline alive.
