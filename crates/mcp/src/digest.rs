//! Big JSON in, small JSON out.
//!
//! `GET /api/state` carries the last 120 lines each component printed, and
//! `/api/stats` carries every counter the bridge keeps. Handing either to a
//! model verbatim would spend thousands of tokens on text nobody asked for,
//! so each tool answers with a projection: the fields that decide something,
//! and nothing else. The projections are pure functions over
//! [`serde_json::Value`] so the mapping is tested against recorded response
//! shapes rather than against a running panel.
//!
//! Everything here is defensive about missing fields: a minimal build has no
//! `stats`, an older panel may not have a key this version knows, and a
//! degraded answer is still an answer.

use serde_json::{Map, Value, json};

/// Copy `keys` from `src` into a fresh object, skipping what is not there.
fn pick(src: &Value, keys: &[&str]) -> Value {
    let mut out = Map::new();
    for k in keys {
        if let Some(v) = src.get(*k)
            && !v.is_null()
        {
            out.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// What `health` says about the control panel, from `GET /api/state`.
pub fn ctl_health(state: &Value) -> Value {
    let components: Vec<Value> = state
        .get("components")
        .and_then(Value::as_array)
        .map(|cs| cs.iter().map(component_health).collect())
        .unwrap_or_default();
    let processes: Vec<Value> = state
        .get("processes")
        .and_then(Value::as_array)
        .map(|ps| ps.iter().map(|p| pick(p, &["pid", "name"])).collect())
        .unwrap_or_default();
    json!({
        "reachable": true,
        "version": state.get("version").cloned().unwrap_or(Value::Null),
        "config": pick(state.get("config").unwrap_or(&Value::Null), &["path", "status"]),
        "recording": state.get("recording").cloned().unwrap_or(Value::Null),
        "components": components,
        "processes": processes,
    })
}

/// One component's line in the health digest: is it up, since when, how did
/// it last end, and — for capture in a full build — its live counters.
fn component_health(c: &Value) -> Value {
    let mut out = pick(
        c,
        &[
            "id",
            "running",
            "pid",
            "since_unix_s",
            "exits",
            "unexpected_exits",
            "saving",
            "bin_found",
        ],
    );
    if let Some(obj) = out.as_object_mut() {
        if let Some(exit) = c.get("last_exit").filter(|v| !v.is_null()) {
            obj.insert(
                "last_exit".into(),
                pick(exit, &["code", "ctrl_break", "at_unix_s", "hint"]),
            );
        }
        if let Some(stats) = c.get("stats").filter(|v| !v.is_null()) {
            obj.insert(
                "stats".into(),
                pick(
                    stats,
                    &[
                        "session",
                        "events",
                        "events_per_s",
                        "drops",
                        "idle_for_s",
                        "udp_unreachable",
                        "jsonl_dropped",
                        "kafka_dropped",
                        "game",
                        "pointer_locked",
                    ],
                ),
            );
        }
    }
    out
}

/// The component object for `id`, if the panel knows it.
pub fn component<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
    state
        .get("components")?
        .as_array()?
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(id))
}

/// The last `lines` lines a component printed, from the panel's in-memory
/// ring. `None` when the panel does not know that component at all.
pub fn component_log(state: &Value, id: &str, lines: usize) -> Option<Vec<String>> {
    let log = component(state, id)?.get("log")?.as_array()?;
    let start = log.len().saturating_sub(lines);
    Some(
        log[start..]
            .iter()
            .map(|l| match l.as_str() {
                Some(s) => s.to_string(),
                None => l.to_string(),
            })
            .collect(),
    )
}

/// What `health` says about the viz server, from `/healthz` and, when the
/// build has it, `/api/stats`.
pub fn viz_health(healthz: &Value, stats: Option<&Value>) -> Value {
    let mut out = pick(
        healthz,
        &[
            "ok",
            "udp_bound",
            "uptime_s",
            "feed",
            "last_datagram_age_s",
            "clients",
            "stalled",
            "udp_addr",
            "http_addr",
            "version",
        ],
    );
    if let Some(obj) = out.as_object_mut() {
        obj.insert("reachable".into(), Value::Bool(true));
        if let Some(s) = stats {
            obj.insert("stats".into(), stats_digest(s));
        }
    }
    out
}

/// The counters worth carrying out of `/api/stats`.
pub fn stats_digest(stats: &Value) -> Value {
    let mut out = pick(
        stats,
        &[
            "uptime_s",
            "datagrams",
            "datagrams_per_s",
            "forwarded",
            "parse_errors",
            "lag_drops",
            "lag_disconnects",
            "clients",
            "seq_gaps",
        ],
    );
    if let Some(obj) = out.as_object_mut()
        && let Some(l) = stats.get("latency").filter(|v| !v.is_null())
    {
        obj.insert(
            "latency".into(),
            pick(l, &["samples", "p50_us", "p99_us", "max_us", "negative"]),
        );
    }
    out
}

/// A counter's value as a float, for the difference between two samples.
fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

/// What changed between two `/api/stats` readings `seconds` apart.
///
/// Rates rather than totals: "is data arriving now" is a different question
/// from "how much has arrived since the server started", and only the first
/// one is what `live_stats` is for.
pub fn stats_delta(first: &Value, second: &Value, seconds: f64) -> Value {
    let mut out = Map::new();
    out.insert("sampled_s".into(), json!(round2(seconds)));
    let per_s = |key: &str| -> Option<f64> {
        let (a, b) = (num(first, key)?, num(second, key)?);
        let d = (b - a).max(0.0);
        Some(if seconds > 0.0 { d / seconds } else { d })
    };
    for (key, name) in [
        ("datagrams", "datagrams_per_s"),
        ("forwarded", "forwarded_per_s"),
        ("parse_errors", "parse_errors_per_s"),
        ("lag_drops", "lag_drops_per_s"),
    ] {
        if let Some(v) = per_s(key) {
            out.insert(name.into(), json!(round2(v)));
        }
    }
    for key in ["datagrams", "forwarded", "parse_errors", "lag_drops"] {
        if let Some((a, b)) = num(first, key).zip(num(second, key)) {
            out.insert(format!("{key}_delta"), json!(round2((b - a).max(0.0))));
        }
    }
    out.insert("latest".into(), stats_digest(second));
    Value::Object(out)
}

/// Two decimals: these are rates for a human-readable answer, not a series.
fn round2(v: f64) -> f64 {
    if v.is_finite() {
        (v * 100.0).round() / 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/api/state` body shaped like the real one, cut to what the digest
    /// reads plus the noise it must drop.
    fn state() -> Value {
        json!({
            "self_pid": 900, "now_unix_s": 1_800_000_000u64, "version": "0.2.0",
            "config": { "path": "C:/tm/telemouse.toml", "found": true, "seeded": false, "status": "loaded", "mtime_unix_s": 1 },
            "places": { "panel_url": "http://127.0.0.1:7880/", "logs": "C:/tm/logs" },
            "recording": { "enabled": true, "dir": "C:/tm/recordings" },
            "components": [
                {
                    "id": "capture", "label": "Capture agent", "summary": "…", "bin": "telemouse",
                    "bin_path": "C:/tm/telemouse.exe", "bin_found": true, "flags": [{"flag": "--print", "help": "…"}],
                    "running": true, "pid": 4321, "since_unix_s": 1_799_999_000u64,
                    "exits": 2, "unexpected_exits": 0, "saving": true,
                    "last_exit": { "code": -1073741510, "ctrl_break": true, "at_unix_s": 1, "hint": "clean", "last_line": "bye" },
                    "log": ["one", "two", "three", "four"], "log_seq": 4,
                    "stats": { "session": "s-9", "events_per_s": 998.0, "events": 120_000, "drops": 0,
                               "idle_for_s": 0.1, "udp_unreachable": false, "jsonl_dropped": 0,
                               "kafka_dropped": 508_000, "game": "cs2.exe", "pointer_locked": true },
                    "foreground_seen": [{ "exe": "cs2.exe", "last_unix_s": 1 }]
                },
                { "id": "viz", "running": false, "bin_found": true, "exits": 0, "unexpected_exits": 0, "log": [], "log_seq": 0, "last_exit": null }
            ],
            "processes": [ { "pid": 4321, "name": "telemouse.exe", "rss_mb": 12.5, "started": "…" } ]
        })
    }

    #[test]
    fn the_ctl_digest_keeps_the_decisive_fields_and_drops_the_bulk() {
        let d = ctl_health(&state());
        assert_eq!(d["reachable"], true);
        assert_eq!(d["version"], "0.2.0");
        assert_eq!(d["config"]["status"], "loaded");
        // The config digest is only path and status; mtime and the seeding
        // flags belong to the panel's own UI.
        assert!(d["config"].get("mtime_unix_s").is_none());
        let capture = &d["components"][0];
        assert_eq!(capture["id"], "capture");
        assert_eq!(capture["running"], true);
        assert_eq!(capture["pid"], 4321);
        assert_eq!(capture["stats"]["kafka_dropped"], 508_000);
        assert_eq!(capture["last_exit"]["ctrl_break"], true);
        // The 120-line log ring, the flag help and the foreground list are
        // what logs_tail and the panel are for, not what health carries.
        for noisy in ["log", "flags", "foreground_seen", "summary", "bin_path"] {
            assert!(
                capture.get(noisy).is_none(),
                "health must not carry {noisy:?}"
            );
        }
        assert!(capture["last_exit"].get("last_line").is_none());
        assert_eq!(d["processes"][0]["pid"], 4321);
        assert_eq!(d["processes"][0]["name"], "telemouse.exe");
        assert!(d["processes"][0].get("rss_mb").is_none());
    }

    #[test]
    fn a_component_that_never_ran_has_no_last_exit_key() {
        let d = ctl_health(&state());
        let viz = &d["components"][1];
        assert_eq!(viz["running"], false);
        assert!(viz.get("last_exit").is_none());
        assert!(viz.get("stats").is_none(), "a minimal build has no stats");
        assert!(viz.get("pid").is_none(), "nothing is running");
    }

    #[test]
    fn an_empty_or_unknown_state_digests_to_something_rather_than_panicking() {
        let d = ctl_health(&json!({}));
        assert_eq!(d["reachable"], true);
        assert_eq!(d["components"], json!([]));
        assert_eq!(d["processes"], json!([]));
        assert!(component(&json!({}), "capture").is_none());
        assert!(component_log(&json!({}), "capture", 10).is_none());
    }

    #[test]
    fn the_component_log_is_the_tail_of_the_panels_ring() {
        let s = state();
        assert_eq!(component_log(&s, "capture", 2).unwrap(), ["three", "four"]);
        assert_eq!(
            component_log(&s, "capture", 99).unwrap(),
            ["one", "two", "three", "four"]
        );
        assert_eq!(component_log(&s, "viz", 5).unwrap(), Vec::<String>::new());
        assert!(component_log(&s, "nope", 5).is_none());
    }

    fn stats(datagrams: u64, forwarded: u64) -> Value {
        json!({
            "uptime_s": 60.0, "datagrams": datagrams, "datagrams_per_s": 52.0,
            "forwarded": forwarded, "parse_errors": 0, "lag_drops": 3, "lag_disconnects": 0,
            "clients": 1, "session_cached": true, "bytes_in": 999_999,
            "latency": { "samples": 100, "p50_us": 900, "p99_us": 2_400, "max_us": 9_000, "mean_us": 1_000, "negative": 0 },
            "seq_gaps": 2
        })
    }

    #[test]
    fn the_viz_digest_joins_healthz_and_stats() {
        let health = json!({
            "ok": true, "udp_bound": true, "uptime_s": 60.0, "feed": "live",
            "last_datagram_age_s": 0.02, "clients": 1, "stalled": false,
            "udp_addr": "127.0.0.1:7878", "http_addr": "127.0.0.1:7879", "version": "0.2.0"
        });
        let d = viz_health(&health, Some(&stats(3_000, 3_000)));
        assert_eq!(d["reachable"], true);
        assert_eq!(d["feed"], "live");
        assert_eq!(d["stats"]["latency"]["p99_us"], 2_400);
        assert_eq!(d["stats"]["seq_gaps"], 2);
        // mean_us and the byte counters are not what a verdict turns on.
        assert!(d["stats"]["latency"].get("mean_us").is_none());
        assert!(d["stats"].get("bytes_in").is_none());
    }

    #[test]
    fn a_minimal_viz_digests_without_stats() {
        let d = viz_health(
            &json!({ "ok": true, "udp_bound": true, "uptime_s": 3.0 }),
            None,
        );
        assert_eq!(d["ok"], true);
        assert!(d.get("stats").is_none());
        assert!(d.get("feed").is_none());
    }

    #[test]
    fn the_delta_is_a_rate_over_the_sampling_window() {
        let d = stats_delta(&stats(1_000, 900), &stats(1_520, 1_400), 10.0);
        assert_eq!(d["sampled_s"], 10.0);
        assert_eq!(d["datagrams_per_s"], 52.0);
        assert_eq!(d["datagrams_delta"], 520.0);
        assert_eq!(d["forwarded_per_s"], 50.0);
        assert_eq!(d["parse_errors_per_s"], 0.0);
        assert_eq!(d["latest"]["datagrams"], 1_520);
    }

    #[test]
    fn a_zero_length_window_reports_the_difference_itself() {
        let d = stats_delta(&stats(10, 10), &stats(10, 10), 0.0);
        assert_eq!(d["sampled_s"], 0.0);
        assert_eq!(d["datagrams_per_s"], 0.0);
    }

    #[test]
    fn a_counter_that_went_backwards_does_not_become_a_negative_rate() {
        // viz restarted between the two samples: its counters reset.
        let d = stats_delta(&stats(5_000, 5_000), &stats(10, 10), 5.0);
        assert_eq!(d["datagrams_per_s"], 0.0);
        assert_eq!(d["datagrams_delta"], 0.0);
        assert_eq!(d["latest"]["datagrams"], 10);
    }
}
