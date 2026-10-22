//! The host's audio handoff (C8), the stream formats (C62, C63) and their
//! encoders.
//!
//! The host writes stereo frames into a preallocated single-producer,
//! single-consumer ring; the stream server's pump thread reads them. The
//! write never allocates, locks, blocks or logs.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};

/// Channels of every stream (C63).
pub const CHANNELS: usize = 2;
/// Bytes of one 16-bit stereo frame.
pub const FRAME_BYTES: usize = 4;

/// Bits per sample of every stream (C63).
pub const BITS_PER_SAMPLE: u32 = 16;

/// The container and codec of one session's stream (C63).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// 16-bit LPCM in WAV, the first choice.
    Wav,
    /// FLAC in its native stream format, the fallback (C84).
    Flac,
}

impl Codec {
    /// The name the host stores per device (C84), also the file extension.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Wav => "wav",
            Codec::Flac => "flac",
        }
    }

    pub fn from_name(name: &str) -> Option<Codec> {
        match name {
            "wav" => Some(Codec::Wav),
            "flac" => Some(Codec::Flac),
            _ => None,
        }
    }
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

    pub fn flac(rate: u32) -> Option<StreamFormat> {
        StreamFormat::new(Codec::Flac, rate)
    }

    /// The same rate in another codec.
    pub fn with_codec(self, codec: Codec) -> StreamFormat {
        StreamFormat { codec, ..self }
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
            Codec::Flac => "audio/flac",
        }
    }

    /// The file extension of the stream path.
    pub fn extension(self) -> &'static str {
        self.codec.name()
    }

    /// Frames in one 20 ms block (C64).
    pub fn block_frames(self) -> usize {
        self.rate as usize / 50
    }

    /// What the encoder adds to the latency (C72). None for WAV, and none
    /// for FLAC either: each 20 ms block the pump hands over becomes one
    /// whole FLAC frame and is sent at once.
    pub fn encoder_delay(self) -> Duration {
        Duration::ZERO
    }

    /// The header every response starts with (C61, C63). For WAV the RIFF
    /// and data sizes are unknown and hold their largest value; for FLAC
    /// the STREAMINFO total sample count is 0 (unknown).
    pub fn stream_header(self) -> Vec<u8> {
        match self.codec {
            Codec::Wav => wav_header(self.rate),
            Codec::Flac => flac_header(self.rate, self.block_frames() as u16),
        }
    }

    /// A fresh encoder for one response (C61).
    pub fn encoder(self) -> Result<BlockEncoder, EncodeError> {
        let flac = match self.codec {
            Codec::Wav => None,
            Codec::Flac => Some(FlacEncoder::new(self)?),
        };
        Ok(BlockEncoder { flac })
    }
}

/// Why a block could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodeError(pub String);

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not encode the stream: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

fn encode_error(what: impl fmt::Debug) -> EncodeError {
    EncodeError(format!("{what:?}"))
}

/// Turns the pump's 20 ms blocks of little-endian 16-bit stereo into the
/// bytes of one response's body.
pub struct BlockEncoder {
    flac: Option<FlacEncoder>,
}

impl BlockEncoder {
    /// The bytes for one block; WAV passes it through.
    pub fn encode(&mut self, block: Vec<u8>) -> Result<Vec<u8>, EncodeError> {
        match &mut self.flac {
            None => Ok(block),
            Some(flac) => flac.encode(&block),
        }
    }
}

/// Frame numbers of a fixed-block-size stream are 31-bit (RFC 9639 §9.1.5).
const FLAC_FRAME_NUMBERS: usize = 1 << 31;

/// FLAC frames of a fixed block size, one per pump block, numbered from 0
/// in each response.
struct FlacEncoder {
    config: Verified<config::Encoder>,
    info: StreamInfo,
    buffer: FrameBuf,
    block_frames: usize,
    frame_number: usize,
}

impl FlacEncoder {
    fn new(format: StreamFormat) -> Result<FlacEncoder, EncodeError> {
        let block_frames = format.block_frames();
        let config = config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| encode_error(error))?;
        let info = StreamInfo::new(format.rate as usize, CHANNELS, BITS_PER_SAMPLE as usize)
            .map_err(encode_error)?;
        let buffer = FrameBuf::with_size(CHANNELS, block_frames).map_err(encode_error)?;
        Ok(FlacEncoder {
            config,
            info,
            buffer,
            block_frames,
            frame_number: 0,
        })
    }

    fn encode(&mut self, block: &[u8]) -> Result<Vec<u8>, EncodeError> {
        // Only the last frame of a fixed-block-size stream may be shorter,
        // and a live stream has no last frame.
        if block.len() != self.block_frames * FRAME_BYTES {
            return Err(EncodeError(format!(
                "a block of {} bytes is not {} frames",
                block.len(),
                self.block_frames
            )));
        }
        self.buffer.fill_le_bytes(block, 2).map_err(encode_error)?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.buffer,
            self.frame_number,
            &self.info,
        )
        .map_err(encode_error)?;
        self.frame_number = (self.frame_number + 1) % FLAC_FRAME_NUMBERS;
        let mut sink = ByteSink::with_capacity(frame.count_bits());
        frame.write(&mut sink).map_err(encode_error)?;
        Ok(sink.into_inner())
    }
}

/// `fLaC`, then STREAMINFO as the only metadata block (RFC 9639 §6, §8.1,
/// §8.2): a fixed block size, frame sizes unknown (0), the rate, two
/// channels of 16 bits, a total sample count of 0 (unknown) and no MD5.
fn flac_header(rate: u32, block_frames: u16) -> Vec<u8> {
    let mut header = Vec::with_capacity(42);
    header.extend_from_slice(b"fLaC");
    // The last metadata block, type 0 (STREAMINFO), 34 bytes long.
    header.extend_from_slice(&[0x80, 0, 0, 34]);
    header.extend_from_slice(&block_frames.to_be_bytes());
    header.extend_from_slice(&block_frames.to_be_bytes());
    header.extend_from_slice(&[0; 6]);
    // 20 bits of rate, 3 of channels - 1, 5 of bits - 1, 36 of samples.
    let packed: u64 = (u64::from(rate) << 44)
        | ((CHANNELS as u64 - 1) << 41)
        | ((u64::from(BITS_PER_SAMPLE) - 1) << 36);
    header.extend_from_slice(&packed.to_be_bytes());
    header.extend_from_slice(&[0; 16]);
    header
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
