//! The host's audio handoff (C8): the write call copies into the
//! preallocated ring and returns, with nobody reading on the other side.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use audiosync_cast::audio::audio_ring;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.with(|count| count.set(count.get() + 1));
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

// covers: C8
#[test]
fn the_host_write_neither_allocates_nor_blocks_with_a_stalled_reader() {
    let (writer, reader) = audio_ring(4800);
    let samples_i16 = vec![1000i16; 960];
    let samples_f32 = vec![0.25f32; 960];
    let before = allocations();
    let start = Instant::now();
    let mut taken = 0;
    // Far more than the ring holds, and nobody reads: the writes return.
    for _ in 0..50 {
        taken += writer.write_i16(&samples_i16);
        taken += writer.write_f32(&samples_f32);
    }
    let elapsed = start.elapsed();
    assert_eq!(allocations(), before, "the write path allocated");
    assert!(elapsed < Duration::from_millis(200), "writes took {elapsed:?}");
    assert_eq!(taken, 4800, "the ring takes what fits");
    assert_eq!(reader.available(), 4800);
    assert_eq!(reader.overflow(), 100 * 480 - 4800, "the rest is counted");
}

#[test]
fn frames_come_out_in_order_as_little_endian_samples() {
    let (writer, mut reader) = audio_ring(8);
    writer.write_i16(&[1, -1, 2, -2, 3, -3]);
    writer.write_f32(&[1.0, -1.0, f32::NAN, 2.0]);
    let mut out = Vec::new();
    assert_eq!(reader.read_into(&mut out, 16), 5);
    let samples: Vec<i16> = out
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    assert_eq!(samples, [1, -1, 2, -2, 3, -3, 32767, -32767, 0, 32767]);
    writer.write_i16(&[5, 5, 6, 6, 7, 7]);
    assert_eq!(reader.skip_to_newest(1), 2);
    out.clear();
    reader.read_into(&mut out, 4);
    assert_eq!(out, [7, 0, 7, 0]);
}
