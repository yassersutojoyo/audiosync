//! An output's render pipeline: from the shared ring, through the output's
//! delay reader, to the device's rate, channels and gain.
//!
//! Per device callback the pipeline reads session-rate frames through its
//! [`Reader`] (at the output's delay, with its own drift correction), converts
//! them to the device's rate with the core's resampler, sizing each read from
//! what the resampler needs (S62), maps stereo onto the device's channels
//! (S63) and applies the output's gain, ramped over one callback when it
//! changes (S64, S65). Delay and lag stay in session-rate frames.

use std::sync::Arc;

use audiosync_core::convert::map_to_device;
use audiosync_core::reader::Reader;
use audiosync_core::resample::Resampler;
use audiosync_core::volume::{GainStage, SharedGain};

use super::SESSION_RATE;
use super::backend::BackendError;

/// Device frames rendered per step; bounds the pipeline's buffers.
pub const RENDER_CHUNK: usize = 512;

/// The most session-rate frames the resampler can ask for to produce
/// `device_frames` frames at `device_rate`, whatever state it is in: the
/// frames those outputs span, plus one step and the rounding on each side.
pub fn reader_frames_bound(device_frames: usize, device_rate: u32) -> usize {
    if device_rate == SESSION_RATE {
        return device_frames;
    }
    let frames = device_frames as u64;
    let from = u64::from(SESSION_RATE);
    let to = u64::from(device_rate.max(1));
    ((frames * from).div_ceil(to) + from / to + 4) as usize
}

/// The render pipeline of one output, owned by its render thread.
///
/// All buffers are allocated by [`RenderPipeline::new`];
/// [`RenderPipeline::render`] never allocates, locks or blocks.
pub struct RenderPipeline {
    reader: Reader,
    resampler: Resampler,
    stage: GainStage,
    gain: Arc<SharedGain>,
    channels: usize,
    /// Session-rate stereo frames from the reader.
    stereo: Box<[f32]>,
    /// Device-rate stereo frames from the resampler.
    converted: Box<[f32]>,
    /// Device frames the resampler could not produce and that were played
    /// as silence. Stays 0 unless [`reader_frames_bound`] is wrong.
    shortfall_frames: u64,
}

impl RenderPipeline {
    /// A pipeline that reads through `reader` and plays at the gain in
    /// `gain`, for a device running at `device_rate` with `channels`
    /// interleaved float channels.
    pub fn new(
        reader: Reader,
        gain: Arc<SharedGain>,
        device_rate: u32,
        channels: usize,
    ) -> Result<RenderPipeline, BackendError> {
        if channels == 0 {
            return Err(BackendError::UnsupportedFormat);
        }
        let resampler = Resampler::new(SESSION_RATE, device_rate)
            .map_err(|_| BackendError::UnsupportedFormat)?;
        let read_frames = reader_frames_bound(RENDER_CHUNK, device_rate);
        Ok(RenderPipeline {
            reader,
            resampler,
            stage: GainStage::new(),
            gain,
            channels,
            stereo: vec![0.0; read_frames * 2].into_boxed_slice(),
            converted: vec![0.0; RENDER_CHUNK * 2].into_boxed_slice(),
            shortfall_frames: 0,
        })
    }

    /// Channels per device frame.
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "the WASAPI render loop sizes its packets with it")
    )]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The device rate.
    #[cfg(test)]
    pub fn device_rate(&self) -> u32 {
        self.resampler.output_rate()
    }

    /// Device frames played as silence because the resampler lacked input.
    #[cfg(test)]
    pub fn shortfall_frames(&self) -> u64 {
        self.shortfall_frames
    }

    /// Fills `out`, interleaved with [`RenderPipeline::channels`] channels,
    /// at the output's gain.
    pub fn render(&mut self, out: &mut [f32]) {
        let gain = self.gain.get();
        self.render_at(out, gain);
    }

    /// Fills `out` with the gain going to 0: the first call fades out over
    /// its buffer, later calls are silent. Used before an output stops, so it
    /// doesn't stop with a click.
    pub fn render_fade_out(&mut self, out: &mut [f32]) {
        self.render_at(out, 0.0);
    }

    fn render_at(&mut self, out: &mut [f32], target_gain: f32) {
        let channels = self.channels;
        let frames = out.len() / channels;
        let read_capacity = self.stereo.len() / 2;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(RENDER_CHUNK);
            let need = self.resampler.input_frames_for(n).min(read_capacity);
            if need > 0 {
                self.reader.read_target(&mut self.stereo[..need * 2], need);
            }
            let (_, produced) = self
                .resampler
                .process(&self.stereo[..need * 2], &mut self.converted[..n * 2]);
            if produced < n {
                self.converted[produced * 2..n * 2].fill(0.0);
                self.shortfall_frames += (n - produced) as u64;
            }
            map_to_device(
                &self.converted[..n * 2],
                n,
                1.0,
                &mut out[done * channels..(done + n) * channels],
                channels,
            );
            done += n;
        }
        // A trailing partial frame is never played; keep it silent.
        out[frames * channels..].fill(0.0);
        self.stage
            .apply(&mut out[..frames * channels], channels, target_gain);
    }
}

#[cfg(test)]
mod tests {
    use audiosync_core::ring::{Ring, RingWriter};

    use super::*;

    struct Setup {
        writer: RingWriter,
        gain: Arc<SharedGain>,
        pipeline: RenderPipeline,
    }

    /// A ring at the session rate and a pipeline reading it at `delay_frames`.
    fn setup(device_rate: u32, channels: usize, delay_frames: i64, gain: f32) -> Setup {
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let writer = ring.writer().unwrap();
        let reader = Reader::new(ring, SESSION_RATE);
        reader.control().set_delay_frames(delay_frames);
        let gain = Arc::new(SharedGain::new(gain));
        let pipeline =
            RenderPipeline::new(reader, Arc::clone(&gain), device_rate, channels).unwrap();
        Setup {
            writer,
            gain,
            pipeline,
        }
    }

    /// Writes stereo frames whose left sample is `index / 1e6` and right
    /// sample its negative, from `first` on.
    fn write_ramp(writer: &mut RingWriter, first: usize, frames: usize) {
        let samples: Vec<f32> = (first..first + frames)
            .flat_map(|i| {
                let value = i as f32 / 1e6;
                [value, -value]
            })
            .collect();
        writer.write(&samples, frames);
    }

    fn tone(writer: &mut RingWriter, first: usize, frames: usize, hz: f64) {
        let samples: Vec<f32> = (first..first + frames)
            .flat_map(|i| {
                let value = (0.5
                    * (i as f64 * hz / f64::from(SESSION_RATE) * std::f64::consts::TAU).sin())
                    as f32;
                [value, value]
            })
            .collect();
        writer.write(&samples, frames);
    }

    #[test]
    fn at_the_session_rate_frames_play_at_the_delay() {
        let mut s = setup(SESSION_RATE, 2, 1_000, 1.0);
        write_ramp(&mut s.writer, 0, 5_000);
        let mut out = vec![9.0; 480 * 2];
        s.pipeline.render(&mut out);
        // Anchored at W - delay = 4000.
        assert_eq!(out[0], 4_000.0 / 1e6);
        assert_eq!(out[1], -4_000.0 / 1e6);
        assert_eq!(out[479 * 2], 4_479.0 / 1e6);
        s.pipeline.render(&mut out);
        assert_eq!(out[0], 4_480.0 / 1e6, "the next block follows on");
        assert_eq!(s.pipeline.shortfall_frames(), 0);
    }

    #[test]
    fn stereo_feeds_the_first_two_channels_and_the_rest_stay_silent() {
        let mut s = setup(SESSION_RATE, 6, 1_000, 1.0);
        write_ramp(&mut s.writer, 0, 5_000);
        let mut out = vec![9.0; 100 * 6];
        s.pipeline.render(&mut out);
        let first = &out[..6];
        assert_eq!(first, [0.004, -0.004, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_mono_device_gets_the_average_of_both_channels() {
        let mut s = setup(SESSION_RATE, 1, 1_000, 1.0);
        let samples: Vec<f32> = (0..5_000).flat_map(|_| [0.5, 0.25]).collect();
        s.writer.write(&samples, 5_000);
        let mut out = vec![9.0; 64];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.375));
    }

    #[test]
    fn the_gain_is_applied_and_follows_changes_from_the_next_callback() {
        let mut s = setup(SESSION_RATE, 2, 1_000, 0.5);
        let samples = vec![0.8f32; 20_000];
        s.writer.write(&samples, 10_000);
        let mut out = vec![0.0; 256 * 2];
        s.pipeline.render(&mut out);
        assert!(
            out.iter().all(|&sample| sample == 0.4),
            "the first callback plays at the gain"
        );
        s.gain.set(0.25);
        s.pipeline.render(&mut out);
        assert!((0.2..0.4).contains(&out[0]), "ramps over one callback");
        assert_eq!(out[255 * 2], 0.2);
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.2));
    }

    #[test]
    fn a_fade_out_ends_in_exact_silence() {
        let mut s = setup(SESSION_RATE, 2, 1_000, 1.0);
        let samples = vec![0.5f32; 40_000];
        s.writer.write(&samples, 20_000);
        let mut out = vec![0.0; 256 * 2];
        s.pipeline.render(&mut out);
        s.pipeline.render_fade_out(&mut out);
        assert!(out[0] > 0.4, "starts near the old gain");
        assert_eq!(out[255 * 2], 0.0);
        s.pipeline.render_fade_out(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn samples_that_are_not_finite_reach_the_device_as_zero() {
        let mut s = setup(SESSION_RATE, 2, 1_000, 1.0);
        let samples: Vec<f32> = (0..5_000)
            .flat_map(|i| {
                if i % 2 == 0 {
                    [f32::NAN, 0.5]
                } else {
                    [f32::INFINITY, 0.5]
                }
            })
            .collect();
        s.writer.write(&samples, 5_000);
        let mut out = vec![9.0; 128 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|sample| sample.is_finite()));
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.5);
    }

    #[test]
    fn before_enough_audio_arrived_the_output_plays_silence() {
        let mut s = setup(SESSION_RATE, 2, 96_000, 1.0);
        write_ramp(&mut s.writer, 0, 1_000);
        let mut out = vec![9.0; 480 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn a_trailing_partial_frame_stays_silent() {
        let mut s = setup(SESSION_RATE, 2, 1_000, 1.0);
        write_ramp(&mut s.writer, 0, 5_000);
        let mut out = vec![9.0; 11];
        s.pipeline.render(&mut out);
        assert_eq!(out[10], 0.0);
    }

    /// Plays a 1 kHz tone through a device at `device_rate`, in callbacks of
    /// `callback` device frames, and checks the result.
    fn play_tone_at(device_rate: u32, callback: usize) {
        let mut s = setup(device_rate, 2, 4_800, 1.0);
        let mut written = 0;
        let mut played = Vec::new();
        // Two seconds, the capture writing 10 ms blocks.
        let device_frames_per_block = device_rate as usize / 100;
        let mut owed = 0usize;
        for _ in 0..200 {
            tone(&mut s.writer, written, 480, 1_000.0);
            written += 480;
            owed += device_frames_per_block;
            while owed >= callback {
                let mut out = vec![0.0; callback * 2];
                s.pipeline.render(&mut out);
                assert!(out.iter().all(|sample| sample.is_finite()));
                played.extend(out.as_chunks::<2>().0.iter().map(|frame| frame[0]));
                owed -= callback;
            }
        }
        assert_eq!(s.pipeline.device_rate(), device_rate);
        assert_eq!(s.pipeline.shortfall_frames(), 0);
        // The last second: a 1 kHz tone at half scale, at the device's rate.
        let last_second = &played[played.len() - device_rate as usize..];
        let peak = last_second.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(
            (0.45..0.55).contains(&peak),
            "peak {peak} at {device_rate} Hz"
        );
        let crossings = last_second
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        assert!(
            (1_990..=2_010).contains(&crossings),
            "{crossings} zero crossings in a second at {device_rate} Hz"
        );
    }

    #[test]
    fn a_device_at_44_1_khz_gets_the_tone_converted() {
        play_tone_at(44_100, 441);
    }

    #[test]
    fn a_device_at_96_khz_gets_the_tone_converted() {
        play_tone_at(96_000, 960);
    }

    #[test]
    fn a_device_at_the_session_rate_plays_the_tone_unchanged() {
        play_tone_at(SESSION_RATE, 480);
    }

    #[test]
    fn odd_callback_sizes_at_another_rate_never_run_short() {
        play_tone_at(44_100, 1_031);
        play_tone_at(22_050, 97);
    }

    #[test]
    fn the_read_bound_covers_what_the_resampler_asks_for() {
        for device_rate in [
            8_000, 11_025, 22_050, 44_100, 88_200, 96_000, 176_400, 192_000, 384_000,
        ] {
            let mut resampler = Resampler::new(SESSION_RATE, device_rate).unwrap();
            let bound = reader_frames_bound(RENDER_CHUNK, device_rate);
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
    fn a_device_without_channels_or_at_an_unsupported_rate_is_refused() {
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let gain = Arc::new(SharedGain::new(1.0));
        let reader = Reader::new(Arc::clone(&ring), SESSION_RATE);
        assert!(matches!(
            RenderPipeline::new(reader, Arc::clone(&gain), SESSION_RATE, 0),
            Err(BackendError::UnsupportedFormat)
        ));
        let reader = Reader::new(ring, SESSION_RATE);
        assert!(matches!(
            RenderPipeline::new(reader, gain, 1_000_000, 2),
            Err(BackendError::UnsupportedFormat)
        ));
    }
}
