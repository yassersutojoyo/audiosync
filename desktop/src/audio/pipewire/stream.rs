//! The capture stream on the default output's monitor, on its own thread.
//! A [`Wiring`] says what the stream links to; for the monitor it watches
//! the `default` metadata and moves the stream to the new default output.
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
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use audiosync_core::eq::EqProcessor;
use audiosync_core::ring::RingWriter;
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamRc, StreamState};
use pw::types::ObjectType;

use super::format::{self, CAPTURE_FORMAT, Negotiated};
use super::graph::{self, Filter, Graph};
use super::procfs;
use super::registry;
use super::timing::{self, PacketClock};
use crate::audio::backend::BackendError;
use crate::audio::capture::{CaptureAttempt, CaptureConverter, Feeds, GapFill};
use crate::audio::capture::{CaptureTarget, LoopbackMode};
use crate::audio::controller::{CaptureHandle, CaptureState};
use crate::audio::process::ProcessTable;

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
const SOURCE_QUIT: u8 = 4;

/// How often a linked capture looks for streams to link.
const RELINK_INTERVAL: Duration = Duration::from_millis(100);

/// How many relink turns pass between checks that the captured apps run.
const QUIT_CHECK_TURNS: u32 = 10;

/// How the capture thread is doing, as the controller reads it.
#[derive(Default)]
struct Status {
    /// The first failure, as one of the codes above.
    failed: AtomicU8,
    /// Whether PipeWire agreed to [`CAPTURE_FORMAT`].
    agreed: AtomicBool,
    /// Whether a linked capture failed to make a link it wants.
    missing_links: AtomicBool,
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

    /// Records that every app the capture follows quit.
    fn quit(&self) {
        let _ =
            self.failed
                .compare_exchange(RUNNING, SOURCE_QUIT, Ordering::AcqRel, Ordering::Acquire);
    }

    fn state(&self) -> CaptureState {
        match self.failed.load(Ordering::Acquire) {
            RUNNING => CaptureState::Running,
            SOURCE_QUIT => CaptureState::SourceQuit,
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
    stop: pw::channel::Sender<Message>,
    /// Whether it links apps' streams, so it can move to other apps.
    linked: bool,
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

    fn retarget(&mut self, target: &CaptureTarget) -> bool {
        let Some(filter) = filter_of(target, 0) else {
            return false;
        };
        if !self.linked || matches!(filter, Filter::Except(_)) {
            return false;
        }
        self.stop.send(Message::Retarget(filter)).is_ok()
    }

    fn holds_all(&self) -> bool {
        !self.status.missing_links.load(Ordering::Acquire)
    }
}

/// What the controller tells a capture's thread.
pub enum Message {
    Stop,
    /// Link the streams of these apps instead.
    Retarget(Filter),
}

/// The streams a capture of `target` links: every app but `own_pid`'s tree
/// (or the activation's target, for 0) for the whole system, else the
/// apps' trees. `None` for an input device.
pub fn filter_of(target: &CaptureTarget, own_pid: u32) -> Option<Filter> {
    match target {
        CaptureTarget::Loopback(activation) => Some(match activation.mode {
            LoopbackMode::ExcludeTree => Filter::Except(if own_pid == 0 {
                activation.target_pid
            } else {
                own_pid
            }),
            LoopbackMode::IncludeTree => Filter::Trees(vec![activation.target_pid]),
        }),
        CaptureTarget::Mix(activations) => Some(Filter::Trees(
            activations
                .iter()
                .map(|activation| activation.target_pid)
                .collect(),
        )),
        CaptureTarget::Input { .. } => None,
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        // Fails only when the thread already left its loop.
        let _ = self.stop.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What the capture stream is linked to. Only this decides how the stream
/// connects and what it follows; from the process callback on, the path of
/// the buffers is the same for every wiring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wiring {
    /// The default output's monitor, moving to the new default when it
    /// changes.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the whole system is captured through links now")
    )]
    DefaultOutputMonitor,
    /// The apps' streams a [`Filter`] picks, linked to the stream one by one
    /// as they appear; the session manager links nothing to it.
    Links,
    /// An input device, by the `target.object` in the [`Plan`].
    Input,
}

/// What a capture links to: the wiring, the apps a linked one takes, and
/// the device an input one records.
pub struct Plan {
    pub wiring: Wiring,
    pub filter: Option<Filter>,
    pub serial: Option<String>,
}

impl Wiring {
    fn flags(self) -> StreamFlags {
        match self {
            Wiring::DefaultOutputMonitor | Wiring::Input => {
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS
            }
            Wiring::Links => StreamFlags::MAP_BUFFERS,
        }
    }

    /// The node to connect to; `None` leaves it to the session manager.
    fn target(self) -> Option<u32> {
        match self {
            Wiring::DefaultOutputMonitor | Wiring::Links | Wiring::Input => None,
        }
    }

    /// Whether the stream records what an output plays rather than an input.
    fn captures_output(self) -> bool {
        matches!(self, Wiring::DefaultOutputMonitor)
    }

    /// Whether the stream is moved when the default output changes: the
    /// session manager leaves an already linked stream where it is.
    fn follows_default(self) -> bool {
        matches!(self, Wiring::DefaultOutputMonitor)
    }
}

/// The metadata key that names the default output in effect.
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

/// The metadata key, on a stream's node, that asks the session manager to
/// link the stream to the node whose `object.serial` it holds.
const TARGET_KEY: &str = "target.object";

/// The node name in a [`DEFAULT_SINK_KEY`] value, `{ "name": "..." }`.
fn sink_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value.get("name")?.as_str().map(str::to_owned)
}

/// The default output as the `default` metadata last named it.
#[derive(Default)]
struct DefaultOutput {
    name: Option<String>,
}

impl DefaultOutput {
    /// Records `value` and tells whether the stream should move to it: each
    /// default that differs from the last, the first one too, since the
    /// session manager may have restored a target from an earlier run.
    fn changed(&mut self, value: Option<&str>) -> bool {
        if self.name.as_deref() == value {
            return false;
        }
        self.name = value.map(str::to_owned);
        value.is_some()
    }
}

/// Starts capturing what `wiring` links to into `writer` through
/// `equalizer`, handing the same audio to `feeds`. Returns once PipeWire
/// agreed on the format and the stream is linked, or with why it couldn't.
#[cfg_attr(not(test), expect(dead_code, reason = "the monitor wiring is tested"))]
pub fn start(
    wiring: Wiring,
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
) -> Result<MonitorCapture, BackendError> {
    let plan = Plan {
        wiring,
        filter: None,
        serial: None,
    };
    start_plan(plan, writer, equalizer, feeds)
}

/// [`start`] for any [`Plan`].
pub fn start_plan(
    plan: Plan,
    writer: RingWriter,
    equalizer: EqProcessor,
    feeds: Feeds,
) -> Result<MonitorCapture, BackendError> {
    let linked = plan.filter.is_some();
    let converter = CaptureConverter::new(CAPTURE_FORMAT, writer, equalizer, feeds)?;
    let status = Arc::new(Status::default());
    let (stop, stop_receiver) = pw::channel::channel();
    let (ready, ready_receiver) = mpsc::channel();
    let thread_status = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("audiosync-pw".to_owned())
        .spawn(move || {
            let served = serve(
                plan,
                converter,
                stop_receiver,
                ready.clone(),
                &thread_status,
            );
            if let Err(error) = served {
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
        linked,
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
    plan: Plan,
    converter: CaptureConverter,
    stop: pw::channel::Receiver<Message>,
    ready: mpsc::Sender<Result<(), BackendError>>,
    status: &Arc<Status>,
) -> Result<(), BackendError> {
    let wiring = plan.wiring;
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
        *pw::keys::STREAM_CAPTURE_SINK => if wiring.captures_output() { "true" } else { "false" },
        *pw::keys::NODE_NAME => NODE_NAME,
        *pw::keys::APP_NAME => "AudioSync",
        *pw::keys::NODE_LATENCY => NODE_LATENCY,
        // The output may still suspend while nothing plays; the timer fills.
        *pw::keys::NODE_PASSIVE => "true",
    };
    let mut properties = properties;
    if !matches!(wiring, Wiring::DefaultOutputMonitor) {
        properties.insert("node.dont-reconnect", "true");
    }
    if let Some(serial) = &plan.serial {
        properties.insert(TARGET_KEY, serial.as_str());
        properties.insert("node.dont-fallback", "true");
    }
    let stream =
        StreamRc::new(core.clone(), NODE_NAME, properties).map_err(|_| BackendError::DeviceGone)?;

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
            let mut placed = false;
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
                placed = true;
            }
            if placed {
                feed.clock.settle(cycle);
            }
        })
        .register()
        .map_err(|_| BackendError::DeviceGone)?;

    connect(&stream, wiring, &enum_format()?)?;

    // The `default` metadata and its listener, once the registry announces it.
    let defaults: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::default();
    let registry = core
        .get_registry_rc()
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let registry_listener = wiring.follows_default().then(|| {
        registry
            .add_listener_local()
            .global({
                let registry = registry.clone();
                let defaults = Rc::clone(&defaults);
                let stream = stream.clone();
                // Each output's `object.serial` by its node name.
                let serials: Rc<RefCell<HashMap<String, String>>> = Rc::default();
                move |global| {
                    let Some(props) = global.props else { return };
                    if global.type_ == ObjectType::Node {
                        if let (Some(name), Some(serial)) =
                            (props.get("node.name"), props.get("object.serial"))
                        {
                            serials
                                .borrow_mut()
                                .insert(name.to_owned(), serial.to_owned());
                        }
                        return;
                    }
                    let is_default = global.type_ == ObjectType::Metadata
                        && props.get("metadata.name") == Some("default");
                    if !is_default || defaults.borrow().is_some() {
                        return;
                    }
                    let Ok(metadata) = registry.bind::<Metadata, _>(global) else {
                        return;
                    };
                    let output = RefCell::new(DefaultOutput::default());
                    let stream = stream.clone();
                    let serials = Rc::clone(&serials);
                    // Weak: the listener lives in `defaults` itself.
                    let defaults_weak = Rc::downgrade(&defaults);
                    let listener = metadata
                        .add_listener_local()
                        .property(move |subject, key, _type, value| {
                            if subject != pw::core::PW_ID_CORE
                                || key != Some(DEFAULT_SINK_KEY)
                                || !output.borrow_mut().changed(value)
                            {
                                return 0;
                            }
                            let Some(name) = value.and_then(sink_name) else {
                                return 0;
                            };
                            let Some(serial) = serials.borrow().get(&name).cloned() else {
                                return 0;
                            };
                            let Some(defaults) = defaults_weak.upgrade() else {
                                return 0;
                            };
                            // Moving the linked node keeps it and its format;
                            // the session manager relinks it to the new output.
                            if let Some((metadata, _)) = defaults.borrow().as_ref() {
                                metadata.set_property(
                                    stream.node_id(),
                                    TARGET_KEY,
                                    None,
                                    Some(serial.as_str()),
                                );
                            }
                            0
                        })
                        .register();
                    *defaults.borrow_mut() = Some((metadata, listener));
                }
            })
            .register()
    });

    let linker = plan
        .filter
        .map(|filter| Linker::follow(&registry, &core, &stream, filter, status));
    let _relink_timer = linker.as_ref().map(|following| {
        let linker = Rc::clone(&following.linker);
        let timer = main_loop.loop_().add_timer(move |_| {
            if let Ok(mut linker) = linker.try_borrow_mut() {
                linker.tick();
            }
        });
        let _ = timer.update_timer(Some(RELINK_INTERVAL), Some(RELINK_INTERVAL));
        timer
    });

    let idle_feed = Rc::clone(&feed);
    let timer = main_loop.loop_().add_timer(move |_| {
        if let Ok(mut feed) = idle_feed.try_borrow_mut() {
            feed.idle(timing::monotonic_hns());
        }
    });
    let _ = timer.update_timer(Some(IDLE_INTERVAL), Some(IDLE_INTERVAL));

    let _stop = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        let linker = linker
            .as_ref()
            .map(|following| Rc::clone(&following.linker));
        move |message| match message {
            Message::Stop => main_loop.quit(),
            Message::Retarget(filter) => {
                if let Some(linker) = &linker {
                    linker.borrow_mut().retarget(filter);
                }
            }
        }
    });
    main_loop.run();
    // PipeWire reuses node IDs: the target set for this node must not pin
    // the next one that gets its ID. A few turns of the loop send it.
    if let Some((metadata, _)) = defaults.borrow().as_ref() {
        metadata.set_property(stream.node_id(), TARGET_KEY, None, None);
        for _ in 0..5 {
            main_loop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
        }
    }
    // The metadata's listener holds the stream; it goes first.
    drop(registry_listener);
    drop(linker);
    defaults.borrow_mut().take();
    let _ = stream.disconnect();
    Ok(())
}

/// The links of a linked capture: a graph of its own from the registry,
/// the links it made, and, for a capture of apps, the apps it follows.
struct Linker {
    core: pw::core::CoreRc,
    stream: StreamRc,
    filter: Filter,
    graph: Graph,
    /// The links made, each kept by its proxy.
    links: BTreeMap<graph::Link, pw::link::Link>,
    /// The apps' top processes and their start times.
    roots: Vec<(u32, Option<u64>)>,
    status: Arc<Status>,
    /// Whether the graph changed since the last relink.
    dirty: bool,
    turns: u32,
}

/// A linker and the registry listener that feeds it.
struct Following {
    linker: Rc<RefCell<Linker>>,
    _listener: pw::registry::Listener,
    _clients: Rc<RefCell<HashMap<u32, registry::FollowedClient>>>,
}

/// Each root of `filter` with its start time, as `/proc` has it now.
fn roots_of(filter: &Filter) -> Vec<(u32, Option<u64>)> {
    let Filter::Trees(roots) = filter else {
        return Vec::new();
    };
    let processes = procfs::snapshot().unwrap_or_default();
    roots
        .iter()
        .map(|&pid| (pid, processes.get(pid).and_then(|entry| entry.created)))
        .collect()
}

/// Whether none of `roots` still runs: each is gone, or its PID now
/// belongs to a process started at another time.
fn all_quit(roots: &[(u32, Option<u64>)], processes: &ProcessTable) -> bool {
    roots.iter().all(|&(pid, created)| {
        processes
            .get(pid)
            .is_none_or(|entry| created.is_some() && entry.created != created)
    })
}

impl Linker {
    fn follow(
        registry: &pw::registry::RegistryRc,
        core: &pw::core::CoreRc,
        stream: &StreamRc,
        filter: Filter,
        status: &Arc<Status>,
    ) -> Following {
        let linker = Rc::new(RefCell::new(Linker {
            core: core.clone(),
            stream: stream.clone(),
            roots: roots_of(&filter),
            filter,
            graph: Graph::default(),
            links: BTreeMap::new(),
            status: Arc::clone(status),
            dirty: true,
            turns: 0,
        }));
        let clients: Rc<RefCell<HashMap<u32, registry::FollowedClient>>> = Rc::default();
        let listener = registry
            .add_listener_local()
            .global({
                let linker = Rc::clone(&linker);
                let clients = Rc::clone(&clients);
                let bind = registry.clone();
                move |global| {
                    let (Some(kind), Some(props)) = (registry::kind(&global.type_), global.props)
                    else {
                        return;
                    };
                    if let Ok(mut linker) = linker.try_borrow_mut() {
                        linker.graph.add(global.id, kind, registry::owned(props));
                        linker.dirty = true;
                    }
                    if kind == graph::Kind::Client {
                        let id = global.id;
                        let linker = Rc::clone(&linker);
                        let followed = registry::follow_client(&bind, global, move |props| {
                            if let Ok(mut linker) = linker.try_borrow_mut() {
                                linker.graph.add(id, graph::Kind::Client, props);
                                linker.dirty = true;
                            }
                        });
                        if let Some(followed) = followed {
                            clients.borrow_mut().insert(id, followed);
                        }
                    }
                }
            })
            .global_remove({
                let linker = Rc::clone(&linker);
                let clients = Rc::clone(&clients);
                move |id| {
                    clients.borrow_mut().remove(&id);
                    if let Ok(mut linker) = linker.try_borrow_mut() {
                        linker.graph.remove(id);
                        linker.dirty = true;
                    }
                }
            })
            .register();
        Following {
            linker,
            _listener: listener,
            _clients: clients,
        }
    }

    fn retarget(&mut self, filter: Filter) {
        self.roots = roots_of(&filter);
        self.filter = filter;
        self.dirty = true;
    }

    /// Relinks after the graph changed, and now and then checks that the
    /// captured apps still run.
    fn tick(&mut self) {
        self.turns = self.turns.wrapping_add(1);
        let check_quit = !self.roots.is_empty() && self.turns.is_multiple_of(QUIT_CHECK_TURNS);
        if !self.dirty && !check_quit {
            return;
        }
        let Ok(processes) = procfs::snapshot() else {
            return;
        };
        if check_quit && all_quit(&self.roots, &processes) {
            self.status.quit();
        }
        if self.dirty {
            self.dirty = false;
            self.relink(&processes);
        }
    }

    fn relink(&mut self, processes: &ProcessTable) {
        let capture = self.stream.node_id();
        let (graph, filter) = (&self.graph, &self.filter);
        let wanted = graph.wanted_links(capture, |id| {
            filter.takes(graph.owner_pid(id, processes), processes)
        });
        self.links.retain(|link, _| wanted.contains(link));
        let mut missing = false;
        for link in wanted {
            if self.links.contains_key(&link) {
                continue;
            }
            let properties = pw::properties::properties! {
                "link.output.node" => link.output_node.to_string(),
                "link.output.port" => link.output_port.to_string(),
                "link.input.node" => link.input_node.to_string(),
                "link.input.port" => link.input_port.to_string(),
                // The links go when AudioSync's connection does.
                "object.linger" => "false",
            };
            match self
                .core
                .create_object::<pw::link::Link>("link-factory", &properties)
            {
                Ok(proxy) => {
                    self.links.insert(link, proxy);
                }
                Err(_) => missing = true,
            }
        }
        self.status.missing_links.store(missing, Ordering::Release);
    }
}

/// Connects `stream` the way `wiring` says, asking for `format`.
fn connect(stream: &StreamRc, wiring: Wiring, format: &[u8]) -> Result<(), BackendError> {
    let format = Pod::from_bytes(format).ok_or(BackendError::UnsupportedFormat)?;
    stream
        .connect(
            Direction::Input,
            wiring.target(),
            wiring.flags(),
            &mut [format],
        )
        .map_err(|_| BackendError::DeviceGone)
}

#[cfg(test)]
mod tests {
    use audiosync_core::eq::equalizer;
    use audiosync_core::ring::Ring;

    use super::*;
    use crate::audio::SESSION_RATE;

    #[test]
    fn each_new_default_moves_the_stream() {
        let loud = Some(r#"{ "name": "ci-loud" }"#);
        let quiet = Some(r#"{ "name": "ci-quiet" }"#);
        let mut output = DefaultOutput::default();
        assert!(output.changed(loud));
        assert!(!output.changed(loud));
        assert!(output.changed(quiet));
        // A default that goes away leaves the stream to the session manager;
        // the next one moves it.
        assert!(!output.changed(None));
        assert!(output.changed(loud));
    }

    #[test]
    fn the_default_value_names_the_sink() {
        assert_eq!(
            sink_name(r#"{ "name": "ci-quiet" }"#).as_deref(),
            Some("ci-quiet")
        );
        assert_eq!(sink_name("ci-quiet"), None);
        assert_eq!(sink_name(r#"{ "id": 38 }"#), None);
    }

    #[test]
    fn the_monitor_wiring_follows_the_default_output() {
        let wiring = Wiring::DefaultOutputMonitor;
        assert_eq!(wiring.target(), None);
        assert!(wiring.flags().contains(StreamFlags::AUTOCONNECT));
        assert!(!wiring.flags().contains(StreamFlags::RT_PROCESS));
        assert!(wiring.captures_output());
        assert!(wiring.follows_default());
    }

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
        let capture = start(
            Wiring::DefaultOutputMonitor,
            writer,
            processor,
            Feeds::default(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(capture.state(), CaptureState::Running);
        assert_eq!(capture.setup(), Some(format::SETUP));
        // Two seconds at 48 kHz, give or take the start.
        let frames = ring.frames_written();
        assert!(frames > 48_000, "{frames} frames written");
        let peak = ring.take_peak();
        assert!(peak > 0.01, "peak {peak}\n{}", graph());
    }

    /// The links and the `default` metadata, for a failing test's message.
    fn graph() -> String {
        let mut text = String::new();
        for (tool, args) in [("pw-link", &["-l"][..]), ("pw-metadata", &[][..])] {
            if let Ok(output) = std::process::Command::new(tool).args(args).output() {
                text.push_str(&String::from_utf8_lossy(&output.stdout));
            }
        }
        text
    }

    /// The ID of the node named `name`, from `pw-dump`.
    fn node_id(name: &str) -> u64 {
        let dump = std::process::Command::new("pw-dump").output().unwrap();
        let objects: serde_json::Value = serde_json::from_slice(&dump.stdout).unwrap();
        objects
            .as_array()
            .unwrap()
            .iter()
            .find(|object| object["info"]["props"]["node.name"] == name)
            .and_then(|object| object["id"].as_u64())
            .unwrap_or_else(|| panic!("no node {name}"))
    }

    fn set_default(name: &str) {
        let id = node_id(name).to_string();
        let status = std::process::Command::new("wpctl")
            .args(["set-default", &id])
            .status()
            .unwrap();
        assert!(status.success());
        std::thread::sleep(Duration::from_secs(1));
    }

    /// Needs the CI job's two null sinks, `ci-loud` and `ci-quiet`, each
    /// playing a tone of its own level through a stream linked to it by hand.
    #[test]
    #[ignore = "needs a running PipeWire daemon with the CI job's two sinks"]
    fn the_capture_follows_a_new_default_output() {
        set_default("ci-loud");
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let writer = ring.writer().unwrap();
        let (_control, processor) = equalizer();
        let capture = start(
            Wiring::DefaultOutputMonitor,
            writer,
            processor,
            Feeds::default(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let loud = ring.take_peak();
        assert!(loud > 0.3, "peak {loud} on the first default\n{}", graph());
        set_default("ci-quiet");
        ring.take_peak();
        std::thread::sleep(Duration::from_secs(1));
        let quiet = ring.take_peak();
        assert_eq!(capture.state(), CaptureState::Running);
        assert!(
            (0.05..0.2).contains(&quiet),
            "peak {quiet} after the default changed\n{}",
            graph()
        );
    }
}
