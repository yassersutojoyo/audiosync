//! The Now Playing art under the cover, as on the Mac: three strands that
//! follow the captured sound's bass, mid and treble while streaming, a slow
//! breath otherwise, and one still line when the desktop's animations are
//! off. The motion's maths is `window_rules::ambient_breath` and
//! `ambient_bands`; this keeps its state and draws it.

use std::time::Duration;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::window_rules::{AmbientMode, Rgb, Tide};
use crate::window_rules::{ambient_bands, ambient_breath};

/// The height of the box the strands are drawn in, stretched to the strip.
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
/// The longest step the art's clock takes between two frames, so a stall
/// picks up where it stopped instead of jumping (the Mac's `ArtClock.maxStep`).
const MAX_STEP: f64 = 0.1;

/// One strand as drawn: its heights in the box, `STEPS` apart, the breath's
/// line opacity over it, the sound's own strand in `tone` at `sound`, and the
/// stroke's width.
#[derive(Clone, Debug, PartialEq)]
pub struct Strand {
    pub ys: [f64; STEPS + 1],
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
    tide: Tide,
    meter: f32,
    since_peak: f64,
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
        self.time += dt.clamp(0.0, MAX_STEP);
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

    /// Moves the cover tint's envelope on `dt` seconds; `peak` is the
    /// capture's largest sample since its last read, `None` while nothing
    /// is captured from apps or the system.
    pub fn follow_sound(&mut self, _dt: f64, _peak: impl FnOnce() -> Option<f32>) {}

    /// The cover tint's opacity over the hero card.
    pub fn tint(&self) -> f64 {
        self.tide.tint()
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
                ys: path(1, 0.0, 0.0, 0.0, slot),
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
                    ys: path(index, reach * density, 1.0 - live, self.time, slot),
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

/// Strand `index`'s heights in the box, widest in the middle.
fn path(index: usize, spread: f64, idle: f64, time: f64, slot: f64) -> [f64; STEPS + 1] {
    std::array::from_fn(|step| {
        let x = step as f64 / STEPS as f64;
        let lift = ambient_breath::lift(index, x, time, spread, idle, slot);
        (11.0 + ambient_breath::taper(x) * lift).clamp(1.0, VIEW_HEIGHT - 1.0)
    })
}

/// Points a pixel is sampled at across; the coverage down a column is its area.
const SUBCOLUMNS: usize = 16;
const MAX_ROWS: usize = 256;

/// Strand `ys` (heights in the 300 x 22 box, `STEPS` apart) `stroke` pixels
/// wide, as a white premultiplied RGBA mask that fills `width` x `height`
/// pixels and fades out over its outer `EDGE_FADE`, as the Mac's Canvas maps
/// and strokes it (AmbientArt.swift:121-141). The round caps fall outside the
/// strip, where the fade is 0.
pub fn mask(ys: &[f64], stroke: f64, pixels: &mut [u8], width: usize, height: usize) {
    pixels.fill(0);
    let height = height.min(MAX_ROWS);
    let step_width = width as f64 / STEPS as f64;
    let scale = height as f64 / VIEW_HEIGHT;
    // Each segment's start, slope and the stroke's height across a column,
    // which the slope stretches.
    let segments: [(f64, f64, f64); STEPS] = std::array::from_fn(|step| {
        let (from, to) = (ys[step] * scale, ys[step + 1] * scale);
        let slope = (to - from) / step_width;
        (from, slope, stroke / 2.0 * (1.0 + slope * slope).sqrt())
    });
    let mut cover = [0.0_f64; MAX_ROWS];
    for column in 0..width {
        let (mut first, mut last) = (height, 0);
        for sub in 0..SUBCOLUMNS {
            let x = column as f64 + (sub as f64 + 0.5) / SUBCOLUMNS as f64;
            let step = ((x / step_width) as usize).min(STEPS - 1);
            let (from, slope, reach) = segments[step];
            let y = from + slope * (x - step as f64 * step_width);
            let (top, bottom) = ((y - reach).max(0.0), (y + reach).min(height as f64));
            let weight = ambient_breath::edge(x / width as f64) / SUBCOLUMNS as f64;
            let rows = top as usize..bottom.ceil() as usize;
            (first, last) = (first.min(rows.start), last.max(rows.end));
            for (row, share) in rows.clone().zip(&mut cover[rows]) {
                *share += weight * row_share(row, top, bottom);
            }
        }
        for (row, share) in cover.iter_mut().enumerate().take(last).skip(first) {
            let alpha = (share.min(1.0) * 255.0).round() as u8;
            let at = (row * width + column) * 4;
            pixels[at..at + 4].fill(alpha);
            *share = 0.0;
        }
    }
}

/// A window's strand masks, two a strand: a frame draws into the ones the
/// window let go of when it took the frame before, so no frame allocates.
#[derive(Default)]
pub struct Masks {
    buffers: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    frame: usize,
}

impl Masks {
    /// The masks of `strands` at `width` x `height` pixels, `scale` pixels a point.
    pub fn draw(&mut self, strands: &[Strand], width: u32, height: u32, scale: f64) -> Vec<Image> {
        self.frame ^= 1;
        let mut images = Vec::with_capacity(strands.len());
        for (index, strand) in strands.iter().enumerate() {
            // Before layout the strip is 0 wide; Slint's software renderer
            // panics stretching a 0-pixel image, an empty one it skips.
            if width == 0 || height == 0 {
                images.push(Image::default());
                continue;
            }
            let at = index * 2 + self.frame;
            if self.buffers.len() <= at {
                self.buffers
                    .resize_with(at + 1, || SharedPixelBuffer::new(0, 0));
            }
            let buffer = &mut self.buffers[at];
            if (buffer.width(), buffer.height()) != (width, height) {
                *buffer = SharedPixelBuffer::new(width, height);
            }
            let pixels = buffer.make_mut_bytes();
            mask(
                &strand.ys,
                strand.width * scale,
                pixels,
                width as usize,
                height as usize,
            );
            images.push(Image::from_rgba8_premultiplied(buffer.clone()));
        }
        images
    }
}

/// How much of pixel row `row` the span `top..bottom` covers.
fn row_share(row: usize, top: f64, bottom: f64) -> f64 {
    (bottom.min(row as f64 + 1.0) - top.max(row as f64)).max(0.0)
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

    fn reach(strand: &Strand) -> f64 {
        strand
            .ys
            .iter()
            .map(|y| (y - 11.0).abs())
            .fold(0.0, f64::max)
    }

    fn sound(driver: &mut Driver, seconds: f64, peak: impl Fn(f64) -> Option<f32>) -> u32 {
        let frame = 1.0 / 30.0;
        let mut reads = 0;
        for tick in 0..(seconds / frame).round() as u32 {
            driver.follow_sound(frame, || {
                reads += 1;
                peak(f64::from(tick) * frame)
            });
        }
        reads
    }

    // The Mac reads the peak every ASC_METER_POLL_MS into the meter, and the
    // meter's level into `Tide` (AmbientArt.swift:49-58).
    #[test]
    fn the_cover_tint_reads_the_meter_ten_times_a_second_and_breathes_with_it() {
        let mut driver = Driver::default();
        let reads = sound(&mut driver, 2.0, |t| {
            Some(if (t * 4.0) as i64 % 2 == 0 { 0.05 } else { 0.8 })
        });
        assert_eq!(reads, 20);
        let tint = driver.tint();
        assert!(tint > 0.0 && tint <= Tide::TINT, "{tint}");
        assert!((tint - Tide::TINT * driver.tide.env().min(1.0)).abs() < 1e-12);
    }

    #[test]
    fn no_capture_or_a_reset_leaves_the_card_untinted() {
        let mut driver = Driver::default();
        sound(&mut driver, 3.0, |_| None);
        assert_eq!(driver.tint(), 0.0, "streaming off or an input device");
        sound(&mut driver, 2.0, |t| {
            Some(if (t * 4.0) as i64 % 2 == 0 { 0.05 } else { 0.8 })
        });
        assert!(driver.tint() > 0.0);
        sound(&mut driver, 15.0, |_| Some(0.0));
        assert_eq!(driver.tint(), 0.0, "paused: gone within 15 s");
        sound(&mut driver, 2.0, |t| {
            Some(if (t * 4.0) as i64 % 2 == 0 { 0.05 } else { 0.8 })
        });
        driver.reset();
        assert_eq!(driver.tint(), 0.0, "off screen");
    }

    #[test]
    fn the_bands_are_read_thirty_times_a_second_or_once_a_slower_frame() {
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 60.0, None), 60);
        assert_eq!(run(&mut Driver::default(), 2.0, 1.0 / 15.0, None), 30);
    }

    #[test]
    fn a_stalled_frame_moves_the_art_on_a_tenth_of_a_second_as_the_macs_clock_does() {
        let mut driver = Driver::default();
        driver.advance(0.5, || None);
        assert!((driver.time - 0.1).abs() < 1e-9, "{}", driver.time);
        driver.advance(1.0 / 60.0, || None);
        assert!((driver.time - (0.1 + 1.0 / 60.0)).abs() < 1e-9);
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
            Some(Duration::from_secs_f64(1.0 / 30.0))
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
        assert!(strands[0].ys.iter().all(|y| *y == 11.0));
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
                assert!(strand.ys.iter().all(|y| (1.0..=21.0).contains(y)));
                widest = widest.max(reach(strand));
            }
        }
        assert!(widest > 2.0, "it sways: {widest}");
        // The Mac's resting swing (WindowRules.swift:205) and the breath on top.
        assert!(
            widest < ambient_breath::SWAY + 2.5,
            "it stays calm: {widest}"
        );
    }

    #[test]
    fn the_tray_panels_strands_reach_less_far() {
        let mut driver = Driver::default();
        run(&mut driver, 1.5, 1.0 / 60.0, Some([0.9; 3]));
        let reach = |density| reach(&driver.strands(&NO_COVER, density, WINDOW_SLOT, false)[0]);
        assert!(reach(FLYOUT_DENSITY) < reach(WINDOW_DENSITY));
    }

    fn masks(driver: &Driver, width: usize, height: usize) -> Vec<Vec<u8>> {
        let strands = driver.strands(&NO_COVER, WINDOW_DENSITY, WINDOW_SLOT, false);
        strands
            .iter()
            .map(|strand| {
                let mut pixels = vec![0; width * height * 4];
                let stroke = strand.width * height as f64 / WINDOW_SLOT;
                mask(&strand.ys, stroke, &mut pixels, width, height);
                pixels
            })
            .collect()
    }

    #[test]
    fn a_strand_covers_each_pixel_by_its_area() {
        let flat = [11.0; STEPS + 1];
        let rows = [
            (1, vec![(10, 0.8), (11, 0.8)]),
            (2, vec![(20, 0.6), (21, 1.0), (22, 1.0), (23, 0.6)]),
        ];
        for (scale, rows) in rows {
            let (width, height) = (182 * scale, 22 * scale);
            let mut pixels = vec![0; width * height * 4];
            let stroke = ambient_breath::STROKE * scale as f64;
            mask(&flat, stroke, &mut pixels, width, height);
            for row in 0..height {
                let want = rows.iter().find(|(r, _)| *r == row).map_or(0.0, |r| r.1);
                let got = f64::from(pixels[(row * width + width / 2) * 4 + 3]) / 255.0;
                assert!((got - want).abs() < 0.01, "{scale}x row {row}: {got}");
            }
        }
    }

    /// Three seconds of the breath at 30 frames a second in the main window's
    /// strip at 1x (182 x 22 pixels), over the middle 70% where nothing fades.
    #[test]
    fn the_breath_moves_by_its_area_without_quarter_steps() {
        let (width, height) = (182, 22);
        let middle = width * 15 / 100..width * 85 / 100;
        let mut driver = Driver::default();
        run(&mut driver, 2.0, 1.0 / 30.0, None);
        let (mut quarters, mut partial) = (0, 0);
        let mut frames: Vec<Vec<f64>> = Vec::new();
        for _ in 0..90 {
            run(&mut driver, 1.0 / 30.0, 1.0 / 30.0, None);
            let masks = masks(&driver, width, height);
            let mut frame = Vec::new();
            for row in 0..height {
                for column in middle.clone() {
                    let at = (row * width + column) * 4 + 3;
                    let alphas = masks.iter().map(|mask| f64::from(mask[at]) / 255.0);
                    for alpha in alphas.clone().filter(|a| (0.03..0.97).contains(a)) {
                        partial += 1;
                        quarters += usize::from((alpha * 4.0 - (alpha * 4.0).round()).abs() < 0.02);
                    }
                    // The strands at their 0.8 opacity, as levels above the dark card.
                    let under = alphas
                        .map(|a| 1.0 - ambient_breath::OPACITY * a)
                        .product::<f64>();
                    frame.push((1.0 - under) * 164.0);
                }
            }
            frames.push(frame);
        }
        let share = quarters as f64 / partial as f64;
        assert!(
            share < 0.1,
            "{share:.2} of the edge pixels on a quarter step"
        );
        let mut jumps: Vec<f64> = frames
            .windows(2)
            .flat_map(|pair| pair[0].iter().zip(&pair[1]))
            .filter(|(a, b)| **a > 8.0 || **b > 8.0)
            .map(|(a, b)| (a - b).abs())
            .collect();
        jumps.sort_by(f64::total_cmp);
        let p90 = jumps[jumps.len() * 9 / 10];
        // Exact coverage gives 21.3 with a 4.125 pt swing, 16.8 with the Mac's 3 pt;
        // four rows a pixel give 32.8.
        assert!(p90 <= 21.3, "frame to frame p90 {p90:.1}");
    }

    #[test]
    fn the_strip_fades_the_strands_ends_as_the_rules_say() {
        let (width, height) = (200, 22);
        let mut pixels = vec![0; width * height * 4];
        mask(
            &[11.0; STEPS + 1],
            ambient_breath::STROKE,
            &mut pixels,
            width,
            height,
        );
        let alpha = |column: usize| pixels[(10 * width + column) * 4 + 3];
        // Full from 9% in, half way at 4.75%, none at the edges.
        assert_eq!((alpha(18), alpha(181)), (alpha(100), alpha(100)));
        assert!((100..=110).contains(&alpha(9)), "{}", alpha(9));
        assert!(alpha(0) <= 6 && alpha(width - 1) <= 6);
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
