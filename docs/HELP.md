# telemouse help

The [README](../README.md) gets you recording in a minute. This page is
the rest: the details you look up once.

- [Checking the download](#checking-the-download)
- [The window and the tray](#the-window-and-the-tray)
- [Setting up your mouse and game](#setting-up-your-mouse-and-game)
- [The live dashboard](#the-live-dashboard)
- [OBS overlay](#obs-overlay)
- [Hotkeys](#hotkeys)
- [What it writes, and how to remove it](#what-it-writes-and-how-to-remove-it)
- [Troubleshooting](#troubleshooting)
- [Reporting problems](#reporting-problems)

## Checking the download

Each [release](https://github.com/uwdivad/telemouse/releases) is one zip,
`telemouse-vX.Y.Z-windows-x86_64.zip`, with a `.sha256` alongside and a
build-provenance attestation that says which commit and workflow built it.

- To verify: `Get-FileHash .\telemouse-*.zip -Algorithm SHA256` in
  PowerShell, and compare with the `.sha256` file.
- Before unzipping, right-click the zip → *Properties* → tick **Unblock**,
  so Windows does not mark every extracted file as downloaded.
- Unzip anywhere you can write to (not `Program Files`).

If Windows shows a blue *Windows protected your PC* box on the first start,
choose *More info → Run anyway*. SmartScreen does that for programs it has
not seen much of yet, signed or not.

## The window and the tray

`telemouse-ctl.exe` opens a window titled **telemouse** and puts a disc in
the tray: grey when idle, green while a recording runs.

- Closing the window hides it to the tray. Left-click the icon to bring it
  back, right-click for the menu.
- *Exit* in the tray menu quits (Shift+close does the same) and stops any
  recording cleanly.
- Tray → *Open in browser* opens the same panel in your default browser.

If the window shows only text with a banner at the top, the **WebView2
Runtime** (the browser engine that ships with Windows 10 and 11) is missing
or could not start. The panel opens in your browser instead and everything
still works; install the WebView2 Runtime from Microsoft to get it in the
window.

## Setting up your mouse and game

The first-run guide asks for two things, and both live under **Settings**
afterwards. No file needs editing.

- **CPI** (also called DPI): from your mouse's software or spec sheet.
  telemouse needs it to turn mouse counts into centimetres on your desk.
- **Your game**: start the game, alt-tab back, and pick it from the
  programs the panel saw in front (or type its exe name). Enter your
  in-game sensitivity. `yaw_coeff` is the engine's degrees per count:
  0.022 for Source-engine games, 0.0066 for modern Call of Duty and
  Overwatch, 0.07 for Valorant.

A wrong CPI or sensitivity is not fatal: recordings store raw counts, so
fix the setting and run the report again.

Network addresses, Kafka and folder locations are not in Settings on
purpose; those are edited in `telemouse.toml` (tray → *Edit
telemouse.toml*). Every key is explained in the file and in
[GUIDE.md](GUIDE.md).

## The live dashboard

Start the live view from the panel's Session section and open the
dashboard (`http://127.0.0.1:7879`): your hand's path in real centimetres,
your crosshair's path in degrees, click rings, live readouts, and replay of
any recording with scrubbing and speed control.

The dashboard tab is the expensive part, not the capture agent: two full
canvases repainting on the game's GPU. Add `?fps=60` to its URL while
playing, or close it and open it afterwards for replay. The recording is
complete either way.

## OBS overlay

Add a **Browser** source in OBS with `http://127.0.0.1:7879/obs` and size
it however you like; the panels fill the source. Tick *Refresh browser when
scene becomes active* if you toggle the scene a lot. The overlay's look is
under **Settings** in the panel; every option is also a URL parameter, see
[GUIDE.md](GUIDE.md).

The overlay dims and shows *no feed* after a few seconds without mouse
data, which includes a hand at rest. If you would rather it never dimmed,
add `?stale=0` to the OBS URL (or a larger number of seconds), or set
`stale_secs` under `[viz.obs]` in `telemouse.toml`.

### OBS on a second PC

A streaming PC on the same LAN can load the overlay from the gaming PC.

1. On the gaming PC, set `http_addr = "0.0.0.0:7879"` under `[viz]` in
   `telemouse.toml` and restart the live view.
2. Allow the port through the firewall, from an elevated PowerShell (the
   shared network must be marked *Private* in Windows):

   ```powershell
   New-NetFirewallRule -DisplayName "telemouse-viz (LAN)" -Direction Inbound -Protocol TCP -LocalPort 7879 -Action Allow -Profile Private
   ```

3. In OBS use the gaming PC's IP address (`http://192.168.1.20:7879/obs`,
   from `ipconfig`), not its name: a hostname is refused.

The other PC gets the overlay and nothing else. The live stream has no
login, so keep it to a network you trust and set the bind back to
`127.0.0.1:7879` when you are done.

## Hotkeys

- **F9** drops a marker into the recording ("clutch", "round start") from
  inside the game. The panel's marker field does the same with a label.
- **Ctrl+Alt+R** stops the current recording and starts a new saved one,
  from inside the game.

Both are changed or disabled under **Settings** (a new Ctrl+Alt+R chord
applies on the panel's next start). Windows hands a registered chord to
telemouse before the game sees it, so pick ones the game does not use.

## Asking an AI assistant about your sessions

`telemouse-mcp.exe` in the folder is an MCP server: it hands an assistant
that speaks MCP (Claude Code, and any other client) a set of tools over your
own recordings — list the sessions, summarise one, compare a week, check
whether capture is healthy right now, tail a log. With them it can answer
"which sessions lost data this week" or "is my overshoot getting better"
without you looking anything up. For Claude Code:

```powershell
claude mcp add telemouse -- "C:\path\to\telemouse\telemouse-mcp.exe"
```

It reads your `telemouse.toml` for where the recordings and the servers are,
and it runs only while the assistant has it open. It can also start, mark
and stop a recording — everything it does goes through the control panel,
with the same limits the panel has, and it can only ever reach this machine.
Add `--read-only` after the path if you would rather it could only look:

```powershell
claude mcp add telemouse -- "C:\path\to\telemouse\telemouse-mcp.exe" --read-only
```

Nothing leaves your PC except what the assistant itself sends to its own
service — which is the answers and the numbers it quotes, never a recording
(they are far too large) and never raw mouse events.
[API.md](API.md) lists every tool.

## What it writes, and how to remove it

Nothing is installed, registered or scheduled. Everything is next to
`telemouse-ctl.exe`:

| What | Where |
|---|---|
| Settings | `telemouse.toml` |
| Recordings | `recordings\<id>.jsonl`, each with a `<id>.meta.json` sidecar |
| Report caches | `recordings\.reports\`, `recordings\.telemouse-analyze-index-v1.json` |
| Logs | `logs\ctl.log`, `logs\<component>.log` (including `logs\mcp.log`) |
| The window's browser cache | `%LOCALAPPDATA%\telemouse\WebView2` (the one thing outside the folder) |

Delete the folder and the WebView2 one, and it is gone. If you added the
firewall rule for OBS on a second PC,
`Remove-NetFirewallRule -DisplayName "telemouse-viz (LAN)"` removes it.

Recordings are never pruned; they grow at roughly 150 MB per hour of active
play at 1 kHz. A recording is also a log of which program had focus, and
the session record carries your monitor layout and mouse device names, so
trim before sharing.

## Troubleshooting

- **"telemouse-ctl is already running"**: the port is in use, almost always
  by another panel, whose page is opened in your browser instead. The other
  one is in the tray; quit it there, or start a second panel with
  `telemouse-ctl.exe serve --http 127.0.0.1:7899`.
- **"telemouse.exe was not found next to telemouse-ctl.exe"**: the zip was
  extracted partially, or an antivirus quarantined it. Re-extract.
- **The tray icon is gone** after Explorer restarted: it comes back on its
  own; give it a second. If the window is there but the icon never was,
  closing the window quits.
- **Antivirus flags it**: a little-known program that reads raw input is
  what heuristics look for, more so while a release is not code-signed yet
  (right-click an exe → Properties → *Digital Signatures* shows whether
  yours is). It never registers for keyboard input. The release page's
  attestation says which commit built it; building from source is the
  other option ([DEVELOPING.md](https://github.com/uwdivad/telemouse/blob/master/docs/DEVELOPING.md)).
- **The window is text-only**: the WebView2 Runtime is missing or could not
  start; the banner says why. Install it from Microsoft, or use the page in
  your browser (tray → *Open in browser*). `telemouse-ctl.exe serve
  --no-webview` keeps the text window on purpose.
- **Nothing starts after editing `telemouse.toml` by hand**: every binary
  refuses a file it cannot parse rather than running on defaults. Fix the
  line it names (game keys must be lowercase exe names), or delete the file
  and the panel writes the sample again on its next start.
- **The OBS overlay says "no feed" while I hold still**: see
  [OBS overlay](#obs-overlay).
- **Run it elevated?** No. Nothing needs administrator rights. "Access
  denied" means the folder is protected; move telemouse to a folder you own.

## Reporting problems

Open an [issue](https://github.com/uwdivad/telemouse/issues) with the
version (the panel's header, or `telemouse-ctl.exe --version`), what you
did, and `logs\ctl.log` and `logs\capture.log`. The panel's **Check my
setup** prints the resolved config and environment checks; it lists your
mouse's device strings and the program in the foreground, so trim anything
you would rather not post.
