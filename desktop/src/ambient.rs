//! The Now Playing art under the cover, as on the Mac: three strands that
//! follow the captured sound's bass, mid and treble while streaming, a slow
//! breath otherwise, and one still line when the desktop's animations are
//! off. The motion's maths is `window_rules::ambient_breath` and
//! `ambient_bands`; this keeps its state and draws it.

use std::time::Duration;

use crate::window_rules::{AmbientMode, Rgb};
use crate::window_rules::{ambient_bands, ambient_breath};

/// The strip's drawing space: the path's viewbox in `ambient.slint`.
pub const VIEW_WIDTH: f64 = 300.0;
pub const VIEW_HEIGHT: f64 = 22.0;
/// How far the strands reach: the main window's hero, and the tray panel's smaller one.
pub const WINDOW_DENSITY: f64 = 1.0;
pub const FLYOUT_DENSITY: f64 = 0.7;
/// The strips' heights, which the viewbox is stretched to.
pub const WINDOW_SLOT: f64 = 22.0;
pub const FLYOUT_SLOT: f64 = 16.0;
/// The colours of a missing cover.
pub const NO_COVER: [Rgb; 3] = [[0.5; 3]; 3];

const STEPS: usize = 60;

/// One strand as drawn: its path, the breath's line opacity over it, the
/// sound's own strand in `tone` at `sound`, and the stroke's width.
#[derive(Clone, Debug, PartialEq)]
pub struct Strand {
    pub commands: String,
    pub breath: f64,
    pub sound: f64,
    pub width: f64,
    pub tone: Rgb,
}

/// The art's state between frames. The bands are read every
/// `ambient_bands::POLL` seconds, or once a frame when frames come slower.
#[derive(Debug, Default)]
pub struct Driver {
    mode: AmbientMode,
    bands: [f64; 3],
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

    /// Moves on `dt` seconds; `bands` is the capture's bass, mid and treble
    /// peaks since their last read, `None` while streaming is off.
    pub fn advance(&mut self, dt: f64, bands: impl FnOnce() -> Option<[f32; 3]>) {
        self.time += dt;
        self.since_poll += dt;
        if self.since_poll + 1e-9 < ambient_bands::POLL {
            return;
        }
        // One read for a late frame, over the time it took.
        let elapsed = std::mem::take(&mut self.since_poll).min(0.25);
        let peaks = bands().unwrap_or_default();
        for (level, peak) in self.bands.iter_mut().zip(peaks) {
            *level = ambient_bands::follow(*level, ambient_bands::level(peak), elapsed);
        }
        let loudest = self.bands.iter().copied().fold(0.0, f64::max);
        self.mode.step(loudest, elapsed);
    }

    /// Time between frames, `None` when the art doesn't move.
    pub fn interval(&self, on_screen: bool, still: bool) -> Option<Duration> {
        ambient_breath::frame_interval(on_screen, still, self.mode.live())
            .map(Duration::from_secs_f64)
    }

    /// The strands for a hero whose strands reach `density` in a strip `slot`
    /// high; one flat line when `still`.
    pub fn strands(&self, cover: &[Rgb; 3], density: f64, slot: f64, still: bool) -> Vec<Strand> {
        if still {
            return vec![Strand {
                commands: path(1, 0.0, 0.0, 0.0, slot),
                breath: 1.0,
                sound: 0.0,
                width: ambient_breath::STROKE,
                tone: cover[1],
            }];
        }
        let live = self.mode.live();
        let breath = ambient_breath::spread(self.time);
        let (line, sound) = ambient_breath::layers(live);
        (0..self.bands.len())
            .map(|index| {
                let reach = ambient_breath::motion(self.bands[index].min(1.0), live, breath);
                Strand {
                    commands: path(index, reach * density, 1.0 - live, self.time, slot),
                    breath: line,
                    sound,
                    width: ambient_breath::stroke(live),
                    tone: cover[index],
                }
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

/// Strand `index` as SVG path commands, widest in the middle.
fn path(index: usize, spread: f64, idle: f64, time: f64, slot: f64) -> String {
    use std::fmt::Write;

    let mut commands = String::with_capacity(STEPS * 14);
    for step in 0..=STEPS {
        let x = step as f64 / STEPS as f64;
        let lift = ambient_breath::lift(index, x, time, spread, idle, slot);
        let y = (11.0 + ambient_breath::taper(x) * lift).clamp(1.0, VIEW_HEIGHT - 1.0);
        let verb = if step == 0 { 'M' } else { 'L' };
        let _ = write!(commands, "{verb}{:.1} {y:.2} ", x * VIEW_WIDTH);
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

    fn run(driver: &mut Driver, seconds: f64, frame: f64, bands: Option<[f32; 3]>) -> u32 {
        let mut reads = 0;
        for _ in 0..(seconds / frame).round() as u32 {
            driver.advance(frame, || {
                reads += 1;
                bands
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

    fn reach(strand: &Strand) -> f64 {
        ys(&strand.commands)
            .iter()
            .map(|y| (y - 11.0).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn the_bands_are_read_thirty_times_a_second_or_once_a_slower_frame() {
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 60.0, None), 60);
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 15.0, None), 30);
    }

    #[test]
    fn held_sound_turns_the_art_live_and_quiet_turns_it_back() {
        let mut driver = Driver::default();
        let frame = 1.0 / 60.0;
        run(&mut driver, 1.5, frame, Some([0.5, 0.2, 0.05]));
        assert_eq!(driver.mode.live(), 1.0);
        assert_eq!(
            driver.interval(true, false),
            Some(Duration::from_secs_f64(frame))
        );
        run(&mut driver, 0.2, frame, None);
        assert_eq!(driver.mode.live(), 1.0, "a short gap keeps it live");
        run(&mut driver, 1.5, frame, None);
        assert_eq!(driver.mode.live(), 0.0, "back to the breath");
        assert_eq!(
            driver.interval(true, false),
            Some(Duration::from_secs_f64(1.0 / 15.0))
        );
    }

    #[test]
    fn each_strand_follows_its_own_band() {
        let treble_reach = |bands| {
            let mut driver = Driver::default();
            run(&mut driver, 1.5, 1.0 / 60.0, Some(bands));
            let strands = driver.strands(&NO_COVER, WINDOW_DENSITY, WINDOW_SLOT, false);
            assert_eq!((strands[0].sound, strands[0].breath), (1.0, 0.0));
            assert_eq!(strands[0].width, ambient_breath::LIVE_STROKE);
            reach(&strands[2])
        };
        let bass = treble_reach([0.9, 0.0, 0.0]);
        let treble = treble_reach([0.0, 0.0, 0.9]);
        assert!(treble > bass + 1.0, "{bass} {treble}");
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
            run(&mut driver, 0.1, 0.1, Some([0.8; 3]));
            run(&mut driver, 0.1, 0.1, Some([0.1; 3]));
        }
        driver.reset();
        assert_eq!(driver.mode.live(), 0.0);
        assert_eq!(driver.bands, [0.0; 3]);
        assert!(driver.time > 1.9, "the breath goes on from where it was");
    }

    #[test]
    fn still_art_is_one_flat_line_at_full_opacity() {
        let strands = Driver::default().strands(&NO_COVER, WINDOW_DENSITY, WINDOW_SLOT, true);
        assert_eq!(strands.len(), 1);
        assert_eq!((strands[0].breath, strands[0].sound), (1.0, 0.0));
        assert_eq!(strands[0].width, ambient_breath::STROKE);
        assert!(ys(&strands[0].commands).iter().all(|y| *y == 11.0));
    }

    #[test]
    fn the_breath_is_three_translucent_strands_that_stay_in_the_strip() {
        let mut driver = Driver::default();
        let mut widest: f64 = 0.0;
        for _ in 0..100 {
            run(&mut driver, 0.05, 0.05, None);
            let strands = driver.strands(&NO_COVER, WINDOW_DENSITY, WINDOW_SLOT, false);
            assert_eq!(strands.len(), 3);
            for strand in &strands {
                assert_eq!(strand.breath, ambient_breath::OPACITY);
                assert_eq!((strand.sound, strand.width), (0.0, ambient_breath::STROKE));
                let ys = ys(&strand.commands);
                assert_eq!(ys.len(), STEPS + 1);
                assert!(ys.iter().all(|y| (1.0..=21.0).contains(y)));
                widest = widest.max(reach(strand));
            }
        }
        assert!(widest > 2.0, "it sways: {widest}");
        assert!(widest < 6.0, "it stays calm: {widest}");
    }

    #[test]
    fn the_tray_panels_strands_reach_less_far() {
        let mut driver = Driver::default();
        run(&mut driver, 1.5, 1.0 / 60.0, Some([0.9; 3]));
        let reach = |density| reach(&driver.strands(&NO_COVER, density, WINDOW_SLOT, false)[0]);
        assert!(reach(FLYOUT_DENSITY) < reach(WINDOW_DENSITY));
    }

    #[test]
    fn the_strip_fades_the_strands_ends_as_the_rules_say() {
        let slint = include_str!("../ui/ambient.slint");
        let fade = ambient_breath::EDGE_FADE * 100.0;
        assert!(slint.contains(&format!(" {fade:.0}%")));
        assert!(slint.contains(&format!(" {:.0}%", 100.0 - fade)));
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
