# cpubench — cycle-exact CPU harness for the live stack

Its own tiny crate (`tmbench`, not a workspace member) plus `bench.ps1`.
Windows only, like the capture agent.

```
tmbench inject <hz> <secs>              synthetic relative mouse motion via SendInput
tmbench ws <url> <secs>                 drain a viz WebSocket like a browser page
tmbench http <url> <secs> <interval_ms> GET a URL on a timer, like the ctl page's poll
tmbench measure <secs> <label=pid>...   QueryProcessCycleTime / QueryThreadCycleTime per target,
                                        per thread with its name (T1/T2/T3 show up by name)
tmbench tcp|udp <addr> <bytes> <hz> <secs>, tcpsink|udpsink <addr> <secs>
                                        loopback send cost micro-benchmarks
```

`bench.ps1` runs the whole thing (see its header). The numbers it produced
on 2026-08-29 are in `results-2026-08-29.jsonl` and summarised in
[docs/BENCHMARKS.md](../../docs/BENCHMARKS.md).

Why cycle counts: Windows' `GetThreadTimes` is sampled at the clock tick and
hid 30% deltas at these levels; `QueryThreadCycleTime` is exact. The
percentages are *of one core* at the calibrated cycle rate of the measuring
thread (the harness spins 300 ms to calibrate), so turbo state adds a few
percent of noise between runs — compare runs taken back to back.
