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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use audiosync_core::eq::EqProcessor;
use audiosync_core::ring::RingWriter;

use crate::audio::airplay::FeedLease;
use windows::Win32::Foundation::{E_FAIL, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
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
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, HSTRING, IUnknown, Interface, Ref};
use windows_core::implement;

use super::com::{
    Apartment, Event, NOT_FOUND, ProAudioTask, Ready, Worker, device_code_error, device_error,
    spawn_worker, system_error, wave_format,
};
use super::devices::enumerator;
use super::processes::ProcessWatch;
use crate::audio::backend::{BackendError, TIMED_OUT};
use crate::audio::capture::{
    Activation, CAPTURE_ATTEMPTS, CaptureAttempt, CaptureConverter, CaptureTarget, GapFill,
    LoopbackMode, StreamFormat,
};
use crate::audio::controller::{CaptureHandle, CaptureState};
use crate::audio::outputs::HNS_PER_MS;

/// The buffer a capture stream asks for, 100 ms: room for the capture thread
/// to be late without losing packets.
const CAPTURE_BUFFER_HNS: i64 = 100 * HNS_PER_MS;

/// How long a capture waits for its activation to complete, in ms.
const ACTIVATION_TIMEOUT_MS: u32 = 5_000;

/// How long the capture loop waits for a packet before it checks that the
/// stream still works and keeps the ring in real time, in ms. A process tree
/// that plays nothing delivers nothing, so the loop writes silence for the
/// time that passed ([`GapFill`]), often enough that no reader runs out of
/// its 35 ms dead band (S50); and a stream that Windows invalidated (when
/// Windows audio restarts, say) may stop signalling without a word; only a
/// call on the stream then tells.
const WAIT_MS: u32 = 10;

/// A running process loopback or input device capture.
pub struct LoopbackCapture {
    stop: Arc<Event>,
    /// The HRESULT that ended the capture, once `ended` is set.
    failure: Arc<AtomicI32>,
    ended: Arc<AtomicBool>,
    thread: Worker,
    /// The captured app, to notice when it quits.
    source: Option<ProcessWatch>,
    /// How Windows accepted to deliver the capture.
    setup: CaptureAttempt,
    /// What the log calls the capture when it fails.
    call: &'static str,
}

impl CaptureHandle for LoopbackCapture {
    fn state(&self) -> CaptureState {
        // An app that quit comes first: its stream may fail because of it.
        if self.source.as_ref().is_some_and(ProcessWatch::has_exited) {
            return CaptureState::SourceQuit;
        }
        if self.ended.load(Ordering::Acquire) {
            let code = HRESULT(self.failure.load(Ordering::Relaxed));
            return CaptureState::Failed(device_code_error(self.call, code));
        }
        CaptureState::Running
    }

    fn setup(&self) -> Option<CaptureAttempt> {
        Some(self.setup)
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
/// to `airplay`.
pub(super) fn start(
    target: CaptureTarget,
    writer: RingWriter,
    equalizer: EqProcessor,
    airplay: Option<FeedLease>,
) -> Result<LoopbackCapture, BackendError> {
    let (source, call) = match &target {
        CaptureTarget::Loopback(activation) => (
            match activation.mode {
                LoopbackMode::IncludeTree => ProcessWatch::open(activation.target_pid)?,
                LoopbackMode::ExcludeTree => None,
            },
            "process loopback capture",
        ),
        CaptureTarget::Input { .. } => (None, "input device capture"),
    };
    let stop = Arc::new(Event::manual_reset()?);
    let failure = Arc::new(AtomicI32::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let (thread, setup) = {
        let stop = Arc::clone(&stop);
        let failure = Arc::clone(&failure);
        let ended = Arc::clone(&ended);
        spawn_worker("audiosync-capture", move |ready| {
            run(
                &target, writer, equalizer, airplay, &stop, &failure, &ended, ready,
            );
        })?
    };
    Ok(LoopbackCapture {
        stop,
        failure,
        ended,
        thread,
        source,
        setup,
        call,
    })
}

/// The capture thread. The AirPlay feed goes back to its slot when the
/// converter is dropped, as the thread ends.
#[expect(
    clippy::too_many_arguments,
    reason = "everything the thread owns or shares, passed once at its start"
)]
fn run(
    target: &CaptureTarget,
    writer: RingWriter,
    equalizer: EqProcessor,
    airplay: Option<FeedLease>,
    stop: &Event,
    failure: &AtomicI32,
    ended: &AtomicBool,
    ready: Ready<CaptureAttempt>,
) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let stream = match open_stream(target) {
        Ok(stream) => stream,
        Err(error) => return ready.fail(error),
    };
    let mut converter = match CaptureConverter::new(stream.format, writer, equalizer, airplay) {
        Ok(converter) => converter,
        Err(error) => return ready.fail(error),
    };
    let _task = ProAudioTask::join();
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        return ready.fail(system_error("IAudioClient::Start", &error));
    }
    let waited = ready.ok(CaptureAttempt {
        format: stream.format,
        autoconvert: stream.autoconvert,
    });
    if !waited {
        // Nobody waits for this capture anymore, and nothing would stop it:
        // it stops at once rather than capture unseen.
        // SAFETY: stops the stream started above.
        let _ = unsafe { stream.client.Stop() };
        return;
    }
    let result = capture_loop(&stream, &mut converter, stop);
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
}

/// Activates and initializes a capture, trying each format of
/// [`CAPTURE_ATTEMPTS`] until one is accepted.
fn open_stream(target: &CaptureTarget) -> Result<CaptureStream, BackendError> {
    let mut first_error = None;
    for attempt in CAPTURE_ATTEMPTS {
        let (client, mut flags) = match target {
            CaptureTarget::Loopback(activation) => (
                activate(*activation)?,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            ),
            CaptureTarget::Input { endpoint_id } => (
                activate_input(endpoint_id)?,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            ),
        };
        let format = wave_format(attempt.format);
        if attempt.autoconvert {
            flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        }
        // SAFETY: `format` lives through the call, which copies it.
        let initialized = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                CAPTURE_BUFFER_HNS,
                0,
                &format,
                None,
            )
        };
        match initialized {
            Ok(()) => return finish_stream(client, attempt),
            Err(error) => {
                first_error.get_or_insert(system_error("IAudioClient::Initialize", &error));
            }
        }
    }
    Err(first_error.unwrap_or(BackendError::UnsupportedFormat))
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
    })
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
            .map_err(|error| device_error("IMMDevice::Activate", &error))
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
) -> Result<(), HRESULT> {
    let handles = [stop.raw(), stream.event.raw()];
    let packet_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let block = stream.format.block_align();
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
        let mut packets = 0;
        loop {
            // SAFETY: calls on the capture client this thread owns.
            let packet = unsafe { stream.capture.GetNextPacketSize() }.map_err(|e| e.code())?;
            if packet == 0 {
                break;
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
                break;
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
            let (fill, skip) = gaps.packet(start, count, discontinuity);
            if fill > 0 {
                converter.push(&[], fill, true);
            }
            let skip = skip.min(count);
            let silent = data.is_null() || flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
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
                converter.push(rest, count - skip, silent);
            }
            // SAFETY: releases the packet read above.
            unsafe { stream.capture.ReleaseBuffer(frames) }.map_err(|e| e.code())?;
        }
        if packets == 0 {
            let fill = gaps.idle(now_hns(frequency));
            if fill > 0 {
                converter.push(&[], fill, true);
            }
        }
    }
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
    fn a_capture_thread_that_hangs_stopping_its_stream_doesnt_hold_quitting() {
        static STUCK: StuckThreads = StuckThreads::new();
        let ring = Ring::for_rate(SESSION_RATE).expect("a ring");
        let writer = ring.writer().expect("the only writer");
        let stop = Arc::new(Event::manual_reset().expect("an event"));
        let (answer, hung) = mpsc::channel::<()>();
        let (stopped, done) = mpsc::channel();
        let (thread, ()) = {
            let stop = Arc::clone(&stop);
            spawn_worker_within("audiosync-test", TIMEOUT, &STUCK, move |ready| {
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
        let capture = LoopbackCapture {
            stop,
            failure: Arc::new(AtomicI32::new(0)),
            ended: Arc::new(AtomicBool::new(false)),
            thread,
            source: None,
            setup: CAPTURE_ATTEMPTS[0],
            call: "process loopback capture",
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
