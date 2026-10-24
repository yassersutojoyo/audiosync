//! One local output: a PipeWire playback stream pinned to its sink, on its
//! own thread with its own connection, like the capture. The stream is
//! connected without `RT_PROCESS`, so the render pipeline is only ever
//! touched on that thread; the callback never allocates, locks, blocks or
//! logs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use audiosync_core::clock;
use audiosync_core::reader::Reader;
use audiosync_core::render::RendererControl;
use audiosync_core::volume::SharedGain;
use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamRc, StreamState};

use super::graph;
use super::timing;
use crate::audio::backend::BackendError;
use crate::audio::controller::RendererHandle;
use crate::audio::outputs::{RenderFormat, StreamSetup, default_channel_mask};
use crate::audio::render::RenderPipeline;

const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The quantum asked for, as the capture asks.
const QUANTUM: u32 = 1024;

/// The most frames one callback renders; a larger request gets this many.
const MAX_FRAMES: usize = 8192;

/// How often the latency the stream reports is handed to the renderer.
const LATENCY_INTERVAL_NS: u64 = 1_000_000_000;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Where and how an output plays.
pub struct Target {
    /// The sink's `object.serial`.
    pub serial: String,
    pub rate: u32,
    pub channels: u16,
    /// The sink's channel names, from its `audio.position`.
    pub positions: Vec<String>,
    pub bluetooth: bool,
}

#[derive(Default)]
struct Status {
    ended: AtomicBool,
    latency_ns: AtomicU64,
    /// The last stream timing's delay, rate denominator, frames filled and
    /// age in ns, for the live tests' failure messages.
    #[cfg(test)]
    timing: [std::sync::atomic::AtomicI64; 4],
}

/// A running output. Dropping it stops the stream at once and waits for its
/// thread.
pub struct Playback {
    control: RendererControl,
    status: Arc<Status>,
    setup: StreamSetup,
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(test)]
impl Playback {
    /// The latency in use and, when the window turned it down, the one the
    /// stream reported.
    pub fn reported_latency_ns(&self) -> (u64, Option<u64>, [i64; 4]) {
        (
            self.status.latency_ns.load(Ordering::Relaxed),
            self.control.rejected_latency_ns(),
            self.status
                .timing
                .each_ref()
                .map(|value| value.load(Ordering::Relaxed)),
        )
    }

    /// The renderer's state and counters, for a failure message.
    pub fn render_report(&self) -> String {
        let control = &self.control;
        format!(
            "fade {:?}, lag {}, late max {} ns, shortfall {}, shortage {}, slowed {}, sped {}, {:?}",
            control.fade_state(),
            control.lag_frames(),
            control.late_max_ns(),
            control.shortfall_frames(),
            control.shortage_frames(),
            control.slowed_frames(),
            control.sped_frames(),
            control.counters()
        )
    }
}

impl RendererHandle for Playback {
    fn request_stop(&mut self) {
        self.control.request_fade_out();
    }

    fn has_ended(&self) -> bool {
        self.status.ended.load(Ordering::Acquire)
    }

    fn is_faded(&self) -> bool {
        self.has_ended()
            || self.control.is_faded()
            || self.control.faded_by_time(
                self.status.latency_ns.load(Ordering::Relaxed),
                clock::now_ns(),
            )
    }

    fn setup(&self) -> Option<StreamSetup> {
        Some(self.setup)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The SPA channel of a channel name; unknown names give 0.
fn channel_id(name: &str) -> u32 {
    match name {
        "MONO" => spa::sys::SPA_AUDIO_CHANNEL_MONO,
        "FL" => spa::sys::SPA_AUDIO_CHANNEL_FL,
        "FR" => spa::sys::SPA_AUDIO_CHANNEL_FR,
        "FC" => spa::sys::SPA_AUDIO_CHANNEL_FC,
        "LFE" => spa::sys::SPA_AUDIO_CHANNEL_LFE,
        "SL" => spa::sys::SPA_AUDIO_CHANNEL_SL,
        "SR" => spa::sys::SPA_AUDIO_CHANNEL_SR,
        "RL" => spa::sys::SPA_AUDIO_CHANNEL_RL,
        "RR" => spa::sys::SPA_AUDIO_CHANNEL_RR,
        "RC" => spa::sys::SPA_AUDIO_CHANNEL_RC,
        _ => 0,
    }
}

/// The `EnumFormat` param for float audio at `target`'s rate and channels.
fn enum_format(target: &Target) -> Result<Vec<u8>, BackendError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(target.rate);
    info.set_channels(u32::from(target.channels));
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    let named = target.positions.len() == usize::from(target.channels);
    let defaults: &[&str] = match target.channels {
        1 => &["MONO"],
        _ => &["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"],
    };
    for (index, slot) in position
        .iter_mut()
        .take(usize::from(target.channels))
        .enumerate()
    {
        let name = if named {
            target.positions[index].as_str()
        } else {
            defaults.get(index).copied().unwrap_or_default()
        };
        *slot = channel_id(name);
    }
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

/// The time until the first frame of the buffer being filled is heard, from
/// the stream's timing; `frames` is that buffer's length.
fn latency_ns(time: &pw::stream::Time, rate: u32, frames: usize, monotonic_ns: u64) -> u64 {
    let fraction = time.rate();
    let since_ns = monotonic_ns.saturating_sub(u64::try_from(time.now()).unwrap_or(0));
    latency_sum_ns(
        time.delay(),
        (fraction.num, fraction.denom),
        time.buffered() + u64::try_from(frames).unwrap_or(0),
        rate,
        since_ns,
    )
}

/// The device delay (`delay_ticks` of `tick` seconds each) plus the frames
/// ahead of the buffer's end at `rate` (the resampler's and the buffer's
/// own, since `pw_buffer.size` is never set and `queued` stays 0), less the
/// time since the report.
fn latency_sum_ns(
    delay_ticks: i64,
    tick: (u32, u32),
    ahead_frames: u64,
    rate: u32,
    since_ns: u64,
) -> u64 {
    let (num, denom) = tick;
    let delay_ns = if denom == 0 {
        0
    } else {
        u128::try_from(delay_ticks.max(0)).unwrap_or(0) * 1_000_000_000 * u128::from(num)
            / u128::from(denom)
    };
    let ahead_ns = u128::from(ahead_frames) * 1_000_000_000 / u128::from(rate.max(1));
    u64::try_from((delay_ns + ahead_ns).saturating_sub(u128::from(since_ns))).unwrap_or(0)
}

/// What the render callback owns.
struct Render {
    pipeline: RenderPipeline,
    control: RendererControl,
    status: Arc<Status>,
    scratch: Vec<f32>,
    channels: usize,
    rate: u32,
    started: bool,
    reported_at_ns: Option<u64>,
}

impl Render {
    fn fill(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let requested = usize::try_from(buffer.requested()).unwrap_or(0);
        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let stride = self.channels * std::mem::size_of::<f32>();
        let frames = {
            let Some(bytes) = data.data() else { return };
            let room = (bytes.len() / stride).min(MAX_FRAMES);
            if requested > 0 {
                requested.min(room)
            } else {
                room
            }
        };
        if !self.started {
            self.pipeline.set_period_frames(frames.max(1));
            self.started = true;
        }
        let now_ns = clock::now_ns();
        let latency = stream.time().ok().map(|time| {
            let monotonic_ns = timing::monotonic_hns().saturating_mul(100);
            #[cfg(test)]
            for (slot, value) in self.status.timing.iter().zip([
                time.delay(),
                i64::from(time.rate().denom),
                i64::try_from(frames).unwrap_or(-1),
                i64::try_from(monotonic_ns).unwrap_or(0) - time.now(),
            ]) {
                slot.store(value, Ordering::Relaxed);
            }
            latency_ns(&time, self.rate, frames, monotonic_ns)
        });
        if let Some(latency) = latency
            && self
                .reported_at_ns
                .is_none_or(|at| now_ns.saturating_sub(at) >= LATENCY_INTERVAL_NS)
        {
            let used = self.control.set_reported_latency(latency, None);
            self.status.latency_ns.store(used, Ordering::Relaxed);
            self.reported_at_ns = Some(now_ns);
        }
        let samples = &mut self.scratch[..frames * self.channels];
        self.pipeline
            .render_timed(samples, now_ns + latency.unwrap_or(0), now_ns);
        if let Some(bytes) = data.data() {
            for (out, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter()) {
                out.copy_from_slice(&sample.to_le_bytes());
            }
        }
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = i32::try_from(stride).unwrap_or(0);
        *chunk.size_mut() = u32::try_from(frames * stride).unwrap_or(0);
    }
}

/// Starts playing to `target` through `reader`. Returns once the stream is
/// linked, or why it couldn't be.
pub fn start(
    target: Target,
    reader: Reader,
    control: RendererControl,
    gain: Arc<SharedGain>,
) -> Result<Playback, BackendError> {
    let channels = usize::from(target.channels.max(1));
    let pipeline = RenderPipeline::new(reader, &control, gain, target.rate, channels)?;
    control.set_bluetooth(target.bluetooth);
    let setup = StreamSetup {
        format: RenderFormat {
            rate: target.rate,
            channels: target.channels,
            channel_mask: default_channel_mask(target.channels),
            convert: false,
        },
        buffer_frames: QUANTUM,
    };
    let status = Arc::new(Status::default());
    let render = Render {
        pipeline,
        control: control.clone(),
        status: Arc::clone(&status),
        scratch: vec![0.0; MAX_FRAMES * channels],
        channels,
        rate: target.rate,
        started: false,
        reported_at_ns: None,
    };
    let (stop, stop_receiver) = pw::channel::channel();
    let (ready, ready_receiver) = mpsc::channel();
    let thread_status = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("audiosync-pw-out".to_owned())
        .spawn(move || {
            if let Err(error) = serve(
                &target,
                render,
                stop_receiver,
                ready.clone(),
                &thread_status,
            ) {
                thread_status.ended.store(true, Ordering::Release);
                let _ = ready.send(Err(error));
            }
        })
        .map_err(|error| BackendError::System {
            call: "std::thread::spawn",
            code: error.raw_os_error().unwrap_or(0),
        })?;
    let playback = Playback {
        control,
        status,
        setup,
        stop,
        thread: Some(thread),
    };
    match ready_receiver.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(playback),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(BackendError::NotResponding),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(BackendError::DeviceGone),
    }
}

fn serve(
    target: &Target,
    render: Render,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<(), BackendError>>,
    status: &Arc<Status>,
) -> Result<(), BackendError> {
    pw::init();
    let main_loop =
        pw::main_loop::MainLoopRc::new(None).map_err(|_| BackendError::ServiceNotRunning)?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let core = context
        .connect_rc(None)
        .map_err(|_| BackendError::ServiceNotRunning)?;
    let _core_listener = core
        .add_listener_local()
        .error({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |id, _seq, _res, _message| {
                if id == pw::core::PW_ID_CORE {
                    status.ended.store(true, Ordering::Release);
                    let _ = ready.send(Err(BackendError::DeviceGone));
                    main_loop.quit();
                }
            }
        })
        .register();

    let name = format!(
        "audiosync-out-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed) + 1
    );
    let properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => name.as_str(),
        *pw::keys::APP_NAME => "AudioSync",
        *pw::keys::NODE_LATENCY => format!("{QUANTUM}/{}", target.rate),
        "target.object" => target.serial.as_str(),
        // Ends with its sink, never lands on another, and can't be moved:
        // a delayed copy only ever plays where the user chose.
        "node.dont-reconnect" => "true",
        "node.dont-fallback" => "true",
        "node.dont-move" => "true",
        "state.restore-target" => "false",
        "state.restore-props" => "false",
    };
    let stream =
        StreamRc::new(core.clone(), &name, properties).map_err(|_| BackendError::DeviceGone)?;
    let _listener = stream
        .add_local_listener_with_user_data(render)
        .state_changed({
            let main_loop = main_loop.clone();
            let status = Arc::clone(status);
            let ready = ready.clone();
            move |_stream, _, old, new| match new {
                StreamState::Paused | StreamState::Streaming => {
                    let _ = ready.send(Ok(()));
                }
                StreamState::Error(_) => {
                    status.ended.store(true, Ordering::Release);
                    let _ = ready.send(Err(BackendError::DeviceGone));
                    main_loop.quit();
                }
                StreamState::Unconnected
                    if matches!(old, StreamState::Paused | StreamState::Streaming) =>
                {
                    status.ended.store(true, Ordering::Release);
                    main_loop.quit();
                }
                _ => {}
            }
        })
        .process(|stream, render| render.fill(stream))
        .register()
        .map_err(|_| BackendError::DeviceGone)?;

    let format = enum_format(target)?;
    let format = Pod::from_bytes(&format).ok_or(BackendError::UnsupportedFormat)?;
    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [format],
        )
        .map_err(|_| BackendError::DeviceGone)?;

    let _stop = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |()| main_loop.quit()
    });
    main_loop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// What `start` needs to play to the sink named `id` in `graph`.
pub fn target(graph: &graph::Graph, id: &str) -> Result<Target, BackendError> {
    let (node_id, node) = graph.named(id).ok_or(BackendError::DeviceGone)?;
    let serial = node
        .prop("object.serial")
        .ok_or(BackendError::DeviceGone)?
        .to_owned();
    let (rate, channels) = graph.sink_format(node_id);
    let positions = node
        .prop("audio.position")
        .map(graph::positions)
        .unwrap_or_default()
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok(Target {
        serial,
        rate,
        channels,
        positions,
        bluetooth: node.bluetooth(),
    })
}

#[cfg(test)]
mod tests {
    use super::latency_sum_ns;

    #[test]
    fn the_latency_adds_the_delay_and_the_frames_ahead_less_the_age() {
        // 1024 ticks of 1/48000 s and a 1024-frame buffer, reported 1 ms ago.
        assert_eq!(
            latency_sum_ns(1024, (1, 48_000), 1024, 48_000, 1_000_000),
            41_666_666
        );
        // The resampler's frames count like the buffer's.
        assert_eq!(
            latency_sum_ns(0, (1, 48_000), 480 + 480, 48_000, 0),
            20_000_000
        );
    }

    #[test]
    fn the_latency_never_goes_below_zero_or_trusts_a_bad_report() {
        assert_eq!(
            latency_sum_ns(-500, (1, 48_000), 480, 48_000, 0),
            10_000_000
        );
        assert_eq!(latency_sum_ns(1024, (1, 0), 480, 48_000, 0), 10_000_000);
        assert_eq!(latency_sum_ns(0, (1, 48_000), 480, 48_000, 50_000_000), 0);
    }
}
