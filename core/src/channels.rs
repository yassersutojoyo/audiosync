//! Channel mapping at the capture edge of the pipeline: an input device's
//! channels to interleaved stereo (S14). The output edge (stereo to a device's
//! channels, S63) belongs to the renderer's output stage.
//!
//! Works a whole interleaved buffer at a time and is real-time safe: no
//! allocation, lock or system call (S126).

#![forbid(unsafe_code)]

/// Converts an interleaved input block with `channels` channels to interleaved
/// stereo (S14): the first two channels become L and R, a one-channel input is
/// copied to both, and channels beyond the second are ignored.
///
/// Converts as many whole frames as both buffers hold and returns that count.
/// Nothing is written when `channels` is 0.
pub fn input_to_stereo(input: &[f32], channels: usize, stereo: &mut [f32]) -> usize {
    if channels == 0 {
        return 0;
    }
    let mut frames = 0;
    for (source, target) in input.chunks_exact(channels).zip(stereo.chunks_exact_mut(2)) {
        let left = source[0];
        let right = if channels == 1 { left } else { source[1] };
        target[0] = left;
        target[1] = right;
        frames += 1;
    }
    frames
}
