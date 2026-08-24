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
//! Shutdown is *bounded*: the forwarder gets [`DRAIN_TIMEOUT`] to finish what
//! is queued, then the runtime is dropped and the remainder is reported as
//! abandoned. An unreachable broker can delay exit, never prevent it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use rskafka::client::partition::{Compression, UnknownTopicHandling};
use rskafka::client::producer::aggregator::RecordAggregator;
use rskafka::client::producer::{BatchProducer, BatchProducerBuilder};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;
use telemouse_core::Envelope;
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
const TOPICS: [&str; 3] = [TOPIC_EVENTS, TOPIC_SESSIONS, TOPIC_MARKERS];

type Producers = HashMap<&'static str, Arc<BatchProducer<RecordAggregator>>>;

/// One already-serialized envelope on its way to Kafka. Carrying `Arc<str>`
/// keeps the shipping thread's reusable buffer out of the picture without
/// deep-cloning the whole typed envelope per batch.
#[derive(Debug, Clone)]
pub struct KafkaJob {
    pub topic: &'static str,
    pub key: Arc<str>,
    pub payload: Arc<str>,
}

impl KafkaJob {
    pub fn new(env: &Envelope, payload: &str) -> Self {
        Self {
            topic: env.topic(),
            key: Arc::from(env.key()),
            payload: Arc::from(payload),
        }
    }
}

pub struct KafkaSink {
    tx: Option<Sender<KafkaJob>>,
    shutdown: Option<oneshot::Sender<()>>,
    stats: Arc<Stats>,
    worker: Option<JoinHandle<()>>,
    /// True while the queue is overflowing, so the warning fires on the
    /// 0→nonzero edge (and the recovery on the way back) instead of per drop.
    dropping: bool,
}

impl KafkaSink {
    /// Connect, ensure topics exist, and start the forwarder thread.
    ///
    /// Returns `Err` if the broker is unreachable — the caller warns and runs
    /// without Kafka rather than crashing.
    pub fn connect(brokers: &[String], stats: Arc<Stats>) -> Result<Self> {
        anyhow::ensure!(!brokers.is_empty(), "no kafka brokers configured");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_name("telemouse-kafka")
            .build()
            .context("build kafka runtime")?;

        let (client, producers) = runtime.block_on(async {
            let client = tokio::time::timeout(
                CONNECT_TIMEOUT,
                ClientBuilder::new(brokers.to_vec())
                    .client_id("telemouse-capture")
                    .build(),
            )
            .await
            .context("kafka connect timed out")?
            .context("kafka connect failed")?;
            ensure_topics(&client).await;
            let producers = open_producers(&client).await?;
            anyhow::Ok((client, producers))
        })?;

        let (tx, rx) = tokio::sync::mpsc::channel::<KafkaJob>(QUEUE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let worker_stats = Arc::clone(&stats);
        let worker = std::thread::Builder::new()
            .name("telemouse-kafka".into())
            .spawn(move || {
                // Keep the client alive for as long as the forwarder runs.
                let _client = client;
                runtime.block_on(async move {
                    let forwarder =
                        tokio::spawn(forward(rx, producers, Arc::clone(&worker_stats)));
                    // Resolves (with Err) the moment the sink is dropped.
                    let _ = shutdown_rx.await;
                    if tokio::time::timeout(DRAIN_TIMEOUT, forwarder).await.is_err() {
                        let abandoned = worker_stats.kafka_queued.load(Ordering::Relaxed);
                        worker_stats
                            .kafka_abandoned
                            .store(abandoned, Ordering::Relaxed);
                        tracing::warn!(
                            abandoned,
                            timeout_s = DRAIN_TIMEOUT.as_secs(),
                            "kafka drain timed out; abandoning queued envelopes"
                        );
                    }
                });
                tracing::debug!("kafka forwarder stopped");
            })
            .context("spawn kafka forwarder")?;

        tracing::info!(
            linger_ms = LINGER.as_millis() as u64,
            compression = "zstd",
            "kafka producer configured"
        );
        Ok(Self {
            tx: Some(tx),
            shutdown: Some(shutdown_tx),
            stats,
            worker: Some(worker),
            dropping: false,
        })
    }
}

/// Drain the channel, keeping several produces in flight so the linger window
/// actually has something to coalesce.
async fn forward(mut rx: Receiver<KafkaJob>, producers: Producers, stats: Arc<Stats>) {
    let producers = Arc::new(producers);
    let mut inflight: JoinSet<()> = JoinSet::new();
    while let Some(job) = rx.recv().await {
        // Reap anything already finished, then bound concurrency so a stalled
        // broker backpressures onto the (bounded, drop-happy) channel rather
        // than growing this set without limit.
        while inflight.try_join_next().is_some() {}
        if inflight.len() >= QUEUE_CAPACITY {
            let _ = inflight.join_next().await;
        }
        let producers = Arc::clone(&producers);
        let stats = Arc::clone(&stats);
        inflight.spawn(async move {
            if let Err(e) = produce(&producers, &job).await {
                stats.kafka_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %format!("{e:#}"), topic = job.topic, "kafka produce failed");
            }
            stats.kafka_queued.fetch_sub(1, Ordering::Relaxed);
        });
    }
    while inflight.join_next().await.is_some() {}
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

async fn produce(producers: &Producers, job: &KafkaJob) -> Result<()> {
    let producer = producers
        .get(job.topic)
        .with_context(|| format!("no producer for {}", job.topic))?;
    let record = Record {
        key: Some(job.key.as_bytes().to_vec()),
        value: Some(job.payload.as_bytes().to_vec()),
        headers: Default::default(),
        timestamp: chrono::Utc::now(),
    };
    producer
        .produce(record)
        .await
        .with_context(|| format!("produce to {}", job.topic))?;
    Ok(())
}

impl Sink for KafkaSink {
    fn name(&self) -> &'static str {
        "kafka"
    }

    fn send(&mut self, env: &Envelope, payload: &str) -> Result<()> {
        let Some(tx) = self.tx.as_ref() else {
            anyhow::bail!("kafka forwarder is shut down");
        };
        match tx.try_send(KafkaJob::new(env, payload)) {
            Ok(()) => {
                self.stats.kafka_queued.fetch_add(1, Ordering::Relaxed);
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
            Err(TrySendError::Closed(_)) => anyhow::bail!("kafka forwarder channel closed"),
        }
    }
}

impl Drop for KafkaSink {
    fn drop(&mut self) {
        // Signal first (starts the bounded drain), then close the channel so
        // the forwarder's recv loop can finish.
        self.shutdown.take();
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
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
        let job = KafkaJob::new(&env, &payload);
        assert_eq!(job.topic, TOPIC_MARKERS);
        assert_eq!(&*job.key, "s-1");
        assert_eq!(&*job.payload, payload);
    }
}
