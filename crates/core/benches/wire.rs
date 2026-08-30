//! Criterion benches over core's per-batch hot path: what the shipping thread
//! (T2) pays 40×/s to turn a drained ring into a UDP datagram / JSONL line, and
//! what every consumer pays to parse it back.
//!
//! Sizes: 25 events is one 25ms window at 1kHz (the steady-state batch);
//! 448 is `MAX_EVENTS_PER_BATCH` (the cap, only reached above ~17kHz or after
//! a stall). Run with `cargo bench -p telemouse-core`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use telemouse_core::{Batch, Batcher, Envelope, EnvelopeView, RawEvent, buttons};

const SIZES: [usize; 2] = [25, 448];

/// A realistic motion batch: small signed deltas, one click, one wheel tick.
fn events(n: usize) -> Vec<RawEvent> {
    (0..n)
        .map(|i| RawEvent {
            ts_qpc: 51_234_567_890 + i as u64 * 10_000,
            dx: (i as i32 % 13) - 6,
            dy: (i as i32 % 7) - 3,
            buttons: if i == 3 { buttons::LEFT_DOWN } else { 0 },
            wheel: if i == 9 { -120 } else { 0 },
            wheel_h: 0,
            device_ix: 0,
        })
        .collect()
}

fn batch(n: usize) -> Batch {
    Batch {
        session_id: "s-20260828-000000-abcd".into(),
        seq_no: 12_345,
        ts_anchor_us: 1_756_000_000_000_000,
        game: Some("cs2.exe".into()),
        pointer_locked: true,
        screen_w: 2560,
        screen_h: 1440,
        cursor_x: None,
        cursor_y: None,
        drops_since_last: 0,
        abs_frames_since_last: 0,
        events: events(n),
    }
}

/// T2's flush: serialize a borrowed view into a reused buffer.
fn bench_encode(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_view");
    for n in SIZES {
        let b = batch(n);
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bench, _| {
            bench.iter(|| {
                buf.clear();
                serde_json::to_writer(&mut buf, &EnvelopeView::Batch(black_box(&b).as_view()))
                    .unwrap();
                black_box(buf.len())
            })
        });
    }
    g.finish();
}

/// The owned path (`Envelope::to_json`), for comparison with the view path.
fn bench_encode_owned(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_owned");
    for n in SIZES {
        let e = Envelope::Batch(batch(n));
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bench, _| {
            bench.iter(|| black_box(black_box(&e).to_json().unwrap().len()))
        });
    }
    g.finish();
}

/// What a consumer (analyze's general path, a Kafka reader) pays to parse an
/// envelope back through the internally-tagged enum.
fn bench_decode(c: &mut Criterion) {
    let mut g = c.benchmark_group("decode_envelope");
    for n in SIZES {
        let s = Envelope::Batch(batch(n)).to_json().unwrap();
        g.throughput(Throughput::Bytes(s.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bench, _| {
            bench.iter(|| match Envelope::from_json(black_box(&s)).unwrap() {
                Envelope::Batch(b) => black_box(b.events.len()),
                _ => unreachable!(),
            })
        });
    }
    g.finish();
}

/// Parsing the `Batch` directly (no tag dispatch) — the ceiling for any
/// tag-prefix-matched fast path like analyze's `BatchRef`.
fn bench_decode_batch(c: &mut Criterion) {
    let mut g = c.benchmark_group("decode_batch_untagged");
    for n in SIZES {
        let s = serde_json::to_string(&batch(n)).unwrap();
        g.throughput(Throughput::Bytes(s.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bench, _| {
            bench.iter(|| {
                let b: Batch = serde_json::from_str(black_box(&s)).unwrap();
                black_box(b.events.len())
            })
        });
    }
    g.finish();
}

/// Batcher accumulate → flush cycle at one window's worth of events.
fn bench_batcher(c: &mut Criterion) {
    let mut g = c.benchmark_group("batcher_cycle");
    for n in SIZES {
        let evs = events(n);
        let mut b = Batcher::with_window_ms(448, 25, 10_000_000);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bench, _| {
            bench.iter(|| {
                for e in &evs {
                    b.push(*e);
                }
                let flush = b.should_flush(black_box(u64::MAX));
                let len = b.events().len();
                b.reset();
                black_box((flush, len))
            })
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_encode,
    bench_encode_owned,
    bench_decode,
    bench_decode_batch,
    bench_batcher
);
criterion_main!(benches);
