//! Per-buffer cost of the audio-path calls, 512-frame blocks.
//!
//! `cargo bench --bench pipeline`. Each benchmark is one buffer's worth of
//! work; the ring writer advances alongside the readers so they stay in
//! steady state.

use std::hint::black_box;
use std::sync::Arc;

use audiosync_core::convert;
use audiosync_core::fifo::FifoFeed;
use audiosync_core::reader::{Reader, tolerance_frames};
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{Ring, RingWriter};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const N: usize = 512;

fn music(frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let t = i as f32;
            [(t * 0.031).sin() * 0.4, (t * 0.017).cos() * 0.3]
        })
        .collect()
}

fn primed(rate: u32) -> (Arc<Ring>, RingWriter, Vec<f32>) {
    let ring = Ring::for_rate(rate).unwrap();
    let mut writer = ring.writer().unwrap();
    let block = music(N);
    for _ in 0..(3 * rate as usize / N) {
        writer.write(&block, N);
    }
    (ring, writer, block)
}

fn per_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("per_512_frame_block");
    group.throughput(Throughput::Elements(N as u64));

    // Ring write alone.
    let (_ring, mut writer, block) = primed(48_000);
    group.bench_function("ring_write", |b| {
        b.iter(|| writer.write(black_box(&block), N))
    });

    // Reader read at a 2 s delay (plus the write that keeps it in steady state).
    let (ring, mut writer, block) = primed(48_000);
    let mut reader = Reader::new(Arc::clone(&ring), 48_000);
    let mut dst = vec![0.0f32; N * 2];
    let t = tolerance_frames(48_000);
    group.bench_function("ring_write_and_reader_read", |b| {
        b.iter(|| {
            writer.write(&block, N);
            black_box(reader.read(&mut dst, N, 96_000, t))
        })
    });

    // Float to PCM16 frames.
    let samples = music(N);
    let mut packed = vec![0u32; N];
    group.bench_function("convert_pcm16", |b| {
        b.iter(|| convert::pack_stereo(black_box(&samples), &mut packed))
    });

    // Resampling one capture block to 48 kHz.
    let mut out = vec![0.0f32; 4 * N * 2];
    for rate in [44_100u32, 96_000, 48_000] {
        let mut resampler = Resampler::new(rate, 48_000).unwrap();
        let input = music(N);
        group.bench_function(format!("resample_{rate}_to_48000"), |b| {
            b.iter(|| resampler.process(black_box(&input), &mut out))
        });
    }

    // The whole chain: write, read at 2 s, resample 44.1 -> 48 kHz, convert.
    let (ring, mut writer, block) = primed(44_100);
    let mut reader = Reader::new(Arc::clone(&ring), 44_100);
    let mut resampler = Resampler::new(44_100, 48_000).unwrap();
    let t = tolerance_frames(44_100);
    group.bench_function("pipeline_read_resample_convert_44100", |b| {
        b.iter(|| {
            writer.write(&block, N);
            reader.read(&mut dst, N, 88_200, t);
            let (_, made) = resampler.process(&dst, &mut out);
            convert::pack_stereo(&out[..made * 2], &mut packed);
            black_box(made)
        })
    });

    // The capture side of the pipe feed at 44.1 kHz: resample, convert,
    // enqueue (the queue is drained so it never fills).
    let feed = FifoFeed::new(4).unwrap();
    let mut producer = feed.producer().unwrap();
    producer.set_input_rate(44_100).unwrap();
    let mut drain = feed.writer().unwrap();
    let input = music(N);
    group.bench_function("fifo_feed_44100", |b| {
        b.iter(|| {
            let queued = producer.feed(black_box(&input), N);
            drain.clear();
            queued
        })
    });

    // Sixteen outputs on one 192 kHz ring (S146): one write, sixteen reads.
    let (ring, mut writer, block) = primed(192_000);
    let mut readers: Vec<Reader> = (0..16)
        .map(|_| Reader::new(Arc::clone(&ring), 192_000))
        .collect();
    let t = tolerance_frames(192_000);
    group.bench_function("sixteen_readers_192000", |b| {
        b.iter(|| {
            writer.write(&block, N);
            for (i, reader) in readers.iter_mut().enumerate() {
                reader.read(&mut dst, N, 192_000 + 4_800 * i as i64, t);
            }
        })
    });

    group.finish();
}

criterion_group!(benches, per_block);
criterion_main!(benches);
