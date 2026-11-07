//! Микробенчмарки статистики кадров: `record` вызывается из present-потока на каждый кадр.

#![allow(missing_docs)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rustdroid_telemetry::FrameStats;

fn bench_frame_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_stats");

    group.bench_function("record", |b| {
        let mut stats = FrameStats::new(1024);
        let mut t = 6.9_f32;
        b.iter(|| {
            t = if t > 20.0 { 6.9 } else { t + 0.37 };
            stats.record_ms(black_box(t));
        });
    });

    group.bench_function("summary_1024", |b| {
        let mut stats = FrameStats::new(1024);
        for i in 0..1024_u16 {
            stats.record_ms(6.9 + f32::from(i % 17) * 0.5);
        }
        b.iter(|| black_box(stats.summary()));
    });

    group.finish();
}

criterion_group!(benches, bench_frame_stats);
criterion_main!(benches);
