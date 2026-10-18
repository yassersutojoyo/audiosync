//! Process loopback capture, through the Application Loopback API.
//!
//! `ActivateAudioInterfaceAsync` on the virtual process loopback device gives
//! an `IAudioClient` that captures one process tree, or everything except it.
//! The activation completes on a Windows thread, which calls our completion
//! handler; the capture thread waits for it. Process loopback has no mix
//! format, so `Initialize` names the format explicitly, trying
//! [`CAPTURE_ATTEMPTS`] in order. The capture thread is event-driven, in the
//! multithreaded apartment and registered with MMCSS; its loop passes each
//! packet to a [`CaptureConverter`], which equalizes it and writes the ring.

use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::JoinHandle;

use audiosync_core::eq::EqProcessor;
use audiosync_core::ring::RingWriter;
use windows::Win32::Foundation::{E_FAIL, ERROR_TIMEOUT, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref};
use windows_core::implement;

use super::com::{
    Apartment, Event, ProAudioTask, Ready, device_code_error, spawn_ready, system_error,
    wave_format,
};
use super::processes::ProcessWatch;
use crate::audio::backend::BackendError;
use crate::audio::capture::{
    Activation, CAPTURE_ATTEMPTS, CaptureAttempt, CaptureConverter, LoopbackMode, StreamFormat,
};
use crate::audio::controller::{CaptureHandle, CaptureState};
use crate::audio::outputs::HNS_PER_MS;

/// The buffer a capture stream asks for, 100 ms: room for the capture thread
/// to be late without losing packets.
const CAPTURE_BUFFER_HNS: i64 = 100 * HNS_PER_MS;

/// How long a capture waits for its activation to complete, in ms.
const ACTIVATION_TIMEOUT_MS: u32 = 5_000;

/// How long the capture loop waits for a packet before it checks that the
/// stream still works, in ms. A process tree that plays nothing may deliver
/// nothing, and a stream that Windows invalidated (when Windows audio
/// restarts, say) may stop signalling without a word; only a call on the
/// stream then tells.
const WAIT_MS: u32 = 500;

/// A running process loopback capture.
pub struct LoopbackCapture {
    stop: Arc<Event>,
    /// The HRESULT that ended the capture, once `ended` is set.
    failure: Arc<AtomicI32>,
    ended: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The captured app, to notice when it quits.
    source: Option<ProcessWatch>,
    /// How Windows accepted to deliver the capture.
    setup: CaptureAttempt,
}

impl CaptureHandle for LoopbackCapture {
    fn state(&self) -> CaptureState {
        // An app that quit comes first: its stream may fail because of it.
        if self.source.as_ref().is_some_and(ProcessWatch::has_exited) {
            return CaptureState::SourceQuit;
        }
        if self.ended.load(Ordering::Acquire) {
            let code = HRESULT(self.failure.load(Ordering::Relaxed));
            return CaptureState::Failed(device_code_error("process loopback capture", code));
        }
        CaptureState::Running
    }

    fn setup(&self) -> Option<CaptureAttempt> {
        Some(self.setup)
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts capturing as `activation` says, writing through `equalizer` and
/// `writer`.
pub(super) fn start(
    activation: Activation,
    writer: RingWriter,
    equalizer: EqProcessor,
) -> Result<LoopbackCapture, BackendError> {
    let source = match activation.mode {
        LoopbackMode::IncludeTree => ProcessWatch::open(activation.target_pid)?,
        LoopbackMode::ExcludeTree => None,
    };
    let stop = Arc::new(Event::manual_reset()?);
    let failure = Arc::new(AtomicI32::new(0));
    let ended = Arc::new(AtomicBool::new(false));
    let (thread, setup) = {
        let stop = Arc::clone(&stop);
        let failure = Arc::clone(&failure);
        let ended = Arc::clone(&ended);
        spawn_ready("audiosync-capture", move |ready| {
            run(
                activation, writer, equalizer, &stop, &failure, &ended, ready,
            );
        })?
    };
    Ok(LoopbackCapture {
        stop,
        failure,
        ended,
        thread: Some(thread),
        source,
        setup,
    })
}

/// The capture thread.
fn run(
    activation: Activation,
    writer: RingWriter,
    equalizer: EqProcessor,
    stop: &Event,
    failure: &AtomicI32,
    ended: &AtomicBool,
    ready: Ready<CaptureAttempt>,
) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let stream = match open_stream(activation) {
        Ok(stream) => stream,
        Err(error) => return ready.fail(error),
    };
    let mut converter = match CaptureConverter::new(stream.format, writer, equalizer) {
        Ok(converter) => converter,
        Err(error) => return ready.fail(error),
    };
    let _task = ProAudioTask::join();
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        return ready.fail(system_error("IAudioClient::Start", &error));
    }
    ready.ok(CaptureAttempt {
        format: stream.format,
        autoconvert: stream.autoconvert,
    });
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
fn open_stream(activation: Activation) -> Result<CaptureStream, BackendError> {
    let mut first_error = None;
    for attempt in CAPTURE_ATTEMPTS {
        let client = activate(activation)?;
        let format = wave_format(attempt.format);
        let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
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

/// The activation parameters and the PROPVARIANT that carries them, kept
/// together on the heap so that they stay put while Windows reads them.
#[repr(C)]
struct ActivationArgs {
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
    variant: PROPVARIANT,
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
        variant: PROPVARIANT::default(),
    }));
    // SAFETY: `args` is a live allocation this function owns. The variant
    // gets the BLOB arm of its union, pointing at the parameters next to it;
    // it owns nothing, so it is never cleared.
    unsafe {
        let value = &mut *(*args).variant.Anonymous.Anonymous;
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
            Some(&raw const (*args).variant),
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
            code: HRESULT::from_win32(ERROR_TIMEOUT.0).0,
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

/// Captures until `stop` is set (`Ok`) or a call fails (its HRESULT).
///
/// Real-time: waits for Windows' packet event, then reads every packet and
/// hands it to the converter. When no packet came for [`WAIT_MS`], it does
/// the same: the packet size call fails once Windows invalidated the stream.
/// Never allocates, locks or logs; it leaves on the first error.
fn capture_loop(
    stream: &CaptureStream,
    converter: &mut CaptureConverter,
    stop: &Event,
) -> Result<(), HRESULT> {
    let handles = [stop.raw(), stream.event.raw()];
    let packet_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let block = stream.format.block_align();
    loop {
        // SAFETY: both handles live as long as this loop.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        if wait != packet_ready && wait != WAIT_TIMEOUT {
            return Err(E_FAIL);
        }
        loop {
            // SAFETY: calls on the capture client this thread owns.
            let packet = unsafe { stream.capture.GetNextPacketSize() }.map_err(|e| e.code())?;
            if packet == 0 {
                break;
            }
            let mut data = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: as above, with writable out parameters.
            unsafe {
                stream
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .map_err(|e| e.code())?;
            if frames == 0 {
                break;
            }
            let silent = data.is_null() || flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let bytes: &[u8] = if silent {
                &[]
            } else {
                // SAFETY: GetBuffer handed out `frames` frames in the
                // initialized format, readable until ReleaseBuffer.
                unsafe { slice::from_raw_parts(data, frames as usize * block) }
            };
            converter.push(bytes, frames as usize, silent);
            // SAFETY: releases the packet read above.
            unsafe { stream.capture.ReleaseBuffer(frames) }.map_err(|e| e.code())?;
        }
    }
}
