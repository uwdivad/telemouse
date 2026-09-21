# telemouse — notes for Claude Code

Windows mouse-telemetry toolkit: a raw-input capture agent, a live viz/OBS
overlay, an offline analyzer and a control panel. Read
`docs/CONVENTIONS.md` before editing any crate; `docs/GUIDE.md` explains the
internals module by module; `docs/API.md` is the reference for every
machine interface (HTTP routes, JSON shapes, files on disk). `README.md`
is deliberately one screen for newcomers; user-facing detail goes in
`docs/HELP.md`, not back into the README. The
`/telemouse` skill (`.claude/skills/telemouse`) covers how to *use* the
tools on recorded sessions.

## Layout

| Crate / dir | Binary | Role |
|---|---|---|
| `crates/core` | lib | Wire format, config, clock math, recordings + sidecars, logging. **Public API is the contract.** |
| `crates/capture` | `telemouse` | Raw input → 25 ms batches → UDP + JSONL recording (+ Kafka). `doctor` subcommand. |
| `crates/viz` | `telemouse-viz` | UDP→WebSocket bridge, dashboard + `/obs` overlay, replay over recordings. |
| `crates/analyze` | `telemouse-analyze` | `report` / `trend` / `list` over `recordings/*.jsonl`. |
| `crates/ctl` | `telemouse-ctl` | Control panel: HTTP JSON API + native window (WebView2) + tray. Starts/stops the others with allow-listed flags. |
| `crates/mcp` | `telemouse-mcp` | MCP server (stdio, `rmcp`): the analyzer library + ctl/viz proxies as typed tools. Registered in `.mcp.json`. |
| `tools/cpubench` | `tmbench` | Standalone CPU harness (not a workspace member). |
| `tools/kafka2parquet` | notebooks | Kafka → Parquet archiver, PySpark walkthrough. |

## Commands (what CI runs)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p telemouse-capture -p telemouse-viz -p telemouse-ctl -p telemouse-mcp --all-targets --locked --no-default-features --features telemouse-capture/quiet,telemouse-viz/quiet,telemouse-ctl/quiet,telemouse-mcp/quiet -- -D warnings
cargo test --workspace --locked
cargo test -p telemouse-core -p telemouse-capture -p telemouse-viz -p telemouse-ctl -p telemouse-mcp --locked --no-default-features
node --check crates/viz/src/app.js
node --test "crates/viz/js-tests/*.test.mjs"
```

Tests need no mouse, admin rights, Kafka or browser. Run the full list before
saying a change is done; a formatting or clippy miss fails CI.

## Rules that are easy to get wrong

- **Two build flavours.** `logging`, `observability`, `kafka` are Cargo
  features, on by default, off in a minimal build (CI-gated, no longer
  published: the release is one zip with the default features); `quiet` compiles
  `tracing` out. Anything that logs to a file, emits stats, writes a sidecar
  or talks to Kafka goes behind the matching feature. Every crate must build
  and test with `--no-default-features` too.
- **Embedded pages need a rebuild.** `crates/viz` (`index.html`, `app.js`) and
  the ctl page are compiled into the binaries with `include_str!`. Editing
  them does nothing until the crate is rebuilt. `app.js` has Node tests.
- **ctl stays a console app.** The tray/status window lives in
  `crates/ctl/src/gui` but the exe keeps the console subsystem; Ctrl-Break
  delivery to children depends on it. `--no-gui` exists for headless runs.
- **The ctl window hosts the panel page via WebView2**
  (`crates/ctl/src/gui/webview.rs`). The read-only `EDIT` text view
  (`model::window_text`) is the fallback when the runtime is missing, fails
  or is disabled with `--no-webview`; it must keep working. Never
  feature-gate WebView2: a minimal build needs the window too. Its user-data
  folder is `%LOCALAPPDATA%\telemouse\WebView2` (`places.webview_data`), the
  one thing telemouse writes outside its own folder.
- **New config keys need every binary rebuilt.** ctl spawns whatever is in
  `bin_dir` (default `target/release`); an older child refuses a
  `telemouse.toml` with keys it does not know. After adding a key, rebuild the
  workspace and update `telemouse.example.toml` (it ships in the zip and is
  what ctl seeds a first-run `telemouse.toml` from).
- **The panel writes `telemouse.toml`** (`crates/ctl/src/settings.rs`,
  `GET`/`POST /api/config`) with `toml_edit`, validated through
  `AppConfig::from_toml` before an atomic write. The editable set is the
  `Patch` struct's allow-list; bind addresses, Kafka, `bin_dir` and
  `log_dir` stay hand-edit only (LAN exposure). A new user-facing key
  belongs in `Patch`, the Settings card and `docs/API.md`.
- **`telemouse.toml` is this machine's live config** (LAN bind, Kafka on).
  Do not copy its values into `telemouse.example.toml`; the example keeps
  loopback defaults.
- **Recording ids** go through `telemouse_core::recordings::is_safe_id`
  everywhere a name is listed or accepted.
- **Logging** is only ever set up via `telemouse_core::logging::init`.
  Every binary installs `telemouse_core::panic_hook`.
- **Degraded environments never panic**: missing broker, unbindable port,
  missing sidecar are warnings and the rest keeps running.
- **Local-server trust rule** (`telemouse_core::localhost`): viz and ctl
  refuse requests whose `Host` is not `localhost`/an IP literal; ctl POSTs
  need `X-Telemouse-Ctl: 1`; viz serves only `/obs`, `/ws`, `/healthz` to
  non-loopback peers. Keep it that way when adding routes.
- **The MCP server only proxies.** `crates/mcp` never spawns, signals or
  terminates anything itself: `capture_start`, `capture_stop`, `marker`,
  `doctor` and `kill` are `POST`s to ctl with its guard header, so ctl's flag
  allow-list and pid rule stay the safety model. `--read-only` removes those
  five tools from the router (`#[tool_handler(router = self.tool_router)]` is
  what makes that stick — the macro's default rebuilds a full router).
  **stdout is the MCP channel**: nothing in that binary may print to it, and
  logging goes to stderr plus `logs/mcp.log`. A second transport is a
  main.rs change only (`docs/AGENTIC-2026-09-13.md`, *Remote*).
- **Anticheat posture** (`docs/ANTICHEAT-2026-09-14.md`, `docs/FAIR-PLAY.md`):
  the shipped binaries only read. Never open a handle on a process that is
  not a telemouse process (the foreground game's name comes from a Toolhelp
  snapshot), never hook, never synthesize input, never draw over the game,
  never advise running as administrator. `tmbench inject` is the one
  `SendInput` in the tree; it stays out of the release zips and behind
  `TMBENCH_ALLOW_INJECT=1`. Do not add lists of game executable names
  anywhere. Every executable carries a version resource and the manifest
  (`telemouse.manifest`, `asInvoker`) via its `build.rs`.
- **Defaults**: batch window 25 ms, coalesce 8 ms, UDP 127.0.0.1:7878,
  viz 7879, ctl 7880, marker hotkey F9.

## Releasing

Bump `version` in the root `Cargo.toml`, add a `## [X.Y.Z]` section to
`CHANGELOG.md`, commit, tag `vX.Y.Z`, push. `release.yml` refuses a tag that
does not match the Cargo version and uses the changelog section as the notes.
It publishes one zip (five default-feature binaries) and signs the exes with
Azure Artifact Signing when the secrets exist (`docs/DEVELOPING.md`, "Code
signing"); without them the release is unsigned. A new binary has to be added
in three places in `release.yml` (the `Copy-Item` in *Package*, the signing
file list, and the verification loop) or it ships unsigned or not at all. The
exe icon is
`assets/telemouse.ico` (regenerate with `assets/make-icon.ps1`).

## Where things land at runtime

`recordings/<id>.jsonl` + `<id>.meta.json` sidecar (rewritten every 5 s with
`"exit":"running"` until a clean stop), `logs/{ctl,capture,viz,mcp}.log` (full
build), `.telemouse-analyze-index-v1.json` cache in the recordings dir,
`<id>.report.json` wherever `--json-dir` points (the panel uses
`recordings/.reports/`, which also holds its `<id>.summary.json`), and the
WebView2 cache in `%LOCALAPPDATA%\telemouse\WebView2`.

## History worth knowing

`docs/AUDIT-2026-08.md` (performance/observability audit),
`docs/PERFORMANCE-2026-09.md` (measured before/after), `docs/BENCHMARKS.md`
(criterion + cpubench numbers), `CHANGELOG.md` (the 2026-09-12 field audit
fixes are under v0.1.2), `docs/AGENTIC-2026-09-13.md` (the agentic-workflow
menu, build order and what has landed; steps 1-3 are done, the post-session
triage note and the log-health loop are next),
`docs/ANTICHEAT-2026-09-14.md` (RICOCHET exposure audit, the public record
in `-sources.md`, what landed from it).
