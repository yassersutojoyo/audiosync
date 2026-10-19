//! The stream's audio format and the conversions the input needs (P72, P73,
//! P75, P76, P121).
//!
//! The stream is 44 100 Hz, 16-bit signed, stereo, left then right (P72).
//! The host hands over float32 or 16-bit frames already at 44 100 Hz (P73);
//! the crate keeps them as 16-bit frames and cuts them into packets of 352
//! frames (P75). Nothing here allocates, so the conversions are fit for the
//! host's real-time input call (P6).

use crate::volume::Volume;

/// Frames per second (P72).
pub const SAMPLE_RATE: u32 = 44_100;
/// Channels per frame, left then right (P72).
pub const CHANNELS: usize = 2;
/// Frames in every packet, with either codec (P75).
pub const FRAMES_PER_PACKET: usize = 352;
/// [`FRAMES_PER_PACKET`] as a frame count on the timeline.
pub const PACKET_FRAMES: u64 = FRAMES_PER_PACKET as u64;
/// Bytes of a PCM payload: 352 frames × 2 channels × 2 bytes (P76).
pub const PCM_PAYLOAD_BYTES: usize = FRAMES_PER_PACKET * CHANNELS * 2;

/// One stereo frame: left, right.
pub type Frame = [i16; 2];

/// The 352 frames of one packet.
pub type PacketFrames = [Frame; FRAMES_PER_PACKET];

/// A float sample as 16-bit (P73): multiplied by 32 767, rounded to nearest
/// (halves away from zero) and clamped to the 16-bit range. NaN and
/// infinities become 0.
pub fn sample_from_f32(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    let scaled = (f64::from(sample) * 32_767.0).round();
    scaled.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// A PCM payload (P76): every sample big-endian, left before right, 1408
/// bytes for a full packet. `out` must hold `4 × frames.len()` bytes.
pub fn write_pcm_payload(frames: &[Frame], out: &mut [u8]) {
    for (frame, bytes) in frames.iter().zip(out.as_chunks_mut::<4>().0) {
        bytes[..2].copy_from_slice(&frame[0].to_be_bytes());
        bytes[2..].copy_from_slice(&frame[1].to_be_bytes());
    }
}

/// The frames of a PCM payload, the inverse of [`write_pcm_payload`].
pub fn read_pcm_payload(bytes: &[u8], out: &mut [Frame]) {
    for (frame, chunk) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *frame = [
            i16::from_be_bytes([chunk[0], chunk[1]]),
            i16::from_be_bytes([chunk[2], chunk[3]]),
        ];
    }
}

/// The factor a software volume applies to the samples (P121):
/// 10^(dB/20), and 0 when muted.
pub fn software_gain(volume: Volume) -> f64 {
    match volume {
        Volume::Muted => 0.0,
        Volume::Db(db) => 10f64.powf(db / 20.0),
    }
}

/// Scales samples by `gain` (P121), rounding to nearest and clamping.
pub fn apply_gain(frames: &mut [Frame], gain: f64) {
    if gain == 1.0 {
        return;
    }
    for frame in frames.iter_mut() {
        for sample in frame.iter_mut() {
            let scaled = (f64::from(*sample) * gain).round();
            *sample = scaled.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        }
    }
}
