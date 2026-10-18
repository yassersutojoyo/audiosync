//! Rate conversion and a local output's output stage through the C interface
//! (S62 to S65).
//!
//! A resampler handle has no shared state: all its calls run on one thread at
//! a time (S62: the output's IO thread, after the control thread created it).
//! A gain stage has two sides in separate fields: the [`SharedGain`] that any
//! thread sets with a relaxed atomic store (S133), and the [`GainStage`] that
//! the output thread runs.

use std::ptr;

use super::{host_slice, host_slice_mut, samples_for, stereo_samples};
use crate::convert;
use crate::resample::Resampler;
use crate::volume::{GainStage, SharedGain};

/// A streaming resampler behind an opaque C pointer (S62, S85).
pub struct AscResampler {
    resampler: Resampler,
}

/// What one `asc_resampler_process` call consumed and produced.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscResampleCount {
    pub input_frames: usize,
    pub output_frames: usize,
}

/// A local output's output stage behind an opaque C pointer: the gain any
/// thread sets and the ramp the output thread applies (S63 to S65).
pub struct AscGainStage {
    gain: SharedGain,
    stage: GainStage,
}

/// Creates a resampler; null unless both rates are supported (S16).
#[unsafe(no_mangle)]
pub extern "C" fn asc_resampler_create(input_rate: u32, output_rate: u32) -> *mut AscResampler {
    match Resampler::new(input_rate, output_rate) {
        Ok(resampler) => Box::into_raw(Box::new(AscResampler { resampler })),
        Err(_) => ptr::null_mut(),
    }
}

/// Destroys a resampler.
///
/// # Safety
///
/// `resampler` is null or a pointer returned by [`asc_resampler_create`] that
/// was not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_destroy(resampler: *mut AscResampler) {
    if resampler.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_resampler_create,
    // and by the caller's contract it is destroyed once, with no call in
    // progress.
    drop(unsafe { Box::from_raw(resampler) });
}

/// Changes the input rate and resets the phase and history (S94); false for
/// an unsupported rate. Never allocates.
///
/// # Safety
///
/// `resampler` is null or a live handle that no other call uses now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_set_input_rate(
    resampler: *mut AscResampler,
    input_rate: u32,
) -> bool {
    // SAFETY: by the caller's contract the handle is null or live and only
    // this call uses it now.
    unsafe { resampler.as_mut() }
        .is_some_and(|handle| handle.resampler.set_input_rate(input_rate).is_ok())
}

/// Clears the history and the phase (S94).
///
/// # Safety
///
/// `resampler` is null or a live handle that no other call uses now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_reset(resampler: *mut AscResampler) {
    // SAFETY: by the caller's contract the handle is null or live and only
    // this call uses it now.
    if let Some(handle) = unsafe { resampler.as_mut() } {
        handle.resampler.reset();
    }
}

/// The content delay in input frames.
///
/// # Safety
///
/// `resampler` is null or a live handle that no other call changes now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_latency_frames(resampler: *const AscResampler) -> usize {
    // SAFETY: by the caller's contract the handle is null or live and nothing
    // changes it now.
    unsafe { resampler.as_ref() }.map_or(0, |handle| handle.resampler.latency_frames())
}

/// Input frames still needed before the next `output_frames` output frames
/// (S62). Saturates instead of overflowing. Real-time safe.
///
/// # Safety
///
/// `resampler` is null or a live handle that no other call changes now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_input_frames_for(
    resampler: *const AscResampler,
    output_frames: usize,
) -> usize {
    // SAFETY: by the caller's contract the handle is null or live and nothing
    // changes it now.
    unsafe { resampler.as_ref() }
        .map_or(0, |handle| handle.resampler.input_frames_for(output_frames))
}

/// Converts `input_frames` stereo frames into at most `output_frames`
/// (S62, S85, S125). Real-time safe.
///
/// # Safety
///
/// `resampler` is null or a live handle that no other call uses now; `input`
/// is null or points to `input_frames` × 2 aligned floats that nothing writes
/// during the call; `output` is null or points to `output_frames` × 2
/// aligned, writable floats that nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_resampler_process(
    resampler: *mut AscResampler,
    input: *const f32,
    input_frames: usize,
    output: *mut f32,
    output_frames: usize,
) -> AscResampleCount {
    let (Some(input_samples), Some(output_samples)) =
        (stereo_samples(input_frames), stereo_samples(output_frames))
    else {
        return AscResampleCount::default();
    };
    // SAFETY: by the caller's contract `input` is null or holds that many
    // readable floats that nothing writes during the call.
    let Some(input) = (unsafe { host_slice(input, input_samples) }) else {
        return AscResampleCount::default();
    };
    // SAFETY: by the caller's contract `output` is null or holds that many
    // writable floats that only this call touches.
    let Some(output) = (unsafe { host_slice_mut(output, output_samples) }) else {
        return AscResampleCount::default();
    };
    // SAFETY: by the caller's contract the handle is null or live and only
    // this call uses it now.
    let Some(handle) = (unsafe { resampler.as_mut() }) else {
        return AscResampleCount::default();
    };
    let (consumed, produced) = handle.resampler.process(input, output);
    AscResampleCount {
        input_frames: consumed,
        output_frames: produced,
    }
}

/// Creates an output stage at linear gain `gain` (S64).
#[unsafe(no_mangle)]
pub extern "C" fn asc_gain_stage_create(gain: f32) -> *mut AscGainStage {
    Box::into_raw(Box::new(AscGainStage {
        gain: SharedGain::new(gain),
        stage: GainStage::new(),
    }))
}

/// Destroys an output stage.
///
/// # Safety
///
/// `stage` is null or a pointer returned by [`asc_gain_stage_create`] that
/// was not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_gain_stage_destroy(stage: *mut AscGainStage) {
    if stage.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_gain_stage_create,
    // and by the caller's contract it is destroyed once, with no call in
    // progress.
    drop(unsafe { Box::from_raw(stage) });
}

/// The shared gain of an output stage.
///
/// # Safety
///
/// `stage` is null or a live output stage.
unsafe fn shared_gain<'a>(stage: *const AscGainStage) -> Option<&'a SharedGain> {
    if stage.is_null() {
        return None;
    }
    // SAFETY: the handle is live; this borrows only the `gain` field, which
    // the output thread only reads, atomically.
    Some(unsafe { &(*stage).gain })
}

/// Any thread: sets the linear gain; the output thread applies it from its
/// next buffer (S64, S133).
///
/// # Safety
///
/// `stage` is null or a live output stage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_gain_stage_set_gain(stage: *mut AscGainStage, gain: f32) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(shared) = unsafe { shared_gain(stage) } {
        shared.set(gain);
    }
}

/// Any thread: the gain last set.
///
/// # Safety
///
/// `stage` is null or a live output stage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_gain_stage_gain(stage: *const AscGainStage) -> f32 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { shared_gain(stage) }.map_or(0.0, SharedGain::get)
}

/// The output thread's side of an output stage and the gain to ramp to.
///
/// # Safety
///
/// `stage` is null or a live output stage whose output-thread calls run on
/// one thread at a time.
unsafe fn output_side<'a>(stage: *mut AscGainStage) -> Option<(&'a mut GainStage, f32)> {
    if stage.is_null() {
        return None;
    }
    // SAFETY: the handle is live; `gain` is only read here, atomically.
    let target = unsafe { (*stage).gain.get() };
    // SAFETY: output-thread calls run on one thread at a time and are the
    // only ones that borrow the `stage` field.
    Some((unsafe { &mut (*stage).stage }, target))
}

/// Output thread: applies the gain to `frames` × `channels` interleaved
/// samples in place (S64, S65). Real-time safe.
///
/// # Safety
///
/// `stage` is null or a live output stage whose output-thread calls run on
/// one thread at a time; `interleaved` is null or points to `frames` ×
/// `channels` aligned, writable floats that nothing else touches during the
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_gain_stage_apply(
    stage: *mut AscGainStage,
    interleaved: *mut f32,
    frames: usize,
    channels: usize,
) {
    let Some(samples) = samples_for(frames, channels) else {
        return;
    };
    // SAFETY: by the caller's contract the buffer is null or holds `samples`
    // writable floats that only this call touches.
    let Some(buffer) = (unsafe { host_slice_mut(interleaved, samples) }) else {
        return;
    };
    // SAFETY: by the caller's contract the handle is null or live and its
    // output-thread calls run on one thread at a time.
    if let Some((stage, target)) = unsafe { output_side(stage) } {
        stage.apply(buffer, channels, target);
    }
}

/// Output thread: maps `frames` stereo frames onto `channels` device channels
/// (S63), then applies the gain (S64, S65). Returns the frames written.
/// Real-time safe.
///
/// # Safety
///
/// `stage` is null or a live output stage whose output-thread calls run on
/// one thread at a time; `stereo` is null or points to `frames` × 2 aligned
/// floats that nothing writes during the call; `device` is null or points to
/// `frames` × `channels` aligned, writable floats that nothing else touches
/// during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_gain_stage_render(
    stage: *mut AscGainStage,
    stereo: *const f32,
    frames: usize,
    device: *mut f32,
    channels: usize,
) -> usize {
    let (Some(stereo_len), Some(device_len)) =
        (stereo_samples(frames), samples_for(frames, channels))
    else {
        return 0;
    };
    // SAFETY: by the caller's contract `stereo` is null or holds that many
    // readable floats that nothing writes during the call.
    let Some(source) = (unsafe { host_slice(stereo, stereo_len) }) else {
        return 0;
    };
    // SAFETY: by the caller's contract `device` is null or holds that many
    // writable floats that only this call touches.
    let Some(target_buffer) = (unsafe { host_slice_mut(device, device_len) }) else {
        return 0;
    };
    // SAFETY: by the caller's contract the handle is null or live and its
    // output-thread calls run on one thread at a time.
    let Some((stage, gain)) = (unsafe { output_side(stage) }) else {
        return 0;
    };
    let written = convert::map_to_device(source, frames, 1.0, target_buffer, channels);
    stage.apply(&mut target_buffer[..written * channels], channels, gain);
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave(frames: usize, seed: u32) -> Vec<f32> {
        (0..frames * 2)
            .map(|i| {
                let x = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed);
                ((x >> 8) as f32 / (1u32 << 23) as f32 - 1.0) * 0.9
            })
            .collect()
    }

    // covers: S16, S62, S85, S94
    #[test]
    fn a_resampler_through_the_c_interface_matches_the_rust_one() {
        assert!(asc_resampler_create(7_999, 48_000).is_null());
        assert!(asc_resampler_create(44_100, 384_001).is_null());
        let handle = asc_resampler_create(44_100, 48_000);
        assert!(!handle.is_null());
        let mut reference = Resampler::new(44_100, 48_000).unwrap();
        let input = wave(4410, 1);
        // SAFETY: `handle` is live; every buffer holds the stated frames.
        unsafe {
            assert_eq!(
                asc_resampler_latency_frames(handle),
                reference.latency_frames()
            );
            let mut chunk = &input[..];
            let mut produced = 0usize;
            while !chunk.is_empty() {
                assert_eq!(
                    asc_resampler_input_frames_for(handle, 300),
                    reference.input_frames_for(300)
                );
                let mut through_c = vec![0.0f32; 600];
                let mut direct = vec![0.0f32; 600];
                let count = asc_resampler_process(
                    handle,
                    chunk.as_ptr(),
                    (chunk.len() / 2).min(441),
                    through_c.as_mut_ptr(),
                    300,
                );
                let (used, made) = reference.process(&chunk[..(chunk.len()).min(882)], &mut direct);
                assert_eq!((count.input_frames, count.output_frames), (used, made));
                assert_eq!(through_c, direct);
                chunk = &chunk[used * 2..];
                produced += made;
            }
            // Drain what the history still holds: no input (a null buffer
            // with 0 frames is an empty one).
            loop {
                let mut through_c = vec![0.0f32; 600];
                let mut direct = vec![0.0f32; 600];
                let count =
                    asc_resampler_process(handle, ptr::null(), 0, through_c.as_mut_ptr(), 300);
                let (used, made) = reference.process(&[], &mut direct);
                assert_eq!((count.input_frames, count.output_frames), (used, made));
                assert_eq!(through_c, direct);
                produced += made;
                if made == 0 {
                    break;
                }
            }
            // 4410 frames at 44.1 kHz make exactly 4800 at 48 kHz.
            assert_eq!(produced, 4800);
            assert!(asc_resampler_set_input_rate(handle, 48_000));
            assert!(!asc_resampler_set_input_rate(handle, 400_000));
            let mut out = vec![0.0f32; 1024];
            let count = asc_resampler_process(handle, input.as_ptr(), 512, out.as_mut_ptr(), 512);
            assert_eq!(
                count,
                AscResampleCount {
                    input_frames: 512,
                    output_frames: 512
                }
            );
            assert_eq!(out, input[..1024], "equal rates pass through bit-exact");
            asc_resampler_reset(handle);
            asc_resampler_destroy(handle);
        }
    }

    // covers: S85
    #[test]
    fn resampled_frame_counts_keep_pace_with_the_input() {
        for rate in [44_100u32, 96_000, 32_000] {
            let handle = asc_resampler_create(rate, 48_000);
            let input = wave(441, 2);
            let mut output = vec![0.0f32; 2 * 2048];
            let mut produced = 0usize;
            // SAFETY: `handle` is live; the buffers hold the stated frames.
            unsafe {
                for _ in 0..200 {
                    let count = asc_resampler_process(
                        handle,
                        input.as_ptr(),
                        441,
                        output.as_mut_ptr(),
                        2048,
                    );
                    assert_eq!(count.input_frames, 441);
                    produced += count.output_frames;
                }
                asc_resampler_destroy(handle);
            }
            let exact = 200.0 * 441.0 * 48_000.0 / f64::from(rate);
            assert!((produced as f64 - exact).abs() <= 2.0, "{rate}: {produced}");
        }
    }

    // covers: S125
    #[test]
    fn non_finite_input_never_leaves_the_resampler() {
        let handle = asc_resampler_create(44_100, 48_000);
        let mut input = wave(441, 3);
        input[10] = f32::NAN;
        input[11] = f32::INFINITY;
        let mut output = vec![0.0f32; 2 * 600];
        // SAFETY: `handle` is live; the buffers hold the stated frames.
        unsafe {
            let count =
                asc_resampler_process(handle, input.as_ptr(), 441, output.as_mut_ptr(), 600);
            assert!(
                output[..count.output_frames * 2]
                    .iter()
                    .all(|x| x.is_finite())
            );
            asc_resampler_destroy(handle);
        }
    }

    // covers: S94
    #[test]
    fn a_reset_starts_a_new_stream() {
        let handle = asc_resampler_create(44_100, 48_000);
        let input = wave(441, 5);
        let mut first = vec![0.0f32; 2 * 600];
        let mut again = vec![0.0f32; 2 * 600];
        // SAFETY: `handle` is live; the buffers hold the stated frames.
        unsafe {
            let a = asc_resampler_process(handle, input.as_ptr(), 441, first.as_mut_ptr(), 600);
            asc_resampler_process(handle, input.as_ptr(), 441, again.as_mut_ptr(), 600);
            assert_ne!(first, again, "the second block continues the stream");
            asc_resampler_reset(handle);
            again.fill(0.0);
            let b = asc_resampler_process(handle, input.as_ptr(), 441, again.as_mut_ptr(), 600);
            assert_eq!((a, &first), (b, &again), "after a reset: a fresh stream");
            asc_resampler_destroy(handle);
        }
    }

    #[test]
    fn huge_requests_saturate_instead_of_panicking() {
        let handle = asc_resampler_create(383_999, 48_000);
        // SAFETY: `handle` is live, then destroyed once.
        unsafe {
            assert_eq!(
                asc_resampler_input_frames_for(handle, usize::MAX),
                usize::MAX
            );
            assert!(asc_resampler_input_frames_for(handle, 1 << 40) > 1 << 40);
            asc_resampler_destroy(handle);
        }
    }

    // covers: S63, S64, S65
    #[test]
    fn the_output_stage_maps_channels_and_ramps_like_the_rust_one() {
        let source = wave(256, 4);
        for channels in [1usize, 2, 3, 6] {
            let stage = asc_gain_stage_create(0.5);
            let mut reference = GainStage::new();
            let mut gain = 0.5f32;
            for round in 0..4 {
                if round == 2 {
                    gain = 0.125;
                    // SAFETY: `stage` is live.
                    unsafe { asc_gain_stage_set_gain(stage, gain) };
                }
                let mut through_c = vec![f32::NAN; 256 * channels];
                // SAFETY: `stage` is live; the buffers hold the stated frames.
                let written = unsafe {
                    asc_gain_stage_render(
                        stage,
                        source.as_ptr(),
                        256,
                        through_c.as_mut_ptr(),
                        channels,
                    )
                };
                assert_eq!(written, 256);
                let mut direct = vec![0.0f32; 256 * channels];
                convert::map_to_device(&source, 256, 1.0, &mut direct, channels);
                reference.apply(&mut direct, channels, gain);
                assert_eq!(through_c, direct, "{channels} channels, round {round}");
            }
            // SAFETY: `stage` is live, then destroyed once.
            unsafe {
                assert_eq!(asc_gain_stage_gain(stage), 0.125);
                asc_gain_stage_destroy(stage);
            }
        }
    }

    // covers: S64, S65, S123
    #[test]
    fn the_gain_stage_zeroes_silence_and_non_finite_values() {
        let stage = asc_gain_stage_create(f32::NAN);
        let mut block = vec![0.5f32, f32::NAN, f32::INFINITY, -1.0, 0.25, 0.75];
        // SAFETY: `stage` is live; `block` holds 3 stereo frames.
        unsafe {
            assert_eq!(asc_gain_stage_gain(stage), 0.0, "NaN counts as 0");
            asc_gain_stage_apply(stage, block.as_mut_ptr(), 3, 2);
            assert!(
                block.iter().all(|x| x.to_bits() == 0),
                "gain 0: exact zeros"
            );
            asc_gain_stage_set_gain(stage, 2.0);
            let mut loud = vec![f32::MAX, f32::NAN, 0.25, -0.25];
            // The first buffer after a change ramps from 0 to 2.
            asc_gain_stage_apply(stage, loud.as_mut_ptr(), 2, 2);
            asc_gain_stage_apply(stage, loud.as_mut_ptr(), 2, 2);
            assert!(loud.iter().all(|x| x.is_finite()));
            asc_gain_stage_set_gain(stage, -3.0);
            assert_eq!(asc_gain_stage_gain(stage), 0.0, "negative counts as 0");
            asc_gain_stage_destroy(stage);
        }
    }

    #[test]
    fn null_handles_and_bad_buffers_do_nothing() {
        let mut block = [0.5f32; 8];
        let source = [0.25f32; 8];
        // SAFETY: every call gets a null handle, a null buffer or a count no
        // buffer can hold, which the interface allows.
        unsafe {
            let null_resampler = ptr::null_mut::<AscResampler>();
            asc_resampler_destroy(null_resampler);
            assert!(!asc_resampler_set_input_rate(null_resampler, 48_000));
            asc_resampler_reset(null_resampler);
            assert_eq!(asc_resampler_latency_frames(null_resampler), 0);
            assert_eq!(asc_resampler_input_frames_for(null_resampler, 64), 0);
            assert_eq!(
                asc_resampler_process(null_resampler, source.as_ptr(), 4, block.as_mut_ptr(), 4),
                AscResampleCount::default()
            );
            let null_stage = ptr::null_mut::<AscGainStage>();
            asc_gain_stage_destroy(null_stage);
            asc_gain_stage_set_gain(null_stage, 1.0);
            assert_eq!(asc_gain_stage_gain(null_stage), 0.0);
            asc_gain_stage_apply(null_stage, block.as_mut_ptr(), 4, 2);
            assert_eq!(
                asc_gain_stage_render(null_stage, source.as_ptr(), 4, block.as_mut_ptr(), 2),
                0
            );
        }
        assert_eq!(block, [0.5; 8]);
        let resampler = asc_resampler_create(48_000, 44_100);
        let stage = asc_gain_stage_create(1.0);
        // SAFETY: the handles are live; the buffers are null, empty or too
        // large to exist, which the interface allows.
        unsafe {
            let none = AscResampleCount::default();
            assert_eq!(
                asc_resampler_process(resampler, ptr::null(), 4, block.as_mut_ptr(), 4),
                none
            );
            assert_eq!(
                asc_resampler_process(resampler, source.as_ptr(), 4, ptr::null_mut(), 4),
                none
            );
            assert_eq!(
                asc_resampler_process(
                    resampler,
                    source.as_ptr(),
                    usize::MAX,
                    block.as_mut_ptr(),
                    4
                ),
                none
            );
            asc_gain_stage_apply(stage, ptr::null_mut(), 4, 2);
            asc_gain_stage_apply(stage, block.as_mut_ptr(), usize::MAX, 2);
            assert_eq!(
                asc_gain_stage_render(stage, source.as_ptr(), 4, block.as_mut_ptr(), 0),
                0
            );
            assert_eq!(
                asc_gain_stage_render(stage, ptr::null(), 4, block.as_mut_ptr(), 2),
                0
            );
            assert_eq!(
                asc_gain_stage_render(stage, source.as_ptr(), 4, block.as_mut_ptr(), usize::MAX),
                0
            );
            asc_resampler_destroy(resampler);
            asc_gain_stage_destroy(stage);
        }
        assert_eq!(block, [0.5; 8]);
    }
}
