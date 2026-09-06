//! Phase-5-lite local recorder: `recordings/<session_id>.jsonl`.
//!
//! The shipping thread only copies each serialized envelope into a bounded
//! channel. A dedicated writer owns the file, writes jobs in FIFO order, and
//! flushes at least once a second. Slow storage can therefore fill/drop from
//! this queue, but can never hold up UDP or later capture batches.
//!
//! Shutdown is bounded. Normally the worker drains every accepted line and
//! performs a final flush. If storage does not finish within [`DRAIN_TIMEOUT`],
//! the remaining count is exposed as `jsonl_abandoned` and shutdown proceeds.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::Sink;
use crate::stats::Stats;

/// Several seconds of normal 25--50ms batches, while keeping memory bounded.
const QUEUE_CAPACITY: usize = 256;
const WRITER_CAPACITY: usize = 64 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

pub struct JsonlSink {
    path: PathBuf,
    tx: Option<SyncSender<Vec<u8>>>,
    stats: Arc<Stats>,
    /// Gates enqueue and flush acknowledgement against the worker's terminal
    /// transition. This is what makes outstanding/loss accounting exact even
    /// when a shutdown timeout races a late writer failure.
    state: Arc<std::sync::Mutex<WriterState>>,
    worker: Option<JoinHandle<()>>,
    done: Receiver<()>,
    drain_timeout: Duration,
    /// Rate-limit queue-overflow logging to the transition into/out of a
    /// saturated period rather than logging for every dropped envelope.
    dropping: bool,
}

#[derive(Debug, Default)]
struct WriterState {
    failed: bool,
    /// Accepted envelopes that have not passed a successful explicit flush.
    outstanding: u64,
    /// Failure and timeout can race. Only the winner transfers `outstanding`
    /// into `jsonl_abandoned`; the late path observes this and does nothing.
    loss_claimed: bool,
    /// A channel disconnect can reveal a panic before Drop joins the worker.
    /// Whichever path observes it first emits the one causal error.
    failure_reported: bool,
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
        Self::spawn(
            path,
            Box::new(file),
            stats,
            QUEUE_CAPACITY,
            WRITER_CAPACITY,
            FLUSH_INTERVAL,
            DRAIN_TIMEOUT,
        )
    }

    fn spawn(
        path: PathBuf,
        writer: Box<dyn Write + Send>,
        stats: Arc<Stats>,
        queue_capacity: usize,
        writer_capacity: usize,
        flush_interval: Duration,
        drain_timeout: Duration,
    ) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(queue_capacity);
        let (done_tx, done) = std::sync::mpsc::channel();
        let state = Arc::new(std::sync::Mutex::new(WriterState::default()));
        let worker_state = Arc::clone(&state);
        let worker_stats = Arc::clone(&stats);
        let worker = std::thread::Builder::new()
            .name("telemouse-jsonl".into())
            .spawn(move || {
                let result = write_loop(
                    &rx,
                    BufWriter::with_capacity(writer_capacity, writer),
                    &worker_stats,
                    &worker_state,
                    flush_interval,
                );
                if let Err(e) = result {
                    let abandoned = claim_loss(&worker_state, &worker_stats);
                    if mark_failure_reported(&worker_state) {
                        worker_stats.jsonl_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            error = %format!("{e:#}"),
                            abandoned,
                            "jsonl writer failed"
                        );
                    }
                }
                // This is deliberately sent after the writer has been dropped:
                // observing completion means every accepted line was flushed,
                // or its failure was accounted for.
                let _ = done_tx.send(());
            })
            .context("spawn jsonl writer")?;
        Ok(Self {
            path,
            tx: Some(tx),
            stats,
            state,
            worker: Some(worker),
            done,
            drain_timeout,
            dropping: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn enqueue(&mut self, line: Vec<u8>) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            self.stats.jsonl_dropped.fetch_add(1, Ordering::Relaxed);
            // The worker logged the causal error. Treat later envelopes like
            // queue-overflow drops without rebuilding/formatting an error on
            // every capture batch.
            return Ok(());
        }
        let Some(tx) = self.tx.as_ref() else {
            anyhow::bail!("jsonl writer is shut down");
        };

        // Increment before publishing: the worker may consume immediately,
        // though completion is acknowledged only after an explicit flush.
        state.outstanding += 1;
        self.stats.jsonl_queued.fetch_add(1, Ordering::Relaxed);
        match tx.try_send(line) {
            Ok(()) => {
                if self.dropping {
                    self.dropping = false;
                    tracing::info!(
                        dropped_total = self.stats.jsonl_dropped.load(Ordering::Relaxed),
                        "jsonl queue recovered"
                    );
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                state.outstanding -= 1;
                self.stats.jsonl_queued.fetch_sub(1, Ordering::Relaxed);
                let total = self.stats.jsonl_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if !self.dropping {
                    self.dropping = true;
                    tracing::warn!(
                        queue_capacity = QUEUE_CAPACITY,
                        dropped_total = total,
                        "jsonl queue full; dropping envelopes until it drains"
                    );
                }
                // Saturation is already counted and edge-logged above. Keep it
                // off fan-out's allocating error path, just like Kafka full.
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                state.outstanding -= 1;
                state.failed = true;
                self.stats.jsonl_queued.fetch_sub(1, Ordering::Relaxed);
                self.stats.jsonl_dropped.fetch_add(1, Ordering::Relaxed);
                let abandoned = claim_loss_locked(&mut state, &self.stats);
                if !state.failure_reported {
                    state.failure_reported = true;
                    self.stats.jsonl_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(abandoned, "jsonl writer channel closed unexpectedly");
                }
                Ok(())
            }
        }
    }
}

fn write_loop(
    rx: &Receiver<Vec<u8>>,
    mut writer: BufWriter<Box<dyn Write + Send>>,
    stats: &Stats,
    state: &std::sync::Mutex<WriterState>,
    flush_interval: Duration,
) -> Result<()> {
    let mut last_flush = Instant::now();
    let mut pending_flush = 0u64;
    loop {
        let until_flush = flush_interval.saturating_sub(last_flush.elapsed());
        match rx.recv_timeout(until_flush) {
            Ok(line) => {
                writer.write_all(&line).context("write recording line")?;
                pending_flush += 1;
                // A perpetually non-empty queue makes recv_timeout(0) return a
                // job, not Timeout. Check the deadline after every write so
                // sustained load cannot starve durability flushes.
                if last_flush.elapsed() >= flush_interval {
                    flush_timed(&mut writer, stats)?;
                    acknowledge_flush(state, stats, &mut pending_flush);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = flush_timed(&mut writer, stats) {
                    return Err(e);
                }
                acknowledge_flush(state, stats, &mut pending_flush);
                last_flush = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Sender disconnection is the graceful finish signal. recv() cannot
    // return it until all accepted FIFO jobs have been consumed.
    flush_timed(&mut writer, stats)?;
    acknowledge_flush(state, stats, &mut pending_flush);
    Ok(())
}

fn acknowledge_flush(
    state: &std::sync::Mutex<WriterState>,
    stats: &Stats,
    pending_flush: &mut u64,
) {
    if *pending_flush == 0 {
        return;
    }
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.loss_claimed {
        debug_assert!(state.outstanding >= *pending_flush);
        state.outstanding -= *pending_flush;
        stats
            .jsonl_queued
            .fetch_sub(*pending_flush, Ordering::Relaxed);
    }
    *pending_flush = 0;
}

/// Atomically transfer this sink's unresolved envelopes into the abandonment
/// counter. The state mutex also excludes enqueue/flush acknowledgement, and
/// `loss_claimed` makes a timeout racing a late writer error exact-once.
fn claim_loss(state: &std::sync::Mutex<WriterState>, stats: &Stats) -> u64 {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    claim_loss_locked(&mut state, stats)
}

fn claim_loss_locked(state: &mut WriterState, stats: &Stats) -> u64 {
    state.failed = true;
    if state.loss_claimed {
        return 0;
    }
    state.loss_claimed = true;
    let abandoned = std::mem::take(&mut state.outstanding);
    stats.jsonl_queued.fetch_sub(abandoned, Ordering::Relaxed);
    stats
        .jsonl_abandoned
        .fetch_add(abandoned, Ordering::Relaxed);
    abandoned
}

fn mark_failure_reported(state: &std::sync::Mutex<WriterState>) -> bool {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    let first = !state.failure_reported;
    state.failure_reported = true;
    first
}

fn flush_timed(writer: &mut BufWriter<Box<dyn Write + Send>>, stats: &Stats) -> Result<()> {
    let started = Instant::now();
    let result = writer.flush();
    stats.observe_jsonl_flush(started.elapsed().as_micros() as u64);
    result.context("flush recording")
}

impl Sink for JsonlSink {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn send(&mut self, _topic: &'static str, _key: &str, payload: &str) -> Result<()> {
        // Avoid copying a line after a terminal writer failure. `enqueue`
        // repeats this check under the same mutex to close the transition race.
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).failed {
            self.stats.jsonl_dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let mut line = Vec::with_capacity(payload.len() + 1);
        line.extend_from_slice(payload.as_bytes());
        line.push(b'\n');
        self.enqueue(line)
    }

    // Flush scheduling belongs to the writer thread. In particular, a disk
    // flush must never migrate back onto shipping through this hook.
}

impl Drop for JsonlSink {
    fn drop(&mut self) {
        // Closing the last sender makes the worker drain and final-flush.
        self.tx.take();
        match self.done.recv_timeout(self.drain_timeout) {
            Ok(()) => {
                if let Some(worker) = self.worker.take()
                    && worker.join().is_err()
                {
                    self.stats.jsonl_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("jsonl writer panicked");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let abandoned = claim_loss(&self.state, &self.stats);
                tracing::warn!(
                    abandoned,
                    timeout_ms = self.drain_timeout.as_millis() as u64,
                    "jsonl drain timed out; abandoning queued envelopes"
                );
                // Dropping JoinHandle detaches. The process can finish even if
                // an OS-backed write remains stuck indefinitely.
                self.worker.take();
            }
            Err(RecvTimeoutError::Disconnected) => {
                let abandoned = claim_loss(&self.state, &self.stats);
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
                if mark_failure_reported(&self.state) {
                    self.stats.jsonl_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(abandoned, "jsonl writer panicked");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    use telemouse_core::{Batch, Envelope, Marker};

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

    fn send(sink: &mut JsonlSink, env: &Envelope) -> Result<()> {
        let json = env.to_json().unwrap();
        sink.send(env.topic(), env.key(), &json)
    }

    #[test]
    fn writes_one_envelope_per_line_in_fifo_order_and_flushes_on_drop() {
        let dir = std::env::temp_dir().join(format!("telemouse-jsonl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stats = Arc::new(Stats::default());
        let path = {
            let mut sink = JsonlSink::create(&dir, "s-test", Arc::clone(&stats)).unwrap();
            let p = sink.path().to_path_buf();
            send(&mut sink, &batch(0)).unwrap();
            send(
                &mut sink,
                &Envelope::Marker(Marker {
                    session_id: "s-test".into(),
                    seq_no: 0,
                    ts_qpc: 5,
                    ts_utc_us: 6,
                    label: "hotkey".into(),
                }),
            )
            .unwrap();
            send(&mut sink, &batch(1)).unwrap();
            p
        };
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""seq_no":0"#));
        assert!(lines[1].contains(r#""type":"marker""#));
        assert!(lines[2].contains(r#""seq_no":1"#));
        for line in lines {
            Envelope::from_json(line).expect("each line parses");
        }
        assert_eq!(stats.snapshot().jsonl_queued, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_flush_is_timed_on_the_worker() {
        let dir = std::env::temp_dir().join(format!("telemouse-jsonl-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::create(&dir, "s-t", Arc::clone(&stats)).unwrap();
        send(&mut sink, &batch(0)).unwrap();
        drop(sink);
        // A flush happened on shutdown; its duration may round to 0us, but the
        // queue reaching zero and readable data prove completion.
        assert_eq!(stats.snapshot().jsonl_queued, 0);
        assert!(dir.join("s-t.jsonl").metadata().unwrap().len() > 0);
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

    #[derive(Default)]
    struct Gate {
        entered: bool,
        release: bool,
    }

    struct BlockingWriter(Arc<(Mutex<Gate>, Condvar)>);

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, ready) = &*self.0;
            let mut gate = lock.lock().unwrap();
            gate.entered = true;
            ready.notify_all();
            while !gate.release {
                gate = ready.wait(gate).unwrap();
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn slow_storage_never_blocks_shipping_and_queue_overflow_is_counted() {
        let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::spawn(
            PathBuf::from("blocked.jsonl"),
            Box::new(BlockingWriter(Arc::clone(&gate))),
            Arc::clone(&stats),
            1,
            1,
            FLUSH_INTERVAL,
            DRAIN_TIMEOUT,
        )
        .unwrap();

        send(&mut sink, &batch(0)).unwrap();
        let (lock, ready) = &*gate;
        let mut state = lock.lock().unwrap();
        while !state.entered {
            state = ready.wait(state).unwrap();
        }
        drop(state);

        // The worker is stuck in write. One more job fits; the next is
        // rejected immediately instead of inheriting the storage latency.
        send(&mut sink, &batch(1)).unwrap();
        let started = Instant::now();
        send(&mut sink, &batch(2)).unwrap();
        let enqueue_elapsed = started.elapsed();
        eprintln!(
            "jsonl blocked-writer overflow returned in {}us",
            enqueue_elapsed.as_micros()
        );
        assert!(enqueue_elapsed < Duration::from_millis(100));
        assert_eq!(stats.snapshot().jsonl_dropped, 1);

        let mut state = lock.lock().unwrap();
        state.release = true;
        ready.notify_all();
        drop(state);
        drop(sink);
        assert_eq!(stats.snapshot().jsonl_queued, 0);
    }

    struct SlowCountingWriter {
        flushes: Arc<AtomicUsize>,
    }

    impl Write for SlowCountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(1));
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn sustained_backlog_cannot_starve_periodic_flushes() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let stats = Stats::default();
        let state = std::sync::Mutex::new(WriterState {
            outstanding: 50,
            ..Default::default()
        });
        let flushes = Arc::new(AtomicUsize::new(0));
        for _ in 0..50 {
            stats.jsonl_queued.fetch_add(1, Ordering::Relaxed);
            tx.send(vec![b'x', b'\n']).unwrap();
        }
        drop(tx);

        write_loop(
            &rx,
            BufWriter::with_capacity(
                1,
                Box::new(SlowCountingWriter {
                    flushes: Arc::clone(&flushes),
                }),
            ),
            &stats,
            &state,
            Duration::from_millis(5),
        )
        .unwrap();

        // At least one in-flight periodic flush plus the final flush. Without
        // the explicit post-write deadline check only the final flush occurs.
        assert!(flushes.load(Ordering::Relaxed) > 1);
        assert_eq!(stats.snapshot().jsonl_queued, 0);
    }

    struct FlushErrorWriter;

    impl Write for FlushErrorWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }

    #[test]
    fn final_flush_failure_accounts_every_accepted_envelope_exactly_once() {
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::spawn(
            PathBuf::from("flush-error.jsonl"),
            Box::new(FlushErrorWriter),
            Arc::clone(&stats),
            8,
            64 * 1024,
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .unwrap();
        for seq in 0..3 {
            send(&mut sink, &batch(seq)).unwrap();
        }
        drop(sink);

        let snap = stats.snapshot();
        eprintln!(
            "jsonl flush failure: queued={} abandoned={} errors={}",
            snap.jsonl_queued, snap.jsonl_abandoned, snap.jsonl_errors
        );
        assert_eq!(snap.jsonl_queued, 0);
        assert_eq!(snap.jsonl_abandoned, 3);
        assert_eq!(snap.jsonl_errors, 1);
    }

    struct WriteErrorWriter;

    impl Write for WriteErrorWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("write failed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_writer_failure_drops_later_envelopes_quietly() {
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::spawn(
            PathBuf::from("write-error.jsonl"),
            Box::new(WriteErrorWriter),
            Arc::clone(&stats),
            8,
            1,
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .unwrap();
        send(&mut sink, &batch(0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !sink.state.lock().unwrap_or_else(|e| e.into_inner()).failed {
            assert!(
                Instant::now() < deadline,
                "writer did not enter failed state"
            );
            std::thread::yield_now();
        }

        for seq in 1..=100 {
            send(&mut sink, &batch(seq)).unwrap();
        }
        drop(sink);
        let snap = stats.snapshot();
        assert_eq!(snap.jsonl_queued, 0);
        assert_eq!(snap.jsonl_abandoned, 1);
        assert_eq!(snap.jsonl_dropped, 100);
        assert_eq!(snap.jsonl_errors, 1);
    }

    #[derive(Default)]
    struct FlushGate {
        entered: bool,
        release: bool,
    }

    struct BlockingFailFlush(Arc<(Mutex<FlushGate>, Condvar)>);

    impl Write for BlockingFailFlush {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let (lock, ready) = &*self.0;
            let mut gate = lock.lock().unwrap();
            gate.entered = true;
            ready.notify_all();
            while !gate.release {
                gate = ready.wait(gate).unwrap();
            }
            Err(std::io::Error::other("late flush failure"))
        }
    }

    #[test]
    fn timeout_and_late_failure_claim_outstanding_loss_only_once() {
        let gate = Arc::new((Mutex::new(FlushGate::default()), Condvar::new()));
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::spawn(
            PathBuf::from("blocked-flush.jsonl"),
            Box::new(BlockingFailFlush(Arc::clone(&gate))),
            Arc::clone(&stats),
            8,
            64 * 1024,
            Duration::from_secs(60),
            Duration::from_millis(10),
        )
        .unwrap();
        send(&mut sink, &batch(0)).unwrap();
        let dropper = std::thread::spawn(move || drop(sink));

        let (lock, ready) = &*gate;
        let mut state = lock.lock().unwrap();
        while !state.entered {
            state = ready.wait(state).unwrap();
        }
        drop(state);
        dropper.join().unwrap();
        let at_timeout = stats.snapshot();
        assert_eq!(at_timeout.jsonl_queued, 0);
        assert_eq!(at_timeout.jsonl_abandoned, 1);

        let mut state = lock.lock().unwrap();
        state.release = true;
        ready.notify_all();
        drop(state);
        let deadline = Instant::now() + Duration::from_secs(1);
        while stats.snapshot().jsonl_errors == 0 {
            assert!(
                Instant::now() < deadline,
                "late writer failure was not observed"
            );
            std::thread::yield_now();
        }
        let final_snap = stats.snapshot();
        eprintln!(
            "jsonl timeout race: queued={} abandoned={} errors={}",
            final_snap.jsonl_queued, final_snap.jsonl_abandoned, final_snap.jsonl_errors
        );
        assert_eq!(final_snap.jsonl_queued, 0);
        assert_eq!(final_snap.jsonl_abandoned, 1);
        assert_eq!(final_snap.jsonl_errors, 1);
    }

    struct PanicWriter;

    impl Write for PanicWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            panic!("writer panic")
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn worker_panic_is_not_misreported_as_a_timeout() {
        let stats = Arc::new(Stats::default());
        let mut sink = JsonlSink::spawn(
            PathBuf::from("panic.jsonl"),
            Box::new(PanicWriter),
            Arc::clone(&stats),
            8,
            1,
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .unwrap();
        send(&mut sink, &batch(0)).unwrap();
        drop(sink);
        let snap = stats.snapshot();
        assert_eq!(snap.jsonl_queued, 0);
        assert_eq!(snap.jsonl_abandoned, 1);
        assert_eq!(snap.jsonl_errors, 1);
    }
}
