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
//! callback when it is larger: what that callback can ask the reader for
//! plus the capture's block (at most [`LEAD_BLOCK_CAP_MS`]), since its
//! callbacks never line up with the capture's blocks. Every renderer
//! converts, at the capture rate at ratio 1, which copies its frames
//! between trims (S85). Only the first read of a callback may
//! correct drift; its later reads are held and what they report as due is
//! ignored. A renderer whose reader holds fewer frames than a
//! callback needs (its device clock runs fast and the lead's slack is used
//! up) slows its converter over that callback by the shortage, up to
//! 1 / [`SLOW_DOWN_SHARE`] of the callback's reads, instead of letting the
//! reader run dry.
//!
//! Clock stamps (S207): when the host gives each callback the time it reads
//! at ([`Renderer::render_at`]) and stamps the capture's blocks on the same
//! clock, a converting renderer follows the capture's clock by those times,
//! whatever its hold-back. It estimates where the capture is now from the
//! newest stamp, takes the fill `Ŵ - P` at the start of every callback,
//! keeps the average over the first
//! [`TRACK_MS`] (or [`TRACK_CALLBACKS`] callbacks, when longer) after the
//! reader anchors as its reference, and trims its converter by a frame or
//! so whenever a running average of the fill (the same time constant)
//! strays more than [`TRIM_DEAD_ZONE_FRAMES`] from it: slowed when the
//! device's clock runs fast, sped up when it runs slow,
//! at most 1 / [`TRIM_SHARE`] of a callback's reads at a time.
//!
//! Late callbacks (S209): given each callback's output time and the time
//! it reads at, the renderer moves its reader's delay by how much later
//! than its earliest callbacks this one came, so a late callback anchors
//! where an early one would. Drift is judged against the earliest
//! callbacks as they stood when the reader anchored, so lateness that
//! builds slower than their floor leaks up is never taken for drift; that
//! anchor floor falls over seconds toward callbacks earlier still. The
//! floor forms over [`FLOOR_FORM_MS`] of callbacks before any counts.
//!
//! The observer (S229): once an output has played [`OBSERVE_AFTER_MS`],
//! the renderer keeps the least slack its reader had at the start of a
//! callback over what that callback needs, the largest drop of the lateness
//! floor below the anchor's, and the steps in the device's output times,
//! next to the shortages S206's slow-down made up. It only measures:
//! nothing it keeps changes what plays.
//!
//! Exact differences (S208): each renderer publishes its start lead and
//! hold-back in its ring's [`SessionLeads`], and gives its reader its
//! hold-back plus the session lead, less its converter's latency, so every
//! output sounds its hold-back plus the same session lead after the
//! capture. When that delay changes, it re-anchors as on a delay change.
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
//!   its endpoint still holds (Windows, [`fade_tail_frames`]).
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
use crate::lead::SessionLeads;
use crate::reader::{Correction, NextRead, ReadReport, Reader, ReaderControl, ReaderCounters};
use crate::resample::Resampler;
use crate::ring::Ring;
use crate::volume::{GainStage, SharedGain};

/// Device frames rendered per read; bounds the renderer's buffers.
pub const RENDER_CHUNK: usize = 512;

/// The longest a host waits for its outputs' fade-outs at a stop or a
/// removal before it stops them anyway (S186).
pub const STOP_FADE_CAP: Duration = Duration::from_millis(500);

/// The most a Windows endpoint's clock may hold a fade-out past the queued
/// buffer (S185). The four endpoints measured on a PC lay 4 to 30 ms past
/// it; a clock that says more (a stalled position, or a driver that counts
/// a Bluetooth link) delays a stop by at most this, well inside S186's cap.
pub const FADE_TAIL_CAP: Duration = Duration::from_millis(100);

/// No output time published yet.
const NO_TIME: u64 = u64::MAX;

/// The callbacks a renderer looks back over for its steady callback size
/// when the host gives no period (S206): a larger one is forgotten that many
/// callbacks later.
pub const CALLBACK_WINDOW: usize = 8;

/// The most of the capture's block a converting renderer's lead counts, in
/// ms (S206): more than the capture's blocks (10 ms packets on Windows, 512
/// or 1024 frames on the Mac), about the first silence a Windows loopback
/// gap writes, and well inside the readers' 50 ms dead band, so silence
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
/// in capture frames, before the converter is trimmed (S207). It filters
/// the hosts' wake jitter out of the error before the trim acts on it
/// (Adriaensen, "Controlling adaptive resampling", LAC 2012): one wake in 33
/// or 58 coming 9 ms late moves the average up to 12.5 frames, which the old
/// 4 frames trimmed back and forth. A dead zone rather than a sign rule,
/// because one late wake keeps the average off the same way for tens of
/// callbacks, which a sign rule would let through.
pub const TRIM_DEAD_ZONE_FRAMES: f64 = 15.0;

/// A trim takes at most this share of a callback's reads, a 256th, and at
/// least one frame (S207).
pub const TRIM_SHARE: usize = 256;

/// A stamp older than this, in ms, or than [`STALE_STAMP_BLOCKS`] of the
/// capture's blocks, whichever is more, means the capture stalled: the
/// tracking restarts (S207).
pub const STALE_STAMP_MS: u64 = 100;

/// See [`STALE_STAMP_MS`].
pub const STALE_STAMP_BLOCKS: u64 = 4;

/// The most a callback's lateness moves its reader's delay, in ms (S209).
pub const LATE_CAP_MS: u64 = 50;

/// The device time, in ms, over which a renderer's timed callbacks form
/// its lateness floor before any of them counts as late (S209). Whatever
/// the callback size, the floor forms over the same span.
pub const FLOOR_FORM_MS: u32 = 300;

/// The fewest timed callbacks the floor forms over (S209), so a device
/// with large callbacks still has a second lowest that is not its first.
pub const FLOOR_FORM_CALLBACKS: u32 = 5;

/// The time constant, in ms of device frames, with which the anchor floor
/// falls toward a callback that came earlier than it (S209): a lasting move
/// earlier is followed within a few seconds, a stray stamp or two hardly.
pub const ANCHOR_FALL_MS: u32 = 2000;

/// How long a renderer's start window stays open, in ms of device frames
/// after its first timed callback (S317): the longest late start the timing
/// matrix gives, the set that confirms it and the step's fade-out.
pub const START_WINDOW_MS: u32 = 3000;

/// How far below the anchor floor the start set's level must lie, in ns,
/// for a start step (S318, S321).
pub const START_STEP_GAP_NS: i64 = 2_000_000;

/// The start set's jitter allowance A, in ns (S321): how far the medians of
/// its older and newer halves may lie apart for it to count as settled.
pub const START_JITTER_NS: i64 = 5_000_000;

/// The most timed callbacks the start set holds (S321).
pub const START_SET_LEN: usize = 256;

/// How fast the earliest lateness a renderer saw rises, in ns per second of
/// device frames (S209). The floor never lies more than [`LATE_CAP_MS`]
/// below the current callback, so early callbacks stop counting within 50 s.
pub const EARLIEST_LEAK_NS_PER_S: u64 = 1_000_000;

/// A callback later than this, in ms, counts as late in
/// [`Renderer::late_callbacks`] (S209).
pub const LATE_COUNT_MS: u64 = 5;

/// A delay increase larger than this, in ms, restarts the reader at the new
/// delay (S187, S159) instead of inserting the increase as silence (S47).
pub const RESTART_INCREASE_MS: u32 = 5000;

/// How long an output plays its reader's frames before the observer takes
/// its values (S229): the span over which the trim measures its reference
/// (S207), long after the lateness floor has formed (S209).
pub const OBSERVE_AFTER_MS: u32 = TRACK_MS;

/// How far a callback's output time may lie from the previous callback's
/// plus that one's frames at the device's rate before the observer counts
/// a step in the device's output times (S229), in ns.
pub const OUTPUT_TIME_STEP_NS: u64 = 1_000_000;

/// What a Bluetooth output adds to its start lead, in capture frames
/// (S230): one capture block, the 512 frames the Mac's capture writes per IO
/// cycle by default (a Windows packet holds 480), so a callback that comes
/// a block early against the capture's writes still finds its frames.
pub const BLUETOOTH_LEAD_FRAMES: usize = 512;

/// The output latencies a Bluetooth device may report and be believed, in
/// ns (S231): Android's window for a Bluetooth delay report, 50 to 1000 ms.
pub const BLUETOOTH_LATENCY_MIN_NS: u64 = 50_000_000;
pub const BLUETOOTH_LATENCY_MAX_NS: u64 = 1_000_000_000;

/// What a Bluetooth output's latency is taken to be when its report lies
/// outside the window and none before it was plausible (S231): the 149.3 ms
/// a 44.1 kHz Bluetooth speaker reported on the Mac, rounded.
pub const BLUETOOTH_LATENCY_DEFAULT_NS: u64 = 150_000_000;

fn plausible_bluetooth_latency(ns: u64) -> bool {
    (BLUETOOTH_LATENCY_MIN_NS..=BLUETOOTH_LATENCY_MAX_NS).contains(&ns)
}

/// The output latency a host uses for a device that reported `reported_ns`
/// (S231): any device's report as it is, except a Bluetooth one's outside
/// the window, which gives way to `last_plausible_ns`, the latest report
/// inside it for the same output, else [`BLUETOOTH_LATENCY_DEFAULT_NS`].
pub fn windowed_latency_ns(
    reported_ns: u64,
    bluetooth: bool,
    last_plausible_ns: Option<u64>,
) -> u64 {
    if !bluetooth || plausible_bluetooth_latency(reported_ns) {
        return reported_ns;
    }
    last_plausible_ns
        .filter(|&ns| plausible_bluetooth_latency(ns))
        .unwrap_or(BLUETOOTH_LATENCY_DEFAULT_NS)
}

/// Where a renderer that follows the capture's clock stands (S207).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tracking {
    /// Measuring the reference since the tracking (re)started: the fills
    /// taken so far, summed, their count, the device frames of their
    /// callbacks, and Jf at the first of them.
    Measuring {
        sum: f64,
        count: u32,
        frames: u64,
        shift: f64,
    },
    /// Following: the fill the renderer keeps, and the running average.
    Following { reference: f64, average: f64 },
}

/// The tracking right after it (re)starts.
const RESTART: Tracking = Tracking::Measuring {
    sum: 0.0,
    count: 0,
    frames: 0,
    shift: 0.0,
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

/// How many frames a Windows endpoint must have taken after a fade-out's
/// last frame before that frame has played (S185), at `rate`: what its
/// clock says is written but not yet played (`clock_queued`), never less
/// than the queued buffer (`padding`) nor more than the buffer plus
/// [`FADE_TAIL_CAP`]. Without a clock reading, the buffer plus the stream
/// latency the endpoint reports (`latency_frames`).
pub fn fade_tail_frames(
    padding: u64,
    clock_queued: Option<u64>,
    latency_frames: u64,
    rate: u32,
) -> u64 {
    match clock_queued {
        Some(queued) => {
            let cap = u64::from(rate) * FADE_TAIL_CAP.as_millis() as u64 / 1000;
            queued.clamp(padding, padding.saturating_add(cap))
        }
        None => padding.saturating_add(latency_frames),
    }
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
    /// The shortages S206's slow-down made up, in capture frames.
    shortage_frames: AtomicU64,
    /// Capture frames the converter was sped up by in all (S207).
    sped_frames: AtomicU64,
    /// The largest lateness of a callback, in ns (S209).
    late_max_ns: AtomicU64,
    /// Callbacks later than [`LATE_COUNT_MS`] (S209).
    late_callbacks: AtomicU64,
    /// The observer's least slack, in ns; `i64::MAX` before its first
    /// value (S229).
    least_slack_ns: AtomicI64,
    /// The observer's largest drop of the lateness floor below the
    /// anchor's, in ns (S229).
    floor_drop_ns: AtomicU64,
    /// The observer's steps in the device's output times (S229).
    time_steps: AtomicU64,
    /// Start steps made, and the capture frames they moved the output
    /// later by (S318).
    start_steps: AtomicU64,
    start_step_frames: AtomicU64,
    /// The host said the device is Bluetooth (S230).
    bluetooth: AtomicBool,
    /// The output latency the device reported and the one the host uses,
    /// in ns (S231); they differ only when the report was rejected.
    reported_latency_ns: AtomicU64,
    output_latency_ns: AtomicU64,
    /// The ring whose session lead counts this renderer, and its slot
    /// there, `None` when every slot was taken (S208).
    ring: Arc<Ring>,
    slot: Option<usize>,
}

impl Shared {
    fn leads(&self) -> &SessionLeads {
        self.ring.session_leads()
    }

    /// What a Bluetooth device adds to the start lead (S230).
    fn extra_lead_frames(&self) -> usize {
        if self.bluetooth.load(Ordering::Acquire) {
            BLUETOOTH_LEAD_FRAMES
        } else {
            0
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            self.ring.session_leads().release(slot);
        }
    }
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
        let ring = Arc::clone(reader.ring());
        let reader = reader.control();
        let slot = ring.session_leads().claim(reader.delay_frames());
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
                shortage_frames: AtomicU64::new(0),
                sped_frames: AtomicU64::new(0),
                late_max_ns: AtomicU64::new(0),
                late_callbacks: AtomicU64::new(0),
                least_slack_ns: AtomicI64::new(i64::MAX),
                floor_drop_ns: AtomicU64::new(0),
                time_steps: AtomicU64::new(0),
                start_steps: AtomicU64::new(0),
                start_step_frames: AtomicU64::new(0),
                bluetooth: AtomicBool::new(false),
                reported_latency_ns: AtomicU64::new(0),
                output_latency_ns: AtomicU64::new(0),
                ring,
                slot,
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
    /// callback. The value is stored before its flag, and counts for the
    /// session lead at once (S208).
    pub fn set_delay_frames(&self, frames: i64) {
        self.shared.delay.store(frames.max(0), Ordering::Relaxed);
        if let Some(slot) = self.shared.slot {
            self.shared.leads().set_hold_back(slot, frames.max(0));
        }
        self.shared.delay_flag.store(true, Ordering::Release);
    }

    /// The newest delay asked for, in capture-rate frames, whether or not
    /// the reader has it yet.
    pub fn delay_frames(&self) -> i64 {
        self.shared.delay.load(Ordering::Relaxed)
    }

    /// Asks the renderer to fade out from its next callback and render exact
    /// zeros after it, before its output stops (S185). It stays closed until
    /// it stops, and no longer counts for the session lead (S208).
    pub fn request_fade_out(&self) {
        if let Some(slot) = self.shared.slot {
            self.shared.leads().leave(slot);
        }
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

    /// [`Renderer::shortage_frames`], from any thread (S206).
    pub fn shortage_frames(&self) -> u64 {
        self.shared.shortage_frames.load(Ordering::Relaxed)
    }

    /// [`Renderer::sped_frames`], from any thread (S207).
    pub fn sped_frames(&self) -> u64 {
        self.shared.sped_frames.load(Ordering::Relaxed)
    }

    /// [`Renderer::late_max_ns`], from any thread (S209).
    pub fn late_max_ns(&self) -> u64 {
        self.shared.late_max_ns.load(Ordering::Relaxed)
    }

    /// [`Renderer::late_callbacks`], from any thread (S209).
    pub fn late_callbacks(&self) -> u64 {
        self.shared.late_callbacks.load(Ordering::Relaxed)
    }

    /// [`Renderer::shortfall_frames`], from any thread.
    pub fn shortfall_frames(&self) -> u64 {
        self.shared.shortfall_frames.load(Ordering::Relaxed)
    }

    /// The least slack the reader had at the start of a callback, in ns of
    /// capture frames: `W - P` less what the callback needs, below 0 when a
    /// callback found too few frames (S229). `None` until the output has
    /// played [`OBSERVE_AFTER_MS`] and a callback was measured since.
    pub fn least_slack_ns(&self) -> Option<i64> {
        let slack = self.shared.least_slack_ns.load(Ordering::Relaxed);
        (slack != i64::MAX).then_some(slack)
    }

    /// The most the lateness floor fell below the floor at the reader's
    /// latest anchor, after the observer's warm-up, in ns (S229); 0 without
    /// output times.
    pub fn floor_drop_ns(&self) -> u64 {
        self.shared.floor_drop_ns.load(Ordering::Relaxed)
    }

    /// The callbacks whose output time stepped off the previous callback's
    /// by more than [`OUTPUT_TIME_STEP_NS`], after the observer's warm-up
    /// (S229); 0 without output times.
    pub fn time_steps(&self) -> u64 {
        self.shared.time_steps.load(Ordering::Relaxed)
    }

    /// The start steps the renderer made inside its start window (S318).
    pub fn start_steps(&self) -> u64 {
        self.shared.start_steps.load(Ordering::Relaxed)
    }

    /// The capture frames its start steps moved the output later by
    /// (S318), apart from the trim's and the slow-downs'.
    pub fn start_step_frames(&self) -> u64 {
        self.shared.start_step_frames.load(Ordering::Relaxed)
    }

    /// Tells the renderer whether its device is Bluetooth, which adds
    /// [`BLUETOOTH_LEAD_FRAMES`] to its start lead (S230). Call it before the
    /// output's first callback and before [`Renderer::set_period_frames`]:
    /// the start lead is published once.
    pub fn set_bluetooth(&self, bluetooth: bool) {
        self.shared.bluetooth.store(bluetooth, Ordering::Release);
    }

    /// What the start lead adds for a Bluetooth device, in capture frames:
    /// [`BLUETOOTH_LEAD_FRAMES`], or 0 for any other (S230).
    pub fn extra_lead_frames(&self) -> i64 {
        self.shared.extra_lead_frames() as i64
    }

    /// Hands the renderer the output latency its device reported and
    /// returns the one to use, [`windowed_latency_ns`] with the renderer's
    /// own Bluetooth flag (S231). Call it after [`Self::set_bluetooth`]. It
    /// only records: the start lead and the session lead never take it.
    pub fn set_reported_latency(&self, reported_ns: u64, last_plausible_ns: Option<u64>) -> u64 {
        let bluetooth = self.shared.bluetooth.load(Ordering::Acquire);
        let used = windowed_latency_ns(reported_ns, bluetooth, last_plausible_ns);
        self.shared
            .reported_latency_ns
            .store(reported_ns, Ordering::Relaxed);
        self.shared.output_latency_ns.store(used, Ordering::Relaxed);
        used
    }

    /// The output latency in use, in ns (S231); 0 before one was reported.
    pub fn output_latency_ns(&self) -> u64 {
        self.shared.output_latency_ns.load(Ordering::Relaxed)
    }

    /// The device's report when the window rejected it, in ns (S231).
    pub fn rejected_latency_ns(&self) -> Option<u64> {
        let reported = self.shared.reported_latency_ns.load(Ordering::Relaxed);
        (reported != self.output_latency_ns()).then_some(reported)
    }
}

/// A renderer's start window (S317, S324).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartWindow {
    /// No timed callback, or pull endpoint's wake, yet.
    Unopened,
    /// Device frames of those callbacks since it opened.
    Open {
        elapsed: u64,
    },
    Closed,
}

/// The values of the newest timed callbacks in a start window, in ns, and
/// their device frames (S321). Made with the renderer, so judging it on
/// the audio thread never allocates.
struct StartSet {
    values: Box<[i64; START_SET_LEN]>,
    frames: Box<[u64; START_SET_LEN]>,
    scratch: Box<[i64; START_SET_LEN]>,
    /// Where the next value goes.
    next: usize,
    len: usize,
}

impl StartSet {
    fn new() -> StartSet {
        StartSet {
            values: Box::new([0; START_SET_LEN]),
            frames: Box::new([0; START_SET_LEN]),
            scratch: Box::new([0; START_SET_LEN]),
            next: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, value: i64, frames: u64) {
        self.values[self.next] = value;
        self.frames[self.next] = frames;
        self.next = (self.next + 1) % START_SET_LEN;
        self.len = (self.len + 1).min(START_SET_LEN);
    }

    /// How many of the newest values span `callbacks` callbacks and
    /// `frames` device frames, the fewest that do; all of them once the set
    /// is full of callbacks too short to.
    fn spanning(&self, callbacks: usize, frames: u64) -> Option<usize> {
        let mut spanned = 0u64;
        for count in 1..=self.len {
            spanned = spanned.saturating_add(self.frames[self.index(count, 0)]);
            if count >= callbacks && spanned >= frames {
                return Some(count);
            }
        }
        (self.len == START_SET_LEN).then_some(START_SET_LEN)
    }

    /// The array index of the `at`-th oldest of the newest `count` values.
    fn index(&self, count: usize, at: usize) -> usize {
        (self.next + START_SET_LEN - count + at) % START_SET_LEN
    }

    /// The median of `len` of the newest `count` values from the `from`-th
    /// oldest on; of an even number, the higher of the middle two.
    fn median(&mut self, count: usize, from: usize, len: usize) -> i64 {
        for at in 0..len {
            self.scratch[at] = self.values[self.index(count, from + at)];
        }
        *self.scratch[..len].select_nth_unstable(len / 2).1
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
    delay_pending: bool,
    /// The hold-back the renderer took from its control.
    hold_back: i64,
    /// The delay its reader was given last: the hold-back plus the session
    /// lead, less the converter's latency (S208).
    reader_delay: i64,
    /// The converter's fixed latency, in capture frames (S62).
    latency: i64,
    /// That latency in device frames, rounded up: how long after a read's
    /// first frame its sound leaves the converter (S184).
    open_delay: usize,
    /// Device frames a fade-in that has begun still keeps closed.
    opening: usize,
    /// [`RESTART_INCREASE_MS`] in capture frames.
    restart_increase: i64,
    /// What Jt counted from at the reader's latest anchor, fallen since
    /// toward the callbacks that came earlier, in ns (S209); `None` until
    /// the floor has formed.
    anchor_floor: Option<i64>,
    /// The anchor floor as that anchor set it, for the observer (S229).
    anchored_floor: Option<i64>,
    /// Timed callbacks and their device frames until the floor has formed
    /// (S209); `floor_formed` once they reach [`FLOOR_FORM_CALLBACKS`] and
    /// `floor_form_frames`.
    timed_callbacks: u32,
    timed_frames: u64,
    floor_form_frames: u64,
    floor_formed: bool,
    /// This callback is timed and its floor has not formed: it counts no
    /// lateness, judges no drift and makes no start re-anchor (S208, S209).
    warming: bool,
    /// The latest timed callback's `now - output time`, in ns (S209).
    callback_late_ns: i64,
    /// The higher of that value and the one of the timed callback before,
    /// so one stray early stamp on an anchor drops no anchor floor (S209).
    anchor_late_ns: i64,
    /// The start lead is published: the host gave the period, or the
    /// first callback came. Not before, so a renderer whose hold-back the
    /// host hasn't set yet never moves the session lead (S208).
    start_lead_fixed: bool,
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
    /// The lowest and second lowest `now - output time` seen, in ns, with
    /// their leak (S209).
    earliest: Option<[i64; 2]>,
    /// [`OBSERVE_AFTER_MS`] in device frames, and the device frames played
    /// so far, counted until they reach it (S229).
    observe_after: u64,
    played: u64,
    /// The latest callback's output time and frames (S229).
    previous_output: Option<(u64, usize)>,
    /// What the observer published last (S229).
    least_slack_ns: i64,
    floor_drop_ns: u64,
    /// The start window, [`START_WINDOW_MS`] in device frames, and the set
    /// it judges (S317, S321).
    start_window: StartWindow,
    start_window_frames: u64,
    start_set: StartSet,
    /// The reader has anchored once; the set fills only after (S321).
    reader_anchored: bool,
    /// The lateness floor as it stood when it formed, in ns (S321).
    start_floor_ns: Option<i64>,
    /// A confirmed start step's anchor floor, waiting for the fade-out.
    step_due: Option<i64>,
    /// The anchor floor a start step set: the next anchor keeps it instead
    /// of applying S209's rule (S318).
    step_floor_set: bool,
    /// A pull endpoint in its start window (S324): its first wake's time,
    /// the device frames its later wakes asked for, this wake's time since
    /// the first less those frames, and that value's lowest since the
    /// reader's latest anchor, with S209's leak, in ns.
    pull_origin: Option<u64>,
    pull_asked: u64,
    pull_gap_ns: i64,
    pull_low_ns: i64,
    /// A confirmed pull start step, waiting for the fade-out and a wake
    /// that asks for at least its period, or the one after (S324).
    pull_step_due: bool,
    pull_step_waited: bool,
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
        // At the capture rate too, so every output can be trimmed (S206, S207).
        let resampler = Resampler::new_at_ratio_one(capture_rate, device_rate)
            .map_err(|_| RenderError::UnsupportedRate)?;
        // A speed-up (S207) reads at most a 256th more, spread over the
        // callback, so a piece may ask for a frame or two above the bound.
        let bound = reader_frames_bound(RENDER_CHUNK, capture_rate, device_rate);
        let read_frames = bound + bound / TRIM_SHARE + 2;
        let latency = resampler.latency_frames() as i64;
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
            delay_pending: false,
            hold_back: control.shared.delay.load(Ordering::Relaxed),
            reader_delay: control.reader.delay_frames(),
            latency,
            open_delay: (latency as u64 * u64::from(device_rate)).div_ceil(u64::from(capture_rate))
                as usize,
            opening: 0,
            restart_increase: i64::from(capture_rate) * i64::from(RESTART_INCREASE_MS) / 1000,
            anchor_floor: None,
            anchored_floor: None,
            timed_callbacks: 0,
            timed_frames: 0,
            floor_form_frames: u64::from(device_rate) * u64::from(FLOOR_FORM_MS) / 1000,
            floor_formed: false,
            warming: false,
            callback_late_ns: 0,
            anchor_late_ns: 0,
            start_lead_fixed: false,
            awaiting_resync: false,
            correction_due: false,
            frames_after_fade: None,
            tracking: RESTART,
            track_frames: f64::from(device_rate) * f64::from(TRACK_MS) / 1000.0,
            earliest: None,
            observe_after: u64::from(device_rate) * u64::from(OBSERVE_AFTER_MS) / 1000,
            played: 0,
            previous_output: None,
            least_slack_ns: i64::MAX,
            floor_drop_ns: 0,
            start_window: StartWindow::Unopened,
            start_window_frames: u64::from(device_rate) * u64::from(START_WINDOW_MS) / 1000,
            start_set: StartSet::new(),
            reader_anchored: false,
            start_floor_ns: None,
            step_due: None,
            step_floor_set: false,
            pull_origin: None,
            pull_asked: 0,
            pull_gap_ns: 0,
            pull_low_ns: 0,
            pull_step_due: false,
            pull_step_waited: false,
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
    /// one plays the output one capture frame later.
    pub fn slowed_frames(&self) -> u64 {
        self.shared.slowed_frames.load(Ordering::Relaxed)
    }

    /// The shortages S206's slow-down made up, in capture frames: what the
    /// reader held less than a callback needed. Part of
    /// [`Renderer::slowed_frames`], and 0 while the lead covers every
    /// callback.
    pub fn shortage_frames(&self) -> u64 {
        self.shared.shortage_frames.load(Ordering::Relaxed)
    }

    /// Capture frames the converter was sped up by to follow the capture's
    /// clock (S207), in all. Each one plays the output one capture frame
    /// earlier. Stays 0 without stamps.
    pub fn sped_frames(&self) -> u64 {
        self.shared.sped_frames.load(Ordering::Relaxed)
    }

    /// The largest lateness of a callback against the earliest ones, in ns
    /// (S209), capped at [`LATE_CAP_MS`]. Stays 0 without output times.
    pub fn late_max_ns(&self) -> u64 {
        self.shared.late_max_ns.load(Ordering::Relaxed)
    }

    /// The callbacks that came more than [`LATE_COUNT_MS`] later than the
    /// earliest ones (S209). Stays 0 without output times.
    pub fn late_callbacks(&self) -> u64 {
        self.shared.late_callbacks.load(Ordering::Relaxed)
    }

    /// Sets the device's period, in device frames: the callback it asks for
    /// when it is on time (a Windows endpoint's period, which each wake
    /// refills). The lead then counts it, or a callback when that is larger,
    /// rather than the largest of the last callbacks (S206). 0, as at the
    /// start, means the host doesn't know it. Call it before the first
    /// callback, on the output's thread.
    pub fn set_period_frames(&mut self, frames: usize) {
        self.period_frames = frames;
        if frames > 0 {
            self.publish_start_lead(frames);
            self.start_lead_fixed = true;
        }
    }

    /// The lead the reader got at the start of the last callback, in
    /// capture frames (S206); 0 before the first one.
    pub fn lead_frames(&self) -> i64 {
        self.reader.lead_frames()
    }

    /// Jf at the last callback, in capture frames (S209): below 0 while the
    /// anchor floor still falls toward callbacks earlier than it.
    pub fn floor_shift_frames(&self) -> i64 {
        self.reader.floor_shift_frames()
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
        let mut pull_silence_ns = 0;
        let mut pull_late = 0;
        let mut pull_wake_ns = 0;
        let floor_late_ns = match (time, now) {
            (Some(time), Some(now)) => {
                let late_ns = self.lateness(time, now, frames);
                self.form_floor(frames);
                self.judge_start(frames);
                if self.warming { 0 } else { late_ns }
            }
            (None, Some(now)) => {
                self.warming = false;
                // Counted as Jf, so neither the drift judgement nor S207's fill
                // takes the silence in before the step (S324).
                pull_silence_ns = self.pull_silence(frames, now);
                self.judge_pull_start(frames, pull_silence_ns);
                pull_late = self.land_pull_step(frames);
                pull_wake_ns = self.pull_wake_ns(frames);
                0
            }
            _ => {
                self.warming = false;
                0
            }
        };
        let (floor_shift_ns, drop_ns) = self.floor_shift_ns(frames);
        let floor_shift_ns = floor_shift_ns.saturating_add(pull_silence_ns);
        // Jt counts from the anchor floor while the floor lies below it, so
        // early stamps the anchor floor didn't follow move no anchor and
        // count no callback as late; the drift is judged as before.
        let below_ns = u64::try_from(floor_shift_ns.saturating_neg()).unwrap_or(0);
        let late_ns = floor_late_ns.saturating_sub(below_ns);
        let shift_ns = floor_shift_ns.saturating_add((floor_late_ns - late_ns) as i64);
        if late_ns > 0 {
            let shared = &*self.shared;
            shared.late_max_ns.fetch_max(late_ns, Ordering::Relaxed);
            if late_ns > LATE_COUNT_MS * 1_000_000 {
                shared.late_callbacks.fetch_add(1, Ordering::Relaxed);
            }
        }
        let late = late_ns as u128 * u128::from(self.capture_rate) / 1_000_000_000;
        let late = i64::try_from(late).unwrap_or(i64::MAX);
        self.reader.set_late_frames(late.saturating_add(pull_late));
        let shift = i128::from(shift_ns) * i128::from(self.capture_rate) / 1_000_000_000;
        self.reader
            .set_floor_shift_frames(shift.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64);
        // At the time the callback really reads at: Jr already counts its
        // lateness, so the capture written meanwhile belongs in the anchor.
        let capture = now.and_then(|now| self.capture_estimate(now));
        self.reader.set_capture_frames(capture);
        // The capture's clock is judged at the time the callback would have
        // read at had it come as early as the earliest ones did when the
        // reader anchored, or a pull endpoint's wake had it come on time. A
        // callback below the anchor floor is that callback's jitter, which
        // the anchor floor's fall follows; taken in full, one stray stamp
        // moved the running average past the dead zone (S207).
        let judged_ns =
            i128::from(late_ns) + i128::from(shift_ns.max(0)) + i128::from(pull_wake_ns);
        let now =
            now.map(|now| (i128::from(now) - judged_ns).clamp(0, i128::from(u64::MAX)) as u64);
        if !self.start_lead_fixed {
            self.publish_start_lead(frames);
            self.start_lead_fixed = true;
        }
        self.take_requests();
        self.follow_session_lead();
        // Before the trim, so a shortage shows as the slack it was.
        self.observe(frames, time, drop_ns);
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
    /// is larger. Lr is the most that callback can ask the reader for plus
    /// the capture's block, at most [`LEAD_BLOCK_CAP_MS`].
    fn update_lead(&mut self, frames: usize) {
        self.callbacks[self.next_callback] = frames;
        self.next_callback = (self.next_callback + 1) % CALLBACK_WINDOW;
        let steady = if self.period_frames > 0 {
            self.period_frames
        } else {
            self.callbacks.iter().copied().fold(0, usize::max)
        };
        let callback = steady.max(frames);
        let block = self.reader.ring().block_frames().min(self.block_cap);
        let lead = reader_frames_bound(callback, self.capture_rate, self.device_rate)
            .saturating_add(block);
        self.reader
            .set_lead_frames(i64::try_from(lead).unwrap_or(i64::MAX));
    }

    /// How much later than the earliest callbacks this one came, in ns, by
    /// `now - time` against its first frame's output time (S209), at most
    /// [`LATE_CAP_MS`].
    fn lateness(&mut self, time: u64, now: u64, frames: usize) -> u64 {
        let late = (i128::from(now) - i128::from(time))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        self.anchor_late_ns = if self.earliest.is_some() {
            late.max(self.callback_late_ns)
        } else {
            late
        };
        self.callback_late_ns = late;
        let leak = (frames as u64).saturating_mul(EARLIEST_LEAK_NS_PER_S)
            / u64::from(self.device_rate.max(1));
        let leak = i64::try_from(leak).unwrap_or(i64::MAX);
        let cap = (LATE_CAP_MS * 1_000_000) as i64;
        let [low, second] = self.earliest.map_or([late, i64::MAX], |[low, second]| {
            let (low, second) = (low.saturating_add(leak), second.saturating_add(leak));
            if late < low {
                [late, low]
            } else {
                [low, second.min(late)]
            }
        });
        let floor = late.saturating_sub(cap);
        self.earliest = Some([low.max(floor), second.max(floor)]);
        // The floor is the second lowest, so one stray early callback, such
        // as a device's first, never counts.
        late.saturating_sub(second.max(floor)).clamp(0, cap) as u64
    }

    /// Counts a timed callback of `frames` device frames toward the floor
    /// (S209) and sets [`Renderer::warming`] for it: the callback that
    /// brings the count to [`FLOOR_FORM_CALLBACKS`] and the frames to
    /// [`FLOOR_FORM_MS`] is the first one the floor counts for.
    fn form_floor(&mut self, frames: usize) {
        if !self.floor_formed {
            self.timed_callbacks = self.timed_callbacks.saturating_add(1);
            self.timed_frames = self.timed_frames.saturating_add(frames as u64);
            self.floor_formed = self.timed_callbacks >= FLOOR_FORM_CALLBACKS
                && self.timed_frames >= self.floor_form_frames;
            if self.floor_formed {
                self.start_floor_ns = self.late_floor();
                if let (Some(floor), Some(anchor)) = (self.start_floor_ns, self.anchor_floor) {
                    self.anchor_floor = Some(anchor.max(floor));
                    self.anchored_floor = self.anchor_floor;
                }
            }
        }
        self.warming = !self.floor_formed;
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
        if self.resampler.is_trimming() || frames < 2 {
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
                self.count_slow_down(slowed as u64, (slowed - own) as u64);
                self.shared
                    .slowed_frames
                    .fetch_add(slowed as u64, Ordering::Relaxed);
                self.shared
                    .shortage_frames
                    .fetch_add(short as u64, Ordering::Relaxed);
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

    /// How old a stamp may be, in ns, before it is stale (S207).
    fn stale_stamp_ns(&self) -> u64 {
        let rate = u64::from(self.capture_rate.max(1));
        let blocks_ns = STALE_STAMP_BLOCKS
            .saturating_mul(self.reader.ring().block_frames() as u64)
            .saturating_mul(1_000_000_000)
            / rate;
        (STALE_STAMP_MS * 1_000_000).max(blocks_ns)
    }

    /// `Ws + S` for a callback that reads at `now` (S322): the newest
    /// stamp's W plus the capture frames of the time since it, at most the
    /// lead's block. `None` without a stamp or with a stale one.
    fn capture_estimate(&self, now: u64) -> Option<i64> {
        let stamp = self.reader.ring().stamp()?;
        let age_ns = now.saturating_sub(stamp.time_ns);
        if age_ns > self.stale_stamp_ns() {
            return None;
        }
        let block = self.reader.ring().block_frames().min(self.block_cap) as u128;
        let since = (u128::from(age_ns) * u128::from(self.capture_rate) / 1_000_000_000).min(block);
        i64::try_from(u128::from(stamp.written) + since).ok()
    }

    /// Follows the capture's clock at the start of a callback of `frames`
    /// device frames that reads at `now`, needs `need` capture frames and
    /// goes on as `next` says (S207). Returns the trim it asks for, in
    /// capture frames: positive to slow the converter, negative to speed it
    /// up, 0 for none. Restarts the tracking when the newest stamp is
    /// stale; a callback without a usable stamp changes nothing.
    fn follow(&mut self, now: u64, frames: usize, need: usize, next: &NextRead) -> i64 {
        // Before the floor forms Jt is 0, so a late callback's fill would
        // hold its whole lateness and raise the reference (S207, S209).
        if self.warming {
            self.tracking = RESTART;
            return 0;
        }
        let ring = self.reader.ring();
        let Some(stamp) = ring.stamp() else {
            return 0;
        };
        let rate = u64::from(self.capture_rate.max(1));
        if now.saturating_sub(stamp.time_ns) > self.stale_stamp_ns() {
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
                shift,
            } => {
                // Against the anchor floor as it stood at the first fill, so a
                // fall during the measuring isn't averaged into the reference;
                // Jf would add each callback's jitter below the anchor floor.
                let minus_anchor = self
                    .anchor_floor
                    .map_or(0.0, |anchor| -(anchor as f64) * rate as f64 / 1e9);
                let shift = if count == 0 { minus_anchor } else { shift };
                let fill = fill + minus_anchor - shift;
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
                        shift,
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
            Tracking::Measuring {
                sum,
                count,
                frames,
                shift,
            } => Tracking::Measuring {
                sum: sum + by * f64::from(count),
                count,
                frames,
                shift,
            },
            Tracking::Following { reference, average } => Tracking::Following {
                reference: reference + kept,
                average: average + by,
            },
        };
    }

    /// The floor that this callback's lateness was measured from (S209):
    /// none until it has formed.
    fn late_floor(&self) -> Option<i64> {
        if !self.floor_formed {
            return None;
        }
        self.earliest
            .map(|[_, floor]| floor)
            .filter(|&floor| floor != i64::MAX)
    }

    /// How far the lateness floor lies above the anchor floor, and how far
    /// it lies below the anchor floor set at the reader's latest anchor, in
    /// ns (S209, S229). The floor leaks up and drops with the callbacks,
    /// so lateness that builds or fades slower than the leak moves the
    /// floor instead of Jt; judged against the anchor floor, it stays
    /// lateness. A callback of `frames` device frames earlier than the
    /// anchor floor means the anchor was too close to W, so the anchor floor
    /// falls toward it by [`ANCHOR_FALL_MS`]; it never rises until the next
    /// anchor.
    fn floor_shift_ns(&mut self, frames: usize) -> (i64, i64) {
        let Some(floor) = self.late_floor() else {
            return (0, 0);
        };
        let anchored = *self.anchored_floor.get_or_insert(floor);
        let anchor = self.anchor_floor.get_or_insert(floor);
        let below = self.callback_late_ns.saturating_sub(*anchor);
        if below < 0 {
            let span = frames as f64 / f64::from(self.device_rate.max(1));
            let share = 1.0 - (-span * 1000.0 / f64::from(ANCHOR_FALL_MS)).exp();
            *anchor = anchor.saturating_add((below as f64 * share) as i64);
        }
        let anchor = *anchor;
        (floor.saturating_sub(anchor), anchored.saturating_sub(floor))
    }

    /// The start window at a timed callback of `frames` device frames (S317,
    /// S321): opens it at the first, closes it after [`START_WINDOW_MS`],
    /// once a stop began or once it made its step, and while it is open adds
    /// the callback's value to the set and makes a start step at the set's
    /// level once the set confirms it.
    fn judge_start(&mut self, frames: usize) {
        let elapsed = match self.start_window {
            StartWindow::Closed => return,
            StartWindow::Unopened => 0,
            StartWindow::Open { elapsed } => elapsed,
        };
        if elapsed >= self.start_window_frames || self.stop != Stop::None {
            self.start_window = StartWindow::Closed;
            self.start_set.clear();
            return;
        }
        self.start_window = StartWindow::Open {
            elapsed: elapsed.saturating_add(frames as u64),
        };
        let anchor = match self.anchor_floor {
            Some(anchor) if self.reader_anchored && self.step_due.is_none() => anchor,
            _ => return,
        };
        self.start_set.push(self.anchor_late_ns, frames as u64);
        let (Some(start_floor), Some(floor)) = (self.start_floor_ns, self.late_floor()) else {
            return;
        };
        let set = &mut self.start_set;
        let Some(count) = set.spanning(FLOOR_FORM_CALLBACKS as usize, self.floor_form_frames)
        else {
            return;
        };
        let level = set.median(count, 0, count);
        // An error to fix, and callbacks that came down from where the
        // device started, which wander doesn't do within a window.
        if level >= anchor.saturating_sub(START_STEP_GAP_NS)
            || level >= start_floor.saturating_sub(START_STEP_GAP_NS + START_JITTER_NS)
        {
            return;
        }
        let older = count / 2;
        let settled = set.median(count, 0, older) - set.median(count, older, count - older);
        if settled.abs() > START_JITTER_NS {
            return;
        }
        set.clear();
        let step = level.max(floor);
        if step >= anchor {
            return;
        }
        self.start_window = StartWindow::Closed;
        if self.fade.is_closed() {
            self.make_start_step(step);
        } else {
            // Made after the fade-out, where any waiting delay or held
            // correction anchors too, so one fade covers both (S188).
            self.step_due = Some(step);
            self.begin_fade_out(None);
        }
    }

    /// The silence a pull endpoint (a period and no output times) played
    /// unheard by the core while its start window is open, in ns, at a wake
    /// at `now` that asks for `frames` device frames (S324): the time since
    /// its first wake less the frames its later wakes asked for, above that
    /// value's lowest since the reader's latest anchor. 0 otherwise.
    fn pull_silence(&mut self, frames: usize, now: u64) -> i64 {
        if self.period_frames == 0
            || (self.start_window == StartWindow::Closed && !self.pull_step_due)
        {
            return 0;
        }
        let rate = u128::from(self.device_rate.max(1));
        let Some(origin) = self.pull_origin else {
            self.pull_origin = Some(now);
            return 0;
        };
        self.pull_asked = self.pull_asked.saturating_add(frames as u64);
        let asked_ns = u128::from(self.pull_asked) * 1_000_000_000 / rate;
        let gap = i128::from(now.saturating_sub(origin)) - asked_ns as i128;
        let gap = gap.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        // The leak follows a device clock slower than the stamps' clock.
        let leak = (frames as u128 * u128::from(EARLIEST_LEAK_NS_PER_S) / rate) as i64;
        self.pull_low_ns = self.pull_low_ns.saturating_add(leak).min(gap);
        self.pull_gap_ns = gap;
        gap.saturating_sub(self.pull_low_ns)
    }

    /// How late a pull endpoint's wake of `frames` device frames came, in ns
    /// of device time (S207): its frames beyond the period, which its buffer
    /// drained while it waited; negative for a wake that asks for less, as
    /// one right after a late wake does. 0 without a period.
    fn pull_wake_ns(&self, frames: usize) -> i64 {
        if self.period_frames == 0 {
            return 0;
        }
        let beyond = frames as i64 - self.period_frames as i64;
        beyond * 1_000_000_000 / i64::from(self.device_rate.max(1))
    }

    /// The start window at a pull endpoint's wake of `frames` device frames
    /// that found `silence_ns` of unheard silence (S324): opens it at the
    /// first wake, closes it as [`Renderer::judge_start`] does, and while
    /// it is open makes a start step once S321's set of those silences
    /// confirms one.
    fn judge_pull_start(&mut self, frames: usize, silence_ns: i64) {
        if self.period_frames == 0 {
            return;
        }
        let elapsed = match self.start_window {
            StartWindow::Closed => return,
            StartWindow::Unopened => 0,
            StartWindow::Open { elapsed } => elapsed,
        };
        if elapsed >= self.start_window_frames || self.stop != Stop::None {
            self.start_window = StartWindow::Closed;
            self.start_set.clear();
            return;
        }
        self.start_window = StartWindow::Open {
            elapsed: elapsed.saturating_add(frames as u64),
        };
        if !self.reader_anchored || self.pull_step_due {
            return;
        }
        self.start_set.push(silence_ns, frames as u64);
        let set = &mut self.start_set;
        let Some(count) = set.spanning(FLOOR_FORM_CALLBACKS as usize, self.floor_form_frames)
        else {
            return;
        };
        let level = set.median(count, 0, count);
        if level <= START_STEP_GAP_NS {
            return;
        }
        let older = count / 2;
        let settled = set.median(count, 0, older) - set.median(count, older, count - older);
        if settled.abs() > START_JITTER_NS {
            return;
        }
        set.clear();
        self.start_window = StartWindow::Closed;
        let rate = u64::from(self.capture_rate);
        let moved = u64::try_from(level).unwrap_or(0).saturating_mul(rate) / 1_000_000_000;
        self.shared
            .start_step_frames
            .fetch_add(moved, Ordering::Relaxed);
        self.pull_step_due = true;
        if !self.fade.is_closed() {
            self.begin_fade_out(None);
        }
    }

    /// Makes a confirmed pull start step at a wake of `frames` device frames
    /// once the fade-out is over (S324): the reader anchors anew at this
    /// callback's first read, which skips the silence the endpoint played.
    /// Returns the capture frames this wake came late by, its frames beyond
    /// the period, for Jr; a wake that asks for less follows a late one, so
    /// the step waits one wake for a better one.
    fn land_pull_step(&mut self, frames: usize) -> i64 {
        if !self.pull_step_due || !self.fade.is_closed() {
            return 0;
        }
        if frames < self.period_frames && !self.pull_step_waited {
            self.pull_step_waited = true;
            return 0;
        }
        self.pull_step_due = false;
        self.pull_step_waited = false;
        self.reader_control.request_resync();
        self.awaiting_resync = true;
        self.shared.start_steps.fetch_add(1, Ordering::Relaxed);
        let beyond = frames.saturating_sub(self.period_frames) as u64;
        let late = beyond * u64::from(self.capture_rate) / u64::from(self.device_rate.max(1));
        i64::try_from(late).unwrap_or(i64::MAX)
    }

    /// Sets the anchor floor to `step` and has the reader anchor against it
    /// at its next read (S318), counting the step.
    fn make_start_step(&mut self, step: i64) {
        // The anchor floor never rises before an anchor, the fade-out's
        // fall included.
        self.anchor_floor = Some(self.anchor_floor.map_or(step, |anchor| anchor.min(step)));
        self.step_floor_set = true;
        self.reader_control.request_resync();
        self.awaiting_resync = true;
        self.shared.start_steps.fetch_add(1, Ordering::Relaxed);
    }

    /// Lets `frames` capture frames by which the output already plays later,
    /// a dry read's silence or a slow-down kept for a shortage, pay toward
    /// what the anchor floor still has to fall, to the floor or this
    /// callback's value when that is lower (S209), so the trim doesn't move
    /// the output later by as much again. Returns the frames paid.
    fn pay_anchor_fall(&mut self, frames: u64) -> u64 {
        let rate = u64::from(self.capture_rate.max(1));
        let low = self
            .late_floor()
            .map(|floor| floor.min(self.callback_late_ns));
        let (Some(low), Some(anchor)) = (low, self.anchor_floor.as_mut()) else {
            return 0;
        };
        let owed_ns = u64::try_from(anchor.saturating_sub(low)).unwrap_or(0);
        let frames_ns = frames.saturating_mul(1_000_000_000) / rate;
        let paid_ns = frames_ns.min(owed_ns);
        *anchor = anchor.saturating_sub(i64::try_from(paid_ns).unwrap_or(i64::MAX));
        // Uncapped, all of it is paid: the round trip through ns would lose a frame.
        let paid = if paid_ns == frames_ns {
            frames
        } else {
            (paid_ns.saturating_mul(rate) / 1_000_000_000).min(frames)
        };
        // Jf rises by what is paid, so the fills from here on are taken
        // against a shift that rose by as much.
        if let Tracking::Measuring { shift, .. } = &mut self.tracking {
            *shift += paid as f64;
        }
        paid
    }

    /// Counts an S206 slow-down of `slowed` capture frames, `kept` of them
    /// for a shortage, in the tracking (S206, S207): what of `kept` pays
    /// toward the anchor floor's fall lowers the anchor floor by as much as
    /// it moves the fill, so only the rest moves the fill and the reference.
    fn count_slow_down(&mut self, slowed: u64, kept: u64) {
        let paid = self.pay_anchor_fall(kept);
        self.count_trim((slowed - paid) as f64, (kept - paid) as f64);
    }

    /// The observer at the start of a callback of `frames` device frames
    /// output at `time`, whose lateness floor lies `drop_ns` below the
    /// anchor floor set at the reader's latest anchor (S229). Once the output has
    /// played [`OBSERVE_AFTER_MS`], it counts a step in the output times, keeps the largest drop and,
    /// while the output plays and no stop is under way, the reader's least
    /// slack over the callback's need. It publishes only a new extreme.
    fn observe(&mut self, frames: usize, time: Option<u64>, drop_ns: i64) {
        let previous = match time {
            Some(time) => self.previous_output.replace((time, frames)),
            None => self.previous_output.take(),
        };
        if self.played < self.observe_after {
            if !self.fade.is_closed() {
                self.played += frames as u64;
            }
            return;
        }
        let shared = &*self.shared;
        if let (Some(time), Some((last, last_frames))) = (time, previous) {
            let span = (last_frames as u64).saturating_mul(1_000_000_000)
                / u64::from(self.device_rate.max(1));
            if time.abs_diff(last.saturating_add(span)) > OUTPUT_TIME_STEP_NS {
                shared.time_steps.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drop = u64::try_from(drop_ns).unwrap_or(0);
        if drop > self.floor_drop_ns {
            self.floor_drop_ns = drop;
            shared.floor_drop_ns.store(drop, Ordering::Relaxed);
        }
        if self.stop != Stop::None || self.fade.is_closed() {
            return;
        }
        let need = self.resampler.input_frames_for(frames);
        let Some(next) = self.reader.next_read(need, true) else {
            return;
        };
        let slack = next.readable.saturating_sub(need as i64);
        let slack_ns = slack.saturating_mul(1_000_000_000) / i64::from(self.capture_rate.max(1));
        if slack_ns < self.least_slack_ns {
            self.least_slack_ns = slack_ns;
            shared.least_slack_ns.store(slack_ns, Ordering::Relaxed);
        }
    }

    /// Takes the control's requests at the start of a callback (S190).
    fn take_requests(&mut self) {
        if self.stop == Stop::None && self.shared.fade_out.load(Ordering::Acquire) {
            self.stop = Stop::Fading;
            // No start step once a stop's fade-out was asked for (S317).
            self.start_window = StartWindow::Closed;
            self.step_due = None;
            self.pull_step_due = false;
            self.pull_step_waited = false;
            self.begin_fade_out(None);
        }
        if take(&self.shared.delay_flag) {
            self.hold_back = self.shared.delay.load(Ordering::Relaxed);
            self.ask_reader_delay();
        }
    }

    /// Publishes the start lead for a steady callback of `callback` device
    /// frames (S208): the most that callback can ask the reader for, plus
    /// the cap on the capture's block and the converter's latency. A Bluetooth output adds a capture
    /// block (S230).
    fn publish_start_lead(&self, callback: usize) {
        let Some(slot) = self.shared.slot else {
            return;
        };
        let lead = reader_frames_bound(callback, self.capture_rate, self.device_rate)
            .saturating_add(self.block_cap)
            .saturating_add(self.latency as usize)
            .saturating_add(self.shared.extra_lead_frames());
        self.shared
            .leads()
            .set_lead(slot, i64::try_from(lead).unwrap_or(i64::MAX));
    }

    /// The delay the reader is to have (S208): the hold-back plus the
    /// session lead, less the converter's latency.
    fn wanted_reader_delay(&self) -> i64 {
        let lead = self.shared.leads().session_lead();
        self.hold_back
            .saturating_add(lead)
            .saturating_sub(self.latency)
            .max(0)
    }

    /// Re-anchors the reader when the session lead moved its delay (S208),
    /// unless the output is stopping, a change already waits, or it plays
    /// while its lateness floor forms (S209): a re-anchor then would land
    /// as if its callback were on time.
    fn follow_session_lead(&mut self) {
        if self.warming && !self.fade.is_closed() {
            return;
        }
        if self.stop == Stop::None
            && !self.delay_pending
            && self.wanted_reader_delay() != self.reader_delay
        {
            self.ask_reader_delay();
        }
    }

    /// Gives the reader its delay now when nothing is audible (S187), else
    /// after a fade-out. The delay is worked out when it is given, so the
    /// changes that come during the fade-out land together; a newer change
    /// replaces a waiting one, and the fade-out isn't restarted.
    fn ask_reader_delay(&mut self) {
        if self.fade.is_closed() {
            self.give_reader_delay();
        } else {
            self.delay_pending = true;
            self.begin_fade_out(None);
        }
    }

    fn give_reader_delay(&mut self) {
        let given = self.reader_delay;
        self.reader_delay = self.wanted_reader_delay();
        self.reader_control.set_delay_frames(self.reader_delay);
        if self.reader_delay.saturating_sub(given) > self.restart_increase {
            // S187: seconds of silence read as an output that stopped, so it
            // plays the new delay's frames at once, even ones it played before.
            self.reader_control.reset_prime();
        }
        self.awaiting_resync = true;
    }

    /// One read of `need` frames for the piece that starts at callback frame
    /// `at`, with corrections held while anything is audible (S188). Drift
    /// is judged once per callback (S206): a read after the callback's first
    /// is always held, and what it reports as due is ignored, since its
    /// error also counts the frames the callback has already read.
    fn read(&mut self, need: usize, at: usize) -> ReadReport {
        // Until the floor has formed no drift is judged (S209).
        let continued = at > 0 || self.warming;
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
        let starved = report.starved as u64;
        let paid = if starved > 0 {
            self.pay_anchor_fall(starved)
        } else {
            0
        };
        // A silence paid in full toward the anchor floor's fall keeps the
        // fill the trim follows, so the trim goes on following the rest of
        // the fall instead of losing what fell before a new reference.
        if report.correction != Correction::None || paid < starved || report.rejected {
            // Anchored, corrected, dry or rejected: the fill the renderer
            // kept is gone, and a new reference is measured (S207).
            self.tracking = RESTART;
        }
        if report.correction != Correction::None {
            self.reader_anchored = true;
            // A pull endpoint's silence counts from here (S324).
            self.pull_low_ns = self.pull_gap_ns;
            // What this callback's Jt counted from, so Jf is 0 from here; a
            // start step already set it (S318).
            if std::mem::take(&mut self.step_floor_set) {
                // A callback below the step's value has a Jt of 0 and lands
                // at its own value, so the anchor floor goes there (S318).
                if let (Some(floor), Some(anchor)) = (self.late_floor(), self.anchor_floor) {
                    self.anchor_floor = Some(floor.max(anchor.min(self.callback_late_ns)));
                }
                let ahead = self.reader.anchor_ahead_frames();
                let moved = report.error.saturating_neg().saturating_sub(ahead);
                let moved = u64::try_from(moved).unwrap_or(0);
                self.shared
                    .start_step_frames
                    .fetch_add(moved, Ordering::Relaxed);
            } else {
                self.start_set.clear();
                self.anchor_floor = match (self.late_floor(), self.anchor_floor) {
                    (Some(floor), Some(anchor)) => Some(floor.max(anchor.min(self.anchor_late_ns))),
                    (Some(floor), None) => Some(floor),
                    // Its Jt is 0, so it lands as if its own value were on
                    // time; the floor raises it once formed (S209).
                    (None, _) => self.earliest.map(|_| self.callback_late_ns),
                };
            }
            self.anchored_floor = self.anchor_floor;
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
    /// frames made from its `need` frames (S184): their converted positions.
    fn layout(&self, report: &ReadReport, need: usize, n: usize) -> Layout {
        let lead = report.leading_silence().min(need);
        let copy_end = (lead + report.copied).min(need);
        let (lead, copy_end) = (lead * n / need, copy_end * n / need);
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
            self.opening = self.open_delay;
        }
        // The read's first frame leaves the converter its latency later; the
        // frames before it stay closed, so the fade-in starts on it.
        let held = self.opening.min(copy_end.saturating_sub(lead));
        piece[lead * channels..(lead + held) * channels].fill(0.0);
        self.opening -= held;
        let from = lead + held;
        if copy_end > from
            && let Some(end) = self
                .fade
                .apply(&mut piece[from * channels..copy_end * channels], channels)
        {
            self.fade_out_done(Some(at + from + end));
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
            || self.delay_pending
            || self.awaiting_resync
            || self.correction_due
            || self.step_due.is_some()
            || self.pull_step_due
    }

    /// Starts a fade-out; one that ends at once (nothing audible) ends after
    /// `last`, the callback frame before it (`None`: before the callback).
    fn begin_fade_out(&mut self, last: Option<usize>) {
        self.opening = 0;
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
        self.opening = 0;
        self.fade.close();
        if fading_out {
            self.fade_out_done(Some(frame));
        }
    }

    /// A fade-out ended at callback frame `last` (`None`: before this
    /// callback): a waiting delay goes to the reader (S187), and a stop's
    /// fade-out is published at the end of the callback (S185).
    fn fade_out_done(&mut self, last: Option<usize>) {
        if let Some(step) = self.step_due.take() {
            self.make_start_step(step);
        }
        if self.delay_pending {
            self.delay_pending = false;
            self.give_reader_delay();
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

    /// What a [`jittered`] run trimmed: frames slowed and sped up, the most
    /// the running average of the fill strayed from the reference, and the
    /// reader's counters.
    struct Jittered {
        slowed: u64,
        sped: u64,
        most_off: f64,
        counters: ReaderCounters,
    }

    /// Runs [`Stepped`] at hold-back `delay` for `seconds`, its device's
    /// clock `ppm` fast, every `every`-th callback waking `late_ns` late and
    /// reading the frames an on-time one would.
    fn jittered(ppm: f64, every: usize, late_ns: u64, delay: i64, seconds: u64) -> Jittered {
        let mut s = Stepped::new();
        s.control.set_delay_frames(delay);
        let period_ns = 10_000_000.0 / (1.0 + ppm * 1e-6);
        let end_ns = seconds * 1_000_000_000;
        let mut block_ns = 10_000_000;
        let mut most_off = 0.0f64;
        for call in 0.. {
            let due_ns = 11_000_000 + (call as f64 * period_ns) as u64;
            if due_ns >= end_ns {
                break;
            }
            let late = if call % every == every - 1 {
                late_ns
            } else {
                0
            };
            let now = due_ns + late;
            while block_ns <= now {
                s.writer.write_at(&s.block, 480, block_ns);
                block_ns += 10_000_000;
            }
            s.renderer.render_at(&mut s.out, now);
            if let Tracking::Following { reference, average } = s.renderer.tracking {
                most_off = most_off.max((average - reference).abs());
            }
        }
        Jittered {
            slowed: s.renderer.slowed_frames(),
            sped: s.renderer.sped_frames(),
            most_off,
            counters: s.control.counters(),
        }
    }

    // covers: S207
    #[test]
    fn wake_jitter_inside_the_dead_zone_trims_nothing_and_drift_still_trims() {
        // One wake in 33 or in 58 comes 9 ms late, the worst of the timing
        // matrix's Windows profile, and its fill jumps by 432 frames. The
        // running average strays past the old dead zone of 4 frames, which
        // trimmed back and forth, but stays inside this one: nothing is
        // trimmed. Under the same wakes, a clock 300 ppm off is still
        // trimmed with it, by 9/10 of what it gained or more and never
        // against it, at hold-back 0 and held back by 100 ms alike, with no
        // correction (S207).
        for every in [33, 58] {
            let calm = jittered(0.0, every, 9_000_000, 0, 60);
            assert!(
                calm.most_off > 4.0 && calm.most_off <= TRIM_DEAD_ZONE_FRAMES,
                "one wake in {every}: strayed {}",
                calm.most_off
            );
            assert_eq!((calm.slowed, calm.sped), (0, 0), "one wake in {every}");
            for ppm in [300.0f64, -300.0] {
                let gained = (ppm.abs() * 1e-6 * 60.0 * 48_000.0) as u64;
                for delay in [0, 4800] {
                    let run = jittered(ppm, every, 9_000_000, delay, 60);
                    let (with, against) = if ppm > 0.0 {
                        (run.slowed, run.sped)
                    } else {
                        (run.sped, run.slowed)
                    };
                    let c = run.counters;
                    let context = format!("{ppm} ppm, hold-back {delay}, one wake in {every}");
                    assert!(
                        with >= gained * 9 / 10 && against == 0,
                        "{context}: {with} with, {against} against, {gained} gained"
                    );
                    assert_eq!(
                        (
                            c.drift_skips,
                            c.drift_silences,
                            c.resyncs,
                            c.starved_renders
                        ),
                        (0, 0, 0, 0),
                        "{context}"
                    );
                }
            }
        }
    }

    // covers: S207
    #[test]
    fn the_tracking_restarts_when_the_capture_stalls_and_goes_on_at_any_hold_back() {
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
        // Held back by 100 ms, beyond its lead and the dead band: the delay
        // change re-anchors it, and it is tracked again from there. On a
        // clock in step nothing is trimmed.
        s.control.set_delay_frames(4800);
        s.step(100);
        assert_eq!(s.control.counters().resyncs, 1);
        let Tracking::Following { reference, .. } = s.renderer.tracking else {
            panic!("not following: {:?}", s.renderer.tracking);
        };
        assert!((reference - 4800.0).abs() < 96.0, "{reference}");
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

    /// 10 ms on [`Stepped`]'s clock with output times: a block, then a
    /// callback `late_ns` after its on-time point whose frames play 20 ms
    /// after that point.
    fn timed_step(s: &mut Stepped, late_ns: u64) {
        s.now += 10_000_000;
        s.writer.write_at(&s.block, 480, s.now);
        let due = s.now + 1_000_000;
        s.renderer
            .render_timed_at(&mut s.out, due + 20_000_000, due + late_ns);
    }

    // covers: S208, S209
    #[test]
    fn the_floor_forms_over_300_ms_of_callbacks_and_nothing_counts_as_late_before() {
        let mut s = Stepped::new();
        timed_step(&mut s, 0);
        timed_step(&mut s, 30_000_000);
        for _ in 2..29 {
            timed_step(&mut s, 0);
        }
        assert!(s.renderer.warming, "290 ms of callbacks");
        assert_eq!(s.renderer.late_max_ns(), 0);
        assert_eq!(s.renderer.late_floor(), None);
        timed_step(&mut s, 0);
        assert!(!s.renderer.warming, "300 ms of callbacks");
        assert!(s.renderer.late_floor().is_some());
        timed_step(&mut s, 20_000_000);
        let late = s.renderer.late_max_ns();
        assert!(late.abs_diff(20_000_000) < 100_000, "{late} ns");
        let counters = s.control.counters();
        assert_eq!(
            (counters.drift_skips, counters.drift_silences),
            (0, 0),
            "{counters:?}"
        );

        // A device with large callbacks waits for five of them.
        let mut r = Stepped::new().renderer;
        for _ in 0..4 {
            r.form_floor(16_384);
        }
        assert!(r.warming, "four callbacks of 370 ms");
        r.form_floor(16_384);
        assert!(!r.warming);
    }

    /// A renderer whose anchor floor owes 10 ms of fall, measuring a
    /// reference of three fills of 1000 frames taken against a Jf of -480.
    fn owing_fall() -> Renderer {
        let mut r = Stepped::new().renderer;
        r.floor_formed = true;
        r.earliest = Some([0, 0]);
        r.callback_late_ns = 0;
        r.anchor_floor = Some(10_000_000);
        r.tracking = Tracking::Measuring {
            sum: 3000.0,
            count: 3,
            frames: 3 * 441,
            shift: -480.0,
        };
        r
    }

    /// The reference being measured, and the shift its next fill is taken
    /// against.
    fn measured(r: &Renderer) -> (f64, f64) {
        let Tracking::Measuring {
            sum, count, shift, ..
        } = r.tracking
        else {
            panic!("{:?}", r.tracking);
        };
        (sum / f64::from(count), shift)
    }

    // covers: S206, S207, S209
    #[test]
    fn what_pays_toward_the_anchor_floors_fall_keeps_the_fill_the_trim_measures() {
        // A dry read's 240 frames (5 ms) of silence pay in full. The fills
        // taken before it never held the silence, so they stay; the next
        // fill holds it and Jf rises by it, so its shift rises by as much.
        let mut r = owing_fall();
        assert_eq!(r.pay_anchor_fall(240), 240);
        assert_eq!(r.anchor_floor, Some(5_000_000));
        assert_eq!(measured(&r), (1000.0, -240.0));
        // Owing 5 ms more, a further 10 ms pays 5 ms, to the floor.
        assert_eq!(r.pay_anchor_fall(480), 240);
        assert_eq!(r.anchor_floor, Some(0));
        assert_eq!(r.pay_anchor_fall(240), 0);

        // A slow-down of 64 frames, 48 of them kept for a shortage and
        // paid: the fill moves by the 16 the trim asked for.
        let mut r = owing_fall();
        r.count_slow_down(64, 48);
        assert_eq!(measured(&r), (1016.0, -432.0));
        r.tracking = Tracking::Following {
            reference: 1000.0,
            average: 1000.0,
        };
        r.count_slow_down(64, 48);
        let Tracking::Following { reference, average } = r.tracking else {
            panic!("{:?}", r.tracking);
        };
        assert_eq!((reference, average), (1000.0, 1016.0));
        // Owing nothing more, the kept part moves the reference too.
        r.anchor_floor = Some(0);
        r.count_slow_down(64, 48);
        let Tracking::Following { reference, average } = r.tracking else {
            panic!("{:?}", r.tracking);
        };
        assert_eq!((reference, average), (1048.0, 1080.0));
    }

    // covers: S209
    #[test]
    fn the_anchor_floor_falls_toward_earlier_callbacks_and_never_rises() {
        let mut s = Stepped::new();
        for _ in 0..200 {
            timed_step(&mut s, 10_000_000);
        }
        let anchored = s.renderer.anchor_floor.unwrap();
        // Two stray callbacks 10 ms early hardly move it.
        timed_step(&mut s, 0);
        timed_step(&mut s, 0);
        for _ in 0..5 {
            timed_step(&mut s, 10_000_000);
        }
        let strayed = anchored - s.renderer.anchor_floor.unwrap();
        assert!(strayed < 1_000_000, "moved {strayed} ns by two strays");
        // A lasting move 10 ms earlier is followed within seconds.
        for _ in 0..1000 {
            timed_step(&mut s, 0);
        }
        let fell = anchored - s.renderer.anchor_floor.unwrap();
        assert!(fell.abs_diff(10_000_000) < 100_000, "fell {fell} ns");
        // Callbacks that come late again leave it where it is.
        let low = s.renderer.anchor_floor;
        for _ in 0..500 {
            timed_step(&mut s, 10_000_000);
        }
        assert_eq!(s.renderer.anchor_floor, low);
        let counters = s.control.counters();
        assert_eq!(
            (
                counters.drift_skips,
                counters.drift_silences,
                counters.starved_renders
            ),
            (0, 0, 0),
            "{counters:?}"
        );
    }

    /// [`timed_step`] with a ramp for the blocks, each frame's value its
    /// index in the capture, and the capture writing on its own clock: a
    /// late callback finds the blocks written by the time it reads at (S322
    /// anchors on them). Returns how many capture frames behind the newest
    /// written one the callback's first frame is.
    fn ramp_step(s: &mut Stepped, written: &mut u64, late_ns: u64) -> f64 {
        stray_step(s, written, late_ns, 0)
    }

    /// [`ramp_step`] whose output time is a stray, `early_ns` too late, so
    /// `now - output time` reads that much early while the callback reads
    /// at its real time.
    fn stray_step(s: &mut Stepped, written: &mut u64, late_ns: u64, early_ns: u64) -> f64 {
        s.now += 10_000_000;
        let due = s.now + 1_000_000;
        let reads_at = due + late_ns;
        while (*written / 480 + 1) * 10_000_000 <= reads_at {
            let block: Vec<f32> = (*written..*written + 480)
                .flat_map(|i| [i as f32 / 4_000_000.0; 2])
                .collect();
            s.writer
                .write_at(&block, 480, (*written / 480 + 1) * 10_000_000);
            *written += 480;
        }
        s.renderer
            .render_timed_at(&mut s.out, due + 20_000_000 + early_ns, reads_at);
        *written as f64 - f64::from(s.out[0]) * 4_000_000.0
    }

    /// An output whose callbacks all come 60 ms late, but for `strays`
    /// early by `early_ns`, whose delay is set to 100 ms after them: how far
    /// behind the capture it plays a minute later, and its late callbacks.
    /// With `stray_anchor`, the callback that resyncs is early by as much.
    fn resync_after_strays(strays: usize, early_ns: u64, stray_anchor: bool) -> (f64, u64) {
        let late = 60_000_000;
        let mut s = Stepped::new();
        let mut written = 0;
        for _ in 0..200 {
            ramp_step(&mut s, &mut written, late);
        }
        for _ in 0..strays {
            stray_step(&mut s, &mut written, late, early_ns);
        }
        for _ in 0..5 {
            ramp_step(&mut s, &mut written, late);
        }
        s.control.set_delay_frames(4800);
        if stray_anchor {
            // The fade-out takes two callbacks; the third resyncs.
            ramp_step(&mut s, &mut written, late);
            ramp_step(&mut s, &mut written, late);
            assert_eq!(s.control.counters().resyncs, 0);
            stray_step(&mut s, &mut written, late, early_ns);
            assert_eq!(s.control.counters().resyncs, 1);
        }
        let mut behind = 0.0;
        for _ in 0..6000 {
            behind = ramp_step(&mut s, &mut written, late);
        }
        let counters = s.control.counters();
        assert_eq!(counters.resyncs, 1, "{counters:?}");
        assert_eq!(
            (counters.drift_skips, counters.drift_silences),
            (0, 0),
            "{counters:?}"
        );
        (behind, s.renderer.late_callbacks())
    }

    // covers: S209
    #[test]
    fn a_resync_after_two_stray_early_stamps_lands_where_it_would_without_them() {
        // Both strays sit in the floor for up to 50 s; anchoring against it
        // used to put the output their size later for good.
        let (calm, _) = resync_after_strays(0, 0, false);
        for early_ns in [20_000_000, 60_000_000] {
            let (behind, late_callbacks) = resync_after_strays(2, early_ns, false);
            let moved_ms = (behind - calm) / 48.0;
            assert!(
                moved_ms.abs() < 2.0,
                "{early_ns} ns strays: {moved_ms:.2} ms"
            );
            assert_eq!(late_callbacks, 0, "{early_ns} ns strays");
        }
    }

    // covers: S209
    #[test]
    fn a_resync_on_a_third_stray_early_stamp_lands_where_it_would_without_them() {
        // The anchor floor used to drop to the stray's value at the anchor:
        // every later callback counted as that much late, and its drift was
        // judged that much off, until the floor leaked back up.
        let (calm, _) = resync_after_strays(0, 0, false);
        for early_ns in [20_000_000, 40_000_000, 60_000_000] {
            let (behind, late_callbacks) = resync_after_strays(2, early_ns, true);
            let moved_ms = (behind - calm) / 48.0;
            assert!(
                moved_ms.abs() < 2.0,
                "{early_ns} ns strays: {moved_ms:.2} ms"
            );
            assert_eq!(late_callbacks, 0, "{early_ns} ns strays");
        }
    }

    /// An output on time whose callbacks 100 and 101, about 1 s in, are
    /// stamped `early_ns` early: how far behind the capture it plays 20 s
    /// later, and the start steps it made.
    fn strays_in_window(early_ns: u64) -> (f64, u64) {
        let late = 20_000_000;
        let mut s = Stepped::new();
        let mut written = 0;
        for _ in 0..100 {
            ramp_step(&mut s, &mut written, late);
        }
        for _ in 0..2 {
            stray_step(&mut s, &mut written, late, early_ns);
        }
        let mut behind = 0.0;
        for _ in 0..2000 {
            behind = ramp_step(&mut s, &mut written, late);
        }
        (behind, s.control.start_steps())
    }

    // covers: S209, S318, S320
    #[test]
    fn two_stray_early_stamps_inside_the_start_window_make_no_start_step() {
        // Each callback's value is the higher of its own and the one
        // before, so two strays sit at the bottom of the set: its median stays.
        let (calm, _) = strays_in_window(0);
        for early_ns in [5_000_000, 20_000_000] {
            let (behind, steps) = strays_in_window(early_ns);
            let moved_ms = (behind - calm) / 48.0;
            assert_eq!(steps, 0, "{early_ns} ns strays");
            assert!(
                moved_ms.abs() < 2.0,
                "{early_ns} ns strays: {moved_ms:.2} ms"
            );
        }
    }

    /// What [`late_start`] saw: the output's delay 3.2 s in and at the end,
    /// in ms, its start steps, the frames they moved it, the silent device
    /// frames after its callbacks came on time, its counters, and the first
    /// callback after which a start step waited for its fade-out.
    struct LateStart {
        delay_ms: [f64; 2],
        steps: u64,
        waited_at: Option<usize>,
        step_frames: u64,
        silent: usize,
        counters: ReaderCounters,
    }

    /// An output held back 100 ms, so no read runs dry as its callbacks come
    /// on time, whose first `late_calls` callbacks come `late_ns` late and
    /// the rest on time, for 20 s, each reading every block written by
    /// then; with `stop_at`, a stop's fade-out is asked for after that many
    /// callbacks. A capture block of 480 ramp frames comes every 10 ms.
    fn late_start(late_ns: u64, late_calls: usize, stop_at: Option<usize>) -> LateStart {
        let mut s = Stepped::new();
        s.control.set_delay_frames(4800);
        let (mut written, mut block_ns) = (0u64, 10_000_000u64);
        let mut silent = 0;
        let mut delay_ms = [f64::NAN; 2];
        let mut waited_at = None;
        for call in 0..2000u64 {
            let due = 11_000_000 + call * 10_000_000;
            let now = due + if call < late_calls as u64 { late_ns } else { 0 };
            while block_ns <= now {
                let block: Vec<f32> = (written..written + 480)
                    .flat_map(|i| [i as f32 / 4_000_000.0; 2])
                    .collect();
                s.writer.write_at(&block, 480, block_ns);
                written += 480;
                block_ns += 10_000_000;
            }
            if Some(call as usize) == stop_at {
                s.control.request_fade_out();
            }
            s.renderer
                .render_timed_at(&mut s.out, due + 20_000_000, now);
            if call >= late_calls as u64 {
                silent += s.out.chunks_exact(2).filter(|f| f[0] == 0.0).count();
            }
            if s.renderer.step_due.is_some() {
                waited_at.get_or_insert(call as usize);
            }
            if s.renderer.fade_state() == FadeState::Open {
                let captured_ms = f64::from(s.out[0]) * 4_000_000.0 / 48.0;
                let delay = (due + 20_000_000) as f64 / 1e6 - captured_ms;
                match call {
                    320 => delay_ms[0] = delay,
                    1999 => delay_ms[1] = delay,
                    _ => {}
                }
            }
        }
        LateStart {
            delay_ms,
            steps: s.control.start_steps(),
            waited_at,
            step_frames: s.control.start_step_frames(),
            silent,
            counters: s.control.counters(),
        }
    }

    // covers: S317, S318
    #[test]
    fn a_late_start_inside_the_window_is_made_up_in_one_step() {
        // 45 ms took the trim about 21 s at a frame a 441-frame callback.
        let calm = late_start(0, 100, None);
        assert_eq!(calm.steps, 0);
        for late_ns in [15_400_000u64, 45_000_000] {
            let seen = late_start(late_ns, 100, None);
            for (at, (seen, calm)) in seen.delay_ms.iter().zip(calm.delay_ms).enumerate() {
                let off = seen - calm;
                assert!(off.abs() < 2.0, "{late_ns} ns, point {at}: {off:.2} ms");
            }
            assert_eq!(seen.steps, 1, "{late_ns} ns");
            // The late callbacks anchored on the capture as it stood when they
            // read, so the output played early by their lateness.
            let ahead_ms = late_ns as f64 / 1e6;
            let step_ms = seen.step_frames as f64 / 48.0;
            assert!(
                (step_ms - ahead_ms).abs() < 2.0,
                "{late_ns} ns: a step of {step_ms:.2} ms"
            );
            // A fade-out, the step's silence and a fade-in, once.
            let silent_ms = seen.silent as f64 / 44.1;
            assert!(
                silent_ms <= late_ns as f64 / 1e6 + 2.0,
                "{late_ns} ns: {silent_ms:.2} ms of silence"
            );
            let c = seen.counters;
            assert_eq!(
                (c.drift_skips, c.drift_silences, c.starved_renders),
                (0, 0, 0),
                "{c:?}"
            );
        }
    }

    // covers: S317, S319
    #[test]
    fn no_start_step_after_the_window_or_once_a_stop_began() {
        // On time only after 3.5 s: the anchor floor's fall follows it.
        assert_eq!(late_start(45_000_000, 350, None).steps, 0);
        // The stop is asked for before the set of 300 ms confirms the step.
        assert_eq!(late_start(45_000_000, 100, Some(110)).steps, 0);
    }

    // covers: S318
    #[test]
    fn a_stop_while_the_start_step_waits_for_its_fade_makes_no_step() {
        let waited_at = late_start(45_000_000, 100, None).waited_at.unwrap();
        // Asked for on the next callback, before the step's fade-out ended.
        let seen = late_start(45_000_000, 100, Some(waited_at + 1));
        assert_eq!(seen.waited_at, Some(waited_at));
        assert_eq!((seen.steps, seen.counters.resyncs), (0, 0));
    }

    /// What [`noisy_start`] saw: its start steps and the latest callback,
    /// in seconds, that made one; the lowest and highest delay it played
    /// from `judged_from_s` on, in ms; and its counters.
    struct NoisyStart {
        steps: u64,
        last_step_s: f64,
        delay_ms: (f64, f64),
        counters: ReaderCounters,
    }

    /// 0 to 8 ms for each of 1000 callbacks, from a seeded xorshift.
    fn jitter_draws() -> Vec<u64> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        (0..1000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % 8_000_001
            })
            .collect()
    }

    /// A 48 kHz output with 480-frame callbacks, held back 100 ms, for 10
    /// s: callback `call` comes `late(call)` ns after its on-time point,
    /// and the capture writes a 480-frame ramp block every 10 ms on its
    /// own clock (S321 "What the tests show").
    fn noisy_start(late: impl Fn(usize) -> u64, judged_from_s: f64) -> NoisyStart {
        let ring = Ring::for_rate(48_000).unwrap();
        let mut writer = ring.writer().unwrap();
        let reader = Reader::new(ring, 48_000);
        let control = RendererControl::new(&reader);
        let gain = Arc::new(SharedGain::new(1.0));
        let mut renderer = Renderer::new(reader, &control, gain, 48_000, 48_000, 2).unwrap();
        control.set_delay_frames(4800);
        let mut out = vec![0.0f32; 480 * 2];
        let (mut written, mut block_ns) = (0u64, 10_000_000u64);
        let mut seen = NoisyStart {
            steps: 0,
            last_step_s: f64::NAN,
            delay_ms: (f64::INFINITY, f64::NEG_INFINITY),
            counters: ReaderCounters::default(),
        };
        for call in 0..1000usize {
            let due = 11_000_000 + call as u64 * 10_000_000;
            let now = due + late(call);
            while block_ns <= now {
                let block: Vec<f32> = (written..written + 480)
                    .flat_map(|i| [i as f32 / 4_000_000.0; 2])
                    .collect();
                writer.write_at(&block, 480, block_ns);
                written += 480;
                block_ns += 10_000_000;
            }
            renderer.render_timed_at(&mut out, due + 20_000_000, now);
            let at_s = call as f64 / 100.0;
            if control.start_steps() > seen.steps {
                seen.steps = control.start_steps();
                seen.last_step_s = at_s;
            }
            if at_s >= judged_from_s && renderer.fade_state() == FadeState::Open {
                let captured_ms = f64::from(out[0]) * 4_000_000.0 / 48.0;
                let delay = (due + 20_000_000) as f64 / 1e6 - captured_ms;
                seen.delay_ms.0 = seen.delay_ms.0.min(delay);
                seen.delay_ms.1 = seen.delay_ms.1.max(delay);
            }
        }
        seen.counters = control.counters();
        seen
    }

    /// How far apart two delay ranges lie at most, in ms.
    fn apart_ms(a: (f64, f64), b: (f64, f64)) -> f64 {
        (a.1 - b.0).abs().max((b.1 - a.0).abs())
    }

    fn assert_one_step_on_its_twin(
        seen: &NoisyStart,
        twin: &NoisyStart,
        slack_ms: f64,
        name: &str,
    ) {
        let c = seen.counters;
        assert_eq!(seen.steps, 1, "{name}: steps");
        assert!(
            seen.last_step_s < 3.0,
            "{name}: a step at {} s",
            seen.last_step_s
        );
        assert_eq!((c.drift_skips, c.drift_silences), (0, 0), "{name}: {c:?}");
        let apart = apart_ms(seen.delay_ms, twin.delay_ms);
        assert!(
            apart <= slack_ms,
            "{name}: {:.2?} ms against its twin's {:.2?}",
            seen.delay_ms,
            twin.delay_ms
        );
    }

    // covers: S207, S209, S318, S321, S322
    #[test]
    fn a_jittery_late_start_steps_once_and_lands_on_its_twin() {
        let draws = jitter_draws();
        let late = |call: usize| if call < 100 { 45_000_000 } else { 0 } + draws[call];
        let seen = noisy_start(late, 7.0);
        let twin = noisy_start(|call| draws[call], 7.0);
        assert_eq!(twin.steps, 0);
        assert_one_step_on_its_twin(&seen, &twin, 2.0, "jitter");
    }

    // covers: S321, S322
    #[test]
    fn a_gradual_return_to_time_steps_once_and_lands_on_its_twin() {
        // 45 ms late for 1 s, then 1 ms earlier each callback.
        let late =
            |call: usize| 45_000_000u64.saturating_sub(call.saturating_sub(99) as u64 * 1_000_000);
        let seen = noisy_start(late, 5.0);
        let twin = noisy_start(|_| 0, 5.0);
        assert_eq!(twin.steps, 0);
        assert_one_step_on_its_twin(&seen, &twin, 2.0, "gradual");
    }

    // covers: S321
    #[test]
    fn wander_and_a_late_start_past_the_window_make_no_start_step() {
        let draws = jitter_draws();
        // 1 ms a second earlier over the first 3 s.
        let wander = |call: usize| 3_000_000u64.saturating_sub(call as u64 * 100_000) + draws[call];
        assert_eq!(noisy_start(wander, 10.0).steps, 0);
        let too_late = |call: usize| if call < 290 { 45_000_000 } else { 0 } + draws[call];
        assert_eq!(noisy_start(too_late, 10.0).steps, 0);
    }

    /// The delay, in ms, of an output held back 100 ms whose callbacks come
    /// `phase_ns` after each 480-frame block, measured 2 s in.
    fn delay_at_phase(phase_ns: u64) -> f64 {
        let mut s = Stepped::new();
        s.control.set_delay_frames(4800);
        let mut written = 0u64;
        let mut delay_ms = f64::NAN;
        for call in 0..200u64 {
            let block_ns = 10_000_000 + call * 10_000_000;
            let block: Vec<f32> = (written..written + 480)
                .flat_map(|i| [i as f32 / 4_000_000.0; 2])
                .collect();
            s.writer.write_at(&block, 480, block_ns);
            written += 480;
            let now = block_ns + phase_ns;
            s.renderer
                .render_timed_at(&mut s.out, now + 20_000_000, now);
            if s.renderer.fade_state() == FadeState::Open {
                let captured_ms = f64::from(s.out[0]) * 4_000_000.0 / 48.0;
                delay_ms = (now + 20_000_000) as f64 / 1e6 - captured_ms;
            }
        }
        delay_ms
    }

    // covers: S322
    #[test]
    fn an_anchor_between_blocks_lands_where_the_capture_is() {
        let delays: Vec<f64> = [500_000u64, 3_000_000, 6_000_000, 9_500_000]
            .into_iter()
            .map(delay_at_phase)
            .collect();
        let low = delays.iter().copied().fold(f64::INFINITY, f64::min);
        let high = delays.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(high - low < 2.0, "delays {delays:.2?} ms");
    }

    /// An output whose first 350 callbacks, past its start window, come
    /// `late_ns` late and the rest on time, whose delay is set to 100 ms
    /// half a second after they came on time: how far behind the capture
    /// it plays a minute later.
    fn resync_after_late_start(late_ns: u64) -> f64 {
        let mut s = Stepped::new();
        let mut written = 0;
        for _ in 0..350 {
            ramp_step(&mut s, &mut written, late_ns);
        }
        for _ in 0..50 {
            ramp_step(&mut s, &mut written, 0);
        }
        s.control.set_delay_frames(4800);
        let mut behind = 0.0;
        for _ in 0..6000 {
            behind = ramp_step(&mut s, &mut written, 0);
        }
        let counters = s.control.counters();
        assert_eq!(counters.resyncs, 1, "{counters:?}");
        assert_eq!(s.control.start_steps(), 0);
        behind
    }

    // covers: S209, S319
    #[test]
    fn a_resync_while_the_anchor_floor_falls_after_a_late_start_lands_on_its_delay() {
        // The anchor lands on the on-time callback, so the anchor floor
        // must not keep the rest of its fall past it.
        let calm = resync_after_late_start(0);
        for late_ns in [15_400_000, 45_000_000] {
            let moved_ms = (resync_after_late_start(late_ns) - calm) / 48.0;
            assert!(
                moved_ms.abs() < 2.0,
                "{late_ns} ns late start: {moved_ms:.2} ms"
            );
        }
    }
}
