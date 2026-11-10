//! The Now Playing art under the cover, as on the Mac: three strands that
//! follow the captured sound while streaming, a slow breath otherwise, and
//! one still line when the desktop's animations are off. The motion's maths
//! is `window_rules::ambient_breath`; this keeps its state and draws it.

use std::time::Duration;

use audiosync_core::meter;

use crate::window_rules::ambient_breath;
use crate::window_rules::{AmbientMode, Rgb, Tide};

/// The strip's drawing space: the path's viewbox in `ambient.slint`.
pub const VIEW_WIDTH: f64 = 300.0;
pub const VIEW_HEIGHT: f64 = 22.0;
/// How far the strands reach: the main window's hero, and the tray panel's smaller one.
pub const WINDOW_DENSITY: f64 = 1.0;
pub const FLYOUT_DENSITY: f64 = 0.7;
/// The colours of a missing cover.
pub const NO_COVER: [Rgb; 3] = [[0.5; 3]; 3];

const POLL: f64 = meter::METER_POLL_MS as f64 / 1000.0;
/// `(amplitude, wavelength, offset)` of each strand, in viewbox units.
const WAVES: [(f64, f64, f64); 3] = [(7.0, 92.0, 0.0), (5.0, 61.0, 1.7), (3.2, 137.0, 3.1)];
const STEPS: usize = 60;

/// One strand as drawn: its path, the breath's line opacity over it, and
/// the sound's own strand in `tone` at `sound`.
#[derive(Clone, Debug, PartialEq)]
pub struct Strand {
    pub commands: String,
    pub breath: f64,
    pub sound: f64,
    pub tone: Rgb,
}

/// The art's state between frames. The capture is read every
/// `METER_POLL_MS`, as the input meter reads it, whatever the frame rate.
#[derive(Debug, Default)]
pub struct Driver {
    tide: Tide,
    mode: AmbientMode,
    meter: f32,
    since_poll: f64,
    time: f64,
}

impl Driver {
    /// Back to the breath, as the art is when it goes off screen.
    pub fn reset(&mut self) {
        *self = Self {
            time: self.time,
            ..Self::default()
        };
    }

    /// Moves on `dt` seconds; `peak` is the capture's peak since its last
    /// read, `None` while streaming is off or an input device is captured.
    pub fn advance(&mut self, dt: f64, peak: impl FnOnce() -> Option<f32>) {
        self.time += dt;
        self.since_poll += dt;
        if self.since_poll + 1e-9 < POLL {
            return;
        }
        // One read per due poll; a late frame doesn't make up the reads it missed.
        self.since_poll = (self.since_poll - POLL).min(POLL);
        let level = peak().map(|peak| {
            self.meter = meter::meter_level(self.meter, peak);
            f64::from(self.meter)
        });
        if level.is_none() {
            self.meter = 0.0;
        }
        self.tide.step(level, POLL);
        self.mode.step(AmbientMode::sounding(level), POLL);
    }

    /// Time between frames, `None` when the art doesn't move.
    pub fn interval(&self, on_screen: bool, still: bool) -> Option<Duration> {
        ambient_breath::frame_interval(on_screen, still, self.mode.live())
            .map(Duration::from_secs_f64)
    }

    /// The strands for a hero whose strands reach `density`; one flat line when `still`.
    pub fn strands(&self, cover: &[Rgb; 3], density: f64, still: bool) -> Vec<Strand> {
        if still {
            return vec![Strand {
                commands: path(1, 0.0, density, 0.0),
                breath: 1.0,
                sound: 0.0,
                tone: cover[1],
            }];
        }
        let (live, env) = (self.mode.live(), self.tide.env().min(1.0));
        let spread = ambient_breath::motion(env, live, ambient_breath::spread(self.time));
        let (breath, sound) = ambient_breath::layers(live, env);
        (0..WAVES.len())
            .map(|index| Strand {
                commands: path(index, spread, density, self.time * 2.0),
                breath,
                sound,
                tone: cover[index],
            })
            .collect()
    }
}

/// The breath's line colour on the hero card, light and dark.
pub fn line_colors(cover: &[Rgb; 3], still: bool) -> (Rgb, Rgb) {
    let opacity = if still { 1.0 } else { ambient_breath::OPACITY };
    (
        ambient_breath::line(cover, false, opacity),
        ambient_breath::line(cover, true, opacity),
    )
}

/// Strand `index` as SVG path commands: pinned at both ends, widest in the middle.
fn path(index: usize, spread: f64, density: f64, phase: f64) -> String {
    use std::f64::consts::{PI, TAU};
    use std::fmt::Write;

    let (amplitude, length, offset) = WAVES[index];
    let mut commands = String::with_capacity(STEPS * 14);
    for step in 0..=STEPS {
        let along = step as f64 / STEPS as f64;
        let taper = (PI * along).sin();
        let angle =
            TAU * VIEW_WIDTH * along / length + phase * (1.0 + index as f64 * 0.35) + offset;
        let lift = amplitude * spread * angle.sin() + (index as f64 - 1.0) * 2.2 * spread;
        let y = (11.0 + taper * lift * density).clamp(1.0, VIEW_HEIGHT - 1.0);
        let verb = if step == 0 { 'M' } else { 'L' };
        let _ = write!(commands, "{verb}{:.1} {y:.2} ", along * VIEW_WIDTH);
    }
    commands
}

/// A cover's top, middle and bottom thirds, averaged, from its premultiplied
/// RGBA pixels `side` wide.
pub fn cover_bands(rgba: &[u8], side: u32) -> [Rgb; 3] {
    let side = side as usize;
    if side == 0 || rgba.len() < side * side * 4 {
        return NO_COVER;
    }
    let row = side * 4;
    let mut bands = NO_COVER;
    for (band, out) in bands.iter_mut().enumerate() {
        let rows = band * side / 3..(band + 1) * side / 3;
        let mut sum = [0.0; 4];
        for pixel in rgba[rows.start * row..rows.end * row].as_chunks::<4>().0 {
            for (total, value) in sum.iter_mut().zip(pixel) {
                *total += f64::from(*value);
            }
        }
        if sum[3] > 0.0 {
            *out = [0, 1, 2].map(|i| (sum[i] / sum[3]).min(1.0));
        }
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(driver: &mut Driver, seconds: f64, frame: f64, peak: Option<f32>) -> u32 {
        let mut reads = 0;
        for _ in 0..(seconds / frame).round() as u32 {
            driver.advance(frame, || {
                reads += 1;
                peak
            });
        }
        reads
    }

    fn ys(commands: &str) -> Vec<f64> {
        commands
            .split_whitespace()
            .skip(1)
            .step_by(2)
            .map(|y| y.parse().expect("a number"))
            .collect()
    }

    #[test]
    fn the_capture_is_read_ten_times_a_second_at_any_frame_rate() {
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 60.0, None), 20);
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 15.0, None), 20);
    }

    #[test]
    fn sound_turns_the_art_live_fast_and_quiet_turns_it_back_after_a_second() {
        let mut driver = Driver::default();
        assert_eq!(driver.mode.live(), 0.0);
        let frame = 1.0 / 60.0;
        let mut loud = 0.9;
        for _ in 0..30 {
            loud = 1.4 - loud;
            run(&mut driver, 0.1, frame, Some(loud));
        }
        assert_eq!(driver.mode.live(), 1.0, "live within the 0.4 s blend");
        assert_eq!(
            driver.interval(true, false),
            Some(Duration::from_secs_f64(frame))
        );
        run(&mut driver, 0.8, frame, None);
        assert_eq!(driver.mode.live(), 1.0, "a short gap keeps it live");
        run(&mut driver, 0.7, frame, None);
        assert_eq!(driver.mode.live(), 0.0, "back to the breath");
        assert_eq!(
            driver.interval(true, false),
            Some(Duration::from_secs_f64(1.0 / 15.0))
        );
    }

    #[test]
    fn the_art_draws_no_frame_off_screen_or_with_animations_off() {
        let driver = Driver::default();
        assert_eq!(driver.interval(false, false), None);
        assert_eq!(driver.interval(true, true), None);
    }

    #[test]
    fn a_reset_drops_the_sound_and_keeps_the_breath_going() {
        let mut driver = Driver::default();
        for _ in 0..10 {
            run(&mut driver, 0.1, 0.1, Some(0.8));
            run(&mut driver, 0.1, 0.1, Some(0.1));
        }
        driver.reset();
        assert_eq!(driver.mode.live(), 0.0);
        assert!(driver.time > 1.9, "the breath goes on from where it was");
    }

    #[test]
    fn still_art_is_one_flat_line_at_full_opacity() {
        let strands = Driver::default().strands(&NO_COVER, WINDOW_DENSITY, true);
        assert_eq!(strands.len(), 1);
        assert_eq!((strands[0].breath, strands[0].sound), (1.0, 0.0));
        assert!(ys(&strands[0].commands).iter().all(|y| *y == 11.0));
    }

    #[test]
    fn the_breath_is_three_translucent_strands_that_stay_in_the_strip() {
        let mut driver = Driver::default();
        let mut widest: f64 = 0.0;
        for _ in 0..100 {
            run(&mut driver, 0.05, 0.05, None);
            let strands = driver.strands(&NO_COVER, WINDOW_DENSITY, false);
            assert_eq!(strands.len(), 3);
            for strand in &strands {
                assert_eq!(strand.breath, ambient_breath::OPACITY);
                assert_eq!(strand.sound, 0.0);
                let ys = ys(&strand.commands);
                assert_eq!(ys.len(), STEPS + 1);
                assert!(ys.iter().all(|y| (1.0..=21.0).contains(y)));
                widest = ys.iter().fold(widest, |w, y| w.max((y - 11.0).abs()));
            }
        }
        assert!(widest > 0.5, "it moves: {widest}");
        assert!(widest < 3.0, "a quarter of the sound's reach: {widest}");
    }

    #[test]
    fn the_tray_panels_strands_reach_less_far() {
        let mut driver = Driver::default();
        run(&mut driver, 2.5, 0.1, None);
        let reach = |density| {
            driver.strands(&NO_COVER, density, false)[0]
                .commands
                .split_whitespace()
                .skip(1)
                .step_by(2)
                .map(|y| (y.parse::<f64>().unwrap() - 11.0).abs())
                .fold(0.0, f64::max)
        };
        assert!(reach(FLYOUT_DENSITY) < reach(WINDOW_DENSITY));
    }

    #[test]
    fn cover_bands_are_its_thirds_without_the_premultiplied_alpha() {
        let side = 3;
        let mut rgba = Vec::new();
        for pixel in [[255, 0, 0, 255], [0, 64, 0, 128], [0, 0, 0, 0]] {
            for _ in 0..side {
                rgba.extend_from_slice(&pixel);
            }
        }
        let bands = cover_bands(&rgba, side as u32);
        assert_eq!(bands[0], [1.0, 0.0, 0.0]);
        assert_eq!(bands[1], [0.0, 0.5, 0.0]);
        assert_eq!(bands[2], NO_COVER[2], "a clear band keeps grey");
        assert_eq!(cover_bands(&[], 4), NO_COVER);
    }

    #[test]
    fn the_still_line_takes_full_opacity_for_its_contrast() {
        let (light, dark) = line_colors(&NO_COVER, true);
        let card = |dark| {
            if dark {
                ambient_breath::DARK_CARD
            } else {
                ambient_breath::LIGHT_CARD
            }
        };
        assert!(ambient_breath::contrast(light, card(false)) >= 3.0);
        assert!(ambient_breath::contrast(dark, card(true)) >= 3.0);
    }
}
