use std::io::Write;

use super::index_cache::{INDEX_CACHE_FILE, INDEX_CACHE_VERSION, SessionIndexCache};
use super::parallel::{
    MAX_LOAD_WORKERS, PARALLEL_MIN_BYTES, load_parallel, load_sequential, load_workers,
};
use super::*;
use crate::testutil::{FIXTURE_FREQ, batch_env, marker_at, session_cfg, write_lines};
use telemouse_core::{Batch, Envelope, event::buttons};

fn ev(qpc: u64, dx: i32, dy: i32, buttons: u16) -> RawEvent {
    RawEvent {
        ts_qpc: qpc,
        dx,
        dy,
        buttons,
        ..Default::default()
    }
}

/// Everything a caller can observe about a load, compared between the two
/// paths. `load_ms` is wall time and `path` is the argument, so neither
/// says anything about the parse.
fn assert_same_load(path: &Path, workers: usize) {
    let want = load_sequential(path).expect("sequential load");
    let got = load_parallel(path, workers).expect("parallel load");
    let at = format!("{} with {workers} workers", path.display());
    assert_eq!(want.config, got.config, "config, {at}");
    assert_eq!(want.events, got.events, "events, {at}");
    assert_eq!(want.markers, got.markers, "markers, {at}");
    assert_eq!(want.batches, got.batches, "batches, {at}");
    assert_eq!(want.total_drops, got.total_drops, "drops, {at}");
    assert_eq!(
        want.total_abs_frames, got.total_abs_frames,
        "abs frames, {at}"
    );
    assert_eq!(want.bad_lines, got.bad_lines, "bad lines, {at}");
    assert_eq!(want.bytes, got.bytes, "bytes, {at}");
    assert_eq!(want.sidecar, got.sidecar, "sidecar, {at}");

    // The interner has to survive the split too: one allocation per
    // distinct name across the whole file, not one per chunk.
    let mut seen: Vec<Arc<str>> = Vec::new();
    for b in got.batches.iter().filter_map(|b| b.game.clone()) {
        match seen.iter().find(|s| ***s == *b) {
            Some(first) => assert!(Arc::ptr_eq(first, &b), "interning split, {at}"),
            None => seen.push(b),
        }
    }
}

/// A body with every shape the loader distinguishes: batches under both
/// spellings of one process, a blank line, a marker, a second session
/// envelope, a line whose tag is not where the fast path looks, interior
/// corruption and a cut-off tail.
fn mixed_lines(cfg: &SessionConfig) -> Vec<String> {
    let mut lines = vec![Envelope::Session(cfg.clone()).to_json().unwrap()];
    for i in 0..40u64 {
        let game = if i % 3 == 0 { "CS2.exe" } else { "cs2.exe" };
        let events = (0..5)
            .map(|k| ev(cfg.anchor.qpc + i * 10_000 + k * 1_000, 1, -1, 0))
            .collect();
        lines.push(
            batch_env(cfg, i, Some(game), (i % 4) as u32, events)
                .to_json()
                .unwrap(),
        );
        match i {
            7 => lines.push(String::new()),
            11 => lines.push(r#"{"type":"batch","session_id":"s-tes"#.to_string()),
            19 => lines.push(Envelope::Marker(marker_at(20, "clutch")).to_json().unwrap()),
            23 => lines.push(Envelope::Session(cfg.clone()).to_json().unwrap()),
            29 => lines.push(
                // Valid envelope, tag not first: takes the general path.
                r#"{"session_id":"s-test","type":"marker","seq_no":1,"ts_qpc":1,"ts_utc_us":1,"label":"late"}"#
                    .to_string(),
            ),
            31 => lines.push("not json at all".to_string()),
            _ => {}
        }
    }
    lines.push(r#"{"type":"batch","truncated"#.to_string());
    lines
}

/// The chunk-parallel loader must answer exactly what the sequential one
/// answers, whatever the worker count and wherever the chunk boundaries
/// land — including in the middle of a line, which is the normal case.
/// And the listing scan, which reads the same lines through the same
/// classifier, must agree with both.
#[test]
fn the_parallel_loader_and_the_scan_match_the_sequential_one() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let lines = mixed_lines(&cfg);

    let write = |name: &str, sep: &str, trailing: bool| {
        let mut text = lines.join(sep);
        if trailing {
            text.push_str(sep);
        }
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let files = [
        write("lf.jsonl", "\n", true),
        write("crlf.jsonl", "\r\n", true),
        write("no-trailing-newline.jsonl", "\n", false),
        write("crlf-no-trailing-newline.jsonl", "\r\n", false),
    ];

    // Worker counts well past the line count force empty chunks and
    // boundaries that land inside lines.
    for path in &files {
        for workers in [1, 2, 3, 5, 8, 13, 64] {
            assert_same_load(path, workers);
        }
        let loaded = load_sequential(path).unwrap();
        let scanned = scan_session(path).unwrap();
        assert_eq!(scanned.events, loaded.events.len() as u64, "{path:?}");
        assert_eq!(scanned.drops, loaded.total_drops, "{path:?}");
        assert_eq!(scanned.bad_lines, loaded.bad_lines, "{path:?}");
        assert_eq!(scanned.games, vec!["cs2.exe".to_string()], "{path:?}");
    }

    // The fixture is worth something only if it has the damage it claims.
    let s = load_sequential(&files[0]).unwrap();
    assert_eq!(s.events.len(), 200);
    assert_eq!(s.markers.len(), 2);
    assert_eq!(s.bad_lines.count, 3);
    assert!(!s.bad_lines.tail_only, "corruption reaches into the body");
    assert_eq!(s.dominant_game().as_deref(), Some("cs2.exe"));
}

/// A body that is one line, and a body with fewer lines than workers: both
/// end up with empty chunks, which must be as harmless as any other.
#[test]
fn degenerate_splits_are_harmless() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let one_batch = batch_env(
        &cfg,
        0,
        Some("cs2.exe"),
        0,
        (0..2_000)
            .map(|k| ev(cfg.anchor.qpc + k * 1_000, 1, 0, 0))
            .collect(),
    )
    .to_json()
    .unwrap();

    let header = Envelope::Session(cfg.clone()).to_json().unwrap();
    for (name, text) in [
        ("one-line.jsonl", format!("{header}\n{one_batch}\n")),
        ("no-body.jsonl", format!("{header}\n")),
        ("header-only-unterminated.jsonl", header.clone()),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        for workers in [1, 2, 7, 33] {
            assert_same_load(&path, workers);
        }
    }
}

/// The split is only used where it pays for itself.
#[test]
fn small_recordings_stay_on_the_sequential_path() {
    assert_eq!(load_workers(0), 1);
    assert_eq!(load_workers(PARALLEL_MIN_BYTES - 1), 1);
    assert!(load_workers(1 << 30) >= 1);
    assert!(load_workers(1 << 30) <= MAX_LOAD_WORKERS);
    // No chunk smaller than MIN_CHUNK_BYTES, whatever the core count.
    assert!(load_workers(PARALLEL_MIN_BYTES) <= 2);
}

#[test]
fn round_trips_a_hand_built_recording() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let mut lines = vec![Envelope::Session(cfg.clone()).to_json().unwrap()];
    lines.push(
        batch_env(
            &cfg,
            0,
            Some("cs2.exe"),
            3,
            vec![
                ev(cfg.anchor.qpc, 1, 0, 0),
                ev(cfg.anchor.qpc + 10_000, 2, -1, 0),
            ],
        )
        .to_json()
        .unwrap(),
    );
    lines.push(Envelope::Marker(marker_at(2, "clutch")).to_json().unwrap());
    lines.push(
        batch_env(
            &cfg,
            1,
            Some("cs2.exe"),
            4,
            vec![ev(cfg.anchor.qpc + 30_000, 0, 0, buttons::LEFT_DOWN)],
        )
        .to_json()
        .unwrap(),
    );
    let path = write_lines(dir.path(), "s-test.jsonl", &lines);

    let s = load_session(&path).unwrap();
    assert_eq!(s.config.session_id, "s-test");
    assert_eq!(s.events.len(), 3);
    assert_eq!(s.markers.len(), 1);
    assert_eq!(s.markers[0].label, "clutch");
    assert_eq!(s.batches.len(), 2);
    // Drops accumulate across batches.
    assert_eq!(s.total_drops, 7);
    assert!(s.bad_lines.is_empty());
    assert!(s.bad_lines.tail_only, "nothing bad is vacuously tail-only");
    assert_eq!(s.bad_lines.flag(), "-");
    assert_eq!(s.bytes, std::fs::metadata(&path).unwrap().len());
    assert!(s.load_ms.is_finite());
    assert_eq!(s.sidecar, None, "no sidecar was written");
    assert_eq!(s.dominant_game().as_deref(), Some("cs2.exe"));
    let (game, sens, fallback) = s.resolve_aim();
    assert_eq!(game.as_deref(), Some("cs2.exe"));
    assert!(!fallback);
    assert_eq!(sens.sens, 2.0);
    // Interning: both batches share one allocation for the process name.
    let a = s.batches[0].game.clone().unwrap();
    let b = s.batches[1].game.clone().unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(s.batches[0].first_event_qpc, Some(cfg.anchor.qpc));
}

#[test]
fn scan_matches_a_full_load() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let evs: Vec<RawEvent> = (0..50)
        .map(|i| ev(cfg.anchor.qpc + i * (FIXTURE_FREQ / 1000), 1, 0, 0))
        .collect();
    let lines = vec![
        Envelope::Session(cfg.clone()).to_json().unwrap(),
        batch_env(&cfg, 0, Some("cs2.exe"), 2, evs)
            .to_json()
            .unwrap(),
    ];
    let path = write_lines(dir.path(), "s-test.jsonl", &lines);

    let scanned = scan_session(&path).unwrap();
    assert_eq!(scanned.events, 50);
    assert_eq!(scanned.drops, 2);
    assert_eq!(scanned.games, vec!["cs2.exe".to_string()]);
    assert!((scanned.duration_s - 0.049).abs() < 1e-6);

    let loaded = load_session(&path).unwrap();
    assert_eq!(loaded.events.len() as u64, scanned.events);
    assert_eq!(loaded.total_drops, scanned.drops);
}

#[test]
fn corrupt_trailing_line_costs_one_line_not_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let lines = vec![
        Envelope::Session(cfg.clone()).to_json().unwrap(),
        batch_env(&cfg, 0, None, 0, vec![ev(cfg.anchor.qpc, 1, 1, 0)])
            .to_json()
            .unwrap(),
        r#"{"type":"batch","session_id":"s-tes"#.to_string(), // truncated
    ];
    let path = write_lines(dir.path(), "s-test.jsonl", &lines);
    let s = load_session(&path).unwrap();
    assert_eq!(s.events.len(), 1);
    assert_eq!(s.bad_lines.count, 1);
    assert_eq!(s.bad_lines.first_line, Some(3));
    assert_eq!(s.bad_lines.last_line, Some(3));
    assert!(s.bad_lines.tail_only, "a killed agent costs the last line");
    assert!(s.bad_lines.first_error.is_some());
    assert_eq!(s.bad_lines.flag(), "1");
}

/// Corruption in the *body* of a recording is a different failure from a
/// truncated tail: events are missing from the middle of the timeline.
#[test]
fn interior_corruption_is_distinguished_from_a_truncated_tail() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let good = |seq| {
        batch_env(
            &cfg,
            seq,
            Some("cs2.exe"),
            0,
            vec![ev(cfg.anchor.qpc + seq * FIXTURE_FREQ / 1000, 1, 1, 0)],
        )
        .to_json()
        .unwrap()
    };
    let lines = vec![
        Envelope::Session(cfg.clone()).to_json().unwrap(),
        good(0),
        r#"{"type":"batch","session_id":"s-t"#.to_string(), // line 3
        good(2),                                            // line 4: a good line *after* it
        r#"{"type":"batch","truncated"#.to_string(),        // line 5
    ];
    let path = write_lines(dir.path(), "s-test.jsonl", &lines);

    let s = load_session(&path).unwrap();
    assert_eq!(s.events.len(), 2);
    assert_eq!(s.bad_lines.count, 2);
    assert_eq!(s.bad_lines.first_line, Some(3));
    assert_eq!(s.bad_lines.last_line, Some(5));
    assert!(!s.bad_lines.tail_only, "line 4 parsed after line 3 did not");
    assert_eq!(s.bad_lines.flag(), "2!");

    // The listing scan agrees with the full load.
    let scanned = scan_session(&path).unwrap();
    assert_eq!(scanned.bad_lines, s.bad_lines);
}

/// A tagged marker line that does not parse is a bad line to both readers —
/// the scan used to wave every `{"type":"marker"` line through unread.
#[test]
fn a_broken_marker_is_bad_for_the_scan_and_the_load_alike() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            r#"{"type":"marker","label":5}"#.to_string(),
            batch_env(&cfg, 0, None, 0, vec![ev(cfg.anchor.qpc, 1, 1, 0)])
                .to_json()
                .unwrap(),
        ],
    );
    let s = load_session(&path).unwrap();
    assert_eq!(s.bad_lines.count, 1);
    assert_eq!(scan_session(&path).unwrap().bad_lines, s.bad_lines);
}

/// Heartbeats are a live-path signal and the agent never writes one to a
/// recording — but a file teed off the UDP stream by hand would hold
/// them, and that is not corruption. Both readers skip them without
/// counting a bad line and without changing a single number.
#[test]
fn heartbeat_lines_are_skipped_rather_than_counted_as_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let beat = Envelope::Heartbeat(telemouse_core::Heartbeat {
        session_id: cfg.session_id.clone(),
        ts_utc_us: cfg.anchor.utc_us + 1_000_000,
    })
    .to_json()
    .unwrap();
    let batch = |seq: u64| {
        batch_env(
            &cfg,
            seq,
            Some("cs2.exe"),
            0,
            vec![ev(cfg.anchor.qpc + seq * FIXTURE_FREQ / 1000, 1, 1, 0)],
        )
        .to_json()
        .unwrap()
    };
    let with = write_lines(
        dir.path(),
        "s-beats.jsonl",
        &[
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch(0),
            beat.clone(),
            beat,
            batch(1),
        ],
    );
    let without = write_lines(
        dir.path(),
        "s-plain.jsonl",
        &[
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch(0),
            batch(1),
        ],
    );

    let s = load_session(&with).unwrap();
    assert!(s.bad_lines.is_empty(), "{:?}", s.bad_lines);
    let plain = load_session(&without).unwrap();
    assert_eq!(s.events, plain.events);
    assert_eq!(s.batches.len(), plain.batches.len());
    assert_eq!(s.total_drops, plain.total_drops);

    // The cheap listing scan agrees.
    let scanned = scan_session(&with).unwrap();
    assert!(scanned.bad_lines.is_empty(), "{:?}", scanned.bad_lines);
    assert_eq!(scanned.events, scan_session(&without).unwrap().events);
}

/// A long parse error is capped before it reaches a warning, the report
/// JSON and the listing cache.
#[test]
fn the_kept_parse_error_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    // A type mismatch, so serde's message quotes the whole 4 KB value.
    let junk = format!(
        "{{\"type\":\"batch\",\"seq_no\":\"{}\",\"ts_anchor_us\":0,\"events\":[]}}",
        "z".repeat(4096)
    );
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[Envelope::Session(cfg).to_json().unwrap(), junk],
    );
    let s = load_session(&path).unwrap();
    let error = s.bad_lines.first_error.unwrap();
    assert!(
        error.chars().count() <= MAX_BAD_LINE_ERROR + 1,
        "{} chars",
        error.chars().count()
    );
}

#[test]
fn missing_header_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let path = write_lines(
        dir.path(),
        "bad.jsonl",
        &[batch_env(&cfg, 0, None, 0, vec![]).to_json().unwrap()],
    );
    assert!(matches!(
        load_session(&path),
        Err(LoadError::NoSessionHeader { .. })
    ));

    let empty = write_lines(dir.path(), "empty.jsonl", &[]);
    assert!(matches!(load_session(&empty), Err(LoadError::Empty { .. })));

    let missing = dir.path().join("nope.jsonl");
    assert!(matches!(load_session(&missing), Err(LoadError::Io { .. })));
}

/// A header whose clock or CPI cannot convert anything is refused by every
/// reader, and the listing skips that one recording rather than failing.
#[test]
fn an_unusable_session_header_is_refused_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut zero_freq = session_cfg();
    zero_freq.session_id = "s-zero".into();
    zero_freq.anchor.qpc_freq = 0;
    zero_freq.qpc_freq = 0;
    let mut no_cpi = session_cfg();
    no_cpi.session_id = "s-cpi".into();
    no_cpi.mouse_cpi = 0.0;
    let mut skewed = session_cfg();
    skewed.session_id = "s-skew".into();
    skewed.started_utc_us += 5;
    let good = session_cfg();
    for cfg in [&zero_freq, &no_cpi, &skewed, &good] {
        write_lines(
            dir.path(),
            &format!("{}.jsonl", cfg.session_id),
            &[Envelope::Session(cfg.clone()).to_json().unwrap()],
        );
    }
    for (id, want) in [
        ("s-zero", "qpc_freq is 0"),
        ("s-cpi", "mouse_cpi"),
        ("s-skew", "started_utc_us"),
    ] {
        let path = dir.path().join(format!("{id}.jsonl"));
        let err = load_session(&path).unwrap_err();
        assert!(
            matches!(err, LoadError::BadSessionHeader { .. }),
            "{id}: {err:?}"
        );
        assert!(
            error_chain(&err).contains(want),
            "{id}: {}",
            error_chain(&err)
        );
        assert!(matches!(
            scan_session(&path),
            Err(LoadError::BadSessionHeader { .. })
        ));
    }
    let listed = scan_dir(dir.path()).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|e| e.session_id.as_str())
            .collect::<Vec<_>>(),
        ["s-test"]
    );
}

#[test]
fn dominant_game_wins_on_event_count_and_falls_back_without_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let e = |i: u64| ev(cfg.anchor.qpc + i * 10_000, 1, 0, 0);
    let lines = vec![
        Envelope::Session(cfg.clone()).to_json().unwrap(),
        batch_env(&cfg, 0, Some("cs2.exe"), 0, vec![e(0)])
            .to_json()
            .unwrap(),
        batch_env(&cfg, 1, Some("valorant.exe"), 0, (1..5).map(e).collect())
            .to_json()
            .unwrap(),
    ];
    let path = write_lines(dir.path(), "s-test.jsonl", &lines);
    let s = load_session(&path).unwrap();
    assert_eq!(s.dominant_game().as_deref(), Some("valorant.exe"));
    let (name, share) = s.dominant_game_share().unwrap();
    assert_eq!(name, "valorant.exe");
    assert!((share - 0.8).abs() < 1e-9);
    let (_, sens, fallback) = s.resolve_aim();
    assert!(fallback, "valorant.exe has no profile in the fixture");
    assert_eq!(sens.sens, 1.0);
    assert_eq!(sens.yaw_coeff, 0.022);
}

#[test]
fn the_metadata_sidecar_puts_sink_losses_in_the_listing() {
    use std::collections::BTreeMap;
    use telemouse_core::recordings::{SessionMeta, SinkMeta};

    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[Envelope::Session(cfg.clone()).to_json().unwrap()],
    );
    // No sidecar yet: nothing to say.
    let first = scan_dir(dir.path()).unwrap();
    assert!(first[0].losses.is_empty());
    assert_eq!(first[0].exit, None);
    assert_eq!(first[0].losses_text(), "-");

    // The agent stops and writes the sidecar; the recording itself is
    // unchanged, so the cached entry is reused — and must still pick
    // the sidecar up.
    let mut sinks = BTreeMap::new();
    sinks.insert(
        "kafka".to_string(),
        SinkMeta {
            errors: 1,
            dropped: 397,
            abandoned: 3,
        },
    );
    sinks.insert("jsonl".to_string(), SinkMeta::default());
    let meta = SessionMeta {
        session_id: "s-test".into(),
        exit: "interrupt".into(),
        sinks,
        ..Default::default()
    };
    std::fs::write(
        dir.path().join("s-test.meta.json"),
        meta.to_json_pretty().unwrap(),
    )
    .unwrap();
    let second = scan_dir(dir.path()).unwrap();
    assert_eq!(second[0].losses, vec![("kafka".to_string(), 400)]);
    assert_eq!(second[0].exit.as_deref(), Some("interrupt"));
    assert_eq!(second[0].losses_text(), "kafka=400");
    assert_eq!(read_sidecar(&path).unwrap().session_id, "s-test");
    // The full load carries it too, read once.
    assert_eq!(load_session(&path).unwrap().sidecar, Some(meta));
    // The sidecar is never mistaken for a recording.
    assert_eq!(second.len(), 1);
}

#[test]
fn scan_dir_lists_only_session_ids_sorted_by_start() {
    let dir = tempfile::tempdir().unwrap();
    let a = crate::testutil::session_cfg_at("later", crate::testutil::FIXTURE_UTC0 + 60_000_000);
    let b = crate::testutil::session_cfg_at("earlier", crate::testutil::FIXTURE_UTC0);
    for cfg in [&a, &b] {
        write_lines(
            dir.path(),
            &format!("{}.jsonl", cfg.session_id),
            &[Envelope::Session(cfg.clone()).to_json().unwrap()],
        );
    }
    std::fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();
    // A well-formed recording under a name no other telemouse tool would
    // serve is not listed either.
    write_lines(
        dir.path(),
        "has space.jsonl",
        &[Envelope::Session(session_cfg()).to_json().unwrap()],
    );

    let list = scan_dir(dir.path()).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].session_id, "earlier");
    assert_eq!(list[1].session_id, "later");
    assert_eq!(list[0].events, 0);

    // Both renderings carry what the listing found.
    let text = render_list(&list, dir.path());
    assert!(
        text.contains("earlier") && text.contains("2 session(s)"),
        "{text}"
    );
    let json: serde_json::Value = serde_json::from_str(&list_json(&list).unwrap()).unwrap();
    assert_eq!(json[0]["schema"], LIST_SCHEMA);
    assert_eq!(json[1]["session_id"], "later");
    assert_eq!(
        render_list(&[], dir.path()),
        format!("no recordings in {}\n", dir.path().display())
    );
}

#[test]
fn scan_dir_cache_invalidates_when_a_recording_grows() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let first = batch_env(
        &cfg,
        0,
        Some("cs2.exe"),
        0,
        vec![ev(cfg.anchor.qpc, 1, 0, 0)],
    )
    .to_json()
    .unwrap();
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[Envelope::Session(cfg.clone()).to_json().unwrap(), first],
    );

    let initial = scan_dir(dir.path()).unwrap();
    assert_eq!(initial[0].events, 1);
    assert!(dir.path().join(INDEX_CACHE_FILE).is_file());

    let second = batch_env(
        &cfg,
        1,
        Some("valorant.exe"),
        2,
        vec![ev(cfg.anchor.qpc + FIXTURE_FREQ, 2, 0, 0)],
    )
    .to_json()
    .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{second}").unwrap();
    file.flush().unwrap();

    let changed = scan_dir(dir.path()).unwrap();
    assert_eq!(changed[0].events, 2);
    assert_eq!(changed[0].drops, 2);
    assert_eq!(changed[0].games, vec!["cs2.exe", "valorant.exe"]);

    // The replacement write succeeded and contains the refreshed entry,
    // rather than merely returning a correct uncached scan once.
    let cache: SessionIndexCache =
        serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap()).unwrap();
    assert_eq!(cache.files.len(), 1);
    assert_eq!(cache.files[0].entry.events, 2);
    assert_eq!(cache.files[0].entry.drops, 2);
    assert_eq!(
        cache.files[0].signature.len,
        std::fs::metadata(&path).unwrap().len()
    );

    let warm = scan_dir(dir.path()).unwrap();
    assert_eq!(warm, changed);
}

#[test]
fn signature_sampling_detects_a_same_size_tail_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let header = Envelope::Session(cfg.clone()).to_json().unwrap();
    let batch = |dx| {
        batch_env(&cfg, 0, None, 0, vec![ev(cfg.anchor.qpc, dx, 0, 0)])
            .to_json()
            .unwrap()
    };
    let path = write_lines(dir.path(), "s-test.jsonl", &[header.clone(), batch(1)]);
    let before = file_signature(&path).unwrap().unwrap();

    write_lines(dir.path(), "s-test.jsonl", &[header, batch(2)]);
    let after = file_signature(&path).unwrap().unwrap();

    assert_eq!(before.len, after.len);
    assert_ne!(before.sample_hash, after.sample_hash);
}

#[test]
fn scan_dir_invalidates_a_same_size_recording_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let header = Envelope::Session(cfg.clone()).to_json().unwrap();
    let batch = |game| {
        batch_env(&cfg, 0, Some(game), 0, vec![ev(cfg.anchor.qpc, 1, 0, 0)])
            .to_json()
            .unwrap()
    };
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[header.clone(), batch("game-a.exe")],
    );
    let initial = scan_dir(dir.path()).unwrap();
    assert_eq!(initial[0].games, vec!["game-a.exe"]);
    let signature_before = file_signature(&path).unwrap().unwrap();

    write_lines(dir.path(), "s-test.jsonl", &[header, batch("game-b.exe")]);
    let signature_after = file_signature(&path).unwrap().unwrap();
    assert_eq!(signature_before.len, signature_after.len);
    assert_ne!(signature_before.sample_hash, signature_after.sample_hash);

    let replaced = scan_dir(dir.path()).unwrap();
    assert_eq!(replaced[0].games, vec!["game-b.exe"]);
    let cache: SessionIndexCache =
        serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap()).unwrap();
    assert_eq!(cache.files[0].signature, signature_after);
    assert_eq!(cache.files[0].entry.games, vec!["game-b.exe"]);
}

#[test]
fn a_corrupt_index_cache_is_disposable() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    write_lines(
        dir.path(),
        "s-test.jsonl",
        &[Envelope::Session(cfg).to_json().unwrap()],
    );
    std::fs::write(dir.path().join(INDEX_CACHE_FILE), "not json").unwrap();

    let list = scan_dir(dir.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session_id, "s-test");
    let rebuilt: SessionIndexCache =
        serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap()).unwrap();
    assert_eq!(rebuilt.version, INDEX_CACHE_VERSION);
    assert_eq!(rebuilt.files.len(), 1);
}

#[test]
fn batch_metadata_survives_the_flattening() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let mut b = Batch {
        session_id: cfg.session_id.clone(),
        seq_no: 9,
        ts_anchor_us: cfg.started_utc_us,
        game: Some("cs2.exe".into()),
        pointer_locked: true,
        screen_w: 2560,
        screen_h: 1440,
        cursor_x: None,
        cursor_y: None,
        drops_since_last: 12,
        abs_frames_since_last: 4,
        events: vec![RawEvent::default()],
    };
    b.events[0].ts_qpc = cfg.anchor.qpc;
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[
            Envelope::Session(cfg).to_json().unwrap(),
            Envelope::Batch(b).to_json().unwrap(),
        ],
    );
    let s = load_session(&path).unwrap();
    let m = &s.batches[0];
    assert_eq!(m.seq_no, 9);
    assert!(m.pointer_locked);
    assert_eq!(m.drops_since_last, 12);
    assert_eq!(m.abs_frames_since_last, 4);
    assert_eq!(m.event_count, 1);
    assert_eq!(s.total_abs_frames, 4);
}

/// New wire fields must survive the borrow-based fast path, and old
/// recordings that predate them must still parse.
#[test]
fn new_and_legacy_event_fields_both_load() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_cfg();
    let mut e = RawEvent {
        ts_qpc: cfg.anchor.qpc,
        dx: 3,
        dy: -2,
        ..Default::default()
    };
    e.wheel_h = -120;
    e.device_ix = 2;
    let path = write_lines(
        dir.path(),
        "s-test.jsonl",
        &[
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch_env(&cfg, 0, Some("cs2.exe"), 0, vec![e])
                .to_json()
                .unwrap(),
            // A pre-device-tracking line, written by hand.
            format!(
                r#"{{"type":"batch","session_id":"s-test","seq_no":1,"ts_anchor_us":{},"pointer_locked":true,"screen_w":1,"screen_h":1,"drops_since_last":0,"events":[{{"ts_qpc":{},"dx":1,"dy":1,"buttons":0,"wheel":0}}]}}"#,
                cfg.started_utc_us,
                cfg.anchor.qpc + 1000
            ),
        ],
    );
    let s = load_session(&path).unwrap();
    assert!(s.bad_lines.is_empty());
    assert_eq!(s.events.len(), 2);
    assert_eq!(s.events[0].wheel_h, -120);
    assert_eq!(s.events[0].device_ix, 2);
    assert_eq!(s.events[1].device_ix, 0);
    assert_eq!(s.device_indices(), vec![0, 2]);
}
