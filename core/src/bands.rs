//! Bass, mid and treble levels of the captured signal, for the ambient art's
//! three strands: the capture thread splits the mono mix into three bands
//! and keeps each band's peak ([`crate::ring::Ring::take_band_peaks`]).

#![forbid(unsafe_code)]

/// Where the bass band ends and the mid band starts, in Hz.
pub const BASS_TOP_HZ: f64 = 200.0;

/// Where the mid band ends and the treble band starts, in Hz.
pub const TREBLE_BOTTOM_HZ: f64 = 2_000.0;

/// Filter state below this is set to zero after a block, so silence never
/// leaves the filters running on subnormal numbers.
const SETTLED: f32 = 1e-20;

/// One second-order section in transposed direct form II.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// A Butterworth low pass (`high` false) or high pass at `hz`, from the
    /// bilinear transform with prewarping (the Audio EQ Cookbook's forms).
    fn butterworth(rate: u32, hz: f64, high: bool) -> Biquad {
        let w0 = 2.0 * std::f64::consts::PI * hz / f64::from(rate);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high {
            ((1.0 + cos) / 2.0, -(1.0 + cos))
        } else {
            ((1.0 - cos) / 2.0, 1.0 - cos)
        };
        Biquad {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b0 / a0) as f32,
            a1: (-2.0 * cos / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    fn settle(&mut self) {
        for z in [&mut self.z1, &mut self.z2] {
            if !z.is_finite() || z.abs() < SETTLED {
                *z = 0.0;
            }
        }
    }
}

/// The three band filters and their state, fixed in size, so splitting a
/// block never allocates: a low pass at [`BASS_TOP_HZ`] for the bass, a high
/// pass there followed by a low pass at [`TREBLE_BOTTOM_HZ`] for the mid, and
/// a high pass at [`TREBLE_BOTTOM_HZ`] for the treble.
#[derive(Clone, Copy, Debug)]
pub struct BandSplitter {
    rate: u32,
    bass: Biquad,
    mid_floor: Biquad,
    mid_ceiling: Biquad,
    treble: Biquad,
}

impl BandSplitter {
    /// A splitter for a signal at `rate` frames per second, its filters at
    /// rest.
    pub fn new(rate: u32) -> BandSplitter {
        BandSplitter {
            rate,
            bass: Biquad::butterworth(rate, BASS_TOP_HZ, false),
            mid_floor: Biquad::butterworth(rate, BASS_TOP_HZ, true),
            mid_ceiling: Biquad::butterworth(rate, TREBLE_BOTTOM_HZ, false),
            treble: Biquad::butterworth(rate, TREBLE_BOTTOM_HZ, true),
        }
    }

    /// The rate it was made for.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Runs the interleaved stereo `samples` through the filters and returns
    /// the largest magnitude each band reached in them: bass, mid, treble.
    /// A frame holding a non-finite sample counts as silence.
    pub fn process(&mut self, samples: &[f32]) -> [f32; 3] {
        let mut peaks = [0.0_f32; 3];
        for frame in samples.chunks_exact(2) {
            let mono = 0.5 * (frame[0] + frame[1]);
            let mono = if mono.is_finite() { mono } else { 0.0 };
            let bands = [
                self.bass.run(mono),
                self.mid_ceiling.run(self.mid_floor.run(mono)),
                self.treble.run(mono),
            ];
            for (peak, band) in peaks.iter_mut().zip(bands) {
                *peak = peak.max(band.abs());
            }
        }
        for filter in [
            &mut self.bass,
            &mut self.mid_floor,
            &mut self.mid_ceiling,
            &mut self.treble,
        ] {
            filter.settle();
        }
        peaks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [u32; 2] = [44_100, 48_000];

    fn tone(rate: u32, hz: f64, seconds: f64, amplitude: f32) -> Vec<f32> {
        let frames = (f64::from(rate) * seconds) as usize;
        (0..frames)
            .flat_map(|i| {
                let phase = 2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate);
                let sample = amplitude * phase.sin() as f32;
                [sample, sample]
            })
            .collect()
    }

    /// The band peaks of a steady tone, once the filters settled.
    fn settled(rate: u32, hz: f64) -> [f32; 3] {
        let mut splitter = BandSplitter::new(rate);
        splitter.process(&tone(rate, hz, 0.25, 0.5));
        splitter.process(&tone(rate, hz, 0.25, 0.5))
    }

    #[test]
    fn a_60_hz_tone_moves_the_bass_band_and_hardly_the_treble() {
        for rate in RATES {
            let [bass, mid, treble] = settled(rate, 60.0);
            assert!(bass > 0.4, "{rate}: bass {bass}");
            assert!(mid < bass / 4.0, "{rate}: mid {mid}");
            assert!(treble < 0.01, "{rate}: treble {treble}");
        }
    }

    #[test]
    fn a_6_khz_tone_moves_the_treble_band_and_hardly_the_bass() {
        for rate in RATES {
            let [bass, mid, treble] = settled(rate, 6_000.0);
            assert!(treble > 0.4, "{rate}: treble {treble}");
            assert!(mid < treble / 4.0, "{rate}: mid {mid}");
            assert!(bass < 0.01, "{rate}: bass {bass}");
        }
    }

    #[test]
    fn a_1_khz_tone_moves_the_mid_band_most() {
        for rate in RATES {
            let [bass, mid, treble] = settled(rate, 1_000.0);
            assert!(mid > 0.3, "{rate}: mid {mid}");
            assert!(bass < mid / 4.0 && treble < mid, "{rate}: {bass} {treble}");
        }
    }

    #[test]
    fn a_click_lands_in_every_band() {
        for rate in RATES {
            let mut splitter = BandSplitter::new(rate);
            let width = rate as usize / 1000;
            let mut click = vec![0.0_f32; rate as usize / 10 * 2];
            click[..width * 2].fill(1.0);
            let peaks = splitter.process(&click);
            assert!(peaks.iter().all(|&peak| peak > 0.1), "{rate}: {peaks:?}");
        }
    }

    #[test]
    fn silence_gives_zeros() {
        for rate in RATES {
            let mut splitter = BandSplitter::new(rate);
            assert_eq!(splitter.process(&vec![0.0; 4096]), [0.0; 3]);
            splitter.process(&tone(rate, 440.0, 0.05, 0.5));
            let quiet = vec![0.0; rate as usize * 2 * 4];
            splitter.process(&quiet);
            assert_eq!(
                splitter.process(&quiet),
                [0.0; 3],
                "{rate}: the filters ring out to exact zeros"
            );
        }
    }

    #[test]
    fn non_finite_samples_count_as_silence() {
        let mut splitter = BandSplitter::new(48_000);
        assert_eq!(
            splitter.process(&[f32::NAN, f32::INFINITY, 0.0, 0.0]),
            [0.0; 3]
        );
        let [bass, ..] = settled(48_000, 60.0);
        splitter.process(&tone(48_000, 60.0, 0.25, 0.5));
        let after = splitter.process(&tone(48_000, 60.0, 0.25, 0.5));
        assert_eq!(after[0], bass, "a NaN leaves no trace in the filters");
    }

    #[test]
    fn its_state_is_fixed_in_size() {
        fn plain_value<T: Copy>() {}
        plain_value::<BandSplitter>();
        assert!(std::mem::size_of::<BandSplitter>() <= 128);
    }
}
