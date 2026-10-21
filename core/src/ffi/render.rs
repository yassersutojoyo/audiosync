//! Rate conversion and a local output's output stage through the C interface
//! (S62 to S65).
//!
//! A resampler handle has no shared state: all its calls run on one thread at
//! a time (S62: the output's IO thread, after the control thread created it).
//! A gain stage has two sides in separate fields: the [`SharedGain`] that any
//! thread sets with a relaxed atomic store (S133), and the [`GainStage`] that
//! the output thread runs.
//!
//! A renderer handle is a whole local output's renderer with its fades
//! (S62 to S65, S183 to S188): the [`Renderer`] its output thread runs, and
//! the [`RendererControl`] and [`SharedGain`] that every other call uses,
//! whose methods are lock-free atomic operations (S190). A call borrows only
//! the fields it needs, so a render and a control call may run at the same
//! time.

use std::ptr;
use std::sync::Arc;

use super::ring::{AscReaderCounters, AscRing, ring_of};
use super::{host_slice, host_slice_mut, samples_for, stereo_samples};
use crate::clock;
use crate::convert;
use crate::fade::{FadeState, fade_frames};
use crate::reader::Reader;
use crate::render::{Renderer, RendererControl, STOP_FADE_CAP};
use crate::resample::Resampler;
use crate::ring;
use crate::volume::{GainStage, SharedGain};

/// The longest a host waits for its outputs' fade-outs at a stop or a
/// removal, in milliseconds (S186); the header's `ASC_STOP_FADE_CAP_MS`.
pub const STOP_FADE_CAP_MS: u32 = STOP_FADE_CAP.as_millis() as u32;

/// `asc_renderer_fade_state` while the renderer renders exact zeros.
pub const FADE_CLOSED: i32 = 0;
/// `asc_renderer_fade_state` while the renderer fades in.
pub const FADE_FADING_IN: i32 = 1;
/// `asc_renderer_fade_state` while frames pass at full level.
pub const FADE_OPEN: i32 = 2;
/// `asc_renderer_fade_state` while the renderer fades out.
pub const FADE_FADING_OUT: i32 = 3;

/// A local output's renderer behind an opaque C pointer: the renderer its
/// output thread runs, and the control and gain that any thread uses.
pub struct AscRenderer {
    control: RendererControl,
    gain: Arc<SharedGain>,
    /// F at the device rate (S182), fixed at creation.
    fade_frames: usize,
    renderer: Renderer,
}

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

/// Clears the history and the phase (S94). Real-time safe.
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

/// The content delay in input frames, 0 when passing through. Real-time safe.
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
/// (S62, S85, S125). With different rates a non-finite input sample becomes
/// silence and clears the history; equal rates pass every sample through
/// unchanged (S85), NaN and infinity included. Real-time safe.
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

/// Control thread: a renderer reading `ring`, captured at `capture_rate`,
/// for a device that runs at `device_rate` with `channels` interleaved
/// channels, at linear gain `gain`. It starts closed, at a delay of 0 until
/// one is set. Null for a null ring, no channels, or a rate outside 8000 to
/// 384 000 Hz.
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_create(
    ring: *mut AscRing,
    capture_rate: u32,
    device_rate: u32,
    channels: usize,
    gain: f32,
) -> *mut AscRenderer {
    if !ring::is_supported_rate(capture_rate) || !ring::is_supported_rate(device_rate) {
        return ptr::null_mut();
    }
    // SAFETY: by the caller's contract the handle is null or live.
    let Some(ring) = (unsafe { ring_of(ring) }) else {
        return ptr::null_mut();
    };
    let reader = Reader::new(Arc::clone(ring), capture_rate);
    let control = RendererControl::new(&reader);
    let gain = Arc::new(SharedGain::new(gain));
    match Renderer::new(
        reader,
        &control,
        Arc::clone(&gain),
        capture_rate,
        device_rate,
        channels,
    ) {
        Ok(renderer) => Box::into_raw(Box::new(AscRenderer {
            control,
            gain,
            fade_frames: fade_frames(device_rate),
            renderer,
        })),
        Err(_) => ptr::null_mut(),
    }
}

/// Destroys a renderer.
///
/// # Safety
///
/// `renderer` is null or a pointer returned by [`asc_renderer_create`] that
/// was not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_destroy(renderer: *mut AscRenderer) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_renderer_create,
    // and by the caller's contract it is destroyed once, with no call in
    // progress.
    drop(unsafe { Box::from_raw(renderer) });
}

/// The output thread's side of a renderer and a device buffer of `frames`
/// frames.
///
/// # Safety
///
/// `renderer` is null or a live renderer whose output-thread calls run on
/// one thread at a time; `device` is null or points to `frames` × the
/// renderer's channels aligned, writable floats that nothing else touches
/// during `'a`.
unsafe fn output_buffer<'a>(
    renderer: *mut AscRenderer,
    device: *mut f32,
    frames: usize,
) -> Option<(&'a mut Renderer, &'a mut [f32])> {
    if renderer.is_null() {
        return None;
    }
    // SAFETY: output-thread calls run on one thread at a time and are the
    // only ones that borrow the `renderer` field.
    let renderer = unsafe { &mut (*renderer).renderer };
    let samples = samples_for(frames, renderer.channels())?;
    // SAFETY: by the caller's contract the buffer is null or holds `samples`
    // writable floats that only this call touches.
    let buffer = unsafe { host_slice_mut(device, samples) }?;
    Some((renderer, buffer))
}

/// Output thread: fills `frames` frames of the renderer's channels,
/// interleaved: read at the output's delay, converted to the device rate,
/// mapped onto the channels, at the gain, with the fade after them (S62 to
/// S65, S183 to S188). Real-time safe.
///
/// # Safety
///
/// `renderer` is null or a live renderer whose output-thread calls run on
/// one thread at a time; `device` is null or points to `frames` × the
/// renderer's channels aligned, writable floats that nothing else touches
/// during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_render(
    renderer: *mut AscRenderer,
    device: *mut f32,
    frames: usize,
) {
    // SAFETY: by the caller's contract the handle and the buffer are null or
    // valid for this call.
    if let Some((renderer, buffer)) = unsafe { output_buffer(renderer, device, frames) } {
        renderer.render(buffer);
    }
}

/// Output thread: as `asc_renderer_render`, for a callback whose first frame
/// is output at `output_time_ns` host nanoseconds, so that a stop's
/// fade-out publishes when its last frame is output (S185). The callback
/// reads at the time of the call on the core's clock, the one
/// `asc_ring_write` stamps the capture's blocks with, so a converting
/// output follows the capture's clock (S207). Real-time safe.
///
/// # Safety
///
/// As [`asc_renderer_render`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_render_timed(
    renderer: *mut AscRenderer,
    device: *mut f32,
    frames: usize,
    output_time_ns: u64,
) {
    // SAFETY: by the caller's contract the handle and the buffer are null or
    // valid for this call.
    if let Some((renderer, buffer)) = unsafe { output_buffer(renderer, device, frames) } {
        renderer.render_timed_at(buffer, output_time_ns, clock::now_ns());
    }
}

/// The control side of a renderer.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
unsafe fn control_side<'a>(renderer: *const AscRenderer) -> Option<&'a RendererControl> {
    if renderer.is_null() {
        return None;
    }
    // SAFETY: the handle is live; this borrows only the `control` field,
    // whose methods are atomic operations.
    Some(unsafe { &(*renderer).control })
}

/// Any thread: the linear gain (negative or non-finite counts as 0), ramped
/// to across the next callback (S64).
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_set_gain(renderer: *mut AscRenderer, gain: f32) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: the handle is live; this borrows only the `gain` field, which
    // the output thread only reads, atomically.
    unsafe { &(*renderer).gain }.set(gain);
}

/// Any thread: the gain last set.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_gain(renderer: *const AscRenderer) -> f32 {
    if renderer.is_null() {
        return 0.0;
    }
    // SAFETY: the handle is live; this borrows only the `gain` field.
    unsafe { &(*renderer).gain }.get()
}

/// Any thread: a new delay in capture-rate frames (negative counts as 0).
/// A renderer that plays fades out first and hands it to its reader after
/// the fade-out's last frame; one that is closed hands it on at its next
/// callback (S187).
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_set_delay_frames(renderer: *mut AscRenderer, frames: i64) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_side(renderer) } {
        control.set_delay_frames(frames);
    }
}

/// Any thread: the newest delay asked for, in capture-rate frames.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_delay_frames(renderer: *const AscRenderer) -> i64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::delay_frames)
}

/// Any thread: asks the renderer to fade out over F device frames from its
/// next callback and render exact zeros after them until it stops (S185).
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_request_fade_out(renderer: *mut AscRenderer) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_side(renderer) } {
        control.request_fade_out();
    }
}

/// Any thread: whether the requested fade-out's last frame has been rendered.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_fade_rendered(renderer: *const AscRenderer) -> bool {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.is_some_and(RendererControl::fade_rendered)
}

/// Any thread: the host time in nanoseconds at which the requested
/// fade-out's last frame is output, from the time its callback was given;
/// false, leaving `time_ns` alone, until a timed render has rendered it.
///
/// # Safety
///
/// `renderer` is null or a live renderer; `time_ns` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_fade_end_time_ns(
    renderer: *const AscRenderer,
    time_ns: *mut u64,
) -> bool {
    // SAFETY: by the caller's contract the handle is null or live.
    let Some(end) = unsafe { control_side(renderer) }.and_then(RendererControl::fade_end_time_ns)
    else {
        return false;
    };
    if !time_ns.is_null() {
        // SAFETY: non-null, and writable by the caller's contract.
        unsafe { time_ns.write(end) };
    }
    true
}

/// Any thread: whether the requested fade-out has played by host time
/// `now_ns`: its last frame's output time plus `latency_ns`, the device's
/// and its stream's latency, has passed (S185, the Mac).
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_faded_by_time(
    renderer: *const AscRenderer,
    latency_ns: u64,
    now_ns: u64,
) -> bool {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }
        .is_some_and(|control| control.faded_by_time(latency_ns, now_ns))
}

/// Any thread: where the fade was at the end of the latest callback, an
/// `ASC_FADE_*` code; closed before the first one.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_fade_state(renderer: *const AscRenderer) -> i32 {
    // SAFETY: by the caller's contract the handle is null or live.
    match unsafe { control_side(renderer) }.map(RendererControl::fade_state) {
        Some(FadeState::FadingIn) => FADE_FADING_IN,
        Some(FadeState::Open) => FADE_OPEN,
        Some(FadeState::FadingOut) => FADE_FADING_OUT,
        Some(FadeState::Closed) | None => FADE_CLOSED,
    }
}

/// Any thread: F, the frames of a fade at the device rate (S182); 0 for a
/// null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_fade_frames(renderer: *const AscRenderer) -> usize {
    if renderer.is_null() {
        return 0;
    }
    // SAFETY: the handle is live; `fade_frames` never changes after creation.
    unsafe { (*renderer).fade_frames }
}

/// Any thread: the reader's last published `lagFrames` (S56).
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_lag_frames(renderer: *const AscRenderer) -> i64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::lag_frames)
}

/// Any thread: the reader's counters (S158); all zero for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_counters(renderer: *const AscRenderer) -> AscReaderCounters {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or_else(AscReaderCounters::default, |control| {
        control.counters().into()
    })
}

/// Any thread: the capture frames the renderer slowed its converter by to
/// keep its reader off the newest frame (S206) or to follow the capture's
/// clock (S207), in all; 0 for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_slowed_frames(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::slowed_frames)
}

/// Any thread: the capture frames the renderer sped its converter up by to
/// follow the capture's clock (S207), in all; 0 for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_sped_frames(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::sped_frames)
}

/// Any thread: the largest lateness of a callback against the renderer's
/// earliest ones, in ns (S209); 0 without output times or for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_late_max_ns(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::late_max_ns)
}

/// Any thread: the callbacks that came more than 5 ms later
/// than the renderer's earliest ones (S209); 0 for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_late_callbacks(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::late_callbacks)
}

/// Any thread: the times a run of windows above the lateness floor lifted
/// it (S209); 0 for a null handle.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_floor_lifts(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::floor_lifts)
}

/// Any thread: the device frames the converter couldn't produce and that
/// were played as silence, in all; 0 for a null handle and, unless the read
/// sizing is wrong, always.
///
/// # Safety
///
/// `renderer` is null or a live renderer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_renderer_shortfall_frames(renderer: *const AscRenderer) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_side(renderer) }.map_or(0, RendererControl::shortfall_frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::ring::{
        AscRingWriter, asc_ring_capacity_for_rate, asc_ring_create, asc_ring_destroy,
        asc_ring_write, asc_ring_writer_create, asc_ring_writer_destroy,
    };

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

    // covers: S85, S123, S125
    #[test]
    fn non_finite_input_becomes_silence_only_when_the_rates_differ() {
        // Different rates: nothing non-finite comes out.
        let handle = asc_resampler_create(44_100, 48_000);
        let mut input = wave(441, 3);
        input[10] = f32::NAN;
        input[11] = f32::INFINITY;
        let mut output = vec![0.0f32; 2 * 600];
        // SAFETY: `handle` is live; the buffers hold the stated frames.
        unsafe {
            let count =
                asc_resampler_process(handle, input.as_ptr(), 441, output.as_mut_ptr(), 600);
            assert!(count.output_frames > 400);
            assert!(
                output[..count.output_frames * 2]
                    .iter()
                    .all(|x| x.is_finite())
            );
            asc_resampler_destroy(handle);
        }
        // Equal rates: every sample passes through bit for bit, and nothing
        // is held, so the next finite block comes out finite.
        let handle = asc_resampler_create(48_000, 48_000);
        let input = [f32::NAN, f32::INFINITY, 0.5, 0.5, f32::NEG_INFINITY, -0.25];
        let mut output = [0.0f32; 6];
        let finite = [0.25f32, -0.25];
        let mut after = [f32::NAN; 2];
        // SAFETY: `handle` is live; each buffer holds the frames passed with it.
        unsafe {
            let count = asc_resampler_process(handle, input.as_ptr(), 3, output.as_mut_ptr(), 3);
            assert_eq!((count.input_frames, count.output_frames), (3, 3));
            asc_resampler_process(handle, finite.as_ptr(), 1, after.as_mut_ptr(), 1);
            assert_eq!(asc_resampler_latency_frames(handle), 0);
            asc_resampler_destroy(handle);
        }
        let bits = |samples: &[f32]| samples.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&output), bits(&input));
        assert_eq!(after, finite);
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

    /// A ring for 48 kHz and its writer, through the C interface.
    fn c_ring() -> (*mut AscRing, *mut AscRingWriter) {
        let ring = asc_ring_create(asc_ring_capacity_for_rate(48_000));
        // SAFETY: `ring` is live.
        let writer = unsafe { asc_ring_writer_create(ring) };
        assert!(!ring.is_null() && !writer.is_null());
        (ring, writer)
    }

    // covers: S183, S187
    #[test]
    fn a_renderer_through_the_c_interface_renders_like_the_rust_one() {
        let (ring, writer) = c_ring();
        // SAFETY: `ring` is live.
        let shared = Arc::clone(unsafe { ring_of(ring) }.unwrap());
        let reader = Reader::new(shared, 48_000);
        let control = RendererControl::new(&reader);
        let gain = Arc::new(SharedGain::new(0.8));
        let mut rust = Renderer::new(reader, &control, gain, 48_000, 44_100, 2).unwrap();
        // SAFETY: `ring` is live.
        let c = unsafe { asc_renderer_create(ring, 48_000, 44_100, 2, 0.8) };
        assert!(!c.is_null());
        control.set_delay_frames(4800);
        let mut rust_out = vec![0.0f32; 512 * 2];
        let mut c_out = vec![0.0f32; 512 * 2];
        // SAFETY: every handle is live and used from this one thread; every
        // buffer holds the frames passed with it.
        unsafe {
            assert_eq!(asc_renderer_fade_frames(c), 882);
            asc_renderer_set_delay_frames(c, 4800);
            let history = wave(9600, 7);
            asc_ring_write(writer, history.as_ptr(), 9600);
            for callback in 0..40u32 {
                let block = wave(557, callback);
                asc_ring_write(writer, block.as_ptr(), 557);
                if callback == 12 {
                    // A delay change fades out and back in on the new delay.
                    control.set_delay_frames(2400);
                    asc_renderer_set_delay_frames(c, 2400);
                }
                if callback == 30 {
                    control.request_fade_out();
                    asc_renderer_request_fade_out(c);
                }
                rust.render(&mut rust_out);
                asc_renderer_render(c, c_out.as_mut_ptr(), 512);
                assert_eq!(rust_out, c_out, "callback {callback}");
            }
            assert_eq!(asc_renderer_delay_frames(c), 2400);
            assert_eq!(asc_renderer_lag_frames(c), control.lag_frames());
            assert_eq!(
                asc_renderer_counters(c),
                AscReaderCounters::from(control.counters())
            );
            assert_eq!(asc_renderer_slowed_frames(c), rust.slowed_frames());
            assert_eq!(asc_renderer_sped_frames(c), rust.sped_frames());
            assert_eq!(asc_renderer_late_max_ns(c), control.late_max_ns());
            assert_eq!(asc_renderer_late_callbacks(c), control.late_callbacks());
            assert_eq!(asc_renderer_floor_lifts(c), control.floor_lifts());
            assert_eq!(asc_renderer_shortfall_frames(c), rust.shortfall_frames());
            assert!(asc_renderer_fade_rendered(c));
            assert!(
                c_out.iter().all(|&s| s == 0.0),
                "closed after the stop's fade-out"
            );
            // Untimed renders publish no output time.
            assert!(!asc_renderer_fade_end_time_ns(c, ptr::null_mut()));
            asc_renderer_destroy(c);
            asc_ring_writer_destroy(writer);
            asc_ring_destroy(ring);
        }
    }

    // covers: S184, S185
    #[test]
    fn a_c_renderer_fades_in_publishes_its_fade_end_and_counts_as_faded_after_the_latency() {
        let (ring, writer) = c_ring();
        // SAFETY: `ring` is live.
        let renderer = unsafe { asc_renderer_create(ring, 48_000, 48_000, 2, 1.0) };
        let level = vec![0.5f32; 512 * 2];
        let mut out = vec![0.0f32; 512 * 2];
        let period = 10_666_667u64;
        let start = 7_000_000_000u64;
        let latency = 4_000_000u64;
        let mut states = Vec::new();
        // SAFETY: the handles are live and used from this one thread; the
        // buffers hold the frames passed with them.
        unsafe {
            assert_eq!(asc_renderer_fade_state(renderer), FADE_CLOSED);
            asc_renderer_set_delay_frames(renderer, 4800);
            for _ in 0..20 {
                asc_ring_write(writer, level.as_ptr(), 512);
            }
            let mut first = Vec::new();
            for callback in 0..5u64 {
                asc_ring_write(writer, level.as_ptr(), 512);
                if callback == 2 {
                    asc_renderer_request_fade_out(renderer);
                }
                asc_renderer_render_timed(
                    renderer,
                    out.as_mut_ptr(),
                    512,
                    start + callback * period,
                );
                states.push(asc_renderer_fade_state(renderer));
                if callback == 0 {
                    first = out.clone();
                }
            }
            // The first audio fades in from (almost) nothing.
            assert!(first[0] > 0.0 && first[0] < 1e-4 && first[2 * 511] < 0.5);
            assert_eq!(
                states,
                [
                    FADE_FADING_IN,
                    FADE_OPEN,
                    FADE_FADING_OUT,
                    FADE_CLOSED,
                    FADE_CLOSED
                ]
            );
            let end = start + 3 * period + 447 * 1_000_000_000 / 48_000;
            let mut published = 0u64;
            assert!(asc_renderer_fade_end_time_ns(renderer, &mut published));
            assert_eq!(published, end);
            assert!(!asc_renderer_faded_by_time(
                renderer,
                latency,
                end + latency - 1
            ));
            assert!(asc_renderer_faded_by_time(renderer, latency, end + latency));
            asc_renderer_destroy(renderer);
            asc_ring_writer_destroy(writer);
            asc_ring_destroy(ring);
        }
    }

    #[test]
    fn null_renderers_and_bad_arguments_do_nothing() {
        let (ring, writer) = c_ring();
        let mut block = [0.5f32; 8];
        let mut end = 7u64;
        // SAFETY: every call gets a null handle, a null buffer or a count no
        // buffer can hold, which the interface allows; `ring` is live.
        unsafe {
            assert!(asc_renderer_create(ptr::null_mut(), 48_000, 48_000, 2, 1.0).is_null());
            assert!(asc_renderer_create(ring, 48_000, 48_000, 0, 1.0).is_null());
            assert!(asc_renderer_create(ring, 1_000, 48_000, 2, 1.0).is_null());
            assert!(asc_renderer_create(ring, 48_000, 1_000_000, 2, 1.0).is_null());
            let null = ptr::null_mut::<AscRenderer>();
            asc_renderer_render(null, block.as_mut_ptr(), 4);
            asc_renderer_render_timed(null, block.as_mut_ptr(), 4, 1);
            asc_renderer_set_gain(null, 1.0);
            asc_renderer_set_delay_frames(null, 100);
            asc_renderer_request_fade_out(null);
            assert_eq!(asc_renderer_gain(null), 0.0);
            assert_eq!(asc_renderer_delay_frames(null), 0);
            assert!(!asc_renderer_fade_rendered(null));
            assert!(!asc_renderer_fade_end_time_ns(null, &mut end));
            assert!(!asc_renderer_faded_by_time(null, 0, u64::MAX));
            assert_eq!(asc_renderer_fade_state(null), FADE_CLOSED);
            assert_eq!(asc_renderer_fade_frames(null), 0);
            assert_eq!(asc_renderer_lag_frames(null), 0);
            assert_eq!(asc_renderer_counters(null), AscReaderCounters::default());
            assert_eq!(asc_renderer_slowed_frames(null), 0);
            assert_eq!(asc_renderer_sped_frames(null), 0);
            assert_eq!(asc_renderer_late_max_ns(null), 0);
            assert_eq!(asc_renderer_late_callbacks(null), 0);
            assert_eq!(asc_renderer_floor_lifts(null), 0);
            assert_eq!(asc_renderer_shortfall_frames(null), 0);
            asc_renderer_destroy(null);
            let renderer = asc_renderer_create(ring, 48_000, 48_000, 2, 1.0);
            asc_renderer_render(renderer, ptr::null_mut(), 4);
            asc_renderer_render(renderer, block.as_mut_ptr(), usize::MAX);
            asc_renderer_render_timed(renderer, ptr::null_mut(), 4, 1);
            asc_renderer_render(renderer, ptr::null_mut(), 0);
            asc_renderer_set_gain(renderer, f32::NAN);
            assert_eq!(asc_renderer_gain(renderer), 0.0);
            asc_renderer_destroy(renderer);
            asc_ring_writer_destroy(writer);
            asc_ring_destroy(ring);
        }
        assert_eq!(block, [0.5; 8]);
        assert_eq!(end, 7);
        assert_eq!(STOP_FADE_CAP_MS, 500);
    }
}
