//! The host's audio handoff (C8) and the stream formats (C62, C63).
//!
//! The host writes stereo frames into a preallocated single-producer,
//! single-consumer ring; the stream server's pump thread reads them. The
//! write never allocates, locks, blocks or logs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Channels of every stream (C63).
pub const CHANNELS: usize = 2;
/// Bytes of one 16-bit stereo frame.
pub const FRAME_BYTES: usize = 4;

/// The container and codec of one session's stream (C63). FLAC, the
/// fallback of C84, comes with its encoder in a later stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// 16-bit LPCM in WAV, the first choice.
    Wav,
}

/// A stream's codec and sample rate (C63).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamFormat {
    codec: Codec,
    rate: u32,
}

impl StreamFormat {
    /// The format, when the rate is one the spec allows (44 100 or
    /// 48 000 Hz).
    pub fn new(codec: Codec, rate: u32) -> Option<StreamFormat> {
        matches!(rate, 44_100 | 48_000).then_some(StreamFormat { codec, rate })
    }

    pub fn wav(rate: u32) -> Option<StreamFormat> {
        StreamFormat::new(Codec::Wav, rate)
    }

    pub fn codec(self) -> Codec {
        self.codec
    }

    pub fn rate(self) -> u32 {
        self.rate
    }

    /// The `contentType` of the `LOAD` and of the HTTP response (C59, C63).
    pub fn content_type(self) -> &'static str {
        match self.codec {
            Codec::Wav => "audio/wav",
        }
    }

    /// The file extension of the stream path.
    pub fn extension(self) -> &'static str {
        match self.codec {
            Codec::Wav => "wav",
        }
    }

    /// Frames in one 20 ms block (C64).
    pub fn block_frames(self) -> usize {
        self.rate as usize / 50
    }

    /// The header every response starts with (C61, C63). For WAV the RIFF
    /// and data sizes are unknown and hold their largest value.
    pub fn stream_header(self) -> Vec<u8> {
        match self.codec {
            Codec::Wav => wav_header(self.rate),
        }
    }
}

fn wav_header(rate: u32) -> Vec<u8> {
    let block_align = FRAME_BYTES as u16;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
    header.extend_from_slice(&rate.to_le_bytes());
    header.extend_from_slice(&(rate * FRAME_BYTES as u32).to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header
}

struct Shared {
    /// One stereo frame per slot, left in the low 16 bits.
    slots: Box<[AtomicU32]>,
    /// Frames written and read so far; only their difference matters.
    written: AtomicUsize,
    read: AtomicUsize,
    /// Frames the writer dropped because the ring was full.
    overflow: AtomicU64,
}

/// Makes a ring that holds `capacity` frames, split into its two ends.
pub fn audio_ring(capacity: usize) -> (AudioWriter, AudioReader) {
    let capacity = capacity.max(1);
    let shared = Arc::new(Shared {
        slots: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
        written: AtomicUsize::new(0),
        read: AtomicUsize::new(0),
        overflow: AtomicU64::new(0),
    });
    (
        AudioWriter {
            shared: Arc::clone(&shared),
        },
        AudioReader { shared },
    )
}

/// The host's end of the ring. Its writes copy frames and return (C8).
pub struct AudioWriter {
    shared: Arc<Shared>,
}

impl AudioWriter {
    /// Writes interleaved stereo 16-bit samples; returns the frames taken.
    /// Frames that don't fit are dropped and counted, never waited for.
    pub fn write_i16(&self, samples: &[i16]) -> usize {
        self.write_frames(
            samples
                .as_chunks::<CHANNELS>()
                .0
                .iter()
                .map(|frame| (frame[0] as u16 as u32) | ((frame[1] as u16 as u32) << 16)),
        )
    }

    /// Writes interleaved stereo float samples in -1.0 to 1.0, clipped
    /// outside it; returns the frames taken.
    pub fn write_f32(&self, samples: &[f32]) -> usize {
        self.write_frames(samples.as_chunks::<CHANNELS>().0.iter().map(|frame| {
            (to_i16(frame[0]) as u16 as u32) | ((to_i16(frame[1]) as u16 as u32) << 16)
        }))
    }

    fn write_frames(&self, frames: impl ExactSizeIterator<Item = u32>) -> usize {
        let shared = &*self.shared;
        let capacity = shared.slots.len();
        let written = shared.written.load(Ordering::Relaxed);
        let read = shared.read.load(Ordering::Acquire);
        let free = capacity - written.wrapping_sub(read);
        let offered = frames.len();
        let mut taken = 0;
        for frame in frames.take(free) {
            shared.slots[written.wrapping_add(taken) % capacity].store(frame, Ordering::Relaxed);
            taken += 1;
        }
        shared
            .written
            .store(written.wrapping_add(taken), Ordering::Release);
        if taken < offered {
            shared
                .overflow
                .fetch_add((offered - taken) as u64, Ordering::Relaxed);
        }
        taken
    }
}

fn to_i16(sample: f32) -> i16 {
    if sample.is_nan() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// The stream server's end of the ring.
pub struct AudioReader {
    shared: Arc<Shared>,
}

impl AudioReader {
    /// Frames waiting.
    pub fn available(&self) -> usize {
        let written = self.shared.written.load(Ordering::Acquire);
        written.wrapping_sub(self.shared.read.load(Ordering::Relaxed))
    }

    /// Frames the writer dropped on a full ring so far.
    pub fn overflow(&self) -> u64 {
        self.shared.overflow.load(Ordering::Relaxed)
    }

    /// Appends up to `frames` frames to `out` as little-endian 16-bit
    /// samples; returns the frames read.
    pub fn read_into(&mut self, out: &mut Vec<u8>, frames: usize) -> usize {
        let shared = &*self.shared;
        let capacity = shared.slots.len();
        let read = shared.read.load(Ordering::Relaxed);
        let count = frames.min(self.available());
        for index in 0..count {
            let frame = shared.slots[read.wrapping_add(index) % capacity].load(Ordering::Relaxed);
            out.extend_from_slice(&(frame as u16).to_le_bytes());
            out.extend_from_slice(&((frame >> 16) as u16).to_le_bytes());
        }
        shared
            .read
            .store(read.wrapping_add(count), Ordering::Release);
        count
    }

    /// Drops all but the newest `keep` frames; returns the frames dropped.
    pub fn skip_to_newest(&mut self, keep: usize) -> usize {
        let available = self.available();
        let skip = available.saturating_sub(keep);
        let read = self.shared.read.load(Ordering::Relaxed);
        self.shared
            .read
            .store(read.wrapping_add(skip), Ordering::Release);
        skip
    }
}
