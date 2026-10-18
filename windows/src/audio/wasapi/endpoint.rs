//! Rendering to one endpoint: a shared-mode, event-driven stream in the
//! endpoint's own float format, fed by a [`RenderPipeline`] on a thread of
//! its own, in the multithreaded apartment and registered with MMCSS.

use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use audiosync_core::reader::Reader;
use audiosync_core::volume::SharedGain;
use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_NOPERSIST,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioRenderClient, WAVEFORMATEX,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};
use windows::core::HSTRING;

use super::com::{
    Apartment, ComMemory, Event, ProAudioTask, Ready, device_error, float_format, read_mix_format,
    spawn_ready,
};
use super::devices::enumerator;
use crate::audio::backend::BackendError;
use crate::audio::controller::RendererHandle;
use crate::audio::outputs::{RenderFormat, render_buffer_hns, render_format};
use crate::audio::render::RenderPipeline;

/// The flags of every render stream: event-driven, and without volume
/// settings that Windows would remember for AudioSync's session.
const RENDER_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST;

/// How long the render loop waits for the device before it checks that the
/// stream still works, in ms. A working stream asks for audio every device
/// period (about 10 ms). One that Windows invalidated (its device was
/// unplugged or changed its format, or Windows audio restarted) may stop
/// asking without a word; only a call on the stream then tells.
const WAIT_MS: u32 = 500;

/// How long the render loop waits for the device while fading out, in ms; a
/// device that stops asking for audio then just stops.
const FADE_WAIT_MS: u32 = 200;

/// The most device periods a fade-out may take before the stream stops
/// anyway.
const FADE_MAX_PERIODS: u32 = 50;

/// A running output.
pub struct EndpointRenderer {
    stop: Arc<Event>,
    ended: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RendererHandle for EndpointRenderer {
    fn request_stop(&mut self) {
        self.stop.set();
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

impl Drop for EndpointRenderer {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts rendering to the endpoint `endpoint_id` through `reader`, at the
/// gain in `gain`. Returns once it plays.
pub(super) fn start(
    endpoint_id: &str,
    reader: Reader,
    gain: Arc<SharedGain>,
) -> Result<EndpointRenderer, BackendError> {
    let id = endpoint_id.to_owned();
    let stop = Arc::new(Event::manual_reset()?);
    let ended = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        let ended = Arc::clone(&ended);
        spawn_ready("audiosync-output", move |ready| {
            run(&id, reader, gain, &stop, &ended, ready);
        })?
    };
    Ok(EndpointRenderer {
        stop,
        ended,
        thread: Some(thread),
    })
}

/// The render thread.
fn run(
    id: &str,
    reader: Reader,
    gain: Arc<SharedGain>,
    stop: &Event,
    ended: &AtomicBool,
    ready: Ready,
) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let stream = match open_stream(id) {
        Ok(stream) => stream,
        Err(error) => return ready.fail(error),
    };
    let channels = usize::from(stream.format.channels);
    // Every buffer the loop uses is allocated here, before it starts.
    let mut pipeline = match RenderPipeline::new(reader, gain, stream.format.rate, channels) {
        Ok(pipeline) => pipeline,
        Err(error) => return ready.fail(error),
    };
    if let Err(error) = prefill_silence(&stream) {
        return ready.fail(error);
    }
    let _task = ProAudioTask::join();
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        return ready.fail(device_error("IAudioClient::Start", &error));
    }
    ready.ok();
    let lost = render_loop(&stream, &mut pipeline, stop);
    // SAFETY: stops the stream started above.
    let _ = unsafe { stream.client.Stop() };
    if lost {
        ended.store(true, Ordering::Release);
    }
}

/// An initialized render stream.
struct RenderStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Set by Windows when the device wants audio. Declared after the
    /// clients, so it is closed after them.
    event: Event,
    format: RenderFormat,
    /// The stream buffer, in device frames.
    buffer_frames: u32,
}

/// Opens a shared-mode, event-driven stream on the endpoint `id`, in its
/// mix format when that is float, or in float at the mix format's rate and
/// channels, converted by Windows, otherwise.
fn open_stream(id: &str) -> Result<RenderStream, BackendError> {
    let enumerator = enumerator()?;
    // SAFETY: calls on COM objects this thread owns. The mix format is freed
    // by ComMemory after the last use of its pointer, and the format passed
    // to Initialize lives through the call, which copies it.
    unsafe {
        let device = enumerator
            .GetDevice(&HSTRING::from(id))
            .map_err(|error| device_error("IMMDeviceEnumerator::GetDevice", &error))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| device_error("IMMDevice::Activate", &error))?;
        let mix = ComMemory::new(
            client
                .GetMixFormat()
                .map_err(|error| device_error("IAudioClient::GetMixFormat", &error))?,
        );
        let format = render_format(read_mix_format(mix.as_ptr()))?;
        let mut default_period = 0i64;
        client
            .GetDevicePeriod(Some(&raw mut default_period), None)
            .map_err(|error| device_error("IAudioClient::GetDevicePeriod", &error))?;
        let own = float_format(format);
        let (wave, flags) = if format.convert {
            (
                (&raw const own).cast::<WAVEFORMATEX>(),
                RENDER_FLAGS
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            )
        } else {
            (mix.as_ptr().cast_const(), RENDER_FLAGS)
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                render_buffer_hns(default_period),
                0,
                wave,
                None,
            )
            .map_err(|error| device_error("IAudioClient::Initialize", &error))?;
        let event = Event::auto_reset()?;
        client
            .SetEventHandle(event.raw())
            .map_err(|error| device_error("IAudioClient::SetEventHandle", &error))?;
        let render = client
            .GetService::<IAudioRenderClient>()
            .map_err(|error| device_error("IAudioClient::GetService", &error))?;
        let buffer_frames = client
            .GetBufferSize()
            .map_err(|error| device_error("IAudioClient::GetBufferSize", &error))?;
        Ok(RenderStream {
            client,
            render,
            event,
            format,
            buffer_frames,
        })
    }
}

/// Fills the whole stream buffer with silence before the start, so the
/// device has a buffer to play while the first period is rendered.
fn prefill_silence(stream: &RenderStream) -> Result<(), BackendError> {
    // SAFETY: calls on the stream this thread owns, before it starts: the
    // whole buffer is free, and it is released as silent without writing.
    unsafe {
        stream
            .render
            .GetBuffer(stream.buffer_frames)
            .map_err(|error| device_error("IAudioRenderClient::GetBuffer", &error))?;
        stream
            .render
            .ReleaseBuffer(stream.buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(|error| device_error("IAudioRenderClient::ReleaseBuffer", &error))
    }
}

/// Renders until stopped (`false`) or until the stream fails (`true`: the
/// output was lost, usually because its device went away).
///
/// After a stop request it renders a fade-out, then silence, until two whole
/// buffers were written: everything still queued in the device is then
/// silent, and the stream stops without a click. A stream that no longer
/// works has nothing to fade out, so it ends at once.
///
/// Real-time: waits for the device's event, then fills the free part of the
/// buffer. When the device hasn't asked for [`WAIT_MS`], it does the same:
/// the padding call fails once Windows invalidated the stream, which ends the
/// loop as lost. Never allocates, locks or logs.
fn render_loop(stream: &RenderStream, pipeline: &mut RenderPipeline, stop: &Event) -> bool {
    let both = [stop.raw(), stream.event.raw()];
    let device_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let channels = pipeline.channels();
    // Frames written and periods waited since the stop request.
    let mut fading: Option<(u32, u32)> = None;
    loop {
        let wait = match fading {
            // SAFETY: both handles live as long as this loop.
            None => unsafe { WaitForMultipleObjects(&both, false, WAIT_MS) },
            // SAFETY: as above.
            Some(_) => unsafe { WaitForSingleObject(stream.event.raw(), FADE_WAIT_MS) },
        };
        match fading {
            None if wait == WAIT_OBJECT_0 => {
                // A stream Windows invalidated may never ask for the
                // fade-out, so it ends now rather than after FADE_WAIT_MS.
                // SAFETY: a call on the stream this thread owns.
                if unsafe { stream.client.GetCurrentPadding() }.is_err() {
                    return true;
                }
                fading = Some((0, 0));
                continue;
            }
            None if wait == device_ready || wait == WAIT_TIMEOUT => {}
            Some(_) if wait == WAIT_OBJECT_0 => {}
            Some(_) if wait == WAIT_TIMEOUT => return false,
            _ => return true,
        }
        // SAFETY: calls on the stream this thread owns.
        let Ok(padding) = (unsafe { stream.client.GetCurrentPadding() }) else {
            return true;
        };
        let available = stream.buffer_frames.saturating_sub(padding);
        if available > 0 {
            // SAFETY: as above; `available` frames are free.
            let Ok(data) = (unsafe { stream.render.GetBuffer(available) }) else {
                return true;
            };
            let flags = if data.is_null() || !data.cast::<f32>().is_aligned() {
                AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
            } else {
                // SAFETY: GetBuffer handed out `available` frames of
                // `channels` float samples (the stream's format), writable
                // until ReleaseBuffer; the pointer is aligned for f32.
                let out = unsafe {
                    slice::from_raw_parts_mut(data.cast::<f32>(), available as usize * channels)
                };
                if fading.is_some() {
                    pipeline.render_fade_out(out);
                } else {
                    pipeline.render(out);
                }
                0
            };
            // SAFETY: releases the frames GetBuffer handed out.
            if unsafe { stream.render.ReleaseBuffer(available, flags) }.is_err() {
                return true;
            }
        }
        if let Some((written, periods)) = fading.as_mut() {
            *written = written.saturating_add(available);
            *periods += 1;
            if *written >= stream.buffer_frames.saturating_mul(2) || *periods >= FADE_MAX_PERIODS {
                return false;
            }
        }
    }
}
