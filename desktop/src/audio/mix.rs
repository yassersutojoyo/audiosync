//! Several process loopback captures mixed into one stream: a source made of
//! several apps, or of every app except some, which one loopback can't
//! capture.
//!
//! Each loopback runs on its own clock and delivers its packets at its own
//! moments, so each stream first becomes stereo float at [`SESSION_RATE`]
//! ([`LaneInput`]) and waits in its own lane of the [`StreamMixer`], which
//! sums the lanes as far as all of them reach. Lanes open and close while
//! the mix runs, as apps join and leave it. Everything is allocated up
//! front; [`LaneInput::convert`] and the mixer never allocate, lock or
//! block.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

use audiosync_core::channels::input_to_stereo;
use audiosync_core::resample::Resampler;
use audiosync_core::ring::is_supported_rate;

use super::SESSION_RATE;
use super::backend::BackendError;
use super::capture::{CONVERT_CHUNK, GapFill, IDLE_LAG_HNS, MAX_MIX_APPS, StreamFormat, decode};

/// How far a lane may run ahead of the slowest one, 60 ms: more than a
/// quiet loopback waits before its silence is filled (25 ms, then 10 ms
/// behind now, `GapFill`) plus one wait of the capture loop.
pub const MAX_LAG_FRAMES: usize = SESSION_RATE as usize * 60 / 1000;

/// What a lead past [`MAX_LAG_FRAMES`] is cut back to.
const TARGET_LAG_FRAMES: usize = MAX_LAG_FRAMES / 2;

/// Room in a lane: the lag it may reach plus one piece of input.
const LANE_FRAMES: usize = MAX_LAG_FRAMES + CONVERT_CHUNK;

/// How long a lane fades in or out when [`MixLanes::select`] moves the mix
/// from one stream to another, 5 ms (S480).
const FADE_FRAMES: f32 = SESSION_RATE as f32 * 0.005;

/// How long a switch of streams waits for silence before it fades out and
/// back in instead, in 100 ns units (S480).
const SILENCE_WAIT_HNS: u64 = 2_000_000;

/// The silence a switch waits for: 10 ms of the mix below [`QUIET_LEVEL`].
const QUIET_FRAMES: usize = SESSION_RATE as usize / 100;
const QUIET_LEVEL: f32 = 1e-4;

/// One stream's frames waiting to be mixed, stereo, in a ring buffer.
struct Lane {
    samples: Box<[f32]>,
    /// Per waiting frame, whether it is flagged or filled silence.
    filled: Box<[bool]>,
    /// The first waiting frame.
    start: usize,
    /// Waiting frames.
    len: usize,
    /// Frames the mix already played as silence for this lane, because it
    /// was behind: as many of its next frames are dropped, so a late stream
    /// keeps its place in time instead of adding delay.
    owed: usize,
    /// Whether its stream is in the mix. A closed lane still hands on what
    /// it holds, but no longer sets the pace.
    open: bool,
    /// The level its frames are mixed at, moving to `target` by one
    /// [`FADE_FRAMES`] step per frame.
    gain: f32,
    target: f32,
    /// What each channel is multiplied by besides `gain`, moving in a
    /// straight line to `comp_target` over the `comp_left` frames left.
    comp: [f32; 2],
    comp_target: [f32; 2],
    comp_left: usize,
}

impl Lane {
    fn new() -> Lane {
        Lane {
            samples: vec![0.0; LANE_FRAMES * 2].into_boxed_slice(),
            filled: vec![false; LANE_FRAMES].into_boxed_slice(),
            start: 0,
            len: 0,
            owed: 0,
            open: true,
            gain: 1.0,
            target: 1.0,
            comp: [1.0, 1.0],
            comp_target: [1.0, 1.0],
            comp_left: 0,
        }
    }

    /// How many of `frames` incoming frames pay off what the lane owes.
    fn pay(&mut self, frames: usize) -> usize {
        let paid = self.owed.min(frames);
        self.owed -= paid;
        paid
    }

    /// Appends `frames` frames of silence; the caller keeps them within room.
    fn append_silence(&mut self, frames: usize) {
        let frames = frames - self.pay(frames);
        for _ in 0..frames {
            let frame = (self.start + self.len) % LANE_FRAMES;
            self.samples[frame * 2..frame * 2 + 2].fill(0.0);
            self.filled[frame] = true;
            self.len += 1;
        }
    }

    /// Appends whole frames from `stereo`, fill when `filled`; the caller
    /// keeps them within room.
    fn append(&mut self, stereo: &[f32], filled: bool) {
        let frames = stereo.as_chunks::<2>().0;
        let paid = self.pay(frames.len());
        for frame in &frames[paid..] {
            let at = (self.start + self.len) % LANE_FRAMES;
            self.samples[at * 2..at * 2 + 2].copy_from_slice(frame);
            self.filled[at] = filled;
            self.len += 1;
        }
    }

    /// Adds the first `frames` waiting frames into `out` and drops them,
    /// clearing `filled` for each frame of data.
    fn take_into(&mut self, out: &mut [f32], filled: &mut [bool], frames: usize) {
        let pairs = out.as_chunks_mut::<2>().0.iter_mut().zip(filled.iter_mut());
        for (frame, filled) in pairs.take(frames) {
            let at = self.start * 2;
            if self.gain != self.target {
                let step = 1.0 / FADE_FRAMES;
                self.gain = if self.gain < self.target {
                    (self.gain + step).min(self.target)
                } else {
                    (self.gain - step).max(self.target)
                };
            }
            if self.comp_left > 0 {
                let left = self.comp_left as f32;
                for (comp, target) in self.comp.iter_mut().zip(self.comp_target) {
                    *comp += (target - *comp) / left;
                }
                self.comp_left -= 1;
            }
            frame[0] += self.samples[at] * self.gain * self.comp[0];
            frame[1] += self.samples[at + 1] * self.gain * self.comp[1];
            *filled &= self.filled[self.start];
            self.start = (self.start + 1) % LANE_FRAMES;
            self.len -= 1;
        }
    }
}

/// Mixes several streams of stereo frames at the session rate. Its sinks
/// get each chunk with its frame count and, per frame, whether it is fill.
pub struct StreamMixer {
    lanes: Box<[Lane]>,
    /// One chunk of the mix.
    out: Box<[f32]>,
    /// Per frame of `out`, whether no lane gave it a frame of data: every
    /// lane's share was flagged or filled silence, or none had a frame.
    filled: Box<[bool]>,
}

impl StreamMixer {
    /// A mixer for `streams` streams, every lane open.
    pub fn new(streams: usize) -> StreamMixer {
        StreamMixer {
            lanes: (0..streams).map(|_| Lane::new()).collect(),
            out: vec![0.0; CONVERT_CHUNK * 2].into_boxed_slice(),
            filled: vec![false; CONVERT_CHUNK].into_boxed_slice(),
        }
    }

    /// Adds `stereo`'s whole frames to `lane`, as flagged or filled silence
    /// when `filled`, and hands every frame of the mix that is ready to
    /// `sink`, in chunks of at most [`CONVERT_CHUNK`].
    pub fn push(
        &mut self,
        lane: usize,
        stereo: &[f32],
        filled: bool,
        sink: &mut impl FnMut(&[f32], usize, &[bool]),
    ) {
        if self.lanes.get(lane).is_none_or(|lane| !lane.open) {
            return;
        }
        let mut rest = &stereo[..stereo.len() / 2 * 2];
        while !rest.is_empty() {
            let piece = (rest.len() / 2).min(CONVERT_CHUNK);
            self.lanes[lane].append(&rest[..piece * 2], filled);
            rest = &rest[piece * 2..];
            self.mix(sink);
        }
    }

    /// Adds `frames` frames of silence to `lane`, as [`StreamMixer::push`].
    pub fn push_silence(
        &mut self,
        lane: usize,
        frames: usize,
        sink: &mut impl FnMut(&[f32], usize, &[bool]),
    ) {
        if self.lanes.get(lane).is_none_or(|lane| !lane.open) {
            return;
        }
        let mut rest = frames;
        while rest > 0 {
            let piece = rest.min(CONVERT_CHUNK);
            self.lanes[lane].append_silence(piece);
            rest -= piece;
            self.mix(sink);
        }
    }

    /// Takes `lane`'s stream into the mix. It starts level with the open
    /// lane furthest behind, padded with silence, so joining never holds
    /// the mix back.
    pub fn open_lane(&mut self, lane: usize) {
        let level = self.slowest().unwrap_or(0);
        let Some(joining) = self.lanes.get_mut(lane) else {
            return;
        };
        joining.start = 0;
        joining.len = 0;
        joining.owed = 0;
        joining.open = true;
        joining.append_silence(level);
    }

    /// Leaves `lane`'s stream out of the mix; what it holds is still mixed.
    pub fn close_lane(&mut self, lane: usize) {
        if let Some(leaving) = self.lanes.get_mut(lane) {
            leaving.open = false;
        }
    }

    /// Whether `lane` is closed and has handed on all it held.
    pub fn is_drained(&self, lane: usize) -> bool {
        self.lanes
            .get(lane)
            .is_some_and(|lane| !lane.open && lane.len == 0)
    }

    /// The frames waiting in the open lane furthest behind.
    fn slowest(&self) -> Option<usize> {
        self.lanes
            .iter()
            .filter(|lane| lane.open)
            .map(|lane| lane.len)
            .min()
    }

    /// Frames waiting in `lane`.
    #[cfg(test)]
    fn waiting(&self, lane: usize) -> usize {
        self.lanes[lane].len
    }

    /// Hands on what every lane has reached, then cuts back a lead past the
    /// allowed lag.
    fn mix(&mut self, sink: &mut impl FnMut(&[f32], usize, &[bool])) {
        let ready = self.slowest().unwrap_or(0);
        self.emit(ready, sink);
        // Drift rule: once a lane leads the slowest by over MAX_LAG, the mix moves on to half that lead, padding short lanes with silence they later skip.
        let lead = self
            .lanes
            .iter()
            .filter(|lane| lane.open)
            .map(|lane| lane.len)
            .max()
            .unwrap_or(0);
        if lead > MAX_LAG_FRAMES {
            self.emit(lead - TARGET_LAG_FRAMES, sink);
        }
    }

    /// Mixes `frames` frames, each lane adding what it has of them; an open
    /// lane that runs short owes the rest.
    fn emit(&mut self, frames: usize, sink: &mut impl FnMut(&[f32], usize, &[bool])) {
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CONVERT_CHUNK);
            let out = &mut self.out[..n * 2];
            out.fill(0.0);
            let filled = &mut self.filled[..n];
            filled.fill(true);
            for lane in self.lanes.iter_mut() {
                let take = lane.len.min(n);
                lane.take_into(out, filled, take);
                if lane.open {
                    lane.owed += n - take;
                }
            }
            sink(out, n, &self.filled[..n]);
            done += n;
        }
    }
}

/// A list of app IDs one thread sets and another reads, without a lock: a
/// version that is odd while the IDs are written tells the reader to come
/// back later. It carries the apps the backend wants a mix to capture, and
/// back the apps the mix really holds.
pub struct MixPlan {
    version: AtomicU64,
    count: AtomicUsize,
    pids: [AtomicU32; MAX_MIX_APPS],
}

impl Default for MixPlan {
    fn default() -> MixPlan {
        MixPlan {
            version: AtomicU64::new(0),
            count: AtomicUsize::new(0),
            pids: [const { AtomicU32::new(0) }; MAX_MIX_APPS],
        }
    }
}

impl MixPlan {
    /// Sets the apps to `pids`, the first [`MAX_MIX_APPS`]. Only one thread
    /// sets a plan.
    pub fn set(&self, pids: &[u32]) {
        let version = self.version.load(Ordering::Relaxed);
        self.version.store(version + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let pids = &pids[..pids.len().min(MAX_MIX_APPS)];
        for (slot, &pid) in self.pids.iter().zip(pids) {
            slot.store(pid, Ordering::Relaxed);
        }
        self.count.store(pids.len(), Ordering::Relaxed);
        self.version.store(version + 2, Ordering::Release);
    }

    /// The apps into `out`, when the plan changed since version `seen` and
    /// isn't being written: their count and the version read.
    pub fn read(&self, seen: u64, out: &mut [u32; MAX_MIX_APPS]) -> Option<(usize, u64)> {
        let version = self.version.load(Ordering::Acquire);
        if version == seen || version % 2 == 1 {
            return None;
        }
        let count = self.count.load(Ordering::Relaxed).min(MAX_MIX_APPS);
        for (pid, slot) in out.iter_mut().zip(&self.pids).take(count) {
            *pid = slot.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.version.load(Ordering::Relaxed) == version).then_some((count, version))
    }
}

/// One app's capture stream in a mix.
pub trait LaneStream {
    /// Reads every waiting packet into `deliver` (bytes, frames, silent),
    /// with the silence `gaps` puts before each. Returns the packets read,
    /// or the failing call's code. Never allocates, locks or logs.
    fn drain(
        &mut self,
        gaps: &mut GapFill,
        deliver: &mut dyn FnMut(&[u8], usize, bool),
    ) -> Result<usize, i32>;

    /// Whether its app is known to have exited.
    fn app_exited(&self) -> bool;
}

/// An app's stream, opened and ready to join a mix, with its converter.
/// Opening and closing happen off the mix's thread, which only moves this
/// box in and out.
pub struct Opened<S> {
    /// The app's top process.
    pub pid: u32,
    /// Its stream, started.
    pub stream: S,
    /// What turns its packets into the mix's format.
    pub input: LaneInput,
    /// The rate of its packets.
    pub rate: u32,
}

/// An app's stream in the mix, with its gap tracking.
struct Slot<S> {
    opened: Box<Opened<S>>,
    gaps: GapFill,
}

/// The streams of a running mix, one lane each, and a pace lane of silence
/// that keeps the mix in real time while no stream is open. Nothing here
/// allocates, frees, locks or blocks: streams come in and leave as boxes.
pub struct MixLanes<S> {
    slots: Box<[Option<Slot<S>>]>,
    /// Lane 0 is the pace lane; slot `i` is lane `i + 1`.
    mixer: StreamMixer,
    pace: GapFill,
    /// The stream heard alone, or the one left out, as [`MixLanes::select`]
    /// set; every stream is heard without one.
    selected: Option<(u32, bool)>,
    /// The selection the mix plays now, as [`MixLanes::effective`] gives it.
    heard: Option<(u32, bool)>,
    /// When the switch to a new selection was asked for, while it waits.
    switch_since: Option<u64>,
    /// Whether the streams heard are fading out for a switch.
    fading: bool,
    /// The frames of the mix below [`QUIET_LEVEL`] in a row, up to
    /// [`QUIET_FRAMES`].
    quiet: usize,
}

impl<S: LaneStream> MixLanes<S> {
    /// Lanes for no app yet, at `now` (100 ns units).
    pub fn new(now: u64) -> MixLanes<S> {
        let mut mixer = StreamMixer::new(MAX_MIX_APPS + 1);
        for lane in 1..=MAX_MIX_APPS {
            mixer.close_lane(lane);
        }
        MixLanes {
            slots: (0..MAX_MIX_APPS).map(|_| None).collect(),
            mixer,
            pace: GapFill::started(SESSION_RATE, now),
            selected: None,
            heard: None,
            switch_since: None,
            fading: false,
            quiet: QUIET_FRAMES,
        }
    }

    /// Hears only the stream of `pid` when `alone`, else every stream but
    /// it; every stream while that leaves none. The mix moves at a read: at
    /// once in 10 ms of silence that comes within [`SILENCE_WAIT_HNS`], else
    /// by fading out to nothing, then the new streams in, over
    /// [`FADE_FRAMES`] each; two live streams are never cross-faded (S480).
    pub fn select(&mut self, pid: u32, alone: bool, needs: u32) {
        let _ = needs;
        self.selected = Some((pid, alone));
    }

    /// The selection as it can be heard with the streams in the mix.
    fn effective(&self) -> Option<(u32, bool)> {
        let others = self
            .slots
            .iter()
            .flatten()
            .any(|slot| self.selected.is_some_and(|(pid, _)| slot.opened.pid != pid));
        self.selected
            .filter(|&(pid, alone)| if alone { self.holds(pid) } else { others })
    }

    fn heard_by(selection: Option<(u32, bool)>, slot: &Slot<S>) -> bool {
        selection.is_none_or(|(pid, alone)| (slot.opened.pid == pid) == alone)
    }

    /// Whether the mix plays the selection, past any wait or fade out.
    pub fn settled(&self) -> bool {
        self.heard == self.effective()
    }

    /// Moves the mix toward the selection, as [`MixLanes::select`] says.
    fn switch(&mut self, now: u64) {
        let wanted = self.effective();
        if wanted == self.heard {
            self.switch_since = None;
            self.fading = false;
            return;
        }
        let since = *self.switch_since.get_or_insert(now);
        if self.fading {
            let sounding = (0..self.slots.len())
                .any(|index| self.slots[index].is_some() && self.mixer.lanes[index + 1].gain > 0.0);
            if sounding {
                return;
            }
        } else if self.quiet < QUIET_FRAMES {
            if now.saturating_sub(since) >= SILENCE_WAIT_HNS {
                self.fading = true;
                for index in 0..self.slots.len() {
                    if let Some(slot) = &self.slots[index]
                        && Self::heard_by(self.heard, slot)
                    {
                        self.mixer.lanes[index + 1].target = 0.0;
                    }
                }
            }
            return;
        }
        for index in 0..self.slots.len() {
            if let Some(slot) = &self.slots[index] {
                let lane = &mut self.mixer.lanes[index + 1];
                lane.gain = 0.0;
                lane.target = if Self::heard_by(wanted, slot) {
                    1.0
                } else {
                    0.0
                };
            }
        }
        self.heard = wanted;
        self.switch_since = None;
        self.fading = false;
    }

    /// Takes `opened` into the mix from `now` on, in a free lane that has
    /// handed on everything of the app that last held it; gives it back
    /// when no lane is free, or its app is in the mix already.
    pub fn adopt(&mut self, opened: Box<Opened<S>>, now: u64) -> Result<(), Box<Opened<S>>> {
        if self.holds(opened.pid) {
            return Err(opened);
        }
        let free = (0..self.slots.len())
            .find(|&index| self.slots[index].is_none() && self.mixer.is_drained(index + 1));
        let Some(index) = free else {
            return Err(opened);
        };
        if !self.any_open() {
            // Streams set the pace from now on.
            self.mixer.close_lane(0);
        }
        let gaps = GapFill::started(opened.rate, now);
        self.slots[index] = Some(Slot { opened, gaps });
        self.mixer.open_lane(index + 1);
        if self.selected.is_some() {
            let heard = self.slots[index]
                .as_ref()
                .is_some_and(|slot| Self::heard_by(self.heard, slot));
            let lane = &mut self.mixer.lanes[index + 1];
            lane.gain = 0.0;
            lane.target = if heard && !self.fading { 1.0 } else { 0.0 };
        }
        Ok(())
    }

    /// Leaves out every stream whose app `keep` rejects, handing each to
    /// `retired`.
    pub fn retire_unless(
        &mut self,
        keep: impl Fn(u32) -> bool,
        retired: &mut impl FnMut(Box<Opened<S>>),
    ) {
        for index in 0..self.slots.len() {
            if self.slots[index]
                .as_ref()
                .is_some_and(|slot| !keep(slot.opened.pid))
            {
                self.retire(index, retired);
            }
        }
    }

    /// Mixes app `pid`'s stream at `gains`, per channel, reached over
    /// [`FADE_FRAMES`].
    #[cfg(any(windows, test))]
    pub fn compensate(&mut self, pid: u32, gains: [f32; 2]) {
        for index in 0..self.slots.len() {
            if self.slots[index]
                .as_ref()
                .is_some_and(|slot| slot.opened.pid == pid)
            {
                let lane = &mut self.mixer.lanes[index + 1];
                if lane.comp_target != gains {
                    lane.comp_target = gains;
                    lane.comp_left = FADE_FRAMES as usize;
                }
            }
        }
    }

    /// Where app `pid`'s stream has reached, in 100 ns units.
    #[cfg(windows)]
    pub fn position(&self, pid: u32) -> Option<u64> {
        self.slots
            .iter()
            .flatten()
            .find(|slot| slot.opened.pid == pid)
            .and_then(|slot| slot.gaps.end())
    }

    /// Whether app `pid`'s stream is in the mix.
    pub fn holds(&self, pid: u32) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.opened.pid == pid)
    }

    /// The apps whose streams are in the mix, into `out`; returns how many.
    pub fn held(&self, out: &mut [u32; MAX_MIX_APPS]) -> usize {
        let mut count = 0;
        for (pid, slot) in out.iter_mut().zip(self.slots.iter().flatten()) {
            *pid = slot.opened.pid;
            count += 1;
        }
        count
    }

    /// The first open stream whose app `pick` takes, with its app.
    #[cfg(windows)]
    pub fn find(&self, pick: impl Fn(u32) -> bool) -> Option<(u32, &S)> {
        self.slots
            .iter()
            .flatten()
            .find(|slot| pick(slot.opened.pid))
            .map(|slot| (slot.opened.pid, &slot.opened.stream))
    }

    /// The open streams, in lane order.
    #[cfg(windows)]
    pub fn streams(&self) -> impl Iterator<Item = &S> {
        self.slots.iter().flatten().map(|slot| &slot.opened.stream)
    }

    fn any_open(&self) -> bool {
        self.slots.iter().any(Option::is_some)
    }

    /// Reads every stream at `now` and hands on what the mix has ready to
    /// `sink`. A stream that fails leaves the mix through `retired` and its
    /// lane goes silent; only when one fails while its app still runs and
    /// no stream is left does the mix end, with that code (Windows audio
    /// itself went away).
    pub fn read(
        &mut self,
        now: u64,
        sink: &mut impl FnMut(&[f32], usize, &[bool]),
        retired: &mut impl FnMut(Box<Opened<S>>),
    ) -> Result<(), i32> {
        self.switch(now);
        let mut quiet = self.quiet;
        let sink = &mut |out: &[f32], frames: usize, filled: &[bool]| {
            for frame in out[..frames * 2].as_chunks::<2>().0 {
                quiet = if frame[0].abs() < QUIET_LEVEL && frame[1].abs() < QUIET_LEVEL {
                    (quiet + 1).min(QUIET_FRAMES)
                } else {
                    0
                };
            }
            sink(out, frames, filled);
        };
        if self.any_open() {
            // Level with what the streams' own fills reach.
            self.pace = GapFill::started(SESSION_RATE, now.saturating_sub(IDLE_LAG_HNS));
        } else {
            let fill = self.pace.idle(now);
            self.mixer.push_silence(0, fill, sink);
        }
        let mut failure = None;
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots[index].as_mut() else {
                continue;
            };
            let mixer = &mut self.mixer;
            let Opened { stream, input, .. } = &mut *slot.opened;
            let gaps = &mut slot.gaps;
            let mut deliver = |data: &[u8], frames: usize, silent: bool| {
                input.convert(data, frames, silent, &mut |stereo: &[f32], filled: bool| {
                    mixer.push(index + 1, stereo, filled, sink);
                });
            };
            match stream.drain(gaps, &mut deliver) {
                Ok(0) => {
                    let fill = gaps.idle(now);
                    if fill > 0 {
                        deliver(&[], fill, true);
                    }
                }
                Ok(_) => {}
                Err(code) => {
                    if !stream.app_exited() {
                        failure = Some(code);
                    }
                    self.retire(index, retired);
                }
            }
        }
        self.quiet = quiet;
        match failure {
            Some(code) if !self.any_open() => Err(code),
            _ => Ok(()),
        }
    }

    fn retire(&mut self, index: usize, retired: &mut impl FnMut(Box<Opened<S>>)) {
        if let Some(slot) = self.slots[index].take() {
            retired(slot.opened);
        }
        self.mixer.close_lane(index + 1);
        if !self.any_open() {
            // The pace lane keeps time again, level with what is left.
            self.mixer.open_lane(0);
        }
    }
}

/// Turns one loopback's packets into stereo float at the session rate.
pub struct LaneInput {
    format: StreamFormat,
    resampler: Option<Resampler>,
    decoded: Box<[f32]>,
    stereo: Box<[f32]>,
    resampled: Box<[f32]>,
}

impl LaneInput {
    /// A converter for packets in `format`.
    pub fn new(format: StreamFormat) -> Result<LaneInput, BackendError> {
        if format.channels == 0 || !is_supported_rate(format.rate) {
            return Err(BackendError::UnsupportedFormat);
        }
        let resampler = if format.rate == SESSION_RATE {
            None
        } else {
            Some(
                Resampler::new(format.rate, SESSION_RATE)
                    .map_err(|_| BackendError::UnsupportedFormat)?,
            )
        };
        let resampled_frames = (CONVERT_CHUNK as u64 * u64::from(SESSION_RATE))
            .div_ceil(u64::from(format.rate)) as usize
            + 2;
        Ok(LaneInput {
            format,
            resampler,
            decoded: vec![0.0; CONVERT_CHUNK * usize::from(format.channels)].into_boxed_slice(),
            stereo: vec![0.0; CONVERT_CHUNK * 2].into_boxed_slice(),
            resampled: vec![0.0; resampled_frames * 2].into_boxed_slice(),
        })
    }

    /// Converts a packet of `frames` frames in the lane's format (silence
    /// when `silent`, whatever `data` holds) and hands the stereo frames to
    /// `out` piece by piece, with whether they came from silence.
    /// Resampled frames take the packet's flag, so a few at a switch between
    /// data and silence carry the wrong one (it only feeds the zero holes).
    pub fn convert(
        &mut self,
        data: &[u8],
        frames: usize,
        silent: bool,
        out: &mut impl FnMut(&[f32], bool),
    ) {
        let block = self.format.block_align();
        let frames = if silent {
            frames
        } else {
            frames.min(data.len() / block)
        };
        let channels = usize::from(self.format.channels);
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CONVERT_CHUNK);
            if silent {
                self.stereo[..n * 2].fill(0.0);
            } else {
                let bytes = &data[done * block..(done + n) * block];
                decode(self.format.sample, bytes, &mut self.decoded[..n * channels]);
                input_to_stereo(
                    &self.decoded[..n * channels],
                    channels,
                    &mut self.stereo[..n * 2],
                );
            }
            match self.resampler.as_mut() {
                None => out(&self.stereo[..n * 2], silent),
                Some(resampler) => {
                    let mut consumed = 0;
                    while consumed < n {
                        let (used, produced) = resampler
                            .process(&self.stereo[consumed * 2..n * 2], &mut self.resampled);
                        out(&self.resampled[..produced * 2], silent);
                        consumed += used;
                        if used == 0 && produced == 0 {
                            break;
                        }
                    }
                }
            }
            done += n;
        }
    }
}

/// The bytes of stereo float frames, as a converter for
/// little-endian float packets reads them.
pub fn as_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: any f32 is four initialized bytes, u8 needs no alignment, and
    // the slice covers exactly the floats' memory for the same lifetime.
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr().cast::<u8>(),
            std::mem::size_of_val(samples),
        )
    }
}

/// Below this level of a render endpoint's volume, in dB, its loopback is
/// too quiet to bring back up cleanly (S480).
#[cfg(any(windows, test))]
pub const UNDO_FLOOR_DB: f32 = -40.0;

/// How far past a volume change a stream must have reached before frames
/// mixed from it surely carry the new volume: the mix's lag bound plus the
/// audio engine's lead, in 100 ns units.
#[cfg(any(windows, test))]
pub const UNDO_MARGIN_HNS: u64 = 1_500_000;

/// The stereo levels, in dB, of a render endpoint with `channels` levels:
/// each channel of a stereo endpoint, else the loudest one on both sides,
/// whose gain is the smallest (S480).
#[cfg(any(windows, test))]
pub fn stereo_levels(channels: &[f32]) -> [f32; 2] {
    match channels {
        [left, right] => [*left, *right],
        _ => {
            let loudest = channels.iter().copied().reduce(f32::max).unwrap_or(0.0);
            [loudest, loudest]
        }
    }
}

/// The gain that brings a render endpoint's loopback back to the level
/// before its software volume; an error only ever makes it quieter (S480).
#[derive(Debug)]
#[cfg(any(windows, test))]
pub struct VolumeUndo {
    applied: [f32; 2],
    /// The gain of the latest change and when it happened.
    pending: Option<([f32; 2], u64)>,
    floored: bool,
}

#[cfg(any(windows, test))]
impl Default for VolumeUndo {
    fn default() -> VolumeUndo {
        VolumeUndo {
            applied: [1.0, 1.0],
            pending: None,
            floored: false,
        }
    }
}

#[cfg(any(windows, test))]
impl VolumeUndo {
    /// The endpoint's volume is `levels` dB per side from `at` on, applied
    /// in software when `software`; in hardware or unknown otherwise, which
    /// the loopback doesn't carry.
    pub fn change(&mut self, levels: [f32; 2], loudest: [f32; 2], software: bool, at: u64) {
        let _ = loudest;
        let gains = if software {
            levels.map(|db| 10_f32.powf(-db / 20.0))
        } else {
            [1.0, 1.0]
        };
        self.floored = software && levels.iter().any(|&db| db < UNDO_FLOOR_DB);
        for (applied, gain) in self.applied.iter_mut().zip(gains) {
            *applied = applied.min(gain);
        }
        self.pending = Some((gains, at));
    }

    /// The gain per side for frames mixed while the stream has reached
    /// `position`.
    pub fn gains(&mut self, position: Option<u64>) -> [f32; 2] {
        if let Some((gains, at)) = self.pending
            && position.is_some_and(|position| position >= at.saturating_add(UNDO_MARGIN_HNS))
        {
            self.applied = gains;
            self.pending = None;
        }
        self.applied
    }

    /// Whether the volume is below [`UNDO_FLOOR_DB`].
    pub fn floored(&self) -> bool {
        self.floored
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::audio::capture::SampleType;

    fn close(gains: [f32; 2], expected: [f32; 2]) -> bool {
        gains
            .iter()
            .zip(expected)
            .all(|(g, e)| (g - e).abs() < 1e-3)
    }

    // covers: S480
    #[test]
    fn a_software_volume_is_undone_per_side() {
        let mut undo = VolumeUndo::default();
        undo.change([-6.0206, -12.0412], [-6.0206, -12.0412], true, 0);
        assert!(close(undo.gains(Some(UNDO_MARGIN_HNS)), [2.0, 4.0]));
        assert!(!undo.floored());
    }

    // covers: S480
    #[test]
    fn a_hardware_or_unknown_volume_is_left_alone() {
        let mut undo = VolumeUndo::default();
        undo.change([-45.0, -20.0], [-45.0, -20.0], false, 0);
        assert_eq!(undo.gains(Some(UNDO_MARGIN_HNS)), [1.0, 1.0]);
        assert!(!undo.floored());
    }

    // covers: S480
    #[test]
    fn a_lowered_volume_keeps_the_old_gain_until_the_stream_is_surely_past_it() {
        let mut undo = VolumeUndo::default();
        let at = 10_000_000;
        undo.change([-20.0, -20.0], [-20.0, -20.0], true, at);
        assert_eq!(undo.gains(None), [1.0, 1.0]);
        assert_eq!(undo.gains(Some(at)), [1.0, 1.0]);
        assert_eq!(undo.gains(Some(at + UNDO_MARGIN_HNS - 1)), [1.0, 1.0]);
        assert!(close(undo.gains(Some(at + UNDO_MARGIN_HNS)), [10.0, 10.0]));
    }

    // covers: S480
    #[test]
    fn a_raised_volume_takes_the_smaller_gain_at_once() {
        let mut undo = VolumeUndo::default();
        undo.change([-20.0, -20.0], [-20.0, -20.0], true, 0);
        undo.gains(Some(UNDO_MARGIN_HNS));
        let at = 10_000_000;
        undo.change([-6.0206, -6.0206], [-6.0206, -6.0206], true, at);
        assert!(close(undo.gains(Some(at)), [2.0, 2.0]));
        // Two changes in a row hold the smallest gain of the three.
        undo.change([-20.0, -20.0], [-20.0, -20.0], true, at + 1);
        undo.change([-12.0412, -12.0412], [-12.0412, -12.0412], true, at + 2);
        assert!(close(undo.gains(Some(at + 2)), [2.0, 2.0]));
        assert!(close(
            undo.gains(Some(at + 2 + UNDO_MARGIN_HNS)),
            [4.0, 4.0]
        ));
    }

    // covers: S480
    #[test]
    fn a_raise_missed_between_two_readings_still_lowers_the_gain_at_once() {
        let mut undo = VolumeUndo::default();
        undo.change([-20.0, -20.0], [-20.0, -20.0], true, 0);
        assert!(close(undo.gains(Some(UNDO_MARGIN_HNS)), [10.0, 10.0]));
        // Raised to 0 dB and cut back to -20 dB before the mix read either.
        let at = 10_000_000;
        undo.change([-20.0, -20.0], [0.0, 0.0], true, at);
        assert_eq!(undo.gains(Some(at)), [1.0, 1.0]);
        assert!(close(undo.gains(Some(at + UNDO_MARGIN_HNS)), [10.0, 10.0]));
    }

    // covers: S480
    #[test]
    fn a_volume_below_the_floor_is_not_brought_back_up() {
        let mut undo = VolumeUndo::default();
        undo.change([-45.0, -10.0], [-45.0, -10.0], true, 0);
        assert!(undo.floored());
        undo.change([-35.0, -35.0], [-35.0, -35.0], true, 1);
        assert!(!undo.floored());
    }

    // covers: S480
    #[test]
    fn an_endpoint_past_stereo_takes_its_loudest_channel_on_both_sides() {
        assert_eq!(stereo_levels(&[-3.0, -9.0]), [-3.0, -9.0]);
        assert_eq!(stereo_levels(&[-6.0]), [-6.0, -6.0]);
        assert_eq!(stereo_levels(&[-9.0, -3.0, -12.0, -6.0]), [-3.0, -3.0]);
        assert_eq!(stereo_levels(&[]), [0.0, 0.0]);
    }

    /// Every frame `mixer` hands on, left channel only.
    fn run(mixer: &mut StreamMixer, pushes: &[(usize, f32, usize)]) -> Vec<f32> {
        let mut mixed = Vec::new();
        for &(lane, value, frames) in pushes {
            let block = vec![value; frames * 2];
            mixer.push(
                lane,
                &block,
                false,
                &mut |out: &[f32], n: usize, _: &[bool]| {
                    mixed.extend(out[..n * 2].iter().step_by(2));
                },
            );
        }
        mixed
    }

    // covers: S386
    #[test]
    fn lanes_are_summed_as_far_as_all_reach() {
        let mut mixer = StreamMixer::new(2);
        let mixed = run(&mut mixer, &[(0, 0.25, 480), (1, 0.5, 300)]);
        assert_eq!(mixed.len(), 300);
        assert!(mixed.iter().all(|&v| (v - 0.75).abs() < 1e-6));
        assert_eq!((mixer.waiting(0), mixer.waiting(1)), (180, 0));
        let more = run(&mut mixer, &[(1, 0.5, 180)]);
        assert_eq!(more.len(), 180);
        assert_eq!((mixer.waiting(0), mixer.waiting(1)), (0, 0));
    }

    // covers: S386
    #[test]
    fn one_stream_passes_straight_through() {
        let mut mixer = StreamMixer::new(1);
        assert_eq!(run(&mut mixer, &[(0, 0.1, 5_000)]).len(), 5_000);
        assert_eq!(mixer.waiting(0), 0);
    }

    // covers: S386
    #[test]
    fn a_stalled_lane_never_holds_the_mix_back_more_than_the_lag_bound() {
        let mut mixer = StreamMixer::new(2);
        // Lane 1 delivers nothing for 200 ms.
        let mixed = run(&mut mixer, &[(0, 0.25, 9_600)]);
        assert!(mixer.waiting(0) <= MAX_LAG_FRAMES);
        assert_eq!(mixed.len() + mixer.waiting(0), 9_600);
        assert!(mixed.iter().all(|&v| (v - 0.25).abs() < 1e-6));
        // When its 200 ms come late, the part already played as silence is
        // skipped, and the rest joins the leader's waiting frames in place.
        let behind = mixer.waiting(0);
        let caught_up = run(&mut mixer, &[(1, 0.5, 9_600)]);
        assert_eq!(caught_up.len(), behind);
        assert!(caught_up.iter().all(|&v| (v - 0.75).abs() < 1e-6));
        assert_eq!((mixer.waiting(0), mixer.waiting(1)), (0, 0));
    }

    // covers: S386
    #[test]
    fn drift_between_clocks_stays_bounded_and_the_mix_keeps_real_time() {
        // Lane 0 runs 1% fast: 485 frames for every 480 of lane 1, for 60 s.
        let mut mixer = StreamMixer::new(2);
        let mut mixed = 0;
        let mut fast = 0;
        for _ in 0..6_000 {
            for (lane, frames) in [(0, 485), (1, 480)] {
                if lane == 0 {
                    fast += frames;
                }
                mixed += run(&mut mixer, &[(lane, 0.0, frames)]).len();
                assert!(mixer.waiting(0) <= MAX_LAG_FRAMES && mixer.waiting(1) <= MAX_LAG_FRAMES);
            }
        }
        // The mix follows the faster clock to within the lag bound: never
        // more frames than it, and never more than the bound behind it.
        assert!(
            mixed <= fast && fast - mixed <= MAX_LAG_FRAMES,
            "{mixed} of {fast}"
        );
    }

    // covers: S386
    #[test]
    fn a_lane_that_never_delivers_keeps_pace_from_its_start_without_bursts() {
        // Lane 1 is a paused app: its loopback never sends a packet, and its
        // fill, anchored when the stream started, writes silence at each wake.
        let mut mixer = StreamMixer::new(2);
        let mut quiet = GapFill::started(SESSION_RATE, 0);
        let (mut playing, mut mixed) = (0, 0);
        for tick in 1..=500_u64 {
            let mut sink = |out: &[f32], n: usize, _: &[bool]| {
                assert!(out[..n * 2].iter().all(|&v| (v - 0.25).abs() < 1e-6));
                mixed += n;
            };
            mixer.push(0, &[0.25; 960], false, &mut sink);
            playing += 480;
            let fill = quiet.idle(tick * 100_000);
            mixer.push_silence(1, fill, &mut sink);
            // 10 ms behind the playing app at most, the fill's own lag,
            // never held back to the drift rule's bound.
            assert!(playing - mixed <= 480, "tick {tick}: {mixed} of {playing}");
        }
    }

    // covers: S386
    #[test]
    fn lanes_join_level_with_the_slowest_and_a_closed_lane_no_longer_sets_the_pace() {
        let mut mixer = StreamMixer::new(3);
        mixer.close_lane(2);
        assert_eq!(run(&mut mixer, &[(0, 0.25, 300), (1, 0.5, 100)]).len(), 100);
        assert_eq!(
            run(&mut mixer, &[(2, 1.0, 50)]).len(),
            0,
            "a closed lane takes nothing"
        );
        mixer.open_lane(2);
        assert_eq!(
            mixer.waiting(2),
            0,
            "level with lane 1, which has nothing waiting"
        );
        let mixed = run(&mut mixer, &[(1, 0.5, 200), (2, 1.0, 200)]);
        assert_eq!(mixed.len(), 200);
        assert!(mixed.iter().all(|&v| (v - 1.75).abs() < 1e-6));
        // Lane 1's app quits: its lane closes, and the others go on alone.
        mixer.close_lane(1);
        let alone = run(&mut mixer, &[(0, 0.25, 100), (2, 1.0, 100)]);
        assert_eq!(alone.len(), 100);
        assert!(alone.iter().all(|&v| (v - 1.25).abs() < 1e-6));
        // With every lane closed nothing is mixed, and nothing breaks.
        mixer.close_lane(0);
        mixer.close_lane(2);
        assert!(run(&mut mixer, &[(0, 0.25, 10)]).is_empty());
    }

    /// What a test makes a fake stream do at the next read.
    #[derive(Default)]
    struct Script {
        /// Packets (start in 100 ns units, frames, sample value), all read
        /// at the next wake.
        packets: Vec<(u64, usize, f32)>,
        /// The code its next read fails with.
        fail: Option<i32>,
        exited: bool,
    }

    struct FakeStream(Rc<RefCell<Script>>);

    impl LaneStream for FakeStream {
        fn drain(
            &mut self,
            gaps: &mut GapFill,
            deliver: &mut dyn FnMut(&[u8], usize, bool),
        ) -> Result<usize, i32> {
            let mut script = self.0.borrow_mut();
            if let Some(code) = script.fail.take() {
                return Err(code);
            }
            let packets = std::mem::take(&mut script.packets);
            for &(start, frames, value) in &packets {
                let (fill, skip, _) = gaps.packet(start, frames, false);
                if fill > 0 {
                    deliver(&[], fill, true);
                }
                let samples: Vec<f32> = (0..frames - skip).flat_map(|_| [value, value]).collect();
                deliver(as_bytes(&samples), frames - skip, false);
            }
            Ok(packets.len())
        }

        fn app_exited(&self) -> bool {
            self.0.borrow().exited
        }
    }

    fn opened(pid: u32, script: &Rc<RefCell<Script>>) -> Box<Opened<FakeStream>> {
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 2,
        };
        Box::new(Opened {
            pid,
            stream: FakeStream(Rc::clone(script)),
            input: LaneInput::new(format).unwrap(),
            rate: SESSION_RATE,
        })
    }

    /// A mix under test: its lanes, the frames mixed, the loud ones among
    /// them, and the apps whose streams left.
    struct Bench {
        lanes: MixLanes<FakeStream>,
        mixed: usize,
        loud: usize,
        retired: Vec<u32>,
    }

    impl Bench {
        fn new() -> Bench {
            Bench {
                lanes: MixLanes::new(0),
                mixed: 0,
                loud: 0,
                retired: Vec::new(),
            }
        }

        fn wake(&mut self, tick: u64) -> Result<(), i32> {
            let (mixed, loud, retired) = (&mut self.mixed, &mut self.loud, &mut self.retired);
            self.lanes.read(
                tick * 100_000,
                &mut |out: &[f32], n: usize, _: &[bool]| {
                    *mixed += n;
                    *loud += out[..n * 2].iter().step_by(2).filter(|&&v| v > 0.0).count();
                },
                &mut |gone: Box<Opened<FakeStream>>| retired.push(gone.pid),
            )
        }

        fn held(&self) -> Vec<u32> {
            let mut pids = [0; MAX_MIX_APPS];
            let count = self.lanes.held(&mut pids);
            pids[..count].to_vec()
        }
    }

    /// The first channel of a mix of two streams over `ticks` wakes: stream
    /// 1 plays `first(tick)` and stream 2 `second(tick)`, stream 1 alone is
    /// heard, and at `switch_at` the mix moves to every stream but it.
    fn switched(
        ticks: u64,
        switch_at: u64,
        first: impl Fn(u64) -> f32,
        second: impl Fn(u64) -> f32,
    ) -> Vec<f32> {
        let scripts: Vec<Rc<RefCell<Script>>> = (0..2)
            .map(|_| Rc::new(RefCell::new(Script::default())))
            .collect();
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(1, &scripts[0]), 0).is_ok());
        lanes.select(1, true, 0);
        assert!(lanes.adopt(opened(2, &scripts[1]), 0).is_ok());
        let mut mixed: Vec<f32> = Vec::new();
        for tick in 1..=ticks {
            let start = (tick - 1) * 100_000;
            scripts[0]
                .borrow_mut()
                .packets
                .push((start, 480, first(tick)));
            scripts[1]
                .borrow_mut()
                .packets
                .push((start, 480, second(tick)));
            if tick == switch_at {
                lanes.select(1, false, 0);
            }
            lanes
                .read(
                    tick * 100_000,
                    &mut |out: &[f32], n: usize, _: &[bool]| {
                        mixed.extend(out[..n * 2].iter().step_by(2));
                    },
                    &mut |_| panic!("no stream fails"),
                )
                .unwrap();
            assert!(tick != switch_at || !lanes.settled(), "the switch waits");
        }
        assert!(lanes.settled());
        mixed
    }

    // covers: S480
    #[test]
    fn a_switch_with_no_silence_fades_out_to_nothing_then_fades_the_other_stream_in() {
        let mixed = switched(150, 50, |_| 0.5, |_| 0.25);
        let start = mixed.iter().position(|&v| v > 0.0).unwrap();
        let mixed = &mixed[start..];
        assert!(mixed.iter().all(|&v| v <= 0.5 + 1e-6), "never both streams");
        let steepest = mixed
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max);
        assert!(steepest <= 0.5 / FADE_FRAMES + 1e-5, "no step: {steepest}");
        let silent = mixed.iter().position(|&v| v == 0.0).expect("faded out");
        assert!(mixed[..silent].iter().all(|&v| v > 0.0 && v <= 0.5));
        assert!(mixed[silent..].iter().all(|&v| v <= 0.25 + 1e-6));
        assert!(
            mixed[mixed.len() - 480..]
                .iter()
                .all(|&v| (v - 0.25).abs() < 1e-6)
        );
    }

    // covers: S480
    #[test]
    fn a_switch_waits_for_silence_and_changes_streams_there_without_a_fade() {
        let loud = |tick: u64| !(40..60).contains(&tick);
        let mixed = switched(
            100,
            30,
            |tick| if loud(tick) { 0.5 } else { 0.0 },
            |tick| if loud(tick) { 0.25 } else { 0.0 },
        );
        // Past the fade-in of the first stream at the start.
        let start = mixed.iter().position(|&v| v == 0.5).unwrap();
        let quiet = start + mixed[start..].iter().position(|&v| v == 0.0).unwrap();
        assert!(mixed[start..quiet].iter().all(|&v| (v - 0.5).abs() < 1e-6));
        let back = quiet + mixed[quiet..].iter().position(|&v| v > 0.0).unwrap();
        assert!(mixed[back..].iter().all(|&v| (v - 0.25).abs() < 1e-6));
    }

    // covers: S480
    #[test]
    fn a_stream_is_mixed_at_its_compensation_per_channel() {
        let script = Rc::new(RefCell::new(Script::default()));
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(1, &script), 0).is_ok());
        lanes.compensate(1, [2.0, 3.0]);
        let mut mixed: Vec<f32> = Vec::new();
        for tick in 1..=20_u64 {
            if tick == 10 {
                lanes.compensate(1, [1.0, 3.0]);
            }
            if tick == 15 {
                lanes.compensate(1, [2.0, 3.0]);
            }
            script
                .borrow_mut()
                .packets
                .push(((tick - 1) * 100_000, 480, 0.25));
            lanes
                .read(
                    tick * 100_000,
                    &mut |out: &[f32], n: usize, _: &[bool]| mixed.extend(&out[..n * 2]),
                    &mut |_| panic!("no stream fails"),
                )
                .unwrap();
        }
        assert_eq!(mixed[mixed.len() - 2..], [0.5, 0.75]);
        let left: Vec<f32> = mixed.iter().step_by(2).copied().collect();
        let start = left.iter().position(|&v| v == 0.5).unwrap();
        let steepest = left[start..]
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max);
        assert!(steepest <= 0.25 / FADE_FRAMES + 1e-5, "no step: {steepest}");
        assert!(left[start..].contains(&0.25), "reached the new gain");
    }

    /// Mixes `ticks` of 10 ms from each lane's script at its value, calling
    /// `at` before each tick's read; returns the left channel.
    fn mix_ticks(
        lanes: &mut MixLanes<FakeStream>,
        scripts: &[(u32, Rc<RefCell<Script>>, f32)],
        ticks: std::ops::RangeInclusive<u64>,
        mut at: impl FnMut(&mut MixLanes<FakeStream>, u64),
    ) -> Vec<f32> {
        let mut mixed: Vec<f32> = Vec::new();
        for tick in ticks {
            at(lanes, tick);
            for (pid, script, value) in scripts {
                if lanes.holds(*pid) {
                    script
                        .borrow_mut()
                        .packets
                        .push(((tick - 1) * 100_000, 480, *value));
                }
            }
            lanes
                .read(
                    tick * 100_000,
                    &mut |out: &[f32], n: usize, _: &[bool]| {
                        mixed.extend(out[..n * 2].iter().step_by(2));
                    },
                    &mut |_| {},
                )
                .unwrap();
        }
        mixed
    }

    // covers: S480
    #[test]
    fn a_stream_taking_a_slot_over_starts_without_the_last_ones_compensation() {
        let script = |_| Rc::new(RefCell::new(Script::default()));
        let scripts = [(1, script(1), 0.25), (2, script(2), 0.25)];
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(1, &scripts[0].1), 0).is_ok());
        lanes.compensate(1, [10.0, 10.0]);
        mix_ticks(&mut lanes, &scripts, 1..=10, |_, _| {});
        lanes.retire_unless(|pid| pid != 1, &mut |_| {});
        mix_ticks(&mut lanes, &scripts, 11..=20, |_, _| {});
        assert!(lanes.adopt(opened(2, &scripts[1].1), 2_000_000).is_ok());
        let mixed = mix_ticks(&mut lanes, &scripts, 21..=40, |_, _| {});
        assert!(mixed.iter().all(|&v| v <= 0.25 + 1e-6), "never louder");
        assert!((mixed[mixed.len() - 1] - 0.25).abs() < 1e-6);
    }

    const ENDPOINT: u32 = 1 << 31;

    // covers: S480
    #[test]
    fn without_an_endpoint_stream_the_whole_system_is_heard_from_its_own_stream() {
        let script = |_| Rc::new(RefCell::new(Script::default()));
        let scripts = [(1, script(1), 0.5), (5, script(5), 0.25)];
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(1, &scripts[0].1), 0).is_ok());
        assert!(lanes.adopt(opened(5, &scripts[1].1), 0).is_ok());
        let mixed = mix_ticks(&mut lanes, &scripts, 1..=60, |lanes, _| {
            lanes.select(1, false, ENDPOINT);
        });
        assert!(
            (mixed[mixed.len() - 1] - 0.5).abs() < 1e-6,
            "{}",
            mixed[mixed.len() - 1]
        );
    }

    // covers: S480
    #[test]
    fn without_its_own_stream_the_whole_system_hears_nothing_rather_than_the_endpoint() {
        let script = |_| Rc::new(RefCell::new(Script::default()));
        let scripts = [(ENDPOINT | 2, script(2), 0.5), (5, script(5), 0.25)];
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(ENDPOINT | 2, &scripts[0].1), 0).is_ok());
        assert!(lanes.adopt(opened(5, &scripts[1].1), 0).is_ok());
        let mixed = mix_ticks(&mut lanes, &scripts, 1..=60, |lanes, _| {
            lanes.select(1, true, ENDPOINT);
        });
        assert_eq!(mixed[mixed.len() - 1], 0.0);
        assert!(lanes.settled());
    }

    // covers: S480
    #[test]
    fn a_switch_to_its_own_stream_alone_fades_at_once_without_waiting_for_silence() {
        let script = |_| Rc::new(RefCell::new(Script::default()));
        let scripts = [(1, script(1), 0.5), (ENDPOINT | 2, script(2), 0.25)];
        let mut lanes = MixLanes::new(0);
        assert!(lanes.adopt(opened(1, &scripts[0].1), 0).is_ok());
        assert!(lanes.adopt(opened(ENDPOINT | 2, &scripts[1].1), 0).is_ok());
        let mixed = mix_ticks(&mut lanes, &scripts, 1..=60, |lanes, tick| {
            lanes.select(1, tick > 40, ENDPOINT);
        });
        assert!((mixed[39 * 480] - 0.25).abs() < 1e-6, "the endpoint before");
        assert!(
            mixed[43 * 480..].iter().all(|&v| (v - 0.5).abs() < 1e-6),
            "within 30 ms"
        );
    }

    /// Ten ms of app audio at `tick`, as its loopback stamps it.
    fn ten_ms(tick: u64) -> (u64, usize, f32) {
        ((tick - 1) * 100_000, 480, 0.25)
    }

    // covers: S386
    #[test]
    fn a_mix_takes_apps_in_and_a_failing_stream_only_silences_its_lane() {
        let scripts: Vec<Rc<RefCell<Script>>> = (0..3).map(|_| Rc::default()).collect();
        let mut bench = Bench::new();
        assert!(bench.lanes.adopt(opened(1, &scripts[1]), 0).is_ok());
        assert!(bench.lanes.adopt(opened(2, &scripts[2]), 0).is_ok());
        assert!(
            bench.lanes.adopt(opened(2, &scripts[2]), 0).is_err(),
            "once each"
        );
        // App 1 plays, app 2 is paused and never sends a packet.
        for tick in 1..=100 {
            scripts[1].borrow_mut().packets.push(ten_ms(tick));
            assert_eq!(bench.wake(tick), Ok(()));
            assert!(tick as usize * 480 - bench.mixed <= 960, "tick {tick}");
        }
        // App 2 quits and its stream fails: the mix goes on with app 1.
        scripts[2].borrow_mut().exited = true;
        scripts[2].borrow_mut().fail = Some(-1);
        assert_eq!(bench.wake(101), Ok(()));
        assert_eq!((bench.held(), bench.retired.clone()), (vec![1], vec![2]));
        // App 0 joins without a new session.
        assert!(
            bench
                .lanes
                .adopt(opened(0, &scripts[0]), 101 * 100_000)
                .is_ok()
        );
        for tick in 102..=150 {
            scripts[1].borrow_mut().packets.push(ten_ms(tick));
            assert_eq!(bench.wake(tick), Ok(()));
            assert!(tick as usize * 480 - bench.mixed <= 960, "tick {tick}");
        }
        assert!(bench.loud > 0);
        // Every stream fails while its app runs: Windows audio went away.
        scripts[0].borrow_mut().fail = Some(-2);
        scripts[1].borrow_mut().fail = Some(-2);
        assert_eq!(bench.wake(151), Err(-2));
        assert!(bench.held().is_empty());
    }

    // covers: S386
    #[test]
    fn a_stall_of_the_capture_thread_adds_no_delay() {
        let scripts: Vec<Rc<RefCell<Script>>> = (0..2).map(|_| Rc::default()).collect();
        let mut bench = Bench::new();
        assert!(bench.lanes.adopt(opened(0, &scripts[0]), 0).is_ok());
        assert!(bench.lanes.adopt(opened(1, &scripts[1]), 0).is_ok());
        for tick in 1..=50 {
            scripts[0].borrow_mut().packets.push(ten_ms(tick));
            bench.wake(tick).unwrap();
        }
        // 300 ms without a wake; the loopback's 100 ms buffer kept only
        // the last ten packets. App 1 stays paused.
        for tick in 71..=80 {
            scripts[0].borrow_mut().packets.push(ten_ms(tick));
        }
        bench.wake(80).unwrap();
        for tick in 81..=200 {
            scripts[0].borrow_mut().packets.push(ten_ms(tick));
            bench.wake(tick).unwrap();
            let due = tick as usize * 480;
            assert!(
                bench.mixed <= due && due - bench.mixed <= 960,
                "tick {tick}: {} of {due}",
                bench.mixed
            );
        }
    }

    // covers: S386
    #[test]
    fn a_free_lane_is_reused_only_once_it_handed_on_the_last_apps_audio() {
        let scripts: Vec<Rc<RefCell<Script>>> = (0..4).map(|_| Rc::default()).collect();
        let mut bench = Bench::new();
        assert!(bench.lanes.adopt(opened(1, &scripts[1]), 0).is_ok());
        assert!(bench.lanes.adopt(opened(2, &scripts[2]), 0).is_ok());
        // App 1 runs 20 ms ahead of app 2, then leaves with that waiting.
        scripts[1].borrow_mut().packets.push((0, 960, 0.25));
        bench.wake(1).unwrap();
        let mut retired = Vec::new();
        bench
            .lanes
            .retire_unless(|pid| pid != 1, &mut |gone| retired.push(gone.pid));
        assert_eq!(retired, [1]);
        assert!(bench.lanes.adopt(opened(3, &scripts[3]), 100_000).is_ok());
        assert_eq!(
            bench.held(),
            [2, 3],
            "app 3 took the next lane, not app 1's"
        );
    }

    // covers: S386
    #[test]
    fn a_mix_of_no_app_writes_silence_in_real_time() {
        let mut bench = Bench::new();
        for tick in 1..=100_u64 {
            bench.wake(tick).unwrap();
        }
        assert_eq!(bench.mixed, 99 * 480, "10 ms behind now, the fill's lag");
    }

    // covers: S386
    #[test]
    fn a_plan_is_read_once_per_change() {
        let plan = MixPlan::default();
        let mut pids = [0; MAX_MIX_APPS];
        assert_eq!(plan.read(0, &mut pids), None);
        plan.set(&[7, 9]);
        let (count, version) = plan.read(0, &mut pids).expect("a new plan");
        assert_eq!(&pids[..count], [7, 9]);
        assert_eq!(plan.read(version, &mut pids), None);
        plan.set(&[]);
        assert_eq!(
            plan.read(version, &mut pids).map(|(count, _)| count),
            Some(0)
        );
    }

    // covers: S386
    #[test]
    fn a_lane_input_converts_to_stereo_at_the_session_rate() {
        let format = StreamFormat {
            sample: SampleType::I16,
            rate: 44_100,
            channels: 2,
        };
        let mut input = LaneInput::new(format).expect("format");
        let packet = vec![0u8; 441 * 4];
        let mut frames = 0;
        for _ in 0..100 {
            input.convert(&packet, 441, false, &mut |stereo: &[f32], _| {
                frames += stereo.len() / 2
            });
        }
        // One second at 44.1 kHz comes out as about one second at 48 kHz.
        assert!((47_900..=48_000).contains(&frames), "{frames}");
        assert!(
            LaneInput::new(StreamFormat {
                channels: 0,
                ..format
            })
            .is_err()
        );
    }

    // covers: S229 (zero holes in a mix)
    #[test]
    fn a_mix_marks_frames_whose_every_lane_gave_flagged_or_filled_silence() {
        let mut mixer = StreamMixer::new(2);
        let mut filled = Vec::new();
        let mut sink = |_: &[f32], n: usize, flags: &[bool]| filled.extend_from_slice(&flags[..n]);
        // Lane 1 is a quiet app, filled; lane 0 plays, gets 10 ms of gap
        // fill, plays again, then sends 10 ms of data zeros.
        mixer.push(1, &[0.0; 1_920 * 2], true, &mut sink);
        mixer.push(0, &[0.25; 480 * 2], false, &mut sink);
        mixer.push(0, &[0.0; 480 * 2], true, &mut sink);
        mixer.push(0, &[0.25; 480 * 2], false, &mut sink);
        mixer.push(0, &[0.0; 480 * 2], false, &mut sink);
        assert_eq!(filled.len(), 1_920);
        assert!(filled[..480].iter().all(|&f| !f), "sound");
        assert!(filled[480..960].iter().all(|&f| f), "the gap fill");
        assert!(filled[960..].iter().all(|&f| !f), "sound, then data zeros");
    }

    #[test]
    fn mixed_frames_read_back_as_the_same_floats() {
        let samples = [0.5_f32, -0.25];
        let bytes = as_bytes(&samples);
        assert_eq!(bytes.len(), 8);
        assert_eq!(f32::from_le_bytes(bytes[4..8].try_into().unwrap()), -0.25);
    }
}
