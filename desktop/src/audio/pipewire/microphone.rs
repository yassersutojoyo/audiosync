//! The calibration's recording of the default input, mono at the device's
//! own rate, on its own thread like the other streams. The recording is
//! owned by that thread and handed back when it stops; the controller
//! reads its progress through atomics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use audiosync_core::clock;
use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamRc, StreamState};

use super::timing;
use crate::audio::calibration::{Microphone, MicrophoneError, MicrophoneId, RecordingClock};

const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The rate the buffer is first sized for when the device's isn't known.
const ASSUMED_RATE: u32 = 48_000;

const NO_GAP: usize = usize::MAX;

#[derive(Default)]
struct Status {
    rate: AtomicU32,
    frames: AtomicUsize,
    /// The first frame's time on the core's clock, 0 while unknown.
    first_frame_ns: AtomicU64,
    gap_at: AtomicUsize,
    stopped: AtomicBool,
}

/// A running recording of the default input.
pub struct PipeWireMicrophone {
    device: String,
    status: Arc<Status>,
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<Vec<f32>>>,
    taken: Option<Vec<f32>>,
}

impl PipeWireMicrophone {
    fn finish(&mut self) -> &[f32] {
        if self.taken.is_none() {
            let _ = self.stop.send(());
            let recorded = self
                .thread
                .take()
                .and_then(|thread| thread.join().ok())
                .unwrap_or_default();
            self.taken = Some(recorded);
        }
        self.taken.as_deref().unwrap_or_default()
    }
}

impl Drop for PipeWireMicrophone {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Microphone for PipeWireMicrophone {
    fn id(&self) -> MicrophoneId {
        MicrophoneId {
            device: self.device.clone(),
            rate: self.status.rate.load(Ordering::Acquire),
        }
    }

    fn first_frame_ns(&self) -> Option<u64> {
        Some(self.status.first_frame_ns.load(Ordering::Acquire)).filter(|&ns| ns > 0)
    }

    fn frames_recorded(&self) -> usize {
        self.status.frames.load(Ordering::Acquire)
    }

    fn gap_at(&self) -> Option<usize> {
        Some(self.status.gap_at.load(Ordering::Acquire)).filter(|&at| at != NO_GAP)
    }

    fn stopped(&self) -> bool {
        self.status.stopped.load(Ordering::Acquire)
    }

    fn take(&mut self, start: usize, count: usize) -> Vec<f32> {
        let recorded = self.finish();
        let start = start.min(recorded.len());
        let end = start.saturating_add(count).min(recorded.len());
        recorded[start..end].to_vec()
    }
}

/// What the recording callback owns.
struct Recording {
    samples: Vec<f32>,
    capacity: usize,
    seconds: f64,
    clock: RecordingClock,
    status: Arc<Status>,
    /// The stream position the next buffer should start at, in its ticks.
    next_ticks: Option<u64>,
}

impl Recording {
    fn buffer(&mut self, stream: &pw::stream::Stream) {
        let rate = self.status.rate.load(Ordering::Relaxed);
        let time = stream.time().ok();
        while let Some(mut buffer) = stream.dequeue_buffer() {
            let Some(data) = buffer.datas_mut().first_mut() else {
                continue;
            };
            let chunk = data.chunk();
            let offset = chunk.offset() as usize;
            let size = chunk.size() as usize;
            let Some(bytes) = data
                .data()
                .and_then(|bytes| bytes.get(offset..offset + size))
            else {
                continue;
            };
            let frames = bytes.len() / 4;
            let before = self.samples.len();
            let now_ns = clock::now_ns();
            let (age_ns, discontinuity) = match &time {
                Some(time) => self.age(time, rate, frames),
                None => (None, false),
            };
            self.clock
                .packet(before, rate, now_ns, age_ns, false, discontinuity);
            let room = self.capacity.saturating_sub(before);
            for sample in bytes.as_chunks::<4>().0.iter().take(room) {
                self.samples.push(f32::from_le_bytes(*sample));
            }
            self.status
                .frames
                .store(self.samples.len(), Ordering::Release);
            if let Some(first) = self.clock.first_frame_ns {
                self.status
                    .first_frame_ns
                    .store(first.max(1), Ordering::Release);
            }
            if let Some(gap) = self.clock.gap_at {
                self.status.gap_at.store(gap, Ordering::Release);
            }
        }
    }

    /// How long before now the buffer's first frame was recorded, and
    /// whether the stream skipped ahead of the frames received.
    fn age(&mut self, time: &pw::stream::Time, rate: u32, frames: usize) -> (Option<u64>, bool) {
        let fraction = time.rate();
        if fraction.denom == 0 || rate == 0 {
            return (None, false);
        }
        let ticks_ns = |ticks: u64| {
            u64::try_from(
                u128::from(ticks) * 1_000_000_000 * u128::from(fraction.num)
                    / u128::from(fraction.denom),
            )
            .unwrap_or(u64::MAX)
        };
        let monotonic_ns = timing::monotonic_hns().saturating_mul(100);
        let since = monotonic_ns.saturating_sub(u64::try_from(time.now()).unwrap_or(0));
        let delay = ticks_ns(u64::try_from(time.delay()).unwrap_or(0));
        let buffered = time.buffered() * 1_000_000_000 / u64::from(rate);
        let frames_ns = frames as u64 * 1_000_000_000 / u64::from(rate);
        let ticks = time.ticks();
        let discontinuity = self
            .next_ticks
            .is_some_and(|expected| ticks > expected && ticks_ns(ticks - expected) > frames_ns);
        let ticks_per_buffer = u128::from(frames as u64) * u128::from(fraction.denom)
            / (u128::from(rate) * u128::from(fraction.num.max(1)));
        self.next_ticks = Some(ticks + u64::try_from(ticks_per_buffer).unwrap_or(0));
        (Some(since + delay + buffered), discontinuity)
    }

    /// Makes room for the recording at the rate PipeWire agreed on.
    fn agreed(&mut self, rate: u32) {
        self.status.rate.store(rate, Ordering::Release);
        let capacity = (self.seconds * f64::from(rate)).ceil() as usize;
        if capacity > self.capacity {
            self.samples.reserve(capacity - self.samples.len());
            self.capacity = capacity;
        }
    }
}

/// The `EnumFormat` param for float mono at whatever rate the device runs.
fn enum_format() -> Option<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_channels(1);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let object = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

/// Starts recording the default input named `device`, which runs at
/// `rate` when known, with room for `seconds`.
pub fn open(
    device: String,
    rate: Option<u32>,
    seconds: f64,
) -> Result<PipeWireMicrophone, MicrophoneError> {
    let status = Arc::new(Status {
        gap_at: AtomicUsize::new(NO_GAP),
        ..Status::default()
    });
    let capacity = (seconds.max(0.0) * f64::from(rate.unwrap_or(ASSUMED_RATE))).ceil() as usize;
    let recording = Recording {
        samples: Vec::with_capacity(capacity),
        capacity,
        seconds: seconds.max(0.0),
        clock: RecordingClock::default(),
        status: Arc::clone(&status),
        next_ticks: None,
    };
    let (stop, stop_receiver) = pw::channel::channel();
    let (ready, ready_receiver) = mpsc::channel();
    let thread_status = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("audiosync-pw-mic".to_owned())
        .spawn(move || {
            let recorded = serve(recording, stop_receiver, ready.clone(), &thread_status);
            thread_status.stopped.store(true, Ordering::Release);
            match recorded {
                Ok(samples) => samples,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    Vec::new()
                }
            }
        })
        .map_err(|_| MicrophoneError::Missing)?;
    let microphone = PipeWireMicrophone {
        device,
        status,
        stop,
        thread: Some(thread),
        taken: None,
    };
    match ready_receiver.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(microphone),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(MicrophoneError::Missing),
    }
}

fn serve(
    recording: Recording,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<(), MicrophoneError>>,
    status: &Arc<Status>,
) -> Result<Vec<f32>, MicrophoneError> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(|_| MicrophoneError::Missing)?;
    let context =
        pw::context::ContextRc::new(&main_loop, None).map_err(|_| MicrophoneError::Missing)?;
    let core = context
        .connect_rc(None)
        .map_err(|_| MicrophoneError::Missing)?;
    let _core_listener = core
        .add_listener_local()
        .error({
            let main_loop = main_loop.clone();
            let ready = ready.clone();
            move |id, _seq, res, _message| {
                if id == pw::core::PW_ID_CORE || res == -libc::EPERM {
                    let error = if res == -libc::EPERM {
                        MicrophoneError::Denied
                    } else {
                        MicrophoneError::Missing
                    };
                    let _ = ready.send(Err(error));
                    main_loop.quit();
                }
            }
        })
        .register();
    let properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_NAME => "audiosync-microphone",
        *pw::keys::APP_NAME => "AudioSync",
    };
    let stream = StreamRc::new(core.clone(), "audiosync-microphone", properties)
        .map_err(|_| MicrophoneError::Missing)?;
    let recording = std::rc::Rc::new(std::cell::RefCell::new(recording));
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |_stream, _, old, new| match new {
                StreamState::Streaming if status.rate.load(Ordering::Acquire) > 0 => {
                    let _ = ready.send(Ok(()));
                }
                StreamState::Error(_) => {
                    let _ = ready.send(Err(MicrophoneError::Missing));
                    main_loop.quit();
                }
                StreamState::Unconnected
                    if matches!(old, StreamState::Paused | StreamState::Streaming) =>
                {
                    main_loop.quit();
                }
                _ => {}
            }
        })
        .param_changed({
            let recording = std::rc::Rc::clone(&recording);
            let ready = ready.clone();
            move |_stream, _, id, param| {
                let Some(param) = param else { return };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_err() || info.rate() == 0 {
                    return;
                }
                if let Ok(mut recording) = recording.try_borrow_mut() {
                    recording.agreed(info.rate());
                }
                let _ = ready.send(Ok(()));
            }
        })
        .process({
            let recording = std::rc::Rc::clone(&recording);
            move |stream, _| {
                if let Ok(mut recording) = recording.try_borrow_mut() {
                    recording.buffer(stream);
                }
            }
        })
        .register()
        .map_err(|_| MicrophoneError::Missing)?;
    let format = enum_format().ok_or(MicrophoneError::Missing)?;
    let format = Pod::from_bytes(&format).ok_or(MicrophoneError::Missing)?;
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|_| MicrophoneError::Missing)?;
    let _stop = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |()| main_loop.quit()
    });
    main_loop.run();
    let _ = stream.disconnect();
    drop(listener);
    let samples = std::mem::take(&mut recording.borrow_mut().samples);
    Ok(samples)
}
