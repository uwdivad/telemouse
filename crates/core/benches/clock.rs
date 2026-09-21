//! Criterion bench over `QpcAnchor::qpc_to_utc_us`, the analyzer's
//! once-per-event tick→UTC conversion (`series::prepare` calls it 15 M times
//! on the largest recording; the 2026-09-20 profile put 2.1% of all analyzer
//! CPU in `__divti3` underneath it).
//!
//! Each group runs the same sweep of event stamps through the shipped
//! function and through the i128 implementation it replaced, so the before /
//! after shows up side by side. Run with `cargo bench -p telemouse-core`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use telemouse_core::QpcAnchor;

/// One second of 1 kHz events, the granularity `prepare` walks.
const EVENTS: usize = 1_000;

/// The implementation the 64-bit path replaced: an i128 division per call,
/// which on MSVC x86-64 is a `__divti3` call.
fn i128_qpc_to_utc_us(a: &QpcAnchor, qpc: u64) -> i64 {
    let dticks = qpc as i128 - a.qpc as i128;
    let dus = if a.qpc_freq == 10_000_000 {
        dticks / 10
    } else {
        dticks * 1_000_000 / a.qpc_freq as i128
    };
    (a.utc_us as i128 + dus) as i64
}

fn anchor(freq: u64) -> QpcAnchor {
    QpcAnchor {
        qpc: 51_234_567_890,
        utc_us: 1_756_000_000_000_000,
        qpc_freq: freq,
    }
}

/// Event stamps as they arrive: monotonic, ~1 ms apart, jittered.
fn stamps(a: &QpcAnchor) -> Vec<u64> {
    let step = a.qpc_freq / 1_000;
    (0..EVENTS)
        .map(|i| a.qpc + i as u64 * step + (i as u64 * 7919) % step)
        .collect()
}

fn bench_qpc_to_utc_us(c: &mut Criterion) {
    // 10 MHz is what every real session runs at; 3_579_545 Hz is the odd-crystal
    // case that takes the general split instead of the divide-by-10 shortcut.
    for (label, freq) in [("10mhz", 10_000_000u64), ("odd_3579545", 3_579_545)] {
        let mut g = c.benchmark_group(format!("qpc_to_utc_us/{label}"));
        let a = anchor(freq);
        let ts = stamps(&a);
        g.throughput(Throughput::Elements(EVENTS as u64));
        g.bench_function("i64", |bench| {
            bench.iter(|| {
                let mut acc = 0i64;
                for &t in black_box(&ts) {
                    acc = acc.wrapping_add(black_box(&a).qpc_to_utc_us(t));
                }
                black_box(acc)
            })
        });
        g.bench_function("i128_before", |bench| {
            bench.iter(|| {
                let mut acc = 0i64;
                for &t in black_box(&ts) {
                    acc = acc.wrapping_add(i128_qpc_to_utc_us(black_box(&a), t));
                }
                black_box(acc)
            })
        });
        g.finish();
    }
}

criterion_group!(benches, bench_qpc_to_utc_us);
criterion_main!(benches);
