# Fair play: what telemouse does on your machine, and what it never does

telemouse records your mouse. It is meant to run while you play, so this
page says exactly what it touches, in the terms an anti-cheat system or a
game's terms of service would use. The evidence behind every line is in
[ANTICHEAT-2026-09-14.md](ANTICHEAT-2026-09-14.md) (a read of every
Windows call in the code, a scan of the built executables, and the public
record on Call of Duty's RICOCHET anti-cheat and Activision's Security &
Enforcement Policy).

## The short version

telemouse only reads. It asks Windows for a *copy* of raw mouse reports,
reads the name of the program in the foreground, and writes what it saw to
files, to loopback sockets and, if you turn it on, to your own Kafka broker.

It never modifies, generates, remaps, delays or filters input. It maps
nothing and automates nothing. It installs no driver, no hook and no
overlay in the game, opens no handle on the game process, reads and writes
no game memory, and changes no game file and no network traffic. It does
nothing that anti-cheat systems are documented to act on.

That is the whole claim. No game publisher endorses third-party tools;
Activision publishes no allowlist, and its policy leaves the judgement to
it. So telemouse is *passive*, and its authors say so precisely rather than
saying "safe".

## Everything it does, by Windows call

| What | How | What the game can see |
|---|---|---|
| Receives mouse input | `RegisterRawInputDevices` with `RIDEV_INPUTSINK` on a hidden message-only window, for the mouse usage page only (never the keyboard: telemouse cannot see keystrokes) | Nothing. Windows delivers a copy; the game's own input is unchanged. |
| Names the foreground program | `GetForegroundWindow`, then the executable name from a process-table snapshot (`CreateToolhelp32Snapshot`), once per foreground change | Nothing. No handle is opened on the game. |
| Knows when a game has taken the cursor | Compares `GetCursorPos` between ticks | Nothing. |
| Lists your mice | `GetRawInputDeviceList` / `GetRawInputDeviceInfo` (device names) | Nothing. |
| Marker hotkey | `RegisterHotKey` for `marker_hotkey` (F9 by default) in the agent, and `[ctl] hotkey` (Ctrl+Alt+R) in the panel | The chord goes to telemouse instead of the game; pick one the game does not use, or set `""`. |
| Keeps its own thread responsive | `SetThreadPriority(ABOVE_NORMAL)` on one thread; an opt-out of power throttling | Nothing. Normal priority class. |
| Control panel | A tray icon and one ordinary window that shows the panel page through WebView2, the browser engine that ships with Windows; a process list that only ever queries or stops `telemouse*` processes | An ordinary window. Never a topmost, layered or transparent one. WebView2 runs as Microsoft-signed `msedgewebview2.exe` child processes that render into that window; telemouse opens no handle on any other process, hooks nothing and draws nothing over a game. |
| Talks to | UDP 127.0.0.1:7878, HTTP 127.0.0.1:7879 and :7880 (the viz can be bound to your LAN for OBS), Kafka only if enabled | Ordinary traffic. No VPN, proxy or shaping. |
| Persists | Nothing: no service, no scheduled task, no registry entry, no startup item. Delete the folder and it is gone. | |

Not in any shipped executable: `SendInput`, `mouse_event`, `keybd_event`,
`SetCursorPos`, `ClipCursor`, `BlockInput`, `SetWindowsHookEx`,
`ReadProcessMemory`, `WriteProcessMemory`, `CreateRemoteThread`, screen
capture, and every driver or injection primitive.

## The one thing in the repository that does emit input

`tools/cpubench` is a developer benchmark harness, not a workspace member
and not in any release. Its `tmbench inject` subcommand feeds synthetic
mouse motion through `SendInput` to load the capture agent on an idle desk.
It refuses to run unless `TMBENCH_ALLOW_INJECT=1` is set, and it must never
run with a game open: to a game it is input automation.

## Things worth knowing

- **A recording is also a focus log.** Every batch carries the name of the
  foreground executable, the pointer-lock state and, outside games, the
  cursor position; the session record carries your monitor layout and mouse
  device strings. Trim before sharing.
- **Antivirus may frown.** The binaries are not code-signed yet, and an
  unsigned program that reads raw input is what heuristics look for; they
  carry a version resource naming them, and the release page carries a
  build-provenance attestation. telemouse never registers for keyboard
  input, which is the line between an input recorder and a keylogger.
- **Never run it elevated.** Nothing telemouse does needs administrator
  rights, and its manifest says so (`asInvoker`). An "access denied" means a
  folder it writes to is protected; move it to a folder you own.
- **Your input pattern is yours.** Modern anti-cheat also judges *how*
  input behaves. telemouse does not change the input the game sees, so it
  cannot affect that; what you do with its metrics is up to you.
