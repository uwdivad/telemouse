# kafka2parquet

A Kafka → Parquet archiver for telemouse, built up step by step in a notebook.

Capture produces `mouse.sessions`, `mouse.events` and `mouse.markers`; nothing in
the workspace consumes them. `kafka2parquet.ipynb` builds that consumer one cell
at a time: decode the JSON envelope, port the QPC→UTC anchor math bit-exactly,
flatten batches to one row per event, write Hive-partitioned Parquet with a
rolling writer, then poll the broker with manual offset commits so the pipeline
is at-least-once. DuckDB queries the result directly.

Sections 1–7 run offline against `recordings/demo-session.jsonl`. Sections 8–10
need the broker named in `telemouse.toml`. Section 12 reconciles the archive from
the JSONL recordings: capture's Kafka sink drops batches when the broker is
unreachable, the recording never does, so the recording is the source of truth
and an idempotent gap-fill writes whatever Kafka missed straight into Parquet.

## Run it

```powershell
cd tools\kafka2parquet
python -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -r requirements.txt
jupyter lab kafka2parquet.ipynb      # or open it in RustRover / VS Code with this venv as the kernel
```

Output lands in `tools/kafka2parquet/data/` (git-ignored):

```
data/fixture/…   written from the demo recording (section 7)
data/live/…      written from the broker (section 9)
```

Section 11 of the notebook describes turning the cells into a long-running
service next to the broker.

## Next step: PySpark over the archive

`spark_time_ranges.ipynb` reads `data/live` (or `data/fixture`) with PySpark and
walks through slicing the events into ranges of time: absolute windows, windows
relative to session start, fixed buckets (`window`), data-driven ranges
(`session_window`, `lag` + running sum for game runs and missing batches), the
same in Spark SQL, and writing rollups back as partitioned Parquet that DuckDB
reads. It is a learning notebook: every step explains the idea, shows the
`explain()` plan where it matters, and cross-checks against DuckDB. Section 5
also finds events archived with `ts_utc_us = NULL` (batches consumed before
their session envelope) and recomputes them bit-exactly from the session anchor.

Needs Java 17+ (`JAVA_HOME` or `java` on `PATH`) and `pip install pyspark`. On
Windows the first setup cell also downloads `winutils.exe` and `hadoop.dll` into
`hadoop/bin` (git-ignored); without them Spark cannot list local files.
