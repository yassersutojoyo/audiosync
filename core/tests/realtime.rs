//! Real-time proof for the audio path (S126, S128).
//!
//! A counting global allocator records every allocation, reallocation and
//! free made by a thread while it is "tracking". After setup, each test
//! tracks only the audio-path calls (ring write, reader read, resample,
//! convert, render and fades) and asserts the count stays at zero. Other test threads never track, so they cannot interfere.
//!
//! The audio-path modules also forbid unsafe code and contain no lock types,
//! and the C interface's audio-path files, which hold the crate's unsafe code,
//! contain no lock types either; both are checked here against their sources
//! (read at run time, so the check works on any checkout's line endings and
//! path separators).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use audiosync_core::clock;
use audiosync_core::convert;
use audiosync_core::fade::InputFade;
use audiosync_core::reader::{Reader, ReaderControl, tolerance_frames};
use audiosync_core::render::{Renderer, RendererControl};
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

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
    let mut device = vec![0.0f32; 4096 * 6];
    let count = allocations_in(|| {
        for r in &mut resamplers {
            for block in [1usize, 441, 512, 4096, 0, 333] {
                let mut chunk = &input[..block * 2];
                loop {
                    let need = r.input_frames_for(64);
                    std::hint::black_box(need);
                    let (used, made) = r.process(chunk, &mut out);
                    std::hint::black_box(&out[..made * 2]);
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
        let mut copy_in_place = [0.5f32, f32::NAN, f32::INFINITY, -2.0];
        convert::zero_non_finite(&mut copy_in_place);
        for channels in [1usize, 2, 6] {
            convert::map_to_device(&input, 4096, 0.5, &mut device, channels);
        }
        std::hint::black_box(&device);
    });
    assert_eq!(count, 0, "resample / convert allocated or freed");
}

// covers: S190, S126, S127
#[test]
fn fading_allocates_and_locks_nothing_on_the_audio_thread() {
    // Setup may allocate: a renderer converting to 44.1 kHz and the native
    // input's fade.
    let ring = Ring::for_rate(48_000).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(Arc::clone(&ring), 48_000);
    let control = RendererControl::new(&reader);
    control.set_delay_frames(4096);
    let gain = Arc::new(SharedGain::new(0.8));
    let mut renderer =
        Renderer::new(reader, &control, Arc::clone(&gain), 48_000, 44_100, 2).unwrap();
    let mut input = InputFade::new(48_000, 256);
    let block = noisy_block(512, 9);
    let burst = noisy_block(3000, 10);
    let mut out = vec![0.0f32; 1500 * 2];
    let mut handed = 0usize;
    let count = allocations_in(|| {
        for round in 0..3000usize {
            // The capture: steady blocks, a burst (a held skip forward) and
            // a stall (a held step back); after the first 1000 rounds, each
            // block stamped on the core's clock (S207).
            let stamped = round >= 1000;
            match round % 500 {
                200 => writer.write(&burst, 3000),
                300..=320 => {}
                _ if stamped => writer.write_at(&block, 512, clock::now_ns()),
                _ => writer.write(&block, 512),
            }
            // The control thread: delay changes, then the native input's stop
            // and a new session's rate.
            match round % 500 {
                50 => control.set_delay_frames(8000),
                120 => control.set_delay_frames(2048),
                121 => control.set_delay_frames(3000),
                400 => input.fade_out(),
                450 => input.restart([48_000, 96_000][round / 500 % 2]),
                _ => {}
            }
            if round == 2600 {
                control.request_fade_out();
            }
            // The output, in callbacks of changing sizes, with and without
            // an output time, and once the capture stamps, with the time
            // it reads at (S207).
            let frames = [512usize, 441, 1500, 97][round % 4];
            let out = &mut out[..frames * 2];
            let now = clock::now_ns();
            match (stamped, round % 2 == 0) {
                (false, true) => renderer.render(out),
                (false, false) => renderer.render_timed(out, round as u64 * 10_000_000),
                (true, true) => renderer.render_at(out, now),
                (true, false) => renderer.render_timed_at(out, round as u64 * 10_000_000, now),
            }
            std::hint::black_box(renderer.frames_after_fade());
            std::hint::black_box(control.faded_by_time(1_000_000, round as u64));
            // The native input.
            input.process(&block, |piece| handed += piece.len());
        }
    });
    assert_eq!(count, 0, "a fade allocated or freed memory");
    let counters = control.counters();
    assert!(
        counters.drift_skips > 0 && counters.drift_silences > 0,
        "{counters:?}"
    );
    assert!(control.fade_rendered());
    assert!(handed > 0);
}

// covers: S207, S126
#[test]
fn following_the_capture_clock_allocates_nothing() {
    // Setup may allocate: an earliest output converting to 44.1 kHz.
    let ring = Ring::for_rate(48_000).unwrap();
    let mut writer = ring.writer().unwrap();
    let reader = Reader::new(Arc::clone(&ring), 48_000);
    let control = RendererControl::new(&reader);
    let gain = Arc::new(SharedGain::new(0.8));
    let mut renderer = Renderer::new(reader, &control, gain, 48_000, 44_100, 2).unwrap();
    let block = noisy_block(480, 11);
    let mut out = vec![0.0f32; 441 * 2];
    // The capture writes 480 frames every 10 ms; the device asks for 441
    // every 10 ms on its own clock, 0.2 % fast for 4 s and then 0.2 %
    // slow, so the converter is trimmed both ways. Both are stamped on one
    // made-up clock.
    let (mut write_ns, mut pull_ns) = (10_000_000u64, 0u64);
    let count = allocations_in(|| {
        while write_ns <= 8_000_000_000 {
            if pull_ns < write_ns {
                renderer.render_at(&mut out, pull_ns);
                pull_ns += if pull_ns < 4_000_000_000 {
                    9_980_000
                } else {
                    10_020_000
                };
            } else {
                writer.write_at(&block, 480, write_ns);
                write_ns += 10_000_000;
            }
        }
    });
    assert_eq!(count, 0, "following the clock allocated or freed memory");
    assert!(
        control.slowed_frames() > 0 && control.sped_frames() > 0,
        "slowed {}, sped {}",
        control.slowed_frames(),
        control.sped_frames()
    );
}

/// The source of a module without its unit tests, with `\n` line ends
/// whatever the checkout uses (a Windows checkout may have `\r\n`). Panics
/// if the module has a test module this split doesn't find, so the scan never
/// silently covers test code or misses a renamed marker.
fn production_part(source: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let marker = "\n#[cfg(test)]\nmod tests";
    match source.split_once(marker) {
        Some((code, _)) => code.to_owned(),
        None => {
            assert!(
                !source.contains("mod tests"),
                "a test module not preceded by its #[cfg(test)] line"
            );
            source
        }
    }
}

/// Words that mean a lock, a blocking wait, logging or file access.
const NOT_REAL_TIME: [&str; 16] = [
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

/// The words of [`NOT_REAL_TIME`] in a module's production code.
fn forbidden_words(code: &str) -> Vec<&'static str> {
    NOT_REAL_TIME
        .into_iter()
        .filter(|word| code.contains(word))
        .collect()
}

#[test]
fn the_source_scan_reads_crlf_checkouts_and_finds_what_it_looks_for() {
    let source = "#![forbid(unsafe_code)]\r\nuse std::sync::atomic::AtomicU32;\r\n\r\n\
                  #[cfg(test)]\r\nmod tests {\r\n    use std::sync::Mutex;\r\n}\r\n";
    let code = production_part(source);
    assert!(code.ends_with("AtomicU32;\n"), "{code:?}");
    assert!(forbidden_words(&code).is_empty(), "test code is left out");
    let locked = production_part("use std::sync::Mutex;\r\nfn f() { thread::sleep(d) }\r\n");
    assert_eq!(forbidden_words(&locked), ["Mutex", "thread::sleep"]);
}

// covers: S128, S126, S133
#[test]
fn audio_path_modules_use_only_atomics() {
    // Where each module may wait: never on an audio thread; the equalizer's
    // control side may yield while the capture thread copies a set (S31,
    // S130).
    let modules = [
        ("ring.rs", "none"),
        ("reader.rs", "none"),
        ("resample.rs", "none"),
        ("convert.rs", "none"),
        ("channels.rs", "none"),
        ("volume.rs", "none"),
        ("fade.rs", "none"),
        ("render.rs", "none"),
        ("eq.rs", "yield"),
        ("clock.rs", "origin"),
    ];
    for (name, waits) in modules {
        let source = source_file(&[name]);
        assert!(
            source.contains("#![forbid(unsafe_code)]"),
            "{name} must forbid unsafe code"
        );
        assert_waits_only_as_allowed(name, &production_part(&source), waits);
    }
}

// covers: S126, S128
#[test]
fn c_interface_audio_calls_use_only_atomics() {
    // The C interface's files with audio-path calls: the equalizer and the
    // gain (ffi.rs), the ring and its readers, and the resampler and the
    // output stage. They hold the crate's unsafe code, so they can't forbid
    // it, but the rest of the scan applies, and none of them waits. The sync
    // rule (ffi/sync.rs) is a control-thread call.
    let files: [&[&str]; 3] = [&["ffi.rs"], &["ffi", "ring.rs"], &["ffi", "render.rs"]];
    for parts in files {
        let name = parts.join("/");
        let code = production_part(&source_file(parts));
        assert!(
            code.contains("#[unsafe(no_mangle)]"),
            "{name} exports nothing"
        );
        assert_waits_only_as_allowed(&name, &code, "none");
    }
}

/// Asserts that a module's production code holds no word of
/// [`NOT_REAL_TIME`] and waits only as `waits` allows: "none", "yield" (a
/// `thread::yield_now`) or "origin" (the stamps' clock, S207: its origin is
/// a `OnceLock` that `Ring::new` sets before any audio thread runs, so on
/// an audio thread it is only loaded, never waited for).
fn assert_waits_only_as_allowed(name: &str, code: &str, waits: &str) {
    let forbidden: Vec<&str> = forbidden_words(code)
        .into_iter()
        .filter(|word| !(waits == "origin" && *word == "OnceLock"))
        .collect();
    assert_eq!(forbidden, [] as [&str; 0], "{name}");
    let mut thread_calls = code.match_indices("thread::").map(|(at, _)| &code[at..]);
    let allowed = |call: &str| waits == "yield" && call.starts_with("thread::yield_now");
    assert!(thread_calls.all(allowed), "{name} blocks");
    assert!(!code.contains("park"), "{name} parks");
}

/// A source file under `src/`, by its path components.
fn source_file(parts: &[&str]) -> String {
    let path = parts.iter().fold(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        |path, part| path.join(part),
    );
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Compile-time check that what audio threads share is thread-safe.
// covers: S128
#[test]
fn shared_types_are_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Ring>();
    send_sync::<ReaderControl>();
    send_sync::<RendererControl>();
    send::<Renderer>();
    send::<InputFade>();
    send::<RingWriter>();
    send::<Reader>();
    send::<Resampler>();
}
