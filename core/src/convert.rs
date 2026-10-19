//! Sample conversions on the audio path: the renderer's output stage
//! (S63-S65) and the non-finite rules that apply to it (S123, S125).
//!
//! Every function here is O(N), never allocates, locks or traps, whatever the
//! input holds (NaN, ±infinity, subnormals).

#![forbid(unsafe_code)]

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
