# Cycle-exact CPU benchmark of the live telemouse stack (capture + viz + ctl).
#
# Starts the three binaries from -BinDir on private ports (17878/17879/17880),
# connects -Clients WebSocket drains and a ctl page poller, measures 10 s idle,
# then injects synthetic mouse motion at -Hz with SendInput for -LoadSecs while
# measuring again. Per-process and per-thread CPU comes from
# QueryProcessCycleTime / QueryThreadCycleTime (Windows' tick-sampled thread
# times hide deltas of this size). One JSON line per run is appended to
# results.jsonl; the full measure/inject/ws/poll outputs go to logs\<label>-<stamp>\.
#
#   cargo build --release            # in this directory: builds tmbench
#   $env:CARGO_TARGET_DIR="..\..\target-bench"; cargo build --release --workspace   # the binaries under test
#   .\bench.ps1 -Label base                       # defaults from the config (window 25, coalesce 8)
#   .\bench.ps1 -Label w25c2 -WindowMs 25 -CoalesceMs 2
#   .\bench.ps1 -Label two-clients -Clients 2     # dashboard + OBS overlay
#
# Do not run cargo (or a game) while a run is in progress; do not run two runs
# at once (same ports). See docs/BENCHMARKS.md for the numbers this produced.
param(
    [string]$Label = "run",
    [string]$BinDir = (Join-Path $PSScriptRoot "..\..\target-bench\release"),
    [int]$IdleSecs = 10,
    [int]$LoadSecs = 30,
    [int]$Hz = 1000,
    [int]$Clients = 1,          # WebSocket clients on the viz bridge (dashboard = 1, + OBS overlay = 2)
    [int]$CtlPollMs = 2000,     # the ctl page polls /api/state this often
    [int]$CoalesceMs = -1,      # -1 = leave the config's default
    [int]$WindowMs = -1,        # -1 = leave the config's default
    [string]$CaptureArgs = "",
    [switch]$NoCtl
)
$ErrorActionPreference = "Stop"
$S = $PSScriptRoot
$BinDir = (Resolve-Path $BinDir).Path
$tm = Join-Path $S "target\release\tmbench.exe"
if (-not (Test-Path $tm)) { throw "build the harness first: cargo build --release (in $S)" }
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$logDir = Join-Path $S "logs\$Label-$stamp"
New-Item -ItemType Directory -Force $logDir, (Join-Path $S "recordings") | Out-Null

# One config per run so the batch settings can be varied from the driver.
$cfg = Join-Path $logDir "telemouse-bench.toml"
$recDir = (Join-Path $S "recordings").Replace('\', '/')
$batch = ""
if ($CoalesceMs -ge 0 -or $WindowMs -ge 0) {
    $batch = "[batch]`n"
    if ($CoalesceMs -ge 0) { $batch += "coalesce_ms = $CoalesceMs`n" }
    if ($WindowMs -ge 0) { $batch += "window_ms = $WindowMs`n" }
}
@"
mouse_cpi = 1600.0
$batch
[udp]
enabled = true
addr = "127.0.0.1:17878"

[kafka]
enabled = false

[recording]
enabled = true
dir = "$recDir"

[viz]
http_addr = "127.0.0.1:17879"

[ctl]
http_addr = "127.0.0.1:17880"
bin_dir = "$($BinDir.Replace('\', '/'))"
"@ | Set-Content $cfg

$viz = Start-Process -FilePath "$BinDir\telemouse-viz.exe" -ArgumentList @("serve","--config",$cfg) -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\viz.log"
$ctl = $null
if (-not $NoCtl) {
    $ctl = Start-Process -FilePath "$BinDir\telemouse-ctl.exe" -ArgumentList @("serve","--config",$cfg) -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\ctl.log"
}
Start-Sleep -Milliseconds 800
$totalSecs = $IdleSecs + $LoadSecs + 12
$capArgs = @("run","--config",$cfg,"--no-kafka","--duration-secs","$totalSecs") + ($CaptureArgs -split ' ' | Where-Object { $_ })
$cap = Start-Process -FilePath "$BinDir\telemouse.exe" -ArgumentList $capArgs -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\capture.log"
Start-Sleep -Seconds 2
$targets = @("capture=$($cap.Id)", "viz=$($viz.Id)")
if ($ctl) { $targets += "ctl=$($ctl.Id)" }

# Long-lived clients for the whole run: WS drains + the ctl page poll.
$phaseSecs = $IdleSecs + $LoadSecs + 6
$wsProcs = @()
for ($i = 0; $i -lt $Clients; $i++) {
    $wsProcs += Start-Process -FilePath $tm -ArgumentList @("ws","ws://127.0.0.1:17879/ws","$phaseSecs") -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\ws-$i.txt"
}
$poll = $null
if ($ctl) {
    $poll = Start-Process -FilePath $tm -ArgumentList @("http","http://127.0.0.1:17880/api/state","$phaseSecs","$CtlPollMs") -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\ctlpoll.txt"
}
Start-Sleep -Milliseconds 700

# --- idle phase (clients connected, no input) ---
& $tm measure $IdleSecs @targets | Out-File "$logDir\measure-idle.txt"

# --- load phase ---
$meas = Start-Process -FilePath $tm -ArgumentList (@("measure","$LoadSecs") + $targets) -PassThru -NoNewWindow -RedirectStandardOutput "$logDir\measure-load.txt"
Start-Sleep -Milliseconds 400   # measure's 300ms calibration spin
Start-Process -FilePath $tm -ArgumentList @("inject","$Hz","$LoadSecs") -NoNewWindow -RedirectStandardOutput "$logDir\inject.txt" -Wait
$meas.WaitForExit()
foreach ($w in $wsProcs) { $w.WaitForExit() }
if ($poll) { $poll.WaitForExit() }

$cap.WaitForExit(15000) | Out-Null
foreach ($p in @($viz, $ctl)) { if ($p -and -not $p.HasExited) { $p.Kill() } }

$idle = Get-Content "$logDir\measure-idle.txt"
$load = Get-Content "$logDir\measure-load.txt"
function Pct($lines, $name) {
    $l = ($lines | Where-Object { $_ -like "proc $name *" })
    if ($l -and $l -match 'pct=([0-9.]+)') { [double]$Matches[1] } else { [double]::NaN }
}
function Threads($lines, $name) {
    ($lines | Where-Object { $_ -like "  thread $name *" }) | ForEach-Object {
        if ($_ -match 'name=(\S+) cpu_ms=([0-9.]+) pct=([0-9.]+)') { "$($Matches[1])=$($Matches[3])" }
    }
}
$r = [ordered]@{
    label = $Label; stamp = $stamp; hz = $Hz; clients = $Clients; ctl_poll_ms = $CtlPollMs; coalesce_ms = $CoalesceMs; window_ms = $WindowMs; capture_args = $CaptureArgs
    capture_idle_pct = Pct $idle "capture"; viz_idle_pct = Pct $idle "viz"; ctl_idle_pct = Pct $idle "ctl"
    capture_load_pct = Pct $load "capture"; viz_load_pct = Pct $load "viz"; ctl_load_pct = Pct $load "ctl"
    capture_threads_load = ((Threads $load "capture") -join " ")
    viz_threads_load = ((Threads $load "viz") -join " ")
    inject = (Get-Content "$logDir\inject.txt" -Raw).Trim()
    ws_load = (Get-Content "$logDir\ws-0.txt" -Raw).Trim()
    ctlpoll = if ($poll) { (Get-Content "$logDir\ctlpoll.txt" -Raw).Trim() } else { "" }
}
$sumIdle = 0.0; $sumLoad = 0.0
foreach ($k in 'capture','viz','ctl') { $v = $r["${k}_idle_pct"]; if (-not [double]::IsNaN($v)) { $sumIdle += $v }; $v = $r["${k}_load_pct"]; if (-not [double]::IsNaN($v)) { $sumLoad += $v } }
$r.total_idle_pct = [math]::Round($sumIdle, 3)
$r.total_load_pct = [math]::Round($sumLoad, 3)
Add-Content (Join-Path $S "results.jsonl") ($r | ConvertTo-Json -Compress)
"=== $Label ==="
"IDLE:"; $idle
"LOAD:"; $load
$r.inject; $r.ws_load; $r.ctlpoll
"TOTAL idle pct (capture+viz+ctl): $($r.total_idle_pct)"
"TOTAL load pct (capture+viz+ctl): $($r.total_load_pct)"
