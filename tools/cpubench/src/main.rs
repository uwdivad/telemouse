//! tmbench: load generator + cycle-exact CPU meter for telemouse benchmarks.
//!
//!   tmbench inject <hz> <secs>              inject relative mouse motion via SendInput
//!                                           (refuses unless TMBENCH_ALLOW_INJECT=1; never with a game open)
//!   tmbench ws <url> <secs>                 drain a viz WebSocket, report frames/bytes
//!   tmbench http <url> <secs> <interval_ms> GET a URL every interval (stands in for the ctl page poll)
//!   tmbench measure <secs> <label=pid>...   QueryProcessCycleTime / QueryThreadCycleTime per target
//!   tmbench udp <addr> <file> <secs>        replay a recording's batches over UDP, one per batch window
//!   tmbench tcpsink <addr> <secs>           accept one TCP connection and count what arrives
//!   tmbench tcp <addr> <bytes> <hz> <secs>  paced TCP sends, reporting this thread's cycles per send
//!   tmbench udpsink <addr> <secs>           bind a UDP socket and count what arrives
//!   tmbench udpsend <addr> <bytes> <hz> <secs> paced UDP sends, reporting this thread's cycles per send
use std::fmt::Display;
use std::io::{Read, Write};
use std::str::FromStr;
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: tmbench <subcommand> ...
  inject <hz> <secs>                 SendInput motion (needs TMBENCH_ALLOW_INJECT=1; never with a game open)
  ws <url> <secs>                    drain a viz WebSocket
  http <url> <secs> <interval_ms>    poll a URL
  measure <secs> <label=pid>...      cycle-exact CPU per process and thread
  udp <addr> <file> <secs>           replay a recording's batches over UDP
  tcpsink <addr> <secs>              count bytes on one accepted TCP connection
  tcp <addr> <bytes> <hz> <secs>     paced TCP sender
  udpsink <addr> <secs>              count bytes arriving on a UDP socket
  udpsend <addr> <bytes> <hz> <secs> paced UDP sender";

/// telemouse's default batch window (`[batch] window_ms`, `telemouse_core`'s
/// config default): `udp` replays one batch per window, as the capture agent
/// sends them. A copy, because tmbench is not a workspace member and does
/// not depend on telemouse-core.
const DEFAULT_WINDOW_MS: u64 = 25;

/// Why a run stopped: a command line that does not say what to do (printed
/// with the usage), or a failure while doing it (printed alone).
enum Error {
    Usage(String),
    Run(String),
}

/// Turn any displayable failure into an [`Error::Run`] naming what was tried.
fn run_err(what: impl Display) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::Run(format!("{what}: {e}"))
}

/// The arguments after the subcommand, read by position with a name for the
/// error message.
struct Args<'a> {
    cmd: &'a str,
    rest: &'a [String],
}

impl<'a> Args<'a> {
    fn str(&self, i: usize, name: &str) -> Result<&'a str, Error> {
        self.rest
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| Error::Usage(format!("{}: missing <{name}>", self.cmd)))
    }

    fn num<T>(&self, i: usize, name: &str) -> Result<T, Error>
    where
        T: FromStr,
        T::Err: Display,
    {
        let s = self.str(i, name)?;
        s.parse()
            .map_err(|e| Error::Usage(format!("{}: <{name}> {s:?} is not a number: {e}", self.cmd)))
    }

    /// A number that something divides by or counts with, so zero is refused.
    fn positive(&self, i: usize, name: &str) -> Result<u64, Error> {
        match self.num::<u64>(i, name)? {
            0 => Err(Error::Usage(format!(
                "{}: <{name}> must be at least 1",
                self.cmd
            ))),
            n => Ok(n),
        }
    }

    /// Refuse anything past the `n` arguments this subcommand takes.
    fn done(&self, n: usize) -> Result<(), Error> {
        match self.rest.get(n) {
            Some(extra) => Err(Error::Usage(format!(
                "{}: unexpected argument {extra:?}",
                self.cmd
            ))),
            None => Ok(()),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => {}
        Err(Error::Usage(e)) => {
            eprintln!("tmbench: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
        Err(Error::Run(e)) => {
            eprintln!("tmbench: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &[String]) -> Result<(), Error> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err(Error::Usage("no subcommand".into()));
    };
    let a = Args { cmd, rest };
    match cmd.as_str() {
        "inject" => {
            let (hz, secs) = (a.positive(0, "hz")?, a.num(1, "secs")?);
            a.done(2)?;
            inject(hz, secs);
            Ok(())
        }
        "ws" => {
            let (url, secs) = (a.str(0, "url")?, a.num(1, "secs")?);
            a.done(2)?;
            ws(url, secs)
        }
        "http" => {
            let (url, secs, interval) = (
                a.str(0, "url")?,
                a.num(1, "secs")?,
                a.num(2, "interval_ms")?,
            );
            a.done(3)?;
            http(url, secs, interval)
        }
        "udp" => {
            let (addr, file, secs) = (a.str(0, "addr")?, a.str(1, "file")?, a.num(2, "secs")?);
            a.done(3)?;
            udp(addr, file, secs)
        }
        "measure" => {
            let secs = a.num(0, "secs")?;
            let targets = rest[1..]
                .iter()
                .map(|t| {
                    let (label, pid) = t.split_once('=').ok_or_else(|| {
                        Error::Usage(format!("measure: {t:?} is not <label=pid>"))
                    })?;
                    let pid = pid
                        .parse::<u32>()
                        .map_err(|e| Error::Usage(format!("measure: pid {pid:?} in {t:?}: {e}")))?;
                    Ok((label.to_string(), pid))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            if targets.is_empty() {
                return Err(Error::Usage(
                    "measure: give at least one <label=pid>".into(),
                ));
            }
            measure(secs, &targets);
            Ok(())
        }
        "tcpsink" => {
            let (addr, secs) = (a.str(0, "addr")?, a.num(1, "secs")?);
            a.done(2)?;
            tcpsink(addr, secs)
        }
        "tcp" => {
            let (addr, bytes, hz, secs) = (
                a.str(0, "addr")?,
                a.num(1, "bytes")?,
                a.positive(2, "hz")?,
                a.positive(3, "secs")?,
            );
            a.done(4)?;
            tcp(addr, bytes, hz, secs)
        }
        "udpsink" => {
            let (addr, secs) = (a.str(0, "addr")?, a.num(1, "secs")?);
            a.done(2)?;
            udpsink(addr, secs)
        }
        "udpsend" => {
            let (addr, bytes, hz, secs) = (
                a.str(0, "addr")?,
                a.num(1, "bytes")?,
                a.positive(2, "hz")?,
                a.positive(3, "secs")?,
            );
            a.done(4)?;
            udpsend(addr, bytes, hz, secs)
        }
        other => Err(Error::Usage(format!("unknown subcommand {other:?}"))),
    }
}

/// The one thing in the telemouse tree that puts input *into* the system:
/// `SendInput` motion (and a click every 1000 reports) to load the capture
/// agent on an idle desk. As far as any game or its anti-cheat is concerned
/// that is input automation, so it refuses to run unless the operator says
/// so explicitly — bench.ps1 sets the variable for its own load phase — and
/// it must never run with a game open (docs/ANTICHEAT-2026-09-14.md, F2).
///
/// `hz` is at least 1 (the argument parser refuses 0).
fn inject(hz: u64, secs: u64) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
        MOUSEINPUT, SendInput,
    };
    if std::env::var("TMBENCH_ALLOW_INJECT").as_deref() != Ok("1") {
        eprintln!(
            "inject: refusing to synthesize input. This subcommand drives SendInput \
             (relative mouse motion plus a click every 1000 reports) into the whole \
             desktop, which is input automation to any game that is running. Close \
             every game, then set TMBENCH_ALLOW_INJECT=1 to run it."
        );
        std::process::exit(2);
    }
    eprintln!(
        "inject: synthesizing {hz} Hz relative mouse motion (a click every 1000 reports) for {secs} s via SendInput"
    );
    let period = Duration::from_nanos(1_000_000_000 / hz);
    let total = hz * secs;
    let start = Instant::now();
    let mut sent = 0u64;
    let mut late = 0u64;
    let mut i: u64 = 0;
    while i < total {
        let due = start + period * (i as u32);
        loop {
            let now = Instant::now();
            if now >= due {
                if now - due > Duration::from_millis(2) {
                    late += 1;
                }
                break;
            }
            let left = due - now;
            if left > Duration::from_micros(1500) {
                std::thread::sleep(left - Duration::from_micros(1000));
            } else {
                std::hint::spin_loop();
            }
        }
        let sign = if i.is_multiple_of(2) { 3 } else { -3 };
        let mut flags = MOUSEEVENTF_MOVE;
        if i.is_multiple_of(1000) {
            flags |= MOUSEEVENTF_LEFTDOWN;
        } else if i % 1000 == 500 {
            flags |= MOUSEEVENTF_LEFTUP;
        }
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: sign,
                    dy: -sign,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: `input` is a fully initialised INPUT_MOUSE record that
        // lives for the call, and `cbSize` is the size of that same type.
        let n = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        sent += n as u64;
        i += 1;
    }
    let el = start.elapsed().as_secs_f64();
    println!(
        "inject: sent={sent} secs={el:.2} rate={:.1}/s late={late}",
        sent as f64 / el
    );
}

/// Whether `line` is a telemouse envelope of `kind`: a `"type"` key whose
/// value is the string `kind`, with any whitespace around the colon — so a
/// pretty-printed or reformatted line matches as well as the agent's own.
fn is_envelope(line: &str, kind: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find("\"type\"") {
        rest = &rest[at + "\"type\"".len()..];
        let Some(after_colon) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let value = after_colon.trim_start();
        if let Some(v) = value.strip_prefix('"')
            && let Some(v) = v.strip_prefix(kind)
            && v.starts_with('"')
        {
            return true;
        }
    }
    false
}

fn ws(url: &str, secs: u64) -> Result<(), Error> {
    let (mut sock, _) =
        tungstenite::connect(url).map_err(|e| Error::Run(format!("ws connect {url}: {e}")))?;
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = sock.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(run_err("ws read timeout"))?;
    }
    let start = Instant::now();
    let (mut frames, mut bytes, mut batches, mut events) = (0u64, 0u64, 0u64, 0u64);
    while start.elapsed() < Duration::from_secs(secs) {
        match sock.read() {
            Ok(tungstenite::Message::Text(t)) => {
                frames += 1;
                bytes += t.len() as u64;
                if is_envelope(&t, "batch") {
                    batches += 1;
                    events += t.matches("\"ts_qpc\"").count() as u64;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("ws error: {e}");
                break;
            }
        }
    }
    let el = start.elapsed().as_secs_f64();
    println!(
        "ws: frames={frames} batches={batches} events={events} bytes={bytes} secs={el:.2} events_per_s={:.0}",
        events as f64 / el
    );
    let _ = sock.close(None);
    Ok(())
}

/// Poll `url` (http://host:port/path) every `interval_ms` with a fresh
/// connection, the way a browser page's fetch loop does.
fn http(url: &str, secs: u64, interval_ms: u64) -> Result<(), Error> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::Usage(format!("http: {url:?} is not an http:// url")))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let start = Instant::now();
    let (mut requests, mut bytes, mut errors) = (0u64, 0u64, 0u64);
    let mut max_ms = 0f64;
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(secs) {
        let t0 = Instant::now();
        match std::net::TcpStream::connect(hostport) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let req =
                    format!("GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n");
                if s.write_all(req.as_bytes()).is_ok() {
                    let mut buf = Vec::new();
                    match s.read_to_end(&mut buf) {
                        Ok(_) => {
                            requests += 1;
                            bytes += buf.len() as u64;
                        }
                        Err(_) => errors += 1,
                    }
                } else {
                    errors += 1;
                }
            }
            Err(_) => errors += 1,
        }
        max_ms = max_ms.max(t0.elapsed().as_secs_f64() * 1e3);
        n += 1;
        let due = start + Duration::from_millis(interval_ms) * n;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
    println!(
        "http: requests={requests} errors={errors} bytes={bytes} max_ms={max_ms:.1} secs={:.2}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// measure <secs> <label=pid>...  — cycle-exact per-process and per-thread CPU
/// over a window, using QueryProcessCycleTime / QueryThreadCycleTime (not the
/// tick-sampled GetThreadTimes). Prints one line per process and per thread
/// that consumed anything, as ms of CPU and % of one core, with the thread's
/// description (Rust thread name) when the OS has one.
///
/// Handles are opened with the `*_LIMITED_INFORMATION` rights only, which is
/// all the cycle counters, `GetThreadTimes` and `GetThreadDescription` need.
fn measure(secs: u64, targets: &[(String, u32)]) {
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetThreadDescription, GetThreadTimes, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::WindowsProgramming::QueryProcessCycleTime;
    use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;

    let cps = calibrate_cps();

    fn ft(f: FILETIME) -> f64 {
        (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64 / 1e7
    }
    struct T {
        tid: u32,
        pid: u32,
        h: HANDLE,
        cyc: u64,
        k: f64,
        u: f64,
        name: String,
    }
    fn threads_of(pids: &[u32]) -> Vec<T> {
        let mut out = Vec::new();
        // SAFETY: plain Win32 calls with no pointer arguments.
        let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
            return out;
        };
        let mut e = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: `snap` is the live snapshot handle opened above and `e` a
        // THREADENTRY32 whose `dwSize` is set, as Thread32First requires.
        let mut more = unsafe { Thread32First(snap, &mut e) }.is_ok();
        while more {
            if pids.contains(&e.th32OwnerProcessID) {
                // SAFETY: no pointer arguments; a failure is an `Err`.
                let opened =
                    unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, e.th32ThreadID) };
                if let Ok(h) = opened {
                    let mut cyc = 0u64;
                    let (mut a, mut b, mut k, mut u) = (
                        FILETIME::default(),
                        FILETIME::default(),
                        FILETIME::default(),
                        FILETIME::default(),
                    );
                    // SAFETY: `h` is the thread handle just opened with the
                    // query right these three need; every out-pointer is a
                    // live local of the right type. The description is a
                    // LocalAlloc'd string, copied and freed exactly once.
                    let name = unsafe {
                        let _ = QueryThreadCycleTime(h, &mut cyc);
                        let _ = GetThreadTimes(h, &mut a, &mut b, &mut k, &mut u);
                        match GetThreadDescription(h) {
                            Ok(p) if !p.is_null() => {
                                let s = p.to_string().unwrap_or_default();
                                let _ = LocalFree(Some(HLOCAL(p.0 as *mut _)));
                                s
                            }
                            _ => String::new(),
                        }
                    };
                    out.push(T {
                        tid: e.th32ThreadID,
                        pid: e.th32OwnerProcessID,
                        h,
                        cyc,
                        k: ft(k),
                        u: ft(u),
                        name,
                    });
                }
            }
            // SAFETY: as for Thread32First.
            more = unsafe { Thread32Next(snap, &mut e) }.is_ok();
        }
        // SAFETY: `snap` is open and closed exactly once, here.
        let _ = unsafe { CloseHandle(snap) };
        out
    }
    let pids: Vec<u32> = targets.iter().map(|t| t.1).collect();
    let procs: Vec<Option<HANDLE>> = pids
        .iter()
        // SAFETY: no pointer arguments; a failure is an `Err`.
        .map(|&p| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p).ok() })
        .collect();
    let cyc = |h: Option<HANDLE>| -> u64 {
        match h {
            Some(h) => {
                let mut c = 0;
                // SAFETY: `h` is a process handle opened above with the
                // query right QueryProcessCycleTime needs, and `c` a live u64.
                let _ = unsafe { QueryProcessCycleTime(h, &mut c) };
                c
            }
            None => 0,
        }
    };
    let pc0: Vec<u64> = procs.iter().map(|&h| cyc(h)).collect();
    let th0 = threads_of(&pids);
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(secs));
    let wall = start.elapsed().as_secs_f64();
    let pc1: Vec<u64> = procs.iter().map(|&h| cyc(h)).collect();
    let th1 = threads_of(&pids);
    println!("measure: wall={wall:.2}s cycles_per_s={:.3e}", cps);
    for (i, (label, pid)) in targets.iter().enumerate() {
        if procs[i].is_none() {
            println!("proc {label} pid={pid} cpu_ms=nan pct=nan (could not open)");
            continue;
        }
        let d = pc1[i].saturating_sub(pc0[i]) as f64 / cps;
        println!(
            "proc {label} pid={pid} cpu_ms={:.1} pct={:.3}",
            d * 1e3,
            100.0 * d / wall
        );
        for t in th1.iter().filter(|t| t.pid == *pid) {
            let (c0, k0, u0) = th0
                .iter()
                .find(|x| x.tid == t.tid)
                .map(|x| (x.cyc, x.k, x.u))
                .unwrap_or((0, 0.0, 0.0));
            let dc = t.cyc.saturating_sub(c0) as f64 / cps;
            if dc * 1e3 >= 0.5 {
                println!(
                    "  thread {label} tid={} name={} cpu_ms={:.1} pct={:.3} kernel_ms~{:.0} user_ms~{:.0}",
                    t.tid,
                    if t.name.is_empty() { "-" } else { &t.name },
                    dc * 1e3,
                    100.0 * dc / wall,
                    (t.k - k0) * 1e3,
                    (t.u - u0) * 1e3
                );
            }
        }
    }
    for h in th0
        .iter()
        .chain(th1.iter())
        .map(|t| t.h)
        .chain(procs.into_iter().flatten())
    {
        // SAFETY: every handle here was opened above and is closed once.
        let _ = unsafe { CloseHandle(h) };
    }
}

/// Raw CPU cycles the calling thread has consumed so far (not seconds:
/// divide by [`calibrate_cps`] for that).
fn thread_cycles() -> u64 {
    use windows::Win32::System::Threading::GetCurrentThread;
    use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut c = 0u64;
    // SAFETY: GetCurrentThread is a pseudo-handle that is always valid for
    // the calling thread, and `c` is a live u64.
    let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}

/// This thread's cycles per second, from a 300 ms spin.
fn calibrate_cps() -> f64 {
    let c0 = thread_cycles();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        std::hint::spin_loop();
    }
    (thread_cycles() - c0) as f64 / t0.elapsed().as_secs_f64()
}

/// Paced sender loop shared by the tcp/udp micro-benchmarks: `hz` sends of
/// `bytes` for `secs`, reporting this thread's cycle cost per send. `hz` and
/// `secs` are at least 1 (the argument parser refuses 0).
fn paced(label: &str, bytes: usize, hz: u64, secs: u64, mut send: impl FnMut(&[u8]) -> bool) {
    let cps = calibrate_cps();
    let payload = vec![b'x'; bytes];
    let period = Duration::from_nanos(1_000_000_000 / hz);
    let start = Instant::now();
    let c0 = thread_cycles();
    let (mut sent, mut failed) = (0u64, 0u64);
    let total = hz * secs;
    for i in 0..total {
        let due = start + period * (i as u32);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        if send(&payload) {
            sent += 1
        } else {
            failed += 1
        }
    }
    let cyc = (thread_cycles() - c0) as f64 / cps;
    // The sleeps themselves cost a wake each; report that floor separately.
    let start2 = Instant::now();
    let c1 = thread_cycles();
    for i in 0..total {
        let due = start2 + period * (i as u32);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
    let idle = (thread_cycles() - c1) as f64 / cps;
    println!(
        "{label}: sent={sent} failed={failed} bytes={bytes} hz={hz} cpu_us_per_send={:.1} (sleep-only floor {:.1} us/iter) total_pct={:.3}",
        cyc * 1e6 / sent.max(1) as f64,
        idle * 1e6 / total.max(1) as f64,
        100.0 * cyc / start.elapsed().as_secs_f64()
    );
}

fn tcpsink(addr: &str, secs: u64) -> Result<(), Error> {
    let l = std::net::TcpListener::bind(addr).map_err(run_err(format!("tcpsink bind {addr}")))?;
    let (mut s, _) = l.accept().map_err(run_err("tcpsink accept"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let start = Instant::now();
    let mut buf = vec![0u8; 65536];
    let mut total = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n as u64,
            Err(_) => {}
        }
    }
    println!("tcpsink: bytes={total}");
    Ok(())
}

fn tcp(addr: &str, bytes: usize, hz: u64, secs: u64) -> Result<(), Error> {
    let mut s =
        std::net::TcpStream::connect(addr).map_err(run_err(format!("tcp connect {addr}")))?;
    s.set_nodelay(true).map_err(run_err("tcp nodelay"))?;
    paced("tcp", bytes, hz, secs, |p| s.write_all(p).is_ok());
    Ok(())
}

fn udpsink(addr: &str, secs: u64) -> Result<(), Error> {
    let s = std::net::UdpSocket::bind(addr).map_err(run_err(format!("udpsink bind {addr}")))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let start = Instant::now();
    let mut buf = vec![0u8; 65536];
    let mut total = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        if let Ok(n) = s.recv(&mut buf) {
            total += n as u64;
        }
    }
    println!("udpsink: bytes={total}");
    Ok(())
}

fn udp_socket(addr: &str) -> Result<std::net::UdpSocket, Error> {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").map_err(run_err("udp bind"))?;
    s.connect(addr)
        .map_err(run_err(format!("udp connect {addr}")))?;
    Ok(s)
}

fn udpsend(addr: &str, bytes: usize, hz: u64, secs: u64) -> Result<(), Error> {
    let s = udp_socket(addr)?;
    paced("udp", bytes, hz, secs, |p| s.send(p).is_ok());
    Ok(())
}

/// Replay `file`'s batches to `addr`, one per [`DEFAULT_WINDOW_MS`], after
/// its session line, looping over the batches for `secs`.
fn udp(addr: &str, file: &str, secs: u64) -> Result<(), Error> {
    use std::io::BufRead;
    let f = std::io::BufReader::new(
        std::fs::File::open(file).map_err(run_err(format!("open {file}")))?,
    );
    let lines: Vec<String> = f.lines().map_while(Result::ok).collect();
    // Larger lines would not fit one datagram.
    let batches: Vec<&String> = lines
        .iter()
        .filter(|l| is_envelope(l, "batch") && l.len() < 60_000)
        .collect();
    if batches.is_empty() {
        return Err(Error::Run(format!(
            "udp: {file} has no batch line under 60000 bytes to replay"
        )));
    }
    let sock = udp_socket(addr)?;
    if let Some(s) = lines.iter().find(|l| is_envelope(l, "session")) {
        sock.send(s.as_bytes())
            .map_err(run_err("udp send session"))?;
    }
    let start = Instant::now();
    let mut sent = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let due = start + Duration::from_millis(DEFAULT_WINDOW_MS * (sent + 1));
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let _ = sock.send(batches[sent as usize % batches.len()].as_bytes());
        sent += 1;
    }
    println!("udp: sent={sent} secs={:.2}", start.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn usage_error(args: &[&str]) -> String {
        match run(&v(args)) {
            Err(Error::Usage(e)) => e,
            Err(Error::Run(e)) => panic!("{args:?}: expected a usage error, got {e}"),
            Ok(()) => panic!("{args:?}: expected a usage error"),
        }
    }

    #[test]
    fn bad_command_lines_are_usage_errors_not_panics() {
        assert!(usage_error(&[]).contains("no subcommand"));
        assert!(usage_error(&["nope"]).contains("unknown"));
        assert!(usage_error(&["ws"]).contains("<url>"));
        assert!(usage_error(&["ws", "ws://x", "ten"]).contains("not a number"));
        assert!(usage_error(&["inject", "0", "1"]).contains("at least 1"));
        assert!(usage_error(&["udpsend", "127.0.0.1:1", "8", "0", "1"]).contains("at least 1"));
        assert!(usage_error(&["tcp", "127.0.0.1:1", "8", "10", "0"]).contains("at least 1"));
        assert!(usage_error(&["measure", "1"]).contains("label=pid"));
        assert!(usage_error(&["measure", "1", "viz"]).contains("label=pid"));
        assert!(usage_error(&["measure", "1", "viz=x"]).contains("pid"));
        assert!(usage_error(&["tcpsink", "a", "1", "extra"]).contains("unexpected"));
        assert!(usage_error(&["http", "ftp://x", "1", "1"]).contains("http://"));
    }

    #[test]
    fn a_recording_without_batches_is_refused() {
        let dir = std::env::temp_dir().join(format!("tmbench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("empty.jsonl");
        std::fs::write(&file, "{\"type\":\"session\"}\n").unwrap();
        match udp("127.0.0.1:9", file.to_str().unwrap(), 1) {
            Err(Error::Run(e)) => assert!(e.contains("no batch line"), "{e}"),
            _ => panic!("expected a run error"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn envelopes_are_recognised_by_their_type_key() {
        assert!(is_envelope(r#"{"type":"batch","seq_no":1}"#, "batch"));
        assert!(is_envelope(r#"{ "type" : "batch" }"#, "batch"));
        assert!(!is_envelope(r#"{"type":"batches"}"#, "batch"));
        assert!(!is_envelope(r#"{"type":"session"}"#, "batch"));
        assert!(!is_envelope(r#"{"label":"type batch"}"#, "batch"));
        assert!(is_envelope(
            r#"{"x":{"type":1},"type":"session"}"#,
            "session"
        ));
    }
}
