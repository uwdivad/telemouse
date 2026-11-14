//! Kafka sink built on `rskafka` (pure Rust — no librdkafka build step).
//!
//! `rskafka` is async and the shipping thread is sync, so the sink is a bounded
//! channel in front of a dedicated forwarder thread running a current-thread
//! tokio runtime. If the channel backs up the envelope is dropped and counted:
//! Kafka must never stall shipping (the plan's "viz latency never gated on
//! Kafka" rule, applied to capture as well).
//!
//! Produces go through rskafka's [`BatchProducer`] with a fixed [`LINGER`]
//! and zstd compression. The linger is deliberately *not* the batch window:
//! a producer that lingers one window sends one RPC per batch, which is no
//! batching at all. At [`LINGER`] a topic gets at most one produce RPC per
//! linger period (four a second, each carrying ~10 batches at the default
//! 25ms window) instead of forty. Kafka is the archive, not the live path, so
//! a quarter of a second of extra delay costs nothing a consumer can see.
//! Coalescing only works if several produce calls are in flight at once — the
//! forwarder therefore spawns each job onto a [`JoinSet`] instead of awaiting
//! them one at a time. An open batch is also sent early, without waiting for
//! the linger, when [`MAX_INFLIGHT`] produce calls are waiting on it and when
//! the sink is closing.
//!
//! Broker connection, topic discovery, and producer setup also happen on that
//! worker. Constructing this sink therefore never delays capture startup.
//!
//! Shutdown is *bounded*: initialization is cancellable and the forwarder gets
//! its share of the shutdown budget (never more than [`DRAIN_TIMEOUT`]) to
//! finish what is queued, then the runtime is dropped and the remainder is
//! reported as abandoned. The tail does not sit out the linger first: once
//! the queue is closed the open batches are flushed at once. An unreachable
//! broker can delay neither capture startup nor exit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rskafka::client::partition::{Compression, UnknownTopicHandling};
use rskafka::client::producer::aggregator::RecordAggregator;
use rskafka::client::producer::{BatchProducer, BatchProducerBuilder, ProducerClient};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;
use telemouse_core::wire::{TOPIC_EVENTS, TOPIC_MARKERS, TOPIC_SESSIONS};
// `hotpath::wrap` types so the queue can be profiled: plain tokio re-exports
// unless `--features hotpath` (https://hotpath.rs/data_flow).
use hotpath::wrap::tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::queue::{QueueAccounting, TrySend};
use super::{Sink, SinkKind};
use crate::stats::{Stats, Throttle};

/// Envelopes buffered towards Kafka before we start dropping. ~3s of batches.
const QUEUE_CAPACITY: usize = 128;
/// How long a topic's producer collects records before one produce RPC.
/// Independent of `batch.window_ms` on purpose (see the module docs): long
/// enough that an RPC carries several batches, short next to anything a
/// Kafka consumer of this stream cares about.
const LINGER: Duration = Duration::from_millis(250);
/// Produce calls in flight at once. Past this the forwarder sends the open
/// batches and waits for one call to finish, so a stalled broker backs up
/// onto the (bounded, drop-happy) channel instead of growing the in-flight
/// set without limit — and a stream faster than `MAX_INFLIGHT` envelopes per
/// [`LINGER`] (a 1ms batch window) is flushed by count instead of backing up
/// behind the linger.
const MAX_INFLIGHT: usize = 128;
/// While the forwarder is waiting on in-flight produce calls it has decided
/// not to linger for (the set is full, or the sink is closing), how often it
/// re-flushes the open batches. One pass is normally enough; the repeat
/// covers a call that had not reached its producer yet when the first ran.
const FLUSH_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Kafka's default `max.message.bytes` is 1MiB; stay comfortably under it.
const MAX_AGGREGATED_BYTES: usize = 900_000;
/// The most a dropped sink waits for the forwarder to finish, whatever share
/// of the shutdown budget it was given.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Do not let runtime-owned I/O resources turn the bounded drain into an
/// unbounded `Runtime::drop` wait.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);
/// A failing produce warns at most this often (CONVENTIONS: anything that
/// stays wrong is repeated once a minute).
const PRODUCE_WARN_INTERVAL: Duration = Duration::from_secs(60);
const TOPICS: [&str; 3] = [TOPIC_EVENTS, TOPIC_SESSIONS, TOPIC_MARKERS];
/// Topics the agent creates itself get one partition (the producers only
/// write partition 0), a replication factor of one (a desk broker), and a
/// few seconds for the controller to answer.
const TOPIC_PARTITIONS: i32 = 1;
const TOPIC_REPLICATION: i16 = 1;
const TOPIC_CREATE_TIMEOUT_MS: i32 = 5_000;

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
    /// Sent (or dropped) to start the bounded drain; carries this sink's
    /// share of the shutdown budget when [`Sink::set_drain_deadline`] set one.
    shutdown: Option<oneshot::Sender<Option<Instant>>>,
    /// Serializes enqueue/completion against terminal failure and shutdown
    /// accounting. This keeps aggregate counters exact when late async jobs
    /// finish after the bounded drain has already claimed them.
    queue: Arc<QueueAccounting>,
    worker: Option<JoinHandle<()>>,
    drain_deadline: Option<Instant>,
}

impl KafkaSink {
    /// Start a forwarder thread. The broker connection and topic setup happen
    /// on that thread; this returns as soon as its bounded channel exists.
    pub fn connect(brokers: &[String], stats: Arc<Stats>) -> Result<Self> {
        anyhow::ensure!(!brokers.is_empty(), "no kafka brokers configured");
        let (tx, rx) = hotpath::channel!(
            tokio::sync::mpsc::channel::<KafkaJob>(QUEUE_CAPACITY),
            label = "kafka_queue"
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<Option<Instant>>();
        let queue = Arc::new(new_queue(stats));
        let worker_queue = Arc::clone(&queue);
        let brokers = brokers.to_vec();
        let worker = std::thread::Builder::new()
            .name("telemouse-kafka".into())
            .spawn(move || {
                run_worker(brokers, rx, shutdown_rx, worker_queue);
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
            queue,
            worker: Some(worker),
            drain_deadline: None,
        })
    }
}

fn new_queue(stats: Arc<Stats>) -> QueueAccounting {
    QueueAccounting::new(SinkKind::Kafka, QUEUE_CAPACITY, stats, Stats::kafka_queue)
}

/// How long the drain may run: what is left of `deadline` when one was
/// given, never more than [`DRAIN_TIMEOUT`].
fn drain_timeout(deadline: Option<Instant>) -> Duration {
    deadline.map_or(DRAIN_TIMEOUT, |d| {
        d.saturating_duration_since(Instant::now())
            .min(DRAIN_TIMEOUT)
    })
}

fn run_worker(
    brokers: Vec<String>,
    rx: Receiver<KafkaJob>,
    mut shutdown: oneshot::Receiver<Option<Instant>>,
    queue: Arc<QueueAccounting>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("telemouse-kafka")
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            queue.count_error();
            tracing::error!(error = %e, "build kafka runtime failed");
            queue.claim_loss();
            return;
        }
    };

    runtime.block_on(async move {
        // Bound the whole initialization, including topic setup and partition
        // discovery. It is also cancellable so shutdown never waits for the
        // connection timeout.
        let initialized = tokio::select! {
            _ = &mut shutdown => {
                queue.claim_loss();
                return;
            }
            result = tokio::time::timeout(CONNECT_TIMEOUT, initialize(brokers)) => result,
        };

        let (client, producers) = match initialized {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => {
                queue.count_error();
                tracing::warn!(error = %format!("{e:#}"), "kafka unavailable; forwarder stopped");
                queue.claim_loss();
                return;
            }
            Err(_) => {
                queue.count_error();
                tracing::warn!(
                    timeout_s = CONNECT_TIMEOUT.as_secs(),
                    "kafka initialization timed out; forwarder stopped"
                );
                queue.claim_loss();
                return;
            }
        };

        tracing::info!("kafka producer configured");
        // Keep the client alive for as long as its partition producers run.
        let _client = client;
        let mut forwarder = tokio::spawn(forward(rx, producers, Arc::clone(&queue)));
        // Resolves the moment the sink is dropped: with its deadline, or
        // with an error if it had none to send.
        let timeout = drain_timeout(shutdown.await.ok().flatten());
        match tokio::time::timeout(timeout, &mut forwarder).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let first_failure = queue.mark_failed();
                let abandoned = queue.claim_loss();
                if first_failure {
                    queue.count_error();
                }
                tracing::error!(error = %e, abandoned, "kafka forwarder panicked");
            }
            Err(_) => {
                let abandoned = queue.claim_loss();
                tracing::warn!(
                    abandoned,
                    timeout_ms = timeout.as_millis() as u64,
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

/// Send every topic's open batch now instead of when its linger runs out,
/// and wait for those writes. A failed write is reported to the produce calls
/// that were waiting on it, which is where it is counted and logged.
async fn flush_open_batches(producers: &Producers) {
    for (topic, producer) in producers {
        if let Err(e) = producer.flush().await {
            tracing::debug!(topic, error = %e, "kafka flush failed");
        }
    }
}

/// Drain the channel, keeping several produces in flight so the linger window
/// actually has something to coalesce.
async fn forward(mut rx: Receiver<KafkaJob>, producers: Producers, queue: Arc<QueueAccounting>) {
    let producers = Arc::new(producers);
    let mut inflight: JoinSet<()> = JoinSet::new();
    let produce_warn = Arc::new(Mutex::new(Throttle::new(PRODUCE_WARN_INTERVAL)));
    while let Some(job) = rx.recv().await {
        // Reap anything already finished, then bound concurrency so a stalled
        // broker backpressures onto the (bounded, drop-happy) channel rather
        // than growing this set without limit.
        while let Some(result) = inflight.try_join_next() {
            observe_join(result, &queue);
        }
        if inflight.len() >= MAX_INFLIGHT {
            // On a healthy broker these calls are lingering, not stuck: send
            // what they are waiting on rather than sitting the linger out
            // with the channel filling behind us. On a stalled one this
            // waits, as a plain join always did.
            settle_one(&mut inflight, &producers, &queue).await;
        }
        let producers = Arc::clone(&producers);
        let queue = Arc::clone(&queue);
        let produce_warn = Arc::clone(&produce_warn);
        inflight.spawn(async move {
            let topic = job.topic;
            match produce(&producers, job).await {
                Ok(()) => queue.finish(1, true),
                Err(e) => {
                    queue.count_error();
                    let allowed = produce_warn
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .allow(Instant::now());
                    if let Some(suppressed) = allowed {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            topic,
                            suppressed,
                            "kafka produce failed"
                        );
                    }
                    queue.finish(1, false);
                }
            }
        });
    }
    // The queue is closed, so nothing more can join the open batches and
    // waiting out the linger would only spend the shutdown budget. Flush
    // until every call has settled; the caller bounds the whole drain.
    while !inflight.is_empty() {
        settle_one(&mut inflight, &producers, &queue).await;
    }
}

/// Wait for one in-flight produce call to finish, sending the open batches
/// rather than leaving them to their linger. A call that has already
/// finished is taken first, so a healthy drain is one flush and then joins.
/// The flush repeats every [`FLUSH_RETRY_INTERVAL`] because a freshly spawned
/// call may not have reached its producer when the first one ran.
async fn settle_one(inflight: &mut JoinSet<()>, producers: &Producers, queue: &QueueAccounting) {
    let mut flush_tick = tokio::time::interval(FLUSH_RETRY_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            joined = inflight.join_next() => {
                if let Some(result) = joined {
                    observe_join(result, queue);
                }
                return;
            }
            _ = flush_tick.tick() => flush_open_batches(producers).await,
        }
    }
}

fn observe_join(result: std::result::Result<(), tokio::task::JoinError>, queue: &QueueAccounting) {
    if let Err(e) = result {
        queue.count_error();
        queue.finish(1, false);
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
        match controller
            .create_topic(
                topic,
                TOPIC_PARTITIONS,
                TOPIC_REPLICATION,
                TOPIC_CREATE_TIMEOUT_MS,
            )
            .await
        {
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
        map.insert(topic, Arc::new(batch_producer(Arc::new(pc), LINGER)));
    }
    Ok(map)
}

/// One topic's producer over `client`: the partition client in production, a
/// stand-in in tests (which also pick their own linger).
fn batch_producer(
    client: Arc<dyn ProducerClient>,
    linger: Duration,
) -> BatchProducer<RecordAggregator> {
    BatchProducerBuilder::new_with_client(client)
        .with_linger(linger)
        .with_compression(Compression::Zstd)
        .build(RecordAggregator::new(MAX_AGGREGATED_BYTES))
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
    fn kind(&self) -> SinkKind {
        SinkKind::Kafka
    }

    #[hotpath::measure(impl_type = "KafkaSink")]
    fn send(&mut self, topic: &'static str, key: &str, payload: &str) -> Result<()> {
        // After a terminal failure the worker has already logged the cause;
        // staying on the successful Sink path avoids allocating and
        // formatting the same error for every later capture batch.
        if self.queue.is_failed() {
            self.queue.count_dropped();
            return Ok(());
        }
        let Some(tx) = self.tx.as_ref() else {
            anyhow::bail!("kafka forwarder is shut down");
        };
        // Counted as outstanding before publishing (the worker may consume at
        // once), under the lock that closes the race with a terminal loss
        // claim. Backed up: drop and count. Never block the shipping loop.
        self.queue
            .enqueue(|| match tx.try_send(KafkaJob::new(topic, key, payload)) {
                Ok(()) => TrySend::Sent,
                Err(TrySendError::Full(_)) => TrySend::Full,
                Err(TrySendError::Closed(_)) => TrySend::Closed,
            });
        Ok(())
    }

    fn drains_on_close(&self) -> bool {
        true
    }

    fn set_drain_deadline(&mut self, deadline: Instant) {
        self.drain_deadline = Some(deadline);
    }
}

impl Drop for KafkaSink {
    fn drop(&mut self) {
        // Signal first (starts the bounded drain), then close the channel so
        // the forwarder's recv loop can finish.
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(self.drain_deadline);
        }
        self.tx.take();
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            let first_failure = self.queue.mark_failed();
            let abandoned = self.queue.claim_loss();
            if first_failure {
                self.queue.count_error();
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
    use std::sync::atomic::Ordering;

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

    /// Stands in for a partition client: accepts every write and remembers
    /// how many records each produce RPC carried.
    #[derive(Debug, Default)]
    struct FakeBroker {
        rpcs: Mutex<Vec<usize>>,
    }

    impl FakeBroker {
        fn rpcs(&self) -> Vec<usize> {
            self.rpcs.lock().unwrap().clone()
        }
    }

    type ProduceFuture<'a> = std::pin::Pin<
        Box<
            dyn Future<Output = std::result::Result<Vec<i64>, rskafka::client::error::Error>>
                + Send
                + 'a,
        >,
    >;

    impl ProducerClient for FakeBroker {
        fn produce(&self, records: Vec<Record>, _compression: Compression) -> ProduceFuture<'_> {
            let n = records.len();
            self.rpcs.lock().unwrap().push(n);
            Box::pin(async move { Ok((0..n as i64).collect()) })
        }
    }

    /// Producers for every topic over one fake broker, with `linger`.
    fn fake_producers(broker: &Arc<FakeBroker>, linger: Duration) -> Producers {
        TOPICS
            .into_iter()
            .map(|topic| {
                let client: Arc<dyn ProducerClient> = broker.clone();
                (topic, Arc::new(batch_producer(client, linger)))
            })
            .collect()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Long enough that a test which waited for it would time out instead.
    const NEVER: Duration = Duration::from_secs(3600);

    fn sink_with(queue: QueueAccounting, worker: Option<JoinHandle<()>>) -> KafkaSink {
        KafkaSink {
            tx: None,
            shutdown: None,
            queue: Arc::new(queue),
            worker,
            drain_deadline: None,
        }
    }

    #[test]
    fn connect_without_brokers_is_an_error_not_a_panic() {
        let stats = Arc::new(Stats::default());
        assert!(KafkaSink::connect(&[], stats).is_err());
    }

    /// K1: the linger used to be the batch window, i.e. one RPC per batch.
    #[test]
    fn the_linger_is_long_enough_to_coalesce_and_short_enough_to_drain() {
        let default_window = Duration::from_millis(25);
        assert!(LINGER >= default_window * 8, "several batches per RPC");
        // The tail is flushed on close rather than lingered, but keep the
        // linger well inside the drain cap anyway.
        assert!(LINGER * 4 <= DRAIN_TIMEOUT);
    }

    /// Ten batches handed over inside one linger period are one produce RPC.
    #[test]
    fn batches_inside_one_linger_share_a_produce_rpc() {
        let stats = Arc::new(Stats::default());
        let queue = Arc::new(new_queue(Arc::clone(&stats)));
        queue.preload(10);
        let broker = Arc::new(FakeBroker::default());
        let producers = fake_producers(&broker, LINGER);
        let (tx, rx) = hotpath::channel!(tokio::sync::mpsc::channel::<KafkaJob>(16));
        runtime().block_on(async {
            let forwarder = tokio::spawn(forward(rx, producers, Arc::clone(&queue)));
            for _ in 0..10 {
                tx.send(KafkaJob::new(TOPIC_EVENTS, "s-1", "{}"))
                    .await
                    .unwrap();
            }
            // The queue stays open: only the linger can send these.
            let waiting = Instant::now();
            while stats.snapshot().kafka_queued > 0 {
                assert!(waiting.elapsed() < Duration::from_secs(5), "never produced");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                waiting.elapsed() >= LINGER / 2,
                "sent after {:?}, without lingering",
                waiting.elapsed()
            );
            drop(tx);
            forwarder.await.unwrap();
        });
        assert_eq!(broker.rpcs(), vec![10]);
        assert_eq!(stats.snapshot().kafka_abandoned, 0);
    }

    /// A clean stop must not spend its drain budget waiting for the linger:
    /// here the linger never ends, and the tail is still delivered at once.
    #[test]
    fn closing_the_queue_flushes_the_tail_without_waiting_for_the_linger() {
        let stats = Arc::new(Stats::default());
        let queue = Arc::new(new_queue(Arc::clone(&stats)));
        queue.preload(5);
        let broker = Arc::new(FakeBroker::default());
        let producers = fake_producers(&broker, NEVER);
        let (tx, rx) = hotpath::channel!(tokio::sync::mpsc::channel::<KafkaJob>(16));
        runtime().block_on(async {
            for topic in [
                TOPIC_SESSIONS,
                TOPIC_EVENTS,
                TOPIC_EVENTS,
                TOPIC_MARKERS,
                TOPIC_EVENTS,
            ] {
                tx.send(KafkaJob::new(topic, "s-1", "{}")).await.unwrap();
            }
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), forward(rx, producers, queue))
                .await
                .expect("the drain waited on the linger");
        });
        assert_eq!(broker.rpcs().iter().sum::<usize>(), 5);
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 0);
        assert_eq!(snap.kafka_errors, 0);
    }

    /// A stream faster than `MAX_INFLIGHT` per linger is sent by count, not
    /// left to back up onto the drop-happy channel behind the linger.
    #[test]
    fn a_full_inflight_set_is_flushed_instead_of_lingering() {
        const JOBS: usize = MAX_INFLIGHT * 2;
        let stats = Arc::new(Stats::default());
        let queue = Arc::new(new_queue(Arc::clone(&stats)));
        queue.preload(JOBS as u64);
        let broker = Arc::new(FakeBroker::default());
        let producers = fake_producers(&broker, NEVER);
        let (tx, rx) = hotpath::channel!(tokio::sync::mpsc::channel::<KafkaJob>(JOBS));
        runtime().block_on(async {
            let forwarder = tokio::spawn(forward(rx, producers, Arc::clone(&queue)));
            for _ in 0..JOBS {
                tx.send(KafkaJob::new(TOPIC_EVENTS, "s-1", "{}"))
                    .await
                    .unwrap();
            }
            // Still open, and the linger never ends: only the cap sends.
            let waiting = Instant::now();
            while broker.rpcs().iter().sum::<usize>() < MAX_INFLIGHT {
                assert!(waiting.elapsed() < Duration::from_secs(5), "never flushed");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop(tx);
            forwarder.await.unwrap();
        });
        assert_eq!(broker.rpcs().iter().sum::<usize>(), JOBS);
        assert_eq!(stats.snapshot().kafka_queued, 0);
    }

    #[test]
    fn the_drain_never_outlives_its_share_or_its_cap() {
        assert_eq!(drain_timeout(None), DRAIN_TIMEOUT);
        assert_eq!(
            drain_timeout(Some(Instant::now() - Duration::from_secs(1))),
            Duration::ZERO
        );
        assert!(
            drain_timeout(Some(Instant::now() + Duration::from_millis(500)))
                <= Duration::from_millis(500)
        );
        assert_eq!(
            drain_timeout(Some(Instant::now() + Duration::from_secs(60))),
            DRAIN_TIMEOUT
        );
    }

    #[test]
    fn every_envelope_variant_has_a_producer_slot() {
        // The forwarder looks producers up by `Envelope::topic()`; if core ever
        // adds a topic this test catches the missing entry.
        for topic in [TOPIC_EVENTS, TOPIC_SESSIONS, TOPIC_MARKERS] {
            assert!(TOPICS.contains(&topic));
        }
        // ...with one deliberate exception. Heartbeats are liveness for the
        // live viz; the shipping loop sends them to the udp sink alone and
        // never fans one out, so no producer is opened for them and a
        // message a second of "still here" never reaches the broker.
        assert!(
            !TOPICS.contains(&telemouse_core::wire::TOPIC_HEARTBEATS),
            "heartbeats are live-only and must not gain a Kafka producer"
        );
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
        let started = Instant::now();
        let sink = KafkaSink::connect(&["203.0.113.1:9092".into()], stats).unwrap();
        // Before initialization moved to the worker this call could consume
        // the full 5s connection timeout before capture even started. The
        // bounds are generous on purpose: what matters is "not the connect
        // timeout", not a benchmark on a loaded CI box.
        assert!(started.elapsed() < Duration::from_secs(2));
        let dropping = Instant::now();
        drop(sink);
        assert!(dropping.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn terminal_initialization_failure_drops_later_jobs_without_error_churn() {
        let stats = Arc::new(Stats::default());
        stats.kafka_errors.store(1, Ordering::Relaxed);
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel::<KafkaJob>(1));
        let mut sink = sink_with(new_queue(Arc::clone(&stats)), None);
        sink.tx = Some(tx);
        assert!(sink.queue.mark_failed());

        const SENDS: u64 = 10_000;
        for _ in 0..SENDS {
            sink.send(TOPIC_EVENTS, "s-1", "{}").unwrap();
        }
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_errors, 1, "no error churn after the cause");
        assert_eq!(snap.kafka_dropped, SENDS);
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 0);
    }

    /// The forwarder going away claims what it had accepted, exactly as the
    /// JSONL writer's does — before the shared accounting, Kafka left those
    /// envelopes "queued" forever.
    #[test]
    fn a_closed_forwarder_channel_claims_the_loss() {
        let stats = Arc::new(Stats::default());
        let (tx, rx) = hotpath::channel!(tokio::sync::mpsc::channel::<KafkaJob>(4));
        let mut sink = sink_with(new_queue(Arc::clone(&stats)), None);
        sink.tx = Some(tx);
        sink.send(TOPIC_EVENTS, "s-1", "{}").unwrap();
        drop(rx);
        sink.send(TOPIC_EVENTS, "s-1", "{}").unwrap();
        sink.send(TOPIC_EVENTS, "s-1", "{}").unwrap();
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 1);
        assert_eq!(snap.kafka_dropped, 2);
        assert_eq!(snap.kafka_errors, 1);
    }

    #[test]
    fn terminal_loss_claim_is_exact_once_and_late_completion_is_harmless() {
        let stats = Arc::new(Stats::default());
        let queue = new_queue(Arc::clone(&stats));
        queue.preload(3);

        assert_eq!(queue.claim_loss(), 3);
        assert_eq!(queue.claim_loss(), 0);
        // Models an async produce completing after the bounded drain claimed it.
        queue.finish(1, true);
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 3);
    }

    #[test]
    fn accepted_produce_failure_is_counted_as_abandoned() {
        let stats = Arc::new(Stats::default());
        let queue = new_queue(Arc::clone(&stats));
        queue.preload(1);

        queue.finish(1, false);
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 1);
    }

    #[test]
    fn panicked_produce_task_is_accounted() {
        let stats = Arc::new(Stats::default());
        let queue = new_queue(Arc::clone(&stats));
        queue.preload(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let task = tokio::spawn(async { panic!("produce panic") });
            observe_join(task.await, &queue);
        });

        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 1);
        assert_eq!(snap.kafka_errors, 1);
    }

    #[test]
    fn panicked_worker_thread_claims_all_outstanding_jobs() {
        let stats = Arc::new(Stats::default());
        let queue = new_queue(Arc::clone(&stats));
        queue.preload(2);
        let worker = std::thread::spawn(|| panic!("worker panic"));
        let sink = sink_with(queue, Some(worker));
        drop(sink);

        let snap = stats.snapshot();
        assert_eq!(snap.kafka_queued, 0);
        assert_eq!(snap.kafka_abandoned, 2);
        assert_eq!(snap.kafka_errors, 1);
    }
}
