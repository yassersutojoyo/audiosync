//! Sample conversions on the audio path: float to the pipe's PCM16 (S83,
//! S86), the renderer's output stage (S63-S65) and the non-finite rules that
//! apply to them (S123, S125).
//!
//! Every function here is O(N), never allocates, locks or traps, whatever the
//! input holds (NaN, ±infinity, subnormals).

#![forbid(unsafe_code)]

/// PCM16 full scale used by the float to int16 mapping (S86).
pub const PCM16_FULL_SCALE: f64 = 32_767.0;

/// Bytes of one pipe frame: two little-endian int16 samples, L then R (S83).
pub const PIPE_FRAME_BYTES: usize = 4;

/// Converts one float sample to int16 (S86): NaN gives 0; anything else gives
/// `round(clamp(x, -1, +1) × 32767)`, ties away from zero, so +∞ gives 32767
/// and -∞ gives -32767. Computed in f64, where the product is exact.
#[inline]
pub fn f32_to_i16(x: f32) -> i16 {
    if x.is_nan() {
        return 0;
    }
    // In range by construction; `as` would also saturate.
    (f64::from(x).clamp(-1.0, 1.0) * PCM16_FULL_SCALE).round() as i16
}

/// Converts `min(src.len(), dst.len())` samples with [`f32_to_i16`] and
/// returns that count.
pub fn f32_to_i16_slice(src: &[f32], dst: &mut [i16]) -> usize {
    let n = src.len().min(dst.len());
    for (d, &s) in dst[..n].iter_mut().zip(&src[..n]) {
        *d = f32_to_i16(s);
    }
    n
}

/// Packs one stereo frame into the pipe's frame word: L in the low 16 bits,
/// R in the high 16 bits, so `to_le_bytes` gives the wire order L, R (S83).
#[inline]
pub fn pack_frame(left: i16, right: i16) -> u32 {
    u32::from(left as u16) | (u32::from(right as u16) << 16)
}

/// Splits a frame word from [`pack_frame`] back into (L, R).
#[inline]
pub fn unpack_frame(frame: u32) -> (i16, i16) {
    (frame as u16 as i16, (frame >> 16) as u16 as i16)
}

/// The four wire bytes of a frame word: little-endian L, then little-endian R
/// (S83).
#[inline]
pub fn frame_bytes(frame: u32) -> [u8; PIPE_FRAME_BYTES] {
    frame.to_le_bytes()
}

/// Converts interleaved stereo floats to packed PCM16 frame words (S83, S86).
/// Converts `min(src.len() / 2, dst.len())` frames and returns that count.
pub fn pack_stereo(src: &[f32], dst: &mut [u32]) -> usize {
    let frames = (src.len() / 2).min(dst.len());
    for (d, s) in dst[..frames].iter_mut().zip(src.chunks_exact(2)) {
        *d = pack_frame(f32_to_i16(s[0]), f32_to_i16(s[1]));
    }
    frames
}

/// Replaces every NaN or ±infinity in `buf` with 0 and returns how many it
/// replaced (S65, S123).
pub fn zero_non_finite(buf: &mut [f32]) -> usize {
    let mut replaced = 0;
    for s in buf.iter_mut() {
        if !s.is_finite() {
            *s = 0.0;
            replaced += 1;
        }
    }
    replaced
}

/// The renderer's output stage: maps `frames` interleaved stereo frames from
/// `src` onto a device buffer of `channels` interleaved channels, multiplied
/// by `gain` (S63, S64, S65).
///
/// L goes to the first channel and R to the second; further channels get 0;
/// a one-channel device gets (L + R) / 2. Every value that would not be
/// finite is written as 0. Converts at most as many frames as both buffers
/// hold and returns that count; `channels == 0` converts nothing.
pub fn map_to_device(
    src: &[f32],
    frames: usize,
    gain: f32,
    dst: &mut [f32],
    channels: usize,
) -> usize {
    if channels == 0 {
        return 0;
    }
    let frames = frames.min(src.len() / 2).min(dst.len() / channels);
    let finite = |x: f32| if x.is_finite() { x } else { 0.0 };
    for (out, s) in dst[..frames * channels]
        .chunks_exact_mut(channels)
        .zip(src.chunks_exact(2))
    {
        if channels == 1 {
            // Halve before adding so two large finite samples cannot overflow.
            out[0] = finite((0.5 * s[0] + 0.5 * s[1]) * gain);
        } else {
            out[0] = finite(s[0] * gain);
            out[1] = finite(s[1] * gain);
            out[2..].fill(0.0);
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S86
    #[test]
    fn mapping_follows_the_rounding_rule() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(-0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32_767);
        assert_eq!(f32_to_i16(-1.0), -32_767);
        assert_eq!(f32_to_i16(0.5), 16_384, "16383.5 rounds away from zero");
        assert_eq!(f32_to_i16(-0.5), -16_384, "-16383.5 rounds away from zero");
        assert_eq!(f32_to_i16(2.0), 32_767, "clamped");
        assert_eq!(f32_to_i16(-7.5), -32_767, "clamped");
        assert_eq!(f32_to_i16(1.0 / 32_767.0), 1);
        assert_eq!(f32_to_i16(0.4 / 32_767.0), 0);
        assert_eq!(f32_to_i16(f32::MIN_POSITIVE / 4.0), 0, "subnormal");
    }

    // covers: S86
    #[test]
    fn rounding_uses_the_exact_product() {
        // Near every half step, an f32 product can round onto (or off) the
        // .5 and flip the result; the mapping must follow the exact product.
        let mut f32_would_differ = 0;
        for k in -32_767i32..32_767 {
            let near = ((f64::from(k) + 0.5) / 32_767.0) as f32;
            for x in [
                f32::from_bits(near.to_bits() - 1),
                near,
                f32::from_bits(near.to_bits() + 1),
            ] {
                let exact = (f64::from(x) * 32_767.0).round();
                assert_eq!(f64::from(f32_to_i16(x)), exact, "x = {x:e}");
                if f64::from((x * 32_767.0).round()) != exact {
                    f32_would_differ += 1;
                }
            }
        }
        assert!(f32_would_differ > 0, "the check above exercises real ties");
        // An exact tie: 0.5 × 32767 = 16383.5 is representable.
        assert_eq!(f32_to_i16(0.5), 16_384);
        assert_eq!(f32_to_i16(-0.5), -16_384);
    }

    // covers: S86, S123
    #[test]
    fn non_finite_values_never_trap() {
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert_eq!(f32_to_i16(-f32::NAN), 0);
        assert_eq!(f32_to_i16(f32::from_bits(0x7fa0_0001)), 0, "signaling NaN");
        assert_eq!(f32_to_i16(f32::INFINITY), 32_767);
        assert_eq!(f32_to_i16(f32::NEG_INFINITY), -32_767);
        assert_eq!(f32_to_i16(f32::MAX), 32_767);
        assert_eq!(f32_to_i16(f32::MIN), -32_767);
        let src = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25];
        let mut frames = [0u32; 2];
        assert_eq!(pack_stereo(&src, &mut frames), 2);
        assert_eq!(unpack_frame(frames[0]), (0, 32_767));
        assert_eq!(unpack_frame(frames[1]), (-32_767, 8192));
    }

    // covers: S83
    #[test]
    fn wire_frames_are_little_endian_left_then_right() {
        let frame = pack_frame(0x1234, -2);
        assert_eq!(frame_bytes(frame), [0x34, 0x12, 0xfe, 0xff]);
        assert_eq!(unpack_frame(frame), (0x1234, -2));
        assert_eq!(PIPE_FRAME_BYTES, 4);
        // One second at 48 kHz is 192 000 bytes.
        assert_eq!(48_000 * PIPE_FRAME_BYTES, 192_000);
        let mut out = [0i16; 3];
        assert_eq!(f32_to_i16_slice(&[1.0, -1.0, 0.0, 0.5], &mut out), 3);
        assert_eq!(out, [32_767, -32_767, 0]);
    }

    // covers: S86
    #[test]
    fn every_int16_level_is_reachable_and_monotonic() {
        let mut previous = i16::MIN;
        for k in -32_767i32..=32_767 {
            let x = (f64::from(k) / 32_767.0) as f32;
            let v = f32_to_i16(x);
            assert_eq!(i32::from(v), k);
            assert!(v >= previous);
            previous = v;
        }
    }

    // covers: S65, S123
    #[test]
    fn non_finite_samples_are_zeroed_at_the_output_stage() {
        let mut buf = [0.5, f32::NAN, f32::INFINITY, -1.0, f32::NEG_INFINITY, -0.0];
        assert_eq!(zero_non_finite(&mut buf), 3);
        assert_eq!(buf, [0.5, 0.0, 0.0, -1.0, 0.0, -0.0]);
    }

    // covers: S63, S65
    #[test]
    fn channels_are_mapped_left_right_then_zeros() {
        let src = [0.5, -0.25, 1.0, 0.75];
        let mut quad = [9.0f32; 8];
        assert_eq!(map_to_device(&src, 2, 1.0, &mut quad, 4), 2);
        assert_eq!(quad, [0.5, -0.25, 0.0, 0.0, 1.0, 0.75, 0.0, 0.0]);
        let mut mono = [9.0f32; 2];
        assert_eq!(map_to_device(&src, 2, 1.0, &mut mono, 1), 2);
        assert_eq!(mono, [0.125, 0.875]);
        let mut stereo = [9.0f32; 4];
        map_to_device(
            &[f32::MAX, f32::MAX, f32::NAN, f32::INFINITY],
            2,
            1.0,
            &mut stereo,
            2,
        );
        assert_eq!(stereo, [f32::MAX, f32::MAX, 0.0, 0.0]);
        let mut mono = [9.0f32; 1];
        map_to_device(&[f32::MAX, f32::MAX], 1, 1.0, &mut mono, 1);
        assert_eq!(mono, [f32::MAX], "no overflow when averaging");
        assert_eq!(map_to_device(&src, 2, 1.0, &mut mono, 0), 0);
    }

    // covers: S64, S65
    #[test]
    fn gain_scales_every_frame_and_zero_is_exact_silence() {
        let src = [0.5, -0.25, 1.0, f32::NAN];
        let mut out = [9.0f32; 4];
        map_to_device(&src, 2, 0.5, &mut out, 2);
        assert_eq!(out, [0.25, -0.125, 0.5, 0.0]);
        map_to_device(&src, 2, 0.0, &mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0), "gain 0 yields exact zeros");
        map_to_device(&src, 2, f32::INFINITY, &mut out, 2);
        assert!(out.iter().all(|s| s.is_finite()));
        map_to_device(&src, 2, f32::NAN, &mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
