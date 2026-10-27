//! Rendering to one endpoint: a shared-mode, event-driven stream in the
//! endpoint's own float format, fed by a [`RenderPipeline`] on a thread of
//! its own, in the multithreaded apartment and registered with MMCSS.
//!
//! A stop fades the output out over 20 ms (S185) and keeps the stream
//! running, rendering zeros, until the endpoint has played the fade: its
//! clock has passed the fade's last frame (or, without a clock, it holds
//! nothing but zeros and its stream latency has passed since; Windows often
//! reports that latency as 0). Only then does the thread stop the stream and
//! mark the output faded, so the end of the fade is never cut; it stops
//! anyway after [`STOP_FADE_CAP`] of audio, or at once when the output is
//! dropped (S186). The drop waits for the thread as a [`Worker`] does:
//! stopping the stream can hang while Windows audio doesn't answer.

use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use audiosync_core::reader::Reader;
use audiosync_core::render::{RendererControl, STOP_FADE_CAP};
use audiosync_core::volume::SharedGain;
use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_NOPERSIST,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioClock, IAudioRenderClient,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};
use windows::core::HSTRING;

use super::com::{
    Apartment, ComMemory, Event, ProAudioTask, Ready, Worker, device_error, float_format,
    read_mix_format, spawn_worker,
};
use super::devices::enumerator;
use crate::audio::backend::BackendError;
use crate::audio::clock_probe::{ClockProbe, ClockQueue, ProbeSampler};
use crate::audio::controller::RendererHandle;
use crate::audio::outputs::{
    RenderFormat, StreamSetup, period_frames, render_buffer_hns, render_format,
};
use crate::audio::render::{EndpointQueue, RenderPipeline, WakeCounters, WakeCounts};

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
const FADE_WAIT_MS: u32 = 100;

/// A running output.
pub struct EndpointRenderer {
    stop: Arc<Event>,
    /// Set when the output is dropped: the render thread stops at once,
    /// faded or not.
    abort: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    control: RendererControl,
    thread: Worker,
    /// How the render thread set up its stream.
    setup: StreamSetup,
    /// The clock-queue probe, only with the debug log on.
    probe: Option<Arc<ClockProbe>>,
    /// The render thread's late and empty wakes.
    wakes: Arc<WakeCounters>,
}

impl RendererHandle for EndpointRenderer {
    fn request_stop(&mut self) {
        self.control.request_fade_out();
        self.stop.set();
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    fn is_faded(&self) -> bool {
        self.control.is_faded() || self.thread.is_finished()
    }

    fn setup(&self) -> Option<StreamSetup> {
        Some(self.setup)
    }

    fn clock_queue(&self) -> Option<ClockQueue> {
        self.probe.as_ref()?.take()
    }

    fn wakes(&self) -> WakeCounts {
        self.wakes.counts()
    }
}

impl Drop for EndpointRenderer {
    fn drop(&mut self) {
        // The thread stops its stream at once and ends.
        self.abort.store(true, Ordering::Release);
        self.stop.set();
        self.thread.finish();
    }
}

/// Starts rendering to the endpoint `endpoint_id` through `reader`,
/// controlled through `control`, at the gain in `gain`. Returns once it
/// plays.
pub(super) fn start(
    endpoint_id: &str,
    reader: Reader,
    control: RendererControl,
    gain: Arc<SharedGain>,
) -> Result<EndpointRenderer, BackendError> {
    let id = endpoint_id.to_owned();
    let stop = Arc::new(Event::manual_reset()?);
    let abort = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let probe = ClockProbe::from_env();
    let wakes = Arc::new(WakeCounters::default());
    let (thread, setup) = {
        let stop = Arc::clone(&stop);
        let abort = Arc::clone(&abort);
        let ended = Arc::clone(&ended);
        let control = control.clone();
        let probe = probe.clone();
        let wakes = Arc::clone(&wakes);
        spawn_worker("audiosync-output", move |ready| {
            let signals = Signals {
                stop: &stop,
                abort: &abort,
                ended: &ended,
                wakes: &wakes,
            };
            run(&id, reader, &control, gain, probe, &signals, ready);
        })?
    };
    Ok(EndpointRenderer {
        stop,
        abort,
        ended,
        control,
        thread,
        setup,
        probe,
        wakes,
    })
}

/// What the render thread shares with its [`EndpointRenderer`].
struct Signals<'a> {
    /// Set by a stop request, and by the drop.
    stop: &'a Event,
    /// Set by the drop.
    abort: &'a AtomicBool,
    /// Set by the thread when the stream failed.
    ended: &'a AtomicBool,
    /// Counted by the thread at each wake that isn't part of a stop.
    wakes: &'a WakeCounters,
}

/// The render thread.
fn run(
    id: &str,
    reader: Reader,
    control: &RendererControl,
    gain: Arc<SharedGain>,
    probe: Option<Arc<ClockProbe>>,
    signals: &Signals,
    ready: Ready<StreamSetup>,
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
    let mut pipeline =
        match RenderPipeline::new(reader, control, gain, stream.format.rate, channels) {
            Ok(pipeline) => pipeline,
            Err(error) => return ready.fail(error),
        };
    pipeline.set_period_frames(stream.period_frames);
    let mut sampler = probe.map(|probe| ProbeSampler::new(probe, stream.format.rate));
    if let Err(error) = prefill_silence(&stream) {
        return ready.fail(error);
    }
    let _task = ProAudioTask::join();
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        return ready.fail(device_error("IAudioClient::Start", &error));
    }
    let waited = ready.ok(StreamSetup {
        format: stream.format,
        buffer_frames: stream.buffer_frames,
    });
    if !waited {
        // Nobody waits for this stream anymore, and nothing would stop it:
        // it stops at once rather than play unseen.
        // SAFETY: stops the stream started above.
        let _ = unsafe { stream.client.Stop() };
        return;
    }
    let lost = render_loop(&stream, &mut pipeline, sampler.as_mut(), signals);
    // SAFETY: stops the stream started above.
    let _ = unsafe { stream.client.Stop() };
    if lost {
        signals.ended.store(true, Ordering::Release);
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
    /// The stream latency Windows reports, in device frames.
    latency_frames: u32,
    /// The stream's clock and its frequency, unless Windows gave none.
    clock: Option<(IAudioClock, u64)>,
    /// The device's default period, in device frames: what each wake
    /// refills when it is on time.
    period_frames: usize,
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
        let clock = client.GetService::<IAudioClock>().ok().and_then(|clock| {
            let frequency = clock
                .GetFrequency()
                .ok()
                .filter(|&frequency| frequency > 0)?;
            Some((clock, frequency))
        });
        // In 100 ns units; a stream that can't tell counts as having none.
        let latency = client.GetStreamLatency().unwrap_or(0).max(0) as u64;
        let latency_frames =
            (latency * u64::from(format.rate) / 10_000_000).min(u64::from(u32::MAX)) as u32;
        Ok(RenderStream {
            client,
            render,
            event,
            format,
            buffer_frames,
            latency_frames,
            clock,
            period_frames: period_frames(default_period, format.rate),
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
/// After a stop request the pipeline fades out from its next callback and
/// renders zeros after it; the loop goes on until the endpoint has played
/// the fade (S185), marks the output faded and stops. It stops anyway once
/// [`STOP_FADE_CAP`] of audio was written since the request, when the device
/// stops asking for audio, or at once when the output is dropped (S186). A
/// stream that no longer works has nothing to fade out, so it ends at once.
///
/// Real-time: waits for the device's event, then fills the free part of the
/// buffer. When the device hasn't asked for [`WAIT_MS`], it does the same:
/// the padding call fails once Windows invalidated the stream, which ends the
/// loop as lost. Never allocates, locks or logs. With a `sampler`, it
/// samples the clock queue at the probe times, counted in frames written
/// past the silent prefill.
fn render_loop(
    stream: &RenderStream,
    pipeline: &mut RenderPipeline,
    mut sampler: Option<&mut ProbeSampler>,
    signals: &Signals,
) -> bool {
    let both = [signals.stop.raw(), stream.event.raw()];
    let device_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let channels = pipeline.channels();
    let cap_frames = (u128::from(stream.format.rate) * STOP_FADE_CAP.as_millis() / 1000) as u32;
    // Frames written since the stop request.
    let mut stopping: Option<u32> = None;
    // Frames written since the start, the silent prefill included.
    let mut written_total = u64::from(stream.buffer_frames);
    loop {
        let wait = match stopping {
            // SAFETY: both handles live as long as this loop.
            None => unsafe { WaitForMultipleObjects(&both, false, WAIT_MS) },
            // SAFETY: as above.
            Some(_) => unsafe { WaitForSingleObject(stream.event.raw(), FADE_WAIT_MS) },
        };
        if signals.abort.load(Ordering::Acquire) {
            return false;
        }
        match stopping {
            None if wait == WAIT_OBJECT_0 => {
                // A stream Windows invalidated may never ask for the
                // fade-out, so it ends now rather than after FADE_WAIT_MS.
                // SAFETY: a call on the stream this thread owns.
                if unsafe { stream.client.GetCurrentPadding() }.is_err() {
                    return true;
                }
                // The pipeline takes the request at its next callback.
                stopping = Some(0);
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
        if stopping.is_none() {
            signals
                .wakes
                .count_wake(padding, stream.buffer_frames, stream.period_frames);
        }
        if stopping.is_some()
            && pipeline.has_played_its_fade(EndpointQueue {
                padding,
                clock_queued: clock_queued(stream, written_total),
                latency_frames: stream.latency_frames,
                rate: stream.format.rate,
            })
        {
            pipeline.mark_faded();
            return false;
        }
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
                pipeline.render(out);
                0
            };
            // SAFETY: releases the frames GetBuffer handed out.
            if unsafe { stream.render.ReleaseBuffer(available, flags) }.is_err() {
                return true;
            }
            written_total += u64::from(available);
            if let Some(sampler) = sampler.as_deref_mut() {
                let elapsed = ProbeSampler::elapsed(written_total, stream.buffer_frames);
                if sampler.is_due(elapsed) {
                    sampler.record(elapsed, clock_queued(stream, written_total));
                }
            }
        }
        if let Some(written) = stopping.as_mut() {
            *written = written.saturating_add(available);
            if *written >= cap_frames {
                return false;
            }
        }
    }
}

/// The frames of `written` the stream's clock says the endpoint hasn't
/// played yet, or `None` without a clock reading. Real-time: no allocation.
fn clock_queued(stream: &RenderStream, written: u64) -> Option<u64> {
    let (clock, frequency) = stream.clock.as_ref()?;
    let mut position = 0u64;
    // SAFETY: a call on the clock of the stream this thread owns.
    unsafe { clock.GetPosition(&raw mut position, None) }.ok()?;
    let played = u128::from(position) * u128::from(stream.format.rate) / u128::from(*frequency);
    Some(written.saturating_sub(u64::try_from(played).unwrap_or(u64::MAX)))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use audiosync_core::ring::Ring;
    use windows::Win32::System::Threading::INFINITE;

    use super::super::com::{StuckThreads, spawn_worker_within};
    use super::*;
    use crate::audio::SESSION_RATE;

    /// How long the fake Windows audio hangs before it answers again.
    const HANG: Duration = Duration::from_secs(3);

    /// How long a test waits for an answer.
    const TIMEOUT: Duration = Duration::from_millis(200);

    #[test]
    fn a_render_thread_that_hangs_stopping_its_stream_doesnt_hold_quitting() {
        static STUCK: StuckThreads = StuckThreads::new();
        let ring = Ring::for_rate(SESSION_RATE).expect("a ring");
        let reader = Reader::new(ring, SESSION_RATE);
        let control = RendererControl::new(&reader);
        let stop = Arc::new(Event::manual_reset().expect("an event"));
        let abort = Arc::new(AtomicBool::new(false));
        let (answer, hung) = mpsc::channel::<()>();
        let (stopped, done) = mpsc::channel();
        let (thread, ()) = {
            let stop = Arc::clone(&stop);
            let abort = Arc::clone(&abort);
            spawn_worker_within("audiosync-test", TIMEOUT, &STUCK, move |ready| {
                // The thread owns the reader, as the real one does.
                let _reader = reader;
                if ready.ok(()) {
                    // SAFETY: waits on an event that outlives this thread's
                    // use of it.
                    unsafe { WaitForSingleObject(stop.raw(), INFINITE) };
                    // Stopping the stream hangs until Windows audio answers.
                    let _ = hung.recv();
                    let _ = stopped.send(abort.load(Ordering::Acquire));
                }
            })
            .expect("it starts")
        };
        let renderer = EndpointRenderer {
            stop,
            abort,
            ended: Arc::new(AtomicBool::new(false)),
            control,
            thread,
            setup: StreamSetup {
                format: RenderFormat {
                    rate: 48_000,
                    channels: 2,
                    channel_mask: 0x3,
                    convert: false,
                },
                buffer_frames: 480,
            },
            probe: None,
            wakes: Arc::default(),
        };
        let answers = thread::spawn(move || {
            thread::sleep(HANG);
            drop(answer);
        });
        let started = Instant::now();
        drop(renderer);
        let waited = started.elapsed();
        assert!(waited < HANG / 2, "the drop waited {waited:?}");
        assert!(STUCK.any(), "the thread counts as stuck");
        // Once Windows audio answers, the thread still stops its stream,
        // having been told to stop at once, and ends.
        assert_eq!(done.recv_timeout(HANG * 3), Ok(true));
        assert!(STUCK.none_within(HANG), "the thread ends");
        answers.join().expect("the answer comes");
    }
}
