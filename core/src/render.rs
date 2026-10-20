//! A local output's renderer (S62 to S65) with its fades (S183 to S188).
//!
//! Per device callback a [`Renderer`] reads capture-rate frames through its
//! [`Reader`] (at the output's delay, with drift corrections held while it
//! plays, S188), converts them to the device's rate, sizing each read from
//! what the resampler needs (S62), maps stereo onto the device's channels
//! (S63), applies the output's gain, ramped over one callback when it changes
//! (S64, S65), and then its fade at the device rate (S183). It works in
//! pieces of at most [`RENDER_CHUNK`] device frames, one read each.
//!
//! Callbacks against the capture's blocks (S206): at the start of every
//! callback the renderer gives its reader a lead, a floor of the effective
//! delay. It counts the device's steady callback (the period the host gives,
//! or else the largest of the last [`CALLBACK_WINDOW`] callbacks) or this
//! callback when it is larger, and with rate conversion what that callback
//! can ask the reader for plus the capture's block (at most
//! [`LEAD_BLOCK_CAP_MS`]), since a converting output's callbacks never line
//! up with the capture's blocks. Only the first read of a callback may
//! correct drift; its later reads are held and what they report as due is
//! ignored. A converting renderer whose reader holds fewer frames than a
//! callback needs (its device clock runs fast and the lead's slack is used
//! up) slows its converter over that callback by the shortage, up to
//! 1 / [`SLOW_DOWN_SHARE`] of the callback's reads, instead of letting the
//! reader run dry.
//!
//! Clock stamps (S207): when the host gives each callback the time it reads
//! at ([`Renderer::render_at`]) and stamps the capture's blocks on the same
//! clock, a converting renderer whose reader plays within the dead band of
//! its lead follows the capture's clock by those times. It estimates where
//! the capture is now from the newest stamp, takes the fill `Ŵ - P` at the
//! start of every callback, keeps the average over the first
//! [`TRACK_MS`] (or [`TRACK_CALLBACKS`] callbacks, when longer) after the
//! reader anchors as its reference, and trims its converter by a frame or
//! so whenever a running average of the fill (the same time constant)
//! strays more than [`TRIM_DEAD_ZONE_FRAMES`] from it: slowed when the
//! device's clock runs fast, sped up when it runs slow,
//! at most 1 / [`TRIM_SHARE`] of a callback's reads at a time.
//!
//! The fade (S184 to S188):
//!
//! - The renderer starts closed. A frame its reader fills with silence closes
//!   it at once; the first frame the reader copies while it is closed starts
//!   a fade-in, at that frame or, with rate conversion, at the device frame
//!   that the frame's position converts to.
//! - [`RendererControl::request_fade_out`] (an output being stopped) fades out
//!   over the next F device frames from the first callback that begins after
//!   the request, then renders exact zeros until the output stops. The
//!   renderer then publishes the output time of the fade's last frame, when
//!   the host gives each callback's output time (the Mac), and counts the
//!   frames it rendered after that frame, which the host compares with what
//!   its endpoint still holds (Windows, [`endpoint_faded`]).
//! - [`RendererControl::set_delay_frames`] while the renderer plays fades out
//!   first; only after the fade-out's last frame does the reader get the new
//!   delay and its resync request, so it still lands exactly on the delay
//!   (S46). A newer change during the fade-out replaces the waiting one; a
//!   change while closed goes to the reader at once.
//! - A read that would make a drift correction holds it and reports it due;
//!   the renderer fades out from the start of that read's block, and its
//!   first read after the fade-out is made without the hold.
//! - After a fade-out for a stop, a delay change or a held correction, the
//!   renderer stays closed, copied frames included, until the read that
//!   applies the change (a stop: until the output stops).
//!
//! Real time (S190): every buffer is allocated by [`Renderer::new`];
//! [`Renderer::render`] never allocates, locks, blocks or logs. The control
//! side's requests are atomics that the renderer takes at the start of a
//! callback; a delay change stores its value before its flag.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use crate::convert::map_to_device;
use crate::fade::{Fade, FadeState};
use crate::reader::{Correction, NextRead, ReadReport, Reader, ReaderControl, ReaderCounters};
use crate::resample::Resampler;
use crate::volume::{GainStage, SharedGain};

/// Device frames rendered per read; bounds the renderer's buffers.
pub const RENDER_CHUNK: usize = 512;

/// The longest a host waits for its outputs' fade-outs at a stop or a
/// removal before it stops them anyway (S186).
pub const STOP_FADE_CAP: Duration = Duration::from_millis(500);

/// No output time published yet.
const NO_TIME: u64 = u64::MAX;

/// The callbacks a renderer looks back over for its steady callback size
/// when the host gives no period (S206): a larger one is forgotten that many
/// callbacks later.
pub const CALLBACK_WINDOW: usize = 8;

/// The most of the capture's block a converting renderer's lead counts, in
/// ms (S206): more than the capture's blocks (10 ms packets on Windows, 512
/// or 1024 frames on the Mac), about the first silence a Windows loopback
/// gap writes, and well inside the readers' 35 ms dead band, so silence
/// written at once after a stall never raises the lead by more than that.
pub const LEAD_BLOCK_CAP_MS: u32 = 25;

/// A converting renderer slows its converter by at most this share of a
/// callback's reads, a sixteenth (S206): a reader that runs shorter than
/// that over one callback isn't kept off W by a fast clock but by a
/// stopped writer, and runs dry as S53 says.
pub const SLOW_DOWN_SHARE: usize = 16;

/// A renderer that follows the capture's clock (S207) averages the fill
/// over this many ms of device frames after its reader anchors for its
/// reference, and moves the running average of the fill with this time
/// constant, or over [`TRACK_CALLBACKS`] callbacks when those last longer.
pub const TRACK_MS: u32 = 500;

/// The fewest callbacks the tracking averages over (S207): with large
/// callbacks, half a second holds only a few, and the jitter of the times
/// the host takes would move the average past the dead zone both ways.
pub const TRACK_CALLBACKS: usize = 16;

/// How far the running average of the fill may stray from the reference,
/// in capture frames, before the converter is trimmed (S207).
pub const TRIM_DEAD_ZONE_FRAMES: f64 = 4.0;

/// A trim takes at most this share of a callback's reads, a 256th, and at
/// least one frame (S207).
pub const TRIM_SHARE: usize = 256;

/// A stamp older than this, in ms, or than [`STALE_STAMP_BLOCKS`] of the
/// capture's blocks, whichever is more, means the capture stalled: the
/// tracking restarts (S207).
pub const STALE_STAMP_MS: u64 = 100;

/// See [`STALE_STAMP_MS`].
pub const STALE_STAMP_BLOCKS: u64 = 4;

/// Where a renderer that follows the capture's clock stands (S207).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tracking {
    /// Measuring the reference since the tracking (re)started: the fills
    /// taken so far, summed, their count, and the device frames of their
    /// callbacks.
    Measuring { sum: f64, count: u32, frames: u64 },
    /// Following: the fill the renderer keeps, and the running average.
    Following { reference: f64, average: f64 },
}

/// The tracking right after it (re)starts.
const RESTART: Tracking = Tracking::Measuring {
    sum: 0.0,
    count: 0,
    frames: 0,
};

/// The most capture-rate frames the resampler can ask for to produce
/// `device_frames` frames at `device_rate`, whatever state it is in: the
/// frames those outputs span, plus one step and the rounding on each side.
pub fn reader_frames_bound(device_frames: usize, capture_rate: u32, device_rate: u32) -> usize {
    if device_rate == capture_rate {
        return device_frames;
    }
    let frames = device_frames as u64;
    let from = u64::from(capture_rate.max(1));
    let to = u64::from(device_rate.max(1));
    ((frames * from).div_ceil(to) + from / to + 4) as usize
}

/// Whether an endpoint has played a renderer's fade-out (S185, Windows): the
/// renderer wrote `frames_after_fade` frames after the fade's last frame,
/// the endpoint still holds `padding` frames, so only zeros are queued once
/// `padding` is no larger than that count, and the frames it played since
/// cover the stream latency, `latency_frames` at the device rate.
pub fn endpoint_faded(frames_after_fade: u64, padding: u64, latency_frames: u64) -> bool {
    frames_after_fade >= padding.saturating_add(latency_frames)
}

/// Whether a fade-out whose last frame is output at `fade_end_ns` has played
/// once the device's and stream's latency, `latency_ns`, is added (S185, the
/// Mac): the host clock reads `now_ns`, in the same nanoseconds.
pub fn faded_by_time(fade_end_ns: u64, latency_ns: u64, now_ns: u64) -> bool {
    now_ns >= fade_end_ns.saturating_add(latency_ns)
}

/// Why a renderer couldn't be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The device has no channels.
    NoChannels,
    /// The resampler doesn't support the capture or the device rate (S16).
    UnsupportedRate,
    /// The control was made for another reader.
    ForeignControl,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RenderError::NoChannels => "the device has no channels",
            RenderError::UnsupportedRate => "the rate is not supported",
            RenderError::ForeignControl => "the control belongs to another reader",
        })
    }
}

impl std::error::Error for RenderError {}

/// State shared between a renderer and the control thread.
#[derive(Debug)]
struct Shared {
    /// A stop's fade-out was asked for.
    fade_out: AtomicBool,
    /// The newest delay asked for, stored before `delay_flag`.
    delay: AtomicI64,
    delay_flag: AtomicBool,
    /// The output time of the stop's fade's last frame, [`NO_TIME`] until
    /// the renderer published it.
    fade_end: AtomicU64,
    /// The stop's fade-out has been rendered.
    fade_rendered: AtomicBool,
    /// The host counted the output as faded (Windows).
    faded: AtomicBool,
    /// Where the fade was at the end of the latest callback ([`state_code`]).
    state: AtomicU8,
    /// Device frames the resampler could not produce and that were played
    /// as silence. Stays 0 unless [`reader_frames_bound`] is wrong.
    shortfall_frames: AtomicU64,
    /// Capture frames the converter was slowed down by in all (S206,
    /// S207).
    slowed_frames: AtomicU64,
    /// Capture frames the converter was sped up by in all (S207).
    sped_frames: AtomicU64,
}

/// A fade state as the renderer publishes it.
fn state_code(state: FadeState) -> u8 {
    match state {
        FadeState::Closed => 0,
        FadeState::FadingIn => 1,
        FadeState::Open => 2,
        FadeState::FadingOut => 3,
    }
}

/// Takes a flag raised with release ordering.
fn take(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed) && flag.swap(false, Ordering::Acquire)
}

/// The control thread's handle to a [`Renderer`]: its delay (S187), its
/// fade-out before a stop (S185), whether that fade-out has played, the
/// reader's lag and counters, and the frames the renderer slowed its
/// converter by or couldn't convert. Cheap to clone; every method is a lock-free
/// atomic operation (S190).
#[derive(Debug, Clone)]
pub struct RendererControl {
    shared: Arc<Shared>,
    reader: ReaderControl,
}

impl RendererControl {
    /// A control for the renderer that `reader` will be given to. The delay
    /// starts at the reader's target.
    pub fn new(reader: &Reader) -> RendererControl {
        let reader = reader.control();
        RendererControl {
            shared: Arc::new(Shared {
                fade_out: AtomicBool::new(false),
                delay: AtomicI64::new(reader.delay_frames()),
                delay_flag: AtomicBool::new(false),
                fade_end: AtomicU64::new(NO_TIME),
                fade_rendered: AtomicBool::new(false),
                faded: AtomicBool::new(false),
                state: AtomicU8::new(state_code(FadeState::Closed)),
                shortfall_frames: AtomicU64::new(0),
                slowed_frames: AtomicU64::new(0),
                sped_frames: AtomicU64::new(0),
            }),
            reader,
        }
    }

    /// The reader's own control.
    pub fn reader(&self) -> &ReaderControl {
        &self.reader
    }

    /// Asks for a new delay in capture-rate frames (S187). A renderer that
    /// plays fades out first and hands the delay to its reader after the
    /// fade-out's last frame; one that is closed hands it on at its next
    /// callback. The value is stored before its flag.
    pub fn set_delay_frames(&self, frames: i64) {
        self.shared.delay.store(frames.max(0), Ordering::Relaxed);
        self.shared.delay_flag.store(true, Ordering::Release);
    }

    /// The newest delay asked for, in capture-rate frames, whether or not
    /// the reader has it yet.
    pub fn delay_frames(&self) -> i64 {
        self.shared.delay.load(Ordering::Relaxed)
    }

    /// Asks the renderer to fade out from its next callback and render exact
    /// zeros after it, before its output stops (S185). It stays closed until
    /// it stops.
    pub fn request_fade_out(&self) {
        self.shared.fade_out.store(true, Ordering::Release);
    }

    /// Whether a fade-out was asked for.
    pub fn fade_out_requested(&self) -> bool {
        self.shared.fade_out.load(Ordering::Relaxed)
    }

    /// Whether the renderer has rendered the requested fade-out's last frame.
    /// The device plays it later; [`RendererControl::is_faded`] and
    /// [`RendererControl::faded_by_time`] say when it has.
    pub fn fade_rendered(&self) -> bool {
        self.shared.fade_rendered.load(Ordering::Acquire)
    }

    /// The output time of the requested fade-out's last frame, in the host
    /// nanoseconds the callbacks were given; `None` until it was rendered,
    /// or when the callbacks had no output time.
    pub fn fade_end_time_ns(&self) -> Option<u64> {
        let end = self.shared.fade_end.load(Ordering::Acquire);
        (end != NO_TIME).then_some(end)
    }

    /// Whether the requested fade-out has played by `now_ns`, given the
    /// device's and stream's latency (S185, the Mac).
    pub fn faded_by_time(&self, latency_ns: u64, now_ns: u64) -> bool {
        self.fade_end_time_ns()
            .is_some_and(|end| faded_by_time(end, latency_ns, now_ns))
    }

    /// Whether the host's render thread counted the output as faded
    /// ([`Renderer::mark_faded`], S185 on Windows).
    pub fn is_faded(&self) -> bool {
        self.shared.faded.load(Ordering::Acquire)
    }

    /// Where the renderer's fade was at the end of its latest callback;
    /// closed before the first one. For diagnostics and checks: it may lag
    /// the renderer by one callback.
    pub fn fade_state(&self) -> FadeState {
        match self.shared.state.load(Ordering::Relaxed) {
            1 => FadeState::FadingIn,
            2 => FadeState::Open,
            3 => FadeState::FadingOut,
            _ => FadeState::Closed,
        }
    }

    /// The reader's last published `lagFrames` (S56).
    pub fn lag_frames(&self) -> i64 {
        self.reader.lag_frames()
    }

    /// The reader's counters (S158).
    pub fn counters(&self) -> ReaderCounters {
        self.reader.counters()
    }

    /// [`Renderer::slowed_frames`], from any thread (S206, S207).
    pub fn slowed_frames(&self) -> u64 {
        self.shared.slowed_frames.load(Ordering::Relaxed)
    }

    /// [`Renderer::sped_frames`], from any thread (S207).
    pub fn sped_frames(&self) -> u64 {
        self.shared.sped_frames.load(Ordering::Relaxed)
    }

    /// [`Renderer::shortfall_frames`], from any thread.
    pub fn shortfall_frames(&self) -> u64 {
        self.shared.shortfall_frames.load(Ordering::Relaxed)
    }
}

/// The stop's fade-out, as the renderer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Not asked for.
    None,
    /// Asked for and under way.
    Fading,
    /// Ended in this callback: its last frame is this callback's frame, or
    /// came before the callback (`None`).
    Ended(Option<usize>),
    /// Ended in an earlier callback.
    Done,
}

/// Where one read's block lies in its piece of device frames: silence before
/// `lead`, the reader's copies up to `copy_end`, silence after.
#[derive(Debug, Clone, Copy)]
struct Layout {
    lead: usize,
    copy_end: usize,
    copies: bool,
}

/// One output's renderer, owned by its output thread. All buffers are
/// allocated by [`Renderer::new`]; [`Renderer::render`] never allocates,
/// locks or blocks.
pub struct Renderer {
    reader: Reader,
    reader_control: ReaderControl,
    resampler: Resampler,
    stage: GainStage,
    gain: Arc<SharedGain>,
    shared: Arc<Shared>,
    channels: usize,
    capture_rate: u32,
    device_rate: u32,
    /// The device's period in device frames as the host gives it, 0 when it
    /// gives none (S206).
    period_frames: usize,
    /// The last [`CALLBACK_WINDOW`] callbacks, in device frames.
    callbacks: [usize; CALLBACK_WINDOW],
    /// Where in `callbacks` the next one goes.
    next_callback: usize,
    /// [`LEAD_BLOCK_CAP_MS`] in capture frames.
    block_cap: usize,
    /// Capture-rate stereo frames from the reader.
    stereo: Box<[f32]>,
    /// Device-rate stereo frames from the resampler.
    converted: Box<[f32]>,
    fade: Fade,
    stop: Stop,
    /// A delay change that waits for the fade-out (S187).
    delay_pending: Option<i64>,
    /// A delay went to the reader; closed until the read that applies it.
    awaiting_resync: bool,
    /// A held correction waits for the fade-out; closed until the first
    /// read after it (S188).
    correction_due: bool,
    /// Device frames rendered after the stop's fade's last frame.
    frames_after_fade: Option<u64>,
    /// Following the capture's clock by its stamps (S207).
    tracking: Tracking,
    /// [`TRACK_MS`] in device frames.
    track_frames: f64,
}

impl fmt::Debug for Renderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Renderer")
            .field("channels", &self.channels)
            .field("device_rate", &self.device_rate)
            .field("fade", &self.fade.state())
            .finish_non_exhaustive()
    }
}

impl Renderer {
    /// A renderer that reads through `reader`, controlled through `control`
    /// (made for this reader with [`RendererControl::new`]), and plays at the
    /// gain in `gain` on a device running at `device_rate` with `channels`
    /// interleaved float channels; the ring holds frames at `capture_rate`.
    pub fn new(
        reader: Reader,
        control: &RendererControl,
        gain: Arc<SharedGain>,
        capture_rate: u32,
        device_rate: u32,
        channels: usize,
    ) -> Result<Renderer, RenderError> {
        if channels == 0 {
            return Err(RenderError::NoChannels);
        }
        if !control.reader.controls(&reader) {
            return Err(RenderError::ForeignControl);
        }
        let resampler =
            Resampler::new(capture_rate, device_rate).map_err(|_| RenderError::UnsupportedRate)?;
        // A speed-up (S207) reads at most a 256th more, spread over the
        // callback, so a piece may ask for a frame or two above the bound.
        let bound = reader_frames_bound(RENDER_CHUNK, capture_rate, device_rate);
        let read_frames = bound + bound / TRIM_SHARE + 2;
        Ok(Renderer {
            reader,
            reader_control: control.reader.clone(),
            resampler,
            stage: GainStage::new(),
            gain,
            shared: Arc::clone(&control.shared),
            channels,
            capture_rate,
            device_rate,
            period_frames: 0,
            callbacks: [0; CALLBACK_WINDOW],
            next_callback: 0,
            block_cap: (u64::from(capture_rate) * u64::from(LEAD_BLOCK_CAP_MS) / 1000) as usize,
            stereo: vec![0.0; read_frames * 2].into_boxed_slice(),
            converted: vec![0.0; RENDER_CHUNK * 2].into_boxed_slice(),
            fade: Fade::new(device_rate),
            stop: Stop::None,
            delay_pending: None,
            awaiting_resync: false,
            correction_due: false,
            frames_after_fade: None,
            tracking: RESTART,
            track_frames: f64::from(device_rate) * f64::from(TRACK_MS) / 1000.0,
        })
    }

    /// Channels per device frame.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The device rate.
    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// F at the device rate.
    pub fn fade_len(&self) -> usize {
        self.fade.len()
    }

    /// Where the fade is.
    pub fn fade_state(&self) -> FadeState {
        self.fade.state()
    }

    /// Device frames rendered after the requested fade-out's last frame;
    /// `None` until it was rendered.
    pub fn frames_after_fade(&self) -> Option<u64> {
        self.frames_after_fade
    }

    /// Marks the output as faded, once its device has played the fade-out
    /// (S185): a release store the control reads with acquire.
    pub fn mark_faded(&self) {
        self.shared.faded.store(true, Ordering::Release);
    }

    /// Device frames played as silence because the resampler lacked input.
    pub fn shortfall_frames(&self) -> u64 {
        self.shared.shortfall_frames.load(Ordering::Relaxed)
    }

    /// Capture frames the converter was slowed down by, in all: to keep the
    /// reader off W (S206) and to follow the capture's clock (S207). Each
    /// one plays the output one capture frame later. Stays 0 without
    /// conversion.
    pub fn slowed_frames(&self) -> u64 {
        self.shared.slowed_frames.load(Ordering::Relaxed)
    }

    /// Capture frames the converter was sped up by to follow the capture's
    /// clock (S207), in all. Each one plays the output one capture frame
    /// earlier. Stays 0 without conversion or stamps.
    pub fn sped_frames(&self) -> u64 {
        self.shared.sped_frames.load(Ordering::Relaxed)
    }

    /// Sets the device's period, in device frames: the callback it asks for
    /// when it is on time (a Windows endpoint's period, which each wake
    /// refills). The lead then counts it, or a callback when that is larger,
    /// rather than the largest of the last callbacks (S206). 0, as at the
    /// start, means the host doesn't know it. Call it before the first
    /// callback, on the output's thread.
    pub fn set_period_frames(&mut self, frames: usize) {
        self.period_frames = frames;
    }

    /// The lead the reader got at the start of the last callback, in
    /// capture frames (S206); 0 before the first one.
    pub fn lead_frames(&self) -> i64 {
        self.reader.lead_frames()
    }

    /// Fills `out`, interleaved with [`Renderer::channels`] channels.
    pub fn render(&mut self, out: &mut [f32]) {
        self.render_inner(out, None, None);
    }

    /// [`Renderer::render`] for a callback whose first frame is output at
    /// `output_time_ns` host nanoseconds, so a fade-out's end can be
    /// published (S185, the Mac).
    pub fn render_timed(&mut self, out: &mut [f32], output_time_ns: u64) {
        self.render_inner(out, Some(output_time_ns), None);
    }

    /// [`Renderer::render`] for a callback that reads at `now_ns` on the
    /// monotonic clock the capture stamps its blocks with
    /// ([`crate::clock`]), so a converting renderer follows the capture's
    /// clock (S207).
    pub fn render_at(&mut self, out: &mut [f32], now_ns: u64) {
        self.render_inner(out, None, Some(now_ns));
    }

    /// [`Renderer::render_timed`] and [`Renderer::render_at`] at once: the
    /// callback's first frame is output at `output_time_ns` host
    /// nanoseconds, and it reads at `now_ns` on the stamps' clock.
    pub fn render_timed_at(&mut self, out: &mut [f32], output_time_ns: u64, now_ns: u64) {
        self.render_inner(out, Some(output_time_ns), Some(now_ns));
    }

    fn render_inner(&mut self, out: &mut [f32], time: Option<u64>, now: Option<u64>) {
        let channels = self.channels;
        let frames = out.len() / channels;
        self.update_lead(frames);
        self.take_requests();
        self.keep_reader_off_w(frames, now);
        let ramp = self.stage.begin(self.gain.get(), frames);
        let read_capacity = self.stereo.len() / 2;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(RENDER_CHUNK);
            let need = self.resampler.input_frames_for(n).min(read_capacity);
            // With no read, the fade carries on over the piece.
            let mut layout = Layout {
                lead: 0,
                copy_end: n,
                copies: false,
            };
            if need > 0 {
                let report = self.read(need, done);
                layout = self.layout(&report, need, n);
            }
            let (_, produced) = self
                .resampler
                .process(&self.stereo[..need * 2], &mut self.converted[..n * 2]);
            if produced < n {
                self.converted[produced * 2..n * 2].fill(0.0);
                self.shared
                    .shortfall_frames
                    .fetch_add((n - produced) as u64, Ordering::Relaxed);
            }
            let piece = &mut out[done * channels..(done + n) * channels];
            map_to_device(&self.converted[..n * 2], n, 1.0, piece, channels);
            ramp.apply(piece, channels, done);
            self.fade_piece(piece, layout, done);
            done += n;
        }
        // A trailing partial frame is never played; keep it silent.
        out[frames * channels..].fill(0.0);
        self.finish_callback(frames, time);
        self.shared
            .state
            .store(state_code(self.fade.state()), Ordering::Relaxed);
    }

    /// Gives the reader its lead Lr at the start of a callback of `frames`
    /// device frames (S206). The callback it counts is the device's steady
    /// one (its period when the host gave it, else the largest of the last
    /// [`CALLBACK_WINDOW`] callbacks, this one included) or this one when it
    /// is larger. Without conversion Lr is that callback; with it, the most
    /// that callback can ask the reader for plus the capture's block, at
    /// most [`LEAD_BLOCK_CAP_MS`].
    fn update_lead(&mut self, frames: usize) {
        self.callbacks[self.next_callback] = frames;
        self.next_callback = (self.next_callback + 1) % CALLBACK_WINDOW;
        let steady = if self.period_frames > 0 {
            self.period_frames
        } else {
            self.callbacks.iter().copied().fold(0, usize::max)
        };
        let callback = steady.max(frames);
        let lead = if self.resampler.is_passthrough() {
            callback
        } else {
            let block = self.reader.ring().block_frames().min(self.block_cap);
            reader_frames_bound(callback, self.capture_rate, self.device_rate).saturating_add(block)
        };
        self.reader
            .set_lead_frames(i64::try_from(lead).unwrap_or(i64::MAX));
    }

    /// Trims the converter over a callback of `frames` device frames that
    /// reads at `now` on the stamps' clock, when the reader would go on
    /// from where it is (S206, S207):
    ///
    /// - When it holds fewer frames than the callback needs, it is slowed
    ///   by that shortage, if that is at most 1 / [`SLOW_DOWN_SHARE`] of
    ///   the need (S206): a device clock that runs fast never runs the
    ///   reader dry. The output keeps playing that much later, so the
    ///   fill the renderer keeps moves along.
    /// - Otherwise, with a usable stamp, the running average of the fill
    ///   is compared with the reference, and the converter slowed or sped
    ///   up by what it strays beyond the dead zone (S207).
    ///
    /// Called after the control's requests are taken, so the first read's
    /// hold is known.
    fn keep_reader_off_w(&mut self, frames: usize, now: Option<u64>) {
        if self.resampler.is_passthrough() || self.resampler.is_trimming() || frames < 2 {
            return;
        }
        let first = self
            .resampler
            .input_frames_for(frames.min(RENDER_CHUNK))
            .min(self.stereo.len() / 2);
        let Some(next) = self.reader.next_read(first, !self.fade.is_closed()) else {
            self.tracking = RESTART;
            return;
        };
        let need = self.resampler.input_frames_for(frames);
        let trim = now.map_or(0, |now| self.follow(now, frames, need, &next));
        let short = need.saturating_sub(usize::try_from(next.readable).unwrap_or(0));
        if short > 0 && short <= need / SLOW_DOWN_SHARE {
            // S206's slow-down wins over a trim the other way; a larger
            // slow-down of the trim's own covers the shortage too.
            let own = usize::try_from(trim).unwrap_or(0);
            let slowed = short.max(own);
            if self.resampler.slow_down(slowed, frames - 1) {
                self.count_trim(slowed as f64, (slowed - own) as f64);
                self.shared
                    .slowed_frames
                    .fetch_add(slowed as u64, Ordering::Relaxed);
            }
            return;
        }
        let by = trim.unsigned_abs() as usize;
        if trim > 0 && self.resampler.slow_down(by, frames - 1) {
            self.count_trim(by as f64, 0.0);
            self.shared
                .slowed_frames
                .fetch_add(by as u64, Ordering::Relaxed);
        } else if trim < 0 && self.resampler.speed_up(by, frames - 1) {
            self.count_trim(-(by as f64), 0.0);
            self.shared
                .sped_frames
                .fetch_add(by as u64, Ordering::Relaxed);
        }
    }

    /// Follows the capture's clock at the start of a callback of `frames`
    /// device frames that reads at `now`, needs `need` capture frames and
    /// goes on as `next` says (S207). Returns the trim it asks for, in
    /// capture frames: positive to slow the converter, negative to speed it
    /// up, 0 for none. Restarts the tracking when the output is out of the
    /// trim's reach or the newest stamp is stale; a callback without a
    /// usable stamp changes nothing.
    fn follow(&mut self, now: u64, frames: usize, need: usize, next: &NextRead) -> i64 {
        let ring = self.reader.ring();
        let Some(stamp) = ring.stamp() else {
            return 0;
        };
        let rate = u64::from(self.capture_rate.max(1));
        let blocks_ns = STALE_STAMP_BLOCKS
            .saturating_mul(ring.block_frames() as u64)
            .saturating_mul(1_000_000_000)
            / rate;
        let stale_ns = (STALE_STAMP_MS * 1_000_000).max(blocks_ns);
        let reach = self.reader.lead_frames().saturating_add(next.tolerance);
        if now.saturating_sub(stamp.time_ns) > stale_ns || next.delay > reach {
            self.tracking = RESTART;
            return 0;
        }
        // Ŵ - P: the W of the stamp, plus the capture frames of the time
        // since it, less P.
        let since_ns = i128::from(now) - i128::from(stamp.time_ns);
        let since = since_ns as f64 * rate as f64 / 1e9;
        let fill = (stamp.written as i64 - next.position) as f64 + since;
        let track = self.track_frames.max((TRACK_CALLBACKS * frames) as f64);
        match self.tracking {
            Tracking::Measuring {
                sum,
                count,
                frames: span,
            } => {
                let (sum, count, span) = (sum + fill, count + 1, span + frames as u64);
                self.tracking = if span as f64 >= track {
                    let reference = sum / f64::from(count);
                    Tracking::Following {
                        reference,
                        average: reference,
                    }
                } else {
                    Tracking::Measuring {
                        sum,
                        count,
                        frames: span,
                    }
                };
                0
            }
            Tracking::Following { reference, average } => {
                let alpha = 1.0 - (-(frames as f64) / track).exp();
                let average = average + alpha * (fill - average);
                self.tracking = Tracking::Following { reference, average };
                let off = average - reference;
                if off.abs() <= TRIM_DEAD_ZONE_FRAMES {
                    return 0;
                }
                let most = (need / TRIM_SHARE).max(1) as f64;
                let by = (off.abs() - TRIM_DEAD_ZONE_FRAMES).ceil().min(most) as i64;
                if off < 0.0 { by } else { -by }
            }
        }
    }

    /// Counts a trim of `by` capture frames (positive slowed, negative sped
    /// up) in the tracking (S207): the fill moves by as much, and so does
    /// the running average; `kept`, the part of a slow-down S206 made for a
    /// shortage, moves the reference too, so the output keeps that slack.
    fn count_trim(&mut self, by: f64, kept: f64) {
        self.tracking = match self.tracking {
            Tracking::Measuring { sum, count, frames } => Tracking::Measuring {
                sum: sum + by * f64::from(count),
                count,
                frames,
            },
            Tracking::Following { reference, average } => Tracking::Following {
                reference: reference + kept,
                average: average + by,
            },
        };
    }

    /// Takes the control's requests at the start of a callback (S190).
    fn take_requests(&mut self) {
        if self.stop == Stop::None && self.shared.fade_out.load(Ordering::Acquire) {
            self.stop = Stop::Fading;
            self.begin_fade_out(None);
        }
        if take(&self.shared.delay_flag) {
            let delay = self.shared.delay.load(Ordering::Relaxed);
            if self.fade.is_closed() {
                // Nothing audible to fade (S187): the reader gets it now.
                self.reader_control.set_delay_frames(delay);
                self.awaiting_resync = true;
            } else {
                // A newer change replaces a waiting one; the fade-out isn't
                // restarted.
                self.delay_pending = Some(delay);
                self.begin_fade_out(None);
            }
        }
    }

    /// One read of `need` frames for the piece that starts at callback frame
    /// `at`, with corrections held while anything is audible (S188). Drift
    /// is judged once per callback (S206): a read after the callback's first
    /// is always held, and what it reports as due is ignored, since its
    /// error also counts the frames the callback has already read.
    fn read(&mut self, need: usize, at: usize) -> ReadReport {
        let continued = at > 0;
        let hold = continued || !self.fade.is_closed();
        let block = &mut self.stereo[..need * 2];
        let report = if hold {
            self.reader.read_target_held(block, need)
        } else {
            self.reader.read_target(block, need)
        };
        if !hold {
            // The first read after a held correction's fade-out, which made
            // it if it was still due.
            self.correction_due = false;
        }
        if report.correction != Correction::None || report.starved > 0 || report.rejected {
            // Anchored, corrected, dry or rejected: the fill the renderer
            // kept is gone, and a new reference is measured (S207).
            self.tracking = RESTART;
        }
        if self.awaiting_resync
            && matches!(report.correction, Correction::Resync | Correction::Prime)
        {
            self.awaiting_resync = false;
        }
        if hold && !continued && report.due != Correction::None && !self.correction_due {
            self.correction_due = true;
            // From the start of this block.
            self.begin_fade_out(at.checked_sub(1));
        }
        report
    }

    /// Where `report`'s silence and copies fall in a piece of `n` device
    /// frames made from its `need` frames (S184): the same frames without
    /// conversion, their converted positions with it.
    fn layout(&self, report: &ReadReport, need: usize, n: usize) -> Layout {
        let lead = report.leading_silence().min(need);
        let copy_end = (lead + report.copied).min(need);
        let (lead, copy_end) = if need == n && self.resampler.is_passthrough() {
            (lead, copy_end)
        } else {
            (lead * n / need, copy_end * n / need)
        };
        Layout {
            lead: lead.min(n),
            copy_end: copy_end.clamp(lead.min(n), n),
            copies: report.copied > 0,
        }
    }

    /// Applies the fade to a piece that starts at callback frame `at`.
    fn fade_piece(&mut self, piece: &mut [f32], layout: Layout, at: usize) {
        let channels = self.channels;
        let n = piece.len() / channels;
        let Layout {
            lead,
            copy_end,
            copies,
        } = layout;
        if lead > 0 {
            self.close_at(at);
            piece[..lead * channels].fill(0.0);
        }
        if copies && self.fade.is_closed() && !self.waiting() {
            self.fade.fade_in();
        }
        if copy_end > lead
            && let Some(end) = self
                .fade
                .apply(&mut piece[lead * channels..copy_end * channels], channels)
        {
            self.fade_out_done(Some(at + lead + end));
        }
        if n > copy_end {
            self.close_at(at + copy_end);
            piece[copy_end * channels..].fill(0.0);
        }
    }

    /// Whether a stop, a delay change or a held correction keeps the
    /// renderer closed (S184).
    fn waiting(&self) -> bool {
        self.stop != Stop::None
            || self.delay_pending.is_some()
            || self.awaiting_resync
            || self.correction_due
    }

    /// Starts a fade-out; one that ends at once (nothing audible) ends after
    /// `last`, the callback frame before it (`None`: before the callback).
    fn begin_fade_out(&mut self, last: Option<usize>) {
        self.fade.fade_out();
        if self.fade.is_closed() {
            self.fade_out_done(last);
        }
    }

    /// A frame the reader filled with silence closes the renderer at once;
    /// a fade-out under way ends there as if its last frame had been
    /// rendered (S184).
    fn close_at(&mut self, frame: usize) {
        let fading_out = self.fade.state() == FadeState::FadingOut;
        self.fade.close();
        if fading_out {
            self.fade_out_done(Some(frame));
        }
    }

    /// A fade-out ended at callback frame `last` (`None`: before this
    /// callback): a waiting delay goes to the reader (S187), and a stop's
    /// fade-out is published at the end of the callback (S185).
    fn fade_out_done(&mut self, last: Option<usize>) {
        if let Some(delay) = self.delay_pending.take() {
            self.reader_control.set_delay_frames(delay);
            self.awaiting_resync = true;
        }
        if self.stop == Stop::Fading {
            self.stop = Stop::Ended(last);
        }
    }

    /// Publishes a stop's fade-out that ended in this callback of `frames`
    /// frames, and counts the frames rendered after it.
    fn finish_callback(&mut self, frames: usize, time: Option<u64>) {
        match self.stop {
            Stop::Ended(last) => {
                let after = frames - last.map_or(0, |last| last + 1);
                self.frames_after_fade = Some(after as u64);
                if let Some(time) = time {
                    let offset = last.unwrap_or(0) as u64;
                    let end = time.saturating_add(
                        offset * 1_000_000_000 / u64::from(self.device_rate.max(1)),
                    );
                    self.shared.fade_end.store(end, Ordering::Release);
                }
                self.shared.fade_rendered.store(true, Ordering::Release);
                self.stop = Stop::Done;
            }
            Stop::Done => {
                if let Some(after) = self.frames_after_fade.as_mut() {
                    *after += frames as u64;
                }
            }
            Stop::None | Stop::Fading => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::Ring;

    // covers: S62
    #[test]
    fn the_read_bound_covers_what_the_resampler_asks_for() {
        for device_rate in [
            8_000, 11_025, 22_050, 44_100, 88_200, 96_000, 176_400, 192_000, 384_000,
        ] {
            let mut resampler = Resampler::new(48_000, device_rate).unwrap();
            let bound = reader_frames_bound(RENDER_CHUNK, 48_000, device_rate);
            let input = vec![0.25f32; bound * 2];
            let mut output = vec![0.0f32; RENDER_CHUNK * 2];
            for step in 0..400 {
                let n = 1 + (step * 37) % RENDER_CHUNK;
                let need = resampler.input_frames_for(n);
                assert!(need <= bound, "{need} > {bound} at {device_rate} Hz");
                let (used, produced) = resampler.process(&input[..need * 2], &mut output[..n * 2]);
                assert_eq!((used, produced), (need, n), "at {device_rate} Hz");
            }
        }
    }

    #[test]
    fn a_renderer_is_refused_without_channels_or_with_another_readers_control() {
        let ring = Ring::for_rate(48_000).unwrap();
        let gain = Arc::new(SharedGain::new(1.0));
        let reader = Reader::new(Arc::clone(&ring), 48_000);
        let other = Reader::new(Arc::clone(&ring), 48_000);
        let control = RendererControl::new(&other);
        let made = Renderer::new(reader, &control, Arc::clone(&gain), 48_000, 48_000, 2);
        assert_eq!(made.err(), Some(RenderError::ForeignControl));
        let control = RendererControl::new(&other);
        let made = Renderer::new(other, &control, Arc::clone(&gain), 48_000, 48_000, 0);
        assert_eq!(made.err(), Some(RenderError::NoChannels));
        let reader = Reader::new(ring, 48_000);
        let control = RendererControl::new(&reader);
        let made = Renderer::new(reader, &control, gain, 48_000, 1_000_000, 2);
        assert_eq!(made.err(), Some(RenderError::UnsupportedRate));
    }

    /// A capture writing 480 frames and a 44.1 kHz device asking for 441,
    /// both every 10 ms, in step, on one made-up clock that `step` moves.
    struct Stepped {
        writer: crate::ring::RingWriter,
        renderer: Renderer,
        control: RendererControl,
        block: Vec<f32>,
        out: Vec<f32>,
        now: u64,
    }

    impl Stepped {
        fn new() -> Stepped {
            let ring = Ring::for_rate(48_000).unwrap();
            let writer = ring.writer().unwrap();
            let reader = Reader::new(ring, 48_000);
            let control = RendererControl::new(&reader);
            let gain = Arc::new(SharedGain::new(1.0));
            let renderer = Renderer::new(reader, &control, gain, 48_000, 44_100, 2).unwrap();
            Stepped {
                writer,
                renderer,
                control,
                block: vec![0.5; 480 * 2],
                out: vec![0.0; 441 * 2],
                now: 0,
            }
        }

        /// 10 ms: a block, then a callback 1 ms after it.
        fn step(&mut self, count: usize) {
            for _ in 0..count {
                self.now += 10_000_000;
                self.writer.write_at(&self.block, 480, self.now);
                self.renderer.render_at(&mut self.out, self.now + 1_000_000);
            }
        }
    }

    // covers: S207
    #[test]
    fn the_tracking_restarts_when_the_capture_stalls_or_the_output_leaves_its_reach() {
        let mut s = Stepped::new();
        // Half a second after the reader anchors, the reference is kept.
        s.step(30);
        assert!(matches!(s.renderer.tracking, Tracking::Measuring { .. }));
        s.step(30);
        let Tracking::Following { reference, average } = s.renderer.tracking else {
            panic!("not following: {:?}", s.renderer.tracking);
        };
        assert!((average - reference).abs() <= 1.0, "{average} {reference}");
        // A callback 150 ms after the newest stamp: the capture stalled, and
        // the tracking measures anew once it writes again.
        s.now += 150_000_000;
        s.renderer.render_at(&mut s.out, s.now);
        assert_eq!(s.renderer.tracking, RESTART);
        s.step(60);
        assert!(matches!(s.renderer.tracking, Tracking::Following { .. }));
        // Held back by 100 ms, beyond its lead and the dead band: no trim
        // reaches it, and the tracking stays restarted.
        s.control.set_delay_frames(4800);
        s.step(100);
        assert_eq!(s.control.counters().resyncs, 1);
        assert_eq!(s.renderer.tracking, RESTART);
        assert_eq!(
            (s.renderer.slowed_frames(), s.renderer.sped_frames()),
            (0, 0)
        );
        // A callback without a time changes nothing either way.
        s.control.set_delay_frames(0);
        s.step(100);
        assert!(matches!(s.renderer.tracking, Tracking::Following { .. }));
        let kept = s.renderer.tracking;
        s.writer.write_at(&s.block.clone(), 480, s.now + 10_000_000);
        s.renderer.render(&mut s.out);
        assert_eq!(s.renderer.tracking, kept);
    }
}
