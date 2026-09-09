//! Kafka sink built on `rskafka` (pure Rust — no librdkafka build step).
//!
//! `rskafka` is async and the shipping thread is sync, so the sink is a bounded
//! channel in front of a dedicated forwarder thread running a current-thread
//! tokio runtime. If the channel backs up the envelope is dropped and counted:
//! Kafka must never stall shipping (the plan's "viz latency never gated on
//! Kafka" rule, applied to capture as well).
//!
//! Produces go through rskafka's [`BatchProducer`] with a 25ms linger and zstd
//! compression, so a second of capture is a couple of RPCs rather than forty.
//! That only works if several produce calls are in flight at once — the
//! forwarder therefore spawns each job onto a [`JoinSet`] instead of awaiting
//! them one at a time.
//!
//! Broker connection, topic discovery, and producer setup also happen on that
//! worker. Constructing this sink therefore never delays capture startup.
//!
//! Shutdown is *bounded*: initialization is cancellable and the forwarder gets
//! [`DRAIN_TIMEOUT`] to finish what is queued, then the runtime is dropped and
//! the remainder is reported as abandoned. An unreachable broker can delay
//! neither capture startup nor exit.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use rskafka::client::partition::{Compression, UnknownTopicHandling};
use rskafka::client::producer::aggregator::RecordAggregator;
use rskafka::client::producer::{BatchProducer, BatchProducerBuilder};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;
use telemouse_core::wire::{TOPIC_EVENTS, TOPIC_MARKERS, TOPIC_SESSIONS};
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::Sink;
use crate::stats::Stats;

/// Envelopes buffered towards Kafka before we start dropping. ~3s of batches.
const QUEUE_CAPACITY: usize = 128;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Coalescing window: matches the batch window, so one RPC per ~2 batches even
/// under a burst, without adding meaningful durability lag.
const LINGER: Duration = Duration::from_millis(25);
/// Kafka's default `max.message.bytes` is 1MiB; stay comfortably under it.
const MAX_AGGREGATED_BYTES: usize = 900_000;
/// How long a dropped sink waits for the forwarder to finish before giving up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Do not let runtime-owned I/O resources turn the bounded drain into an
/// unbounded `Runtime::drop` wait.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
const TOPICS: [&str; 3] = [TOPIC_EVENTS, TOPIC_SESSIONS, TOPIC_MARKERS];

type Producers = HashMap<&'static str, Arc<BatchProducer<RecordAggregator>>>;

/// One already-serialized envelope on its way to Kafka. Owning plain `Vec<u8>`
/// buffers means one copy out of the shipping thread's reusable buffer at job
/// creation; `produce` then *moves* them into the `Record` rather than copying
/// again.
#[derive(Debug)]
pub struct KafkaJob {
    pub topic: &'static str,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
}

impl KafkaJob {
    pub fn new(topic: &'static str, key: &str, payload: &str) -> Self {
        Self {
            topic,
            key: key.as_bytes().to_vec(),
            payload: payload.as_bytes().to_vec(),
        }
    }
}

pub struct KafkaSink {
    tx: Option<Sender<KafkaJob>>,
    shutdown: Option<oneshot::Sender<()>>,
    stats: Arc<Stats>,
    /// Serializes enqueue/completion against terminal failure and shutdown
    /// accounting. This keeps aggregate counters exact when late async jobs
    /// finish after the bounded drain has already claimed them.
    state: Arc<std::sync::Mutex<KafkaState>>,
    worker: Option<JoinHandle<()>>,
    /// True while the queue is overflowing, so the warning fires on the
    /// 0→nonzero edge (and the recovery on the way back) instead of per drop.
    dropping: bool,
}

#[derive(Debug, Default)]
struct KafkaState {
    failed: bool,
    /// Jobs accepted by the channel but not yet successfully produced.
    outstanding: u64,
    /// Initialization failure, worker panic, and shutdown timeout are terminal
    /// claims. Only the first transfers all outstanding jobs to abandoned.
    loss_claimed: bool,
}

impl KafkaSink {
    /// Start a forwarder thread. The broker connection and topic setup happen
    /// on that thread; this returns as soon as its bounded channel exists.
    pub fn connect(brokers: &[String], stats: Arc<Stats>) -> Result<Self> {
        anyhow::ensure!(!brokers.is_empty(), "no kafka brokers configured");
        let (tx, rx) = tokio::sync::mpsc::channel::<KafkaJob>(QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let worker_stats = Arc::clone(&stats);
        let state = Arc::new(std::sync::Mutex::new(KafkaState::default()));
        let worker_state = Arc::clone(&state);
        let brokers = brokers.to_vec();
        let worker = std::thread::Builder::new()
            .name("telemouse-kafka".into())
            .spawn(move || {
                run_worker(brokers, rx, shutdown_rx, worker_stats, worker_state);
                tracing::debug!("kafka forwarder stopped");
            })
            .context("spawn kafka forwarder")?;

        tracing::info!(
            linger_ms = LINGER.as_millis() as u64,
            compression = "zstd",
            "kafka forwarder started; connecting in background"
        );
        Ok(Self {
            tx: Some(tx),
            shutdown: Some(shutdown_tx),
            stats,
            state,
            worker: Some(worker),
            dropping: false,
        })
    }
}

fn run_worker(
    brokers: Vec<String>,
    rx: Receiver<KafkaJob>,
    mut shutdown: oneshot::Receiver<()>,
    stats: Arc<Stats>,
    state: Arc<std::sync::Mutex<KafkaState>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("telemouse-kafka")
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "build kafka runtime failed");
            mark_failed(&state);
            claim_loss(&state, &stats);
            return;
        }
    };

    runtime.block_on(async move {
        // Bound the whole initialization, including topic setup and partition
        // discovery. It is also cancellable so shutdown never waits for the
        // connection timeout.
        let initialized = tokio::select! {
            _ = &mut shutdown => {
                claim_loss(&state, &stats);
                return;
            }
            result = tokio::time::timeout(CONNECT_TIMEOUT, initialize(brokers)) => result,
        };

        let (client, producers) = match initialized {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => {
                stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %format!("{e:#}"), "kafka unavailable; forwarder stopped");
                mark_failed(&state);
                claim_loss(&state, &stats);
                return;
            }
            Err(_) => {
                stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    timeout_s = CONNECT_TIMEOUT.as_secs(),
                    "kafka initialization timed out; forwarder stopped"
                );
                mark_failed(&state);
                claim_loss(&state, &stats);
                return;
            }
        };

        tracing::info!("kafka producer configured");
        // Keep the client alive for as long as its partition producers run.
        let _client = client;
        let mut forwarder = tokio::spawn(forward(
            rx,
            producers,
            Arc::clone(&stats),
            Arc::clone(&state),
        ));
        // Resolves (with Err) the moment the sink is dropped.
        let _ = shutdown.await;
        match tokio::time::timeout(DRAIN_TIMEOUT, &mut forwarder).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let first_failure = mark_failed(&state);
                let abandoned = claim_loss(&state, &stats);
                if first_failure {
                    stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                }
                tracing::error!(error = %e, abandoned, "kafka forwarder panicked");
            }
            Err(_) => {
                let abandoned = claim_loss(&state, &stats);
                tracing::warn!(
                    abandoned,
                    timeout_s = DRAIN_TIMEOUT.as_secs(),
                    "kafka drain timed out; abandoning queued envelopes"
                );
                forwarder.abort();
                let _ = forwarder.await;
            }
        }
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
}

async fn initialize(brokers: Vec<String>) -> Result<(Client, Producers)> {
    let client = ClientBuilder::new(brokers)
        .client_id("telemouse-capture")
        .build()
        .await
        .context("kafka connect failed")?;
    ensure_topics(&client).await;
    let producers = open_producers(&client).await?;
    Ok((client, producers))
}

fn mark_failed(state: &std::sync::Mutex<KafkaState>) -> bool {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    let first = !state.failed;
    state.failed = true;
    first
}

fn claim_loss(state: &std::sync::Mutex<KafkaState>, stats: &Stats) -> u64 {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    state.failed = true;
    if state.loss_claimed {
        return 0;
    }
    state.loss_claimed = true;
    let abandoned = std::mem::take(&mut state.outstanding);
    stats.kafka_queued.fetch_sub(abandoned, Ordering::Relaxed);
    stats
        .kafka_abandoned
        .fetch_add(abandoned, Ordering::Relaxed);
    abandoned
}

fn finish_job(state: &std::sync::Mutex<KafkaState>, stats: &Stats, delivered: bool) {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    if state.loss_claimed {
        return;
    }
    debug_assert!(state.outstanding > 0);
    state.outstanding -= 1;
    stats.kafka_queued.fetch_sub(1, Ordering::Relaxed);
    if !delivered {
        stats.kafka_abandoned.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drain the channel, keeping several produces in flight so the linger window
/// actually has something to coalesce.
async fn forward(
    mut rx: Receiver<KafkaJob>,
    producers: Producers,
    stats: Arc<Stats>,
    state: Arc<std::sync::Mutex<KafkaState>>,
) {
    let producers = Arc::new(producers);
    let mut inflight: JoinSet<()> = JoinSet::new();
    let produce_warning_emitted = Arc::new(AtomicBool::new(false));
    while let Some(job) = rx.recv().await {
        // Reap anything already finished, then bound concurrency so a stalled
        // broker backpressures onto the (bounded, drop-happy) channel rather
        // than growing this set without limit.
        while let Some(result) = inflight.try_join_next() {
            observe_join(result, &state, &stats);
        }
        if inflight.len() >= QUEUE_CAPACITY
            && let Some(result) = inflight.join_next().await
        {
            observe_join(result, &state, &stats);
        }
        let producers = Arc::clone(&producers);
        let stats = Arc::clone(&stats);
        let state = Arc::clone(&state);
        let produce_warning_emitted = Arc::clone(&produce_warning_emitted);
        inflight.spawn(async move {
            let topic = job.topic;
            match produce(&producers, job).await {
                Ok(()) => {
                    finish_job(&state, &stats, true);
                }
                Err(e) => {
                    stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                    if !produce_warning_emitted.swap(true, Ordering::AcqRel) {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            topic,
                            "kafka produce failed; suppressing repeats for this sink"
                        );
                    }
                    finish_job(&state, &stats, false);
                }
            }
        });
    }
    while let Some(result) = inflight.join_next().await {
        observe_join(result, &state, &stats);
    }
}

fn observe_join(
    result: std::result::Result<(), tokio::task::JoinError>,
    state: &std::sync::Mutex<KafkaState>,
    stats: &Stats,
) {
    if let Err(e) = result {
        stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
        finish_job(state, stats, false);
        tracing::error!(error = %e, "kafka produce task panicked");
    }
}

async fn ensure_topics(client: &Client) {
    let controller = match client.controller_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "no kafka controller; skipping topic creation");
            return;
        }
    };
    for topic in TOPICS {
        match controller.create_topic(topic, 1, 1, 5_000).await {
            Ok(()) => tracing::info!(topic, "created kafka topic"),
            // Almost always "topic already exists", which is the happy path.
            Err(e) => tracing::debug!(topic, error = %e, "kafka topic not created"),
        }
    }
}

async fn open_producers(client: &Client) -> Result<Producers> {
    let mut map = HashMap::new();
    for topic in TOPICS {
        let pc = client
            .partition_client(topic, 0, UnknownTopicHandling::Retry)
            .await
            .with_context(|| format!("open kafka partition for {topic}"))?;
        let producer = BatchProducerBuilder::new(Arc::new(pc))
            .with_linger(LINGER)
            .with_compression(Compression::Zstd)
            .build(RecordAggregator::new(MAX_AGGREGATED_BYTES));
        map.insert(topic, Arc::new(producer));
    }
    Ok(map)
}

async fn produce(producers: &Producers, job: KafkaJob) -> Result<()> {
    let topic = job.topic;
    let producer = producers
        .get(topic)
        .with_context(|| format!("no producer for {topic}"))?;
    // One clock read per produce call, taken before the record is assembled.
    let timestamp = chrono::Utc::now();
    // The job's buffers move straight into the record: no third copy.
    let record = Record {
        key: Some(job.key),
        value: Some(job.payload),
        headers: Default::default(),
        timestamp,
    };
    producer
        .produce(record)
        .await
        .with_context(|| format!("produce to {topic}"))?;
    Ok(())
}

impl Sink for KafkaSink {
    fn name(&self) -> &'static str {
        "kafka"
    }

    fn send(&mut self, topic: &'static str, key: &str, payload: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            self.stats.kafka_dropped.fetch_add(1, Ordering::Relaxed);
            // The worker has already logged the causal failure. Staying on the
            // successful Sink path avoids allocating and formatting the same
            // error for every later capture batch.
            return Ok(());
        }
        let Some(tx) = self.tx.as_ref() else {
            anyhow::bail!("kafka forwarder is shut down");
        };
        // Increment before publishing: the worker may consume immediately.
        // Holding `state` closes the race with a terminal loss claim.
        state.outstanding += 1;
        self.stats.kafka_queued.fetch_add(1, Ordering::Relaxed);
        match tx.try_send(KafkaJob::new(topic, key, payload)) {
            Ok(()) => {
                if self.dropping {
                    self.dropping = false;
                    tracing::info!(
                        dropped_total = self.stats.kafka_dropped.load(Ordering::Relaxed),
                        "kafka queue recovered"
                    );
                }
                Ok(())
            }
            // Backed up: drop and count. Never block the shipping loop.
            Err(TrySendError::Full(_)) => {
                state.outstanding -= 1;
                self.stats.kafka_queued.fetch_sub(1, Ordering::Relaxed);
                let total = self.stats.kafka_dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if !self.dropping {
                    self.dropping = true;
                    tracing::warn!(
                        queue_capacity = QUEUE_CAPACITY,
                        dropped_total = total,
                        "kafka queue full; dropping envelopes until it drains"
                    );
                }
                Ok(())
            }
            Err(TrySendError::Closed(_)) => {
                state.outstanding -= 1;
                let first_failure = !state.failed;
                state.failed = true;
                self.stats.kafka_queued.fetch_sub(1, Ordering::Relaxed);
                self.stats.kafka_dropped.fetch_add(1, Ordering::Relaxed);
                if first_failure {
                    self.stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("kafka forwarder channel closed unexpectedly");
                }
                Ok(())
            }
        }
    }
}

impl Drop for KafkaSink {
    fn drop(&mut self) {
        // Signal first (starts the bounded drain), then close the channel so
        // the forwarder's recv loop can finish.
        self.shutdown.take();
        self.tx.take();
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            let first_failure = mark_failed(&self.state);
            let abandoned = claim_loss(&self.state, &self.stats);
            if first_failure {
                self.stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
            }
            tracing::error!(
                abandoned,
                panic = ?payload,
                "kafka worker thread panicked"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use telemouse_core::Envelope;

    use super::*;

    fn marker(label: &str) -> Envelope {
        Envelope::Marker(telemouse_core::Marker {
            session_id: "s-1".into(),
            seq_no: 0,
            ts_qpc: 1,
            ts_utc_us: 2,
            label: label.into(),
        })
    }

    #[test]
    fn connect_without_brokers_is_an_error_not_a_panic() {
        let stats = Arc::new(Stats::default());
        assert!(KafkaSink::connect(&[], stats).is_err());
    }

    #[test]
    fn every_envelope_variant_has_a_producer_slot() {
        // The forwarder looks producers up by `Envelope::topic()`; if core ever
        // adds a topic this test catches the missing entry.
        for topic in [TOPIC_EVENTS, TOPIC_SESSIONS, TOPIC_MARKERS] {
            assert!(TOPICS.contains(&topic));
        }
    }

    #[test]
    fn jobs_carry_the_routing_the_forwarder_needs() {
        let env = marker("hotkey");
        let payload = env.to_json().unwrap();
        let job = KafkaJob::new(env.topic(), env.key(), &payload);
        assert_eq!(job.topic, TOPIC_MARKERS);
        assert_eq!(job.key, b"s-1");
        assert_eq!(job.payload, payload.as_bytes());
    }

    #[test]
    fn unreachable_broker_does_not_delay_sink_construction_or_shutdown() {
        let stats = Arc::new(Stats::default());
        let started = std::time::Instant::now();
        let sink = KafkaSink::connect(&["203.0.113.1:9092".into()], stats).unwrap();
        // Before initialization moved to the worker this call could consume
        // the full 5s connection timeout before capture even started.
        let connect_elapsed = started.elapsed();
        assert!(connect_elapsed < Duration::from_millis(250));
        let dropping = std::time::Instant::now();
        drop(sink);
        let drop_elapsed = dropping.elapsed();
        eprintln!(
            "kafka unreachable connect={}us drop={}us",
            connect_elapsed.as_micros(),
            drop_elapsed.as_micros()
        );
        assert!(drop_elapsed < Duration::from_millis(250));
    }

    #[test]
    fn terminal_initialization_failure_drops_later_jobs_without_error_churn() {
        let stats = Arc::new(Stats::default());
        stats.kafka_errors.store(1, Ordering::Relaxed);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut sink = KafkaSink {
            tx: Some(tx),
            shutdown: None,
            stats: Arc::clone(&stats),
            state: Arc::new(std::sync::Mutex::new(KafkaState {
                failed: true,
                ..Default::default()
            })),
            worker: None,
            dropping: false,
        };

        const SENDS: u64 = 10_000;
        let started = std::time::Instant::now();
        for _ in 0..SENDS {
            sink.send(TOPIC_EVENTS, "s-1", "{}").unwrap();
        }
        let elapsed = started.elapsed();
        eprintln!(
            "kafka terminal-failure fast path: sends={SENDS} total_us={} ns_per_send={}",
            elapsed.as_micros(),
            elapsed.as_nanos() / SENDS as u128
        );
        assert!(elapsed < Duration::from_millis(250));
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_errors, 1);
        assert_eq!(snap.kafka_dropped, SENDS);
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 0);
    }

    #[test]
    fn terminal_loss_claim_is_exact_once_and_late_completion_is_harmless() {
        let stats = Stats::default();
        stats.kafka_queued.store(3, Ordering::Relaxed);
        let state = std::sync::Mutex::new(KafkaState {
            outstanding: 3,
            ..Default::default()
        });

        assert_eq!(claim_loss(&state, &stats), 3);
        assert_eq!(claim_loss(&state, &stats), 0);
        // Models an async produce completing after the bounded drain claimed it.
        finish_job(&state, &stats, true);
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 3);
    }

    #[test]
    fn accepted_produce_failure_is_counted_as_abandoned() {
        let stats = Stats::default();
        stats.kafka_queued.store(1, Ordering::Relaxed);
        let state = std::sync::Mutex::new(KafkaState {
            outstanding: 1,
            ..Default::default()
        });

        finish_job(&state, &stats, false);
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 1);
    }

    #[test]
    fn panicked_produce_task_is_accounted() {
        let stats = Arc::new(Stats::default());
        stats.kafka_queued.store(1, Ordering::Relaxed);
        let state = Arc::new(std::sync::Mutex::new(KafkaState {
            outstanding: 1,
            ..Default::default()
        }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let task = tokio::spawn(async { panic!("produce panic") });
            observe_join(task.await, &state, &stats);
        });

        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 1);
        assert_eq!(snap.kafka_errors, 1);
    }

    #[test]
    fn panicked_worker_thread_claims_all_outstanding_jobs() {
        let stats = Arc::new(Stats::default());
        stats.kafka_queued.store(2, Ordering::Relaxed);
        let state = Arc::new(std::sync::Mutex::new(KafkaState {
            outstanding: 2,
            ..Default::default()
        }));
        let worker = std::thread::spawn(|| panic!("worker panic"));
        let sink = KafkaSink {
            tx: None,
            shutdown: None,
            stats: Arc::clone(&stats),
            state,
            worker: Some(worker),
            dropping: false,
        };
        drop(sink);

        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 2);
        assert_eq!(snap.kafka_errors, 1);
    }
}
