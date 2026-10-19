//! Click-free fades at stream boundaries (S182, S189, S190).
//!
//! A fade is a gain w applied alike to every channel of a frame over
//! F = round(0.020 × R) frames at the rate R of the stream it is applied to.
//! A fade-in follows w = ½ − ½·cos(π·x) and a fade-out w = ½ + ½·cos(π·x),
//! with x = (i + 1) / F for frame i; the last frame of a fade-in is exactly 1
//! and the last frame of a fade-out exactly 0 (S182).
//!
//! [`Fade`] is the state machine: open (w = 1, frames pass unchanged), fading
//! in, fading out, or closed (exact zeros, whatever the input). A request for
//! the other direction while a fade is under way continues on the other
//! curve from the frame whose w equals the current one, so w never jumps; a
//! request for the state a stream is in or heading to changes nothing.
//!
//! [`InputFade`] wraps a [`Fade`] for an input that hands blocks on to a
//! converter, the AirPlay sender's native input (S189): it fades in after
//! each restart and out after a stop request, and leaves the caller's block
//! untouched, so the ring and the AirPlay leg still get bit-identical input.
//!
//! Real time (S190): w is computed per frame, with no table, so nothing here
//! allocates after construction; every call is O(frames) and never locks,
//! blocks or logs.

#![forbid(unsafe_code)]

use std::f64::consts::PI;

/// A fade's length in milliseconds (S182).
pub const FADE_MS: u32 = 20;

/// F at `rate`: `round(0.020 × rate)` frames, at least 1 (S182). 960 at
/// 48 kHz, 882 at 44.1 kHz. Computed in integers, so ties round up exactly.
pub fn fade_frames(rate: u32) -> usize {
    ((u64::from(FADE_MS) * u64::from(rate) + 500) / 1000).max(1) as usize
}

/// w of frame `index` of a fade-in of `len` frames: ½ − ½·cos(π·(i + 1) / F),
/// exactly 1 from the last frame on (S182).
pub fn fade_in_gain(index: usize, len: usize) -> f32 {
    if index + 1 >= len {
        return 1.0;
    }
    let x = (index + 1) as f64 / len as f64;
    (0.5 - 0.5 * (PI * x).cos()) as f32
}

/// w of frame `index` of a fade-out of `len` frames: ½ + ½·cos(π·(i + 1) / F),
/// exactly 0 from the last frame on (S182).
pub fn fade_out_gain(index: usize, len: usize) -> f32 {
    if index + 1 >= len {
        return 0.0;
    }
    let x = (index + 1) as f64 / len as f64;
    (0.5 + 0.5 * (PI * x).cos()) as f32
}

/// Where a stream is (S182).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeState {
    /// w = 1: frames pass unchanged.
    Open,
    /// On the fade-in curve.
    FadingIn,
    /// On the fade-out curve.
    FadingOut,
    /// w = 0: exact zeros, whatever the input.
    Closed,
}

/// A stream's fade state machine (S182). Plain data: copying it copies the
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fade {
    len: usize,
    state: FadeState,
    /// While fading: the index on the current curve of the next frame.
    next: usize,
}

impl Fade {
    /// A closed stream at `rate`.
    pub fn new(rate: u32) -> Fade {
        Fade {
            len: fade_frames(rate),
            state: FadeState::Closed,
            next: 0,
        }
    }

    /// An open stream at `rate`.
    pub fn opened(rate: u32) -> Fade {
        Fade {
            state: FadeState::Open,
            ..Fade::new(rate)
        }
    }

    /// F, in frames.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: a fade is at least one frame long.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Where the stream is.
    pub fn state(&self) -> FadeState {
        self.state
    }

    /// Whether w = 1.
    pub fn is_open(&self) -> bool {
        self.state == FadeState::Open
    }

    /// Whether w = 0.
    pub fn is_closed(&self) -> bool {
        self.state == FadeState::Closed
    }

    /// Frames left until the fade under way ends; 0 while open or closed.
    pub fn remaining(&self) -> usize {
        match self.state {
            FadeState::FadingIn | FadeState::FadingOut => self.len - self.next,
            FadeState::Open | FadeState::Closed => 0,
        }
    }

    /// The current level: w of the last frame rendered, 1 before the first
    /// frame of a fade-out and 0 before the first frame of a fade-in.
    pub fn level(&self) -> f32 {
        match self.state {
            FadeState::Open => 1.0,
            FadeState::Closed => 0.0,
            FadeState::FadingIn if self.next == 0 => 0.0,
            FadeState::FadingIn => fade_in_gain(self.next - 1, self.len),
            FadeState::FadingOut if self.next == 0 => 1.0,
            FadeState::FadingOut => fade_out_gain(self.next - 1, self.len),
        }
    }

    /// Sets the rate for the next fade (F follows it); a fade under way keeps
    /// its level and runs out at the new length.
    pub fn set_rate(&mut self, rate: u32) {
        let len = fade_frames(rate);
        if len == self.len {
            return;
        }
        if matches!(self.state, FadeState::FadingIn | FadeState::FadingOut) {
            // Same position on the curve, in the new length's frames.
            self.next = (self.next * len / self.len).min(len - 1);
        }
        self.len = len;
    }

    /// Starts a fade-in; continues from the fade-out's level when one is
    /// under way; nothing while open or fading in.
    pub fn fade_in(&mut self) {
        match self.state {
            FadeState::Open | FadeState::FadingIn => {}
            FadeState::Closed => {
                self.state = FadeState::FadingIn;
                self.next = 0;
            }
            FadeState::FadingOut => {
                // The fade-in frame whose w equals the fade-out's level is
                // F - 1 - next; the next frame follows it.
                let next = self.len - self.next;
                if next >= self.len {
                    self.state = FadeState::Open;
                    self.next = 0;
                } else {
                    self.state = FadeState::FadingIn;
                    self.next = next;
                }
            }
        }
    }

    /// Starts a fade-out; continues from the fade-in's level when one is
    /// under way; nothing while closed or fading out.
    pub fn fade_out(&mut self) {
        match self.state {
            FadeState::Closed | FadeState::FadingOut => {}
            FadeState::Open => {
                self.state = FadeState::FadingOut;
                self.next = 0;
            }
            FadeState::FadingIn => {
                let next = self.len - self.next;
                if next >= self.len {
                    self.state = FadeState::Closed;
                    self.next = 0;
                } else {
                    self.state = FadeState::FadingOut;
                    self.next = next;
                }
            }
        }
    }

    /// Opens at once (w = 1), ending any fade.
    pub fn open(&mut self) {
        self.state = FadeState::Open;
        self.next = 0;
    }

    /// Closes at once (w = 0), ending any fade.
    pub fn close(&mut self) {
        self.state = FadeState::Closed;
        self.next = 0;
    }

    /// w of the next frame, moving on by one frame.
    pub fn next_gain(&mut self) -> f32 {
        match self.state {
            FadeState::Open => 1.0,
            FadeState::Closed => 0.0,
            FadeState::FadingIn => {
                let w = fade_in_gain(self.next, self.len);
                self.advance(FadeState::Open);
                w
            }
            FadeState::FadingOut => {
                let w = fade_out_gain(self.next, self.len);
                self.advance(FadeState::Closed);
                w
            }
        }
    }

    fn advance(&mut self, end: FadeState) {
        self.next += 1;
        if self.next >= self.len {
            self.state = end;
            self.next = 0;
        }
    }

    /// Applies the fade to the whole frames of `buffer`, interleaved with
    /// `channels` channels, in place: open frames are left as they are,
    /// closed ones become exact zeros, and fading ones are multiplied by w
    /// after NaN and ±infinity are replaced by 0. Returns the index of the
    /// frame that ended a fade-out (its last frame, which is 0), if one
    /// ended here. A trailing partial frame is left as it is.
    pub fn apply(&mut self, buffer: &mut [f32], channels: usize) -> Option<usize> {
        if channels == 0 {
            return None;
        }
        let frames = buffer.len() / channels;
        let mut ended = None;
        let mut index = 0;
        while index < frames {
            match self.state {
                FadeState::Open => break,
                FadeState::Closed => {
                    buffer[index * channels..frames * channels].fill(0.0);
                    break;
                }
                FadeState::FadingIn | FadeState::FadingOut => {
                    let out = self.state == FadeState::FadingOut;
                    let w = self.next_gain();
                    for sample in &mut buffer[index * channels..(index + 1) * channels] {
                        *sample = if sample.is_finite() { *sample * w } else { 0.0 };
                    }
                    if out && self.is_closed() {
                        ended = Some(index);
                    }
                    index += 1;
                }
            }
        }
        ended
    }
}

/// The fade of an input that hands each block on to a converter: the AirPlay
/// sender's native input (S189). It fades in over F frames at the capture
/// rate after each [`InputFade::restart`] (a rate set at a capture build) and
/// out over F frames after [`InputFade::fade_out`] (a stop request), then
/// gives zeros until the next restart. The caller's block is never changed:
/// while the input is open it is handed on as it is, otherwise a faded copy
/// is, in pieces of at most the chunk given at construction.
#[derive(Debug)]
pub struct InputFade {
    fade: Fade,
    scratch: Box<[f32]>,
}

impl InputFade {
    /// A closed input at `rate` whose faded copies come in pieces of at most
    /// `chunk_frames` stereo frames (at least 1). Allocates its buffer here.
    pub fn new(rate: u32, chunk_frames: usize) -> InputFade {
        InputFade {
            fade: Fade::new(rate),
            scratch: vec![0.0; chunk_frames.max(1) * 2].into_boxed_slice(),
        }
    }

    /// Starts the input afresh at `rate`: closed, then a fade-in from the
    /// next frame on.
    pub fn restart(&mut self, rate: u32) {
        self.fade = Fade::new(rate);
        self.fade.fade_in();
    }

    /// Fades out over the next F frames, then gives zeros until the next
    /// [`InputFade::restart`].
    pub fn fade_out(&mut self) {
        self.fade.fade_out();
    }

    /// Where the input is.
    pub fn state(&self) -> FadeState {
        self.fade.state()
    }

    /// Hands the whole stereo frames of `block` to `deliver`, faded: the block
    /// itself while open, faded copies otherwise. Never allocates.
    pub fn process(&mut self, block: &[f32], mut deliver: impl FnMut(&[f32])) {
        let samples = block.len() / 2 * 2;
        if self.fade.is_open() {
            if samples > 0 {
                deliver(&block[..samples]);
            }
            return;
        }
        let mut done = 0;
        while done < samples {
            if self.fade.is_open() {
                deliver(&block[done..samples]);
                return;
            }
            let n = (samples - done).min(self.scratch.len());
            let piece = &mut self.scratch[..n];
            piece.copy_from_slice(&block[done..done + n]);
            self.fade.apply(piece, 2);
            deliver(piece);
            done += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S182
    #[test]
    fn the_curves_meet_their_ends_exactly() {
        assert_eq!(fade_in_gain(959, 960), 1.0);
        assert_eq!(fade_out_gain(959, 960), 0.0);
        assert_eq!(fade_frames(0), 1);
        for index in 0..960 {
            let sum = fade_in_gain(index, 960) + fade_out_gain(index, 960);
            assert!((sum - 1.0).abs() < 1e-6, "{index}");
        }
    }

    // covers: S182
    #[test]
    fn a_new_rate_keeps_the_level_of_a_fade_under_way() {
        let mut fade = Fade::new(48_000);
        fade.fade_in();
        for _ in 0..480 {
            fade.next_gain();
        }
        let before = fade.level();
        fade.set_rate(96_000);
        assert_eq!(fade.len(), 1920);
        assert!((fade.next_gain() - before).abs() < 2e-3);
        assert_eq!(fade.remaining(), 959);
    }

    // covers: S189
    #[test]
    fn an_input_fade_leaves_the_callers_block_alone() {
        let mut input = InputFade::new(48_000, 100);
        input.restart(48_000);
        let block = vec![0.5f32; 960];
        let mut delivered = Vec::new();
        input.process(&block, |piece| delivered.extend_from_slice(piece));
        assert!(block.iter().all(|&sample| sample == 0.5));
        assert_eq!(delivered.len(), 960);
        assert!(delivered[0] < 1e-5);
    }
}
