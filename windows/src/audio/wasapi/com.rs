//! The plumbing the WASAPI driver shares: COM apartments, kernel handles and
//! events, MMCSS registration, threads that report their start, wave formats
//! and errors.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, ERROR_NOT_FOUND, HANDLE, RPC_E_CHANGED_MODE,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING, WAVE_FORMAT_PCM, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
};
use windows::core::{HRESULT, PCWSTR, PWSTR, w};

use crate::audio::backend::BackendError;
use crate::audio::capture::{SampleType, StreamFormat};
use crate::audio::outputs::{MixFormat, RenderFormat};

/// COM on the calling thread, in the multithreaded apartment, for as long as
/// this value lives. A thread that is already in a single-threaded apartment
/// (a UI thread) keeps it, and COM works there too. Declare it before the COM
/// objects it serves, so they are released first.
pub(super) struct Apartment {
    initialized: bool,
    /// COM initialization belongs to the thread that made it.
    _thread_bound: PhantomData<*const ()>,
}

impl Apartment {
    pub(super) fn enter() -> Result<Apartment, BackendError> {
        // SAFETY: initializes COM for the calling thread only; Drop undoes a
        // successful call on the same thread.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Apartment {
                initialized: false,
                _thread_bound: PhantomData,
            });
        }
        result
            .ok()
            .map_err(|error| system_error("CoInitializeEx", &error))?;
        Ok(Apartment {
            initialized: true,
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances this thread's successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

/// A kernel object handle, closed on drop.
pub(super) struct Handle(HANDLE);

// SAFETY: a kernel object handle is not tied to the thread that opened it;
// any thread may wait on it, signal it or close it.
unsafe impl Send for Handle {}
// SAFETY: as above; the operations used on a shared handle (waits, SetEvent)
// are thread-safe.
unsafe impl Sync for Handle {}

impl Handle {
    pub(super) fn new(handle: HANDLE) -> Handle {
        Handle(handle)
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: this value owns the handle and nothing uses it after.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// An unnamed event object.
pub(super) struct Event(Handle);

impl Event {
    /// An event that resets itself when a wait returns, as WASAPI's stream
    /// events are.
    pub(super) fn auto_reset() -> Result<Event, BackendError> {
        Event::create(false)
    }

    /// An event that stays set once set: a stop request.
    pub(super) fn manual_reset() -> Result<Event, BackendError> {
        Event::create(true)
    }

    fn create(manual_reset: bool) -> Result<Event, BackendError> {
        // SAFETY: an unnamed event with default security, not set; the
        // handle is owned by the returned value.
        let handle = unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
            .map_err(|error| system_error("CreateEventW", &error))?;
        Ok(Event(Handle(handle)))
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.0.raw()
    }

    pub(super) fn set(&self) {
        // SAFETY: the handle is a live event owned by this value.
        let _ = unsafe { SetEvent(self.raw()) };
    }
}

/// The calling thread's registration with the Multimedia Class Scheduler
/// Service as a "Pro Audio" task, which schedules it at real-time priority.
/// Undone on drop; it holds a raw handle, so it stays on its thread.
pub(super) struct ProAudioTask(HANDLE);

impl ProAudioTask {
    /// Registers the calling thread, or returns `None` when MMCSS refuses
    /// (the thread then runs at normal priority).
    pub(super) fn join() -> Option<ProAudioTask> {
        let mut task_index = 0u32;
        // SAFETY: registers the calling thread under a static task name.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
            .ok()
            .map(ProAudioTask)
    }
}

impl Drop for ProAudioTask {
    fn drop(&mut self) {
        // SAFETY: the registration this thread made in `join`.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// How a starting thread tells the thread that started it that it runs, or
/// why it couldn't start.
pub(super) struct Ready(mpsc::Sender<Result<(), BackendError>>);

impl Ready {
    pub(super) fn ok(self) {
        let _ = self.0.send(Ok(()));
    }

    pub(super) fn fail(self, error: BackendError) {
        let _ = self.0.send(Err(error));
    }
}

/// Runs `body` on a new thread named `name` and waits until it reports
/// through its [`Ready`]. A thread that couldn't start is joined, and its
/// error returned.
pub(super) fn spawn_ready(
    name: &str,
    body: impl FnOnce(Ready) + Send + 'static,
) -> Result<JoinHandle<()>, BackendError> {
    let (sender, receiver) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || body(Ready(sender)))
        .map_err(|error| BackendError::System {
            call: "CreateThread",
            code: error
                .raw_os_error()
                .map_or(E_FAIL.0, |code| HRESULT::from_win32(code as u32).0),
        })?;
    match receiver.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        // The thread ended without a word: it panicked.
        Err(_) => {
            let _ = thread.join();
            Err(BackendError::System {
                call: "audio thread start",
                code: E_FAIL.0,
            })
        }
    }
}

/// A failed call.
pub(super) fn system_error(call: &'static str, error: &windows::core::Error) -> BackendError {
    BackendError::System {
        call,
        code: error.code().0,
    }
}

/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: an endpoint ID that is no longer
/// known.
const NOT_FOUND: HRESULT = HRESULT::from_win32(ERROR_NOT_FOUND.0);

/// A failed call on a device: [`BackendError::DeviceGone`] when the device
/// went away or Windows audio stopped, a failed call otherwise.
pub(super) fn device_error(call: &'static str, error: &windows::core::Error) -> BackendError {
    device_code_error(call, error.code())
}

/// [`device_error`] for a bare HRESULT.
pub(super) fn device_code_error(call: &'static str, code: HRESULT) -> BackendError {
    if code == AUDCLNT_E_DEVICE_INVALIDATED
        || code == AUDCLNT_E_SERVICE_NOT_RUNNING
        || code == NOT_FOUND
    {
        BackendError::DeviceGone
    } else {
        BackendError::System { call, code: code.0 }
    }
}

/// Copies a string that COM allocated, and frees it.
///
/// # Safety
///
/// `text` is null, or a NUL-terminated string from `CoTaskMemAlloc` that
/// nothing uses afterwards.
pub(super) unsafe fn take_com_string(text: PWSTR) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    let copy = unsafe { text.to_string() }.ok();
    // SAFETY: the caller guarantees it came from CoTaskMemAlloc and isn't
    // used again.
    unsafe { CoTaskMemFree(Some(text.0.cast_const().cast::<c_void>())) };
    copy
}

/// Memory COM allocated for the caller, freed on drop.
pub(super) struct ComMemory<T>(*mut T);

impl<T> ComMemory<T> {
    /// Takes ownership of `memory`, which came from `CoTaskMemAlloc`.
    pub(super) fn new(memory: *mut T) -> ComMemory<T> {
        ComMemory(memory)
    }

    pub(super) fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for ComMemory<T> {
    fn drop(&mut self) {
        // SAFETY: the memory came from CoTaskMemAlloc and is used no more.
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast::<c_void>())) };
    }
}

/// The bytes a `WAVEFORMATEXTENSIBLE` has after its `WAVEFORMATEX`, its
/// `cbSize`.
pub(super) const EXTENSIBLE_EXTRA: u16 =
    (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16;

/// A plain `WAVEFORMATEX` for `format`, as a capture asks for.
pub(super) fn wave_format(format: StreamFormat) -> WAVEFORMATEX {
    let block_align = format.block_align() as u16;
    WAVEFORMATEX {
        wFormatTag: match format.sample {
            SampleType::F32 => WAVE_FORMAT_IEEE_FLOAT as u16,
            SampleType::I16 => WAVE_FORMAT_PCM as u16,
        },
        nChannels: format.channels,
        nSamplesPerSec: format.rate,
        nAvgBytesPerSec: format.rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: format.bits(),
        cbSize: 0,
    }
}

/// A 32-bit float `WAVEFORMATEXTENSIBLE` with the rate, channels and channel
/// mask of `format`.
pub(super) fn float_format(format: RenderFormat) -> WAVEFORMATEXTENSIBLE {
    let block_align = format.channels.saturating_mul(4);
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: format.channels,
            nSamplesPerSec: format.rate,
            nAvgBytesPerSec: format.rate * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: EXTENSIBLE_EXTRA,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: format.channel_mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// The mix format at `format`.
///
/// # Safety
///
/// `format` points to a readable `WAVEFORMATEX`, followed by the `cbSize`
/// bytes it announces.
pub(super) unsafe fn read_mix_format(format: *const WAVEFORMATEX) -> MixFormat {
    // SAFETY: the caller guarantees a readable WAVEFORMATEX. The struct is
    // packed, so it is read without assuming alignment.
    let base = unsafe { format.read_unaligned() };
    let tag = u32::from(base.wFormatTag);
    let bits = base.wBitsPerSample;
    let mut float32 = tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32;
    let mut channel_mask = 0;
    if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= EXTENSIBLE_EXTRA {
        // SAFETY: cbSize says the rest of a WAVEFORMATEXTENSIBLE follows.
        let extensible = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        let sub_format = extensible.SubFormat;
        float32 = sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32;
        channel_mask = extensible.dwChannelMask;
    }
    MixFormat {
        float32,
        rate: base.nSamplesPerSec,
        channels: base.nChannels,
        channel_mask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::CAPTURE_ATTEMPTS;

    #[test]
    fn the_extensible_part_of_a_wave_format_is_22_bytes() {
        assert_eq!(size_of::<WAVEFORMATEX>(), 18);
        assert_eq!(EXTENSIBLE_EXTRA, 22);
    }

    #[test]
    fn a_capture_format_counts_its_bytes() {
        let format = wave_format(CAPTURE_ATTEMPTS[0].format);
        assert_eq!({ format.wFormatTag }, 3);
        assert_eq!({ format.nChannels }, 2);
        assert_eq!({ format.nBlockAlign }, 8);
        assert_eq!({ format.nAvgBytesPerSec }, 384_000);
        let format = wave_format(CAPTURE_ATTEMPTS[3].format);
        assert_eq!({ format.wFormatTag }, 1);
        assert_eq!({ format.nBlockAlign }, 4);
        assert_eq!({ format.nAvgBytesPerSec }, 176_400);
    }

    #[test]
    fn a_float_render_format_reads_back_as_float() {
        let format = float_format(RenderFormat {
            rate: 44_100,
            channels: 6,
            channel_mask: 0x3f,
            convert: true,
        });
        // SAFETY: a whole WAVEFORMATEXTENSIBLE on the stack.
        let mix = unsafe { read_mix_format((&raw const format).cast()) };
        assert_eq!(
            mix,
            MixFormat {
                float32: true,
                rate: 44_100,
                channels: 6,
                channel_mask: 0x3f,
            }
        );
    }

    #[test]
    fn a_pcm_mix_format_is_not_float() {
        let format = wave_format(CAPTURE_ATTEMPTS[2].format);
        // SAFETY: a whole WAVEFORMATEX on the stack, with cbSize 0.
        let mix = unsafe { read_mix_format(&raw const format) };
        assert!(!mix.float32);
        assert_eq!(mix.channel_mask, 0);
    }

    #[test]
    fn a_device_that_went_away_is_gone() {
        assert_eq!(
            device_code_error("x", AUDCLNT_E_DEVICE_INVALIDATED),
            BackendError::DeviceGone
        );
        assert_eq!(device_code_error("x", NOT_FOUND), BackendError::DeviceGone);
        assert_eq!(
            device_code_error("x", E_FAIL),
            BackendError::System {
                call: "x",
                code: E_FAIL.0,
            }
        );
    }
}
