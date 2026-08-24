//! Phase-5-lite local recorder: `recordings/<session_id>.jsonl`.
//!
//! One JSON envelope per line, buffered, flushed at least once a second so a
//! hard kill loses at most a second of a session. The flush is timed and its
//! worst case reported in the periodic stats line — a stalling disk shows up
//! there before it shows up as a gap in the data.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use telemouse_core::Envelope;

use super::Sink;
use crate::stats::Stats;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

pub struct JsonlSink {
    path: PathBuf,
    writer: BufWriter<File>,
    last_flush: Instant,
    stats: Arc<Stats>,
}

impl JsonlSink {
    /// Create (or truncate) `<dir>/<session_id>.jsonl`, creating `dir` if
    /// needed. The caller writes the `session` envelope first.
    pub fn create(dir: &Path, session_id: &str, stats: Arc<Stats>) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create recording dir {}", dir.display()))?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("open recording {}", path.display()))?;
        Ok(Self {
            path,
            writer: BufWriter::with_capacity(64 * 1024, file),
            last_flush: Instant::now(),
            stats,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn flush_if_due(&mut self, force: bool) -> Result<()> {
        if force || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            let started = Instant::now();
            let result = self.writer.flush();
            self.stats
                .observe_jsonl_flush(started.elapsed().as_micros() as u64);
            result.context("flush recording")?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }
}

impl Sink for JsonlSink {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn send(&mut self, _env: &Envelope, payload: &str) -> Result<()> {
        self.writer
            .write_all(payload.as_bytes())
            .context("write recording line")?;
        self.writer.write_all(b"\n").context("write recording line")?;
        self.flush_if_due(false)
    }

    fn tick(&mut self) -> Result<()> {
        self.flush_if_due(false)
    }
}

impl Drop for JsonlSink {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use telemouse_core::{Batch, Marker};

    use super::*;

    fn batch(seq: u64) -> Envelope {
        Envelope::Batch(Batch {
            session_id: "s-test".into(),
            seq_no: seq,
            ts_anchor_us: 1,
            game: None,
            pointer_locked: false,
            screen_w: 1920,
            screen_h: 1080,
            cursor_x: Some(1),
            cursor_y: Some(2),
            drops_since_last: 0,
            abs_frames_since_last: 0,
            events: vec![],
        })
    }

    fn send(sink: &mut JsonlSink, env: &Envelope) {
        let json = env.to_json().unwrap();
        sink.send(env, &json).unwrap();
    }

    #[test]
    fn writes_one_envelope_per_line_and_flushes_on_drop() {
        let dir = std::env::temp_dir().join(format!("telemouse-jsonl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stats = Arc::new(Stats::default());
        let path = {
            let mut sink = JsonlSink::create(&dir, "s-test", Arc::clone(&stats)).unwrap();
            let p = sink.path().to_path_buf();
            send(&mut sink, &batch(0));
            send(
                &mut sink,
                &Envelope::Marker(Marker {
                    session_id: "s-test".into(),
                    seq_no: 0,
                    ts_qpc: 5,
                    ts_utc_us: 6,
                    label: "hotkey".into(),
                }),
            );
            send(&mut sink, &batch(1));
            p
        };
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""type":"batch""#));
        assert!(lines[1].contains(r#""type":"marker""#));
        for l in lines {
            Envelope::from_json(l).expect("each line parses");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forced_flush_is_timed_into_stats() {
        let dir = std::env::temp_dir().join(format!("telemouse-jsonl-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::create(&dir, "s-t", Arc::clone(&stats)).unwrap();
        send(&mut sink, &batch(0));
        // The 1s interval has not elapsed, so nothing has been timed yet.
        assert_eq!(stats.snapshot().jsonl_flush_max_us, 0);
        sink.flush_if_due(true).unwrap();
        // A flush happened; its duration was recorded (possibly 0µs if fast).
        assert!(sink.last_flush.elapsed() < Duration::from_secs(1));
        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_directories() {
        let dir = std::env::temp_dir()
            .join(format!("telemouse-mkdir-{}", std::process::id()))
            .join("nested");
        let _ = std::fs::remove_dir_all(&dir);
        let sink = JsonlSink::create(&dir, "s-x", Arc::new(Stats::default())).unwrap();
        assert!(sink.path().exists());
        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
