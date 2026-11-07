//! Микробенчмарк горячего пути ввода: перевод доли экрана в координаты панели.

#![allow(missing_docs)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rustdroid_input::{NormPoint, PanelSize, Rotation};

fn bench_geometry(c: &mut Criterion) {
    let panel = PanelSize {
        width: 1080,
        height: 2400,
    };
    let points: Vec<NormPoint> = (0..256_u16)
        .map(|i| NormPoint::new(f32::from(i) / 255.0, f32::from(255 - i) / 255.0))
        .collect();

    c.bench_function("geometry/to_touch_256", |b| {
        b.iter(|| {
            for (i, p) in points.iter().enumerate() {
                let rot = match i & 3 {
                    0 => Rotation::Deg0,
                    1 => Rotation::Deg90,
                    2 => Rotation::Deg180,
                    _ => Rotation::Deg270,
                };
                black_box(black_box(*p).to_touch(rot, panel));
            }
        });
    });
}

criterion_group!(benches, bench_geometry);
criterion_main!(benches);
