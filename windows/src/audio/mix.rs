//! Several process loopback captures mixed into one stream: a source made of
//! several apps, or of every app except some, which one loopback can't
//! capture.
//!
//! Each loopback runs on its own clock and delivers its packets at its own
//! moments, so each stream first becomes stereo float at [`SESSION_RATE`]
//! ([`LaneInput`]) and waits in its own lane of the [`StreamMixer`], which
//! sums the lanes as far as all of them reach. Everything is allocated up
//! front; [`LaneInput::convert`] and [`StreamMixer::push`] never allocate,
//! lock or block.

use audiosync_core::channels::input_to_stereo;
use audiosync_core::resample::Resampler;
use audiosync_core::ring::is_supported_rate;

use super::SESSION_RATE;
use super::backend::BackendError;
use super::capture::{CONVERT_CHUNK, StreamFormat, decode};

/// How far a lane may run ahead of the slowest one, 60 ms: more than a
/// quiet loopback waits before its silence is filled (25 ms, then 10 ms
/// behind now, `GapFill`) plus one wait of the capture loop.
pub const MAX_LAG_FRAMES: usize = SESSION_RATE as usize * 60 / 1000;

/// What a lead past [`MAX_LAG_FRAMES`] is cut back to.
const TARGET_LAG_FRAMES: usize = MAX_LAG_FRAMES / 2;

/// Room in a lane: the lag it may reach plus one piece of input.
const LANE_FRAMES: usize = MAX_LAG_FRAMES + CONVERT_CHUNK;

/// One stream's frames waiting to be mixed, stereo, in a ring buffer.
struct Lane {
    samples: Box<[f32]>,
    /// The first waiting frame.
    start: usize,
    /// Waiting frames.
    len: usize,
}

impl Lane {
    fn new() -> Lane {
        Lane {
            samples: vec![0.0; LANE_FRAMES * 2].into_boxed_slice(),
            start: 0,
            len: 0,
        }
    }

    /// Appends whole frames from `stereo`; the caller keeps them within room.
    fn append(&mut self, stereo: &[f32]) {
        for frame in stereo.as_chunks::<2>().0 {
            let at = (self.start + self.len) % LANE_FRAMES * 2;
            self.samples[at..at + 2].copy_from_slice(frame);
            self.len += 1;
        }
    }

    /// Adds the first `frames` waiting frames into `out` and drops them.
    fn take_into(&mut self, out: &mut [f32], frames: usize) {
        for frame in out.as_chunks_mut::<2>().0.iter_mut().take(frames) {
            let at = self.start * 2;
            frame[0] += self.samples[at];
            frame[1] += self.samples[at + 1];
            self.start = (self.start + 1) % LANE_FRAMES;
            self.len -= 1;
        }
    }
}

/// Mixes several streams of stereo frames at the session rate.
pub struct StreamMixer {
    lanes: Box<[Lane]>,
    /// One chunk of the mix.
    out: Box<[f32]>,
}

impl StreamMixer {
    /// A mixer for `streams` streams.
    pub fn new(streams: usize) -> StreamMixer {
        StreamMixer {
            lanes: (0..streams).map(|_| Lane::new()).collect(),
            out: vec![0.0; CONVERT_CHUNK * 2].into_boxed_slice(),
        }
    }

    /// Adds `stereo`'s whole frames to `lane` and hands every frame of the
    /// mix that is ready to `sink`, in chunks of at most [`CONVERT_CHUNK`].
    pub fn push(&mut self, lane: usize, stereo: &[f32], sink: &mut impl FnMut(&[f32], usize)) {
        if lane >= self.lanes.len() {
            return;
        }
        let mut rest = &stereo[..stereo.len() / 2 * 2];
        while !rest.is_empty() {
            let piece = (rest.len() / 2).min(CONVERT_CHUNK);
            self.lanes[lane].append(&rest[..piece * 2]);
            rest = &rest[piece * 2..];
            self.mix(sink);
        }
    }

    /// Frames waiting in `lane`.
    #[cfg(test)]
    fn waiting(&self, lane: usize) -> usize {
        self.lanes[lane].len
    }

    /// Hands on what every lane has reached, then cuts back a lead past the
    /// allowed lag.
    fn mix(&mut self, sink: &mut impl FnMut(&[f32], usize)) {
        let ready = self.lanes.iter().map(|lane| lane.len).min().unwrap_or(0);
        self.emit(ready, sink);
        // Drift rule: once a lane leads the slowest by over MAX_LAG, the mix moves on to half that lead, padding short lanes with silence and dropping nothing.
        let lead = self.lanes.iter().map(|lane| lane.len).max().unwrap_or(0);
        if lead > MAX_LAG_FRAMES {
            self.emit(lead - TARGET_LAG_FRAMES, sink);
        }
    }

    /// Mixes `frames` frames, each lane adding what it has of them.
    fn emit(&mut self, frames: usize, sink: &mut impl FnMut(&[f32], usize)) {
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CONVERT_CHUNK);
            let out = &mut self.out[..n * 2];
            out.fill(0.0);
            for lane in self.lanes.iter_mut() {
                let take = lane.len.min(n);
                lane.take_into(out, take);
            }
            sink(out, n);
            done += n;
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
    /// `out` piece by piece.
    pub fn convert(
        &mut self,
        data: &[u8],
        frames: usize,
        silent: bool,
        out: &mut impl FnMut(&[f32]),
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
                None => out(&self.stereo[..n * 2]),
                Some(resampler) => {
                    let mut consumed = 0;
                    while consumed < n {
                        let (used, produced) = resampler
                            .process(&self.stereo[consumed * 2..n * 2], &mut self.resampled);
                        out(&self.resampled[..produced * 2]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::SampleType;

    /// Every frame `mixer` hands on, left channel only.
    fn run(mixer: &mut StreamMixer, pushes: &[(usize, f32, usize)]) -> Vec<f32> {
        let mut mixed = Vec::new();
        for &(lane, value, frames) in pushes {
            let block = vec![value; frames * 2];
            mixer.push(lane, &block, &mut |out: &[f32], n: usize| {
                mixed.extend(out[..n * 2].iter().step_by(2));
            });
        }
        mixed
    }

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

    #[test]
    fn one_stream_passes_straight_through() {
        let mut mixer = StreamMixer::new(1);
        assert_eq!(run(&mut mixer, &[(0, 0.1, 5_000)]).len(), 5_000);
        assert_eq!(mixer.waiting(0), 0);
    }

    #[test]
    fn a_stalled_lane_never_holds_the_mix_back_more_than_the_lag_bound() {
        let mut mixer = StreamMixer::new(2);
        // Lane 1 delivers nothing for 200 ms.
        let mixed = run(&mut mixer, &[(0, 0.25, 9_600)]);
        assert!(mixer.waiting(0) <= MAX_LAG_FRAMES);
        assert_eq!(mixed.len() + mixer.waiting(0), 9_600);
        assert!(mixed.iter().all(|&v| (v - 0.25).abs() < 1e-6));
        // When it comes back, its frames join the leader's waiting ones.
        let behind = mixer.waiting(0);
        let caught_up = run(&mut mixer, &[(1, 0.5, behind)]);
        assert_eq!(caught_up.len(), behind);
        assert!(caught_up.iter().all(|&v| (v - 0.75).abs() < 1e-6));
        assert_eq!((mixer.waiting(0), mixer.waiting(1)), (0, 0));
    }

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
            input.convert(&packet, 441, false, &mut |stereo: &[f32]| {
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

    #[test]
    fn mixed_frames_read_back_as_the_same_floats() {
        let samples = [0.5_f32, -0.25];
        let bytes = as_bytes(&samples);
        assert_eq!(bytes.len(), 8);
        assert_eq!(f32::from_le_bytes(bytes[4..8].try_into().unwrap()), -0.25);
    }
}
