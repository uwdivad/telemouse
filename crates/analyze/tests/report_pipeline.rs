//! End-to-end: write a synthetic recording to a temp dir, run the whole
//! report pipeline over the file, and check the outputs.
//!
//! Everything the metric modules are unit-tested against goes through the
//! JSONL round-trip here, so a serialization change that quietly loses events
//! or batch metadata shows up as a failing metric rather than a passing test.

use std::path::Path;

use telemouse_analyze::{load, report, series::Params, testutil, trend};
use telemouse_core::{Marker, RawEvent, event::buttons};

/// Three flicks-with-correction-and-click, a slow drag that must *not* be a
/// flick, and a marker. Amplitudes are exact count sums by construction.
fn synthetic_events() -> Vec<RawEvent> {
    let mut b = testutil::StreamBuilder::new();
    b.idle_ms(250);
    for _ in 0..3 {
        b.move_ms(25, 60, 0) // 1500 counts ballistic
            .idle_ms(10)
            .move_ms(10, -10, 0) // 100 counts back
            .idle_ms(5)
            .button(buttons::LEFT_DOWN)
            .idle_ms(39)
            .button(buttons::LEFT_UP) // 40ms hold
            .idle_ms(600);
    }
    // A long slow drag: lots of travel, never fast enough to be a flick.
    b.move_at_ms(1500, 400.0, 0.0).idle_ms(500);
    b.into_events()
}

fn write_fixture(dir: &Path) -> std::path::PathBuf {
    let cfg = testutil::session_cfg();
    let events = synthetic_events();
    let marker = Marker {
        session_id: cfg.session_id.clone(),
        seq_no: 0,
        ts_qpc: cfg.anchor.qpc + cfg.anchor.ms_to_ticks(1000),
        ts_utc_us: cfg
            .anchor
            .qpc_to_utc_us(cfg.anchor.qpc + cfg.anchor.ms_to_ticks(1000)),
        label: "round-start".into(),
    };
    // 448 events per batch, matching the capture agent's cap.
    testutil::write_session(dir, &cfg, Some("cs2.exe"), &events, &[marker], 448)
}

#[test]
fn full_pipeline_over_a_written_recording() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path());

    // --- load -------------------------------------------------------------
    let session = load::load_session(&path).expect("load");
    let event_count = session.events.len();
    assert_eq!(session.config.session_id, "s-test");
    assert_eq!(event_count, synthetic_events().len());
    assert_eq!(session.markers.len(), 1);
    assert!(session.batches.len() > 1, "events should span many batches");
    assert!(session.batches.iter().all(|b| b.event_count <= 448));
    assert_eq!(session.total_drops, 0);
    assert_eq!(session.total_abs_frames, 0);
    assert!(session.bad_lines.is_empty());
    assert_eq!(session.dominant_game().as_deref(), Some("cs2.exe"));
    assert!(session.batches[0].first_event_qpc.is_some());

    // --- analyze ----------------------------------------------------------
    let r = report::build(session, Params::default());

    assert_eq!(r.schema, report::SCHEMA);
    assert_eq!(r.analyzer_version, report::ANALYZER_VERSION);
    assert!(r.generated_utc_us > 1_700_000_000_000_000);
    assert_eq!(r.grid_dt_us, 1000);
    assert!(r.grid_cells > 0);
    assert_eq!(r.session.session_id, "s-test");
    assert_eq!(r.session.game.as_deref(), Some("cs2.exe"));
    assert!(!r.session.aim_profile_missing);
    assert!((r.session.deg_per_count - testutil::FIXTURE_DEG_PER_COUNT).abs() < 1e-12);

    // Three flicks; the slow drag is excluded.
    assert_eq!(r.flicks.count, 3, "{:#?}", r.flicks.flicks);
    for f in &r.flicks.flicks {
        assert!((f.amplitude_counts - 1500.0).abs() < 1e-6);
        assert!((f.amplitude_deg - 66.0).abs() < 1e-6);
        assert!((f.overshoot_ratio - 100.0 / 1500.0).abs() < 1e-6);
        // The click lands 50ms after the first raw event of the pull; the
        // detector's flick start sits a few ms earlier, where the *smoothed*
        // speed left stillness (the SG window is symmetric).
        let ttc = f
            .time_to_click_ms
            .expect("each flick is followed by a click");
        assert!((ttc - 50.0).abs() <= 5.0, "time to click {ttc}");
    }
    assert_eq!(r.flicks.clicked_fraction, 1.0);

    // Clicks: three presses, each held 40ms.
    assert_eq!(r.clicks.total_clicks, 3);
    assert_eq!(r.clicks.unmatched_downs, 0);
    assert!((r.clicks.hold_ms.median - 40.0).abs() < 1e-6);
    assert_eq!(
        r.clicks.clicks[0].button,
        telemouse_analyze::clicks::Button::Left
    );

    // Kinematics: 3 * 1600 counts of flick travel plus 600 of drag.
    let expected_counts = 3.0 * (1500.0 + 100.0) + 600.0;
    assert!(
        (r.kinematics.total_distance_counts - expected_counts).abs() < 1.0,
        "{}",
        r.kinematics.total_distance_counts
    );
    assert!(r.kinematics.total_distance_m > 0.0);
    assert!(r.kinematics.segment_count >= 4);
    // The cm/deg accel variants are the count figure rescaled.
    assert!(
        (r.kinematics.accel_deg_per_s2.median
            - r.kinematics.accel_counts_per_s2.median * testutil::FIXTURE_DEG_PER_COUNT)
            .abs()
            < 1e-6
    );

    // Quality: clean synthetic capture, apart from the deliberate idle gaps.
    assert_eq!(r.quality.ring_drops, 0);
    assert_eq!(r.quality.lost_batches, 0);
    assert_eq!(r.quality.monotonicity_violations, 0);
    assert!(!r.quality.aim_profile_missing);
    assert_eq!(r.quality.event_count, event_count);
    assert_eq!(r.quality.locked_fraction, 1.0);
    assert!((r.quality.dominant_game_share - 1.0).abs() < 1e-9);
    assert_eq!(r.quality.anchor_uncertainty_us, Some(8));
    assert_eq!(r.quality.batch_latency_ms.n, r.session.batch_count);

    // Per-second table covers the session with no holes.
    assert!(r.per_second.len() >= 4, "{} rows", r.per_second.len());
    for (i, row) in r.per_second.iter().enumerate() {
        assert_eq!(row.second, i as u64);
        assert_eq!(
            row.t_utc_us,
            r.session.started_utc_us + i as i64 * 1_000_000
        );
    }
    assert_eq!(r.per_second.iter().map(|s| s.clicks).sum::<u64>(), 3);
    assert_eq!(r.per_second.iter().map(|s| s.flicks).sum::<u64>(), 3);
    assert_eq!(
        r.per_second.iter().map(|s| s.events).sum::<u64>(),
        event_count as u64
    );

    // Per-minute table rolls the same numbers up.
    assert_eq!(r.per_minute.len(), 1);
    assert_eq!(r.per_minute[0].flicks, 3);
    assert_eq!(r.per_minute[0].clicks, 3);

    // Marker landed on the timeline, and cut the session in two.
    assert_eq!(r.markers.len(), 1);
    assert!((r.markers[0].t_s - 1.0).abs() < 1e-6);
    assert_eq!(r.markers[0].label, "round-start");
    assert_eq!(r.segments.len(), 2);
    assert_eq!(r.segments[1].label, "round-start");
    assert_eq!(
        r.segments.iter().map(|s| s.flicks).sum::<usize>(),
        r.flicks.count
    );
    assert!(r.per_second.iter().any(|s| s.marker_label == "round-start"));

    // --- summary ----------------------------------------------------------
    // The few-KB projection an agent reads instead of the whole document; it
    // has to name the stretches, or a marked experiment is unreadable from it.
    let sum = r.summary(Vec::new());
    assert_eq!(sum.schema, report::SUMMARY_SCHEMA);
    assert_eq!(sum.markers.len(), 1);
    assert_eq!(sum.markers_total, 1);
    assert_eq!(sum.markers[0].label, "round-start");
    assert!((sum.markers[0].t_s - 1.0).abs() < 1e-6);
    assert_eq!(sum.segments_total, 2);
    assert_eq!(sum.segments[0].next_label, "round-start");
    assert_eq!(sum.segments[1].label, "round-start");
    assert_eq!(
        sum.segments.iter().map(|s| s.flicks).sum::<usize>(),
        r.flicks.count
    );
    let sum_json = serde_json::to_string_pretty(&sum).unwrap();
    assert!(
        sum_json.len() < 8_000,
        "the summary is meant to stay a few KB, not {} bytes",
        sum_json.len()
    );

    // --- render -----------------------------------------------------------
    let text = r.render();
    for needle in [
        "telemouse session  s-test",
        "Data quality",
        "Kinematics",
        "Flicks",
        "Trigger discipline",
        "Repositioning lifts",
        "round-start",
    ] {
        assert!(text.contains(needle), "missing {needle:?}");
    }
}

#[test]
fn json_and_csv_outputs_are_well_formed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path());
    let session = load::load_session(&path).unwrap();
    let r = report::build(session, Params::default());

    // JSON: parse back as an untyped value and index the documented shape.
    let json = r.to_json_pretty().unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["schema"], report::SCHEMA);
    for key in [
        "analyzer_version",
        "generated_utc_us",
        "compute_ms",
        "grid_cells",
        "grid_dt_us",
        "params",
        "session",
        "quality",
        "kinematics",
        "flicks",
        "micro",
        "clicks",
        "lifts",
        "markers",
        "segments",
        "per_second",
        "per_minute",
        "warnings",
        "timings",
    ] {
        assert!(v.get(key).is_some(), "missing top-level key {key:?}");
    }
    assert_eq!(v["flicks"]["count"], 3);
    assert_eq!(v["flicks"]["flicks"].as_array().unwrap().len(), 3);
    assert_eq!(v["params"]["flick_speed"], 800.0);
    assert_eq!(v["clicks"]["clicks"][0]["button"], "left");
    assert!(v["quality"]["interval_histogram"].as_array().unwrap().len() > 1);
    assert!(
        v["micro"]["micro_adjustments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["count"].as_u64().unwrap() > 0)
    );

    // And it survives a typed round-trip. Compared field-wise: serde_json's
    // float parser can land 1 ULP off, so byte-identical f64s are not a
    // guarantee of the format.
    let back: report::Report = serde_json::from_str(&json).unwrap();
    assert_eq!(back.schema, r.schema);
    assert_eq!(back.session.session_id, r.session.session_id);
    assert_eq!(back.flicks.count, r.flicks.count);
    assert_eq!(back.clicks.total_clicks, r.clicks.total_clicks);
    assert_eq!(back.per_second.len(), r.per_second.len());
    assert!(
        (back.flicks.flicks[0].overshoot_ratio - r.flicks.flicks[0].overshoot_ratio).abs() < 1e-12
    );

    // CSVs.
    let out = dir.path().join("derived");
    let written = r.write_csvs(&out).unwrap();
    assert_eq!(written.len(), 3);

    let per_second = std::fs::read_to_string(out.join("per_second.csv")).unwrap();
    let lines: Vec<&str> = per_second.lines().collect();
    assert_eq!(lines.len(), r.per_second.len() + 1);
    let header_cols = lines[0].split(',').count();
    assert!(
        lines[1..]
            .iter()
            .all(|l| l.split(',').count() == header_cols)
    );
    assert!(lines[0].contains("distance_cm"));
    assert!(lines[0].contains("marker_label"));

    let flicks = std::fs::read_to_string(out.join("flicks.csv")).unwrap();
    let flines: Vec<&str> = flicks.lines().collect();
    assert_eq!(flines.len(), 4);
    assert!(flines[0].starts_with("index,t_start_s"));
    assert!(
        flines[1].contains("66.0"),
        "amplitude column: {}",
        flines[1]
    );

    let minutes = std::fs::read_to_string(out.join("per_minute.csv")).unwrap();
    assert_eq!(minutes.lines().count(), r.per_minute.len() + 1);
}

#[test]
fn a_recording_without_a_sensitivity_profile_warns_but_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = testutil::session_cfg();
    let events = synthetic_events();
    // A process the fixture config has no profile for.
    let path = testutil::write_session(dir.path(), &cfg, Some("valorant.exe"), &events, &[], 448);

    let session = load::load_session(&path).unwrap();
    let r = report::build(session, Params::default());

    assert!(r.session.aim_profile_missing);
    assert_eq!(r.session.sens, 1.0);
    assert!(
        r.warnings.iter().any(|w| w.contains("sensitivity profile")),
        "{:?}",
        r.warnings
    );
    // Count-space metrics are unaffected by the missing profile.
    assert_eq!(r.flicks.count, 3);
    assert!((r.flicks.flicks[0].amplitude_counts - 1500.0).abs() < 1e-6);
    // Degree-space metrics fall back to sens 1.0 * 0.022.
    assert!((r.flicks.flicks[0].amplitude_deg - 1500.0 * 0.022).abs() < 1e-6);
    assert!(r.render().contains("FALLBACK"));
}

/// The `report --json-dir` cache: computed once, reused after, and reported as
/// such in the outcome the CLI logs.
#[test]
fn a_cached_report_is_reused_and_matches_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_fixture(dir.path());
    let cache = dir.path().join("cache");

    let (first, why) = trend::report_for(&path, Some(&cache), Params::default()).unwrap();
    assert_eq!(why, trend::CacheOutcome::Missing);
    assert!(cache.join("s-test.report.json").exists());

    let (second, why) = trend::report_for(&path, Some(&cache), Params::default()).unwrap();
    assert_eq!(why, trend::CacheOutcome::Hit);
    assert_eq!(second.flicks.count, first.flicks.count);
    assert_eq!(second.generated_utc_us, first.generated_utc_us);
    assert_eq!(second.per_second.len(), first.per_second.len());

    // Touching the recording invalidates it.
    let text = std::fs::read_to_string(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, text).unwrap();
    let (_, why) = trend::report_for(&path, Some(&cache), Params::default()).unwrap();
    assert_eq!(why, trend::CacheOutcome::Stale);
}
