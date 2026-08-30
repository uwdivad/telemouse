//! tmbench: load generator + cycle-exact CPU meter for telemouse benchmarks.
//!
//!   tmbench inject <hz> <secs>              inject relative mouse motion via SendInput
//!   tmbench ws <url> <secs>                 drain a viz WebSocket, report frames/bytes
//!   tmbench http <url> <secs> <interval_ms> GET a URL every interval (stands in for the ctl page poll)
//!   tmbench measure <secs> <label=pid>...   QueryProcessCycleTime / QueryThreadCycleTime per target
//!   tmbench udp <addr> <file> <secs>        replay a recording's batches over UDP at 40/s
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("inject") => inject(args[2].parse().unwrap(), args[3].parse().unwrap()),
        Some("ws") => ws(&args[2], args[3].parse().unwrap()),
        Some("http") => http(&args[2], args[3].parse().unwrap(), args[4].parse().unwrap()),
        Some("udp") => udp(&args[2], &args[3], args[4].parse().unwrap()),
        Some("measure") => measure(&args[2..]),
        Some("tcpsink") => tcpsink(&args[2], args[3].parse().unwrap()),
        Some("tcp") => tcp(&args[2], args[3].parse().unwrap(), args[4].parse().unwrap(), args[5].parse().unwrap()),
        Some("udpsink") => udpsink(&args[2], args[3].parse().unwrap()),
        Some("udpsend") => udpsend(&args[2], args[3].parse().unwrap(), args[4].parse().unwrap(), args[5].parse().unwrap()),
        _ => eprintln!("usage: tmbench inject <hz> <secs> | ws <url> <secs> | http <url> <secs> <interval_ms> | measure <secs> <label=pid>... | udp <addr> <file> <secs>"),
    }
}

fn inject(hz: u64, secs: u64) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
        MOUSEINPUT, SendInput,
    };
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
        let sign = if i % 2 == 0 { 3 } else { -3 };
        let mut flags = MOUSEEVENTF_MOVE;
        if i % 1000 == 0 {
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

fn ws(url: &str, secs: u64) {
    let (mut sock, _) = tungstenite::connect(url).expect("ws connect");
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = sock.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    }
    let start = Instant::now();
    let (mut frames, mut bytes, mut batches, mut events) = (0u64, 0u64, 0u64, 0u64);
    while start.elapsed() < Duration::from_secs(secs) {
        match sock.read() {
            Ok(tungstenite::Message::Text(t)) => {
                frames += 1;
                bytes += t.len() as u64;
                if t.contains("\"type\":\"batch\"") {
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
}

/// Poll `url` (http://host:port/path) every `interval_ms` with a fresh
/// connection, the way a browser page's fetch loop does.
fn http(url: &str, secs: u64, interval_ms: u64) {
    let rest = url.strip_prefix("http://").expect("http:// url");
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
                let req = format!("GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n");
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
}

/// measure <secs> <label=pid>...  — cycle-exact per-process and per-thread CPU
/// over a window, using QueryProcessCycleTime / QueryThreadCycleTime (not the
/// tick-sampled GetThreadTimes). Prints one line per process and per thread
/// that consumed anything, as ms of CPU and % of one core, with the thread's
/// description (Rust thread name) when the OS has one.
fn measure(args: &[String]) {
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LocalFree, HLOCAL};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetCurrentThread, GetThreadDescription, GetThreadTimes, OpenProcess, OpenThread,
        PROCESS_QUERY_INFORMATION, THREAD_QUERY_INFORMATION,
    };
    use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};
    let secs: u64 = args[0].parse().unwrap();
    let targets: Vec<(String, u32)> = args[1..]
        .iter()
        .map(|a| {
            let (l, p) = a.split_once('=').unwrap();
            (l.to_string(), p.parse().unwrap())
        })
        .collect();

    // Calibrate cycles/s on this thread: spin for 300ms.
    let cps = unsafe {
        let mut c0 = 0u64;
        QueryThreadCycleTime(GetCurrentThread(), &mut c0).unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(300) {
            std::hint::spin_loop();
        }
        let mut c1 = 0u64;
        QueryThreadCycleTime(GetCurrentThread(), &mut c1).unwrap();
        (c1 - c0) as f64 / t0.elapsed().as_secs_f64()
    };

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
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).unwrap();
            let mut e = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            if Thread32First(snap, &mut e).is_ok() {
                loop {
                    if pids.contains(&e.th32OwnerProcessID) {
                        if let Ok(h) = OpenThread(THREAD_QUERY_INFORMATION, false, e.th32ThreadID) {
                            let mut cyc = 0u64;
                            let _ = QueryThreadCycleTime(h, &mut cyc);
                            let (mut a, mut b, mut k, mut u) = (
                                FILETIME::default(),
                                FILETIME::default(),
                                FILETIME::default(),
                                FILETIME::default(),
                            );
                            let _ = GetThreadTimes(h, &mut a, &mut b, &mut k, &mut u);
                            let name = match GetThreadDescription(h) {
                                Ok(p) if !p.is_null() => {
                                    let s = p.to_string().unwrap_or_default();
                                    let _ = LocalFree(Some(HLOCAL(p.0 as *mut _)));
                                    s
                                }
                                _ => String::new(),
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
                    if Thread32Next(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }
    let pids: Vec<u32> = targets.iter().map(|t| t.1).collect();
    let procs: Vec<Option<HANDLE>> = pids
        .iter()
        .map(|&p| unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, p).ok() })
        .collect();
    let cyc = |h: Option<HANDLE>| -> u64 {
        match h {
            Some(h) => {
                let mut c = 0;
                unsafe { QueryProcessCycleTime(h, &mut c).ok() };
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
        let d = (pc1[i] - pc0[i]) as f64 / cps;
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
    unsafe {
        for t in th0.iter().chain(th1.iter()) {
            let _ = CloseHandle(t.h);
        }
        for h in procs.into_iter().flatten() {
            let _ = CloseHandle(h);
        }
    }
}

/// Cycles consumed by the calling thread so far, as seconds at the calibrated rate.
fn thread_cycles() -> u64 {
    use windows::Win32::System::Threading::GetCurrentThread;
    use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c).unwrap() };
    c
}

fn calibrate_cps() -> f64 {
    let c0 = thread_cycles();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        std::hint::spin_loop();
    }
    (thread_cycles() - c0) as f64 / t0.elapsed().as_secs_f64()
}

/// Paced sender loop shared by the tcp/udp micro-benchmarks: `hz` sends of
/// `bytes` for `secs`, reporting this thread's cycle cost per send.
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
        if send(&payload) { sent += 1 } else { failed += 1 }
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
        idle * 1e6 / total as f64,
        100.0 * cyc / start.elapsed().as_secs_f64()
    );
}

fn tcpsink(addr: &str, secs: u64) {
    let l = std::net::TcpListener::bind(addr).unwrap();
    let (mut s, _) = l.accept().unwrap();
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
}

fn tcp(addr: &str, bytes: usize, hz: u64, secs: u64) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    paced("tcp", bytes, hz, secs, |p| s.write_all(p).is_ok());
}

fn udpsink(addr: &str, secs: u64) {
    let s = std::net::UdpSocket::bind(addr).unwrap();
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
}

fn udpsend(addr: &str, bytes: usize, hz: u64, secs: u64) {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.connect(addr).unwrap();
    paced("udp", bytes, hz, secs, |p| s.send(p).is_ok());
}

fn udp(addr: &str, file: &str, secs: u64) {
    use std::io::BufRead;
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.connect(addr).unwrap();
    let f = std::io::BufReader::new(std::fs::File::open(file).unwrap());
    let lines: Vec<String> = f.lines().map_while(Result::ok).collect();
    if let Some(s) = lines.iter().find(|l| l.contains("\"type\":\"session\"")) {
        sock.send(s.as_bytes()).unwrap();
    }
    let batches: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("\"type\":\"batch\"") && l.len() < 60_000)
        .collect();
    let start = Instant::now();
    let mut i = 0usize;
    let mut sent = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let due = start + Duration::from_millis(25 * (sent as u32 + 1) as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let _ = sock.send(batches[i % batches.len()].as_bytes());
        i += 1;
        sent += 1;
    }
    println!("udp: sent={sent} secs={:.2}", start.elapsed().as_secs_f64());
}
