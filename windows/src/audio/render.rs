//! An output's render pipeline: the core's renderer from the shared ring,
//! through the output's delay reader, to the device's rate, channels, gain
//! and fade, plus the endpoint's side of a stop.
//!
//! Per device callback the core's [`Renderer`] reads session-rate frames
//! through its [`Reader`] (at the output's delay, with its own drift
//! correction, held while it plays), converts them to the device's rate,
//! sizing each read from what the resampler needs (S62), maps stereo onto
//! the device's channels (S63), applies the output's gain, ramped over one
//! callback when it changes (S64, S65), and then its 20 ms fade at the
//! device rate (S183): the first audio fades in (S184), a stop fades out over
//! 20 ms whatever a callback holds (S185), and a delay change fades out, lands
//! and fades in again (S187). Delay and lag stay in session-rate frames.
//!
//! WASAPI keeps up to a whole endpoint buffer queued, so the end of a fade is
//! heard well after it was rendered: an output counts as faded only once the
//! endpoint's clock has played past the fade's last frame, or without a
//! clock, once it holds nothing but zeros and its stream latency has passed
//! since ([`RenderPipeline::has_played_its_fade`]).

use std::sync::Arc;

use audiosync_core::clock;
use audiosync_core::reader::Reader;
use audiosync_core::render::{Renderer, RendererControl, fade_tail_frames};
use audiosync_core::volume::SharedGain;

use super::SESSION_RATE;
use super::backend::BackendError;

/// What an endpoint still holds when the render loop asks whether a fade has
/// played, in device frames.
#[derive(Clone, Copy, Debug)]
pub struct EndpointQueue {
    /// Frames queued in the stream buffer (`GetCurrentPadding`).
    pub padding: u32,
    /// Frames written but not yet played by the endpoint's clock
    /// (`IAudioClock`), when it could be read.
    pub clock_queued: Option<u64>,
    /// The stream latency the endpoint reports (`GetStreamLatency`).
    pub latency_frames: u32,
    /// The device rate.
    pub rate: u32,
}

/// The render pipeline of one output, owned by its render thread.
///
/// All buffers are allocated by [`RenderPipeline::new`];
/// [`RenderPipeline::render`] never allocates, locks or blocks.
pub struct RenderPipeline {
    renderer: Renderer,
}

impl RenderPipeline {
    /// A pipeline that reads through `reader`, controlled through `control`
    /// (made for that reader), and plays at the gain in `gain`, for a device
    /// running at `device_rate` with `channels` interleaved float channels.
    pub fn new(
        reader: Reader,
        control: &RendererControl,
        gain: Arc<SharedGain>,
        device_rate: u32,
        channels: usize,
    ) -> Result<RenderPipeline, BackendError> {
        let renderer = Renderer::new(reader, control, gain, SESSION_RATE, device_rate, channels)
            .map_err(|_| BackendError::UnsupportedFormat)?;
        Ok(RenderPipeline { renderer })
    }

    /// Channels per device frame.
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "the WASAPI render loop sizes its packets with it")
    )]
    pub fn channels(&self) -> usize {
        self.renderer.channels()
    }

    /// The device rate.
    #[cfg(test)]
    pub fn device_rate(&self) -> u32 {
        self.renderer.device_rate()
    }

    /// Device frames played as silence because the resampler lacked input.
    #[cfg(test)]
    pub fn shortfall_frames(&self) -> u64 {
        self.renderer.shortfall_frames()
    }

    /// Sets the endpoint's period, in device frames: what each wake refills
    /// when it is on time, which the output's lead counts rather than a late
    /// wake's larger fill (S206). Call it before the first render.
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "the WASAPI render loop sets it")
    )]
    pub fn set_period_frames(&mut self, frames: usize) {
        self.renderer.set_period_frames(frames);
    }

    /// The lead the output's reader got at its last callback (S206).
    #[cfg(test)]
    pub fn lead_frames(&self) -> i64 {
        self.renderer.lead_frames()
    }

    /// Fills `out`, interleaved with [`RenderPipeline::channels`] channels,
    /// reading now on the core's clock, the one the capture stamps its
    /// blocks with, so a converting output follows the capture's clock
    /// (S207).
    pub fn render(&mut self, out: &mut [f32]) {
        self.renderer.render_at(out, clock::now_ns());
    }

    /// Whether the endpoint has played the requested fade-out (S185): the
    /// frames written after its last frame cover what `queue` says is still
    /// queued ([`fade_tail_frames`]).
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "the WASAPI render loop asks it")
    )]
    pub fn has_played_its_fade(&self, queue: EndpointQueue) -> bool {
        self.renderer.frames_after_fade().is_some_and(|after| {
            after
                >= fade_tail_frames(
                    u64::from(queue.padding),
                    queue.clock_queued,
                    u64::from(queue.latency_frames),
                    queue.rate,
                )
        })
    }

    /// Marks the output as faded for the control thread (a release store).
    #[cfg_attr(
        not(windows),
        expect(dead_code, reason = "the WASAPI render loop calls it")
    )]
    pub fn mark_faded(&self) {
        self.renderer.mark_faded();
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use audiosync_core::render::RendererControl;
    use audiosync_core::ring::{Ring, RingWriter};

    use super::*;

    struct Setup {
        writer: RingWriter,
        gain: Arc<SharedGain>,
        control: RendererControl,
        pipeline: RenderPipeline,
        /// Session-rate frames the live capture owes the ring.
        owed: f64,
    }

    /// A ring at the session rate and a pipeline reading it at `delay_frames`.
    fn setup(device_rate: u32, channels: usize, delay_frames: i64, gain: f32) -> Setup {
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let writer = ring.writer().unwrap();
        let reader = Reader::new(ring, SESSION_RATE);
        reader.control().set_delay_frames(delay_frames);
        let control = RendererControl::new(&reader);
        let gain = Arc::new(SharedGain::new(gain));
        let pipeline =
            RenderPipeline::new(reader, &control, Arc::clone(&gain), device_rate, channels)
                .unwrap();
        Setup {
            writer,
            gain,
            control,
            pipeline,
            owed: 0.0,
        }
    }

    /// The converter's latency at the session rate (S85): a fade-in starts on
    /// the first frame that leaves it.
    const LATENCY: usize = 21;

    /// A fade's frames at `rate` (S182): 20 ms.
    fn fade_len(rate: u32) -> usize {
        (rate as usize * 20 + 500) / 1000
    }

    /// w of frame `i` of a fade-in, or of a fade-out, of `len` frames (S182).
    fn fade_in(i: usize, len: usize) -> f64 {
        0.5 - 0.5 * (PI * (i + 1) as f64 / len as f64).cos()
    }

    fn fade_out(i: usize, len: usize) -> f64 {
        0.5 + 0.5 * (PI * (i + 1) as f64 / len as f64).cos()
    }

    /// Renders `frames` frames in callbacks of `callback` frames and returns
    /// the first channel.
    fn play(s: &mut Setup, frames: usize, callback: usize) -> Vec<f32> {
        let mut played = Vec::new();
        while played.len() < frames {
            let n = callback.min(frames - played.len());
            let mut out = vec![9.0f32; n * 2];
            s.pipeline.render(&mut out);
            played.extend(out.as_chunks::<2>().0.iter().map(|frame| frame[0]));
        }
        played
    }

    /// Like [`play`], with a capture that writes 0.5 at the pace the device
    /// plays before each callback.
    fn live(s: &mut Setup, frames: usize, callback: usize) -> Vec<f32> {
        let mut played = Vec::new();
        while played.len() < frames {
            let n = callback.min(frames - played.len());
            s.owed += n as f64 * f64::from(SESSION_RATE) / f64::from(s.pipeline.device_rate());
            let write = s.owed.floor();
            s.owed -= write;
            let write = write as usize;
            s.writer.write(&vec![0.5f32; write * 2], write);
            played.extend(play(s, n, n));
        }
        played
    }

    /// A pipeline at a 100 ms delay whose ring holds those 100 ms, so the
    /// first live callback primes it.
    fn primed(device_rate: u32) -> Setup {
        let mut s = setup(device_rate, 2, 4_800, 1.0);
        s.writer.write(&vec![0.5f32; 4_800 * 2], 4_800);
        s
    }

    #[test]
    fn the_first_audio_of_a_session_fades_in_over_20_ms() {
        let mut s = primed(SESSION_RATE);
        let played = live(&mut s, 4_000, 480);
        let len = fade_len(SESSION_RATE);
        assert!(played[..LATENCY].iter().all(|&sample| sample == 0.0));
        for (i, &sample) in played[LATENCY..LATENCY + len].iter().enumerate() {
            let expected = 0.5 * fade_in(i, len);
            assert!(
                (f64::from(sample) - expected).abs() <= 1e-6,
                "frame {i}: {sample}, the fade says {expected}"
            );
        }
        assert!(
            played[LATENCY + len - 1..]
                .iter()
                .all(|&sample| sample == 0.5)
        );

        // At another device rate the fade counts that rate's frames.
        let mut s = primed(44_100);
        let played = live(&mut s, 4_000, 441);
        let len = fade_len(44_100);
        assert_eq!(len, 882);
        assert!(played[100].abs() < 0.05, "{}", played[100]);
        assert!((played[len / 2] - 0.25).abs() < 0.02, "{}", played[len / 2]);
        assert!(
            played[len + 64..]
                .iter()
                .all(|&sample| (sample - 0.5).abs() < 1e-3)
        );
    }

    #[test]
    fn a_stop_fades_out_over_20_ms_whatever_the_callback_holds() {
        let len = fade_len(SESSION_RATE);
        for callback in [64usize, 480, 1_056, 4_096] {
            let mut s = primed(SESSION_RATE);
            live(&mut s, 2_048, callback);
            s.control.request_fade_out();
            let played = live(&mut s, 3_000, callback);
            for (i, &sample) in played[..len].iter().enumerate() {
                let expected = 0.5 * fade_out(i, len);
                assert!(
                    (f64::from(sample) - expected).abs() <= 1e-6,
                    "{callback}-frame callbacks, frame {i}: {sample} for {expected}"
                );
            }
            assert!(
                played[len - 1..].iter().all(|&sample| sample == 0.0),
                "{callback}-frame callbacks: silent after 20 ms"
            );
        }
    }

    #[test]
    fn a_delay_change_fades_out_first_and_in_after() {
        let len = fade_len(SESSION_RATE);
        let mut s = setup(SESSION_RATE, 2, 4_800, 1.0);
        let block = vec![0.5f32; 480 * 2];
        let tick = |s: &mut Setup| {
            s.writer.write(&block, 480);
            play(s, 480, 480)
        };
        for _ in 0..20 {
            tick(&mut s);
        }
        assert_eq!(s.control.lag_frames(), 4_800 - 480 - LATENCY as i64);
        s.control.set_delay_frames(2_400);
        let mut played = Vec::new();
        for _ in 0..6 {
            played.extend(tick(&mut s));
        }
        for i in 0..len {
            let out = 0.5 * fade_out(i, len);
            let back = 0.5 * fade_in(i, len);
            assert!((f64::from(played[i]) - out).abs() <= 1e-6, "out {i}");
            let at = len + LATENCY + i;
            assert!((f64::from(played[at]) - back).abs() <= 1e-6, "in {i}");
        }
        assert!(
            played[len..len + LATENCY]
                .iter()
                .all(|&sample| sample == 0.0)
        );
        assert_eq!(
            s.control.lag_frames(),
            2_400 - 480 - LATENCY as i64,
            "the change landed exactly after the fade-out"
        );
    }

    /// What the endpoint holds, as the render loop reads it at 48 kHz.
    fn queue(padding: u32, clock_queued: Option<u64>, latency_frames: u32) -> EndpointQueue {
        EndpointQueue {
            padding,
            clock_queued,
            latency_frames,
            rate: 48_000,
        }
    }

    #[test]
    fn a_fade_has_played_only_once_the_endpoint_clock_has_passed_its_end() {
        let mut s = primed(SESSION_RATE);
        live(&mut s, 2_400, 480);
        s.control.request_fade_out();
        live(&mut s, 480, 480);
        play(&mut s, 480, 480);
        // Only zeros are queued, but the clock says 30 ms more than the
        // padding is written and not yet played: the end of the fade is
        // among them, and GetStreamLatency says 0.
        play(&mut s, 480, 480);
        let behind = 480 + 1_440;
        assert!(
            !s.pipeline.has_played_its_fade(queue(480, Some(behind), 0)),
            "the clock hasn't reached the fade's last frame"
        );
        play(&mut s, 1_440, 480);
        assert!(s.pipeline.has_played_its_fade(queue(480, Some(behind), 0)));
    }

    #[test]
    fn a_clock_far_behind_the_padding_holds_a_stop_at_most_the_tail_cap() {
        let mut s = primed(SESSION_RATE);
        live(&mut s, 2_400, 480);
        s.control.request_fade_out();
        live(&mut s, 480, 480);
        play(&mut s, 480, 480);
        // A clock 1 s behind counts as the padding plus 100 ms.
        play(&mut s, 480 + 4_320, 480);
        assert!(!s.pipeline.has_played_its_fade(queue(480, Some(48_480), 0)));
        play(&mut s, 480, 480);
        assert!(s.pipeline.has_played_its_fade(queue(480, Some(48_480), 0)));
        // A clock ahead of the padding (it counted a glitch's silence)
        // still waits for the padding.
        assert!(!s.pipeline.has_played_its_fade(queue(9_999, Some(0), 0)));
    }

    #[test]
    fn an_output_is_faded_only_when_the_endpoint_holds_nothing_but_zeros() {
        let mut s = primed(SESSION_RATE);
        live(&mut s, 2_400, 480);
        assert!(
            !s.pipeline.has_played_its_fade(queue(0, None, 0)),
            "no stop asked for"
        );
        s.control.request_fade_out();
        live(&mut s, 480, 480);
        assert!(
            !s.pipeline.has_played_its_fade(queue(0, None, 0)),
            "half the fade rendered"
        );
        // The fade's last frame is this callback's last.
        play(&mut s, 480, 480);
        assert!(s.pipeline.has_played_its_fade(queue(0, None, 0)));
        assert!(
            !s.pipeline.has_played_its_fade(queue(480, None, 0)),
            "the endpoint still holds the end of the fade"
        );
        play(&mut s, 480, 480);
        assert!(
            s.pipeline.has_played_its_fade(queue(480, None, 0)),
            "only zeros queued"
        );
        assert!(
            !s.pipeline.has_played_its_fade(queue(480, None, 240)),
            "the stream latency hasn't passed since"
        );
        play(&mut s, 240, 240);
        assert!(s.pipeline.has_played_its_fade(queue(480, None, 240)));
        assert!(!s.control.is_faded());
        s.pipeline.mark_faded();
        assert!(s.control.is_faded());
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

    /// Plays past the first audio's fade-in.
    fn warm_up(s: &mut Setup) {
        play(s, 1_024, 512);
    }

    #[test]
    fn at_the_session_rate_frames_play_at_the_delay() {
        let mut s = setup(SESSION_RATE, 2, 4_000, 1.0);
        write_ramp(&mut s.writer, 0, 10_000);
        let mut out = vec![9.0; 480 * 2];
        // Anchored at W - delay = 6000; 960 frames fade in after the
        // converter's latency.
        s.pipeline.render(&mut out);
        assert!(out[0].abs() < 1e-6, "the fade-in starts from silence");
        for _ in 0..3 {
            s.pipeline.render(&mut out);
        }
        assert_eq!(out[0], 7_440.0 / 1e6);
        assert_eq!(out[1], -7_440.0 / 1e6);
        assert_eq!(out[479 * 2], 7_919.0 / 1e6);
        s.pipeline.render(&mut out);
        assert_eq!(out[0], 7_920.0 / 1e6, "the next block follows on");
        assert_eq!(s.pipeline.shortfall_frames(), 0);
    }

    #[test]
    fn stereo_feeds_the_first_two_channels_and_the_rest_stay_silent() {
        let mut s = setup(SESSION_RATE, 6, 4_000, 1.0);
        write_ramp(&mut s.writer, 0, 10_000);
        let mut out = vec![9.0; 1_024 * 6];
        s.pipeline.render(&mut out);
        let mut out = vec![9.0; 100 * 6];
        s.pipeline.render(&mut out);
        let first = &out[..6];
        let value = 7_024.0 / 1e6;
        assert_eq!(first, [value, -value, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_mono_device_gets_the_average_of_both_channels() {
        let mut s = setup(SESSION_RATE, 1, 4_000, 1.0);
        let samples: Vec<f32> = (0..5_000).flat_map(|_| [0.5, 0.25]).collect();
        s.writer.write(&samples, 5_000);
        let mut out = vec![9.0; 1_024];
        s.pipeline.render(&mut out);
        let mut out = vec![9.0; 64];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.375));
    }

    #[test]
    fn the_gain_is_applied_and_follows_changes_from_the_next_callback() {
        let mut s = setup(SESSION_RATE, 2, 4_000, 0.5);
        let samples = vec![0.8f32; 20_000];
        s.writer.write(&samples, 10_000);
        warm_up(&mut s);
        let mut out = vec![0.0; 256 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.4), "plays at the gain");
        s.gain.set(0.25);
        s.pipeline.render(&mut out);
        assert!((0.2..0.4).contains(&out[0]), "ramps over one callback");
        assert_eq!(out[255 * 2], 0.2);
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.2));
    }

    #[test]
    fn samples_that_are_not_finite_reach_the_device_as_zero() {
        let mut s = setup(SESSION_RATE, 2, 4_000, 1.0);
        // Not finite on the left up to frame 8000, finite from there.
        let samples: Vec<f32> = (0..10_000)
            .flat_map(|i| match i {
                8_000.. => [0.5, 0.5],
                _ if i % 2 == 0 => [f32::NAN, 0.5],
                _ => [f32::INFINITY, 0.5],
            })
            .collect();
        s.writer.write(&samples, 10_000);
        let mut out = vec![9.0; 1_024 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|sample| sample.is_finite()), "fading in");
        let mut out = vec![9.0; 128 * 2];
        s.pipeline.render(&mut out);
        // The converter clears what it still had to play on both channels
        // at each non-finite sample (S125).
        assert!(out.iter().all(|&sample| sample == 0.0));
        let mut out = vec![9.0; 1_024 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|sample| sample.is_finite()));
        let mut out = vec![9.0; 128 * 2];
        s.pipeline.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.5), "finite again");
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

    /// The capture has run for a second, writing a 10 ms block on its own
    /// clock, when an endpoint whose clock runs 100 ppm fast starts asking
    /// for one 10 ms period per event, at hold-back 0: the output never runs
    /// dry after its start when it converts (S206).
    #[test]
    fn a_converting_output_at_hold_back_0_never_runs_dry_after_its_start() {
        for device_rate in [44_100, 96_000] {
            for phase in [0.0, 0.25, 0.5, 0.75, 0.95] {
                let mut s = setup(device_rate, 2, 0, 1.0);
                let block = SESSION_RATE as usize / 100;
                let period = device_rate as usize / 100;
                for _ in 0..100 {
                    s.writer.write(&vec![0.5f32; block * 2], block);
                }
                // Time in units of 1 / (session rate × device rate × 10^4)
                // seconds; the endpoint's period is 10^-4 short.
                let write_every = block as u128 * u128::from(device_rate) * 10_000;
                let pull_every = period as u128 * u128::from(SESSION_RATE) * 9_999;
                let offset = (phase * write_every as f64) as u128;
                let (mut written, mut pulled) = (1u128, 0u128);
                let mut played = Vec::new();
                // Five seconds.
                while played.len() < 5 * device_rate as usize {
                    if pulled * pull_every + offset < written * write_every {
                        played.extend(play(&mut s, period, period));
                        pulled += 1;
                    } else {
                        s.writer.write(&vec![0.5f32; block * 2], block);
                        written += 1;
                    }
                }
                let counters = s.control.counters();
                assert_eq!(
                    (
                        counters.starved_renders,
                        counters.drift_skips,
                        counters.drift_silences
                    ),
                    (0, 0, 0),
                    "{device_rate} Hz, phase {phase}"
                );
                let first = played.iter().position(|&x| x != 0.0).unwrap();
                let open = first + fade_len(device_rate) + 64;
                assert!(
                    played[open..].iter().all(|&x| (x - 0.5).abs() < 1e-3),
                    "a gap after the fade-in at {device_rate} Hz, phase {phase}"
                );
            }
        }
    }

    /// An endpoint whose period the pipeline knows: a late wake's fill of
    /// two periods gets a lead for two periods, and the next on-time wake is
    /// back at one (S206).
    #[test]
    fn a_late_wake_raises_the_lead_for_its_own_fill_only() {
        let mut s = setup(44_100, 2, 0, 1.0);
        s.pipeline.set_period_frames(441);
        for _ in 0..100 {
            s.writer.write(&[0.5f32; 960], 480);
        }
        play(&mut s, 441, 441);
        let on_time = s.pipeline.lead_frames();
        s.writer.write(&[0.5f32; 960], 480);
        s.writer.write(&[0.5f32; 960], 480);
        play(&mut s, 882, 882);
        let late = s.pipeline.lead_frames();
        s.writer.write(&[0.5f32; 960], 480);
        play(&mut s, 441, 441);
        assert!(late > on_time, "{late} after a late wake, {on_time} before");
        assert_eq!(s.pipeline.lead_frames(), on_time);
        assert_eq!(s.control.counters().starved_renders, 0);
    }

    #[test]
    fn a_device_without_channels_or_at_an_unsupported_rate_is_refused() {
        let ring = Ring::for_rate(SESSION_RATE).unwrap();
        let gain = Arc::new(SharedGain::new(1.0));
        let reader = Reader::new(Arc::clone(&ring), SESSION_RATE);
        let control = RendererControl::new(&reader);
        assert!(matches!(
            RenderPipeline::new(reader, &control, Arc::clone(&gain), SESSION_RATE, 0),
            Err(BackendError::UnsupportedFormat)
        ));
        let reader = Reader::new(ring, SESSION_RATE);
        let control = RendererControl::new(&reader);
        assert!(matches!(
            RenderPipeline::new(reader, &control, gain, 1_000_000, 2),
            Err(BackendError::UnsupportedFormat)
        ));
    }
}
