# tools/

Optional. Nothing here is needed to capture, watch or analyze mouse data;
the release zips do not include it and the workspace build does not touch
it.

| Directory | What | Needs |
|---|---|---|
| `cpubench/` | `tmbench`, a cycle-exact CPU harness for the live stack, plus `bench.ps1`. Its own Cargo crate, not a workspace member: `cargo build --release` inside the directory. Results and method: `docs/BENCHMARKS.md`. | Rust, Windows |
| `kafka2parquet/` | Two teaching notebooks: a Kafka → Parquet archiver for the `mouse.*` topics, and a PySpark walk-through over the archive. Committed without outputs; run them to see results. | Python 3.12+, a Kafka broker (`compose.yaml` at the repo root), Java 17+ for the Spark notebook |
