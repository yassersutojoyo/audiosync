//! Microphone calibration of one local output against the reference (spec
//! S232 to S234): the music fades out, the core's test sweep plays on the
//! reference alone and then on the chosen output alone, the default input
//! device records each run, and the core's estimate gives the chosen
//! output's new offset. A refusal, a cancel or any change during the runs
//! leaves the offset as it was.
//!
//! [`CalibrationTap`] is the capture's side: the converter fades the music
//! out of the ring and the senders' feeds and adds the sweep to the ring
//! only, so it reaches each local output through its renderer at the
//! hold-back it has. [`CalibrationRun`] is the control side, ticked by the
//! UI thread; the decisions it makes need no hardware, so they are tested
//! against a [`Rig`] of fakes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use audiosync_core::calibrate::{
    self, Calibration, FADE_SECONDS, MAX_RATE, MAX_RECORDING_FRAMES, MIN_RATE, Recording, Refusal,
    Refused, SIGNAL_SECONDS,
};

use super::SESSION_RATE;
use super::backend::AudioBackend;
use crate::debug_log::DebugLog;

/// The fades of the music and of a cut-off sweep, in session-rate frames.
const FADE_FRAMES: usize = (FADE_SECONDS * SESSION_RATE as f64) as usize;

/// No signal frame to write.
const IDLE: i64 = -1;

/// No time recorded yet.
const UNSET: u64 = u64::MAX;

/// Which path the test signal takes: the ring, which reaches the local
/// outputs, or one network sender's feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Ring,
    AirPlay,
    Cast,
}

impl Route {
    fn from_u8(value: u8) -> Route {
        match value {
            1 => Route::AirPlay,
            2 => Route::Cast,
            _ => Route::Ring,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Route::Ring => 0,
            Route::AirPlay => 1,
            Route::Cast => 2,
        }
    }
}

/// The capture's way to play the test signal through the session's ring or
/// a network sender's feed.
///
/// While engaged, the captured audio fades out over [`FADE_FRAMES`] and
/// stays silent, for the ring and the network senders alike; disengaged, it
/// fades back in. A signal asked for with [`CalibrationTap::play`] starts
/// with the next block and goes along its [`Route`] only: into the ring, or
/// into the AirPlay or the Cast feed; the time of that block is kept as the
/// moment the signal entered every output's path, before its hold-back.
/// Which output or AirPlay speaker is heard is the controller's gains and
/// volumes, not the tap.
///
/// The capture thread calls [`CalibrationTap::is_active`],
/// [`CalibrationTap::route`],
/// [`CalibrationTap::fade_music`], [`CalibrationTap::block_cursor`],
/// [`CalibrationTap::add_signal`] and [`CalibrationTap::signal_started`]:
/// atomics only, no allocation, lock or wait. The rest runs on the UI thread.
pub struct CalibrationTap {
    /// The signal at [`SESSION_RATE`], mono.
    signal: Box<[f32]>,
    engaged: AtomicBool,
    /// The music's level in frames of its fade: 0 silent, [`FADE_FRAMES`]
    /// full. Written by the capture thread only.
    music_level: AtomicUsize,
    /// The next signal frame to write, or [`IDLE`].
    position: AtomicI64,
    /// When the signal's first frame entered the ring, or [`UNSET`].
    start_ns: AtomicU64,
    /// The signal's level in frames of its fade-out.
    signal_level: AtomicUsize,
    /// The signal fades out instead of playing to its end.
    stopping: AtomicBool,
    /// The [`Route`] the signal takes.
    route: AtomicU8,
}

impl CalibrationTap {
    pub fn new() -> CalibrationTap {
        CalibrationTap {
            signal: calibrate::signal(SESSION_RATE)
                .unwrap_or_default()
                .into_boxed_slice(),
            engaged: AtomicBool::new(false),
            music_level: AtomicUsize::new(FADE_FRAMES),
            position: AtomicI64::new(IDLE),
            start_ns: AtomicU64::new(UNSET),
            signal_level: AtomicUsize::new(FADE_FRAMES),
            stopping: AtomicBool::new(false),
            route: AtomicU8::new(Route::Ring.as_u8()),
        }
    }

    /// Sends the signal played next along `route`.
    pub fn set_route(&self, route: Route) {
        self.route.store(route.as_u8(), Ordering::Release);
    }

    /// The signal's path. Capture thread.
    pub fn route(&self) -> Route {
        Route::from_u8(self.route.load(Ordering::Acquire))
    }

    /// The music fades out and stays out.
    pub fn engage(&self) {
        self.engaged.store(true, Ordering::Release);
    }

    /// The music fades back in, and a signal still on its way stops.
    pub fn disengage(&self) {
        self.position.store(IDLE, Ordering::Release);
        self.engaged.store(false, Ordering::Release);
    }

    /// Plays the signal from its start, beginning with the next block.
    pub fn play(&self) {
        self.start_ns.store(UNSET, Ordering::Relaxed);
        self.signal_level.store(FADE_FRAMES, Ordering::Relaxed);
        self.stopping.store(false, Ordering::Relaxed);
        self.position.store(0, Ordering::Release);
    }

    /// Fades a signal on its way out, so a cut-off sweep doesn't click.
    pub fn fade_out_signal(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    /// When the signal last asked for entered the ring, on the core's clock.
    pub fn played_at_ns(&self) -> Option<u64> {
        let ns = self.start_ns.load(Ordering::Acquire);
        (ns != UNSET).then_some(ns)
    }

    /// Engaged, or the music still fades back in. Capture thread.
    pub fn is_active(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
            || self.music_level.load(Ordering::Relaxed) < FADE_FRAMES
            || self.position.load(Ordering::Relaxed) != IDLE
    }

    /// Fades `block`, interleaved stereo, toward silence while engaged and
    /// back toward full otherwise. Capture thread.
    pub fn fade_music(&self, block: &mut [f32]) {
        let down = self.engaged.load(Ordering::Acquire);
        let mut level = self.music_level.load(Ordering::Relaxed);
        if !down && level >= FADE_FRAMES {
            return;
        }
        for frame in block.as_chunks_mut::<2>().0 {
            if down {
                level = level.saturating_sub(1);
            } else if level < FADE_FRAMES {
                level += 1;
            }
            let gain = level as f32 / FADE_FRAMES as f32;
            frame[0] *= gain;
            frame[1] *= gain;
        }
        self.music_level.store(level, Ordering::Relaxed);
    }

    /// Where a block's signal starts: the next frame to write, or [`IDLE`].
    /// Read once at the start of each block. A signal for a network feed
    /// waits for a block that starts with the music fully faded, since the
    /// block is cleared once that feed has it. Capture thread.
    pub fn block_cursor(&self) -> i64 {
        if self.route() != Route::Ring && !self.music_silent() {
            return IDLE;
        }
        self.position.load(Ordering::Acquire)
    }

    /// The music faded out completely and stays out.
    fn music_silent(&self) -> bool {
        self.engaged.load(Ordering::Acquire) && self.music_level.load(Ordering::Relaxed) == 0
    }

    /// Adds the signal from `cursor` to `block`, interleaved stereo, and
    /// moves `cursor` on. A play, a stop or a disengage from the UI thread
    /// since the block started wins: the block then adds no more. Capture
    /// thread.
    pub fn add_signal(&self, block: &mut [f32], cursor: &mut i64) {
        if *cursor < 0 {
            return;
        }
        let stopping = self.stopping.load(Ordering::Acquire);
        let mut level = self.signal_level.load(Ordering::Relaxed);
        let mut next = *cursor as usize;
        for frame in block.as_chunks_mut::<2>().0 {
            if next >= self.signal.len() || (stopping && level == 0) {
                next = self.signal.len();
                break;
            }
            if stopping {
                level -= 1;
            }
            let sample = self.signal[next] * (level as f32 / FADE_FRAMES as f32);
            frame[0] += sample;
            frame[1] += sample;
            next += 1;
        }
        self.signal_level.store(level, Ordering::Relaxed);
        let stored = if next >= self.signal.len() {
            IDLE
        } else {
            next as i64
        };
        *cursor = match self.position.compare_exchange(
            *cursor,
            stored,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => stored,
            Err(_) => IDLE,
        };
    }

    /// The block whose cursor started at 0 closed at `ns`, `frames` long:
    /// its first frame entered the ring then. Capture thread.
    pub fn signal_started(&self, ns: u64, frames: usize) {
        let length = frames as u64 * 1_000_000_000 / u64::from(SESSION_RATE);
        let _ = self.start_ns.compare_exchange(
            UNSET,
            ns.saturating_sub(length),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

impl Default for CalibrationTap {
    fn default() -> CalibrationTap {
        CalibrationTap::new()
    }
}

/// Which of the two runs something concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Reference,
    Chosen,
}

/// Why a calibration changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No reference output (the PC's own speakers) is listed, or the chosen
    /// output is the reference.
    NoReference,
    /// Streaming is off, or the chosen output or the reference isn't playing.
    NotPlaying,
    /// Windows' privacy settings block microphone access for desktop apps.
    MicrophoneDenied,
    /// No default input device, or one that delivered nothing.
    NoMicrophone,
    /// The input device runs at a rate the test signal isn't made at.
    UnsupportedRate,
    /// The capture wrote no block to carry the signal.
    SourceSilent,
    /// Streaming stopped, the source changed, or the speakers or their
    /// delays changed during the measurement.
    Interrupted,
    /// The default input device or its rate changed between the two runs.
    MicrophoneChanged,
    /// The microphone lost audio during a recording, so its timing can't
    /// be trusted.
    MicrophoneSkipped,
    /// The default input device is a Bluetooth headset's hands-free one,
    /// which opening would switch to call mode.
    HandsFreeMicrophone,
    /// The estimate couldn't run.
    Unfinished,
    /// Calibrate All found no speaker on besides the reference.
    NothingToMeasure,
    TooShort(Option<Which>),
    TooLong(Option<Which>),
    Clipped(Option<Which>),
    TooQuiet(Option<Which>),
    AmbiguousEcho(Option<Which>),
    Implausible,
}

/// What a running calibration does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The music fades out and what is still on its way plays out.
    Preparing,
    Measuring(Which),
    Estimating,
}

/// Where one calibration stands.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    Running(Step),
    /// The new offset was set; `clamped` when the range cut it.
    Done {
        offset_ms: f64,
        clamped: bool,
    },
    Failed(Failure),
    Cancelled,
}

impl Phase {
    pub fn is_running(&self) -> bool {
        matches!(self, Phase::Running(_))
    }
}

/// What a shown result depends on: once one of these changes, it no longer
/// describes the speaker and goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub streaming: bool,
    pub reference: Option<String>,
    pub speaker_on: bool,
}

/// The calibration the window shows: the output it concerns, by endpoint
/// ID, and its phase.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub id: Option<String>,
    pub phase: Phase,
    /// The context `phase` was shown in.
    pub context: Option<Context>,
    /// A run still holds the outputs: running, or letting a cut-off sweep
    /// play out after it ended.
    pub holds_outputs: bool,
}

impl Status {
    /// The status once streaming, the reference or the speaker's switch may
    /// have changed: an ended run's line goes once its context moved; a
    /// running one stays, since the run checks for itself.
    pub fn following(&self, now: &Context) -> Status {
        if self.phase.is_running()
            || self.id.is_none()
            || self.phase == Phase::Idle
            || self.context.as_ref() == Some(now)
        {
            return self.clone();
        }
        Status {
            phase: Phase::Idle,
            context: None,
            ..self.clone()
        }
    }

    /// The status after output `id`'s offset was set by hand: the line
    /// about that output's last calibration goes.
    pub fn offset_changed(&self, id: &str) -> Status {
        if self.phase.is_running() || self.id.as_deref() != Some(id) {
            return self.clone();
        }
        Status {
            phase: Phase::Idle,
            context: None,
            ..self.clone()
        }
    }
}

/// What the backend plays, as a calibration depends on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Engine {
    /// The running session's number, which a source change or a capture
    /// restart moves on; `None` while streaming is off.
    pub session: Option<u64>,
    /// The local outputs that play, by endpoint ID.
    pub playing: Vec<String>,
    pub reference: Option<String>,
    /// Each playing output's hold-back, in ms.
    pub hold_backs_ms: BTreeMap<String, f64>,
}

/// The input device a run recorded with, and its rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrophoneId {
    pub device: String,
    pub rate: u32,
}

/// Why the microphone didn't start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophoneError {
    Denied,
    Missing,
    HandsFree,
}

impl From<MicrophoneError> for Failure {
    fn from(error: MicrophoneError) -> Failure {
        match error {
            MicrophoneError::Denied => Failure::MicrophoneDenied,
            MicrophoneError::Missing => Failure::NoMicrophone,
            MicrophoneError::HandsFree => Failure::HandsFreeMicrophone,
        }
    }
}

/// When a recording's first frame was made, and where it first lost audio,
/// from what each packet says. Allocates nothing: the recording thread
/// keeps one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordingClock {
    pub first_frame_ns: Option<u64>,
    pub gap_at: Option<usize>,
}

impl RecordingClock {
    /// A packet that follows `frames_before` frames at `rate`, recorded
    /// `age_ns` before `now_ns` (`None`: no position came with it). The
    /// first frame's time comes from the first packet whose timestamp the
    /// device vouches for. A discontinuity on the very first packet only
    /// marks the stream's start.
    pub fn packet(
        &mut self,
        frames_before: usize,
        rate: u32,
        now_ns: u64,
        age_ns: Option<u64>,
        timestamp_error: bool,
        discontinuity: bool,
    ) {
        if discontinuity && frames_before > 0 && self.gap_at.is_none() {
            self.gap_at = Some(frames_before);
        }
        if self.first_frame_ns.is_some() || timestamp_error || rate == 0 {
            return;
        }
        if let Some(age_ns) = age_ns {
            let before_ns = (frames_before as u128 * 1_000_000_000 / u128::from(rate)) as u64;
            self.first_frame_ns = Some(now_ns.saturating_sub(age_ns).saturating_sub(before_ns));
        }
    }
}

/// A running recording of the default input device, mono.
pub trait Microphone {
    fn id(&self) -> MicrophoneId;
    /// When its first frame was recorded, on the core's clock.
    fn first_frame_ns(&self) -> Option<u64>;
    fn frames_recorded(&self) -> usize;
    /// The frame where the recording first lost audio, if it did.
    fn gap_at(&self) -> Option<usize>;
    /// Stops recording and gives `count` frames from `start`, fewer when it
    /// recorded less.
    fn take(&mut self, start: usize, count: usize) -> Vec<f32>;
}

/// What a run uses of the app: the backend, through the blanket impl below,
/// or fakes in the tests.
pub trait Rig {
    fn engine(&self) -> Engine;
    /// Output `id`'s stored offset.
    fn offset_ms(&self, id: &str) -> Option<f64>;
    fn tap(&self) -> Option<Arc<CalibrationTap>>;
    /// Only `id` of the local outputs is heard; `None`: all are.
    fn set_solo(&mut self, id: Option<&str>);
    /// Starts recording the default input device, with room for `seconds`.
    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError>;
    /// Sets output `id`'s offset, as its delay field does.
    fn set_offset(&mut self, id: &str, ms: f64);
}

impl<A: AudioBackend> Rig for A {
    fn engine(&self) -> Engine {
        self.calibration_engine()
    }

    fn offset_ms(&self, id: &str) -> Option<f64> {
        self.outputs()
            .into_iter()
            .find(|output| output.id == id)
            .map(|output| output.delay_ms)
    }

    fn tap(&self) -> Option<Arc<CalibrationTap>> {
        self.calibration_tap()
    }

    fn set_solo(&mut self, id: Option<&str>) {
        self.set_calibration_solo(id);
    }

    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError> {
        AudioBackend::open_microphone(self, seconds)
    }

    fn set_offset(&mut self, id: &str, ms: f64) {
        self.set_output_delay(id, ms);
    }
}

/// Allowance for an output's own latency in a recording's length; a
/// Bluetooth output stays under it.
pub const LATENCY_ALLOWANCE_S: f64 = 0.5;

/// Room after the signal in each recording.
const MARGIN_S: f64 = 0.5;

/// How long the microphone gets to deliver its first frames.
pub(crate) const WARM_UP_LIMIT_S: f64 = 2.0;

/// How long the microphone runs before the signal plays, so its own start
/// is behind it.
pub(crate) const WARM_UP_S: f64 = 0.2;

/// How long the capture gets to carry the signal into the ring.
pub(crate) const SIGNAL_WAIT_S: f64 = 1.5;

/// Past a run's length, how long a stalled microphone gets before the
/// estimate judges what came in.
pub(crate) const STALL_S: f64 = 2.0;

/// Whether output `chosen` can be calibrated now: a reference other than
/// it is listed, streaming runs, and both play (S232: both take part).
pub fn blocker(engine: &Engine, chosen: &str) -> Option<Failure> {
    let Some(reference) = engine.reference.as_deref() else {
        return Some(Failure::NoReference);
    };
    if reference == chosen {
        return Some(Failure::NoReference);
    }
    let plays = |id: &str| engine.playing.iter().any(|p| p == id);
    if engine.session.is_none() || !plays(chosen) || !plays(reference) {
        return Some(Failure::NotPlaying);
    }
    None
}

/// Why the button can't start a calibration, from what the row knows;
/// `None` when it may try ([`blocker`] checks the rest on the press).
pub fn unavailable(streaming: bool, has_reference: bool, speaker_on: bool) -> Option<Failure> {
    if !has_reference {
        Some(Failure::NoReference)
    } else if streaming && speaker_on {
        None
    } else {
        Some(Failure::NotPlaying)
    }
}

/// Frames each run keeps from the moment the signal enters the outputs'
/// paths (S232): the largest hold-back taking part, the output's latency,
/// the signal and a margin.
pub fn recording_frames(largest_hold_back_ms: f64, rate: u32) -> usize {
    let hold_back_s = if largest_hold_back_ms.is_finite() {
        largest_hold_back_ms.max(0.0) / 1000.0
    } else {
        0.0
    };
    ((hold_back_s + LATENCY_ALLOWANCE_S + SIGNAL_SECONDS + MARGIN_S) * f64::from(rate)).ceil()
        as usize
}

/// How long the music on its way through the hold-backs takes to play out
/// once it faded, before the first run.
pub fn quiet_seconds(largest_hold_back_ms: f64) -> f64 {
    let hold_back_s = if largest_hold_back_ms.is_finite() {
        largest_hold_back_ms.max(0.0) / 1000.0
    } else {
        0.0
    };
    hold_back_s + LATENCY_ALLOWANCE_S
}

/// How long an ended run keeps the outputs as they are (one heard, the
/// music silent): a sweep already sent plays out through the hold-backs on
/// that one output, faded, instead of on every output at once. Nothing to
/// wait for when no sweep was sent, or its session is gone.
pub fn settle_seconds(signal_sent: bool, same_session: bool, largest_hold_back_ms: f64) -> f64 {
    if signal_sent && same_session {
        quiet_seconds(largest_hold_back_ms)
    } else {
        0.0
    }
}

/// Whether the microphone just opened may record a run: the chosen output's
/// run uses the device and rate of the reference's, since only an input
/// latency that is the same in both cancels out (S232); the rate is one the
/// signal is made at; and the recording fits the estimate's limit there.
pub fn microphone_check(
    first: Option<&MicrophoneId>,
    now: &MicrophoneId,
    largest_hold_back_ms: f64,
) -> Option<Failure> {
    if first.is_some_and(|first| first != now) {
        return Some(Failure::MicrophoneChanged);
    }
    if !(MIN_RATE..=MAX_RATE).contains(&now.rate) {
        return Some(Failure::UnsupportedRate);
    }
    if recording_frames(largest_hold_back_ms, now.rate) > MAX_RECORDING_FRAMES {
        return Some(Failure::TooLong(None));
    }
    None
}

/// The recorded frame at `signal_ns`, the moment the signal entered the
/// outputs' paths, in a recording whose first frame was at `first_frame_ns`:
/// each run starts there, so the gap from recording start to play start is
/// 0 in both (S232). `None` when the recording started after it.
pub fn trim_start(signal_ns: u64, first_frame_ns: u64, rate: u32) -> Option<usize> {
    let since = signal_ns.checked_sub(first_frame_ns)?;
    Some((since as f64 * f64::from(rate) / 1e9).round() as usize)
}

/// What the estimate's answer does to the chosen output's offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Commit { offset_ms: f64, clamped: bool },
    Refuse(Failure),
}

pub fn outcome(result: &Result<Calibration, Refused>) -> Outcome {
    match result {
        Ok(found) => Outcome::Commit {
            offset_ms: found.offset_ms,
            clamped: found.clamped,
        },
        Err(refused) => {
            let which = refused.recording.map(|recording| match recording {
                Recording::Reference => Which::Reference,
                Recording::Chosen => Which::Chosen,
            });
            Outcome::Refuse(match refused.reason {
                Refusal::TooShort => Failure::TooShort(which),
                Refusal::TooLong => Failure::TooLong(which),
                Refusal::Clipped => Failure::Clipped(which),
                Refusal::TooQuiet => Failure::TooQuiet(which),
                Refusal::AmbiguousEcho => Failure::AmbiguousEcho(which),
                Refusal::Implausible => Failure::Implausible,
            })
        }
    }
}

pub(crate) fn ns(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1e9) as u64
}

enum Stage {
    Quiet {
        until_ns: u64,
    },
    /// The microphone runs; the signal plays once it is warm.
    Warming {
        which: Which,
        mic: Box<dyn Microphone>,
        since_ns: u64,
    },
    Playing {
        which: Which,
        mic: Box<dyn Microphone>,
        asked_ns: u64,
    },
    Recording {
        which: Which,
        mic: Box<dyn Microphone>,
        start: usize,
        frames: usize,
        deadline_ns: u64,
    },
    Estimating(Receiver<Result<Calibration, Refused>>),
    /// Ended; a cut-off sweep plays out before the outputs come back.
    Settling {
        until_ns: u64,
    },
    /// Ended, and the outputs are back.
    Over,
}

/// One calibration, from the music fading out to the new offset or the
/// reason there is none. The UI thread ticks it every few tens of ms.
pub struct CalibrationRun {
    chosen: String,
    reference: String,
    stage: Stage,
    tap: Arc<CalibrationTap>,
    /// The phase the row shows.
    phase: Phase,
    /// The reference's recording and its rate.
    reference_take: Option<(Vec<f32>, u32)>,
    /// The input the reference's run recorded with.
    first_microphone: Option<MicrophoneId>,
    // What the measurement depends on, as it was at the start (S232).
    session: u64,
    playing: Vec<String>,
    hold_backs_ms: BTreeMap<String, f64>,
    old_offset_ms: f64,
    largest_hold_back_ms: f64,
    log: DebugLog,
}

impl CalibrationRun {
    /// Starts calibrating output `chosen`: the music fades out. Refused with
    /// why when it can't start; nothing changed then.
    pub fn start(
        rig: &mut dyn Rig,
        chosen: &str,
        now_ns: u64,
        log: DebugLog,
    ) -> Result<CalibrationRun, Failure> {
        let engine = rig.engine();
        let refuse = |failure: Failure| {
            log.log(|| format!("calibration of {chosen} refused: {failure:?}"));
            Err(failure)
        };
        let Some(reference) = engine.reference.clone() else {
            return refuse(Failure::NoReference);
        };
        if let Some(failure) = blocker(&engine, chosen) {
            return refuse(failure);
        }
        let (Some(session), Some(tap)) = (engine.session, rig.tap()) else {
            return refuse(Failure::NotPlaying);
        };
        // Windows' privacy settings show only when a stream opens: asking
        // before the music fades keeps a denial from pausing it.
        match rig.open_microphone(0.1) {
            Ok(mut probe) => drop(probe.take(0, 0)),
            Err(error) => return refuse(error.into()),
        }
        let largest_hold_back_ms = engine
            .hold_backs_ms
            .values()
            .copied()
            .filter(|ms| ms.is_finite())
            .fold(0.0, f64::max);
        let old_offset_ms = rig.offset_ms(chosen).unwrap_or(0.0);
        tap.engage();
        log.log(|| {
            format!(
                "calibration of {chosen} against {reference} started: hold-backs {:?} ms, \
                 offset {old_offset_ms} ms",
                engine.hold_backs_ms
            )
        });
        Ok(CalibrationRun {
            chosen: chosen.to_owned(),
            reference,
            stage: Stage::Quiet {
                until_ns: now_ns + ns(quiet_seconds(largest_hold_back_ms)),
            },
            tap,
            phase: Phase::Running(Step::Preparing),
            reference_take: None,
            first_microphone: None,
            session,
            playing: sorted(engine.playing),
            hold_backs_ms: engine.hold_backs_ms,
            old_offset_ms,
            largest_hold_back_ms,
            log,
        })
    }

    /// The output it calibrates, by endpoint ID.
    pub fn chosen(&self) -> &str {
        &self.chosen
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether it still holds the outputs (running, or a cut-off sweep
    /// playing out).
    pub fn holds_outputs(&self) -> bool {
        !matches!(self.stage, Stage::Over)
    }

    /// The user cancelled it: nothing changes.
    pub fn cancel(&mut self, rig: &mut dyn Rig, now_ns: u64) {
        if self.phase.is_running() {
            self.log
                .log(|| format!("calibration of {} cancelled", self.chosen));
            self.end(rig, Phase::Cancelled, now_ns);
        }
    }

    /// Moves the run on. Returns whether what the row shows changed.
    pub fn tick(&mut self, rig: &mut dyn Rig, now_ns: u64) -> bool {
        let before = (self.phase, self.holds_outputs());
        self.advance(rig, now_ns);
        before != (self.phase, self.holds_outputs())
    }

    fn advance(&mut self, rig: &mut dyn Rig, now_ns: u64) {
        match &self.stage {
            Stage::Over => return,
            Stage::Settling { until_ns } => {
                let session_goes_on = rig.engine().session == Some(self.session);
                if !session_goes_on || now_ns >= *until_ns {
                    self.give_back(rig);
                }
                return;
            }
            Stage::Estimating(_) => return self.collect(rig, now_ns),
            _ => {}
        }
        if self.changed(&rig.engine()) {
            return self.fail(rig, Failure::Interrupted, now_ns);
        }
        match std::mem::replace(&mut self.stage, Stage::Over) {
            Stage::Quiet { until_ns } => {
                self.stage = Stage::Quiet { until_ns };
                if now_ns >= until_ns {
                    self.begin(rig, Which::Reference, now_ns);
                }
            }
            Stage::Warming {
                which,
                mic,
                since_ns,
            } => {
                if mic.first_frame_ns().is_some() && now_ns >= since_ns + ns(WARM_UP_S) {
                    self.tap.play();
                    self.stage = Stage::Playing {
                        which,
                        mic,
                        asked_ns: now_ns,
                    };
                } else if now_ns >= since_ns + ns(WARM_UP_LIMIT_S) {
                    self.stage = Stage::Warming {
                        which,
                        mic,
                        since_ns,
                    };
                    self.fail(rig, Failure::NoMicrophone, now_ns);
                } else {
                    self.stage = Stage::Warming {
                        which,
                        mic,
                        since_ns,
                    };
                }
            }
            Stage::Playing {
                which,
                mic,
                asked_ns,
            } => match (self.tap.played_at_ns(), mic.first_frame_ns()) {
                (Some(played), Some(first)) => {
                    let rate = mic.id().rate;
                    let Some(start) = trim_start(played, first, rate) else {
                        self.stage = Stage::Playing {
                            which,
                            mic,
                            asked_ns,
                        };
                        return self.fail(rig, Failure::NoMicrophone, now_ns);
                    };
                    let frames = recording_frames(self.largest_hold_back_ms, rate);
                    self.stage = Stage::Recording {
                        which,
                        mic,
                        start,
                        frames,
                        deadline_ns: now_ns + ns(frames as f64 / f64::from(rate) + STALL_S),
                    };
                }
                _ => {
                    let silent = now_ns >= asked_ns + ns(SIGNAL_WAIT_S);
                    self.stage = Stage::Playing {
                        which,
                        mic,
                        asked_ns,
                    };
                    if silent {
                        self.fail(rig, Failure::SourceSilent, now_ns);
                    }
                }
            },
            Stage::Recording {
                which,
                mut mic,
                start,
                frames,
                deadline_ns,
            } => {
                // Past the deadline the microphone stalled: the estimate
                // judges what came in.
                let waiting = mic.frames_recorded() < start + frames && now_ns < deadline_ns;
                let skipped = mic.gap_at().is_some_and(|gap| gap < start + frames);
                if waiting || skipped {
                    self.stage = Stage::Recording {
                        which,
                        mic,
                        start,
                        frames,
                        deadline_ns,
                    };
                    if skipped {
                        self.fail(rig, Failure::MicrophoneSkipped, now_ns);
                    }
                    return;
                }
                let rate = mic.id().rate;
                let samples = mic.take(start, frames);
                drop(mic);
                self.log.log(|| {
                    format!(
                        "calibration: {which:?} recorded, {} frames at {rate} Hz from frame {start}",
                        samples.len()
                    )
                });
                match which {
                    Which::Reference => {
                        self.reference_take = Some((samples, rate));
                        self.begin(rig, Which::Chosen, now_ns);
                    }
                    Which::Chosen => self.estimate(rig, samples, rate, now_ns),
                }
            }
            other => self.stage = other,
        }
    }

    /// Whether what the measurement depends on moved since the start.
    fn changed(&self, engine: &Engine) -> bool {
        let hold_back = |engine_map: &BTreeMap<String, f64>, id: &str| engine_map.get(id).copied();
        engine.session != Some(self.session)
            || engine.reference.as_deref() != Some(self.reference.as_str())
            || sorted(engine.playing.clone()) != self.playing
            || [self.chosen.as_str(), self.reference.as_str()]
                .iter()
                .any(|id| {
                    hold_back(&engine.hold_backs_ms, id) != hold_back(&self.hold_backs_ms, id)
                })
    }

    /// One run: only `which`'s output is heard, the microphone starts, and
    /// the signal plays once it is warm.
    fn begin(&mut self, rig: &mut dyn Rig, which: Which, now_ns: u64) {
        let solo = match which {
            Which::Reference => self.reference.clone(),
            Which::Chosen => self.chosen.clone(),
        };
        rig.set_solo(Some(&solo));
        let room = quiet_seconds(self.largest_hold_back_ms)
            + SIGNAL_SECONDS
            + MARGIN_S
            + WARM_UP_LIMIT_S
            + SIGNAL_WAIT_S
            + STALL_S;
        let mic = match rig.open_microphone(room) {
            Ok(mic) => mic,
            Err(error) => return self.fail(rig, error.into(), now_ns),
        };
        let heard = mic.id();
        if let Some(failure) = microphone_check(
            self.first_microphone.as_ref(),
            &heard,
            self.largest_hold_back_ms,
        ) {
            drop(mic);
            return self.fail(rig, failure, now_ns);
        }
        self.log.log(|| {
            format!(
                "calibration: {which:?} run, only {solo} heard, recording at {} Hz",
                heard.rate
            )
        });
        self.first_microphone.get_or_insert(heard);
        self.stage = Stage::Warming {
            which,
            mic,
            since_ns: now_ns,
        };
        self.phase = Phase::Running(Step::Measuring(which));
    }

    /// Every output is heard again while the core estimates on a thread of
    /// its own: two recordings of a few seconds take tens of ms.
    fn estimate(&mut self, rig: &mut dyn Rig, chosen: Vec<f32>, rate: u32, now_ns: u64) {
        rig.set_solo(None);
        let Some((reference, reference_rate)) = self.reference_take.take() else {
            return self.fail(rig, Failure::Unfinished, now_ns);
        };
        if reference_rate != rate {
            return self.fail(rig, Failure::MicrophoneChanged, now_ns);
        }
        let Some(signal) = calibrate::signal(rate) else {
            return self.fail(rig, Failure::UnsupportedRate, now_ns);
        };
        let old = self.old_offset_ms;
        let (send, receive) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("audiosync-calibration".into())
            .spawn(move || {
                let _ = send.send(calibrate::estimate(&signal, &reference, &chosen, rate, old));
            });
        if spawned.is_err() {
            return self.fail(rig, Failure::Unfinished, now_ns);
        }
        self.stage = Stage::Estimating(receive);
        self.phase = Phase::Running(Step::Estimating);
    }

    fn collect(&mut self, rig: &mut dyn Rig, now_ns: u64) {
        let Stage::Estimating(receive) = &self.stage else {
            return;
        };
        let result = match receive.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                return self.fail(rig, Failure::Unfinished, now_ns);
            }
        };
        // The offset moved while the estimate ran: the result is for the
        // old one.
        if rig.offset_ms(&self.chosen) != Some(self.old_offset_ms) {
            return self.fail(rig, Failure::Interrupted, now_ns);
        }
        match &result {
            Ok(found) => self.log.log(|| {
                format!(
                    "calibration of {}: reference at {:.2} ms, chosen at {:.2} ms, difference \
                     {:.2} ms, offset {} -> {} ms{}",
                    self.chosen,
                    found.reference_ms,
                    found.chosen_ms,
                    found.difference_ms,
                    self.old_offset_ms,
                    found.offset_ms,
                    if found.clamped { " (clamped)" } else { "" }
                )
            }),
            Err(refused) => self.log.log(|| {
                format!(
                    "calibration of {} refused by the estimate: {:?} ({:?})",
                    self.chosen, refused.reason, refused.recording
                )
            }),
        }
        match outcome(&result) {
            Outcome::Commit { offset_ms, clamped } => {
                rig.set_offset(&self.chosen, offset_ms);
                self.end(rig, Phase::Done { offset_ms, clamped }, now_ns);
            }
            Outcome::Refuse(failure) => self.end(rig, Phase::Failed(failure), now_ns),
        }
    }

    fn fail(&mut self, rig: &mut dyn Rig, failure: Failure, now_ns: u64) {
        if !self.phase.is_running() {
            return;
        }
        self.log
            .log(|| format!("calibration of {} failed: {failure:?}", self.chosen));
        self.end(rig, Phase::Failed(failure), now_ns);
    }

    /// The row shows `phase` at once. A sweep cut off on its way fades out
    /// and plays out through the hold-backs on the one output heard before
    /// every output is heard again and the music fades in; otherwise that
    /// happens now.
    fn end(&mut self, rig: &mut dyn Rig, phase: Phase, now_ns: u64) {
        self.phase = phase;
        self.reference_take = None;
        let in_flight = matches!(self.stage, Stage::Playing { .. } | Stage::Recording { .. });
        // Dropping the stage stops its microphone.
        self.stage = Stage::Over;
        let engine = rig.engine();
        let largest = engine
            .hold_backs_ms
            .values()
            .copied()
            .filter(|ms| ms.is_finite())
            .fold(self.largest_hold_back_ms, f64::max);
        let wait = settle_seconds(in_flight, engine.session == Some(self.session), largest);
        if in_flight {
            self.tap.fade_out_signal();
        }
        if wait > 0.0 {
            self.log.log(|| {
                format!("calibration: the cut-off sweep plays out for {wait} s before the outputs return")
            });
            self.stage = Stage::Settling {
                until_ns: now_ns + ns(wait),
            };
        } else {
            self.give_back(rig);
        }
    }

    /// Every output heard again, the music fading back in and the signal
    /// out of the ring.
    fn give_back(&mut self, rig: &mut dyn Rig) {
        self.stage = Stage::Over;
        rig.set_solo(None);
        self.tap.disengage();
    }
}

impl Drop for CalibrationRun {
    fn drop(&mut self) {
        if self.holds_outputs() {
            self.tap.disengage();
        }
    }
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    const MIC_RATE: u32 = 44_100;
    const TICK_NS: u64 = 20_000_000;

    /// What the fake room does with the sweep: each output is heard this
    /// long after the signal entered the ring.
    struct Room {
        heard_after_ms: BTreeMap<String, f64>,
        /// The microphone hears only noise.
        deaf: bool,
    }

    struct FakeMic {
        id: MicrophoneId,
        first_ns: u64,
        now: Rc<Cell<u64>>,
        tap: Arc<CalibrationTap>,
        heard_after_ms: Option<f64>,
        deaf: bool,
        gap_at: Option<usize>,
    }

    impl Microphone for FakeMic {
        fn id(&self) -> MicrophoneId {
            self.id.clone()
        }

        fn first_frame_ns(&self) -> Option<u64> {
            (self.now.get() > self.first_ns).then_some(self.first_ns)
        }

        fn frames_recorded(&self) -> usize {
            let since = self.now.get().saturating_sub(self.first_ns);
            (since as f64 * f64::from(self.id.rate) / 1e9) as usize
        }

        fn gap_at(&self) -> Option<usize> {
            self.gap_at.filter(|gap| *gap < self.frames_recorded())
        }

        fn take(&mut self, start: usize, count: usize) -> Vec<f32> {
            let count = count.min(self.frames_recorded().saturating_sub(start));
            let rate = self.id.rate;
            let mut noise = 0x1234_5678_u32;
            let mut recording: Vec<f32> = (0..count)
                .map(|_| {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    (f64::from(noise) / f64::from(u32::MAX) - 0.5) as f32 * 0.001
                })
                .collect();
            if self.deaf {
                return recording;
            }
            let (Some(played), Some(after)) = (self.tap.played_at_ns(), self.heard_after_ms) else {
                return recording;
            };
            let signal = calibrate::signal(rate).unwrap();
            let heard_ns = played + ns(after / 1000.0);
            let at = ((heard_ns - self.first_ns) as f64 * f64::from(rate) / 1e9).round() as usize;
            for (i, sample) in signal.iter().enumerate() {
                if let Some(slot) = (at + i)
                    .checked_sub(start)
                    .and_then(|j| recording.get_mut(j))
                {
                    *slot += sample * 0.4;
                }
            }
            recording
        }
    }

    struct FakeRig {
        now: Rc<Cell<u64>>,
        engine: Engine,
        offsets: BTreeMap<String, f64>,
        tap: Arc<CalibrationTap>,
        solo: Option<String>,
        room: Room,
        mic: Result<MicrophoneId, MicrophoneError>,
        /// Where each recording loses audio.
        mic_gap_at: Option<usize>,
        set_offsets: RefCell<Vec<(String, f64)>>,
    }

    impl FakeRig {
        fn new() -> FakeRig {
            let ids = ["builtin", "usb"];
            FakeRig {
                now: Rc::new(Cell::new(1_000_000_000)),
                engine: Engine {
                    session: Some(1),
                    playing: ids.iter().map(|id| (*id).to_owned()).collect(),
                    reference: Some("builtin".to_owned()),
                    hold_backs_ms: BTreeMap::from([
                        ("builtin".to_owned(), 300.0),
                        ("usb".to_owned(), 0.0),
                    ]),
                },
                offsets: BTreeMap::from([("builtin".to_owned(), 0.0), ("usb".to_owned(), 300.0)]),
                tap: Arc::new(CalibrationTap::new()),
                solo: None,
                room: Room {
                    // USB is held back 0 ms and heard 40 ms after the ring,
                    // the reference held back 300 ms and heard 310 ms after.
                    heard_after_ms: BTreeMap::from([
                        ("builtin".to_owned(), 310.0),
                        ("usb".to_owned(), 40.0),
                    ]),
                    deaf: false,
                },
                mic: Ok(MicrophoneId {
                    device: "mic".to_owned(),
                    rate: MIC_RATE,
                }),
                mic_gap_at: None,
                set_offsets: RefCell::new(Vec::new()),
            }
        }

        /// One capture block of 10 ms through the tap, as the converter
        /// writes it.
        fn capture_block(&self) {
            let mut block = [0.0_f32; 960];
            let mut cursor = self.tap.block_cursor();
            let starts = cursor == 0;
            self.tap.fade_music(&mut block);
            self.tap.add_signal(&mut block, &mut cursor);
            if starts {
                self.tap.signal_started(self.now.get(), 480);
            }
        }
    }

    impl Rig for FakeRig {
        fn engine(&self) -> Engine {
            self.engine.clone()
        }

        fn offset_ms(&self, id: &str) -> Option<f64> {
            self.offsets.get(id).copied()
        }

        fn tap(&self) -> Option<Arc<CalibrationTap>> {
            Some(Arc::clone(&self.tap))
        }

        fn set_solo(&mut self, id: Option<&str>) {
            self.solo = id.map(str::to_owned);
        }

        fn open_microphone(
            &mut self,
            _seconds: f64,
        ) -> Result<Box<dyn Microphone>, MicrophoneError> {
            let id = self.mic.clone()?;
            let heard_after_ms = self
                .solo
                .as_ref()
                .and_then(|solo| self.room.heard_after_ms.get(solo).copied());
            Ok(Box::new(FakeMic {
                id,
                first_ns: self.now.get() + 30_000_000,
                now: Rc::clone(&self.now),
                tap: Arc::clone(&self.tap),
                heard_after_ms,
                deaf: self.room.deaf,
                gap_at: self.mic_gap_at,
            }))
        }

        fn set_offset(&mut self, id: &str, ms: f64) {
            self.offsets.insert(id.to_owned(), ms);
            self.set_offsets.borrow_mut().push((id.to_owned(), ms));
        }
    }

    /// Ticks until the run gave the outputs back, at most a minute of fake
    /// time; `meddle` runs before each tick.
    fn run_to_end(
        rig: &mut FakeRig,
        run: &mut CalibrationRun,
        mut meddle: impl FnMut(&mut FakeRig, &mut CalibrationRun),
    ) {
        for _ in 0..3000 {
            if !run.holds_outputs() {
                return;
            }
            meddle(rig, &mut *run);
            rig.now.set(rig.now.get() + TICK_NS);
            rig.capture_block();
            if matches!(run.phase(), Phase::Running(Step::Estimating)) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let now = rig.now.get();
            run.tick(rig, now);
        }
        panic!("the run never ended: {:?}", run.phase());
    }

    fn start(rig: &mut FakeRig) -> CalibrationRun {
        let now = rig.now.get();
        CalibrationRun::start(rig, "usb", now, DebugLog::default()).expect("it starts")
    }

    #[test]
    fn a_recorded_sweep_sets_the_offset_from_the_difference() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |_, _| {});
        // Heard 270 ms before the reference: held back 270 ms more.
        let Phase::Done { offset_ms, clamped } = run.phase() else {
            panic!("not done: {:?}", run.phase());
        };
        assert!((offset_ms - 570.0).abs() <= 1.0, "offset {offset_ms}");
        assert!(!clamped);
        assert_eq!(rig.offsets["usb"], offset_ms);
        assert_eq!(rig.solo, None, "every output is heard again");
        assert!(
            !rig.tap.engaged.load(Ordering::Relaxed),
            "the music is back"
        );
    }

    #[test]
    fn each_run_hears_only_its_own_output() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut solos = Vec::new();
        run_to_end(&mut rig, &mut run, |rig, run| {
            if let Phase::Running(Step::Measuring(which)) = run.phase() {
                solos.push((which, rig.solo.clone()));
            }
        });
        assert!(solos.contains(&(Which::Reference, Some("builtin".to_owned()))));
        assert!(solos.contains(&(Which::Chosen, Some("usb".to_owned()))));
        assert!(solos.iter().all(|(which, solo)| match which {
            Which::Reference => solo.as_deref() == Some("builtin"),
            Which::Chosen => solo.as_deref() == Some("usb"),
        }));
    }

    #[test]
    fn an_uncertain_measurement_changes_nothing() {
        let mut rig = FakeRig::new();
        rig.room.deaf = true;
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |_, _| {});
        assert_eq!(
            run.phase(),
            Phase::Failed(Failure::TooQuiet(Some(Which::Reference)))
        );
        assert_eq!(rig.offsets["usb"], 300.0);
        assert!(rig.set_offsets.borrow().is_empty());
        assert_eq!(rig.solo, None);
    }

    #[test]
    fn a_speaker_turned_off_mid_run_cancels_it() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |rig, run| {
            if run.phase() == Phase::Running(Step::Measuring(Which::Chosen)) {
                rig.engine.playing.retain(|id| id != "usb");
            }
        });
        assert_eq!(run.phase(), Phase::Failed(Failure::Interrupted));
        assert!(rig.set_offsets.borrow().is_empty());
        assert_eq!(rig.solo, None);
        assert!(!rig.tap.engaged.load(Ordering::Relaxed));
    }

    #[test]
    fn a_source_switched_mid_run_cancels_it() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |rig, run| {
            if run.phase() == Phase::Running(Step::Measuring(Which::Reference)) {
                rig.engine.session = Some(2);
            }
        });
        assert_eq!(run.phase(), Phase::Failed(Failure::Interrupted));
        assert!(rig.set_offsets.borrow().is_empty());
    }

    #[test]
    fn a_speaker_added_mid_run_cancels_it() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |rig, run| {
            if run.phase() == Phase::Running(Step::Measuring(Which::Chosen)) {
                rig.engine.playing.push("hdmi".to_owned());
            }
        });
        assert_eq!(run.phase(), Phase::Failed(Failure::Interrupted));
        assert!(rig.set_offsets.borrow().is_empty());
    }

    #[test]
    fn an_offset_typed_during_the_estimate_wins() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |rig, run| {
            if run.phase() == Phase::Running(Step::Estimating) {
                rig.offsets.insert("usb".to_owned(), 150.0);
            }
        });
        assert_eq!(run.phase(), Phase::Failed(Failure::Interrupted));
        assert_eq!(rig.offsets["usb"], 150.0);
        assert!(rig.set_offsets.borrow().is_empty());
    }

    #[test]
    fn a_cancelled_sweep_fades_and_plays_out_before_the_outputs_return() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let mut cancelled_at = None;
        run_to_end(&mut rig, &mut run, |rig, run| {
            if cancelled_at.is_none() && rig.tap.played_at_ns().is_some() {
                let now = rig.now.get();
                run.cancel(rig, now);
                cancelled_at = Some(now);
                assert_eq!(run.phase(), Phase::Cancelled);
                assert!(run.holds_outputs(), "the sweep plays out first");
                assert_eq!(rig.solo.as_deref(), Some("builtin"));
                assert!(rig.tap.stopping.load(Ordering::Relaxed));
            }
        });
        let held = rig.now.get() - cancelled_at.expect("cancelled");
        assert!(held >= ns(quiet_seconds(300.0)), "held {held} ns");
        assert_eq!(rig.solo, None);
        assert!(rig.set_offsets.borrow().is_empty());
    }

    #[test]
    fn a_cancel_before_the_sweep_gives_the_outputs_back_at_once() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        let now = rig.now.get();
        run.cancel(&mut rig, now);
        assert_eq!(run.phase(), Phase::Cancelled);
        assert!(!run.holds_outputs());
        assert!(!rig.tap.engaged.load(Ordering::Relaxed));
    }

    #[test]
    fn a_blocked_microphone_refuses_before_the_music_fades() {
        let mut rig = FakeRig::new();
        rig.mic = Err(MicrophoneError::Denied);
        let now = rig.now.get();
        let refused = CalibrationRun::start(&mut rig, "usb", now, DebugLog::default());
        assert_eq!(refused.err(), Some(Failure::MicrophoneDenied));
        assert!(!rig.tap.engaged.load(Ordering::Relaxed));
        rig.mic = Err(MicrophoneError::Missing);
        let refused = CalibrationRun::start(&mut rig, "usb", now, DebugLog::default());
        assert_eq!(refused.err(), Some(Failure::NoMicrophone));
        rig.mic = Err(MicrophoneError::HandsFree);
        let refused = CalibrationRun::start(&mut rig, "usb", now, DebugLog::default());
        assert_eq!(refused.err(), Some(Failure::HandsFreeMicrophone));
        assert!(!rig.tap.engaged.load(Ordering::Relaxed));
    }

    #[test]
    fn a_microphone_that_loses_audio_while_recording_changes_nothing() {
        let mut rig = FakeRig::new();
        rig.mic_gap_at = Some(MIC_RATE as usize / 2);
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |_, _| {});
        assert_eq!(run.phase(), Phase::Failed(Failure::MicrophoneSkipped));
        assert!(rig.set_offsets.borrow().is_empty());
        assert_eq!(rig.solo, None);
    }

    #[test]
    fn another_microphone_for_the_second_run_changes_nothing() {
        let mut rig = FakeRig::new();
        let mut run = start(&mut rig);
        run_to_end(&mut rig, &mut run, |rig, run| {
            if run.phase() == Phase::Running(Step::Measuring(Which::Reference)) {
                rig.mic = Ok(MicrophoneId {
                    device: "webcam".to_owned(),
                    rate: MIC_RATE,
                });
            }
        });
        assert_eq!(run.phase(), Phase::Failed(Failure::MicrophoneChanged));
        assert!(rig.set_offsets.borrow().is_empty());
    }

    #[test]
    fn the_first_frame_is_timed_from_the_first_packet_the_device_vouches_for() {
        let rate = 48_000;
        let mut clock = RecordingClock::default();
        // The stream's first packet: its discontinuity marks the start.
        clock.packet(0, rate, 1_000_000_000, Some(5_000_000), true, true);
        assert_eq!(clock, RecordingClock::default(), "a timestamp error");
        clock.packet(480, rate, 1_010_000_000, None, false, false);
        assert_eq!(clock.first_frame_ns, None, "no position");
        // 960 frames before it, recorded 4 ms ago: 20 ms more before that.
        clock.packet(960, rate, 1_020_000_000, Some(4_000_000), false, false);
        assert_eq!(clock.first_frame_ns, Some(996_000_000));
        clock.packet(1440, rate, 1_030_000_000, Some(1_000_000), false, true);
        assert_eq!(clock.first_frame_ns, Some(996_000_000), "kept");
        assert_eq!(clock.gap_at, Some(1440));
        clock.packet(1920, rate, 1_040_000_000, Some(1_000_000), false, true);
        assert_eq!(clock.gap_at, Some(1440), "the first gap");
    }

    #[test]
    fn a_calibration_needs_streaming_the_speaker_and_the_reference() {
        let mut engine = FakeRig::new().engine;
        assert_eq!(blocker(&engine, "usb"), None);
        assert_eq!(blocker(&engine, "builtin"), Some(Failure::NoReference));
        engine.playing.retain(|id| id != "builtin");
        assert_eq!(blocker(&engine, "usb"), Some(Failure::NotPlaying));
        engine.session = None;
        assert_eq!(blocker(&engine, "usb"), Some(Failure::NotPlaying));
        engine.reference = None;
        assert_eq!(blocker(&engine, "usb"), Some(Failure::NoReference));
        assert_eq!(unavailable(true, false, true), Some(Failure::NoReference));
        assert_eq!(unavailable(false, true, true), Some(Failure::NotPlaying));
        assert_eq!(unavailable(true, true, false), Some(Failure::NotPlaying));
        assert_eq!(unavailable(true, true, true), None);
    }

    #[test]
    fn the_chosen_run_must_use_the_references_microphone_and_a_rate_that_fits() {
        let mic = MicrophoneId {
            device: "mic".to_owned(),
            rate: 48_000,
        };
        assert_eq!(microphone_check(None, &mic, 2000.0), None);
        let other = MicrophoneId {
            device: "other".to_owned(),
            rate: 48_000,
        };
        assert_eq!(
            microphone_check(Some(&mic), &other, 2000.0),
            Some(Failure::MicrophoneChanged)
        );
        let fast = MicrophoneId {
            device: "mic".to_owned(),
            rate: 192_000,
        };
        assert_eq!(
            microphone_check(None, &fast, 20_000.0),
            Some(Failure::TooLong(None))
        );
        let slow = MicrophoneId {
            device: "mic".to_owned(),
            rate: 16_000,
        };
        assert_eq!(
            microphone_check(None, &slow, 0.0),
            Some(Failure::UnsupportedRate)
        );
    }

    #[test]
    fn a_recording_starts_where_the_signal_entered_the_ring() {
        assert_eq!(
            trim_start(1_500_000_000, 1_000_000_000, 48_000),
            Some(24_000)
        );
        assert_eq!(trim_start(1_000_000_000, 1_500_000_000, 48_000), None);
        assert_eq!(
            recording_frames(2000.0, 48_000),
            ((2.0 + 0.5 + 0.75 + 0.5) * 48_000.0) as usize
        );
        assert_eq!(settle_seconds(false, true, 2000.0), 0.0);
        assert_eq!(settle_seconds(true, false, 2000.0), 0.0);
        assert_eq!(settle_seconds(true, true, 2000.0), 2.5);
    }

    #[test]
    fn a_refusal_names_the_recording_it_concerns() {
        let refused = |reason, recording| outcome(&Err(Refused { reason, recording }));
        assert_eq!(
            refused(Refusal::Clipped, Some(Recording::Chosen)),
            Outcome::Refuse(Failure::Clipped(Some(Which::Chosen)))
        );
        assert_eq!(
            refused(Refusal::Implausible, None),
            Outcome::Refuse(Failure::Implausible)
        );
    }

    #[test]
    fn a_shown_result_goes_once_its_context_moves_or_the_offset_is_set_by_hand() {
        let context = Context {
            streaming: true,
            reference: Some("builtin".to_owned()),
            speaker_on: true,
        };
        let status = Status {
            id: Some("usb".to_owned()),
            phase: Phase::Done {
                offset_ms: 120.0,
                clamped: false,
            },
            context: Some(context.clone()),
            holds_outputs: false,
        };
        assert_eq!(status.following(&context), status);
        let off = Context {
            speaker_on: false,
            ..context
        };
        assert_eq!(status.following(&off).phase, Phase::Idle);
        assert_eq!(status.offset_changed("usb").phase, Phase::Idle);
        assert_eq!(status.offset_changed("hdmi"), status);
        let running = Status {
            phase: Phase::Running(Step::Preparing),
            ..status
        };
        assert_eq!(running.following(&off), running);
    }

    #[test]
    fn the_tap_fades_the_music_out_and_adds_the_signal_from_a_block_start() {
        let tap = CalibrationTap::new();
        let mut block = vec![1.0_f32; 2 * FADE_FRAMES * 2];
        tap.engage();
        tap.fade_music(&mut block);
        assert!(block[0] > 0.99, "the fade starts at full");
        assert!(block[2 * FADE_FRAMES..].iter().all(|s| *s == 0.0));
        tap.play();
        let mut cursor = tap.block_cursor();
        assert_eq!(cursor, 0);
        let mut silent = vec![0.0_f32; 960];
        tap.add_signal(&mut silent, &mut cursor);
        tap.signal_started(1_000_000_000, 480);
        assert_eq!(cursor, 480);
        assert_eq!(tap.played_at_ns(), Some(1_000_000_000 - 10_000_000));
        let signal = calibrate::signal(SESSION_RATE).unwrap();
        assert_eq!(silent[2 * 100], signal[100]);
        assert_eq!(silent[2 * 100 + 1], signal[100]);
        // A cut-off signal fades to nothing over FADE_FRAMES.
        tap.fade_out_signal();
        let mut rest = vec![0.0_f32; 4 * FADE_FRAMES];
        let mut cursor = tap.block_cursor();
        tap.add_signal(&mut rest, &mut cursor);
        assert!(rest[2 * FADE_FRAMES..].iter().all(|s| *s == 0.0));
        assert_eq!(tap.block_cursor(), IDLE);
    }

    #[test]
    fn a_play_from_the_ui_thread_mid_block_waits_for_the_next_block() {
        let tap = CalibrationTap::new();
        let mut cursor = tap.block_cursor();
        tap.play();
        let mut block = vec![0.0_f32; 960];
        tap.add_signal(&mut block, &mut cursor);
        assert!(block.iter().all(|s| *s == 0.0));
        assert_eq!(tap.block_cursor(), 0, "the next block starts it");
    }
}
