//! `telemouse doctor` — what the agent can see about this machine, as rows
//! either a person or a program can read.
//!
//! Looking and describing are two different jobs, and only the second one can
//! be tested on a machine with no mouse and no screen. So the Win32 half stays
//! in [`crate::platform`] (plus the sinks and one TCP probe) and lands here as
//! [`Facts`] — a plain struct of everything that was observed — and everything
//! below that is pure: [`Facts::into_report`] turns the facts into [`Check`]
//! rows, [`Report::render_text`] draws those rows for a human, and serde draws
//! the very same rows for a program (`doctor --json`). Neither renderer knows
//! anything the other does not, so the two can no longer drift.
//!
//! `--json` prints exactly one JSON document and nothing else on stdout; the
//! log — which every binary sends to stderr, never to stdout — stays where it
//! was, so `telemouse doctor --json | ConvertFrom-Json` works even from a
//! build that logs. Exit codes are the same in both modes: doctor exits `0`
//! whenever it produced a report, `fail` rows included, and non-zero only when
//! it could not get that far (a `telemouse.toml` that exists but does not
//! parse). A caller decides on [`Report::verdict`], not on the exit code.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use telemouse_core::config::AppConfig;
use telemouse_core::session::MonitorInfo;

/// Schema tag of the `--json` document. Bump it when a field — or the meaning
/// of one — changes; a consumer that does not know the tag should say so
/// rather than guess.
pub const SCHEMA: &str = "telemouse-doctor/1";

/// How long a broker gets to answer a TCP connect before it counts as
/// unreachable. Doctor is run in front of an impatient person.
const BROKER_TIMEOUT: Duration = Duration::from_millis(500);

/// How one row came out. Ordered worst-last, so the overall verdict is simply
/// the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nothing to do about this one.
    Pass,
    /// Works, but something will be missing or slower than it should be.
    Warn,
    /// This part of the pipeline will not work at all.
    Fail,
}

impl Status {
    /// The wire spelling, which is also the text mode's tag column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One thing doctor looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Stable across versions and independent of wording: what a script
    /// matches on. Lowercase, `_`-separated.
    pub id: String,
    pub status: Status,
    /// The row's name for a human, e.g. `Kafka broker 1`.
    pub title: String,
    /// What was found, in one line.
    pub detail: String,
    /// What to do about it, when there is something to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn new(id: &str, status: Status, title: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status,
            title: title.to_string(),
            detail: detail.into(),
            hint: None,
        }
    }

    fn pass(id: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(id, Status::Pass, title, detail)
    }

    fn warn(id: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(id, Status::Warn, title, detail)
    }

    fn fail(id: &str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(id, Status::Fail, title, detail)
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The `--json` document, and the thing the text mode is drawn from.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// The agent that produced it.
    pub capture_version: String,
    /// When, UTC µs — so a report pasted into a bug says how old it is.
    pub generated_utc_us: i64,
    /// The worst status in `checks`: `pass` only when every row passed.
    pub verdict: Status,
    pub checks: Vec<Check>,
    /// The configuration as the agent resolved it, every relative path made
    /// absolute — the same document the text mode prints under
    /// `--- resolved config ---`.
    pub config: serde_json::Value,
    /// That same config as TOML, for the text mode alone. Not part of the
    /// JSON document: a consumer that wants the config has `config`.
    #[serde(skip_serializing)]
    pub config_toml: String,
}

impl Report {
    /// The rows as a person reads them: one line each, the hint indented under
    /// its row, then the tally and the resolved config.
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "telemouse {} — doctor", self.capture_version);
        out.push('\n');
        // Wide enough for the longest title, but never so wide that a stray
        // long one pushes every detail off an 80-column console.
        let width = self
            .checks
            .iter()
            .map(|c| c.title.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(4, 24);
        for c in &self.checks {
            let _ = writeln!(
                out,
                "  {:<4}  {:<width$} : {}",
                c.status.as_str(),
                c.title,
                c.detail
            );
            if let Some(h) = &c.hint {
                let _ = writeln!(out, "        {:<width$}   hint: {h}", "");
            }
        }
        let (mut pass, mut warn, mut fail) = (0usize, 0usize, 0usize);
        for c in &self.checks {
            match c.status {
                Status::Pass => pass += 1,
                Status::Warn => warn += 1,
                Status::Fail => fail += 1,
            }
        }
        let _ = writeln!(
            out,
            "\nverdict: {} — {pass} ok, {warn} warning(s), {fail} failed",
            self.verdict.as_str()
        );
        out.push_str("\n--- resolved config ---\n");
        out.push_str(&self.config_toml);
        if !self.config_toml.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

/// How one sink answered when doctor asked whether it could start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkProbe {
    /// Turned off in `telemouse.toml`; nothing was tried.
    Disabled,
    /// Opened, and here is where it points.
    Ready(String),
    /// Could not be opened, and why.
    Failed(String),
}

/// One `[kafka] brokers` entry and whether a TCP connect reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    pub addr: String,
    pub reachable: bool,
}

/// Everything doctor observed, before any of it is judged.
///
/// Building one of these touches Win32, the filesystem and the network;
/// turning one into a [`Report`] touches nothing, which is why the tests below
/// can cover every row of a machine this crate is not running on.
#[derive(Debug, Clone)]
pub struct Facts {
    pub capture_version: String,
    pub generated_utc_us: i64,
    /// `debug` or `release` — a debug build drops events a release build
    /// would not, so it is worth a row of its own.
    pub profile: &'static str,
    /// Comma-separated Cargo features this binary was built with.
    pub features: String,
    /// Is this a Windows build? Capture needs one; doctor itself does not.
    pub windows: bool,
    /// `Windows 10.0.19045`, when the OS will say.
    pub os_version: Option<String>,
    pub config_path: String,
    pub config_found: bool,
    /// Settings that differ from the built-in defaults, `(field, value)`.
    pub non_default: Vec<(String, String)>,
    pub qpc_freq: u64,
    /// QPC ticks between two back-to-back reads: the clock's practical
    /// resolution, and proof that it moves at all.
    pub qpc_delta: u64,
    pub primary_screen: (u32, u32),
    pub monitors: Vec<MonitorInfo>,
    pub cursor: Option<(i32, i32)>,
    pub foreground: Option<String>,
    /// Pointing devices as raw input enumerates them.
    pub devices: Vec<String>,
    pub udp: SinkProbe,
    pub recording: SinkProbe,
    /// Was this binary built with the Kafka sink compiled in?
    pub kafka_built_in: bool,
    /// `[kafka] enabled`.
    pub kafka_enabled: bool,
    pub brokers: Vec<Broker>,
    pub config: serde_json::Value,
    pub config_toml: String,
}

impl Facts {
    /// Judge every fact. Pure: same facts in, same rows out, on any machine.
    pub fn into_report(self) -> Report {
        let mut checks: Vec<Check> = Vec::new();

        // Which telemouse this is. A debug build is the first thing a
        // surprising drop count should be checked against.
        let build = format!("{}, features [{}]", self.profile, self.features);
        checks.push(if self.profile == "debug" {
            Check::warn("build", "Build", build).hint(
                "a debug build drops events at rates a release build does not; \
                 use a release build to measure anything",
            )
        } else {
            Check::pass("build", "Build", build)
        });

        // The OS. Raw input is Win32; off Windows only doctor itself runs.
        checks.push(match (self.windows, &self.os_version) {
            (true, Some(v)) => Check::pass("os", "OS", v.clone()),
            (true, None) => Check::warn("os", "OS", "Windows, build number unknown"),
            (false, _) => Check::fail("os", "OS", "not Windows")
                .hint("raw input capture needs Windows; only `doctor` runs here"),
        });

        // Where the settings came from.
        checks.push(if self.config_found {
            Check::pass("config", "Config", format!("loaded {}", self.config_path))
        } else {
            Check::warn(
                "config",
                "Config",
                format!("{} not found; built-in defaults in use", self.config_path),
            )
            .hint(
                "the control panel writes one on first run, or copy \
                 telemouse.example.toml next to the executable",
            )
        });

        // What in it is not standard: a support question starts here.
        checks.push(Check::pass(
            "config_overrides",
            "Non-default",
            if self.non_default.is_empty() {
                "(none — every setting is at its default)".to_string()
            } else {
                format!(
                    "{}: {}",
                    self.non_default.len(),
                    self.non_default
                        .iter()
                        .map(|(f, v)| format!("{f}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        ));

        // Every timestamp in a recording is QPC ticks converted with this
        // frequency, so a clock that does not tick fast is not a detail.
        let clock = format!(
            "{} ticks/s ({:.3} MHz), {} ticks between two reads",
            self.qpc_freq,
            self.qpc_freq as f64 / 1e6,
            self.qpc_delta
        );
        checks.push(if self.qpc_freq >= 1_000_000 {
            Check::pass("clock", "Clock (QPC)", clock)
        } else {
            Check::warn("clock", "Clock (QPC)", clock)
                .hint("a performance counter below 1 MHz cannot time 1 kHz reports")
        });

        // Screen geometry goes into the session record; a consumer converts
        // counts to screen space with it.
        let (w, h) = self.primary_screen;
        let mons = self
            .monitors
            .iter()
            .map(|m| {
                format!(
                    "{}x{}{}{}",
                    m.width,
                    m.height,
                    m.refresh_hz.map(|r| format!("@{r}Hz")).unwrap_or_default(),
                    if m.primary { " (primary)" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let screen = format!(
            "{w}x{h} primary, {} monitor(s): {}",
            self.monitors.len(),
            if mons.is_empty() { "none" } else { &mons }
        );
        checks.push(if w > 0 && h > 0 {
            Check::pass("screen", "Screens", screen)
        } else {
            Check::fail("screen", "Screens", screen)
                .hint("no primary screen was reported; sessions would record 0x0")
        });

        checks.push(match self.cursor {
            Some((x, y)) => Check::pass("cursor", "Cursor", format!("at {x},{y}")),
            None => Check::warn("cursor", "Cursor", "unavailable")
                .hint("the cursor position could not be read; absolute frames will be missing"),
        });

        checks.push(match &self.foreground {
            Some(name) => Check::pass("foreground", "Foreground", name.clone()),
            None => Check::warn("foreground", "Foreground", "unknown").hint(
                "no foreground window was named, so batches will carry no program name \
                 and the games table cannot match",
            ),
        });

        checks.push(if self.devices.is_empty() {
            Check::warn("devices", "Pointing devices", "none enumerated")
                .hint("plug a mouse in; without one every event reports device_ix=0")
        } else {
            Check::pass(
                "devices",
                "Pointing devices",
                format!("{}: {}", self.devices.len(), self.devices.join("; ")),
            )
        });

        checks.push(match &self.udp {
            SinkProbe::Disabled => Check::pass("udp", "UDP sink", "disabled in telemouse.toml"),
            SinkProbe::Ready(addr) => Check::pass("udp", "UDP sink", format!("ready -> {addr}")),
            SinkProbe::Failed(e) => Check::fail("udp", "UDP sink", format!("unavailable ({e})"))
                .hint("the live dashboard and the OBS overlay are fed over this socket; check [udp] addr"),
        });

        checks.push(match &self.recording {
            SinkProbe::Disabled => {
                Check::pass("recording", "Recording", "disabled in telemouse.toml")
            }
            SinkProbe::Ready(dir) => {
                Check::pass("recording", "Recording", format!("ready -> {dir}"))
            }
            SinkProbe::Failed(e) => Check::fail("recording", "Recording", format!("unavailable ({e})"))
                .hint("nothing can be analysed later without a recording; point [recording] dir at a folder you own"),
        });

        // Kafka is the one sink that can be missing from the binary as well as
        // from the config, and the two say very different things.
        checks.push(match (self.kafka_built_in, self.kafka_enabled) {
            (true, true) => Check::pass(
                "kafka",
                "Kafka",
                format!("enabled, {} broker(s)", self.brokers.len()),
            ),
            (true, false) => Check::pass("kafka", "Kafka", "disabled in telemouse.toml"),
            (false, true) => Check::warn(
                "kafka",
                "Kafka",
                "[kafka] enabled = true, but this build has no Kafka support",
            )
            .hint("use a default-feature build, or set [kafka] enabled = false"),
            (false, false) => {
                Check::pass("kafka", "Kafka", "not built into this binary, not enabled")
            }
        });

        // Reachability is probed whatever the config says — a broker that is
        // up is worth knowing about before the switch is flipped — but it is
        // only a warning when this run would actually have shipped to it.
        let kafka_in_use = self.kafka_built_in && self.kafka_enabled;
        for (i, b) in self.brokers.iter().enumerate() {
            let title = format!("Kafka broker {}", i + 1);
            let id = format!("kafka_broker_{}", i + 1);
            checks.push(if b.reachable {
                Check::pass(&id, &title, format!("{} reachable", b.addr))
            } else if kafka_in_use {
                Check::warn(&id, &title, format!("{} unreachable", b.addr)).hint(
                    "batches queue and are dropped while the broker is away; \
                     start it or set [kafka] enabled = false",
                )
            } else {
                Check::pass(
                    &id,
                    &title,
                    format!("{} unreachable (Kafka is off, so nothing will try)", b.addr),
                )
            });
        }

        let verdict = checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Pass);
        Report {
            schema: SCHEMA,
            capture_version: self.capture_version,
            generated_utc_us: self.generated_utc_us,
            verdict,
            checks,
            config: self.config,
            config_toml: self.config_toml,
        }
    }
}

/// Look at the machine. This is the whole impure half of `doctor`: Win32
/// through [`crate::platform`], one UDP socket, one `create_dir_all`, and a
/// TCP connect per configured broker.
pub fn probe(
    config_path: &Path,
    cfg: &AppConfig,
    capture_version: &str,
    profile: &'static str,
    features: String,
) -> Facts {
    use crate::{devices, platform, sinks, stats};

    let qpc_freq = platform::qpc_freq();
    let a = platform::qpc();
    let b = platform::qpc();

    // UDP: only our own end can be proven. Nothing answers on the far side of
    // an unconnected datagram socket, so "ready" means the socket opened.
    let udp = if cfg.udp.enabled {
        match sinks::UdpSink::connect(&cfg.udp.addr, std::sync::Arc::new(stats::Stats::default())) {
            Ok(s) => SinkProbe::Ready(s.addr().to_string()),
            Err(e) => SinkProbe::Failed(format!("{e:#}")),
        }
    } else {
        SinkProbe::Disabled
    };

    let recording = if cfg.recording.enabled {
        match std::fs::create_dir_all(&cfg.recording.dir) {
            Ok(()) => SinkProbe::Ready(cfg.recording.dir.display().to_string()),
            Err(e) => SinkProbe::Failed(e.to_string()),
        }
    } else {
        SinkProbe::Disabled
    };

    let brokers = cfg
        .kafka
        .brokers
        .iter()
        .map(|addr| Broker {
            addr: addr.clone(),
            reachable: addr
                .to_socket_addrs()
                .ok()
                .and_then(|mut addrs| addrs.next())
                .map(|a| TcpStream::connect_timeout(&a, BROKER_TIMEOUT).is_ok())
                .unwrap_or(false),
        })
        .collect();

    Facts {
        capture_version: capture_version.to_string(),
        generated_utc_us: chrono::Utc::now().timestamp_micros(),
        profile,
        features,
        windows: cfg!(windows),
        os_version: platform::os_version(),
        config_path: config_path.display().to_string(),
        config_found: config_path.exists(),
        non_default: cfg
            .non_default_fields()
            .into_iter()
            .map(|(f, v)| (f.to_string(), v))
            .collect(),
        qpc_freq,
        qpc_delta: b.saturating_sub(a),
        primary_screen: platform::primary_screen(),
        monitors: platform::monitors(),
        cursor: platform::cursor_pos(),
        foreground: platform::foreground_process_name(),
        // device_ix 0 is reserved for "unknown", so these are the real ones.
        devices: devices::enumerate_mice()
            .into_iter()
            .map(|(_, name)| name)
            .collect(),
        udp,
        recording,
        kafka_built_in: cfg!(feature = "kafka"),
        kafka_enabled: cfg.kafka.enabled,
        brokers,
        config: serde_json::to_value(cfg).unwrap_or(serde_json::Value::Null),
        config_toml: toml::to_string_pretty(cfg)
            .unwrap_or_else(|e| format!("(could not render config: {e})\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with nothing wrong with it.
    fn healthy() -> Facts {
        Facts {
            capture_version: "9.9.9".into(),
            generated_utc_us: 1_700_000_000_000_000,
            profile: "release",
            features: "logging,observability,kafka".into(),
            windows: true,
            os_version: Some("Windows 10.0.19045".into()),
            config_path: r"C:\telemouse\telemouse.toml".into(),
            config_found: true,
            non_default: vec![("mouse_cpi".into(), "800".into())],
            qpc_freq: 10_000_000,
            qpc_delta: 3,
            primary_screen: (2560, 1440),
            monitors: vec![MonitorInfo {
                width: 2560,
                height: 1440,
                refresh_hz: Some(240),
                primary: true,
            }],
            cursor: Some((100, 200)),
            foreground: Some("explorer.exe".into()),
            devices: vec!["mouse-a".into(), "mouse-b".into()],
            udp: SinkProbe::Ready("127.0.0.1:7878".into()),
            recording: SinkProbe::Ready(r"C:\telemouse\recordings".into()),
            kafka_built_in: true,
            kafka_enabled: false,
            brokers: vec![Broker {
                addr: "192.168.1.9:9092".into(),
                reachable: false,
            }],
            config: serde_json::json!({ "mouse_cpi": 800.0 }),
            config_toml: "mouse_cpi = 800.0\n".into(),
        }
    }

    fn row<'a>(r: &'a Report, id: &str) -> &'a Check {
        r.checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no check {id}; have {:?}", ids(r)))
    }

    fn ids(r: &Report) -> Vec<&str> {
        r.checks.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn a_healthy_machine_passes_every_row() {
        let r = healthy().into_report();
        assert_eq!(r.verdict, Status::Pass);
        for c in &r.checks {
            assert_eq!(c.status, Status::Pass, "{}: {}", c.id, c.detail);
            assert!(!c.title.is_empty() && !c.detail.is_empty(), "{}", c.id);
        }
        assert_eq!(r.schema, "telemouse-doctor/1");
        assert_eq!(r.capture_version, "9.9.9");
    }

    /// The ids are the API: a script matches on them, so a rename is a schema
    /// change and this test is where it gets noticed.
    #[test]
    fn the_row_ids_are_stable_and_unique() {
        let r = healthy().into_report();
        assert_eq!(
            ids(&r),
            vec![
                "build",
                "os",
                "config",
                "config_overrides",
                "clock",
                "screen",
                "cursor",
                "foreground",
                "devices",
                "udp",
                "recording",
                "kafka",
                "kafka_broker_1",
            ]
        );
        let mut sorted = ids(&r);
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), r.checks.len(), "ids must be unique");
    }

    #[test]
    fn the_verdict_is_the_worst_row() {
        let mut f = healthy();
        f.cursor = None;
        let r = f.clone().into_report();
        assert_eq!(r.verdict, Status::Warn);
        assert_eq!(row(&r, "cursor").status, Status::Warn);
        assert!(row(&r, "cursor").hint.is_some());

        f.udp = SinkProbe::Failed("os error 10049".into());
        let r = f.into_report();
        assert_eq!(r.verdict, Status::Fail, "a fail outranks a warn");
        assert!(row(&r, "udp").detail.contains("10049"));
        assert!(row(&r, "udp").hint.is_some());
    }

    #[test]
    fn a_debug_build_is_a_warning_with_the_reason() {
        let mut f = healthy();
        f.profile = "debug";
        let r = f.into_report();
        assert_eq!(row(&r, "build").status, Status::Warn);
        assert!(row(&r, "build").hint.as_ref().unwrap().contains("release"));
        assert_eq!(r.verdict, Status::Warn);
    }

    #[test]
    fn a_missing_config_warns_and_says_where_one_comes_from() {
        let mut f = healthy();
        f.config_found = false;
        let r = f.into_report();
        let c = row(&r, "config");
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("telemouse.toml"));
        assert!(c.hint.as_ref().unwrap().contains("telemouse.example.toml"));
    }

    /// An unreachable broker matters only if this run would have shipped to
    /// it; doctor probes either way so the answer is there when the switch is
    /// flipped.
    #[test]
    fn an_unreachable_broker_is_a_warning_only_when_kafka_is_on() {
        let off = healthy().into_report();
        assert_eq!(row(&off, "kafka_broker_1").status, Status::Pass);
        assert!(row(&off, "kafka_broker_1").detail.contains("unreachable"));
        assert_eq!(row(&off, "kafka").status, Status::Pass);

        let mut f = healthy();
        f.kafka_enabled = true;
        let on = f.clone().into_report();
        assert_eq!(row(&on, "kafka_broker_1").status, Status::Warn);
        assert!(row(&on, "kafka").detail.contains("1 broker"));

        f.kafka_built_in = false;
        let missing = f.into_report();
        assert_eq!(row(&missing, "kafka").status, Status::Warn);
        // The build cannot ship to it, so the broker is not this run's problem.
        assert_eq!(row(&missing, "kafka_broker_1").status, Status::Pass);
    }

    #[test]
    fn a_second_broker_gets_its_own_numbered_row() {
        let mut f = healthy();
        f.kafka_enabled = true;
        f.brokers.push(Broker {
            addr: "10.0.0.2:9092".into(),
            reachable: true,
        });
        let r = f.into_report();
        assert_eq!(row(&r, "kafka_broker_2").title, "Kafka broker 2");
        assert_eq!(row(&r, "kafka_broker_2").status, Status::Pass);
        assert!(row(&r, "kafka_broker_2").detail.contains("10.0.0.2:9092"));
    }

    #[test]
    fn a_machine_without_windows_or_a_screen_fails_those_rows() {
        let mut f = healthy();
        f.windows = false;
        f.os_version = None;
        f.primary_screen = (0, 0);
        f.monitors.clear();
        f.devices.clear();
        let r = f.into_report();
        assert_eq!(row(&r, "os").status, Status::Fail);
        assert_eq!(row(&r, "screen").status, Status::Fail);
        assert_eq!(row(&r, "devices").status, Status::Warn);
        assert_eq!(r.verdict, Status::Fail);
    }

    #[test]
    fn a_slow_performance_counter_is_flagged() {
        let mut f = healthy();
        f.qpc_freq = 1_000;
        let r = f.into_report();
        assert_eq!(row(&r, "clock").status, Status::Warn);
        assert!(row(&r, "clock").detail.contains("1000 ticks/s"));
    }

    /// The JSON document is the contract: a versioned tag, a verdict, and a
    /// row per check with the five documented fields.
    #[test]
    fn the_json_document_has_the_shape_docs_api_promises() {
        let mut f = healthy();
        f.cursor = None;
        let r = f.into_report();
        let v: serde_json::Value = serde_json::from_str(&r.to_json_pretty().unwrap()).unwrap();

        assert_eq!(v["schema"], "telemouse-doctor/1");
        assert_eq!(v["capture_version"], "9.9.9");
        assert_eq!(v["generated_utc_us"], 1_700_000_000_000_000i64);
        assert_eq!(v["verdict"], "warn");
        assert_eq!(v["config"]["mouse_cpi"], 800.0);
        // The TOML rendering is the text mode's business only.
        assert!(v.get("config_toml").is_none());

        let rows = v["checks"].as_array().unwrap();
        assert_eq!(rows.len(), r.checks.len());
        for row in rows {
            assert!(row["id"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(matches!(
                row["status"].as_str(),
                Some("pass" | "warn" | "fail")
            ));
            assert!(row["title"].as_str().is_some());
            assert!(row["detail"].as_str().is_some());
        }
        let cursor = rows.iter().find(|r| r["id"] == "cursor").unwrap();
        assert!(cursor["hint"].as_str().unwrap().contains("cursor"));
        // A row with nothing to fix carries no hint key at all.
        let build = rows.iter().find(|r| r["id"] == "build").unwrap();
        assert!(build.get("hint").is_none());
    }

    #[test]
    fn the_text_mode_shows_every_row_its_hints_and_the_config() {
        let mut f = healthy();
        f.cursor = None;
        let r = f.into_report();
        let text = r.render_text();

        assert!(text.starts_with("telemouse 9.9.9 — doctor\n"));
        for c in &r.checks {
            assert!(text.contains(&c.title), "missing row {}", c.title);
            assert!(text.contains(&c.detail), "missing detail of {}", c.id);
            if let Some(h) = &c.hint {
                assert!(text.contains(h), "missing hint of {}", c.id);
            }
        }
        assert!(text.contains("verdict: warn"));
        assert!(text.contains("1 warning(s)"));
        assert!(text.contains("--- resolved config ---\nmouse_cpi = 800.0\n"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn statuses_order_worst_last_and_spell_themselves_lowercase() {
        assert!(Status::Pass < Status::Warn && Status::Warn < Status::Fail);
        assert_eq!(serde_json::to_string(&Status::Fail).unwrap(), "\"fail\"");
        assert_eq!(Status::Warn.as_str(), "warn");
    }
}
