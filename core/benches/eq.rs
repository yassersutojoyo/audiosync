//! Cost of equalizing one 512-frame stereo block (S127), with every band in
//! use and once more with a block full of non-finite samples (the reset path).

use std::f64::consts::PI;
use std::hint::black_box;

use audiosync_core::eq::{EqSettings, PRESETS, equalizer};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const FRAMES: usize = 512;

fn music_like_block() -> Vec<f32> {
    (0..FRAMES)
        .flat_map(|i| {
            let t = i as f64 / 48_000.0;
            let x = 0.2 * (2.0 * PI * 110.0 * t).sin() + 0.1 * (2.0 * PI * 3_300.0 * t).sin();
            [x as f32, (0.8 * x) as f32]
        })
        .collect()
}

fn equalizer_block(c: &mut Criterion) {
    let (mut control, mut processor) = equalizer();
    control.set_settings(EqSettings {
        enabled: true,
        preamp_db: -3.0,
        gains_db: PRESETS[10].gains_db,
    });
    let source = music_like_block();
    let mut block = source.clone();

    let mut group = c.benchmark_group("equalizer");
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("process 512 stereo frames", |b| {
        b.iter(|| {
            block.copy_from_slice(&source);
            processor.process(black_box(&mut block));
        })
    });
    let poisoned = vec![f32::NAN; FRAMES * 2];
    group.bench_function("process 512 non-finite stereo frames", |b| {
        b.iter(|| {
            block.copy_from_slice(&poisoned);
            processor.process(black_box(&mut block));
        })
    });
    control.set_settings(EqSettings::default());
    group.bench_function("bypass 512 stereo frames (disabled)", |b| {
        b.iter(|| processor.process(black_box(&mut block)))
    });
    group.finish();
}

criterion_group!(benches, equalizer_block);
criterion_main!(benches);
