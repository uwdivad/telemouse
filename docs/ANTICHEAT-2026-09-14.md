# Anticheat exposure audit (2026-09-14)

**Question.** Does anything in telemouse do what Call of Duty's RICOCHET
anti-cheat, or Activision's Security & Enforcement Policy, acts on? Could
running it lead to a shadowban (limited matchmaking)?

**Answer.** No, on the evidence of the code, the built binaries and the
public record. The shipped binaries only *read*: they ask Windows for a copy
of raw mouse input, read the foreground window's process name, and talk to
loopback and to your own LAN. They synthesize no input, install no hooks, no
driver and no in-game overlay, read and write no game memory, and modify no
game files or network traffic. Those are the things anti-cheat systems and
the policy act on, and telemouse does none of them. A shadowban is a
server-side decision driven by reports and statistics, which a passive
recorder cannot influence.

The public record agrees. Every documented enforcement action, from
Cronus/XIM through reWASD to the QuadStick false positive, involved
something that modifies, injects, emulates or hides input or touches the
game process; no case was found, at any source tier, of RICOCHET acting
against a tool that only reads input (section 3). The honest ceiling for any
third-party tool is still "unnamed and unenforced", not "safe": Activision
publishes no allowlist and reserves the right to monitor "applications
running on a machine during gameplay".

Two things deserve attention anyway, because they are the only places where
telemouse *touches* the game or *could* emit input:

1. **The capture agent opens one short-lived handle to the game process**,
   with the weakest possible right, each time the foreground window changes
   (F1). Harmless by every public account, and the same call Task Manager and
   Discord make, but it is the single footprint a kernel driver could log,
   and it can be removed entirely with about thirty lines.
2. **The benchmark harness `tmbench inject` synthesizes mouse motion and
   clicks with `SendInput`** (F2). It is not shipped and not a workspace
   member, but it is built on this machine and the agentic plan proposes
   handing it to an agent. Run while a game is open, it would be real input
   automation. It should refuse to run without an explicit opt-in.

Everything else is either normal for any desktop application or a matter of
presentation: unsigned binaries with no version resource, a hint that tells
users to run the panel as administrator, and docs that promise a little more
than anyone can ("anticheat-safe").

## Method

- Read in full every module that touches Win32: `crates/capture/src/{raw_input,platform,context_thread,context,pointer_lock,devices,main,stdin_markers}.rs`,
  `crates/ctl/src/{procs,manager,main}.rs`, `crates/ctl/src/gui/win.rs`,
  `crates/core/src/{shutdown,hotkey,event,batch,config}.rs`,
  `tools/cpubench/src/main.rs` and `tools/cpubench/bench.ps1`.
- Searched the whole tree (Rust, PowerShell, JS, HTML, TOML, YAML) for input
  synthesis, hooks, process and memory access, screen capture, overlay window
  styles, registry and service persistence, device access and dynamic
  library loading.
- Scanned the four release executables built on 2026-09-13 and `tmbench.exe`
  for imported API names (Appendix A), and checked their signature and
  version resource.
- Checked `release.yml` for what the zips contain and `Cargo.lock` for
  input-synthesis crates (none: no `enigo`, `rdev`, `inputbot`, `winput`,
  `interception`).
- Compared the result with the public record on RICOCHET and the policy:
  a research pass of about a hundred fetches on 2026-09-14, every claim
  tagged official, journalism, vendor or community. Section 3 summarises
  it; the unedited notes with all 90 sources are in
  [ANTICHEAT-2026-09-14-sources.md](ANTICHEAT-2026-09-14-sources.md).

## 1. What the game can observe: the footprint

Everything below is the complete list of Win32 behaviour in the shipped
binaries. Nothing else in the tree touches the OS beyond files and sockets.

| Behaviour | Where | Observable from the game side | Assessment |
|---|---|---|---|
| Registers for raw mouse input with `RIDEV_INPUTSINK` on a message-only window (`HWND_MESSAGE`), mouse usage only, no keyboard | `crates/capture/src/raw_input.rs:759-809` | Windows delivers a *copy* of each HID report; the game's own input is untouched. Registration is not visible through any public API. | The mechanism mouse testers and input viewers use. Quieter than NohBoard and the OBS input-overlay plugin, which install global low-level hooks instead. Not a signal. |
| Drains reports with `GetRawInputBuffer` / `GetRawInputData` | `raw_input.rs:484-582` | Nothing | Read-only. |
| Enumerates pointing devices, `GetRawInputDeviceList` / `GetRawInputDeviceInfoW` (device name only) | `crates/capture/src/devices.rs:121-167` | Nothing | Read-only; no `hid.dll`, no `CreateFile` on device paths. |
| `GetForegroundWindow` + `GetWindowThreadProcessId` every 250 ms | `crates/capture/src/platform.rs:147-157`, `context_thread.rs:32,208-229` | Nothing (no handle) | Read-only. |
| **`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` on the foreground PID, once per PID change; also once in `doctor`** | `platform.rs:163-181`, cache at `platform.rs:411-446`, `main.rs:835` | A handle open on the game process with the minimum right, closed within microseconds | **F1.** The one touch of the game process. See finding. |
| `GetCursorPos` every 250 ms (pointer-lock heuristic) | `platform.rs:140-144`, `pointer_lock.rs` | Nothing | Read-only. |
| `RegisterHotKey(F9)` (hard-coded) in capture; `[ctl] hotkey` (default Ctrl+Alt+R) in the panel | `raw_input.rs:364-371,811`; `crates/ctl/src/gui/win.rs:456-479` | The chord is consumed system-wide: pressing F9 in-game reaches telemouse, not the game | Normal (OBS, Discord). Not a signal. UX note in F6. |
| `SetThreadPriority(ABOVE_NORMAL)` on T1; `SetProcessInformation(ProcessPowerThrottling)` | `platform.rs:189-218` | Nothing | Own process, normal priority class. Not a signal. |
| `GetProcAddress(ntdll, "RtlGetVersion")` | `platform.rs:96-116` | Nothing | Documented way to read the real build number. Cosmetic note in F9. |
| `SetProcessDpiAwarenessContext`, `EnumDisplayMonitors`, `EnumDisplaySettingsW`, `GetSystemMetrics` | `platform.rs:60-66,221-290` | Nothing | Read-only geometry. |
| `SetConsoleCtrlHandler`; `GenerateConsoleCtrlEvent(CTRL_BREAK)` to the panel's own process group | `crates/core/src/shutdown.rs:195-201`; `crates/ctl/src/manager.rs:1269,1435-1445` | Nothing | Own children only. |
| Process table: one `CreateToolhelp32Snapshot` every 30 s, then `OpenProcess(QUERY_LIMITED)`, `GetProcessTimes`, `GetProcessMemoryInfo`, `NtQueryInformationProcess(ProcessCommandLineInformation)` **only for names starting with `telemouse` or `cargo`**; `TerminateProcess` only after `classify` says the target is a telemouse process | `crates/ctl/src/procs.rs:73-98,220-282,287-320,377-545` | The snapshot opens no per-process handles. The game is never a candidate. | Never touches the game. Native-API import noted in F4. |
| Tray icon, one `WS_OVERLAPPEDWINDOW` status window, `ShowWindow(SW_HIDE)` on its own console, `MessageBoxW(MB_TOPMOST)` for errors, `ShellExecuteW("open")` | `gui/win.rs:168-191,216-229,727-738` | An ordinary window. No `WS_EX_TOPMOST`, `WS_EX_LAYERED`, `WS_EX_TRANSPARENT` (the overlay/ESP pattern) anywhere. | Not a signal. |
| Sockets: UDP 127.0.0.1:7878, HTTP 7879 and 7880 (7879 bound to the LAN on this machine for OBS), Kafka to a LAN broker in the full build | config, sinks | Ordinary outbound traffic | No VPN, proxy or traffic shaping. Not a signal. |
| Persistence | none | none | No registry writes, services, scheduled tasks or startup entries (grep-verified). |

Absent everywhere in the shipped binaries: `SendInput`, `mouse_event`,
`keybd_event`, `SetCursorPos`, `ClipCursor`, `BlockInput`,
`SetWindowsHookEx`, `SetWinEventHook`, `ReadProcessMemory`,
`WriteProcessMemory`, `VirtualAllocEx`, `CreateRemoteThread`,
`GetAsyncKeyState`, `BitBlt`/`GetDC`/DXGI capture, `FindWindow`,
`EnumWindows`, `SetWindowDisplayAffinity`, `SetPriorityClass`, `LoadLibrary`
of anything but the CRT's own, and every kernel-driver or DLL-injection
primitive.

## 2. Findings

Severity is anticheat exposure, not code quality. *Medium* means "the one
thing worth changing before wider distribution"; nothing here is high.

### F1. Medium-low: one handle into the game process per foreground change

`crates/capture/src/platform.rs:163-181` opens the foreground process with
`PROCESS_QUERY_LIMITED_INFORMATION`, reads its image path with
`QueryFullProcessImageNameW`, and closes the handle. `ForegroundCache`
(`platform.rs:411-446`) makes sure this happens only when the foreground PID
changes, so during play the agent opens nothing. `doctor` does it once
(`crates/capture/src/main.rs:835`).

Why it is low: this is the least-privileged process right there is. Task
Manager, Explorer, Discord and Steam open the game the same way, and
anti-cheat handle-stripping callbacks leave this right alone because
stripping it would break Task Manager. Nothing is read from the process's
memory.

Why it is still worth removing: it is the only moment telemouse holds a
handle to `cod.exe`, a kernel driver that logs handle opens would see an
unsigned executable from a user directory doing it, and the design document
(`mouse-telemetry-plan.md:25`) promises "no handles into the game process",
which the code only approximates.

Fix: resolve PID to executable name from a `CreateToolhelp32Snapshot`
process snapshot instead. The snapshot is a kernel-built object and opens no
handle to any process; `crates/ctl/src/procs.rs:377-401` already contains
the exact loop. It costs about 7 ms once per alt-tab on the 250 ms context
thread, never on the capture thread, and it also reports the name of an
*elevated* game, which `OpenProcess` refuses from a non-elevated agent.
That removes the reason for F5 too.

### F2. Medium (developer tool, not shipped): `tmbench inject` synthesizes input

`tools/cpubench/src/main.rs:27-84` calls `SendInput` with relative mouse
motion at the requested rate and a left-button press every 1000 events and
release 500 later. This is the input-automation primitive the policy names.
Synthetic input is also distinguishable from hardware input (a raw-input
listener sees a null device handle; low-level hooks see the injected flag),
so an anti-cheat that watches for it would see exactly that.

Mitigating facts: the crate is not a workspace member, neither release zip
contains it (`.github/workflows/release.yml:84,89`), and
`bench.ps1:17` already says not to run a game during a bench run. It is,
however, built on this machine (`tools/cpubench/target/release/tmbench.exe`,
2026-08-30), and `docs/AGENTIC-2026-09-13.md:74` (item B2, the experiment
runner) proposes letting an agent call `tmbench inject` for synthetic input.

Fix: make `inject` refuse to run unless an explicit opt-in is present (an
environment variable such as `TMBENCH_ALLOW_INJECT=1` or a `--i-am-not-in-a-game`
flag), print what it is about to do, and keep that gate in the experiment
runner. Do not add a list of game executable names to it; an executable that
carries `SendInput` plus game process names is the classic macro signature
and would look worse than the current code. Keep it out of the release zips,
as now.

### F3. Low: unsigned binaries with no version resource

All four release executables are unsigned and carry no `FileDescription`,
`CompanyName` or `ProductVersion` (Appendix A). This is not something RICOCHET
acts on, but it is the standard antivirus and SmartScreen heuristic profile:
unknown publisher, reads raw input, hides its own console, runs from a user
folder. It also makes any support conversation harder, because the file
cannot be traced to a publisher. The README already warns about SmartScreen
and some antivirus products, and code signing (Azure Trusted Signing) is on
the shipping list.

The public record makes this the best-evidenced real-world hazard for
input-capture software, ahead of anything anti-cheat related: Keyviz, a
keystroke visualiser, has five closed issues from antivirus false
positives, including Windows Defender classing it as
`Backdoor:Win32/Bladabindi!ml`. telemouse registers only the mouse usage
page and never sees keystrokes, which is its strongest defence and should
be stated in the user-facing docs.

Fix: add a version resource and an application manifest through a build
script (`winresource` or `embed-resource`), and sign the release binaries
before distributing them beyond this machine. The provenance attestation in
`release.yml` is a good bridge but is not what Windows or a user looks at.

### F4. Low: the panel imports `NtQueryInformationProcess`

`crates/ctl/src/procs.rs:409-448` reads a process's command line through
`NtQueryInformationProcess(ProcessCommandLineInformation)`, imported
statically from ntdll (`Wdk_System_Threading` in `crates/ctl/Cargo.toml`).
It is only ever called for processes whose name starts with `telemouse` or
`cargo`, and it is the documented, handle-light way to get a command line.
The only reason it exists is to tell a `cargo run -p telemouse-viz` from any
other cargo, which matters on a developer box and never for a user of the
release zip. The import is the classic anti-debug API and a mild static
heuristic.

Fix (optional): read the command line only for `cargo` candidates, or gate
the cargo classification behind a developer feature so the minimal zip's
panel has no ntdll import.

### F5. Low: a hint that tells users to run the panel as administrator

`crates/ctl/src/manager.rs:254-257` turns a child's "os error 5" into
"access denied: the target runs elevated; run the panel as administrator".
Elevating the panel elevates every child, so the capture agent would then
open its handle to the game from an elevated process, which is a worse
posture for no gain. With F1 fixed the elevated-game case needs no handle
at all. The error itself is Windows' integrity boundary (a non-elevated
process cannot query an elevated one), not a detection event.

Fix: reword the hint (it should say what failed and that elevation is not
needed), and never advise elevation.

### F6. Informational: hard-coded F9 hotkey

The capture agent registers F9 system-wide with no way to change it
(`raw_input.rs:364-371`; `main.rs:516` passes `Hotkey::default()`). A registered
hotkey is consumed before the game sees it, so an F9 bound in-game stops
working while capture runs. Not an anticheat matter; the panel's chord is
configurable and F9 could follow it.

### F7. Informational: what the raw-input registration is and is not

`RIDEV_INPUTSINK` asks Windows to deliver copies of mouse reports to a
window that is not in the foreground. It does not hook, filter, delay or
modify the game's input, and it is per-window state inside win32k that no
public API lets another process enumerate. Only the mouse usage is
registered, so the agent never sees keystrokes and is not a keylogger. The
window class names (`TelemouseRawInputClass`, `TelemouseCtlWindow`) collide
with nothing.

The popular stream input overlays are not a precedent for this mechanism:
NohBoard and the OBS input-overlay plugin do not use Raw Input, they
install global `WH_KEYBOARD_LL`/`WH_MOUSE_LL` hooks (verified in their
sources), which run in the system hook chain. Raw Input never enters that
chain. telemouse should describe its mechanism on its own terms rather than
by kinship with those tools; the difference favours it.

### F8. Informational: priority and power settings

`THREAD_PRIORITY_ABOVE_NORMAL` on one thread inside the normal priority
class, and an opt-out of EcoQoS throttling, are what audio and capture
software do. There is no `SetPriorityClass`, no realtime class, and no timer
resolution change.

### F9. Informational: dynamic `RtlGetVersion` lookup

`platform.rs:96-116` resolves `RtlGetVersion` with `GetProcAddress`. This is
the documented way to read the build number without a manifest. If the
manifest from F3 lands, `GetVersionExW` becomes truthful and the lookup can
go; alternatively the `windows` crate imports `RtlGetVersion` statically
(`Wdk_System_SystemServices`). Cosmetic either way.

### F10. Informational: the process scanner never touches the game

`classify` (`procs.rs:73-88`) and `name_is_candidate` (`procs.rs:94-98`)
are the only definitions of "related". Per-process queries run only for
candidates (`procs.rs:220-282`); `kill` re-classifies the live process and
refuses itself and anything unrelated (`procs.rs:287-320`); the tests pin
this (`procs.rs:646-680,766-778`). The 30 s snapshot lists every process by name
without opening any of them.

### F11. Informational: recordings are a focus log

Every batch carries the foreground executable name, the pointer-lock state
and, outside games, the cursor position; the session record carries device
strings and monitor layout (`crates/core/src/batch.rs:16-29`). On this
machine the 39 recordings tag about 20 hours of `cod.exe` in the foreground
alongside `chrome.exe`, `discord.exe`, `tradingview.exe` and IDEs. The
README documents this. It is a privacy consideration for sharing recordings,
not an anticheat one.

### F12. Documentation: "anticheat-safe by design"

`README.md:8-12`, `docs/GUIDE.md:53-57` and `mouse-telemetry-plan.md:12,16`
say telemouse is "anticheat-safe". The code supports "passive" and "does
nothing anti-cheat systems act on"; it cannot support "safe", because no
vendor whitelists third-party tools and Activision's policy leaves the
judgement to them. Suggested wording: "Passive by design: it reads a copy of
raw input and the foreground process name, and nothing else. It does nothing
that anti-cheat systems are documented to act on, and it is not endorsed by
any game publisher." A short `docs/FAIR-PLAY.md` carrying section 1's table
would let a user see exactly what runs on their machine. It should also
say, in plain words, that telemouse maps, remaps and synthesizes no input,
because "input mapping software" is the one phrase in the policy's
enumerated list that a reader could misapply to a mouse tool. No
self-service allowlist exists: the only application Activision has ever
approved by name is the Cephable accessibility app, bespoke, limited to
non-competitive modes, and revocable.

### F13. Informational: cheat-adjacent vocabulary

In October 2024 RICOCHET banned players on context-free matches of strings
such as "Trigger Bot" that cheat developers pushed into *game* memory
through chat and friend requests. telemouse's own strings never enter the
game process, so the exposure is theoretical, but it is the one mechanism
by which a program that does not interact with the game was ever caught.
Keep process names, window class names and marker labels boring (they are),
and never paste analyzer output, which talks about "trigger discipline" and
"recoil", into in-game chat.

## 3. What RICOCHET and the policy act on

Public-record summary, compiled 2026-09-14. Every claim below is tagged
official, journalism, vendor or community in the source notes
([ANTICHEAT-2026-09-14-sources.md](ANTICHEAT-2026-09-14-sources.md)), which
also list what was searched for and not found. Two official documents could
not be fetched by automation and should be read by hand before relying on
this section for a compliance decision: Activision's Software Terms of Use
(the binding contract) and the RICOCHET support page.

**The policy prohibits modifying, not reading.** The Security & Enforcement
Policy (updated 07/31/26) defines unauthorized software as code "not
authorized by Activision that can be used in connection with the game ...
which changes and/or facilitates the gameplay ... including to gain an
unfair advantage, manipulate stats, and/or manipulate game data",
enumerated as "aimbots, wallhacks, trainers, stats hacks, texture hacks,
leaderboard hacks, injectors, input mapping software, or any other software
used to deliberately modify game data on disk or in memory". Unsupported
peripherals are "unapproved input modification devices, modded controllers,
IP flooders, and lag switches". Overlays, streaming and recording tools,
VPNs, macros, scripts and passive tools appear nowhere on the page. The
penalty ladder is warnings, temporary and permanent bans, limited
matchmaking, ranked restrictions, in-game mitigations and hardware bans;
temporary bans and limited matchmaking cannot be appealed.

**What RICOCHET says it detects.** The official overview (updated 11/10/25):
the kernel driver "monitors the machine and processes interacting with a
game ... to determine if they are manipulating the game", loads with the
game and unloads when it exits. The same page reserves wider ground:
"monitoring applications running on a machine during gameplay, detecting
anomalies in gameplay using behavioral models, validating that hardware has
not been tampered with". That sentence is why no third-party tool can call
itself documented-safe; it does not say what the observation is used for,
and no documented case exists of a non-interacting application being
actioned. Named programmes: third-party hardware passthroughs (XIM, Cronus
Zen, ReaSnow S1) since April 2023; "tools to activate aim assist while using
a mouse and keyboard" since January 2024, where "the Call of Duty
application will close if detected" and repeat use "may lead to further
account action"; machine-learning behavioural models and a Replay
Investigation Tool since late 2023; and, since 2026-02-05, input detection
that "focus[es] on how inputs behave, not which device is plugged in ...
analyzes input timing, consistency, and response patterns". TPM 2.0 and
Secure Boot have been required since Season 05 (2025-08-07); failing
attestation restricts playlists and is not a ban.

**Shadowbans.** Officially a state in which "an alarm was raised", not a
verdict. The triggers the developers name are "a major change in an
account's behavior" or "a brand-new account ... dropping improbable stats";
"spam reporting does nothing"; under 0.15% of players are in the state at
any time; party members are pulled in with you. No software of any kind is
named as a trigger, and no corroborated case of software causing a
shadowban by its presence was found.

**Track record of neighbouring software.** Every documented adverse outcome
involves something that modifies, injects, emulates, filters or hides
input: reWASD (the game closes; the vendor says Activision made the game
"unworkable if reWASD is installed"), Cronus/XIM/ReaSnow (the full ladder to
hardware bans), and a QuadStick sip-and-puff accessibility controller wrongly
temp-banned by the behavioural detector in May 2026 and reversed only after
publicity. Nothing was found, at any tier, against a tool that only reads
input: no report of any kind for mouse testers, aim trainers, NohBoard, the
OBS input-overlay plugin or Gamepad Viewer. Raw Accel, a kernel driver that
*modifies* mouse input, has one refuted shadowban claim and no confirmed
Call of Duty ban in a community with many CoD players. Razer Synapse is
named positively in Activision's own troubleshooting; MSI Afterburner, RTSS,
Discord and NVIDIA overlays appear only as crash conflicts. Activision's
crash guidance says "input remapping software, overlays, and recording
software may interfere ... even if they're not running" and recommends
uninstalling them: stability advice, never enforcement.

**Documented false positives** were all internal to RICOCHET: a
context-free string-signature scan of game memory (October 2024), an
"overlap between separate detections" that banned a pro player during the
MW4 beta (September 2026), and the QuadStick behavioural misfire. None was
caused by legitimate software on the player's PC.

**What this means for telemouse.** Nothing in the shipped binaries touches
the game, modifies input, or looks like an account anomaly. Activision
publishes no allowlist; the sole named approval in Call of Duty's history
is the Cephable accessibility app (bespoke, Zombies and co-op campaign only,
revocable); and BattlEye's explicit "no one is banned for ... passive
non-cheating activity" has no Activision equivalent. So the defensible claim
is "passive, and does nothing anti-cheat systems are documented to act on",
not "safe". The best-evidenced real-world hazard for input-capture software
is not anti-cheat at all but antivirus keylogger heuristics against
unsigned binaries (F3).

## 4. Recommendations, in order

1. **Replace `OpenProcess` in `platform::process_name` with a Toolhelp
   lookup** (F1). Keep `ForegroundCache`. Reuse the loop from
   `crates/ctl/src/procs.rs:377-401`. Update `doctor` the same way. After
   this the capture agent holds no handle to any process but itself.
2. **Gate `tmbench inject` behind an explicit opt-in** (F2), and carry the
   gate into the agentic experiment runner before it is built.
3. **Sign the release binaries and add a version resource and manifest**
   (F3) before distributing beyond this machine.
4. **Reword `HINT_ACCESS_DENIED`** so it never advises elevation (F5).
5. **Fix the docs' claim** (F12) and add `docs/FAIR-PLAY.md`.
6. Optional: drop the ntdll import from the minimal panel build (F4); make
   the F9 hotkey configurable (F6).

If the binaries are about to go to other people, do item 3 first: the
public record says antivirus heuristics, not anti-cheat, are what actually
bites unsigned input-capture tools.

### Status, 2026-09-14 (same day)

Landed in the working tree:

- **F1**: `platform::process_name` reads the name from a Toolhelp process
  snapshot; the capture agent opens no handle on any process but itself.
  `doctor` uses the same path. Test: `the_process_table_names_this_process_and_nobody_else`.
- **F2**: `tmbench inject` refuses to run without `TMBENCH_ALLOW_INJECT=1`;
  `bench.ps1` sets it for its own load phase only; the AGENTIC B2 row
  carries the guard.
- **F3, half**: every executable carries a version resource and the
  `telemouse.manifest` (`asInvoker`, PerMonitorV2 DPI, Windows 10+) through
  a `build.rs` on `winresource`. Code signing still needs a certificate
  (Azure Trusted Signing, on the shipping list) and is not done.
- **F5**: `HINT_ACCESS_DENIED` names the protected-folder cause and says
  elevation is not needed.
- **F6**: `marker_hotkey` in `telemouse.toml` (F9 by default, `""` for
  none); a `[ctl] hotkey` equal to it is rejected at load.
- **F12**: README, GUIDE and the plan no longer say "anticheat-safe";
  `docs/FAIR-PLAY.md` states the surface; `CLAUDE.md` carries the posture
  as a rule.

Deliberately not done: **F4**. Dropping the command line from the panel's
process table (the only use of `NtQueryInformationProcess`) would cost the
column that shows which flags a running agent was started with, and the
only handle-free alternative is worse (reading the PEB with
`PROCESS_VM_READ`). The import is what Task Manager itself uses for the
same column; it stays.

## 5. Sources

The load-bearing ones; the full list of 90 with tiers and dates is in
[ANTICHEAT-2026-09-14-sources.md](ANTICHEAT-2026-09-14-sources.md).

Official (Activision / Call of Duty):

- Call of Duty Security and Enforcement Policy, updated 07/31/26:
  <https://support.activision.com/articles/call-of-duty-security-and-enforcement-policy>
- Appeal a Ban: <https://support.activision.com/ban-appeal>
- RICOCHET Anti-Cheat overview, updated 11/10/25:
  <https://support.activision.com/articles/ricochet-overview>
- TPM 2.0 and Secure Boot, updated 08/27/26:
  <https://support.activision.com/articles/trusted-platform-module-and-secure-boot>
- Cephable approval, updated 04/09/26:
  <https://support.activision.com/articles/expanding-accessibility-options-in-call-of-duty-using-cephable>
- Black Ops 6 PC troubleshooting (overlays and recording software as a
  stability issue), 07/02/25:
  <https://support.activision.com/black-ops-6/articles/black-ops-6-pc-troubleshooting>
- Warzone 2 PC troubleshooting (Razer Synapse named positively), 03/11/26:
  <https://support.activision.com/warzone-2/articles/warzone-2-pc-troubleshooting>
- RICOCHET announcement, October 2021:
  <https://news.blizzard.com/en-us/article/23733251/ricochet-anti-cheat-call-of-dutys-new-anti-cheat-initiative>
- RICOCHET progress report, Black Ops 6 launch, October 2024:
  <https://news.blizzard.com/en-us/blizzard/24150099/ricochet-anti-cheat-progress-report-black-ops-6-launch>
- RICOCHET progress report, Season 01, November 2024:
  <https://news.blizzard.com/en-us/article/24151773/ricochet-anti-cheattm-progress-report-season-01-and-ranked-play>
- TeamRICOCHET Season 02 update (behavioural input detection), 2026-02-02:
  <https://news.blizzard.com/en-us/article/24243445/teamricochet-season-02-update>

Journalism:

- Engadget, XIM/Cronus/ReaSnow detection, 2023-04-05:
  <https://www.engadget.com/call-of-duty-can-detect-and-ban-xim-style-cheat-hardware-100314416.html>
- TechSpot, game closes on mouse-and-keyboard aim-assist tools, 2024-01-17:
  <https://www.techspot.com/news/101549-call-duty-anti-cheat-system-now-close-game.html>
- Dexerto, developers explain shadowbans and spam reports, 2025-03-28:
  <https://www.dexerto.com/call-of-duty/black-ops-6-warzone-devs-finally-explain-how-spam-reports-shadowbans-work-3172211/>
- TechCrunch, string-signature ban exploit, 2024-11-07:
  <https://techcrunch.com/2024/11/07/hacker-says-they-banned-thousands-of-call-of-duty-gamers-by-abusing-anti-cheat-flaw>
- Dexerto, Cronus/XIM crackdown via input behaviour, 2026-02-02:
  <https://www.dexerto.com/call-of-duty/cod-cracks-down-on-cronus-zen-xim-in-major-anti-cheat-update-for-black-ops-7-season-2-3313252/>
- GameRant, QuadStick accessibility controller wrongly banned, 2026-05-23:
  <https://gamerant.com/cod-paralyzed-streamer-banned/>
- NME, MW2 ban wave attributed to crash-induced false positives, 2022-12-16:
  <https://www.nme.com/news/gaming-news/modern-warfare-2-players-hit-with-perma-bans-due-to-faulty-anti-cheat-software-3368068>

Vendors and source code:

- reWASD, "games unworkable if reWASD is installed", 2024-02-05:
  <https://www.rewasd.com/blog/post/cancel-culture>
- Raw Accel issue #238 (shadowban claim, refuted), 2024-09-13:
  <https://github.com/RawAccelOfficial/rawaccel/issues/238>
- BattlEye support, "no one is banned for ... passive non-cheating activity":
  <https://www.battleye.com/support/>
- NohBoard hook imports:
  <https://raw.githubusercontent.com/ThoNohT/NohBoard/master/NohBoard/Hooking/Interop/FunctionImports.cs>;
  libuiohook (OBS input-overlay) Windows backend:
  <https://raw.githubusercontent.com/kwhat/libuiohook/1.2/src/windows/input_hook.c>;
  Microsoft, About Hooks:
  <https://learn.microsoft.com/en-us/windows/win32/winmsg/about-hooks>
- Keyviz antivirus false positives:
  <https://github.com/mulaRahul/keyviz/issues>

Not retrieved, read by hand: Activision Software Terms of Use
(<https://www.activision.com/legal/software-terms-of-use>) and
`support.activision.com/ricochet-anti-cheat`.

## Incidental, out of scope

`WM_DISPLAYCHANGE` is a broadcast to top-level windows, and the raw-input
window is a message-only window, which by the `CreateWindowEx`
documentation receives no broadcast messages. The display-change path
(`raw_input.rs:467-475`, consumed in `context_thread.rs:347-350`) is
therefore probably never exercised, and the screen size in batches is read
once at startup. Not an anticheat matter; noted because the code was read.

## Appendix A. Binary scan (release builds of 2026-09-13)

ASCII scan of the PE files for API names; every Rust executable also imports
`GetProcAddress`, `LoadLibraryA`, `TerminateProcess` and
`IsDebuggerPresent` through the standard library's runtime, which is why
they appear in `telemouse-viz.exe` and `telemouse-analyze.exe` too.

| Executable | Signed | Version resource | Input / process API names present |
|---|---|---|---|
| `telemouse.exe` | no | none | `RegisterRawInputDevices`, `GetRawInputBuffer`, `GetRawInputData`, `GetRawInputDeviceList`, `RegisterHotKey`, `GetForegroundWindow`, `GetWindowThreadProcessId`, `OpenProcess`, `QueryFullProcessImageNameW`, `GetCursorPos`, `SetThreadPriority`, `SetProcessInformation`, `RtlGetVersion`, `CreateWaitableTimerExW` |
| `telemouse-ctl.exe` | no | none | `CreateToolhelp32Snapshot`, `OpenProcess`, `QueryFullProcessImageNameW`, `NtQueryInformationProcess`, `TerminateProcess`, `RegisterHotKey`, `GetKeyState`, `GetCursorPos`, `Shell_NotifyIconW`, `GenerateConsoleCtrlEvent` |
| `telemouse-viz.exe` | no | none | runtime only |
| `telemouse-analyze.exe` | no | none | runtime only |
| `tmbench.exe` (not shipped) | no | none | **`SendInput`**, `OpenProcess`, `CreateToolhelp32Snapshot`, `CreateWaitableTimerExW` |

Not present in any shipped executable: `SendInput`, `mouse_event`,
`keybd_event`, `SetCursorPos`, `ClipCursor`, `BlockInput`,
`SetWindowsHookExW/A`, `SetWinEventHook`, `ReadProcessMemory`,
`WriteProcessMemory`, `VirtualAllocEx`, `CreateRemoteThread`,
`NtQuerySystemInformation`, `GetAsyncKeyState`, `BitBlt`, `GetDC`,
`FindWindowW`, `EnumWindows`, `SetWindowDisplayAffinity`, `SetPriorityClass`.

## Appendix B. Exposure so far

The recordings directory on this machine holds 39 sessions. Batches tagged
with a foreground `cod.exe`: 2,962,443, which at 40 batches per second is
about 20.6 hours of Call of Duty played with the agent running, plus the
full build's Kafka sink and, at times, the LAN-bound viz. No enforcement
action has been reported against the account in that period. That is
consistent with the analysis above; it is not proof, and it says nothing
about future detection changes.
