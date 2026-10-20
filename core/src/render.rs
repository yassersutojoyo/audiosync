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
//! delay: the largest callback so far, and with rate conversion what that
//! callback can ask the reader for plus the writer's latest block, since a
//! converting output's callbacks never line up with the capture's blocks.
//! Only the first read of a callback may correct drift; its later reads are
//! held and what they report as due is ignored.
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
use crate::reader::{Correction, ReadReport, Reader, ReaderControl, ReaderCounters};
use crate::resample::Resampler;
use crate::volume::{GainStage, SharedGain};

/// Device frames rendered per read; bounds the renderer's buffers.
pub const RENDER_CHUNK: usize = 512;

/// The longest a host waits for its outputs' fade-outs at a stop or a
/// removal before it stops them anyway (S186).
pub const STOP_FADE_CAP: Duration = Duration::from_millis(500);

/// No output time published yet.
const NO_TIME: u64 = u64::MAX;

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
/// fade-out before a stop (S185), whether that fade-out has played, and the
/// reader's lag and counters. Cheap to clone; every method is a lock-free
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
    /// The largest callback rendered so far, in device frames (S206).
    largest_callback: usize,
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
    /// Device frames the resampler could not produce and that were played
    /// as silence. Stays 0 unless [`reader_frames_bound`] is wrong.
    shortfall_frames: u64,
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
        let read_frames = reader_frames_bound(RENDER_CHUNK, capture_rate, device_rate);
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
            largest_callback: 0,
            stereo: vec![0.0; read_frames * 2].into_boxed_slice(),
            converted: vec![0.0; RENDER_CHUNK * 2].into_boxed_slice(),
            fade: Fade::new(device_rate),
            stop: Stop::None,
            delay_pending: None,
            awaiting_resync: false,
            correction_due: false,
            frames_after_fade: None,
            shortfall_frames: 0,
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
        self.shortfall_frames
    }

    /// Fills `out`, interleaved with [`Renderer::channels`] channels.
    pub fn render(&mut self, out: &mut [f32]) {
        self.render_inner(out, None);
    }

    /// [`Renderer::render`] for a callback whose first frame is output at
    /// `output_time_ns` host nanoseconds, so a fade-out's end can be
    /// published (S185, the Mac).
    pub fn render_timed(&mut self, out: &mut [f32], output_time_ns: u64) {
        self.render_inner(out, Some(output_time_ns));
    }

    fn render_inner(&mut self, out: &mut [f32], time: Option<u64>) {
        let channels = self.channels;
        let frames = out.len() / channels;
        self.update_lead(frames);
        self.take_requests();
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
                self.shortfall_frames += (n - produced) as u64;
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
    /// device frames (S206): the largest callback so far, and with rate
    /// conversion the most that callback can ask the reader for plus the
    /// frames of the writer's latest block.
    fn update_lead(&mut self, frames: usize) {
        self.largest_callback = self.largest_callback.max(frames);
        let lead = if self.resampler.is_passthrough() {
            self.largest_callback
        } else {
            reader_frames_bound(self.largest_callback, self.capture_rate, self.device_rate)
                .saturating_add(self.reader.ring().last_block_frames())
        };
        self.reader
            .set_lead_frames(i64::try_from(lead).unwrap_or(i64::MAX));
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
}
