use super::render::commas;
use super::*;
use crate::testutil::{StreamBuilder, loaded_from, marker_at};

fn demo_session() -> LoadedSession {
    let mut b = StreamBuilder::new();
    b.idle_ms(200);
    for _ in 0..3 {
        b.flick_cycle(1, 0, 500);
    }
    loaded_from(b.into_events(), Some("cs2.exe"))
}

/// [`build`] with parameters that are known to validate.
fn build(session: LoadedSession, params: Params) -> Report {
    super::build(session, params).expect("valid params")
}

#[test]
fn a_full_report_has_every_section_populated() {
    let r = build(demo_session(), Params::default());
    assert_eq!(r.schema, SCHEMA);
    assert_eq!(r.analyzer_version, ANALYZER_VERSION);
    assert!(r.generated_utc_us > 1_700_000_000_000_000);
    assert!(r.compute_ms.is_finite() && r.compute_ms > 0.0);
    assert_eq!(r.total_ms, r.load_ms + r.compute_ms);
    assert!(r.grid_cells > 0);
    assert!(r.grid_stored_cells <= r.grid_cells);
    assert_eq!(r.grid_dt_us, 1000);
    assert_eq!(r.session.session_id, "s-test");
    assert_eq!(r.session.game.as_deref(), Some("cs2.exe"));
    assert!(!r.session.aim_profile_missing);
    assert_eq!(r.flicks.count, 3);
    assert_eq!(r.clicks.total_clicks, 3);
    assert!(r.kinematics.total_distance_counts > 0.0);
    assert!(!r.per_second.is_empty());
    assert_eq!(r.per_minute.len(), 1);
    assert_eq!(r.segments.len(), 1, "unmarked session is one interval");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    // Every build phase is timed.
    assert!(r.timings.len() >= 8, "{:?}", r.timings);
    assert!(r.timings.iter().any(|t| t.phase == "prepare"));
}

#[test]
fn parallel_and_sequential_builds_have_identical_results() {
    let mut session = demo_session();
    session.markers.push(marker_at(1_000, "round-2"));
    session.total_drops = 2;
    session.batches[0].drops_since_last = 2;
    let sequential = build_with_parallelism(session.clone(), Params::default(), false).unwrap();
    let parallel = build_with_parallelism(session, Params::default(), true).unwrap();

    let mut sequential = serde_json::to_value(sequential).unwrap();
    let mut parallel = serde_json::to_value(parallel).unwrap();
    for report in [&mut sequential, &mut parallel] {
        let report = report.as_object_mut().unwrap();
        report.remove("generated_utc_us");
        report.remove("compute_ms");
        report.remove("total_ms");
        report.remove("timings");
    }
    // Section by section, so a mismatch names the section instead of
    // dumping two whole reports.
    let (p, s) = (
        parallel.as_object().unwrap(),
        sequential.as_object().unwrap(),
    );
    assert_eq!(p.keys().collect::<Vec<_>>(), s.keys().collect::<Vec<_>>());
    for (k, pv) in p {
        assert_eq!(
            pv, &s[k],
            "section {k} differs between parallel and sequential"
        );
    }
}

#[test]
fn report_parallelism_requires_enough_work_and_cpus() {
    assert!(!parallel_worthwhile(PARALLEL_MIN_EVENTS - 1, 32));
    assert!(!parallel_worthwhile(
        PARALLEL_MIN_EVENTS,
        PARALLEL_MIN_CPUS - 1
    ));
    assert!(parallel_worthwhile(PARALLEL_MIN_EVENTS, PARALLEL_MIN_CPUS));
}

#[test]
fn the_rendered_report_reads_like_a_report() {
    let r = build(demo_session(), Params::default());
    let text = r.render();
    for needle in [
        "telemouse session  s-test",
        "Data quality",
        "Kinematics",
        "Flicks",
        "Sub-movements & micro-control",
        "Trigger discipline",
        "Repositioning lifts",
        "Notes",
        "path efficiency",
        "overshoot ratio",
        "pointer locked",
        "batch latency",
        "no data-quality problems detected",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
    // Every line stays inside a sane terminal width.
    for line in text.lines() {
        assert!(
            line.chars().count() <= 100,
            "line too wide ({}): {line}",
            line.chars().count()
        );
    }
}

#[test]
fn json_round_trips_through_serde() {
    let r = build(demo_session(), Params::default());
    let js = r.to_json_pretty().unwrap();
    let back: Report = serde_json::from_str(&js).unwrap();
    // Compared field-wise with a tolerance rather than by `==`:
    // serde_json's float *parser* is up to 1 ULP off on some values
    // (std's `str::parse` is exact), so a byte-perfect f64 round-trip is
    // not something the format guarantees.
    assert_eq!(back.schema, r.schema);
    assert_eq!(back.analyzer_version, r.analyzer_version);
    assert_eq!(back.generated_utc_us, r.generated_utc_us);
    assert_eq!(back.grid_cells, r.grid_cells);
    assert_eq!(back.session.session_id, r.session.session_id);
    assert_eq!(back.flicks.count, r.flicks.count);
    assert_eq!(back.clicks.total_clicks, r.clicks.total_clicks);
    assert_eq!(back.per_second.len(), r.per_second.len());
    assert_eq!(back.warnings, r.warnings);
    assert!((back.kinematics.total_distance_m - r.kinematics.total_distance_m).abs() < 1e-9);
    assert!((back.flicks.flicks[0].amplitude_deg - r.flicks.flicks[0].amplitude_deg).abs() < 1e-9);

    // And the shape a consumer would index into.
    let v: serde_json::Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v["schema"], SCHEMA);
    assert!(v["flicks"]["flicks"].as_array().unwrap().len() == 3);
    assert!(!v["per_second"].as_array().unwrap().is_empty());
    assert!(v["kinematics"]["total_distance_m"].as_f64().unwrap() > 0.0);
    assert!(v["clicks"]["clicks"][0]["button"] == "left");
}

#[test]
fn csvs_land_in_the_requested_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("derived");
    let r = build(demo_session(), Params::default());
    let written = r.write_csvs(&out).unwrap();
    assert_eq!(written.len(), 3);

    let ps = std::fs::read_to_string(out.join("per_second.csv")).unwrap();
    assert_eq!(ps.lines().count(), r.per_second.len() + 1);
    let fl = std::fs::read_to_string(out.join("flicks.csv")).unwrap();
    assert_eq!(fl.lines().count(), 4); // header + 3 flicks
    let pm = std::fs::read_to_string(out.join("per_minute.csv")).unwrap();
    assert_eq!(pm.lines().count(), r.per_minute.len() + 1);
}

#[test]
fn json_streams_to_a_file_identically_to_the_string_form() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("r.json");
    let r = build(demo_session(), Params::default());
    r.write_json(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.trim_end(), r.to_json_pretty().unwrap());
    let back: Report = serde_json::from_str(&text).unwrap();
    assert_eq!(back.flicks.count, r.flicks.count);
}

#[test]
fn warnings_reach_the_rendered_output() {
    let mut s = demo_session();
    s.total_drops = 5;
    s.batches[0].drops_since_last = 5;
    let r = build(s, Params::default());
    assert!(!r.warnings.is_empty());
    let text = r.render();
    assert!(text.contains("ring-buffer drops"), "{text}");
    assert!(text.contains("← problem"), "{text}");
}

#[test]
fn an_empty_recording_still_renders() {
    let r = build(loaded_from(Vec::new(), None), Params::default());
    let text = r.render();
    assert!(text.contains("telemouse session"));
    assert!(text.contains("unknown"));
    assert_eq!(r.flicks.count, 0);
    assert!(r.to_json_pretty().is_ok());
}

#[test]
fn thousands_separators() {
    assert_eq!(commas(0), "0");
    assert_eq!(commas(999), "999");
    assert_eq!(commas(1_000), "1,000");
    assert_eq!(commas(1_234_567), "1,234,567");
}

#[test]
fn markers_are_placed_on_the_session_timeline() {
    let mut s = demo_session();
    s.markers.push(marker_at(1500, "clutch"));
    let r = build(s, Params::default());
    assert_eq!(r.markers.len(), 1);
    assert!((r.markers[0].t_s - 1.5).abs() < 1e-6);
    assert!(r.render().contains("clutch"));
}

#[test]
fn split_by_marker_renders_the_per_interval_table() {
    let mut s = demo_session();
    s.markers.push(marker_at(1000, "round-2"));
    let params = Params {
        split_by_marker: true,
        ..Default::default()
    };
    let r = build(s, params);
    assert_eq!(r.segments.len(), 2);
    let text = r.render();
    assert!(text.contains("Between markers"), "{text}");
    assert!(text.contains("round-2"), "{text}");
    assert!(text.contains("(before the first marker)"), "{text}");
    // Without the flag the JSON still carries them, the terminal does not.
    let plain = build(
        {
            let mut s = demo_session();
            s.markers.push(marker_at(1000, "round-2"));
            s
        },
        Params::default(),
    );
    assert_eq!(plain.segments.len(), 2);
    assert!(!plain.render().contains("Between markers"));
}

#[test]
fn the_summary_names_the_stretch_every_segment_covers() {
    let mut s = demo_session();
    s.markers.push(marker_at(500, "sens A"));
    s.markers.push(marker_at(1_000, "sens B"));
    let sum = build(s, Params::default()).summary(Vec::new());

    assert_eq!(sum.schema, SUMMARY_SCHEMA);
    assert_eq!(
        sum.markers
            .iter()
            .map(|m| m.label.as_str())
            .collect::<Vec<_>>(),
        ["sens A", "sens B"]
    );
    assert!((sum.markers[0].t_s - 0.5).abs() < 1e-6);
    assert!((sum.markers[1].t_s - 1.0).abs() < 1e-6);
    assert_eq!(sum.markers_total, 2);

    // Three stretches: the warmup, then one per marker. Each says which
    // labels bound it, so "sens A" can be compared against "sens B"
    // without opening the full report.
    assert_eq!(sum.segments_total, 3);
    let bounds: Vec<(&str, &str)> = sum
        .segments
        .iter()
        .map(|s| (s.label.as_str(), s.next_label.as_str()))
        .collect();
    assert_eq!(
        bounds,
        [("", "sens A"), ("sens A", "sens B"), ("sens B", "")]
    );
    assert!((sum.segments[1].t_start_s - 0.5).abs() < 1e-6);
    assert!((sum.segments[1].t_end_s - 1.0).abs() < 1e-6);
    assert_eq!(
        sum.segments.iter().map(|s| s.flicks).sum::<usize>(),
        sum.flicks.count
    );

    // And it stays the few-KB document its callers paste into a chat.
    let json = serde_json::to_string_pretty(&sum).unwrap();
    assert!(json.len() < 8_000, "summary grew to {} bytes", json.len());
    // The marker rows survive the round-trip (the float-heavy headline
    // fields are compared field-wise elsewhere: serde_json's parser can
    // land 1 ULP off, so `==` over the whole document is not a promise
    // the format makes).
    let back: ReportSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.schema, sum.schema);
    assert_eq!(back.markers_total, sum.markers_total);
    assert_eq!(back.segments_total, sum.segments_total);
    let labels = |s: &ReportSummary| -> Vec<String> {
        s.markers
            .iter()
            .map(|m| m.label.clone())
            .chain(s.segments.iter().map(|g| g.next_label.clone()))
            .collect()
    };
    assert_eq!(labels(&back), labels(&sum));
}

#[test]
fn a_session_with_no_markers_carries_no_marker_rows() {
    let sum = build(demo_session(), Params::default()).summary(Vec::new());
    assert!(sum.markers.is_empty());
    assert_eq!(sum.markers_total, 0);
    // The lone interval would only repeat the headline numbers.
    assert!(sum.segments.is_empty());
    assert_eq!(sum.segments_total, 0);
}

#[test]
fn the_summary_caps_the_marker_and_segment_lists() {
    let mut s = demo_session();
    for i in 0..(MAX_SUMMARY_MARKERS as u64 + 5) {
        s.markers.push(marker_at(50 + i * 50, &format!("m{i}")));
    }
    let long = "x".repeat(MAX_SUMMARY_LABEL_CHARS + 40);
    s.markers.push(marker_at(20, &long));
    let sum = build(s, Params::default()).summary(Vec::new());

    assert_eq!(sum.markers.len(), MAX_SUMMARY_MARKERS);
    assert_eq!(sum.markers_total, MAX_SUMMARY_MARKERS + 6);
    assert_eq!(sum.segments.len(), MAX_SUMMARY_MARKERS);
    assert_eq!(sum.segments_total, MAX_SUMMARY_MARKERS + 7);
    // Oldest first, so the truncation drops the tail.
    assert_eq!(sum.markers[1].label, "m0");
    // A label longer than the rest of the system accepts is clipped, not
    // carried whole.
    assert_eq!(
        sum.markers[0].label.chars().count(),
        MAX_SUMMARY_LABEL_CHARS + 1
    );
    assert!(sum.markers[0].label.ends_with('…'));
    let json = serde_json::to_string_pretty(&sum).unwrap();
    assert!(json.len() < 8_000, "summary grew to {} bytes", json.len());
}

#[test]
fn unlabelled_hotkey_markers_are_told_apart_by_their_offsets() {
    let mut s = demo_session();
    s.markers.push(marker_at(400, "hotkey"));
    s.markers.push(marker_at(1_000, "hotkey"));
    let sum = build(s, Params::default()).summary(Vec::new());

    assert_eq!(sum.markers_total, 2);
    assert!(sum.markers.iter().all(|m| m.label == "hotkey"));
    assert!(sum.markers[0].t_s < sum.markers[1].t_s);
    // The segments still line up with the timeline even when nothing
    // distinguishes the labels.
    assert_eq!(sum.segments.len(), 3);
    assert!((sum.segments[1].t_start_s - 0.4).abs() < 1e-6);
    assert!((sum.segments[2].t_start_s - 1.0).abs() < 1e-6);
    assert_eq!(sum.segments[2].next_label, "");
}

#[test]
fn the_timing_table_covers_every_phase() {
    let mut r = build(demo_session(), Params::default());
    r.push_timing("render", 1.5);
    let t = r.timing_table();
    assert!(t.contains("prepare"), "{t}");
    assert!(t.contains("per_minute"), "{t}");
    assert!(t.contains("render"), "{t}");
    assert!(t.contains("phase elapsed sum"), "{t}");
    assert!(t.contains("build wall"), "{t}");
}

#[test]
fn a_build_with_invalid_params_is_an_error_not_a_panic() {
    let err = super::build(
        demo_session(),
        Params {
            sg_half: 3,
            sg_order: 7,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.field, "sg_order");
}

/// The capture agent writes its final sidecar after the recording's last
/// flush; a report built before that is brought up to date in place, and
/// its headline carries the sidecar's sink losses without a second read.
#[test]
fn refreshing_the_sidecar_updates_the_verdict_and_the_headline() {
    use std::collections::BTreeMap;
    use telemouse_core::recordings::{ExitReason, SessionMeta, SinkMeta};

    let mut s = demo_session();
    let events = s.events.len() as u64;
    s.sidecar = Some(SessionMeta {
        exit: ExitReason::Running.as_str().into(),
        events,
        ..Default::default()
    });
    let mut r = build(s, Params::default());
    assert!(r.quality.sidecar.as_ref().unwrap().unfinished);
    assert!(!r.quality.clean);
    assert!(
        r.warnings.iter().any(|w| w.contains("unfinished")),
        "{:?}",
        r.warnings
    );
    assert!(r.headline().quality.unfinished);

    let mut sinks = BTreeMap::new();
    sinks.insert(
        "kafka".to_string(),
        SinkMeta {
            dropped: 7,
            ..Default::default()
        },
    );
    r.refresh_sidecar(Some(&SessionMeta {
        exit: ExitReason::Interrupt.as_str().into(),
        clean: true,
        events,
        sinks,
        ..Default::default()
    }));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(r.quality.clean);
    let h = r.headline();
    assert!(!h.quality.unfinished);
    assert_eq!(h.quality.exit.as_deref(), Some("interrupt"));
    assert_eq!(h.quality.sink_losses, vec![("kafka".to_string(), 7)]);
    assert!(h.quality.clean);
    // `summary` with the same losses is the same document.
    assert_eq!(r.summary(vec![("kafka".to_string(), 7)]), h);
}

#[test]
fn bars_never_overflow_their_width() {
    use super::render::bar;
    for (f, filled) in [
        (0.0, 0),
        (0.5, 10),
        (1.0, 20),
        (7.0, 20),
        (-1.0, 0),
        (f64::NAN, 0),
    ] {
        let b = bar(f, 20);
        assert_eq!(b.chars().count(), 20, "{f}");
        assert_eq!(b.chars().filter(|&c| c == '█').count(), filled, "{f}");
    }
}
