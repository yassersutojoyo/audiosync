//! Interleaved stereo held back by a delay that may change while it plays
//! (C77): a change fades out, moves the read point and fades in, so it never
//! clicks. Only [`DelayLine::new`] allocates, and it touches every page of
//! the buffer, so the real-time thread never takes a first-touch page fault.

use crate::fade::Fade;

/// The smallest page size the hosts map memory in.
const PAGE_BYTES: usize = 4096;

/// Samples handed on per call of the output closure.
const CHUNK_SAMPLES: usize = 512;

pub struct DelayLine {
    buffer: Box<[f32]>,
    /// The frame written next.
    write: usize,
    /// Frames written since the last reset, at most the capacity: older ones
    /// read as silence.
    filled: usize,
    /// The delay in frames; `None` until the first frame after a reset, which
    /// takes the target at once since only silence is behind it.
    delay: Option<usize>,
    /// [`DelayLine::close`] ran: the output fades out and stays silent.
    closing: bool,
    fade: Fade,
}

impl DelayLine {
    /// A line that holds back up to `max_frames` frames, fading over the
    /// fade length at `rate`. Allocates; never on a real-time thread.
    pub fn new(max_frames: usize, rate: u32) -> DelayLine {
        let mut buffer = vec![0.0_f32; (max_frames + 1) * 2].into_boxed_slice();
        let last = buffer.len() - 1;
        let stride = PAGE_BYTES / size_of::<f32>();
        for index in (0..last).step_by(stride).chain([last]) {
            // SAFETY: `index` is in bounds, so the pointer is valid and aligned.
            unsafe { std::ptr::write_volatile(&raw mut buffer[index], 0.0) };
        }
        DelayLine {
            buffer,
            write: 0,
            filled: 0,
            delay: None,
            closing: false,
            fade: Fade::opened(rate),
        }
    }

    /// The longest delay it holds, in frames.
    pub fn max_frames(&self) -> usize {
        self.buffer.len() / 2 - 1
    }

    /// Forgets the audio taken in, so a new stream never replays the end of
    /// the old one, and opens a closed line. Real-time safe.
    pub fn reset(&mut self) {
        self.filled = 0;
        self.delay = None;
        self.closing = false;
        self.fade.open();
    }

    /// Fades the output out over the fade length and keeps it silent until
    /// [`DelayLine::reset`]: the stream is stopping, and what the line still
    /// holds would otherwise end it with a cut. Real-time safe.
    pub fn close(&mut self) {
        self.closing = true;
    }

    /// Takes in the whole frames of `input` and hands as many frames, held
    /// back by `target` frames (at most [`DelayLine::max_frames`]), to `out`.
    /// Real-time safe: never allocates, locks or blocks.
    pub fn process(&mut self, input: &[f32], target: usize, mut out: impl FnMut(&[f32])) {
        let capacity = self.buffer.len() / 2;
        let target = target.min(capacity - 1);
        let mut chunk = [0.0_f32; CHUNK_SAMPLES];
        for piece in input.chunks(CHUNK_SAMPLES) {
            let frames = piece.len() / 2;
            for (frame, held) in piece.chunks_exact(2).zip(chunk.chunks_exact_mut(2)) {
                let delay = *self.delay.get_or_insert(target);
                if self.closing {
                    self.fade.fade_out();
                } else if delay == target {
                    self.fade.fade_in();
                } else if self.fade.is_closed() {
                    self.delay = Some(target);
                    self.fade.fade_in();
                } else {
                    self.fade.fade_out();
                }
                let delay = self.delay.unwrap_or(target);
                let write = self.write * 2;
                self.buffer[write..write + 2].copy_from_slice(frame);
                self.filled = (self.filled + 1).min(capacity);
                let gain = self.fade.next_gain();
                if delay < self.filled {
                    let read = (self.write + capacity - delay) % capacity * 2;
                    held[0] = self.buffer[read] * gain;
                    held[1] = self.buffer[read + 1] * gain;
                } else {
                    held.fill(0.0);
                }
                self.write = (self.write + 1) % capacity;
            }
            out(&chunk[..frames * 2]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn delayed(line: &mut DelayLine, input: &[f32], target: usize) -> Vec<f32> {
        let mut out = Vec::new();
        line.process(input, target, |held| out.extend_from_slice(held));
        out
    }

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames * 2).map(|i| (i + 1) as f32 / 8000.0).collect()
    }

    #[test]
    fn audio_passes_unchanged_or_late_by_the_target() {
        let input = ramp(1000);
        assert_eq!(delayed(&mut DelayLine::new(4800, RATE), &input, 0), input);
        let out = delayed(&mut DelayLine::new(4800, RATE), &input, 100);
        assert!(out[..200].iter().all(|sample| *sample == 0.0));
        assert_eq!(out[200..], input[..1800]);
        let mut short = DelayLine::new(50, RATE);
        assert_eq!(short.max_frames(), 50);
        let out = delayed(&mut short, &input, 100);
        assert_eq!(out[100..], input[..1900], "held back at most its length");
    }

    #[test]
    fn a_reset_never_replays_the_old_stream() {
        let mut line = DelayLine::new(4800, RATE);
        delayed(&mut line, &ramp(1000), 300);
        line.reset();
        let input = ramp(500);
        let out = delayed(&mut line, &input, 300);
        assert!(
            out[..600].iter().all(|sample| *sample == 0.0),
            "silence, not the old ramp"
        );
        assert_eq!(out[600..], input[..400]);
    }

    #[test]
    fn a_closed_line_fades_out_and_stays_silent_until_a_reset() {
        let mut line = DelayLine::new(4800, RATE);
        let input = vec![0.5_f32; 4000];
        delayed(&mut line, &input, 100);
        line.close();
        let out = delayed(&mut line, &input, 100);
        let fade = Fade::new(RATE).len();
        assert!(out[0] > 0.0 && out[0] <= 0.5);
        assert!(
            out.chunks_exact(2)
                .zip(out.chunks_exact(2).skip(1))
                .all(|(a, b)| b[0] <= a[0]),
            "falls without a jump back up"
        );
        assert!(out[fade * 2..].iter().all(|sample| *sample == 0.0));
        line.reset();
        let out = delayed(&mut line, &input, 0);
        assert_eq!(out, input, "open again at once");
    }

    #[test]
    fn a_new_target_fades_out_and_in_without_a_jump() {
        let step = std::f32::consts::TAU * 440.0 / RATE as f32;
        let sine: Vec<f32> = (0..24_000)
            .flat_map(|i| {
                let sample = 0.5 * (i as f32 * step).sin();
                [sample, sample]
            })
            .collect();
        let mut line = DelayLine::new(48_000, RATE);
        let mut out = delayed(&mut line, &sine[..9600], 0);
        out.extend(delayed(&mut line, &sine[9600..], 12_000));
        let largest = out
            .chunks_exact(2)
            .zip(out.chunks_exact(2).skip(1))
            .map(|(a, b)| (b[0] - a[0]).abs())
            .fold(0.0, f32::max);
        // A sine of amplitude 0.5 moves at most 0.5 * step per frame.
        assert!(largest < 0.5 * step * 1.5, "largest step {largest}");
        assert_eq!(
            out[40_000..],
            sine[16_000..24_000],
            "then 12 000 frames late"
        );
    }
}
