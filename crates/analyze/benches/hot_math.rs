//! Criterion benches over the hot paths, at the scale that actually matters.
//!
//! A three-hour 1 kHz session is ~10.8 M grid cells, so 1 M and 10 M cells
//! bracket "a long warmup" and "a full evening". Each case is built from
//! `testutil::bench_events`, which produces the flick-correct-click-rest shape
//! of a real aim session — long idle stretches included, because skipping those
//! cheaply is the whole point of the sparse grid.
//!
//! Run with `cargo bench -p telemouse-analyze`; profile with
//! `cargo build --profile profiling` and a sampling profiler over the same
//! workload.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use telemouse_analyze::savgol::SavGol;
use telemouse_analyze::series::{Params, prepare};
use telemouse_analyze::{flicks, micro, testutil};

/// The two session sizes every bench runs at.
const SIZES: [usize; 2] = [1_000_000, 10_000_000];

fn bench_prepare(c: &mut Criterion) {
    let mut g = c.benchmark_group("prepare");
    g.sample_size(10);
    for cells in SIZES {
        let session = testutil::bench_session(cells);
        g.throughput(Throughput::Elements(cells as u64));
        g.bench_with_input(BenchmarkId::from_parameter(cells), &cells, |b, _| {
            b.iter(|| {
                let p = prepare(black_box(session.clone()), Params::default());
                black_box(p.grid.stored_cells())
            })
        });
    }
    g.finish();
}

fn bench_savgol(c: &mut Criterion) {
    let mut g = c.benchmark_group("savgol");
    g.sample_size(10);
    for cells in SIZES {
        // The smoother is applied lane by lane over the stored cells, so the
        // input is one dense buffer of that size.
        let p = prepare(testutil::bench_session(cells), Params::default());
        let x = p.grid.dense(|r, j| r.vx[j]);
        let sg = SavGol::smoother(3, 2);
        g.throughput(Throughput::Elements(x.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(cells), &cells, |b, _| {
            b.iter(|| black_box(sg.apply(black_box(&x), 0.001).len()))
        });
    }
    g.finish();
}

fn bench_welch(c: &mut Criterion) {
    let mut g = c.benchmark_group("welch");
    g.sample_size(10);
    for cells in SIZES {
        let p = prepare(testutil::bench_session(cells), Params::default());
        g.throughput(Throughput::Elements(cells as u64));
        g.bench_with_input(BenchmarkId::from_parameter(cells), &cells, |b, _| {
            b.iter(|| black_box(micro::compute(black_box(&p)).analyzed_blocks))
        });
    }
    g.finish();
}

fn bench_flick_detect(c: &mut Criterion) {
    let mut g = c.benchmark_group("flick_detect");
    g.sample_size(10);
    for cells in SIZES {
        let p = prepare(testutil::bench_session(cells), Params::default());
        g.throughput(Throughput::Elements(cells as u64));
        g.bench_with_input(BenchmarkId::from_parameter(cells), &cells, |b, _| {
            b.iter(|| black_box(flicks::detect(black_box(&p)).len()))
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_prepare,
    bench_savgol,
    bench_welch,
    bench_flick_detect
);
criterion_main!(benches);
