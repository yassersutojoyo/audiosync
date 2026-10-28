//! Process loopback capture, through the Application Loopback API, and
//! input device capture.
//!
//! `ActivateAudioInterfaceAsync` on the virtual process loopback device gives
//! an `IAudioClient` that captures one process tree, or everything except it.
//! The activation completes on a Windows thread, which calls our completion
//! handler; the capture thread waits for it. Process loopback has no mix
//! format, so `Initialize` names the format explicitly, trying
//! [`CAPTURE_ATTEMPTS`] in order. The capture thread is event-driven, in the
//! multithreaded apartment and registered with MMCSS; its loop passes each
//! packet to a [`CaptureConverter`], which equalizes it and writes the ring
//! and the AirPlay sender's feed. Dropping the capture waits for the thread
//! as a [`Worker`] does: stopping the stream can hang while Windows audio
//! doesn't answer, and a thread let go keeps the feed until it ends.
//!
//! An input device is opened from its own capture endpoint
//! (`IMMDevice::Activate`) and initialized in shared mode, without the
//! loopback flag, through the same formats; the rest is the same.

use std::mem::ManuallyDrop;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use std::thread;

use audiosync_core::channels::InputPair;
use audiosync_core::eq::EqProcessor;
use audiosync_core::ring::RingWriter;

use crate::audio::capture::Feeds;
use windows::Win32::Foundation::{E_FAIL, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, DEVICE_STATE_ACTIVE, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, HSTRING, IUnknown, Interface, Ref};
use windows_core::implement;

use super::com::{
    Apartment, ComMemory, Event, NOT_FOUND, ProAudioTask, Ready, Worker, device_code_error,
    device_error, float_format, input_code_error, read_mix_format, spawn_worker, system_error,
    wave_format,
};
use super::devices::enumerator;
use super::processes::{ProcessWatch, Watch};
use crate::audio::SESSION_RATE;
use crate::audio::backend::{BackendError, TIMED_OUT};
use crate::audio::capture::{
    Activation, CAPTURE_ATTEMPTS, CaptureAttempt, CaptureConverter, CaptureCounters,
    CaptureGlitches, CaptureTarget, GapFill, LoopbackMode, MAX_MIX_APPS, SampleType, StreamFormat,
    pair_attempts,
};
use crate::audio::controller::{CaptureHandle, CaptureState};
use crate::audio::mix::{LaneInput, LaneStream, MixLanes, MixPlan, Opened, as_bytes};
use crate::audio::outputs::HNS_PER_MS;
use crate::audio::outputs::RenderFormat;

/// The buffer a capture stream asks for, 100 ms: room for the capture thread
/// to be late without losing packets.
const CAPTURE_BUFFER_HNS: i64 = 100 * HNS_PER_MS;

/// How long a capture waits for its activation to complete, in ms.
const ACTIVATION_TIMEOUT_MS: u32 = 5_000;

/// How long the capture loop waits for a packet before it checks that the
/// stream still works and keeps the ring in real time, in ms. A process tree
/// that plays nothing delivers nothing, so the loop writes silence for the
/// time that passed ([`GapFill`]), often enough that no reader runs out of
/// its 50 ms dead band (S50); and a stream that Windows invalidated (when
/// Windows audio restarts, say) may stop signalling without a word; only a
/// call on the stream then tells.
const WAIT_MS: u32 = 10;

/// A running process loopback or input device capture.
pub struct LoopbackCapture {
    stop: Arc<Event>,
    /// The HRESULT that ended the capture, once `ended` is set.
    failure: Arc<AtomicI32>,
    ended: Arc<AtomicBool>,
    /// The frames Windows delivered, the discontinuities, silent packets,
    /// gap fill and zero holes.
    counters: Arc<CaptureCounters>,
    thread: Worker,
    /// The captured apps, to notice when they quit; empty when one can't be
    /// watched or the capture isn't of apps.
    sources: Vec<ProcessWatch>,
    /// The error the single app captured refused a watch with (S327).
    watch_refused: Option<i32>,
    /// What a mix shares with its threads, to follow its apps while it
    /// runs.
    mix: Option<Arc<MixShare>>,
    /// The apps a mix should capture.
    mix_pids: Vec<u32>,
    /// How Windows accepted to deliver the capture.
    setup: CaptureAttempt,
    /// What the log calls the capture when it fails.
    call: &'static str,
    /// Whether it captures an input device, whose failures
    /// [`input_code_error`] reads.
    input: bool,
}

impl CaptureHandle for LoopbackCapture {
    fn state(&self) -> CaptureState {
        // An app that quit comes first: its stream may fail because of it.
        if !self.sources.is_empty() && self.sources.iter().all(ProcessWatch::has_exited) {
            return CaptureState::SourceQuit;
        }
        if self.ended.load(Ordering::Acquire) {
            let code = HRESULT(self.failure.load(Ordering::Relaxed));
            let error = if self.input {
                input_code_error(self.call, code)
            } else {
                device_code_error(self.call, code)
            };
            return CaptureState::Failed(error);
        }
        CaptureState::Running
    }

    fn setup(&self) -> Option<CaptureAttempt> {
        Some(self.setup)
    }

    fn retarget(&mut self, target: &CaptureTarget) -> bool {
        let (Some(mix), CaptureTarget::Mix(activations)) = (&self.mix, target) else {
            return false;
        };
        self.mix_pids = pids_of(activations);
        mix.plan.set(&self.mix_pids);
        mix.wake.set();
        self.sources = watch_all(activations);
        true
    }

    fn holds_all(&self) -> bool {
        self.mix
            .as_ref()
            .is_none_or(|mix| mix.holds_all(&self.mix_pids))
    }

    fn glitches(&self) -> CaptureGlitches {
        self.counters.glitches()
    }

    fn watch_refused(&self) -> Option<i32> {
        self.watch_refused
    }
}

#[cfg(test)]
impl LoopbackCapture {
    fn frames_captured(&self) -> u64 {
        self.counters.frames_captured()
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        // The thread stops its stream and ends.
        self.stop.set();
        self.thread.finish();
    }
}

/// Starts capturing `target`, writing through `equalizer` and `writer`, and
/// to `feeds`.
pub(super) fn start(
    target: CaptureTarget,
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
) -> Result<LoopbackCapture, BackendError> {
    let input = matches!(target, CaptureTarget::Input { .. });
    let mut watch_refused = None;
    let (sources, call) = match &target {
        CaptureTarget::Loopback(activation) => (
            match activation.mode {
                LoopbackMode::IncludeTree => match ProcessWatch::open(activation.target_pid)? {
                    Watch::Watching(watch) => vec![watch],
                    Watch::Refused(code) => {
                        watch_refused = Some(code.0);
                        Vec::new()
                    }
                },
                LoopbackMode::ExcludeTree => Vec::new(),
            },
            "process loopback capture",
        ),
        CaptureTarget::Mix(activations) => (watch_all(activations), "process loopback capture"),
        CaptureTarget::Input { .. } => (Vec::new(), "input device capture"),
    };
    let mix_pids = match &target {
        CaptureTarget::Mix(activations) => pids_of(activations),
        _ => Vec::new(),
    };
    let mix = match target {
        CaptureTarget::Mix(_) => {
            let share = MixShare::new()?;
            share.plan.set(&mix_pids);
            Some(Arc::new(share))
        }
        _ => None,
    };
    let stop = Arc::new(Event::manual_reset()?);
    let failure = Arc::new(AtomicI32::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(CaptureCounters::default());
    let (thread, setup) = {
        let stop = Arc::clone(&stop);
        let failure = Arc::clone(&failure);
        let ended = Arc::clone(&ended);
        let counters = Arc::clone(&counters);
        let mix = mix.clone();
        spawn_worker("audiosync-capture", move |ready| {
            run(
                &target,
                mix.as_ref(),
                writer,
                equalizer,
                feeds,
                &stop,
                &failure,
                &ended,
                &counters,
                ready,
            );
        })?
    };
    Ok(LoopbackCapture {
        stop,
        failure,
        ended,
        counters,
        thread,
        sources,
        watch_refused,
        mix,
        mix_pids,
        setup,
        call,
        input,
    })
}

/// Watches for the apps of a mix: all of them, or none when one of them
/// can't be watched (its exit would go unnoticed, so "all quit" can't be
/// told). An app that already quit is left out of the watch.
fn pids_of(activations: &[Activation]) -> Vec<u32> {
    activations
        .iter()
        .map(|activation| activation.target_pid)
        .collect()
}

fn watch_all(activations: &[Activation]) -> Vec<ProcessWatch> {
    let mut watches = Vec::with_capacity(activations.len());
    for activation in activations {
        match ProcessWatch::open(activation.target_pid) {
            Ok(Watch::Watching(watch)) => watches.push(watch),
            Err(_) => {}
            Ok(Watch::Refused(_)) => return Vec::new(),
        }
    }
    watches
}

/// The capture thread. The AirPlay feed goes back to its slot when the
/// converter is dropped, as the thread ends.
#[expect(
    clippy::too_many_arguments,
    reason = "everything the thread owns or shares, passed once at its start"
)]
fn run(
    target: &CaptureTarget,
    mix: Option<&Arc<MixShare>>,
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
    stop: &Event,
    failure: &AtomicI32,
    ended: &AtomicBool,
    counters: &Arc<CaptureCounters>,
    ready: Ready<CaptureAttempt>,
) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    if let (CaptureTarget::Mix(activations), Some(share)) = (target, mix) {
        return run_mix(
            activations,
            share,
            writer,
            equalizer,
            feeds,
            stop,
            failure,
            ended,
            counters,
            ready,
        );
    }
    let stream = match open_stream(target) {
        Ok(stream) => stream,
        Err(error) => return ready.fail(error),
    };
    let mut converter = match CaptureConverter::new(stream.format, writer, equalizer, feeds) {
        Ok(converter) => converter
            .reading(stream.pair)
            .counting(Arc::clone(counters)),
        Err(error) => return ready.fail(error),
    };
    let _task = ProAudioTask::join();
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        return ready.fail(capture_error(target, "IAudioClient::Start", error.code()));
    }
    let waited = ready.ok(CaptureAttempt {
        format: stream.format,
        autoconvert: stream.autoconvert,
        pair: stream.pair,
    });
    if !waited {
        // Nobody waits for this capture anymore, and nothing would stop it:
        // it stops at once rather than capture unseen.
        // SAFETY: stops the stream started above.
        let _ = unsafe { stream.client.Stop() };
        return;
    }
    let result = capture_loop(&stream, &mut converter, stop, counters);
    // SAFETY: stops the stream started above.
    let _ = unsafe { stream.client.Stop() };
    if let Err(code) = result {
        failure.store(code.0, Ordering::Relaxed);
        ended.store(true, Ordering::Release);
    }
}

/// An initialized capture stream.
struct CaptureStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Set by Windows when a packet is ready. Declared after the clients, so
    /// it is closed after them.
    event: Event,
    format: StreamFormat,
    /// Whether Windows converts to `format`.
    autoconvert: bool,
    /// The channels that play as L and R.
    pair: InputPair,
}

/// Activates and initializes a capture, trying each format of
/// [`CAPTURE_ATTEMPTS`] until one is accepted. An input device playing a
/// pair other than 1-2 first tries its own channel count
/// ([`pair_attempts`]); when Windows takes none of those, it plays 1-2.
fn open_stream(target: &CaptureTarget) -> Result<CaptureStream, BackendError> {
    let mut first_error = None;
    let (mut attempts, channel_mask) = match target {
        CaptureTarget::Input { endpoint_id, pair } if *pair != 0 => {
            input_pair_attempts(endpoint_id, *pair)?
        }
        _ => (Vec::new(), 0),
    };
    attempts.extend(CAPTURE_ATTEMPTS);
    for attempt in attempts {
        let (client, mut flags) = match target {
            CaptureTarget::Loopback(activation) => (
                activate(*activation)?,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            ),
            // `run_mix` opens each of a mix's loopbacks on its own.
            CaptureTarget::Mix(_) => return Err(BackendError::NotAnApp),
            CaptureTarget::Input { endpoint_id, .. } => (
                activate_input(endpoint_id)?,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            ),
        };
        let plain = wave_format(attempt.format);
        // Windows wants an extensible format, with its speaker mask, past two channels.
        let extensible = float_format(RenderFormat {
            rate: attempt.format.rate,
            channels: attempt.format.channels,
            channel_mask,
            convert: attempt.autoconvert,
        });
        let format: *const WAVEFORMATEX = if attempt.format.channels > 2 {
            (&raw const extensible).cast()
        } else {
            &raw const plain
        };
        if attempt.autoconvert {
            flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        }
        // SAFETY: `format` points to `plain` or `extensible`, which live
        // through the call, which copies it.
        let initialized = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                CAPTURE_BUFFER_HNS,
                0,
                format,
                None,
            )
        };
        match initialized {
            Ok(()) => return finish_stream(client, attempt),
            Err(error) => match capture_error(target, "IAudioClient::Initialize", error.code()) {
                // No other format gets past the privacy settings, or a
                // device another app holds (S333).
                error @ (BackendError::MicrophoneBlocked | BackendError::DeviceBusy) => {
                    return Err(error);
                }
                error => {
                    first_error.get_or_insert(error);
                }
            },
        }
    }
    Err(first_error.unwrap_or(BackendError::UnsupportedFormat))
}

/// How a capture reads a failed `IAudioClient` call on its stream: as a
/// failed call on its device, as an output's are (S333), and an input's
/// `E_ACCESSDENIED` as the privacy settings' block (S141).
fn capture_error(target: &CaptureTarget, call: &'static str, code: HRESULT) -> BackendError {
    if matches!(target, CaptureTarget::Input { .. }) {
        input_code_error(call, code)
    } else {
        device_code_error(call, code)
    }
}

/// Connects an initialized client to its event and capture service.
fn finish_stream(
    client: IAudioClient,
    attempt: CaptureAttempt,
) -> Result<CaptureStream, BackendError> {
    let event = Event::auto_reset()?;
    // SAFETY: calls on an initialized client; the event outlives the stream.
    unsafe { client.SetEventHandle(event.raw()) }
        .map_err(|error| system_error("IAudioClient::SetEventHandle", &error))?;
    // SAFETY: as above.
    let capture = unsafe { client.GetService::<IAudioCaptureClient>() }
        .map_err(|error| system_error("IAudioClient::GetService", &error))?;
    Ok(CaptureStream {
        client,
        capture,
        event,
        format: attempt.format,
        autoconvert: attempt.autoconvert,
        pair: attempt.pair,
    })
}

/// The attempts that capture every channel of input device `endpoint_id`
/// for the pair stored as `saved`, from its mix format, and the speaker mask
/// they name. None, and the stereo attempts alone, when the mix format can't
/// be read or the pair is gone (the log notes that, [`CaptureAttempt::pair`]).
fn input_pair_attempts(
    endpoint_id: &str,
    saved: u16,
) -> Result<(Vec<CaptureAttempt>, u32), BackendError> {
    let client = activate_input(endpoint_id)?;
    // SAFETY: a call on a live client; the mix format is freed by ComMemory
    // after `read_mix_format` copied what it needs.
    let mix = match unsafe { client.GetMixFormat() } {
        Ok(format) => {
            let memory = ComMemory::new(format);
            // SAFETY: GetMixFormat gives a readable format with its extra bytes.
            unsafe { read_mix_format(memory.as_ptr()) }
        }
        Err(_) => return Ok((Vec::new(), 0)),
    };
    Ok((
        pair_attempts(saved, mix.channels, mix.rate),
        mix.channel_mask,
    ))
}

/// An `IAudioClient` for the input device of capture endpoint `endpoint_id`:
/// [`BackendError::InputGone`] when Windows no longer knows the endpoint or
/// it isn't active (unplugged or disabled).
fn activate_input(endpoint_id: &str) -> Result<IAudioClient, BackendError> {
    let enumerator = enumerator()?;
    // SAFETY: calls on a live enumerator and the device it gives.
    unsafe {
        let device = match enumerator.GetDevice(&HSTRING::from(endpoint_id)) {
            Ok(device) => device,
            Err(error) if error.code() == NOT_FOUND => return Err(BackendError::InputGone),
            Err(error) => return Err(device_error("IMMDeviceEnumerator::GetDevice", &error)),
        };
        let state = device
            .GetState()
            .map_err(|error| device_error("IMMDevice::GetState", &error))?;
        if state != DEVICE_STATE_ACTIVE {
            return Err(BackendError::InputGone);
        }
        device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| input_code_error("IMMDevice::Activate", error.code()))
    }
}

/// The activation parameters and the PROPVARIANT that carries them, kept
/// together on the heap so that they stay put while Windows reads them.
#[repr(C)]
struct ActivationArgs {
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
    /// Never dropped: the `windows` crate's PROPVARIANT clears itself on
    /// drop, and clearing a VT_BLOB frees the blob (the parameters above)
    /// with `CoTaskMemFree`.
    variant: ManuallyDrop<PROPVARIANT>,
}

/// An `IAudioClient` for a process loopback capture of `activation`.
fn activate(activation: Activation) -> Result<IAudioClient, BackendError> {
    let done = Arc::new(Event::manual_reset()?);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone {
        done: Arc::clone(&done),
    }
    .into();
    let mode = match activation.mode {
        LoopbackMode::IncludeTree => PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        LoopbackMode::ExcludeTree => PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    };
    let args = Box::into_raw(Box::new(ActivationArgs {
        params: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: activation.target_pid,
                    ProcessLoopbackMode: mode,
                },
            },
        },
        variant: ManuallyDrop::new(PROPVARIANT::default()),
    }));
    // SAFETY: `args` is a live allocation this function owns. The variant
    // gets the BLOB arm of its union, pointing at the parameters next to it;
    // it owns nothing, so it is never dropped.
    unsafe {
        let variant: &mut PROPVARIANT = &mut (*args).variant;
        let value = &mut *variant.Anonymous.Anonymous;
        value.vt = VT_BLOB;
        value.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw mut (*args).params).cast(),
        };
    }
    // SAFETY: the device path and interface ID are static; the variant and
    // its blob stay allocated until the activation completed (or for good,
    // below); the handler is agile, as the call requires.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            // `ManuallyDrop` is `repr(transparent)`.
            Some((&raw const (*args).variant).cast::<PROPVARIANT>()),
            &handler,
        )
    };
    let operation = match operation {
        Ok(operation) => operation,
        Err(error) => {
            // SAFETY: no activation started, so nothing else reads `args`.
            drop(unsafe { Box::from_raw(args) });
            return Err(system_error("ActivateAudioInterfaceAsync", &error));
        }
    };
    // SAFETY: waits on an event this function owns.
    if unsafe { WaitForSingleObject(done.raw(), ACTIVATION_TIMEOUT_MS) } != WAIT_OBJECT_0 {
        // Windows may still read `args`, so it stays allocated.
        return Err(BackendError::System {
            call: "ActivateAudioInterfaceAsync",
            code: TIMED_OUT,
        });
    }
    // SAFETY: the activation completed, so nothing else reads `args`.
    drop(unsafe { Box::from_raw(args) });
    let mut result = HRESULT(0);
    let mut interface: Option<IUnknown> = None;
    // SAFETY: the activation completed, so its result is ready.
    unsafe { operation.GetActivateResult(&mut result, &mut interface) }.map_err(|error| {
        system_error(
            "IActivateAudioInterfaceAsyncOperation::GetActivateResult",
            &error,
        )
    })?;
    result
        .ok()
        .map_err(|error| system_error("ActivateAudioInterfaceAsync", &error))?;
    interface
        .ok_or(BackendError::System {
            call: "ActivateAudioInterfaceAsync",
            code: E_FAIL.0,
        })?
        .cast::<IAudioClient>()
        .map_err(|error| system_error("IUnknown::QueryInterface(IAudioClient)", &error))
}

/// Sets an event when an activation completes. Windows calls it on one of
/// its own threads; `#[implement]` objects are agile (they answer
/// `IAgileObject` and use the free-threaded marshaler), as
/// `ActivateAudioInterfaceAsync` requires of its handler.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone {
    done: Arc<Event>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        self.done.set();
        Ok(())
    }
}

/// The performance counter now, in 100 ns units like a packet's position;
/// 0 when it can't be read.
fn now_hns(frequency: i64) -> u64 {
    let mut count = 0_i64;
    // SAFETY: writes one integer.
    if frequency <= 0 || unsafe { QueryPerformanceCounter(&mut count) }.is_err() {
        return 0;
    }
    (i128::from(count) * 10_000_000 / i128::from(frequency)) as u64
}

/// Captures until `stop` is set (`Ok`) or a call fails (its HRESULT).
///
/// Real-time: waits for Windows' packet event, then reads every packet and
/// hands it to the converter. When no packet came for [`WAIT_MS`], it does
/// the same: the packet size call fails once Windows invalidated the stream.
/// Process loopback delivers nothing while the captured audio is silent, so
/// [`GapFill`] keeps the ring in real time: silence for a jump in the
/// packets' positions, and, while nothing comes, silence up to shortly
/// before now at every wake. Never allocates, locks or logs; it leaves on
/// the first error.
fn capture_loop(
    stream: &CaptureStream,
    converter: &mut CaptureConverter,
    stop: &Event,
    counters: &CaptureCounters,
) -> Result<(), HRESULT> {
    let handles = [stop.raw(), stream.event.raw()];
    let packet_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let mut gaps = GapFill::new(stream.format.rate);
    let mut frequency = 0_i64;
    // SAFETY: writes one integer.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    loop {
        // SAFETY: both handles live as long as this loop.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        if wait != packet_ready && wait != WAIT_TIMEOUT {
            return Err(E_FAIL);
        }
        let packets = drain(
            stream,
            &mut gaps,
            frequency,
            counters,
            &mut |data, frames, silent| {
                converter.push(data, frames, silent);
            },
        )?;
        if packets == 0 {
            let fill = gaps.idle(now_hns(frequency));
            if fill > 0 {
                converter.push(&[], fill, true);
            }
        }
    }
}

/// Reads every packet `stream` holds and hands each to `deliver` as its
/// bytes, frames and whether it is silent, with the silence [`GapFill`]
/// puts before it. Returns the packets read, or the HRESULT of the first
/// failing call. Never allocates, locks or logs.
fn drain(
    stream: &CaptureStream,
    gaps: &mut GapFill,
    frequency: i64,
    counters: &CaptureCounters,
    deliver: &mut dyn FnMut(&[u8], usize, bool),
) -> Result<usize, HRESULT> {
    let block = stream.format.block_align();
    let mut packets = 0;
    loop {
        // SAFETY: calls on the capture client this thread owns.
        let packet = unsafe { stream.capture.GetNextPacketSize() }.map_err(|e| e.code())?;
        if packet == 0 {
            return Ok(packets);
        }
        let mut data = ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        let mut position = 0u64;
        // SAFETY: as above, with writable out parameters.
        unsafe {
            stream.capture.GetBuffer(
                &mut data,
                &mut frames,
                &mut flags,
                None,
                Some(&mut position),
            )
        }
        .map_err(|e| e.code())?;
        if frames == 0 {
            return Ok(packets);
        }
        packets += 1;
        let count = frames as usize;
        // A packet without a position counts as captured just now.
        let start = if position != 0 {
            position
        } else {
            let length = count as u64 * 10_000_000 / u64::from(stream.format.rate);
            now_hns(frequency).saturating_sub(length)
        };
        let discontinuity = flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0;
        let (fill, skip, jumped) = gaps.packet(start, count, discontinuity);
        counters.count_packet(frames, discontinuity, fill, jumped);
        if fill > 0 {
            deliver(&[], fill, true);
        }
        let skip = skip.min(count);
        let silent = data.is_null() || flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        if silent {
            counters.count_silent_packet();
        }
        let bytes: &[u8] = if silent {
            &[]
        } else {
            // SAFETY: GetBuffer handed out `frames` frames in the
            // initialized format, readable until ReleaseBuffer.
            unsafe { slice::from_raw_parts(data, count * block) }
        };
        if skip < count {
            let rest = if silent {
                bytes
            } else {
                &bytes[skip * block..]
            };
            deliver(rest, count - skip, silent);
        }
        // SAFETY: releases the packet read above.
        unsafe { stream.capture.ReleaseBuffer(frames) }.map_err(|e| e.code())?;
    }
}

/// The format a mix hands the converter: what [`StreamMixer`] produces.
///
/// [`StreamMixer`]: crate::audio::mix::StreamMixer
const MIX_FORMAT: StreamFormat = StreamFormat {
    sample: SampleType::F32,
    rate: SESSION_RATE,
    channels: 2,
};

/// How often the opener looks at the plan when nothing wakes it, in ms.
const OPENER_WAIT_MS: u32 = 50;

/// One app's loopback in a mix.
struct LoopbackLane {
    stream: CaptureStream,
    /// Its app, to tell a stream that failed because the app quit; `None`
    /// when it can't be watched.
    watch: Option<ProcessWatch>,
    frequency: i64,
    counters: Arc<CaptureCounters>,
}

impl LoopbackLane {
    /// Opens and starts the loopback of app `pid`'s tree, ready for a mix.
    fn open(
        pid: u32,
        frequency: i64,
        counters: &Arc<CaptureCounters>,
    ) -> Result<Box<Opened<LoopbackLane>>, BackendError> {
        let target = CaptureTarget::Loopback(Activation {
            mode: LoopbackMode::IncludeTree,
            target_pid: pid,
        });
        let stream = open_stream(&target)?;
        let format = stream.format;
        let input = LaneInput::new(format)?;
        // SAFETY: starts the stream initialized above; dropping the lane
        // stops it.
        unsafe { stream.client.Start() }
            .map_err(|error| lane_start_error(&target, error.code()))?;
        Ok(Box::new(Opened {
            pid,
            stream: LoopbackLane {
                stream,
                watch: ProcessWatch::open(pid).ok().and_then(Watch::watching),
                frequency,
                counters: Arc::clone(counters),
            },
            input,
            rate: format.rate,
        }))
    }
}

/// How a mix lane reads a failed `IAudioClient::Start` on `target`.
fn lane_start_error(_target: &CaptureTarget, code: HRESULT) -> BackendError {
    system_error(
        "IAudioClient::Start",
        &windows::core::Error::from_hresult(code),
    )
}

impl Drop for LoopbackLane {
    fn drop(&mut self) {
        // SAFETY: stops the stream started when the lane opened.
        let _ = unsafe { self.stream.client.Stop() };
    }
}

impl LaneStream for LoopbackLane {
    fn drain(
        &mut self,
        gaps: &mut GapFill,
        deliver: &mut dyn FnMut(&[u8], usize, bool),
    ) -> Result<usize, i32> {
        drain(&self.stream, gaps, self.frequency, &self.counters, deliver).map_err(|code| code.0)
    }

    fn app_exited(&self) -> bool {
        self.watch.as_ref().is_some_and(ProcessWatch::has_exited)
    }
}

/// An app's loopback on its way into or out of a mix.
type LanePtr = AtomicPtr<Opened<LoopbackLane>>;

/// What a mix's capture thread shares with the backend and with its
/// opener thread, all without locks. The opener does what can block:
/// activating a new app's loopback, and stopping one the mix let go; the
/// capture thread only moves the ready boxes in and out.
pub(super) struct MixShare {
    /// The apps the backend wants.
    plan: MixPlan,
    /// The apps whose loopbacks are in the mix.
    held: MixPlan,
    /// Loopbacks opened for the mix to take in.
    ready: [LanePtr; MAX_MIX_APPS],
    /// Loopbacks the mix let go, for the opener to stop and free.
    retired: [LanePtr; MAX_MIX_APPS],
    /// Wakes the opener.
    wake: Event,
    quit: AtomicBool,
}

impl MixShare {
    fn new() -> Result<MixShare, BackendError> {
        Ok(MixShare {
            plan: MixPlan::default(),
            held: MixPlan::default(),
            ready: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_MIX_APPS],
            retired: [const { AtomicPtr::new(ptr::null_mut()) }; MAX_MIX_APPS],
            wake: Event::auto_reset()?,
            quit: AtomicBool::new(false),
        })
    }

    /// Frees every box left in `slots`, stopping its stream.
    fn sweep(slots: &[LanePtr]) {
        for slot in slots {
            let lane = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if !lane.is_null() {
                // SAFETY: a non-null pointer here came from `Box::into_raw`
                // and the swap made this thread its only owner. The stream
                // was opened in the multithreaded apartment, which every
                // thread of a mix enters, so any of them may stop it.
                drop(unsafe { Box::from_raw(lane) });
            }
        }
    }

    /// Hands `lane` over through the first free slot of `slots`; gives it
    /// back when none is free.
    fn post(
        slots: &[LanePtr],
        lane: Box<Opened<LoopbackLane>>,
    ) -> Result<usize, Box<Opened<LoopbackLane>>> {
        let raw = Box::into_raw(lane);
        for (index, slot) in slots.iter().enumerate() {
            if slot
                .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(index);
            }
        }
        // SAFETY: `raw` came from `Box::into_raw` above and was posted
        // nowhere.
        Err(unsafe { Box::from_raw(raw) })
    }

    /// Whether the mix holds every app of `pids`, as far as can be told
    /// now.
    pub(super) fn holds_all(&self, pids: &[u32]) -> bool {
        let mut held = [0; MAX_MIX_APPS];
        match self.held.read(u64::MAX, &mut held) {
            Some((count, _)) => pids.iter().all(|pid| held[..count].contains(pid)),
            None => true,
        }
    }
}

impl Drop for MixShare {
    fn drop(&mut self) {
        Self::sweep(&self.ready);
        Self::sweep(&self.retired);
    }
}

/// The opener thread of a mix: opens the loopback of each app the plan
/// names that the mix doesn't hold yet, once per plan, and stops the ones
/// the mix let go, until the mix quits.
fn opener(share: &MixShare, frequency: i64, counters: &Arc<CaptureCounters>) {
    let _apartment = Apartment::enter();
    let (mut seen, mut count) = (0, 0);
    let mut planned = [0_u32; MAX_MIX_APPS];
    let mut held = [0_u32; MAX_MIX_APPS];
    let mut posted = [0_u32; MAX_MIX_APPS];
    let mut tried: Vec<u32> = Vec::new();
    loop {
        MixShare::sweep(&share.retired);
        if share.quit.load(Ordering::Acquire) {
            // The mix hands back its last streams just before it quits.
            MixShare::sweep(&share.retired);
            MixShare::sweep(&share.ready);
            return;
        }
        if let Some((apps, version)) = share.plan.read(seen, &mut planned) {
            (seen, count) = (version, apps);
            // A new plan tries every app again, a failed one included.
            tried.clear();
        }
        if let Some((holding, _)) = share.held.read(u64::MAX, &mut held) {
            for &pid in &planned[..count] {
                let waiting = share.ready.iter().zip(&posted).any(|(slot, &posted)| {
                    posted == pid && !slot.load(Ordering::Acquire).is_null()
                });
                if held[..holding].contains(&pid) || waiting || tried.contains(&pid) {
                    continue;
                }
                tried.push(pid);
                if let Ok(lane) = LoopbackLane::open(pid, frequency, counters)
                    && let Ok(index) = MixShare::post(&share.ready, lane)
                {
                    posted[index] = pid;
                }
            }
        }
        // SAFETY: waits on an event the share owns.
        let _ = unsafe { WaitForSingleObject(share.wake.raw(), OPENER_WAIT_MS) };
    }
}

/// The capture thread of a mix: opens the loopback of every app in
/// `activations`, then mixes them ([`mix_loop`]), following the plan in
/// `share` as the backend changes the apps. A mix of none writes silence
/// in real time, so an "all except" source whose other apps have no audio
/// session still runs. Fails to start only when every app's loopback fails
/// to open.
#[expect(
    clippy::too_many_arguments,
    reason = "everything the thread owns or shares, passed once at its start"
)]
fn run_mix(
    activations: &[Activation],
    share: &Arc<MixShare>,
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
    stop: &Event,
    failure: &AtomicI32,
    ended: &AtomicBool,
    counters: &Arc<CaptureCounters>,
    ready: Ready<CaptureAttempt>,
) {
    let mut converter = match CaptureConverter::new(MIX_FORMAT, writer, equalizer, feeds) {
        Ok(converter) => converter.counting(Arc::clone(counters)),
        Err(error) => return ready.fail(error),
    };
    let mut frequency = 0_i64;
    // SAFETY: writes one integer.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    // The first apps open here, before the mix runs.
    let mut first_error = None;
    let mut opened = Vec::new();
    for activation in activations.iter().take(MAX_MIX_APPS) {
        match LoopbackLane::open(activation.target_pid, frequency, counters) {
            Ok(lane) => opened.push(lane),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error
        && opened.is_empty()
    {
        return ready.fail(error);
    }
    let setup = opened.first().map_or(
        CaptureAttempt {
            format: MIX_FORMAT,
            autoconvert: false,
            pair: InputPair::FIRST,
        },
        |lane| CaptureAttempt {
            format: lane.stream.stream.format,
            autoconvert: lane.stream.stream.autoconvert,
            pair: InputPair::FIRST,
        },
    );
    let now = now_hns(frequency);
    let mut lanes = MixLanes::new(now);
    for lane in opened {
        let _ = lanes.adopt(lane, now);
    }
    let mut held = [0_u32; MAX_MIX_APPS];
    let count = lanes.held(&mut held);
    share.held.set(&held[..count]);
    let opener_thread = {
        let share = Arc::clone(share);
        let counters = Arc::clone(counters);
        thread::Builder::new()
            .name("audiosync-mix-opener".into())
            .spawn(move || opener(&share, frequency, &counters))
    };
    let opener_thread = match opener_thread {
        Ok(thread) => thread,
        Err(_) => return ready.fail(BackendError::NotResponding),
    };
    let _task = ProAudioTask::join();
    let result = if ready.ok(setup) {
        mix_loop(&mut lanes, share, &mut converter, stop, frequency)
    } else {
        Ok(())
    };
    let mut handed = |lane| {
        if let Err(lane) = MixShare::post(&share.retired, lane) {
            drop(lane);
        }
    };
    lanes.retire_unless(|_| false, &mut handed);
    share.quit.store(true, Ordering::Release);
    share.wake.set();
    let _ = opener_thread.join();
    if let Err(code) = result {
        failure.store(code.0, Ordering::Relaxed);
        ended.store(true, Ordering::Release);
    }
}

/// Mixes until `stop` is set (`Ok`) or the mix fails (its HRESULT): every
/// wake takes in the loopbacks the opener has ready, lets go of the apps
/// the plan left out, reads each stream's packets and gap silence into its
/// lane, and hands on what the mixer has ready. Never allocates, frees,
/// locks, blocks or logs: streams come and go as boxes through `share`.
fn mix_loop(
    lanes: &mut MixLanes<LoopbackLane>,
    share: &MixShare,
    converter: &mut CaptureConverter,
    stop: &Event,
    frequency: i64,
) -> Result<(), HRESULT> {
    let mut handles = [HANDLE::default(); MAX_MIX_APPS + 1];
    let mut count = wait_handles(lanes, stop, &mut handles);
    let mut seen = 0;
    let mut planned = [0_u32; MAX_MIX_APPS];
    let mut wanted = lanes.held(&mut planned);
    let (mut before, mut after) = ([0_u32; MAX_MIX_APPS], [0_u32; MAX_MIX_APPS]);
    let mut sink = |stereo: &[f32], frames: usize, filled: &[bool]| {
        converter.push_mixed(as_bytes(stereo), frames, filled);
    };
    let mut handed = |lane| {
        // A full mailbox means the opener is stuck; this one is freed here.
        if let Err(lane) = MixShare::post(&share.retired, lane) {
            drop(lane);
        }
    };
    loop {
        // SAFETY: every handle lives as long as its stream, which lives
        // until the handles are taken again below.
        let wait = unsafe { WaitForMultipleObjects(&handles[..count], false, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        let packet_ready = (1..count as u32).contains(&wait.0.wrapping_sub(WAIT_OBJECT_0.0));
        if wait != WAIT_TIMEOUT && !packet_ready {
            return Err(E_FAIL);
        }
        let now = now_hns(frequency);
        let had = lanes.held(&mut before);
        if let Some((apps, version)) = share.plan.read(seen, &mut planned) {
            (seen, wanted) = (version, apps);
            let keep = &planned[..wanted];
            lanes.retire_unless(|pid| keep.contains(&pid), &mut handed);
        }
        for slot in &share.ready {
            let lane = slot.swap(ptr::null_mut(), Ordering::AcqRel);
            if lane.is_null() {
                continue;
            }
            // SAFETY: the opener posted it from `Box::into_raw`; the swap
            // made this thread its only owner.
            let lane = unsafe { Box::from_raw(lane) };
            if !planned[..wanted].contains(&lane.pid) {
                handed(lane);
            } else if let Err(lane) = lanes.adopt(lane, now) {
                handed(lane);
            }
        }
        lanes.read(now, &mut sink, &mut handed).map_err(HRESULT)?;
        let has = lanes.held(&mut after);
        if before[..had] != after[..has] {
            share.held.set(&after[..has]);
            share.wake.set();
            count = wait_handles(lanes, stop, &mut handles);
        }
    }
}

/// Fills `handles` with the stop event and each open stream's packet event;
/// returns how many it holds.
fn wait_handles(
    lanes: &MixLanes<LoopbackLane>,
    stop: &Event,
    handles: &mut [HANDLE; MAX_MIX_APPS + 1],
) -> usize {
    handles[0] = stop.raw();
    let mut count = 1;
    for (slot, lane) in handles[1..].iter_mut().zip(lanes.streams()) {
        *slot = lane.stream.event.raw();
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use audiosync_core::ring::Ring;
    use windows::Win32::System::Threading::INFINITE;

    use super::super::com::{READY_TIMEOUT, StuckThreads, spawn_worker_within};
    use super::*;
    use crate::audio::SESSION_RATE;

    /// How long the fake Windows audio hangs before it answers again.
    const HANG: Duration = Duration::from_secs(3);

    /// How long a test waits for an answer.
    const TIMEOUT: Duration = Duration::from_millis(200);

    /// How long a real input device gets to deliver its first frames.
    const FIRST_FRAMES: Duration = Duration::from_secs(1);

    /// Captures each of this PC's input devices into a ring nothing plays,
    /// until Windows delivers frames (silent ones count: a muted microphone
    /// still delivers). Ignored by default since it needs Windows audio;
    /// run it on a PC with `cargo test real_inputs -- --ignored --nocapture`.
    #[test]
    #[ignore = "captures the PC's own input devices"]
    fn real_inputs_are_captured_into_the_ring() {
        use crate::audio::backend::SourceId;
        use crate::audio::capture::input_sources;

        let inputs = input_sources(super::super::devices::capture_endpoints().expect("listed"));
        if inputs.is_empty() {
            println!("skipped: this PC has no input device");
            return;
        }
        for input in inputs {
            let SourceId::Input { endpoint_id } = input.id else {
                unreachable!("only input devices are listed");
            };
            let ring = Ring::for_rate(SESSION_RATE).expect("a ring");
            let writer = ring.writer().expect("the only writer");
            let (_control, processor) = audiosync_core::eq::equalizer();
            let capture = start(
                CaptureTarget::Input {
                    endpoint_id: endpoint_id.clone(),
                    pair: 0,
                },
                writer,
                processor,
                Feeds::default(),
            )
            .unwrap_or_else(|error| panic!("{} didn't start: {error}", input.name));
            let started = Instant::now();
            while capture.frames_captured() == 0 && started.elapsed() < FIRST_FRAMES {
                thread::sleep(Duration::from_millis(20));
            }
            println!(
                "{}: {:?}, {} frames captured in {:?}, {} written",
                input.name,
                capture.setup(),
                capture.frames_captured(),
                started.elapsed(),
                ring.frames_written()
            );
            assert_eq!(capture.state(), CaptureState::Running);
            assert!(
                capture.frames_captured() > 0,
                "{} delivered no frames within {FIRST_FRAMES:?}",
                input.name
            );
        }
    }

    #[test]
    fn a_capture_thread_that_hangs_stopping_its_stream_doesnt_hold_quitting() {
        static STUCK: StuckThreads = StuckThreads::new();
        let ring = Ring::for_rate(SESSION_RATE).expect("a ring");
        let writer = ring.writer().expect("the only writer");
        let stop = Arc::new(Event::manual_reset().expect("an event"));
        let (answer, hung) = mpsc::channel::<()>();
        let (stopped, done) = mpsc::channel();
        let (mut thread, ()) = {
            let stop = Arc::clone(&stop);
            spawn_worker_within("audiosync-test", READY_TIMEOUT, &STUCK, move |ready| {
                // The thread owns the ring's writer, as the real one does.
                let _writer = writer;
                if ready.ok(()) {
                    // SAFETY: waits on an event that outlives this thread's
                    // use of it.
                    unsafe { WaitForSingleObject(stop.raw(), INFINITE) };
                    // Stopping the stream hangs until Windows audio answers.
                    let _ = hung.recv();
                    let _ = stopped.send(());
                }
            })
            .expect("it starts")
        };
        thread.set_timeout(TIMEOUT);
        let capture = LoopbackCapture {
            stop,
            failure: Arc::new(AtomicI32::new(0)),
            ended: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(CaptureCounters::default()),
            thread,
            sources: Vec::new(),
            watch_refused: None,
            mix: None,
            mix_pids: Vec::new(),
            setup: CAPTURE_ATTEMPTS[0],
            call: "process loopback capture",
            input: false,
        };
        let answers = thread::spawn(move || {
            thread::sleep(HANG);
            drop(answer);
        });
        let started = Instant::now();
        drop(capture);
        let waited = started.elapsed();
        assert!(waited < HANG / 2, "the drop waited {waited:?}");
        assert!(STUCK.any(), "the thread counts as stuck");
        // Once Windows audio answers, the thread still stops its stream, and
        // ends.
        assert_eq!(done.recv_timeout(HANG * 3), Ok(()));
        assert!(STUCK.none_within(HANG), "the thread ends");
        answers.join().expect("the answer comes");
    }

    fn microphone() -> CaptureTarget {
        CaptureTarget::Input {
            endpoint_id: "{0.0.1.00000000}.{mic}".to_owned(),
            pair: 0,
        }
    }

    // covers: S333
    #[test]
    fn an_input_held_exclusively_is_busy_at_initialize_and_at_start() {
        use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_IN_USE;
        for call in ["IAudioClient::Initialize", "IAudioClient::Start"] {
            assert_eq!(
                capture_error(&microphone(), call, AUDCLNT_E_DEVICE_IN_USE),
                BackendError::DeviceBusy,
                "{call}"
            );
        }
    }

    // covers: S333
    #[test]
    fn an_input_whose_device_went_away_is_gone_at_initialize() {
        use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED;
        assert_eq!(
            capture_error(
                &microphone(),
                "IAudioClient::Initialize",
                AUDCLNT_E_DEVICE_INVALIDATED
            ),
            BackendError::DeviceGone
        );
    }

    // covers: S333
    #[test]
    fn a_mix_lane_whose_device_went_away_is_gone_at_start() {
        use windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED;
        let lane = CaptureTarget::Loopback(Activation {
            mode: LoopbackMode::IncludeTree,
            target_pid: 4,
        });
        assert_eq!(
            lane_start_error(&lane, AUDCLNT_E_DEVICE_INVALIDATED),
            BackendError::DeviceGone
        );
    }

    // covers: S333, S141
    #[test]
    fn an_input_blocked_by_privacy_settings_stays_microphone_blocked() {
        use windows::Win32::Foundation::E_ACCESSDENIED;
        for call in ["IAudioClient::Initialize", "IAudioClient::Start"] {
            assert_eq!(
                capture_error(&microphone(), call, E_ACCESSDENIED),
                BackendError::MicrophoneBlocked,
                "{call}"
            );
        }
    }

    #[test]
    fn a_timed_out_activation_carries_the_windows_timeout_code() {
        use windows::Win32::Foundation::ERROR_TIMEOUT;
        assert_eq!(TIMED_OUT, HRESULT::from_win32(ERROR_TIMEOUT.0).0);
    }

    #[test]
    fn activation_args_never_clear_their_variant() {
        // The `windows` crate's PROPVARIANT clears itself on drop, and
        // clearing a VT_BLOB frees the blob with CoTaskMemFree. The blob is
        // the parameters inside the same allocation, so a drop would free
        // Rust's allocation behind its back: a heap corruption.
        assert!(!std::mem::needs_drop::<ActivationArgs>());
    }
}
