//! The host's input call never allocates (P6), checked with an allocator
//! that counts the allocations of the thread being watched, in the calm
//! case, with a full ring, with late frames, and while the stream cuts on
//! another thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use audiosync_airplay::clock::SenderClock;
use audiosync_airplay::stream::{Stream, StreamConfig};

struct Counting;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

fn note() {
    let _ = WATCHING.try_with(|watching| {
        if watching.get() {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
    });
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations `work` makes on this thread.
fn allocations(work: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    ALLOCATIONS.with(Cell::get)
}

// Part of P6 (the crate's own network threads come with the transport stage).
// covers: P6
#[test]
fn rt_input_no_alloc() {
    let start = Instant::now();
    let clock = SenderClock::new(start);
    let (mut stream, mut writer) = Stream::new(StreamConfig::default(), clock, start);
    let block_i16: Vec<i16> = (0..882).map(|i| (i * 37) as i16).collect();
    let block_f32: Vec<f32> = block_i16.iter().map(|&s| f32::from(s) / 32_768.0).collect();
    assert!(
        allocations(|| {
            std::hint::black_box(Vec::<u8>::with_capacity(16));
        }) > 0,
        "the counter works"
    );

    // Calm writes, then a full ring (2 s), all without allocating.
    let count = allocations(|| {
        for _ in 0..500 {
            writer.write_i16(&block_i16, start);
            writer.write_f32(&block_f32, start);
        }
    });
    assert_eq!(count, 0);
    assert!(writer.position() >= stream.input_frames() as u64);

    // Late frames: the stream cut far past the writer's position.
    stream.handle_timeout(start + Duration::from_secs(10));
    let count = allocations(|| {
        for _ in 0..100 {
            writer.write_i16(&block_i16, start + Duration::from_secs(10));
        }
    });
    assert_eq!(count, 0);
    stream.handle_timeout(start + Duration::from_secs(10));
    assert!(stream.counters().input_frames_dropped() > 0);

    // A contended run: the writer on its own thread while the stream cuts
    // on this one, as fast as both go.
    let start = Instant::now();
    let clock = SenderClock::new(start);
    let (mut stream, mut writer) = Stream::new(StreamConfig::default(), clock, start);
    let handle = std::thread::spawn(move || {
        let block: Vec<i16> = (0..882).map(|i| i as i16).collect();
        let count = allocations(|| {
            for _ in 0..20_000 {
                writer.write_i16(&block, Instant::now());
            }
        });
        (count, writer)
    });
    while !handle.is_finished() {
        stream.handle_timeout(Instant::now());
        while stream.poll_transmit().is_some() {}
    }
    let (count, writer) = handle.join().expect("writer thread");
    assert_eq!(count, 0, "no allocation while contended");
    assert!(writer.position() > 0);
}
