//! The capture stream on the default output's monitor, on its own thread.
//!
//! Every PipeWire object of a capture (main loop, context, connection,
//! stream and listeners) is made, run and dropped on one thread,
//! `audiosync-pw`: they can't cross threads. The controller reaches it only
//! through a PipeWire channel, which stops it, and atomics, which tell how
//! it is doing.
//!
//! The stream is connected without `RT_PROCESS`, so its buffers come to the
//! main loop on that thread, where a timer also keeps the ring in real time
//! while the output is suspended and nothing comes. The converter and its
//! [`GapFill`] are then only ever used on one thread, without a lock; the
//! callback never allocates, locks, blocks or logs.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use audiosync_core::eq::EqProcessor;
use audiosync_core::ring::RingWriter;
use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamBox, StreamFlags, StreamState};

use super::format::{self, CAPTURE_FORMAT, Negotiated};
use super::timing::{self, PacketClock};
use crate::audio::backend::BackendError;
use crate::audio::capture::{CaptureAttempt, CaptureConverter, Feeds, GapFill};
use crate::audio::controller::{CaptureHandle, CaptureState};

/// How long a capture may take to connect before it counts as
/// [`BackendError::NotResponding`].
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the timer looks for a gap while no buffer comes.
const IDLE_INTERVAL: Duration = Duration::from_millis(10);

/// The quantum asked for: PipeWire's default, so other apps' latency stays
/// as it was.
const NODE_LATENCY: &str = "1024/48000";

/// The node's name, for the logs and for telling AudioSync's own streams
/// apart from the apps'.
const NODE_NAME: &str = "audiosync-capture";

/// No failure yet.
const RUNNING: u8 = 0;
const DEVICE_GONE: u8 = 1;
const UNSUPPORTED_FORMAT: u8 = 2;
const SERVICE_NOT_RUNNING: u8 = 3;

/// How the capture thread is doing, as the controller reads it.
#[derive(Default)]
struct Status {
    /// The first failure, as one of the codes above.
    failed: AtomicU8,
    /// Whether PipeWire agreed to [`CAPTURE_FORMAT`].
    agreed: AtomicBool,
}

impl Status {
    /// Records `error` unless an earlier failure is recorded.
    fn fail(&self, error: &BackendError) {
        let code = match error {
            BackendError::UnsupportedFormat => UNSUPPORTED_FORMAT,
            BackendError::ServiceNotRunning => SERVICE_NOT_RUNNING,
            _ => DEVICE_GONE,
        };
        let _ = self
            .failed
            .compare_exchange(RUNNING, code, Ordering::AcqRel, Ordering::Acquire);
    }

    fn state(&self) -> CaptureState {
        match self.failed.load(Ordering::Acquire) {
            RUNNING => CaptureState::Running,
            UNSUPPORTED_FORMAT => CaptureState::Failed(BackendError::UnsupportedFormat),
            SERVICE_NOT_RUNNING => CaptureState::Failed(BackendError::ServiceNotRunning),
            _ => CaptureState::Failed(BackendError::DeviceGone),
        }
    }
}

/// A running capture of the default output's monitor. Dropping it stops the
/// stream and waits for its thread.
pub struct MonitorCapture {
    status: Arc<Status>,
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureHandle for MonitorCapture {
    fn state(&self) -> CaptureState {
        self.status.state()
    }

    fn setup(&self) -> Option<CaptureAttempt> {
        self.status
            .agreed
            .load(Ordering::Acquire)
            .then_some(format::SETUP)
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        // Fails only when the thread already left its loop.
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts capturing the default output's monitor into `writer` through
/// `equalizer`, handing the same audio to `feeds`. Returns once PipeWire
/// agreed on the format and the stream is linked, or with why it couldn't.
pub fn start(
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
) -> Result<MonitorCapture, BackendError> {
    let converter = CaptureConverter::new(CAPTURE_FORMAT, writer, equalizer, feeds)?;
    let status = Arc::new(Status::default());
    let (stop, stop_receiver) = pw::channel::channel();
    let (ready, ready_receiver) = mpsc::channel();
    let thread_status = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("audiosync-pw".to_owned())
        .spawn(move || {
            if let Err(error) = serve(converter, stop_receiver, ready.clone(), &thread_status) {
                thread_status.fail(&error);
                let _ = ready.send(Err(error));
            }
        })
        .map_err(|error| BackendError::System {
            call: "std::thread::spawn",
            code: error.raw_os_error().unwrap_or(0),
        })?;
    let capture = MonitorCapture {
        status,
        stop,
        thread: Some(thread),
    };
    match ready_receiver.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(capture),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(BackendError::NotResponding),
        Err(mpsc::RecvTimeoutError::Disconnected) => match capture.status.state() {
            CaptureState::Failed(error) => Err(error),
            _ => Err(BackendError::DeviceGone),
        },
    }
}

/// Whether a PipeWire daemon answers: a connection made and dropped on this
/// thread.
pub fn probe() -> Result<(), BackendError> {
    pw::init();
    let main_loop =
        pw::main_loop::MainLoopBox::new(None).map_err(|_| BackendError::ServiceNotRunning)?;
    let context = pw::context::ContextBox::new(main_loop.loop_(), None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    context
        .connect(None)
        .map(drop)
        .map_err(|_| BackendError::ServiceNotRunning)
}

/// The capture's audio path, from PipeWire's buffers to the converter.
struct Feed {
    converter: CaptureConverter,
    gaps: GapFill,
    clock: PacketClock,
}

impl Feed {
    /// A buffer of `frames` frames from the cycle at `cycle_hns`: `bytes`
    /// in [`CAPTURE_FORMAT`], or `None` when it wasn't mapped (then silence).
    fn buffer(&mut self, cycle_hns: u64, bytes: Option<&[u8]>, frames: usize) {
        let start = self.clock.place(cycle_hns, frames);
        let (fill, skip) = self.gaps.packet(start, frames, false);
        if fill > 0 {
            self.converter.push(&[], fill, true);
        }
        let skip = skip.min(frames);
        if skip == frames {
            return;
        }
        match bytes {
            Some(bytes) => {
                let from = (skip * CAPTURE_FORMAT.block_align()).min(bytes.len());
                self.converter.push(&bytes[from..], frames - skip, false);
            }
            None => self.converter.push(&[], frames - skip, true),
        }
    }

    /// No buffer came, and it is `now_hns`: silence while the output is
    /// suspended.
    fn idle(&mut self, now_hns: u64) {
        let fill = self.gaps.idle(self.clock.idle_now(now_hns));
        if fill > 0 {
            self.converter.push(&[], fill, true);
        }
    }
}

/// The format PipeWire settled on, from its `Format` param.
fn negotiated(param: &Pod) -> Negotiated {
    let raw_audio = format_utils::parse_format(param)
        .is_ok_and(|(kind, subtype)| kind == MediaType::Audio && subtype == MediaSubtype::Raw);
    let mut info = AudioInfoRaw::new();
    if !raw_audio || info.parse(param).is_err() {
        return Negotiated {
            raw_audio,
            f32_interleaved: false,
            rate: 0,
            channels: 0,
        };
    }
    Negotiated {
        raw_audio,
        f32_interleaved: info.format() == AudioFormat::F32LE,
        rate: info.rate(),
        channels: info.channels(),
    }
}

/// The `EnumFormat` param that asks for [`CAPTURE_FORMAT`], front left and
/// right, as a serialized pod.
fn enum_format() -> Result<Vec<u8>, BackendError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(CAPTURE_FORMAT.rate);
    info.set_channels(u32::from(CAPTURE_FORMAT.channels));
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| BackendError::UnsupportedFormat)
}

/// Runs the capture on this thread until the stop message comes or the
/// stream fails. Reports through `ready` once the stream is linked, or why
/// it couldn't be; an `Err` returned is a failure before the loop ran.
fn serve(
    converter: CaptureConverter,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<(), BackendError>>,
    status: &Arc<Status>,
) -> Result<(), BackendError> {
    pw::init();
    // Without a PipeWire daemon there is no connection to make.
    let main_loop =
        pw::main_loop::MainLoopRc::new(None).map_err(|_| BackendError::ServiceNotRunning)?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let core = context
        .connect_rc(None)
        .map_err(|_| BackendError::ServiceNotRunning)?;

    // An error on the connection itself means the daemon went away.
    let _core_listener = core
        .add_listener_local()
        .error({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |id, _seq, _res, _message| {
                if id == pw::core::PW_ID_CORE {
                    status.fail(&BackendError::DeviceGone);
                    let _ = ready.send(Err(BackendError::DeviceGone));
                    main_loop.quit();
                }
            }
        })
        .register();

    let properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        // What the default output plays, not what an input records.
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::NODE_NAME => NODE_NAME,
        *pw::keys::APP_NAME => "AudioSync",
        *pw::keys::NODE_LATENCY => NODE_LATENCY,
        // The output may still suspend while nothing plays; the timer fills.
        *pw::keys::NODE_PASSIVE => "true",
    };
    let stream =
        StreamBox::new(&core, NODE_NAME, properties).map_err(|_| BackendError::DeviceGone)?;

    let feed = Rc::new(RefCell::new(Feed {
        converter,
        gaps: GapFill::started(CAPTURE_FORMAT.rate, timing::monotonic_hns()),
        clock: PacketClock::new(CAPTURE_FORMAT.rate),
    }));

    let process_feed = Rc::clone(&feed);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |_stream, _, old, new| match new {
                StreamState::Paused | StreamState::Streaming => {
                    let _ = ready.send(Ok(()));
                }
                StreamState::Error(_) => {
                    status.fail(&BackendError::DeviceGone);
                    let _ = ready.send(Err(BackendError::DeviceGone));
                    main_loop.quit();
                }
                StreamState::Unconnected
                    if matches!(old, StreamState::Paused | StreamState::Streaming) =>
                {
                    status.fail(&BackendError::DeviceGone);
                    main_loop.quit();
                }
                _ => {}
            }
        })
        .param_changed({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |_stream, _, id, param| {
                let Some(param) = param else { return };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                match format::check(negotiated(param)) {
                    Ok(()) => status.agreed.store(true, Ordering::Release),
                    Err(error) => {
                        status.fail(&error);
                        let _ = ready.send(Err(error));
                        main_loop.quit();
                    }
                }
            }
        })
        .process(move |stream, _| {
            let Ok(mut feed) = process_feed.try_borrow_mut() else {
                return;
            };
            let cycle = stream
                .time()
                .ok()
                .and_then(|time| u64::try_from(time.now()).ok())
                .filter(|&now| now > 0)
                .map_or_else(timing::monotonic_hns, |now| now / 100);
            while let Some(mut buffer) = stream.dequeue_buffer() {
                let Some(data) = buffer.datas_mut().first_mut() else {
                    continue;
                };
                let chunk = data.chunk();
                let offset = chunk.offset() as usize;
                let frames = format::frames_in(chunk.size(), chunk.stride());
                if frames == 0 {
                    continue;
                }
                let length = frames * CAPTURE_FORMAT.block_align();
                let bytes = data
                    .data()
                    .and_then(|mapped| mapped.get(offset..offset + length));
                feed.buffer(cycle, bytes, frames);
            }
        })
        .register()
        .map_err(|_| BackendError::DeviceGone)?;

    let format = enum_format()?;
    let format = Pod::from_bytes(&format).ok_or(BackendError::UnsupportedFormat)?;
    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|_| BackendError::DeviceGone)?;

    let idle_feed = Rc::clone(&feed);
    let timer = main_loop.loop_().add_timer(move |_| {
        if let Ok(mut feed) = idle_feed.try_borrow_mut() {
            feed.idle(timing::monotonic_hns());
        }
    });
    let _ = timer.update_timer(Some(IDLE_INTERVAL), Some(IDLE_INTERVAL));

    let _stop = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |()| main_loop.quit()
    });
    main_loop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use audiosync_core::eq::equalizer;
    use audiosync_core::ring::Ring;

    use super::*;
    use crate::audio::SESSION_RATE;

    #[test]
    fn the_first_failure_is_the_one_reported() {
        let status = Status::default();
        assert_eq!(status.state(), CaptureState::Running);
        status.fail(&BackendError::UnsupportedFormat);
        status.fail(&BackendError::DeviceGone);
        assert_eq!(
            status.state(),
            CaptureState::Failed(BackendError::UnsupportedFormat)
        );
    }

    #[test]
    fn the_asked_format_serializes() {
        let bytes = enum_format().unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        assert_eq!(
            negotiated(pod),
            Negotiated {
                raw_audio: true,
                f32_interleaved: true,
                rate: SESSION_RATE,
                channels: 2,
            }
        );
    }

    /// Needs a PipeWire daemon whose default output plays a tone (the Linux
    /// CI job starts one on a null sink): `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a running PipeWire daemon with a tone on the default output"]
    fn the_monitor_of_a_playing_output_is_captured() {
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let writer = ring.writer().unwrap();
        let (_control, processor) = equalizer();
        let capture = start(writer, processor, Feeds::default()).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(capture.state(), CaptureState::Running);
        assert_eq!(capture.setup(), Some(format::SETUP));
        // Two seconds at 48 kHz, give or take the start.
        let frames = ring.frames_written();
        assert!(frames > 48_000, "{frames} frames written");
        let peak = ring.take_peak();
        assert!(peak > 0.01, "peak {peak}");
    }
}
