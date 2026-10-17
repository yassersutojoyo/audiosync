//! Real-time proof for the audio path (S126, S128, S129).
//!
//! A counting global allocator records every allocation, reallocation and
//! free made by a thread while it is "tracking". After setup, each test
//! tracks only the audio-path calls (ring write, reader read, resample,
//! convert, FIFO feed, and the FIFO writer's step) and asserts the count
//! stays at zero. Other test threads never track, so they cannot interfere.
//!
//! The audio-path modules also forbid unsafe code and contain no lock types,
//! checked here against their sources.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use audiosync_core::convert;
use audiosync_core::fifo::{FifoFeed, OpenError, PipeSink, Step, WriteError};
use audiosync_core::reader::{Reader, ReaderControl, tolerance_frames};
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{Ring, RingWriter};

struct Counting;

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static EVENTS: Cell<usize> = const { Cell::new(0) };
}

/// Counts an allocator call made by a tracking thread, in that thread's own
/// counter (const-initialized thread locals never allocate).
fn note() {
    if TRACKING.try_with(Cell::get).unwrap_or(false) {
        let _ = EVENTS.try_with(|events| events.set(events.get() + 1));
    }
}

// SAFETY: forwards every call unchanged to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Runs `f` while counting this thread's allocator calls; returns the count.
fn allocations_in(f: impl FnOnce()) -> usize {
    let before = EVENTS.with(Cell::get);
    TRACKING.with(|t| t.set(true));
    f();
    TRACKING.with(|t| t.set(false));
    EVENTS.with(Cell::get) - before
}

// covers: S126
#[test]
fn the_counter_sees_allocations() {
    let mut keep = Vec::new();
    let n = allocations_in(|| keep.push(Box::new(7u64)));
    assert!(n >= 1);
}

fn noisy_block(frames: usize, seed: u32) -> Vec<f32> {
    (0..frames * 2)
        .map(|i| {
            let x = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed);
            (x >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        })
        .collect()
}

// covers: S126, S128, S127
#[test]
fn ring_write_and_reader_read_do_not_allocate() {
    let ring = Ring::new(1 << 17, 8192).unwrap();
    let mut writer: RingWriter = ring.writer().unwrap();
    let mut readers: Vec<Reader> = (0..3)
        .map(|_| Reader::new(Arc::clone(&ring), 48_000))
        .collect();
    let controls: Vec<ReaderControl> = readers.iter().map(Reader::control).collect();
    let small = noisy_block(512, 1);
    let large = noisy_block(3 << 17, 2);
    let mut bad = noisy_block(512, 3);
    bad[10] = f32::NAN;
    bad[11] = f32::INFINITY;
    let mut dst = vec![0.0f32; 8192 * 2];
    let t = tolerance_frames(48_000);
    writer.write(&small, 512);
    let count = allocations_in(|| {
        for round in 0..400usize {
            match round % 50 {
                // A block larger than the margin and than the whole ring (S38).
                7 => writer.write(&large, 3 << 17),
                // A writer pause: no write (underrun, then step-back).
                20..=30 => {}
                13 => writer.write(&bad, 512),
                _ => writer.write(&small, 512),
            }
            if round % 37 == 0 {
                controls[0].set_delay_frames(4096 + 100 * round as i64);
                controls[1].request_resync();
                controls[2].set_tolerance_frames(t);
                controls[1].reset_prime();
            }
            for (i, reader) in readers.iter_mut().enumerate() {
                let frames = [512usize, 441, 8192][i];
                match i {
                    0 => reader.read_target(&mut dst, frames),
                    _ => reader.read(&mut dst, frames, 2048 * i as i64, t),
                };
            }
            if round % 60 == 0 {
                // A stall: one reader skips reads while capture continues.
                for _ in 0..3 {
                    writer.write(&small, 512);
                }
            }
            for control in &controls {
                std::hint::black_box((control.lag_frames(), control.counters()));
            }
        }
    });
    assert_eq!(count, 0, "ring write / reader read allocated or freed");
    let counters = controls[2].counters();
    assert!(counters.starved_frames > 0 || counters.drift_silences > 0 || counters.drift_skips > 0);
}

// covers: S126, S127
#[test]
fn resample_and_convert_do_not_allocate() {
    let pairs = [
        (44_100u32, 48_000u32),
        (96_000, 48_000),
        (8_000, 48_000),
        (47_999, 48_000),
        (48_000, 48_000),
        (48_000, 44_100),
        (192_000, 8_000),
    ];
    let mut resamplers: Vec<Resampler> = pairs
        .iter()
        .map(|&(i, o)| Resampler::new(i, o).unwrap())
        .collect();
    let mut input = noisy_block(4096, 4);
    input[100] = f32::NAN;
    input[777] = f32::NEG_INFINITY;
    let mut out = vec![0.0f32; 2 * 2048];
    let mut packed = vec![0u32; 4096];
    let mut pcm = vec![0i16; 8192];
    let mut device = vec![0.0f32; 4096 * 6];
    let count = allocations_in(|| {
        for r in &mut resamplers {
            for block in [1usize, 441, 512, 4096, 0, 333] {
                let mut chunk = &input[..block * 2];
                loop {
                    let need = r.input_frames_for(64);
                    std::hint::black_box(need);
                    let (used, made) = r.process(chunk, &mut out);
                    convert::pack_stereo(&out[..made * 2], &mut packed);
                    chunk = &chunk[used * 2..];
                    if chunk.is_empty() && made < out.len() / 2 {
                        break;
                    }
                }
            }
            // Re-rating at a session start does not allocate either (S94).
            r.set_input_rate(32_000).unwrap();
            r.reset();
        }
        convert::f32_to_i16_slice(&input, &mut pcm);
        let mut copy_in_place = [0.5f32, f32::NAN, f32::INFINITY, -2.0];
        convert::zero_non_finite(&mut copy_in_place);
        for channels in [1usize, 2, 6] {
            convert::map_to_device(&input, 4096, 0.5, &mut device, channels);
        }
        std::hint::black_box((&packed, &pcm, &device));
    });
    assert_eq!(count, 0, "resample / convert allocated or freed");
}

// covers: S126, S95, S87
#[test]
fn fifo_feed_does_not_allocate() {
    let feed = FifoFeed::new(1).unwrap();
    let mut producer = feed.producer().unwrap();
    let mut drain = feed.writer().unwrap();
    let mut block = noisy_block(10_000, 5);
    block[3] = f32::NAN;
    block[4] = f32::INFINITY;
    let count = allocations_in(|| {
        for rate in [48_000u32, 44_100, 96_000, 8_000, 384_000] {
            producer.set_input_rate(rate).unwrap();
            for size in [441usize, 512, 1, 10_000, 4096] {
                producer.feed(&block, size);
            }
            // Fill the queue past capacity: the overflow is dropped (S87).
            for _ in 0..12 {
                producer.feed(&block, 10_000);
            }
            drain.clear();
        }
        std::hint::black_box(producer.samples_queued());
    });
    assert_eq!(count, 0, "feed allocated or freed");
    assert!(feed.frames_dropped() > 0);
}

/// A pipe that never allocates: it plays a fixed script of write outcomes,
/// refuses every third open and counts the bytes it accepts.
struct ScriptedPipe {
    script: [Result<usize, WriteError>; 8],
    call: usize,
    open: bool,
    opens: usize,
    bytes: u64,
}

impl PipeSink for ScriptedPipe {
    fn open(&mut self) -> Result<(), OpenError> {
        self.opens += 1;
        if self.opens % 3 == 1 {
            return Err(OpenError::NoReader);
        }
        self.open = true;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
        let outcome = self.script[self.call % self.script.len()];
        self.call += 1;
        let n = outcome?.min(bytes.len());
        self.bytes += n as u64;
        Ok(n)
    }

    fn close(&mut self) {
        self.open = false;
    }
}

// covers: S129, S90, S93
#[test]
fn fifo_writer_step_does_not_allocate() {
    let feed = FifoFeed::new(1).unwrap();
    let mut producer = feed.producer().unwrap();
    let mut writer = feed.writer().unwrap();
    let block = noisy_block(4800, 6);
    let mut pipe = ScriptedPipe {
        script: [
            Ok(4098),
            Ok(3),
            Err(WriteError::WouldBlock),
            Ok(usize::MAX),
            Err(WriteError::BrokenPipe),
            Ok(1),
            Err(WriteError::Interrupted),
            Ok(0),
        ],
        call: 0,
        open: false,
        opens: 0,
        bytes: 0,
    };
    let count = allocations_in(|| {
        for round in 0..2000 {
            if round % 4 == 0 {
                producer.feed(&block, 4800);
            }
            let step = writer.step(&mut pipe);
            std::hint::black_box(step == Step::Continue);
        }
        writer.disconnect(&mut pipe);
    });
    assert_eq!(count, 0, "the writer's step allocated or freed");
    assert!(pipe.bytes > 0 && feed.counters().backlog_skips > 0);
    assert!(!pipe.open);
}

/// The source of a module without its unit tests.
fn production_part(source: &str) -> &str {
    source.split("#[cfg(test)]\nmod tests").next().unwrap()
}

// covers: S128, S126, S133
#[test]
fn audio_path_modules_use_only_atomics() {
    let modules = [
        ("ring.rs", include_str!("../src/ring.rs")),
        ("reader.rs", include_str!("../src/reader.rs")),
        ("resample.rs", include_str!("../src/resample.rs")),
        ("convert.rs", include_str!("../src/convert.rs")),
        ("fifo.rs", include_str!("../src/fifo.rs")),
    ];
    let forbidden = [
        "Mutex",
        "RwLock",
        "Condvar",
        "Barrier",
        "mpsc",
        "Semaphore",
        ".lock(",
        "OnceLock",
        "LazyLock",
        "RefCell",
        "UnsafeCell",
        "println!",
        "eprintln!",
        "std::fs",
        "File::",
        "thread::sleep",
    ];
    for (name, source) in modules {
        assert!(
            source.contains("#![forbid(unsafe_code)]"),
            "{name} must forbid unsafe code"
        );
        let code = production_part(source);
        for word in forbidden {
            assert!(!code.contains(word), "{name} uses {word}");
        }
    }
    // Only the FIFO writer thread may sleep, and only between steps.
    for (name, source) in &modules[..4] {
        let code = production_part(source);
        assert!(
            !code.contains("park") && !code.contains("thread::"),
            "{name} blocks"
        );
    }
}

/// Compile-time check that what audio threads share is thread-safe.
// covers: S128
#[test]
fn shared_types_are_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Ring>();
    send_sync::<ReaderControl>();
    send_sync::<FifoFeed>();
    send::<RingWriter>();
    send::<Reader>();
    send::<Resampler>();
    send::<audiosync_core::fifo::FifoProducer>();
    send::<audiosync_core::fifo::FifoWriter>();
}
