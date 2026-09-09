# Handoff: local Kafka for telemouse (2026-09-03)

Purpose: start a fresh Claude Code session on another machine and get a local
Kafka broker receiving telemouse data there. Paste the "Prompt for the new
session" at the bottom into Claude Code after the setup steps.

## What telemouse is (30 seconds)

Rust workspace, five crates. `crates/capture` (bin `telemouse`) reads raw
mouse input on Windows, batches it every 25ms (`batch.window_ms`), and ships each batch as one
JSON `Envelope` to three sinks: localhost UDP (for `telemouse-viz`), a JSONL
recording, and Kafka. `crates/core` is the shared contract (wire format,
config). `crates/ctl` is a control panel on 127.0.0.1:7880. Conventions live
in `docs/CONVENTIONS.md`; the long tour is `docs/GUIDE.md`.

The Kafka sink already exists and is complete: `crates/capture/src/sinks/kafka.rs`,
built on `rskafka` (pure Rust, no librdkafka). It is a bounded queue in front
of a forwarder thread, zstd + 25ms linger, drops and counts rather than ever
blocking capture, bounded 3s drain on shutdown. Topics: `mouse.events`,
`mouse.sessions`, `mouse.markers`, all keyed by session id. Capture creates
them on connect. Do not swap the Kafka client and do not add a metrics server
(metrics are tracing log lines by convention).

## What was done on the first machine (uncommitted as of writing)

- `compose.yaml` at the repo root: single-node KRaft `apache/kafka:3.9.1`,
  bound to `127.0.0.1:9092`, data in the `kafka-data` volume, healthcheck.
- `telemouse.toml`: `[kafka] enabled = true` (was false).
- README, `docs/GUIDE.md`, `CHANGELOG.md` updated to describe the broker.
- Verified end to end: `telemouse doctor` reports the broker reachable; an
  8s run produced 30 batches + 1 session record with zero errors; offsets
  confirmed with `kafka-get-offsets.sh`. Full workspace test suite passes.

If those changes have been pushed by the time you read this, `git pull` and
skip straight to "Bring up the broker". If not, recreate `compose.yaml` from
the block below and flip the config switch by hand.

Kafka 3.9 was chosen over 4.x deliberately: Kafka 4 removed old protocol
versions and rskafka 0.6 has not been tested against it. Stay on 3.9.x.

## Prerequisites on the new machine

- Rust stable (edition 2024 workspace, so a 2025+ toolchain).
- Docker with Compose v2 (`docker compose version`). Linux engine is fine on
  Windows (Docker Desktop / WSL2 backend).
- Windows is only needed for `telemouse run` (actual capture). `doctor`,
  viz, analyze, and the tests build anywhere.
- Java is NOT needed with Docker. Only the no-Docker path needs Java 17.

## Bring up the broker

Create `compose.yaml` in the repo root if it is not there:

```yaml
services:
  kafka:
    image: apache/kafka:3.9.1
    container_name: telemouse-kafka
    ports:
      - "127.0.0.1:9092:9092"
    environment:
      KAFKA_NODE_ID: 1
      KAFKA_PROCESS_ROLES: broker,controller
      KAFKA_CONTROLLER_QUORUM_VOTERS: 1@localhost:9093
      KAFKA_CONTROLLER_LISTENER_NAMES: CONTROLLER
      KAFKA_LISTENERS: PLAINTEXT://0.0.0.0:9092,CONTROLLER://0.0.0.0:9093
      KAFKA_ADVERTISED_LISTENERS: PLAINTEXT://127.0.0.1:9092
      KAFKA_LISTENER_SECURITY_PROTOCOL_MAP: CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT
      KAFKA_INTER_BROKER_LISTENER_NAME: PLAINTEXT
      KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR: 1
      KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR: 1
      KAFKA_TRANSACTION_STATE_LOG_MIN_ISR: 1
      KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS: 0
      KAFKA_AUTO_CREATE_TOPICS_ENABLE: "true"
      KAFKA_LOG_RETENTION_HOURS: 168
      KAFKA_LOG_DIRS: /var/lib/kafka/data
    volumes:
      - kafka-data:/var/lib/kafka/data
    healthcheck:
      test: ["CMD-SHELL", "/opt/kafka/bin/kafka-broker-api-versions.sh --bootstrap-server localhost:9092 >/dev/null 2>&1"]
      interval: 5s
      timeout: 10s
      retries: 12
      start_period: 10s

volumes:
  kafka-data:
```

Then:

```powershell
docker compose up -d
docker inspect --format '{{.State.Health.Status}}' telemouse-kafka   # "healthy" within ~30s
```

In `telemouse.toml`, under `[kafka]`, set `enabled = true` (brokers already
default to `127.0.0.1:9092`).

## Verify

```powershell
cargo build --release -p telemouse-capture
.\target\release\telemouse.exe doctor          # kafka: enabled / broker 127.0.0.1:9092: reachable
.\target\release\telemouse.exe run --duration-secs 8 --no-record
```

Expected log lines: `kafka sink starting`, `created kafka topic` x3,
`kafka producer configured`, and a final `session finished` line with
`kafka_errors=0 kafka_dropped=0 kafka_abandoned=0`. Move the mouse during
the run so `mouse.events` gets batches. Then:

```powershell
docker exec telemouse-kafka /opt/kafka/bin/kafka-get-offsets.sh --bootstrap-server localhost:9092 --topic mouse.events
docker exec telemouse-kafka /opt/kafka/bin/kafka-get-offsets.sh --bootstrap-server localhost:9092 --topic mouse.sessions
# peek at a payload
docker exec telemouse-kafka /opt/kafka/bin/kafka-console-consumer.sh --bootstrap-server localhost:9092 --topic mouse.events --from-beginning --max-messages 1 --timeout-ms 10000 --property print.key=true
```

`mouse.sessions` should be at offset 1 after one run; `mouse.events` at the
batch count from the `session finished` line.

## No-Docker fallback

Download the Kafka 3.9.1 binary tarball, needs Java 17:

```bash
KAFKA_CLUSTER_ID=$(bin/kafka-storage.sh random-uuid)
bin/kafka-storage.sh format -t $KAFKA_CLUSTER_ID -c config/kraft/server.properties
bin/kafka-server-start.sh config/kraft/server.properties
```

On Windows prefer WSL2 over the `bin\windows\*.bat` scripts.

## Sharing one broker between two machines (optional)

On the machine hosting the broker, change the port binding to
`"0.0.0.0:9092:9092"` and `KAFKA_ADVERTISED_LISTENERS` to
`PLAINTEXT://<that machine's LAN IP>:9092`. Point the other machine's
`[kafka] brokers` at that IP. Clients connect to the advertised address, so
leaving it at 127.0.0.1 breaks remote producers even with the port open.
The container has no auth; keep it on a trusted LAN.

## Gotchas known from the first machine

- Claude Code's Bash tool (msys) was broken on the first box; PowerShell
  worked. Check which works on the new one before fighting it.
- The repo must stay rustfmt-clean at default settings; CI runs
  `cargo fmt --check`, `clippy -D warnings`, and tests on windows-latest.
- Embedded viz pages are `include_str!`, so a running server keeps old HTML
  until rebuilt.
- Do not touch the live capture path without reading `docs/AUDIT-2026-08.md`
  and `docs/PERFORMANCE-2026-09.md`; the batch defaults (window 25 / coalesce
  8) favor live responsiveness while retaining the low-cost input cadence.
  Use window 50 when minimum per-batch CPU matters more than latency.

## Prompt for the new session

```
Read docs/HANDOFF-2026-09-03-kafka.md first. Goal: get a local Kafka broker
running on this machine and confirm telemouse capture is producing to it.
Follow the "Bring up the broker" and "Verify" sections, using whichever of
compose.yaml or the config switch is missing on this checkout. The Kafka
sink already exists in crates/capture/src/sinks/kafka.rs; do not rewrite it.
Report the doctor output, the session finished log line, and the topic
offsets when done.
```
