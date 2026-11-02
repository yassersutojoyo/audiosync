//! The calibration's recording of the default input device: shared mode,
//! raw (without the device's effects) when the device and Windows allow it,
//! 32-bit float at the device's own rate and channels (Windows converts from
//! its mix format), mixed down to mono into a buffer allocated
//! before the stream starts. The thread stamps the first frame with the
//! core's clock, from a packet's position on the performance counter, so a
//! run can find the frame where its signal entered the ring, and marks where
//! the device lost audio, which would move every later frame, and when it
//! stopped on its own, as an unplugged device does.

use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use audiosync_core::clock;
use windows::Win32::Foundation::{E_ACCESSDENIED, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDCLNT_STREAMOPTIONS_RAW, AudioCategory_Other,
    AudioClientProperties, IAudioCaptureClient, IAudioClient, IAudioClient2, IMMDevice,
    WAVEFORMATEX, eCapture, eConsole,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::core::{HRESULT, Interface};

use super::com::{
    Apartment, ComMemory, Event, Ready, Worker, float_format, read_mix_format, spawn_worker,
};
use super::devices::{device_id, enumerator, properties, raw_processing_supported};
use crate::audio::calibration::{Microphone, MicrophoneError, MicrophoneId, RecordingClock};
use crate::audio::outputs::{
    HNS_PER_MS, MixFormat, RenderFormat, default_channel_mask, is_hands_free,
};
use crate::debug_log::DebugLog;

/// The stream buffer asked for, 100 ms.
const BUFFER_HNS: i64 = 100 * HNS_PER_MS;

/// Windows converts even from a float mix format: the channel mask asked
/// for may not be the device's.
const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

/// How long the loop waits for a packet before it checks the stream.
const WAIT_MS: u32 = 20;

/// No first frame yet.
const UNSET: u64 = u64::MAX;

/// No audio lost yet.
const NO_GAP: usize = usize::MAX;

/// A running recording of the default input device.
pub struct WasapiMicrophone {
    id: MicrophoneId,
    stop: Arc<Event>,
    frames: Arc<AtomicUsize>,
    first_ns: Arc<AtomicU64>,
    gap_at: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    recording: Receiver<Vec<f32>>,
    thread: Worker,
}

/// Starts recording the default input device, with room for `seconds`.
pub(super) fn open(seconds: f64) -> Result<WasapiMicrophone, MicrophoneError> {
    let stop = Arc::new(Event::manual_reset().map_err(|_| MicrophoneError::Missing)?);
    let frames = Arc::new(AtomicUsize::new(0));
    let first_ns = Arc::new(AtomicU64::new(UNSET));
    let gap_at = Arc::new(AtomicUsize::new(NO_GAP));
    let stopped = Arc::new(AtomicBool::new(false));
    let (send, recording) = mpsc::channel();
    let (thread, id) = {
        let stop = Arc::clone(&stop);
        let marks = Marks {
            frames: Arc::clone(&frames),
            first_ns: Arc::clone(&first_ns),
            gap_at: Arc::clone(&gap_at),
            stopped: Arc::clone(&stopped),
        };
        spawn_worker("audiosync-microphone", move |ready| {
            run(seconds, &stop, &marks, &send, ready);
        })
        .map_err(|_| MicrophoneError::Missing)?
    };
    match id {
        Ok(id) => Ok(WasapiMicrophone {
            id,
            stop,
            frames,
            first_ns,
            gap_at,
            stopped,
            recording,
            thread,
        }),
        Err(error) => Err(error),
    }
}

impl Microphone for WasapiMicrophone {
    fn id(&self) -> MicrophoneId {
        self.id.clone()
    }

    fn first_frame_ns(&self) -> Option<u64> {
        let ns = self.first_ns.load(Ordering::Acquire);
        (ns != UNSET).then_some(ns)
    }

    fn frames_recorded(&self) -> usize {
        self.frames.load(Ordering::Acquire)
    }

    fn gap_at(&self) -> Option<usize> {
        let gap = self.gap_at.load(Ordering::Acquire);
        (gap != NO_GAP).then_some(gap)
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn take(&mut self, start: usize, count: usize) -> Vec<f32> {
        self.stop.set();
        self.thread.finish();
        let mut samples = self.recording.try_recv().unwrap_or_default();
        let end = start.saturating_add(count).min(samples.len());
        if start >= end {
            return Vec::new();
        }
        samples.truncate(end);
        samples.drain(..start);
        samples
    }
}

impl Drop for WasapiMicrophone {
    fn drop(&mut self) {
        self.stop.set();
        self.thread.finish();
    }
}

/// An initialized capture stream of the default input device.
struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Declared after the clients, so it is closed after them.
    event: Event,
    id: MicrophoneId,
    channels: usize,
    raw: Raw,
}

/// What the recording thread tells the run while it records.
struct Marks {
    frames: Arc<AtomicUsize>,
    first_ns: Arc<AtomicU64>,
    gap_at: Arc<AtomicUsize>,
    /// The stream failed before it was told to stop.
    stopped: Arc<AtomicBool>,
}

fn setup_error(code: HRESULT) -> MicrophoneError {
    match code {
        E_ACCESSDENIED => MicrophoneError::Denied,
        AUDCLNT_E_DEVICE_IN_USE => MicrophoneError::InUse,
        _ => MicrophoneError::Missing,
    }
}

/// What became of the request for the input's raw signal, which skips the
/// device's effects (automatic gain, noise suppression) that can reshape the
/// test signal differently from one run to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Raw {
    Used,
    /// The device says it has no raw signal.
    Unsupported,
    /// The client refused the request.
    Refused,
    /// Initialize failed with the request; the stream opened again without it.
    Failed,
}

/// The outcome of asking for raw on a device that `supported` it, when the
/// client `accepted` the request and the stream `initialized` with it.
fn raw_outcome(supported: bool, accepted: bool, initialized: bool) -> Raw {
    match (supported, accepted, initialized) {
        (false, _, _) => Raw::Unsupported,
        (true, false, _) => Raw::Refused,
        (true, true, false) => Raw::Failed,
        (true, true, true) => Raw::Used,
    }
}

/// The debug line saying whether the recording is raw.
fn raw_note(raw: Raw) -> &'static str {
    match raw {
        Raw::Used => {
            "Calibration microphone: recording the raw signal, without the input's effects"
        }
        Raw::Unsupported => {
            "Calibration microphone: the input offers no raw signal; recording with the input's effects"
        }
        Raw::Refused => {
            "Calibration microphone: Windows refused the raw signal; recording with the input's effects"
        }
        Raw::Failed => {
            "Calibration microphone: the raw stream failed to open; recording with the input's effects"
        }
    }
}

/// An initialized client of `device`, its mix format and whether it took the
/// request for raw, made when `raw` is set; or why it failed, and whether it
/// had taken the request.
unsafe fn initialize(
    device: &IMMDevice,
    raw: bool,
) -> Result<(IAudioClient, MixFormat, bool), (MicrophoneError, bool)> {
    // SAFETY: as in open_stream, whose device this is.
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| (setup_error(error.code()), false))?;
        // Windows takes the stream's options only before Initialize.
        let properties = AudioClientProperties {
            cbSize: size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AudioCategory_Other,
            Options: AUDCLNT_STREAMOPTIONS_RAW,
        };
        let accepted = raw
            && client
                .cast::<IAudioClient2>()
                .and_then(|client| client.SetClientProperties(&raw const properties))
                .is_ok();
        let mix = ComMemory::new(
            client
                .GetMixFormat()
                .map_err(|error| (setup_error(error.code()), accepted))?,
        );
        let mix = read_mix_format(mix.as_ptr());
        if mix.channels == 0 || mix.rate == 0 {
            return Err((MicrophoneError::Missing, accepted));
        }
        let own = float_format(RenderFormat {
            rate: mix.rate,
            channels: mix.channels,
            channel_mask: if mix.channel_mask == 0 {
                default_channel_mask(mix.channels)
            } else {
                mix.channel_mask
            },
            convert: true,
        });
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                STREAM_FLAGS,
                BUFFER_HNS,
                0,
                (&raw const own).cast::<WAVEFORMATEX>(),
                None,
            )
            .map_err(|error| (setup_error(error.code()), accepted))?;
        Ok((client, mix, accepted))
    }
}

fn open_stream() -> Result<Stream, MicrophoneError> {
    let enumerator = enumerator().map_err(|_| MicrophoneError::Missing)?;
    // SAFETY: calls on COM objects this thread owns. The mix format is freed
    // by ComMemory after its last use, and the format passed to Initialize
    // lives through the call, which copies it.
    unsafe {
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .map_err(|_| MicrophoneError::Missing)?;
        let enumerator_name = properties(&device).ok().and_then(|found| found.enumerator);
        if is_hands_free(enumerator_name.as_deref().unwrap_or_default()) {
            return Err(MicrophoneError::HandsFree);
        }
        let device_name = device_id(&device).unwrap_or_default();
        let supported = raw_processing_supported(&device);
        let mut opened = initialize(&device, supported);
        let raw = match &opened {
            Ok((_, _, accepted)) => raw_outcome(supported, *accepted, true),
            Err((_, accepted)) => raw_outcome(supported, *accepted, false),
        };
        if raw == Raw::Failed {
            opened = initialize(&device, false);
        }
        let (client, mix, _) = opened.map_err(|(error, _)| error)?;
        let event = Event::auto_reset().map_err(|_| MicrophoneError::Missing)?;
        client
            .SetEventHandle(event.raw())
            .map_err(|_| MicrophoneError::Missing)?;
        let capture = client
            .GetService::<IAudioCaptureClient>()
            .map_err(|_| MicrophoneError::Missing)?;
        Ok(Stream {
            client,
            capture,
            event,
            id: MicrophoneId {
                device: device_name,
                rate: mix.rate,
            },
            channels: usize::from(mix.channels),
            raw,
        })
    }
}

/// The recording thread: opens the stream, reports, records until told to
/// stop or the buffer is full, and hands the recording back.
fn run(
    seconds: f64,
    stop: &Event,
    marks: &Marks,
    send: &Sender<Vec<f32>>,
    ready: Ready<Result<MicrophoneId, MicrophoneError>>,
) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(_) => {
            let _ = ready.ok(Err(MicrophoneError::Missing));
            return;
        }
    };
    let stream = match open_stream() {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.ok(Err(error));
            return;
        }
    };
    let capacity = (seconds.max(0.0) * f64::from(stream.id.rate)).ceil() as usize;
    let mut samples = vec![0.0_f32; capacity];
    // SAFETY: starts the stream initialized above.
    if let Err(error) = unsafe { stream.client.Start() } {
        let _ = ready.ok(Err(setup_error(error.code())));
        return;
    }
    if ready.ok(Ok(stream.id.clone())) {
        let (written, failed) = record(&stream, &mut samples, stop, marks);
        samples.truncate(written);
        if failed {
            marks.stopped.store(true, Ordering::Release);
        }
    }
    // SAFETY: stops the stream started above.
    let _ = unsafe { stream.client.Stop() };
    // Logged after the recording, as opening the log and flushing it would delay its start.
    let log = DebugLog::appending_from_env();
    log.log(|| raw_note(stream.raw).to_owned());
    log.flush(Duration::from_secs(1));
    let _ = send.send(samples);
}

/// The performance counter now, in 100 ns units like a packet's position.
fn now_hns(frequency: i64) -> Option<u64> {
    let mut count = 0_i64;
    // SAFETY: writes one integer.
    if frequency <= 0 || unsafe { QueryPerformanceCounter(&mut count) }.is_err() {
        return None;
    }
    Some((i128::from(count) * 10_000_000 / i128::from(frequency)) as u64)
}

/// Records into `samples` until `stop` is set, a call fails or it is full;
/// returns the frames recorded and whether a call failed. Allocates nothing.
fn record(stream: &Stream, samples: &mut [f32], stop: &Event, marks: &Marks) -> (usize, bool) {
    let handles = [stop.raw(), stream.event.raw()];
    let packet_ready = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
    let mut frequency = 0_i64;
    // SAFETY: writes one integer.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    let mut timing = RecordingClock::default();
    let mut written = 0;
    loop {
        // SAFETY: both handles live as long as this loop.
        let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_MS) };
        if wait == WAIT_OBJECT_0 {
            return (written, false);
        }
        if wait != packet_ready && wait != WAIT_TIMEOUT {
            return (written, true);
        }
        loop {
            // SAFETY: calls on the capture client this thread owns.
            let Ok(packet) = (unsafe { stream.capture.GetNextPacketSize() }) else {
                return (written, true);
            };
            if packet == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut count = 0u32;
            let mut flags = 0u32;
            let mut position = 0u64;
            // SAFETY: as above, with writable out parameters.
            if unsafe {
                stream.capture.GetBuffer(
                    &mut data,
                    &mut count,
                    &mut flags,
                    None,
                    Some(&mut position),
                )
            }
            .is_err()
            {
                return (written, true);
            }
            let count = count as usize;
            if count > 0 {
                let now_ns = clock::now_ns();
                let age_ns = now_hns(frequency)
                    .filter(|_| position != 0)
                    .map(|now| now.saturating_sub(position) * 100);
                timing.packet(
                    written,
                    stream.id.rate,
                    now_ns,
                    age_ns,
                    flags & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0,
                    flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0,
                );
                if let Some(first) = timing.first_frame_ns {
                    marks.first_ns.store(first, Ordering::Release);
                }
                if let Some(gap) = timing.gap_at {
                    marks.gap_at.store(gap, Ordering::Release);
                }
            }
            let room = samples.len() - written;
            let take = count.min(room);
            let out = &mut samples[written..written + take];
            if data.is_null() || flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                out.fill(0.0);
            } else {
                // SAFETY: GetBuffer handed out `count` frames of float
                // samples in the initialized format, readable until
                // ReleaseBuffer.
                let packet =
                    unsafe { slice::from_raw_parts(data.cast::<f32>(), count * stream.channels) };
                let scale = 1.0 / stream.channels as f32;
                for (slot, frame) in out.iter_mut().zip(packet.chunks_exact(stream.channels)) {
                    *slot = frame.iter().sum::<f32>() * scale;
                }
            }
            written += take;
            marks.frames.store(written, Ordering::Release);
            // SAFETY: releases the packet read above.
            if unsafe { stream.capture.ReleaseBuffer(count as u32) }.is_err() {
                return (written, true);
            }
            if written == samples.len() {
                return (written, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::E_FAIL;

    use super::*;

    // covers: S232
    #[test]
    fn a_device_held_by_another_app_is_told_apart_from_a_missing_one() {
        assert_eq!(setup_error(E_ACCESSDENIED), MicrophoneError::Denied);
        assert_eq!(setup_error(AUDCLNT_E_DEVICE_IN_USE), MicrophoneError::InUse);
        assert_eq!(setup_error(E_FAIL), MicrophoneError::Missing);
    }

    #[test]
    fn the_calibration_records_raw_only_when_the_device_and_windows_allow_it() {
        assert_eq!(raw_outcome(false, false, true), Raw::Unsupported);
        assert_eq!(raw_outcome(false, false, false), Raw::Unsupported);
        assert_eq!(raw_outcome(true, false, true), Raw::Refused);
        assert_eq!(raw_outcome(true, false, false), Raw::Refused);
        assert_eq!(raw_outcome(true, true, false), Raw::Failed);
        assert_eq!(raw_outcome(true, true, true), Raw::Used);
        assert!(raw_note(Raw::Used).ends_with("without the input's effects"));
        let fallbacks = [Raw::Unsupported, Raw::Refused, Raw::Failed].map(raw_note);
        for note in fallbacks {
            assert!(note.ends_with(" with the input's effects"), "{note}");
        }
        assert!(fallbacks[0] != fallbacks[1] && fallbacks[1] != fallbacks[2]);
    }
}
